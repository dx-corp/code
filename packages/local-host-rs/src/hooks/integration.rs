//! Hook integration with the native agent
//!
//! Provides utilities for integrating the hook system with the native Rust agent.
//! This module bridges the hook registry with tool execution.

use super::{
    config::{HookSource, LoadedHook, LoadedHookConfig, load_hook_config},
    lua::LuaHookExecutor,
    matcher::{ToolMatcher, matcher_or_match_all},
    overflow::{OverflowDetector, OverflowStatus},
    registry::{HookRegistry, SafetyHook},
    types::{
        EvalGateHook, EvalGateInput, HookEventType, HookOutput, HookResult, OnErrorHook,
        OnErrorInput, OverflowHook, OverflowInput, PermissionRequestHook, PermissionRequestInput,
        PostMessageHook, PostMessageInput, PostToolUseHook, PostToolUseInput, PreMessageHook,
        PreMessageInput, PreToolUseHook, PreToolUseInput, SessionEndHook, SessionEndInput,
        SessionStartHook, SessionStartInput, StopFailureHook, StopFailureInput, SubagentStartHook,
        SubagentStartInput, SubagentStopHook, SubagentStopInput, UserPromptSubmitHook,
        UserPromptSubmitInput,
    },
    wasm::WasmHookExecutor,
};
use anyhow::Result;
use serde::Serialize;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use wait_timeout::ChildExt;

const MAX_EXTERNAL_HOOK_OUTPUT_BYTES: usize = 1024 * 1024;

/// Largest hook payload that is also copied into the child's `INPUT_JSON`
/// environment variable.
///
/// Stdin is the documented transport for the payload
/// (`docs/design/HOOKS_SYSTEM.md`, "Example Hook Scripts") and has no size
/// limit; `INPUT_JSON` is a convenience duplicate for one-line scripts. The
/// environment is not an unbounded channel: Linux caps a single `execve`
/// entry at `MAX_ARG_STRLEN` (32 pages, 128 KiB), and exceeding it fails the
/// spawn with `E2BIG` — the hook never runs, so a large tool payload would
/// turn every configured command hook into a hard error. Payloads above this
/// bound are delivered on stdin only, with `INPUT_JSON_OMITTED` set to the
/// payload's byte length so a script can tell "no payload" apart from "too
/// large for the environment".
const MAX_HOOK_INPUT_JSON_ENV_BYTES: usize = 64 * 1024;

/// How long to keep draining a hook's stdout/stderr once the hook itself has
/// finished or been killed.
///
/// A hook can leave a backgrounded descendant holding the pipes open, in which
/// case the reader threads never observe EOF. This bounds that wait; the
/// partial capture taken at the bound is what the hook actually wrote. 100ms
/// matches the slack `ExternalHook::execute` already allows past `timeout`
/// before it abandons the worker, so a bounded drain cannot by itself turn a
/// hook that answered in time into a reported timeout.
const HOOK_OUTPUT_DRAIN_GRACE: Duration = Duration::from_millis(100);

/// Integrated hook system for the native agent
///
/// Combines all native hook backends (Rust, Lua, and WASM) into a unified executor.
/// Lua and WASM executors are lazily initialized on first use to minimize
/// startup overhead (~21µs → ~200ns for basic creation).
pub struct IntegratedHookSystem {
    /// Native Rust hooks
    pub registry: HookRegistry,
    /// Lua script executor (lazy-initialized)
    lua_executor: Option<LuaHookExecutor>,
    /// WASM plugin executor (lazy-initialized)
    wasm_executor: Option<WasmHookExecutor>,
    /// Overflow detector
    overflow_detector: OverflowDetector,
    /// Current working directory
    cwd: String,
    /// Session ID
    session_id: Option<String>,
    /// Canonical JSONL path for the active persisted session.
    transcript_path: Option<String>,
    transcript_checkpoint_size: Option<u64>,
    organization_id: Option<String>,
    workspace_id: Option<String>,
    session_history: Option<Arc<MaestroSessionHistoryHook>>,
    /// Whether hooks are enabled
    enabled: bool,
    /// Session start time
    session_start: Option<Instant>,
    /// Turn count for session
    turn_count: u32,
    /// Execution metrics
    metrics: HookMetrics,
    /// Hook timeout
    timeout: Duration,
    /// Log file path
    log_file: Option<String>,
}

/// Prompt hook that injects static context on user prompt submission
struct PromptHook {
    prompt: String,
}

/// Product-owned Session History capture enabled by a verified Maestro login.
///
/// Unlike configured hooks, this carries the bearer only in memory and calls
/// the shared capture library directly. A workspace cannot replace the
/// command or observe the credential.
#[derive(Clone)]
struct MaestroSessionHistoryHook {
    organization_id: String,
    workspace_id: String,
    access_token: Arc<std::sync::Mutex<String>>,
    endpoint: Option<String>,
    state_dir: PathBuf,
}

impl MaestroSessionHistoryHook {
    fn capture(
        &self,
        event_name: &str,
        session_id: Option<&str>,
        cwd: &str,
        transcript_path: Option<&str>,
        transcript_size_before: Option<u64>,
    ) {
        let Some(session_id) = session_id else {
            return;
        };
        #[cfg(test)]
        let access_token = self
            .access_token
            .lock()
            .expect("session-history access-token lock")
            .clone();
        #[cfg(not(test))]
        let access_token = {
            let Ok(mut access_token) = self.access_token.lock() else {
                eprintln!("[session-history] capture deferred: access-token lock poisoned");
                return;
            };
            match crate::credential_mode::refreshed_identity_session_for_capture(&access_token) {
                Ok(Some(session)) => {
                    if session.organization_id != self.organization_id
                        || session.workspace_id.as_deref() != Some(self.workspace_id.as_str())
                    {
                        eprintln!(
                            "[session-history] capture deferred: refreshed Identity tenant changed"
                        );
                        return;
                    }
                    *access_token = session.access_token;
                }
                Ok(None) => {}
                Err(error) => {
                    eprintln!("[session-history] capture deferred: {error}");
                    return;
                }
            }
            access_token.clone()
        };
        let event = maestro_session_history::MaestroTranscriptEvent {
            event_name: event_name.to_string(),
            source_session_id: session_id.to_string(),
            cwd: PathBuf::from(cwd),
            transcript_path: transcript_path.map(PathBuf::from),
            transcript_size_before,
            organization_id: self.organization_id.clone(),
            workspace_id: self.workspace_id.clone(),
            endpoint: self.endpoint.clone(),
            access_token: Some(access_token),
            model: None,
        };
        if let Err(error) =
            maestro_session_history::capture_maestro_event(event, Some(&self.state_dir))
        {
            eprintln!("[session-history] capture deferred: {error}");
        }
    }
}

impl SessionStartHook for MaestroSessionHistoryHook {
    fn on_session_start(&self, input: &SessionStartInput) -> HookResult {
        self.capture(
            &input.hook_event_name,
            input.session_id.as_deref(),
            &input.cwd,
            None,
            None,
        );
        HookResult::Continue
    }
}

impl SessionEndHook for MaestroSessionHistoryHook {
    fn on_session_end(&self, input: &SessionEndInput) -> HookResult {
        self.capture(
            &input.hook_event_name,
            input.session_id.as_deref(),
            &input.cwd,
            input.transcript_path.as_deref(),
            None,
        );
        HookResult::Continue
    }
}

impl PostMessageHook for MaestroSessionHistoryHook {
    fn on_post_message(&self, input: &PostMessageInput) -> HookResult {
        // `ResponseEnd` and this hook are emitted by the native-agent task, while
        // the TUI appends and flushes the assistant message after it consumes
        // `ResponseEnd`. Waiting for that file growth on the agent task blocks
        // the runtime that must deliver the event, so the transcript only grows
        // after the wait times out. Keep configured PostMessage hooks synchronous,
        // but let this product-owned observer wait off-task for the canonical
        // persistence boundary. SessionEnd remains a synchronous final retry.
        let hook = self.clone();
        let input = input.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("maestro-session-history".to_string())
            .spawn(move || {
                hook.capture(
                    &input.hook_event_name,
                    input.session_id.as_deref(),
                    &input.cwd,
                    input.transcript_path.as_deref(),
                    input.transcript_size_before,
                );
            })
        {
            eprintln!(
                "[session-history] capture deferred: could not start capture worker: {error}"
            );
        }
        HookResult::Continue
    }
}

impl UserPromptSubmitHook for PromptHook {
    fn on_user_prompt_submit(&self, _input: &UserPromptSubmitInput) -> HookResult {
        HookResult::InjectContext {
            context: self.prompt.clone(),
        }
    }
}

/// A configured fail-closed WASM policy whose backend could not be loaded.
///
/// Keeping this as a normal registry hook makes the unavailable state
/// observable at the same boundary as a loaded policy: a tool call cannot
/// proceed while the configured enforcement hook is missing.
struct UnavailableWasmHook {
    path: PathBuf,
    tools: ToolMatcher,
    reason: String,
}

/// A hook configuration error must not turn configured policy into an allow.
///
/// Configuration is assembled before hooks are registered. If any trusted
/// source fails that load, no partial set is available to enforce, so block
/// tool execution until the configuration is corrected.
struct HookConfigLoadFailure {
    reason: String,
}

impl PreToolUseHook for HookConfigLoadFailure {
    fn on_pre_tool_use(&self, _input: &PreToolUseInput) -> HookResult {
        HookResult::Block {
            reason: format!("Hook configuration failed to load: {}", self.reason),
        }
    }
}

impl PreToolUseHook for UnavailableWasmHook {
    fn on_pre_tool_use(&self, _input: &PreToolUseInput) -> HookResult {
        HookResult::Block {
            reason: format!(
                "Required WASM hook {} is unavailable: {}",
                self.path.display(),
                self.reason
            ),
        }
    }

    fn matches(&self, tool_name: &str) -> bool {
        self.tools.matches(tool_name)
    }
}

#[derive(Clone)]
enum ExternalHookSource {
    Command(String),
    Http(String),
}

/// Adapter for command and HTTP hooks supplied by a user or trusted plugin.
///
/// Hook traits are synchronous, so the external operation runs on a short-lived
/// worker thread. Both the worker and the underlying command/client have a
/// deadline, keeping a slow hook from blocking the agent indefinitely.
struct ExternalHook {
    event: HookEventType,
    tools: ToolMatcher,
    source: ExternalHookSource,
    timeout: Duration,
    working_dir: std::path::PathBuf,
}

/// A hook input that can be published to an external command or HTTP hook.
///
/// The documented wire contract (`docs/design/HOOKS_SYSTEM.md`) is not always
/// the struct's serialized shape. Most inputs differ from it only in case, which
/// [`external_hook_payload`] already folds. `PostToolUse` differs structurally,
/// so it overrides [`ExternalHookInput::apply_contract_shape`]; every other
/// input takes the no-op default.
trait ExternalHookInput: Serialize {
    /// Rewrite the serialized payload into the documented contract shape.
    ///
    /// Runs after the camelCase keys are folded in, so an entry written here
    /// replaces the plain rename for the same key.
    fn apply_contract_shape(_payload: &mut serde_json::Map<String, serde_json::Value>) {}
}

/// Implement [`ExternalHookInput`] for inputs whose serialized shape already
/// matches the documented contract once the keys are camelCased.
macro_rules! passthrough_external_hook_input {
    ($($input:ty),+ $(,)?) => {
        $(impl ExternalHookInput for $input {})+
    };
}

passthrough_external_hook_input!(
    PreToolUseInput,
    SessionStartInput,
    SessionEndInput,
    OverflowInput,
    StopFailureInput,
    UserPromptSubmitInput,
    PreMessageInput,
    PostMessageInput,
    OnErrorInput,
    EvalGateInput,
    SubagentStartInput,
    SubagentStopInput,
    PermissionRequestInput,
);

impl ExternalHookInput for PostToolUseInput {
    /// `toolOutput` is documented as an object — `{ content, isError }` — but
    /// the struct holds a flat `tool_output` string beside a sibling
    /// `is_error`. Serializing it published `toolOutput` as a bare string, so a
    /// hook reading the documented `.toolOutput.isError` path got `null` and
    /// treated every failed tool call as a success.
    ///
    /// The flat `tool_output` and `is_error` keys stay in the payload: external
    /// hooks have shipped against the observed shape, and dropping them would
    /// break hooks that read it.
    fn apply_contract_shape(payload: &mut serde_json::Map<String, serde_json::Value>) {
        let content = payload
            .get("tool_output")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let is_error = payload
            .get("is_error")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        payload.insert(
            "toolOutput".to_string(),
            serde_json::json!({
                "content": [{ "type": "text", "text": content }],
                "isError": is_error,
            }),
        );
    }
}

impl ExternalHook {
    fn matches_tool(&self, tool_name: &str) -> bool {
        self.tools.matches(tool_name)
    }

    /// Whether this hook may observe a tool result with the given status.
    ///
    /// `PostToolUse` and `PostToolUseFailure` hooks share one registry list,
    /// so the configured event is the only thing that separates them: the
    /// documented contract (`docs/design/HOOKS_SYSTEM.md`) runs `PostToolUse`
    /// after a successful execution and `PostToolUseFailure` after a failed
    /// one. Without this filter a failure-only remediation command runs after
    /// every successful tool call as well.
    fn matches_result(&self, is_error: bool) -> bool {
        match self.event {
            HookEventType::PostToolUseFailure => is_error,
            HookEventType::PostToolUse => !is_error,
            _ => true,
        }
    }

    fn execute<T: ExternalHookInput>(&self, input: &T, tool_name: Option<&str>) -> HookResult {
        if let Some(tool_name) = tool_name {
            if !self.matches_tool(tool_name) {
                return HookResult::Continue;
            }
        }

        let payload = match external_hook_payload(input) {
            Ok(payload) => payload,
            Err(error) => {
                return HookResult::Block {
                    reason: format!("Failed to serialize external hook input: {error}"),
                };
            }
        };
        let source = self.source.clone();
        let event = self.event;
        let event_name = format!("{event:?}");
        let tool_name = tool_name.map(str::to_owned);
        let working_dir = self.working_dir.clone();
        let timeout = self.timeout.max(Duration::from_millis(1));
        let worker_timeout = timeout.saturating_add(Duration::from_millis(100));
        let (sender, receiver) = mpsc::channel();

        std::thread::spawn(move || {
            let result = match source {
                ExternalHookSource::Command(command) => run_external_command(
                    &command,
                    &payload,
                    event,
                    tool_name.as_deref(),
                    &working_dir,
                    timeout,
                ),
                ExternalHookSource::Http(url) => run_external_http(&url, &payload, event, timeout),
            };
            let _ = sender.send(result);
        });

        match receiver.recv_timeout(worker_timeout) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => HookResult::Block {
                reason: format!("External {event_name} hook timed out"),
            },
            Err(RecvTimeoutError::Disconnected) => HookResult::Block {
                reason: format!("External {event_name} hook failed before returning a result"),
            },
        }
    }
}

/// Serialize a hook input for an external (command or HTTP) hook.
///
/// The documented wire contract is camelCase: `docs/design/HOOKS_SYSTEM.md`
/// specifies `toolName`, `toolInput`, `hookEventName`, `durationMs`, and its
/// own example scripts read `jq -r '.toolName'`. The Rust input structs use
/// snake_case field names, and serializing them straight to JSON published
/// `tool_name`, so a hook written against the documentation read `null` —
/// a policy hook that blocks on tool name silently allowed everything.
///
/// Only the top-level keys are renamed. `toolInput` carries the tool's own
/// arguments and is passed through untouched; rewriting keys inside it would
/// rename tool parameters such as `file_path`.
///
/// The original snake_case keys are emitted alongside the camelCase ones.
/// External command and HTTP hooks have shipped with the snake_case names, so
/// dropping them would break every hook written against the observed
/// behavior rather than the documentation.
fn external_hook_payload<T: ExternalHookInput>(input: &T) -> Result<Vec<u8>, serde_json::Error> {
    let value = serde_json::to_value(input)?;
    let serde_json::Value::Object(fields) = value else {
        return serde_json::to_vec(&value);
    };
    let mut payload = serde_json::Map::new();
    for (key, value) in fields {
        if let Some(contract_key) = camel_case_key(&key) {
            payload.insert(contract_key, value.clone());
        }
        payload.insert(key, value);
    }
    T::apply_contract_shape(&mut payload);
    serde_json::to_vec(&serde_json::Value::Object(payload))
}

/// `hook_event_name` -> `hookEventName`.
///
/// Returns `None` when the key has no underscore to fold, so single-word keys
/// such as `cwd` and `prompt` are not duplicated.
fn camel_case_key(key: &str) -> Option<String> {
    if !key.contains('_') {
        return None;
    }
    let mut camel = String::with_capacity(key.len());
    let mut capitalize_next = false;
    for character in key.chars() {
        if character == '_' {
            capitalize_next = true;
            continue;
        }
        if capitalize_next {
            camel.extend(character.to_uppercase());
            capitalize_next = false;
        } else {
            camel.push(character);
        }
    }
    (!camel.is_empty() && camel != key).then_some(camel)
}

fn run_external_command(
    command: &str,
    payload: &[u8],
    event: HookEventType,
    tool_name: Option<&str>,
    working_dir: &Path,
    timeout: Duration,
) -> HookResult {
    let event_name = format!("{event:?}");
    #[cfg(windows)]
    let mut process = {
        let mut command_builder = ProcessCommand::new("cmd");
        command_builder.args(["/C", command]);
        command_builder
    };
    #[cfg(not(windows))]
    let mut process = {
        let mut command_builder = ProcessCommand::new("sh");
        command_builder.args(["-c", command]);
        command_builder
    };

    process
        .current_dir(working_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("HOOK_EVENT_NAME", &event_name);
    // The payload always goes to stdin below. The environment copy is
    // bounded, and dropped entirely when it would not fit, because an
    // oversized entry fails the spawn instead of the hook.
    if payload.len() <= MAX_HOOK_INPUT_JSON_ENV_BYTES {
        process
            .env("INPUT_JSON", String::from_utf8_lossy(payload).as_ref())
            .env_remove("INPUT_JSON_OMITTED");
    } else {
        // Removed rather than left inherited: this process may itself have
        // been started by a hook, and a stale `INPUT_JSON` from that outer
        // invocation would look like this hook's payload.
        process
            .env_remove("INPUT_JSON")
            .env("INPUT_JSON_OMITTED", payload.len().to_string());
    }
    if let Some(tool_name) = tool_name {
        process.env("TOOL_NAME", tool_name);
    }
    // Run the hook in its own process group so the deadline can terminate
    // everything it started. A hook that backgrounds a descendant
    // (`some-daemon &`) leaves that descendant holding the inherited
    // stdout/stderr, and killing only the shell neither stops it nor closes
    // the pipes this function reads.
    #[cfg(unix)]
    process.process_group(0);

    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(error) => {
            return HookResult::Block {
                reason: format!("Failed to start external hook command: {error}"),
            };
        }
    };

    // The payload must be written concurrently with the wait below. A hook
    // that never reads stdin leaves `write_all` blocked as soon as the pipe
    // buffer fills, and a synchronous write here would run *before* the
    // killable deadline starts: the outer worker timeout would return while
    // this thread and the child process both leaked. The writer owns the
    // handle and closes it (the child's EOF) when it finishes.
    //
    // The writer is deliberately not joined. Killing the child normally
    // unblocks it with a broken pipe, but a grandchild that inherited the
    // pipe can hold the read end open past the child's exit, and joining
    // would reintroduce the unbounded block this fix removes.
    if let Some(mut stdin) = child.stdin.take() {
        let payload = payload.to_vec();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&payload);
            let _ = stdin.flush();
        });
    }

    let stdout_reader = child.stdout.take().map(spawn_hook_output_reader);
    let stderr_reader = child.stderr.take().map(spawn_hook_output_reader);
    let mut timed_out = false;
    let mut wait_error = None;
    let status = match child.wait_timeout(timeout) {
        Ok(Some(status)) => Some(status),
        Ok(None) => {
            timed_out = true;
            terminate_hook_process(&mut child);
            None
        }
        Err(error) => {
            terminate_hook_process(&mut child);
            wait_error = Some(error.to_string());
            None
        }
    };

    // One deadline covers both streams. Giving each call its own grace let a
    // hook that leaks both pipes spend `2 * HOOK_OUTPUT_DRAIN_GRACE` here,
    // which is more than the slack `ExternalHook::execute` allows past
    // `timeout` before it abandons the worker and reports a timeout.
    let drain_deadline = Instant::now() + HOOK_OUTPUT_DRAIN_GRACE;
    let stdout = match collect_hook_output(stdout_reader, "stdout", drain_deadline) {
        Ok(output) => output,
        Err(error) => {
            return HookResult::Block {
                reason: format!("Failed reading external {event_name} hook stdout: {error}"),
            };
        }
    };
    let stderr = match collect_hook_output(stderr_reader, "stderr", drain_deadline) {
        Ok(output) => output,
        Err(error) => {
            return HookResult::Block {
                reason: format!("Failed reading external {event_name} hook stderr: {error}"),
            };
        }
    };

    if let Some(error) = wait_error {
        return HookResult::Block {
            reason: format!("Failed waiting for external {event_name} hook: {error}"),
        };
    }
    if timed_out {
        return HookResult::Block {
            reason: format!("External {event_name} hook timed out"),
        };
    }
    if stdout.truncated || stderr.truncated {
        return HookResult::Block {
            reason: format!(
                "External {event_name} hook output exceeded {MAX_EXTERNAL_HOOK_OUTPUT_BYTES} bytes"
            ),
        };
    }
    let Some(status) = status else {
        return HookResult::Block {
            reason: format!("External {event_name} hook exited without a status"),
        };
    };

    if !status.success() {
        let detail = if stderr.text.trim().is_empty() {
            format!("exit status {status}")
        } else {
            stderr.text.trim().to_string()
        };
        return HookResult::Block {
            reason: format!("External {event_name} hook failed: {detail}"),
        };
    }

    parse_external_hook_output(event, &stdout.text)
}

/// Kill a hook process and everything it started.
///
/// `Child::kill` signals only the shell. A descendant the hook backgrounded
/// survives it, keeps running past the deadline, and holds the inherited
/// stdout/stderr open so the readers below never see EOF. The hook is spawned
/// into its own process group (see `process_group(0)` in
/// `run_external_command`) precisely so it can be terminated as a unit here.
fn terminate_hook_process(child: &mut std::process::Child) {
    let pid = child.id();
    #[cfg(unix)]
    crate::tools::process_utils::kill_process_group(pid);
    #[cfg(not(unix))]
    crate::tools::process_utils::kill_process_tree(pid);
    let _ = child.kill();
    let _ = child.wait();
}

#[derive(Default)]
struct CapturedHookOutput {
    text: String,
    truncated: bool,
}

#[derive(Default)]
struct HookOutputBuffer {
    bytes: Vec<u8>,
    truncated: bool,
}

/// A pipe being drained by a background thread.
///
/// The bytes are published into `buffer` as they arrive rather than returned
/// at the end, so a collector that gives up waiting can still use everything
/// the hook managed to write.
struct HookOutputReader {
    buffer: Arc<Mutex<HookOutputBuffer>>,
    finished: mpsc::Receiver<std::io::Result<()>>,
}

fn spawn_hook_output_reader<R>(mut pipe: R) -> HookOutputReader
where
    R: Read + Send + 'static,
{
    let buffer = Arc::new(Mutex::new(HookOutputBuffer::default()));
    let (finished_tx, finished) = mpsc::channel();
    let reader_buffer = Arc::clone(&buffer);
    std::thread::spawn(move || {
        let mut chunk = [0_u8; 8192];
        let result = loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break Ok(()),
                Ok(read) => {
                    let Ok(mut buffer) = reader_buffer.lock() else {
                        break Ok(());
                    };
                    let remaining =
                        MAX_EXTERNAL_HOOK_OUTPUT_BYTES.saturating_sub(buffer.bytes.len());
                    let keep = read.min(remaining);
                    buffer.bytes.extend_from_slice(&chunk[..keep]);
                    if keep < read {
                        buffer.truncated = true;
                    }
                }
                Err(error) => break Err(error),
            }
        };
        let _ = finished_tx.send(result);
    });

    HookOutputReader { buffer, finished }
}

/// Collect one captured stream, waiting no longer than `deadline`.
///
/// Joining the reader thread unconditionally is what let a hook block this
/// function forever: a descendant that inherited the pipe holds it open after
/// the hook itself exits, so the read never returns EOF. Past the deadline the
/// reader is abandoned with whatever it has captured; it exits on its own once
/// the last writer closes the pipe.
///
/// The caller passes a deadline rather than a duration so that draining several
/// streams shares one bound instead of granting each its own.
fn collect_hook_output(
    reader: Option<HookOutputReader>,
    stream_name: &str,
    deadline: Instant,
) -> Result<CapturedHookOutput, String> {
    collect_hook_output_with_wait(
        reader,
        stream_name,
        deadline,
        Instant::now(),
        |reader, remaining| reader.finished.recv_timeout(remaining),
    )
}

fn collect_hook_output_with_wait(
    reader: Option<HookOutputReader>,
    stream_name: &str,
    deadline: Instant,
    now: Instant,
    wait: impl FnOnce(&HookOutputReader, Duration) -> Result<std::io::Result<()>, RecvTimeoutError>,
) -> Result<CapturedHookOutput, String> {
    let Some(reader) = reader else {
        return Ok(CapturedHookOutput::default());
    };
    let remaining = deadline.saturating_duration_since(now);
    match wait(&reader, remaining) {
        Ok(Ok(())) | Err(RecvTimeoutError::Timeout) => {}
        Ok(Err(error)) => return Err(error.to_string()),
        Err(RecvTimeoutError::Disconnected) => {
            return Err(format!("{stream_name} reader panicked"));
        }
    }
    let buffer = reader
        .buffer
        .lock()
        .map_err(|_| format!("{stream_name} reader panicked"))?;
    Ok(CapturedHookOutput {
        text: String::from_utf8_lossy(&buffer.bytes).into_owned(),
        truncated: buffer.truncated,
    })
}

fn run_external_http(
    url: &str,
    payload: &[u8],
    event: HookEventType,
    timeout: Duration,
) -> HookResult {
    let client = match reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return HookResult::Block {
                reason: format!("Failed to create HTTP hook client: {error}"),
            };
        }
    };
    let response = match client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(payload.to_vec())
        .send()
    {
        Ok(response) => response,
        Err(error) => {
            return HookResult::Block {
                reason: format!("HTTP hook request failed: {error}"),
            };
        }
    };
    let status = response.status();
    let body = match read_bounded_http_body(response) {
        Ok(body) => body,
        Err(error) => {
            return HookResult::Block {
                reason: format!("HTTP hook response could not be read: {error}"),
            };
        }
    };
    if body.truncated {
        return HookResult::Block {
            reason: format!("HTTP hook response exceeded {MAX_EXTERNAL_HOOK_OUTPUT_BYTES} bytes"),
        };
    }
    if !status.is_success() {
        return HookResult::Block {
            reason: format!("HTTP hook returned {status}: {}", body.text.trim()),
        };
    }
    parse_external_hook_output(event, &body.text)
}

/// Read an HTTP hook response under the same cap as command-hook output.
///
/// `Response::text` buffers whatever the endpoint sends, so a hostile or
/// broken hook server could exhaust memory. The reader stops one byte past
/// the cap, which bounds both the allocation and the time spent draining the
/// socket; the extra byte is what makes an exactly-at-the-cap body succeed
/// while a larger one is reported as truncated.
fn read_bounded_http_body(
    mut response: reqwest::blocking::Response,
) -> std::io::Result<CapturedHookOutput> {
    let limit = u64::try_from(MAX_EXTERNAL_HOOK_OUTPUT_BYTES).unwrap_or(u64::MAX);
    let mut body = Vec::new();
    Read::take(&mut response, limit.saturating_add(1)).read_to_end(&mut body)?;
    let truncated = body.len() > MAX_EXTERNAL_HOOK_OUTPUT_BYTES;
    body.truncate(MAX_EXTERNAL_HOOK_OUTPUT_BYTES);
    Ok(CapturedHookOutput {
        text: String::from_utf8_lossy(&body).into_owned(),
        truncated,
    })
}

/// How strictly external-hook output is held to the typed schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookOutputMode {
    /// Report the schema violation and fall back to the legacy value-walking
    /// parser. This is the compatibility default.
    Lenient,
    /// Refuse the hook output. The operation the hook guards is blocked.
    Strict,
}

/// Environment variable that opts a session into [`HookOutputMode::Strict`].
pub const STRICT_HOOK_OUTPUT_ENV: &str = "MAESTRO_STRICT_HOOK_OUTPUT";

/// Resolve the compatibility mode for hook output.
///
/// Default is [`HookOutputMode::Lenient`]: a hook whose output does not match
/// the typed schema still works, and the violation is printed once per
/// execution. Setting `MAESTRO_STRICT_HOOK_OUTPUT` to `1` or `true` refuses
/// that output instead.
fn hook_output_mode() -> HookOutputMode {
    match std::env::var(STRICT_HOOK_OUTPUT_ENV) {
        Ok(value) => {
            let value = value.trim().to_ascii_lowercase();
            if value == "1" || value == "true" || value == "yes" {
                HookOutputMode::Strict
            } else {
                HookOutputMode::Lenient
            }
        }
        Err(_) => HookOutputMode::Lenient,
    }
}

fn parse_external_hook_output(event: HookEventType, output: &str) -> HookResult {
    parse_external_hook_output_with_mode(event, output, hook_output_mode())
}

/// Deserialize a hook's stdout (or HTTP response body) into [`HookOutput`] and
/// validate it against `event`.
///
/// Runs one response validator per hook step instead of walking an untyped value.
///
/// Three things change relative to the previous value-walking parser:
///
/// - Output that is not a JSON object no longer becomes model context. A shell
///   hook that prints a stray `set -x` line used to have that line injected
///   into the conversation.
/// - Unknown keys are reported, so `modifedInput` is a visible error rather
///   than a field that is skipped.
/// - `modifiedInput` must be a JSON object and only `PreToolUse` may return it.
fn parse_external_hook_output_with_mode(
    event: HookEventType,
    output: &str,
    mode: HookOutputMode,
) -> HookResult {
    let output = output.trim();
    if output.is_empty() {
        return HookResult::Continue;
    }

    let value = match serde_json::from_str::<serde_json::Value>(output) {
        Ok(value) => value,
        Err(error) => {
            return reject_or_fall_back(
                event,
                mode,
                &format!("output is not JSON: {error}"),
                || HookResult::InjectContext {
                    context: output.to_string(),
                },
            );
        }
    };

    if !value.is_object() {
        return reject_or_fall_back(event, mode, "output is not a JSON object", || {
            value
                .as_str()
                .map_or(HookResult::Continue, |context| HookResult::InjectContext {
                    context: context.to_string(),
                })
        });
    }

    match serde_json::from_value::<HookOutput>(value.clone()) {
        Ok(typed) => match typed.validate_for(event) {
            Ok(result) => result,
            Err(error) => reject_or_fall_back(event, mode, &format!("{error}"), || {
                legacy_parse_external_hook_output(&value)
            }),
        },
        Err(error) => reject_or_fall_back(
            event,
            mode,
            &format!("output does not match the hook output schema: {error}"),
            || legacy_parse_external_hook_output(&value),
        ),
    }
}

/// Apply the configured compatibility mode to a rejected hook response.
fn reject_or_fall_back(
    event: HookEventType,
    mode: HookOutputMode,
    detail: &str,
    fallback: impl FnOnce() -> HookResult,
) -> HookResult {
    match mode {
        HookOutputMode::Strict => HookResult::Block {
            reason: format!("External {event:?} hook returned invalid output: {detail}"),
        },
        HookOutputMode::Lenient => {
            eprintln!(
                "[hooks] External {event:?} hook returned invalid output: {detail}. \
                 Accepting it under the compatibility parser; set \
                 {STRICT_HOOK_OUTPUT_ENV}=1 to refuse it."
            );
            fallback()
        }
    }
}

/// The pre-schema parser used by lenient compatibility mode.
///
/// It reads whichever key spelling it finds and accepts any JSON type, which
/// preserves existing hook contracts while strict mode enforces the schema.
fn legacy_parse_external_hook_output(value: &serde_json::Value) -> HookResult {
    let Some(object) = value.as_object() else {
        return HookResult::Continue;
    };

    let specific = object
        .get("hookSpecificOutput")
        .or_else(|| object.get("hook_specific_output"))
        .and_then(serde_json::Value::as_object);
    let specific_field = |camel: &str, snake: &str| {
        specific.and_then(|specific| specific.get(camel).or_else(|| specific.get(snake)))
    };

    let permission_decision = specific_field("permissionDecision", "permission_decision")
        .and_then(serde_json::Value::as_str)
        .map(str::trim);
    let decision = object
        .get("decision")
        .and_then(serde_json::Value::as_str)
        .map(str::trim);
    let blocked = object
        .get("block")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        || matches!(decision, Some("block" | "deny" | "reject"))
        || matches!(permission_decision, Some("deny"))
        || object.get("continue").and_then(serde_json::Value::as_bool) == Some(false);
    let confirmation_requested = matches!(permission_decision, Some("ask"));

    if blocked || confirmation_requested {
        let fallback = if blocked {
            "Blocked by external hook"
        } else {
            "External hook requested confirmation, which external hooks cannot prompt for"
        };
        let reason = specific_field("permissionDecisionReason", "permission_decision_reason")
            .or_else(|| object.get("reason"))
            .or_else(|| object.get("message"))
            .or_else(|| object.get("block_reason"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|reason| !reason.is_empty())
            .unwrap_or(fallback);
        return HookResult::Block {
            reason: reason.to_string(),
        };
    }

    if let Some(input) = specific_field("modifiedInput", "modified_input")
        .or_else(|| object.get("modifiedInput"))
        .or_else(|| object.get("modified_input"))
        .or_else(|| object.get("modify_input"))
    {
        return HookResult::ModifyInput {
            new_input: input.clone(),
        };
    }

    if let Some(context) = specific_field("contextToAdd", "context_to_add")
        .or_else(|| object.get("contextToAdd"))
        .or_else(|| object.get("context"))
        .or_else(|| object.get("additional_context"))
        .and_then(serde_json::Value::as_str)
    {
        return HookResult::InjectContext {
            context: context.to_string(),
        };
    }

    let eval_passed = specific_field("passed", "passed")
        .or_else(|| object.get("passed"))
        .and_then(serde_json::Value::as_bool);
    let eval_score = specific_field("score", "score")
        .or_else(|| object.get("score"))
        .and_then(serde_json::Value::as_f64);
    let eval_threshold = specific_field("threshold", "threshold")
        .or_else(|| object.get("threshold"))
        .and_then(serde_json::Value::as_f64);
    let eval_rationale = specific_field("rationale", "rationale")
        .or_else(|| object.get("rationale"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|rationale| !rationale.is_empty());
    let assertions = specific_field("assertions", "assertions")
        .or_else(|| object.get("assertions"))
        .and_then(serde_json::Value::as_array);
    let assertion_failed = assertions.is_some_and(|items| {
        items.iter().any(|item| {
            item.get("passed").and_then(serde_json::Value::as_bool) == Some(false)
                || match (
                    item.get("score").and_then(serde_json::Value::as_f64),
                    item.get("threshold").and_then(serde_json::Value::as_f64),
                ) {
                    (Some(score), Some(threshold)) => score < threshold,
                    _ => false,
                }
        })
    });
    let score_below_threshold = match (eval_score, eval_threshold) {
        (Some(score), Some(threshold)) => score < threshold,
        _ => false,
    };
    let eval_failed =
        matches!(eval_passed, Some(false)) || score_below_threshold || assertion_failed;
    if eval_failed {
        let reason = eval_rationale
            .map(str::to_owned)
            .or_else(|| match (eval_score, eval_threshold) {
                (Some(score), Some(threshold)) => {
                    Some(format!("score {score} below threshold {threshold}"))
                }
                _ if assertion_failed => Some("EvalGate assertion failed".to_string()),
                _ => None,
            })
            .unwrap_or_else(|| "EvalGate failed".to_string());
        return HookResult::Block { reason };
    }
    if eval_passed == Some(true) || eval_score.is_some() || assertions.is_some() {
        let mut parts = Vec::new();
        if let Some(score) = eval_score {
            if let Some(threshold) = eval_threshold {
                parts.push(format!("eval score {score} (threshold {threshold})"));
            } else {
                parts.push(format!("eval score {score}"));
            }
        } else if eval_passed == Some(true) {
            parts.push("eval passed".to_string());
        }
        if let Some(rationale) = eval_rationale {
            parts.push(rationale.to_owned());
        }
        if !parts.is_empty() {
            return HookResult::InjectContext {
                context: parts.join(": "),
            };
        }
    }

    HookResult::Continue
}

impl PreToolUseHook for ExternalHook {
    fn on_pre_tool_use(&self, input: &PreToolUseInput) -> HookResult {
        self.execute(input, Some(&input.tool_name))
    }

    fn matches(&self, tool_name: &str) -> bool {
        self.matches_tool(tool_name)
    }
}

impl PostToolUseHook for ExternalHook {
    fn on_post_tool_use(&self, input: &PostToolUseInput) -> HookResult {
        if !self.matches_result(input.is_error) {
            return HookResult::Continue;
        }
        self.execute(input, Some(&input.tool_name))
    }

    fn matches(&self, tool_name: &str) -> bool {
        self.matches_tool(tool_name)
    }
}

impl SessionStartHook for ExternalHook {
    fn on_session_start(&self, input: &SessionStartInput) -> HookResult {
        self.execute(input, None)
    }
}

impl SessionEndHook for ExternalHook {
    fn on_session_end(&self, input: &SessionEndInput) -> HookResult {
        self.execute(input, None)
    }
}

impl OverflowHook for ExternalHook {
    fn on_overflow(&self, input: &OverflowInput) -> HookResult {
        self.execute(input, None)
    }
}

impl StopFailureHook for ExternalHook {
    fn on_stop_failure(&self, input: &StopFailureInput) -> HookResult {
        self.execute(input, None)
    }
}

impl UserPromptSubmitHook for ExternalHook {
    fn on_user_prompt_submit(&self, input: &UserPromptSubmitInput) -> HookResult {
        self.execute(input, None)
    }
}

impl PreMessageHook for ExternalHook {
    fn on_pre_message(&self, input: &PreMessageInput) -> HookResult {
        self.execute(input, None)
    }
}

impl PostMessageHook for ExternalHook {
    fn on_post_message(&self, input: &PostMessageInput) -> HookResult {
        self.execute(input, None)
    }
}

impl OnErrorHook for ExternalHook {
    fn on_error(&self, input: &OnErrorInput) -> HookResult {
        self.execute(input, None)
    }
}

impl EvalGateHook for ExternalHook {
    fn on_eval_gate(&self, input: &EvalGateInput) -> HookResult {
        self.execute(input, Some(&input.tool_name))
    }
}

impl SubagentStartHook for ExternalHook {
    fn on_subagent_start(&self, input: &SubagentStartInput) -> HookResult {
        self.execute(input, None)
    }
}

impl SubagentStopHook for ExternalHook {
    fn on_subagent_stop(&self, input: &SubagentStopInput) -> HookResult {
        self.execute(input, None)
    }
}

impl PermissionRequestHook for ExternalHook {
    fn on_permission_request(&self, input: &PermissionRequestInput) -> HookResult {
        self.execute(input, Some(&input.tool_name))
    }
}

impl IntegratedHookSystem {
    /// Create a new hook system for a given working directory
    ///
    /// This is extremely fast (~200ns) because Lua and WASM executors
    /// are lazily initialized only when scripts/plugins are actually loaded.
    #[must_use]
    pub fn new(cwd: &str) -> Self {
        Self {
            registry: HookRegistry::new(),
            lua_executor: None,
            wasm_executor: None,
            overflow_detector: OverflowDetector::new(),
            cwd: cwd.to_string(),
            session_id: None,
            transcript_path: None,
            transcript_checkpoint_size: None,
            organization_id: None,
            workspace_id: None,
            session_history: None,
            enabled: true,
            session_start: None,
            turn_count: 0,
            metrics: HookMetrics::default(),
            timeout: Duration::from_secs(30),
            log_file: None,
        }
    }

    /// Get or initialize the Lua executor
    fn lua_executor_mut(&mut self) -> &mut LuaHookExecutor {
        self.lua_executor.get_or_insert_with(LuaHookExecutor::new)
    }

    /// Get or initialize the WASM executor
    fn wasm_executor_mut(&mut self) -> &mut WasmHookExecutor {
        self.wasm_executor.get_or_insert_with(WasmHookExecutor::new)
    }

    /// Create and load hooks from configuration files
    #[must_use]
    pub fn load_from_config(cwd: &str) -> Self {
        Self::from_config_result(cwd, load_hook_config(Path::new(cwd)))
    }

    fn from_config_result(cwd: &str, config_result: Result<LoadedHookConfig>) -> Self {
        let mut system = Self::new(cwd);

        // Load config
        match config_result {
            Ok(config) => {
                system.enabled = config.settings.enabled;
                system.timeout = Duration::from_millis(config.settings.timeout_ms);
                system.log_file = config.settings.log_file.clone();
                system.load_hooks_from_config(&config);

                if !config.hooks.is_empty() {
                    eprintln!(
                        "[hooks] Configured {} hooks from {:?}",
                        config.hooks.len(),
                        config.source_paths
                    );
                }
            }
            Err(e) => {
                eprintln!("[hooks] Warning: Failed to load config: {e}");
                system
                    .registry
                    .register_pre_tool_use(Arc::new(HookConfigLoadFailure {
                        reason: format!("{e:#}"),
                    }));
            }
        }

        // Register built-in safety hook
        system.registry.register_pre_tool_use(Arc::new(SafetyHook));

        system
    }

    /// Load hooks from parsed configuration
    fn load_hooks_from_config(&mut self, config: &LoadedHookConfig) {
        for hook in &config.hooks {
            match &hook.source {
                HookSource::Prompt(prompt) => {
                    if hook.definition.event == HookEventType::UserPromptSubmit {
                        self.registry
                            .register_user_prompt_submit(Arc::new(PromptHook {
                                prompt: prompt.clone(),
                            }));
                    } else {
                        eprintln!(
                            "[hooks] Prompt hooks are only supported for UserPromptSubmit in Rust TUI"
                        );
                    }
                }
                HookSource::LuaInline(script) => {
                    if let Err(e) = self.lua_executor_mut().load_script(
                        script,
                        hook.definition.event,
                        hook.definition.tools.clone(),
                    ) {
                        eprintln!("[hooks] Failed to load Lua script: {e}");
                    }
                }
                HookSource::LuaFile(path) => {
                    if let Err(e) = self.lua_executor_mut().load_file(
                        path,
                        hook.definition.event,
                        hook.definition.tools.clone(),
                    ) {
                        eprintln!("[hooks] Failed to load Lua file {}: {}", path.display(), e);
                    }
                }
                HookSource::Wasm(path) => {
                    let required = hook.definition.fail_closed();
                    let timeout = self.timeout;
                    let load_result = {
                        let executor = self.wasm_executor_mut();
                        executor.set_timeout(timeout);
                        executor.load_plugin_with_policy(
                            path,
                            hook.definition.event,
                            hook.definition.tools.clone(),
                            required,
                        )
                    };
                    if let Err(e) = load_result {
                        eprintln!(
                            "[hooks] Failed to load WASM plugin {}: {}",
                            path.display(),
                            e
                        );
                        if required && hook.definition.event == HookEventType::PreToolUse {
                            self.registry
                                .register_pre_tool_use(Arc::new(UnavailableWasmHook {
                                    path: path.clone(),
                                    tools: matcher_or_match_all(&hook.definition.tools),
                                    reason: e.to_string(),
                                }));
                        }
                    }
                }
                HookSource::Command(_) | HookSource::Http(_) => {
                    self.register_external_hook(hook);
                }
            }
        }
    }

    fn register_external_hook(&mut self, hook: &LoadedHook) {
        let source = match &hook.source {
            HookSource::Command(command) => ExternalHookSource::Command(command.clone()),
            HookSource::Http(url) => ExternalHookSource::Http(url.clone()),
            _ => return,
        };
        let external = Arc::new(ExternalHook {
            event: hook.definition.event,
            tools: matcher_or_match_all(&hook.definition.tools),
            source,
            timeout: hook.definition.timeout_ms.map_or(self.timeout, |timeout| {
                Duration::from_millis(timeout.max(1))
            }),
            working_dir: hook
                .definition
                .working_dir
                .clone()
                .unwrap_or_else(|| std::path::PathBuf::from(&self.cwd)),
        });

        match hook.definition.event {
            HookEventType::PreToolUse => self.registry.register_pre_tool_use(external),
            HookEventType::PostToolUse | HookEventType::PostToolUseFailure => {
                self.registry.register_post_tool_use(external);
            }
            HookEventType::SessionStart => self.registry.register_session_start(external),
            HookEventType::SessionEnd => self.registry.register_session_end(external),
            HookEventType::Overflow => self.registry.register_overflow(external),
            HookEventType::StopFailure => self.registry.register_stop_failure(external),
            HookEventType::UserPromptSubmit => self.registry.register_user_prompt_submit(external),
            HookEventType::PreMessage => self.registry.register_pre_message(external),
            HookEventType::PostMessage => self.registry.register_post_message(external),
            HookEventType::OnError => self.registry.register_on_error(external),
            HookEventType::EvalGate => self.registry.register_eval_gate(external),
            HookEventType::SubagentStart => self.registry.register_subagent_start(external),
            HookEventType::SubagentStop => self.registry.register_subagent_stop(external),
            HookEventType::PermissionRequest => self.registry.register_permission_request(external),
            unsupported => eprintln!(
                "[hooks] External hooks are not supported for {unsupported:?} in the Rust TUI"
            ),
        }
    }

    /// Reload all hooks from config files
    pub fn reload(&mut self) -> Result<ReloadResult> {
        let lua_reloaded = self
            .lua_executor
            .as_mut()
            .map(super::lua::LuaHookExecutor::reload)
            .transpose()?
            .unwrap_or(0);
        let wasm_reloaded = self
            .wasm_executor
            .as_mut()
            .map(super::wasm::WasmHookExecutor::reload)
            .transpose()?
            .unwrap_or(0);

        // Reload config
        if let Ok(config) = load_hook_config(Path::new(&self.cwd)) {
            self.enabled = config.settings.enabled;
            self.timeout = Duration::from_millis(config.settings.timeout_ms);
            self.log_file = config.settings.log_file.clone();
            if let Some(wasm) = self.wasm_executor.as_mut() {
                wasm.set_timeout(self.timeout);
            }
        }

        Ok(ReloadResult {
            lua_scripts: lua_reloaded,
            wasm_plugins: wasm_reloaded,
        })
    }

    /// Set session ID for hook context
    pub fn set_session_id(&mut self, session_id: Option<String>) {
        self.set_session_context(session_id, None);
    }

    /// Set the active persisted session identity and canonical JSONL path.
    pub fn set_session_context(
        &mut self,
        session_id: Option<String>,
        transcript_path: Option<String>,
    ) {
        self.session_id = session_id;
        self.transcript_checkpoint_size = transcript_path.as_deref().map(|path| {
            std::fs::metadata(path)
                .map(|metadata| metadata.len())
                .unwrap_or(0)
        });
        self.transcript_path = transcript_path;
    }

    /// Snapshot the canonical JSONL immediately before `ResponseEnd` tells the
    /// UI to persist the assistant response. The file may already contain the
    /// current user message, so the previous capture boundary is not precise
    /// enough for this synchronization point.
    pub fn checkpoint_transcript_before_response(&mut self) {
        self.transcript_checkpoint_size = self
            .transcript_path
            .as_deref()
            .and_then(|path| std::fs::metadata(path).ok())
            .map(|metadata| metadata.len())
            .or(self.transcript_checkpoint_size);
    }

    pub fn record_tool_operation(
        &mut self,
        record: &maestro_runtime_contracts::ToolOperationRecord,
    ) -> Result<(), String> {
        let Some(path) = self.transcript_path.as_deref() else {
            return Ok(());
        };
        maestro_session::session::tool_operations::append_tool_operation(
            std::path::Path::new(path),
            record,
        )
        .map_err(|error| error.to_string())
    }

    pub fn load_tool_operations(
        &self,
    ) -> Result<Vec<maestro_runtime_contracts::ToolOperationRecord>, String> {
        let Some(path) = self.transcript_path.as_deref() else {
            return Ok(Vec::new());
        };
        maestro_session::session::tool_operations::load_tool_operation_ledger(std::path::Path::new(
            path,
        ))
        .map(|ledger| ledger.latest_records().cloned().collect())
        .map_err(|error| error.to_string())
    }

    /// Attach the tenant scope resolved from the authenticated Identity session.
    /// External hooks receive identifiers only; bearer credentials remain out of
    /// the hook payload.
    pub fn set_identity_context(
        &mut self,
        organization_id: Option<String>,
        workspace_id: Option<String>,
    ) {
        self.organization_id = organization_id;
        self.workspace_id = workspace_id;
    }

    /// Enable zero-configuration Session History capture for the authenticated
    /// tenant. This is registered by Maestro itself after Identity verification;
    /// repository hook configuration is not involved.
    pub fn enable_authenticated_session_history(
        &mut self,
        organization_id: String,
        workspace_id: String,
        access_token: String,
        endpoint: Option<String>,
        state_dir: PathBuf,
    ) {
        self.set_identity_context(Some(organization_id.clone()), Some(workspace_id.clone()));
        let hook = Arc::new(MaestroSessionHistoryHook {
            organization_id,
            workspace_id,
            access_token: Arc::new(std::sync::Mutex::new(access_token)),
            endpoint,
            state_dir,
        });
        self.session_history = Some(hook);
    }

    /// Point `log_event` at a file so dispatches become assertable in tests.
    pub fn set_log_file(&mut self, path: Option<String>) {
        self.log_file = path;
    }

    /// The session id stamped onto every hook payload, if one is set.
    #[must_use]
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Set the model for overflow detection
    pub fn set_model(&mut self, model_id: &str) {
        self.overflow_detector = OverflowDetector::for_model(model_id);
    }

    /// Update token count for overflow detection
    pub fn update_tokens(&mut self, input: u64, output: u64, cache: u64) {
        self.overflow_detector.update_tokens(input, output, cache);
    }

    /// Check overflow status
    #[must_use]
    pub fn check_overflow(&self) -> OverflowStatus {
        self.overflow_detector.check_status()
    }

    /// Signal session start
    pub fn on_session_start(&mut self, source: &str) -> HookResult {
        self.session_start = Some(Instant::now());
        self.turn_count = 0;

        let input = SessionStartInput {
            hook_event_name: "SessionStart".to_string(),
            cwd: self.cwd.clone(),
            session_id: self.session_id.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            source: source.to_string(),
            organization_id: self.organization_id.clone(),
            workspace_id: self.workspace_id.clone(),
        };

        if let Some(hook) = &self.session_history {
            let _ = hook.on_session_start(&input);
        }
        if !self.enabled {
            return HookResult::Continue;
        }

        self.log_event(
            "SessionStart",
            &serde_json::to_string(&input).unwrap_or_default(),
        );
        self.registry.execute_session_start(&input)
    }

    /// Signal session end
    pub fn on_session_end(&mut self, reason: &str) -> HookResult {
        let duration_ms = self
            .session_start
            .map_or(0, |s| s.elapsed().as_millis() as u64);

        let input = SessionEndInput {
            hook_event_name: "SessionEnd".to_string(),
            cwd: self.cwd.clone(),
            session_id: self.session_id.clone(),
            transcript_path: self.transcript_path.clone(),
            organization_id: self.organization_id.clone(),
            workspace_id: self.workspace_id.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            reason: reason.to_string(),
            duration_ms,
            turn_count: self.turn_count,
        };

        if let Some(hook) = &self.session_history {
            let _ = hook.on_session_end(&input);
        }
        if !self.enabled {
            return HookResult::Continue;
        }

        self.log_event(
            "SessionEnd",
            &serde_json::to_string(&input).unwrap_or_default(),
        );
        self.registry.execute_session_end(&input)
    }

    /// Increment turn count
    pub fn increment_turn(&mut self) {
        self.turn_count += 1;
    }

    /// Execute `PreToolUse` hooks (sync version - no IPC)
    ///
    /// Returns the hook result which may block, modify, or continue execution.
    /// This is the canonical native hook execution path.
    /// The async wrapper delegates to this native implementation.
    pub fn execute_pre_tool_use(
        &mut self,
        tool_name: &str,
        tool_call_id: &str,
        tool_input: &serde_json::Value,
    ) -> HookResult {
        if !self.enabled {
            return HookResult::Continue;
        }

        let start = Instant::now();

        let input = PreToolUseInput {
            hook_event_name: "PreToolUse".to_string(),
            cwd: self.cwd.clone(),
            session_id: self.session_id.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            tool_name: tool_name.to_string(),
            tool_call_id: tool_call_id.to_string(),
            tool_input: tool_input.clone(),
        };

        self.log_event("PreToolUse", &format!("tool={tool_name} id={tool_call_id}"));

        // Execute with timeout protection
        let result = self.execute_with_timeout(|| {
            // Execute native hooks first
            let native_result = self.registry.execute_pre_tool_use(&input);
            if !matches!(native_result, HookResult::Continue) {
                return native_result;
            }

            // Execute Lua hooks (if any loaded)
            if let Some(ref lua) = self.lua_executor {
                let lua_result = lua.execute_pre_tool_use(&input);
                if !matches!(lua_result, HookResult::Continue) {
                    return lua_result;
                }
            }

            // Execute WASM hooks (if any loaded)
            if let Some(ref wasm) = self.wasm_executor {
                let wasm_result = wasm.execute_pre_tool_use(&input);
                if !matches!(wasm_result, HookResult::Continue) {
                    return wasm_result;
                }
            }

            HookResult::Continue
        });

        // Update metrics
        self.metrics.pre_tool_use_count += 1;
        self.metrics.total_duration += start.elapsed();
        if matches!(result, HookResult::Block { .. }) {
            self.metrics.blocks += 1;
        }

        result
    }

    /// Execute `PreToolUse` hooks through the async agent interface.
    pub async fn execute_pre_tool_use_async(
        &mut self,
        tool_name: &str,
        tool_call_id: &str,
        tool_input: &serde_json::Value,
    ) -> HookResult {
        self.execute_pre_tool_use(tool_name, tool_call_id, tool_input)
    }

    /// Execute `PostToolUse` hooks
    pub fn execute_post_tool_use(
        &mut self,
        tool_name: &str,
        tool_call_id: &str,
        tool_input: &serde_json::Value,
        tool_output: &str,
        is_error: bool,
        duration_ms: u64,
    ) -> HookResult {
        if !self.enabled {
            return HookResult::Continue;
        }

        let start = Instant::now();

        // `PostToolUse` and `PostToolUseFailure` share this path and are
        // separated by the result status (see `ExternalHook::matches_result`).
        // Reporting `PostToolUse` for both made the payload disagree with the
        // hook that was actually selected, so a failure hook that branches on
        // `hookEventName` took its success branch.
        let hook_event_name = if is_error {
            "PostToolUseFailure"
        } else {
            "PostToolUse"
        };

        let input = PostToolUseInput {
            hook_event_name: hook_event_name.to_string(),
            cwd: self.cwd.clone(),
            session_id: self.session_id.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            tool_name: tool_name.to_string(),
            tool_call_id: tool_call_id.to_string(),
            tool_input: tool_input.clone(),
            tool_output: tool_output.to_string(),
            is_error,
            duration_ms,
        };

        self.log_event(
            hook_event_name,
            &format!("tool={tool_name} error={is_error} duration={duration_ms}ms"),
        );

        let result = self.registry.execute_post_tool_use(&input);

        // Update metrics
        self.metrics.post_tool_use_count += 1;
        self.metrics.total_duration += start.elapsed();

        result
    }

    /// Execute with timeout and panic protection
    ///
    /// Wraps hook execution with:
    /// - Panic catching: panics in hooks don't crash the agent
    /// - Timeout warnings: logs if execution takes too long
    fn execute_with_timeout<F>(&self, f: F) -> HookResult
    where
        F: FnOnce() -> HookResult,
    {
        let start = Instant::now();

        // Catch panics from hook execution
        let result = panic::catch_unwind(AssertUnwindSafe(f));

        if start.elapsed() > self.timeout {
            eprintln!(
                "[hooks] Warning: Hook execution exceeded timeout ({:?})",
                self.timeout
            );
        }

        match result {
            Ok(hook_result) => hook_result,
            Err(panic_info) => {
                // Extract panic message if possible
                let panic_msg = if let Some(s) = panic_info.downcast_ref::<&str>() {
                    (*s).to_string()
                } else if let Some(s) = panic_info.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "Unknown panic".to_string()
                };

                eprintln!("[hooks] Error: Hook panicked: {panic_msg}");
                self.log_event("HookPanic", &panic_msg);

                // Continue execution despite the panic
                HookResult::Continue
            }
        }
    }

    /// Execute a hook with full error boundary protection
    ///
    /// This is the safest way to run hook code that might panic.
    /// Returns Continue on any error to ensure the agent keeps running.
    pub fn safe_execute<F>(&self, name: &str, f: F) -> HookResult
    where
        F: FnOnce() -> HookResult + panic::UnwindSafe,
    {
        match panic::catch_unwind(f) {
            Ok(result) => result,
            Err(panic_info) => {
                let msg = if let Some(s) = panic_info.downcast_ref::<&str>() {
                    format!("{name}: {s}")
                } else if let Some(s) = panic_info.downcast_ref::<String>() {
                    format!("{name}: {s}")
                } else {
                    format!("{name}: Unknown panic")
                };

                eprintln!("[hooks] PANIC in {msg}");
                HookResult::Continue
            }
        }
    }

    /// Log an event if logging is enabled
    fn log_event(&self, event_type: &str, details: &str) {
        if let Some(ref log_path) = self.log_file {
            let timestamp = chrono::Utc::now().to_rfc3339();
            let log_line = format!("[{timestamp}] {event_type} {details}\n");

            // Try to append to log file
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_path)
            {
                use std::io::Write;
                let _ = file.write_all(log_line.as_bytes());
            }
        }
    }

    /// Check if a stop reason indicates overflow
    #[must_use]
    pub fn is_overflow_stop(&self, stop_reason: &str) -> bool {
        self.overflow_detector.check_stop_reason(stop_reason)
    }

    /// Handle overflow - returns true if auto-compaction should proceed
    pub fn handle_overflow(&mut self) -> bool {
        if !self.enabled {
            return true;
        }

        let input = OverflowInput {
            hook_event_name: "Overflow".to_string(),
            cwd: self.cwd.clone(),
            session_id: self.session_id.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            token_count: self.overflow_detector.current_tokens(),
            max_tokens: self.overflow_detector.max_tokens(),
        };

        self.log_event(
            "Overflow",
            &format!("tokens={}/{}", input.token_count, input.max_tokens),
        );

        let result = self.registry.execute_overflow(&input);
        self.metrics.overflow_count += 1;

        matches!(result, HookResult::Continue)
    }

    /// Execute `UserPromptSubmit` hooks - called when user submits a prompt
    pub fn execute_user_prompt_submit(
        &mut self,
        prompt: &str,
        attachment_count: u32,
    ) -> HookResult {
        if !self.enabled {
            return HookResult::Continue;
        }

        let input = UserPromptSubmitInput {
            hook_event_name: "UserPromptSubmit".to_string(),
            cwd: self.cwd.clone(),
            session_id: self.session_id.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            prompt: prompt.to_string(),
            attachment_count,
        };

        self.log_event(
            "UserPromptSubmit",
            &format!("len={} attachments={attachment_count}", prompt.len()),
        );

        self.execute_with_timeout(|| self.registry.execute_user_prompt_submit(&input))
    }

    /// Execute `PreMessage` hooks - called before sending user message to model
    pub fn execute_pre_message(
        &mut self,
        message: &str,
        attachments: &[String],
        model: Option<&str>,
    ) -> HookResult {
        if !self.enabled {
            return HookResult::Continue;
        }

        let input = PreMessageInput {
            hook_event_name: "PreMessage".to_string(),
            cwd: self.cwd.clone(),
            session_id: self.session_id.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            message: message.to_string(),
            attachments: attachments.to_vec(),
            model: model.map(String::from),
        };

        self.log_event("PreMessage", &format!("len={}", message.len()));

        self.execute_with_timeout(|| self.registry.execute_pre_message(&input))
    }

    /// Execute `PostMessage` hooks - called after assistant response
    pub fn execute_post_message(
        &mut self,
        response: &str,
        input_tokens: u64,
        output_tokens: u64,
        duration_ms: u64,
        stop_reason: Option<&str>,
    ) -> HookResult {
        let input = PostMessageInput {
            hook_event_name: "PostMessage".to_string(),
            cwd: self.cwd.clone(),
            session_id: self.session_id.clone(),
            transcript_path: self.transcript_path.clone(),
            transcript_size_before: self.transcript_checkpoint_size,
            organization_id: self.organization_id.clone(),
            workspace_id: self.workspace_id.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            response: response.to_string(),
            input_tokens,
            output_tokens,
            duration_ms,
            stop_reason: stop_reason.map(String::from),
        };

        if let Some(hook) = &self.session_history {
            let _ = hook.on_post_message(&input);
        }
        self.transcript_checkpoint_size = self
            .transcript_path
            .as_deref()
            .and_then(|path| std::fs::metadata(path).ok())
            .map(|metadata| metadata.len())
            .or(self.transcript_checkpoint_size);
        if !self.enabled {
            return HookResult::Continue;
        }

        self.log_event(
            "PostMessage",
            &format!("tokens={input_tokens}+{output_tokens} duration={duration_ms}ms"),
        );

        self.registry.execute_post_message(&input)
    }

    /// Execute `OnError` hooks - called when an error occurs
    pub fn execute_on_error(
        &mut self,
        error: &str,
        error_kind: &str,
        context: Option<&str>,
        recoverable: bool,
    ) -> HookResult {
        if !self.enabled {
            return HookResult::Continue;
        }

        let input = OnErrorInput {
            hook_event_name: "OnError".to_string(),
            cwd: self.cwd.clone(),
            session_id: self.session_id.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            error: error.to_string(),
            error_kind: error_kind.to_string(),
            context: context.map(String::from),
            recoverable,
        };

        self.log_event(
            "OnError",
            &format!("kind={error_kind} recoverable={recoverable}"),
        );

        self.registry.execute_on_error(&input)
    }

    /// Execute StopFailure hooks - called when recovery cannot produce a valid completion
    pub fn execute_stop_failure(
        &mut self,
        error: &str,
        error_details: Option<&str>,
        last_assistant_message: Option<&str>,
    ) -> HookResult {
        if !self.enabled {
            return HookResult::Continue;
        }

        let input = StopFailureInput {
            hook_event_name: "StopFailure".to_string(),
            cwd: self.cwd.clone(),
            session_id: self.session_id.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            error: error.to_string(),
            error_details: error_details.map(String::from),
            last_assistant_message: last_assistant_message.map(String::from),
        };

        self.log_event("StopFailure", &format!("error={error}"));

        self.execute_with_timeout(|| self.registry.execute_stop_failure(&input))
    }

    /// Execute `EvalGate` hooks - called after tool execution for evaluation
    pub fn execute_eval_gate(
        &mut self,
        tool_name: &str,
        tool_call_id: &str,
        tool_input: &serde_json::Value,
        tool_output: &str,
    ) -> HookResult {
        if !self.enabled {
            return HookResult::Continue;
        }

        let input = EvalGateInput {
            hook_event_name: "EvalGate".to_string(),
            cwd: self.cwd.clone(),
            session_id: self.session_id.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            tool_name: tool_name.to_string(),
            tool_call_id: tool_call_id.to_string(),
            tool_input: tool_input.clone(),
            tool_output: tool_output.to_string(),
        };

        self.log_event("EvalGate", &format!("tool={tool_name}"));

        self.registry.execute_eval_gate(&input)
    }

    /// Execute `SubagentStart` hooks - called before spawning a subagent
    pub fn execute_subagent_start(
        &mut self,
        subagent_type: &str,
        task: &str,
        parent_agent_id: Option<&str>,
    ) -> HookResult {
        if !self.enabled {
            return HookResult::Continue;
        }

        let input = SubagentStartInput {
            hook_event_name: "SubagentStart".to_string(),
            cwd: self.cwd.clone(),
            session_id: self.session_id.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            subagent_type: subagent_type.to_string(),
            task: task.to_string(),
            parent_agent_id: parent_agent_id.map(String::from),
        };

        self.log_event("SubagentStart", &format!("type={subagent_type}"));

        self.registry.execute_subagent_start(&input)
    }

    /// Execute `SubagentStop` hooks - called when a subagent completes
    pub fn execute_subagent_stop(
        &mut self,
        subagent_type: &str,
        subagent_id: &str,
        result: Option<&str>,
        duration_ms: u64,
        success: bool,
    ) -> HookResult {
        if !self.enabled {
            return HookResult::Continue;
        }

        let input = SubagentStopInput {
            hook_event_name: "SubagentStop".to_string(),
            cwd: self.cwd.clone(),
            session_id: self.session_id.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            subagent_type: subagent_type.to_string(),
            subagent_id: subagent_id.to_string(),
            result: result.map(String::from),
            duration_ms,
            success,
        };

        self.log_event(
            "SubagentStop",
            &format!("type={subagent_type} success={success} duration={duration_ms}ms"),
        );

        self.registry.execute_subagent_stop(&input)
    }

    /// Execute `PermissionRequest` hooks - called when permission is required
    pub fn execute_permission_request(
        &mut self,
        tool_name: &str,
        tool_call_id: &str,
        tool_input: &serde_json::Value,
        reason: &str,
    ) -> HookResult {
        if !self.enabled {
            return HookResult::Continue;
        }

        let input = PermissionRequestInput {
            hook_event_name: "PermissionRequest".to_string(),
            cwd: self.cwd.clone(),
            session_id: self.session_id.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            tool_name: tool_name.to_string(),
            tool_call_id: tool_call_id.to_string(),
            tool_input: tool_input.clone(),
            reason: reason.to_string(),
        };

        self.log_event(
            "PermissionRequest",
            &format!("tool={tool_name} reason={reason}"),
        );

        self.registry.execute_permission_request(&input)
    }

    /// Get hook statistics
    #[must_use]
    pub fn stats(&self) -> HookStats {
        HookStats {
            native_hooks: self.registry.total_hook_count(),
            lua_scripts: self
                .lua_executor
                .as_ref()
                .map_or(0, super::lua::LuaHookExecutor::script_count),
            wasm_plugins: self
                .wasm_executor
                .as_ref()
                .map_or(0, super::wasm::WasmHookExecutor::plugin_count),
            enabled: self.enabled,
        }
    }

    /// Get execution metrics
    #[must_use]
    pub fn metrics(&self) -> &HookMetrics {
        &self.metrics
    }

    /// Reset metrics
    pub fn reset_metrics(&mut self) {
        self.metrics = HookMetrics::default();
    }

    /// Enable hooks
    pub fn enable(&mut self) {
        self.enabled = true;
    }

    /// Disable hooks
    pub fn disable(&mut self) {
        self.enabled = false;
    }

    /// Check if hooks are enabled
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Get session duration
    #[must_use]
    pub fn session_duration(&self) -> Option<Duration> {
        self.session_start.map(|s| s.elapsed())
    }

    /// Get turn count
    #[must_use]
    pub fn turn_count(&self) -> u32 {
        self.turn_count
    }
}

/// Statistics about loaded hooks
#[derive(Debug, Clone)]
pub struct HookStats {
    pub native_hooks: usize,
    pub lua_scripts: usize,
    pub wasm_plugins: usize,
    pub enabled: bool,
}

impl HookStats {
    /// Total number of hooks
    #[must_use]
    pub fn total(&self) -> usize {
        self.native_hooks + self.lua_scripts + self.wasm_plugins
    }
}

/// Execution metrics for hooks
#[derive(Debug, Clone, Default)]
pub struct HookMetrics {
    /// Number of `PreToolUse` hooks executed
    pub pre_tool_use_count: u64,
    /// Number of `PostToolUse` hooks executed
    pub post_tool_use_count: u64,
    /// Number of overflow events
    pub overflow_count: u64,
    /// Number of blocks
    pub blocks: u64,
    /// Total duration of hook execution
    pub total_duration: Duration,
}

impl HookMetrics {
    /// Average hook execution time
    #[must_use]
    pub fn average_duration(&self) -> Duration {
        let total_calls = self.pre_tool_use_count + self.post_tool_use_count;
        if total_calls == 0 {
            Duration::ZERO
        } else {
            self.total_duration / total_calls as u32
        }
    }
}

/// Result of reloading hooks
#[derive(Debug, Clone)]
pub struct ReloadResult {
    pub lua_scripts: usize,
    pub wasm_plugins: usize,
}

impl ReloadResult {
    #[must_use]
    pub fn total(&self) -> usize {
        self.lua_scripts + self.wasm_plugins
    }
}

#[cfg(test)]
mod tests {
    #[cfg(not(feature = "wasm"))]
    use crate::hooks::config::{HookDefinition, HookSettings};

    use super::*;

    #[test]
    fn test_integrated_system() {
        let system = IntegratedHookSystem::new("/tmp");
        assert!(system.enabled);

        let stats = system.stats();
        assert_eq!(stats.native_hooks, 0);
    }

    #[test]
    fn config_load_failure_blocks_pre_tool_use() {
        let mut system = IntegratedHookSystem::from_config_result(
            "/tmp",
            Err(anyhow::anyhow!("invalid project matcher")),
        );

        assert!(matches!(
            system.execute_pre_tool_use("Read", "call-1", &serde_json::json!({})),
            HookResult::Block { reason }
                if reason.contains("Hook configuration failed to load")
                    && reason.contains("invalid project matcher")
        ));
    }

    #[cfg(not(feature = "wasm"))]
    #[test]
    fn configured_required_wasm_is_not_active_and_fails_closed_without_backend() {
        let path = tempfile::tempdir()
            .unwrap()
            .path()
            .join("missing-required-policy.wasm");
        let config = LoadedHookConfig {
            settings: HookSettings::default(),
            hooks: vec![LoadedHook {
                definition: HookDefinition {
                    event: HookEventType::PreToolUse,
                    tools: vec!["Bash".to_string()],
                    command: None,
                    http: None,
                    prompt: None,
                    lua: None,
                    lua_file: None,
                    wasm: Some(path.to_string_lossy().into_owned()),
                    timeout_ms: None,
                    enabled: true,
                    required: None,
                    description: None,
                    working_dir: None,
                },
                source: HookSource::Wasm(path),
            }],
            source_paths: Vec::new(),
            skipped_untrusted_paths: Vec::new(),
        };
        let mut system = IntegratedHookSystem::new("/tmp");
        system.load_hooks_from_config(&config);

        assert_eq!(system.stats().wasm_plugins, 0);
        assert!(matches!(
            system.execute_pre_tool_use("bash", "call-1", &serde_json::json!({})),
            HookResult::Block { reason } if reason.contains("unavailable")
        ));
    }

    #[cfg(not(feature = "wasm"))]
    #[test]
    fn configured_advisory_wasm_remains_advisory_without_backend() {
        let plugin = tempfile::NamedTempFile::new().unwrap();
        let path = plugin.path().to_path_buf();
        let config = LoadedHookConfig {
            settings: HookSettings::default(),
            hooks: vec![LoadedHook {
                definition: HookDefinition {
                    event: HookEventType::PreToolUse,
                    tools: Vec::new(),
                    command: None,
                    http: None,
                    prompt: None,
                    lua: None,
                    lua_file: None,
                    wasm: Some(path.to_string_lossy().into_owned()),
                    timeout_ms: None,
                    enabled: true,
                    required: Some(false),
                    description: None,
                    working_dir: None,
                },
                source: HookSource::Wasm(path),
            }],
            source_paths: Vec::new(),
            skipped_untrusted_paths: Vec::new(),
        };
        let mut system = IntegratedHookSystem::new("/tmp");
        system.load_hooks_from_config(&config);

        assert_eq!(system.stats().wasm_plugins, 0);
        assert!(matches!(
            system.execute_pre_tool_use("Bash", "call-1", &serde_json::json!({})),
            HookResult::Continue
        ));
    }

    #[test]
    fn test_pre_tool_use_hook() {
        let mut system = IntegratedHookSystem::new("/tmp");
        system.registry.register_pre_tool_use(Arc::new(SafetyHook));

        // Safe command
        let result =
            system.execute_pre_tool_use("Bash", "123", &serde_json::json!({ "command": "ls -la" }));
        assert!(matches!(result, HookResult::Continue));

        // Dangerous command
        let result = system.execute_pre_tool_use(
            "Bash",
            "456",
            &serde_json::json!({ "command": "rm -rf /" }),
        );
        assert!(matches!(result, HookResult::Block { .. }));
    }

    #[test]
    fn test_session_lifecycle() {
        let mut system = IntegratedHookSystem::new("/tmp");

        system.on_session_start("cli");
        assert_eq!(system.turn_count(), 0);

        system.increment_turn();
        system.increment_turn();
        assert_eq!(system.turn_count(), 2);

        assert!(system.session_duration().is_some());

        system.on_session_end("user_exit");
    }

    #[test]
    fn test_metrics() {
        let mut system = IntegratedHookSystem::new("/tmp");

        system.execute_pre_tool_use("Read", "1", &serde_json::json!({}));
        system.execute_pre_tool_use("Write", "2", &serde_json::json!({}));
        system.execute_post_tool_use("Read", "1", &serde_json::json!({}), "ok", false, 0);

        let metrics = system.metrics();
        assert_eq!(metrics.pre_tool_use_count, 2);
        assert_eq!(metrics.post_tool_use_count, 1);
    }

    #[test]
    fn external_hook_output_supports_block_modify_and_context() {
        assert!(matches!(
            parse_external_hook_output(HookEventType::PreToolUse, r#"{"block":true,"reason":"nope"}"#),
            HookResult::Block { reason } if reason == "nope"
        ));
        assert!(matches!(
            parse_external_hook_output(HookEventType::PreToolUse, r#"{"modified_input":{"safe":true}}"#),
            HookResult::ModifyInput { new_input } if new_input == serde_json::json!({"safe": true})
        ));
        assert!(matches!(
            parse_external_hook_output(HookEventType::PreToolUse, r#"{"additional_context":"remember this"}"#),
            HookResult::InjectContext { context } if context == "remember this"
        ));
    }

    #[test]
    fn external_hook_output_honors_eval_gate_structured_fields() {
        assert!(matches!(
            parse_external_hook_output(
                HookEventType::EvalGate,
                r#"{"hookSpecificOutput":{"passed":false,"rationale":"bad result"}}"#
            ),
            HookResult::Block { reason } if reason == "bad result"
        ));
        assert!(matches!(
            parse_external_hook_output(
                HookEventType::EvalGate,
                r#"{"hookSpecificOutput":{"score":0.2,"threshold":0.8}}"#
            ),
            HookResult::Block { reason } if reason == "score 0.2 below threshold 0.8"
        ));
        assert!(matches!(
            parse_external_hook_output(
                HookEventType::EvalGate,
                r#"{"hookSpecificOutput":{"passed":true,"score":0.9,"threshold":0.8,"rationale":"looks good"}}"#
            ),
            HookResult::InjectContext { context }
                if context == "eval score 0.9 (threshold 0.8): looks good"
        ));
        assert!(matches!(
            parse_external_hook_output(
                HookEventType::EvalGate,
                r#"{"hookSpecificOutput":{"assertions":[{"name":"fmt","passed":false}]}}"#
            ),
            HookResult::Block { reason } if reason == "EvalGate assertion failed"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn external_command_hook_receives_input_and_can_block() {
        let hook = ExternalHook {
            event: HookEventType::PreToolUse,
            tools: ToolMatcher::compile(&["Bash".to_string()]).unwrap(),
            source: ExternalHookSource::Command(
                "printf '{\"block\":true,\"reason\":\"policy\"}'".to_string(),
            ),
            timeout: Duration::from_secs(1),
            working_dir: std::env::current_dir().unwrap(),
        };
        let input = PreToolUseInput {
            hook_event_name: "PreToolUse".to_string(),
            cwd: "/tmp".to_string(),
            session_id: None,
            timestamp: "now".to_string(),
            tool_name: "bash".to_string(),
            tool_call_id: "call-1".to_string(),
            tool_input: serde_json::json!({"command": "pwd"}),
        };

        assert!(matches!(
            hook.on_pre_tool_use(&input),
            HookResult::Block { reason } if reason == "policy"
        ));
    }

    #[test]
    fn external_hook_output_honors_the_documented_hook_specific_envelope() {
        // The contract in docs/design/HOOKS_SYSTEM.md ("Hook Output Format")
        // nests these fields under `hookSpecificOutput` using camelCase keys.
        assert!(matches!(
            parse_external_hook_output(
                HookEventType::PreToolUse,
                r#"{"continue":true,"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"writes outside the workspace"}}"#
            ),
            HookResult::Block { reason } if reason == "writes outside the workspace"
        ));
        assert!(matches!(
            parse_external_hook_output(
                HookEventType::PreToolUse,
                r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny"}}"#
            ),
            HookResult::Block { reason } if reason == "Blocked by external hook"
        ));
        // No interactive path exists for external hooks, so "ask" must refuse
        // rather than fall through to Continue.
        assert!(matches!(
            parse_external_hook_output(
                HookEventType::PreToolUse,
                r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"ask"}}"#
            ),
            HookResult::Block { .. }
        ));
        assert!(matches!(
            parse_external_hook_output(
                HookEventType::PreToolUse,
                r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow","modifiedInput":{"command":"ls"}}}"#
            ),
            HookResult::ModifyInput { new_input } if new_input == serde_json::json!({"command": "ls"})
        ));
        assert!(matches!(
            parse_external_hook_output(
                HookEventType::PostToolUse,
                r#"{"hookSpecificOutput":{"hookEventName":"PostToolUse","contextToAdd":"remember this"}}"#
            ),
            HookResult::InjectContext { context } if context == "remember this"
        ));
        assert!(matches!(
            parse_external_hook_output(
                HookEventType::PreToolUse,
                r#"{"continue":true,"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow"}}"#
            ),
            HookResult::Continue
        ));
        // The documented top-level envelope carries the block reason in
        // `message`, as in the design doc's own example hook script.
        assert!(matches!(
            parse_external_hook_output(HookEventType::PreToolUse, r#"{"continue":false,"message":"Dangerous command blocked"}"#),
            HookResult::Block { reason } if reason == "Dangerous command blocked"
        ));
        assert!(matches!(
            parse_external_hook_output(HookEventType::PreToolUse, r#"{"decision":"reject","message":"policy"}"#),
            HookResult::Block { reason } if reason == "policy"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn external_command_hook_denies_through_the_documented_envelope() {
        let hook = ExternalHook {
            event: HookEventType::PreToolUse,
            tools: ToolMatcher::compile(&["Bash".to_string()]).unwrap(),
            source: ExternalHookSource::Command(
                "printf '{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"deny\",\"permissionDecisionReason\":\"policy\"}}'"
                    .to_string(),
            ),
            timeout: Duration::from_secs(5),
            working_dir: std::env::current_dir().expect("current directory"),
        };
        let input = PreToolUseInput {
            hook_event_name: "PreToolUse".to_string(),
            cwd: "/tmp".to_string(),
            session_id: None,
            timestamp: "now".to_string(),
            tool_name: "bash".to_string(),
            tool_call_id: "call-envelope".to_string(),
            tool_input: serde_json::json!({"command": "rm -rf /"}),
        };

        assert!(matches!(
            hook.on_pre_tool_use(&input),
            HookResult::Block { reason } if reason == "policy"
        ));
    }

    #[cfg(unix)]
    fn external_command_hook(event: HookEventType, command: &str) -> ExternalHook {
        ExternalHook {
            event,
            tools: ToolMatcher::default(),
            source: ExternalHookSource::Command(command.to_string()),
            timeout: Duration::from_secs(5),
            working_dir: std::env::current_dir().expect("current directory"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn post_tool_use_hooks_are_routed_by_tool_result_status() {
        let mut system = IntegratedHookSystem::new("/tmp");
        system
            .registry
            .register_post_tool_use(Arc::new(external_command_hook(
                HookEventType::PostToolUse,
                "printf '{\"additional_context\":\"success-hook\"}'",
            )));
        system
            .registry
            .register_post_tool_use(Arc::new(external_command_hook(
                HookEventType::PostToolUseFailure,
                "printf '{\"additional_context\":\"failure-hook\"}'",
            )));

        assert!(matches!(
            system.execute_post_tool_use("bash", "1", &serde_json::json!({}), "ok", false, 0),
            HookResult::InjectContext { context } if context == "success-hook"
        ));
        assert!(matches!(
            system.execute_post_tool_use("bash", "2", &serde_json::json!({}), "boom", true, 0),
            HookResult::InjectContext { context } if context == "failure-hook"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn external_command_hook_that_ignores_stdin_is_killed_at_its_deadline() {
        let marker = std::env::temp_dir().join(format!(
            "maestro-hook-stdin-{}-{}.marker",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        ));
        let _ = std::fs::remove_file(&marker);
        let hook = ExternalHook {
            event: HookEventType::PreToolUse,
            tools: ToolMatcher::default(),
            // Never reads stdin and outlives the deadline. If the payload
            // write runs ahead of the killable wait it blocks once the pipe
            // fills, the kill never happens, and this command runs to
            // completion after the hook has already reported a timeout.
            source: ExternalHookSource::Command(format!("sleep 1; : > {}", marker.display())),
            timeout: Duration::from_millis(200),
            working_dir: std::env::current_dir().expect("current directory"),
        };
        let input = PreToolUseInput {
            hook_event_name: "PreToolUse".to_string(),
            cwd: "/tmp".to_string(),
            session_id: None,
            timestamp: "now".to_string(),
            // Larger than a 64 KiB pipe buffer, so the payload write blocks
            // until the child reads it, which this command never does.
            tool_input: serde_json::json!({ "command": "a".repeat(90 * 1024) }),
            tool_name: "bash".to_string(),
            tool_call_id: "call-stdin".to_string(),
        };

        assert!(matches!(
            hook.on_pre_tool_use(&input),
            HookResult::Block { reason } if reason.contains("timed out")
        ));
        std::thread::sleep(Duration::from_millis(1500));
        let survived = marker.exists();
        let _ = std::fs::remove_file(&marker);
        assert!(
            !survived,
            "a hook killed at its deadline must not keep running to completion"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_oversized_hook_payload_reaches_stdin_and_stays_out_of_the_environment() {
        // Above `MAX_ARG_STRLEN` (128 KiB), so passing this in the
        // environment fails the spawn with E2BIG on Linux and the hook never
        // runs at all.
        let payload = serde_json::to_vec(&serde_json::json!({
            "command": "a".repeat(3 * MAX_HOOK_INPUT_JSON_ENV_BYTES)
        }))
        .expect("payload should serialize");
        assert!(payload.len() > 128 * 1024);

        // Reports the byte counts rather than the payload itself; echoing a
        // 192 KiB payload back would trip the output bound instead.
        let result = run_external_command(
            "printf '{\"additional_context\":\"%s/%s/%s\"}' \
             \"${#INPUT_JSON}\" \"${INPUT_JSON_OMITTED:-unset}\" \"$(cat | wc -c | tr -d ' ')\"",
            &payload,
            HookEventType::PreToolUse,
            None,
            &std::env::current_dir().expect("current directory"),
            Duration::from_secs(10),
        );

        let expected = format!("0/{}/{}", payload.len(), payload.len());
        assert!(
            matches!(&result, HookResult::InjectContext { context } if context == &expected),
            "expected {expected:?}, got {result:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_small_hook_payload_is_still_offered_in_the_environment() {
        let payload =
            serde_json::to_vec(&serde_json::json!({"command": "pwd"})).expect("payload serializes");

        let result = run_external_command(
            "printf '{\"additional_context\":\"%s/%s\"}' \
             \"${#INPUT_JSON}\" \"${INPUT_JSON_OMITTED:-unset}\"",
            &payload,
            HookEventType::PreToolUse,
            None,
            &std::env::current_dir().expect("current directory"),
            Duration::from_secs(10),
        );

        let expected = format!("{}/unset", payload.len());
        assert!(
            matches!(&result, HookResult::InjectContext { context } if context == &expected),
            "expected {expected:?}, got {result:?}"
        );
    }

    #[test]
    fn external_hook_payload_uses_the_documented_camel_case_field_names() {
        let input = PreToolUseInput {
            hook_event_name: "PreToolUse".to_string(),
            cwd: "/workspace".to_string(),
            session_id: Some("session-1".to_string()),
            timestamp: "now".to_string(),
            tool_name: "bash".to_string(),
            tool_call_id: "call-1".to_string(),
            // A tool argument that is itself snake_case: the contract only
            // covers the envelope, and renaming tool parameters would break
            // the hook's view of the call.
            tool_input: serde_json::json!({"file_path": "src/main.rs"}),
        };

        let payload = external_hook_payload(&input).expect("payload should serialize");
        let payload: serde_json::Value =
            serde_json::from_slice(&payload).expect("payload should be JSON");

        // The names docs/design/HOOKS_SYSTEM.md tells hook authors to read.
        assert_eq!(payload["hookEventName"], "PreToolUse");
        assert_eq!(payload["toolName"], "bash");
        assert_eq!(payload["toolCallId"], "call-1");
        assert_eq!(payload["sessionId"], "session-1");
        assert_eq!(payload["toolInput"]["file_path"], "src/main.rs");
        assert_eq!(payload["cwd"], "/workspace");
        assert_eq!(payload["timestamp"], "now");
        // The shipped snake_case names stay available for hooks written
        // against the previous behavior.
        assert_eq!(payload["tool_name"], "bash");
        assert_eq!(payload["hook_event_name"], "PreToolUse");
        assert_eq!(payload["tool_input"]["file_path"], "src/main.rs");
    }

    fn post_tool_use_input(is_error: bool) -> PostToolUseInput {
        PostToolUseInput {
            hook_event_name: if is_error {
                "PostToolUseFailure".to_string()
            } else {
                "PostToolUse".to_string()
            },
            cwd: "/workspace".to_string(),
            session_id: Some("session-1".to_string()),
            timestamp: "now".to_string(),
            tool_name: "bash".to_string(),
            tool_call_id: "call-1".to_string(),
            tool_input: serde_json::json!({"command": "false"}),
            tool_output: "command failed".to_string(),
            is_error,
            duration_ms: 17,
        }
    }

    #[test]
    fn one_deadline_bounds_every_stream_drained_under_it() {
        // Drive elapsed time at the blocking boundary. A real wall-clock upper
        // bound also counts unrelated OS scheduling delays on busy CI hosts.
        let started = Instant::now();
        let deadline = started + HOOK_OUTPUT_DRAIN_GRACE;
        let now = std::cell::Cell::new(started);
        let mut allowances = Vec::new();
        for stream in ["stdout", "stderr"] {
            let (_sender, finished) = mpsc::channel();
            let reader = HookOutputReader {
                buffer: Arc::new(Mutex::new(HookOutputBuffer::default())),
                finished,
            };
            let output = collect_hook_output_with_wait(
                Some(reader),
                stream,
                deadline,
                now.get(),
                |_, remaining| {
                    allowances.push(remaining);
                    now.set(now.get() + remaining);
                    Err(RecvTimeoutError::Timeout)
                },
            )
            .expect("stream drain should not fail");
            assert!(output.text.is_empty());
        }
        assert_eq!(allowances, [HOOK_OUTPUT_DRAIN_GRACE, Duration::ZERO]);
        assert_eq!(now.get(), deadline, "both streams share one drain budget");
    }

    #[derive(Clone, Default)]
    struct RecordingPostToolUseHook {
        seen_event_names: Arc<Mutex<Vec<String>>>,
    }

    impl super::PostToolUseHook for RecordingPostToolUseHook {
        fn on_post_tool_use(&self, input: &PostToolUseInput) -> HookResult {
            if let Ok(mut seen) = self.seen_event_names.lock() {
                seen.push(input.hook_event_name.clone());
            }
            HookResult::Continue
        }
    }

    #[test]
    fn a_failed_tool_call_is_reported_as_post_tool_use_failure() {
        // `PostToolUse` and `PostToolUseFailure` share this path and are
        // separated by the result status. Reporting `PostToolUse` in both cases
        // made the payload contradict the hook that was selected, so a failure
        // hook branching on the event name took its success branch.
        let recorder = RecordingPostToolUseHook::default();
        let mut system = IntegratedHookSystem::new("/tmp");
        system
            .registry
            .register_post_tool_use(Arc::new(recorder.clone()));

        system.execute_post_tool_use("bash", "1", &serde_json::json!({}), "boom", true, 0);
        system.execute_post_tool_use("bash", "2", &serde_json::json!({}), "ok", false, 0);

        let seen = recorder.seen_event_names.lock().expect("recorder lock");
        assert_eq!(
            seen.as_slice(),
            ["PostToolUseFailure".to_string(), "PostToolUse".to_string()]
        );
    }

    #[test]
    fn post_tool_use_payload_carries_the_session_id_and_duration() {
        // `sessionId` was camel-cased correctly but always null, because
        // nothing set it; `durationMs` was documented but had no field at all.
        let payload =
            external_hook_payload(&post_tool_use_input(false)).expect("payload should serialize");
        let payload: serde_json::Value =
            serde_json::from_slice(&payload).expect("payload should be JSON");

        assert_eq!(payload["sessionId"], "session-1");
        assert_eq!(payload["durationMs"], 17);
        assert_eq!(payload["duration_ms"], 17);
    }

    #[test]
    fn a_hook_system_reports_the_session_it_was_given() {
        let mut system = IntegratedHookSystem::new("/tmp");
        assert_eq!(system.session_id(), None);

        system.set_session_id(Some("alpha".to_string()));

        assert_eq!(system.session_id(), Some("alpha"));
    }

    #[test]
    fn post_tool_use_payload_nests_tool_output_as_documented() {
        // docs/design/HOOKS_SYSTEM.md documents `toolOutput` as
        // `{ content, isError }`. Serializing the struct published it as a bare
        // string with a sibling `isError`, so a hook reading the documented
        // `.toolOutput.isError` path saw null and treated a failed tool call as
        // a success.
        let payload =
            external_hook_payload(&post_tool_use_input(true)).expect("payload should serialize");
        let payload: serde_json::Value =
            serde_json::from_slice(&payload).expect("payload should be JSON");

        assert_eq!(payload["toolOutput"]["isError"], true);
        assert_eq!(payload["toolOutput"]["content"][0]["type"], "text");
        assert_eq!(
            payload["toolOutput"]["content"][0]["text"],
            "command failed"
        );
        // The shipped flat keys stay available for hooks written against the
        // previous behavior.
        assert_eq!(payload["tool_output"], "command failed");
        assert_eq!(payload["is_error"], true);
    }

    #[test]
    fn post_tool_use_payload_reports_success_in_the_nested_shape() {
        let mut input = post_tool_use_input(false);
        input.tool_output = "ok".to_string();

        let payload = external_hook_payload(&input).expect("payload should serialize");
        let payload: serde_json::Value =
            serde_json::from_slice(&payload).expect("payload should be JSON");

        assert_eq!(payload["toolOutput"]["isError"], false);
        assert_eq!(payload["toolOutput"]["content"][0]["text"], "ok");
    }

    #[cfg(unix)]
    #[test]
    fn a_command_hook_reads_the_documented_nested_tool_output() {
        // The documented failure-hook shape: branch on `.toolOutput.isError`.
        // The marker is the nested object, not a bare `"isError":true`, which
        // the envelope's camelCase rename produces on its own.
        let hook = external_command_hook(
            HookEventType::PostToolUseFailure,
            "grep -q '\"toolOutput\":{\"content\"' \
             && printf '{\"block\":true,\"reason\":\"read isError\"}'",
        );

        assert!(matches!(
            hook.on_post_tool_use(&post_tool_use_input(true)),
            HookResult::Block { reason } if reason == "read isError"
        ));
    }

    #[test]
    fn external_hook_payload_camel_cases_multi_word_keys_only() {
        assert_eq!(camel_case_key("duration_ms").as_deref(), Some("durationMs"));
        assert_eq!(
            camel_case_key("hook_event_name").as_deref(),
            Some("hookEventName")
        );
        assert_eq!(camel_case_key("cwd"), None);
        assert_eq!(camel_case_key("prompt"), None);
        assert_eq!(camel_case_key("toolName"), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_command_hook_reads_the_documented_field_names_from_stdin() {
        // The documented policy-hook shape: decide on `.toolName`. Before the
        // payload carried that key this grep never matched, so a hook written
        // from the documentation failed open.
        let hook = external_command_hook(
            HookEventType::PreToolUse,
            "grep -q '\"toolName\":\"bash\"' && printf '{\"block\":true,\"reason\":\"read toolName\"}'",
        );
        let input = PreToolUseInput {
            hook_event_name: "PreToolUse".to_string(),
            cwd: "/tmp".to_string(),
            session_id: None,
            timestamp: "now".to_string(),
            tool_name: "bash".to_string(),
            tool_call_id: "call-contract".to_string(),
            tool_input: serde_json::json!({"command": "pwd"}),
        };

        assert!(matches!(
            hook.on_pre_tool_use(&input),
            HookResult::Block { reason } if reason == "read toolName"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn session_end_hook_receives_the_active_transcript_path() {
        // Session History can only capture Maestro's canonical JSONL when the
        // lifecycle adapter receives the exact active file. A session id alone
        // is insufficient because callers may use a custom sessions directory.
        let hook = external_command_hook(
            HookEventType::SessionEnd,
            "grep -q '\"transcriptPath\":\"/tmp/maestro-session.jsonl\"' && \
             printf '{\"block\":true,\"reason\":\"read transcript path\"}'",
        );
        let mut system = IntegratedHookSystem::new("/tmp");
        system.registry.register_session_end(Arc::new(hook));
        system.set_session_context(
            Some("session-history-1".to_string()),
            Some("/tmp/maestro-session.jsonl".to_string()),
        );
        system.set_identity_context(
            Some("org-identity".to_string()),
            Some("workspace-identity".to_string()),
        );

        assert!(matches!(
            system.on_session_end("exit"),
            HookResult::Block { reason } if reason == "read transcript path"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn post_message_hook_receives_the_active_transcript_path() {
        // A completed turn must be independently spoolable before SessionEnd;
        // otherwise a crash after this hook loses the latest durable JSONL.
        let hook = external_command_hook(
            HookEventType::PostMessage,
            "body=$(cat); printf '%s' \"$body\" | grep -q '\"transcriptPath\":\"/tmp/maestro-session.jsonl\"' && \
             printf '%s' \"$body\" | grep -q '\"organizationId\":\"org-identity\"' && \
             printf '%s' \"$body\" | grep -q '\"workspaceId\":\"workspace-identity\"' && \
             printf '{\"block\":true,\"reason\":\"read transcript path\"}'",
        );
        let mut system = IntegratedHookSystem::new("/tmp");
        system.registry.register_post_message(Arc::new(hook));
        system.set_session_context(
            Some("session-history-1".to_string()),
            Some("/tmp/maestro-session.jsonl".to_string()),
        );
        system.set_identity_context(
            Some("org-identity".to_string()),
            Some("workspace-identity".to_string()),
        );

        assert!(matches!(
            system.execute_post_message("done", 10, 5, 100, Some("stop")),
            HookResult::Block { reason } if reason == "read transcript path"
        ));
    }

    #[test]
    fn authenticated_session_history_is_a_builtin_hook() {
        let temp = tempfile::tempdir().unwrap();
        let transcript = temp.path().join("session.jsonl");
        std::fs::write(&transcript, "").unwrap();
        let state = temp.path().join("session-history");
        let mut system = IntegratedHookSystem::new(temp.path().to_str().unwrap());
        system.enable_authenticated_session_history(
            "org-identity".to_string(),
            "workspace-identity".to_string(),
            "access-token".to_string(),
            None,
            state.clone(),
        );
        system.disable();
        system.set_session_context(
            Some("maestro-native-session".to_string()),
            Some(transcript.to_string_lossy().into_owned()),
        );
        std::fs::write(&transcript, "{\"type\":\"user\",\"text\":\"go\"}\n").unwrap();
        system.checkpoint_transcript_before_response();
        let started = Instant::now();
        assert!(matches!(
            system.execute_post_message("captured", 1, 1, 1, Some("stop")),
            HookResult::Continue
        ));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "built-in capture must not block the task that delivers ResponseEnd"
        );

        // Model the real TUI ordering: only after PostMessage returns can the
        // app consume ResponseEnd and flush the assistant entry.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap();
        file.write_all(b"{\"type\":\"assistant\",\"text\":\"captured\"}\n")
            .unwrap();
        file.flush().unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let manifests = loop {
            let manifests = std::fs::read_dir(state.join("transcripts"))
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .map(|entry| entry.path().join("manifest.json"))
                .filter(|path| path.is_file())
                .collect::<Vec<_>>();
            if !manifests.is_empty() || Instant::now() >= deadline {
                break manifests;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(manifests.len(), 1);
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifests[0]).unwrap()).unwrap();
        assert_eq!(manifest["organization_id"], "org-identity");
        assert_eq!(manifest["workspace_id"], "workspace-identity");
    }

    #[cfg(unix)]
    #[test]
    fn a_hook_that_backgrounds_a_descendant_returns_without_waiting_for_it() {
        // The shell answers and exits immediately, but the backgrounded
        // descendant inherits stdout and holds the pipe open for 10s.
        // Joining the readers unconditionally blocked here until the hook's
        // 5s deadline elapsed, and the caller saw a timeout instead of the
        // answer the hook had already written.
        let hook = external_command_hook(
            HookEventType::PreToolUse,
            "printf '{\"additional_context\":\"answered\"}'; sleep 10 &",
        );
        let input = PreToolUseInput {
            hook_event_name: "PreToolUse".to_string(),
            cwd: "/tmp".to_string(),
            session_id: None,
            timestamp: "now".to_string(),
            tool_name: "bash".to_string(),
            tool_call_id: "call-daemon".to_string(),
            tool_input: serde_json::json!({"command": "pwd"}),
        };

        let started = Instant::now();
        let result = hook.on_pre_tool_use(&input);
        let elapsed = started.elapsed();

        assert!(
            matches!(&result, HookResult::InjectContext { context } if context == "answered"),
            "expected the hook's own answer, got {result:?}"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "collection waited {elapsed:?} on a descendant that holds the pipe"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_timed_out_hook_takes_its_backgrounded_descendant_with_it() {
        let marker = std::env::temp_dir().join(format!(
            "maestro-hook-group-{}-{}.marker",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        ));
        let _ = std::fs::remove_file(&marker);
        // The descendant outlives the shell. Killing only the shell at the
        // deadline left it running, and it created the marker afterwards.
        let hook = ExternalHook {
            event: HookEventType::PreToolUse,
            tools: ToolMatcher::default(),
            source: ExternalHookSource::Command(format!(
                "(sleep 1; : > {}) & sleep 5",
                marker.display()
            )),
            timeout: Duration::from_millis(200),
            working_dir: std::env::current_dir().expect("current directory"),
        };
        let input = PreToolUseInput {
            hook_event_name: "PreToolUse".to_string(),
            cwd: "/tmp".to_string(),
            session_id: None,
            timestamp: "now".to_string(),
            tool_name: "bash".to_string(),
            tool_call_id: "call-group".to_string(),
            tool_input: serde_json::json!({"command": "pwd"}),
        };

        assert!(matches!(
            hook.on_pre_tool_use(&input),
            HookResult::Block { reason } if reason.contains("timed out")
        ));
        std::thread::sleep(Duration::from_secs(2));
        let descendant_survived = marker.exists();
        let _ = std::fs::remove_file(&marker);
        assert!(
            !descendant_survived,
            "a hook killed at its deadline must not leave a descendant running"
        );
    }

    #[test]
    fn http_hook_rejects_an_unbounded_response_body() {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("test hook server should bind");
        let address = listener.local_addr().expect("test hook server address");
        let server = std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut request = [0_u8; 8192];
            let _ = stream.read(&mut request);
            let body_len = MAX_EXTERNAL_HOOK_OUTPUT_BYTES * 4;
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {body_len}\r\n\r\n"
                )
                .as_bytes(),
            );
            let chunk = vec![b'a'; 64 * 1024];
            let mut written = 0;
            while written < body_len {
                if stream.write_all(&chunk).is_err() {
                    break;
                }
                written += chunk.len();
            }
        });

        let result = run_external_http(
            &format!("http://{address}/hook"),
            b"{}",
            HookEventType::PreToolUse,
            Duration::from_secs(10),
        );
        assert!(
            matches!(&result, HookResult::Block { reason } if reason.contains("exceeded")),
            "unbounded hook body should be rejected, got {result:?}"
        );
        let _ = server.join();
    }

    #[cfg(unix)]
    #[test]
    fn external_command_hook_drains_both_pipes_with_a_bounded_capture() {
        let hook = ExternalHook {
            event: HookEventType::PreToolUse,
            tools: ToolMatcher::default(),
            source: ExternalHookSource::Command(
                "head -c 2000000 /dev/zero; head -c 2000000 /dev/zero >&2; printf '{\"additional_context\":\"ok\"}'"
                    .to_string(),
            ),
            timeout: Duration::from_secs(2),
            working_dir: std::env::current_dir().unwrap(),
        };
        let input = PreToolUseInput {
            hook_event_name: "PreToolUse".to_string(),
            cwd: "/tmp".to_string(),
            session_id: None,
            timestamp: "now".to_string(),
            tool_name: "bash".to_string(),
            tool_call_id: "call-2".to_string(),
            tool_input: serde_json::json!({"command": "pwd"}),
        };

        assert!(matches!(
            hook.on_pre_tool_use(&input),
            HookResult::Block { reason } if reason.contains("output exceeded")
        ));
    }

    #[test]
    fn non_json_stdout_is_not_injected_into_context_under_strict_mode() {
        let result = parse_external_hook_output_with_mode(
            HookEventType::PreToolUse,
            "+ rm -rf /tmp/scratch",
            HookOutputMode::Strict,
        );
        match result {
            HookResult::Block { reason } => assert!(reason.contains("not JSON"), "{reason}"),
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn non_json_stdout_still_injects_under_lenient_mode() {
        assert!(matches!(
            parse_external_hook_output_with_mode(
                HookEventType::PreToolUse,
                "note for the model",
                HookOutputMode::Lenient,
            ),
            HookResult::InjectContext { context } if context == "note for the model"
        ));
    }

    #[test]
    fn unknown_key_is_refused_in_strict_mode_and_warned_in_lenient_mode() {
        let payload = r#"{"modifedInput":{"command":"ls"}}"#;
        match parse_external_hook_output_with_mode(
            HookEventType::PreToolUse,
            payload,
            HookOutputMode::Strict,
        ) {
            HookResult::Block { reason } => assert!(reason.contains("modifedInput"), "{reason}"),
            other => panic!("expected Block, got {other:?}"),
        }
        assert!(matches!(
            parse_external_hook_output_with_mode(
                HookEventType::PreToolUse,
                payload,
                HookOutputMode::Lenient,
            ),
            HookResult::Continue
        ));
    }

    #[test]
    fn out_of_domain_permission_is_refused_in_strict_mode() {
        let payload = r#"{"hookSpecificOutput":{"permissionDecision":"ask"}}"#;
        match parse_external_hook_output_with_mode(
            HookEventType::SessionStart,
            payload,
            HookOutputMode::Strict,
        ) {
            HookResult::Block { reason } => assert!(reason.contains("SessionStart"), "{reason}"),
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn non_object_modified_input_is_refused_in_strict_mode() {
        match parse_external_hook_output_with_mode(
            HookEventType::PreToolUse,
            r#"{"modifiedInput":"rm -rf /"}"#,
            HookOutputMode::Strict,
        ) {
            HookResult::Block { reason } => assert!(reason.contains("JSON object"), "{reason}"),
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn hook_only_fires_for_tools_its_regex_matches() {
        let hook = ExternalHook {
            event: HookEventType::PreToolUse,
            tools: ToolMatcher::compile(&["Write.*".to_string()]).unwrap(),
            source: ExternalHookSource::Command("true".to_string()),
            timeout: Duration::from_secs(1),
            working_dir: std::env::current_dir().unwrap(),
        };
        assert!(hook.matches_tool("Write"));
        assert!(hook.matches_tool("WriteFile"));
        assert!(!hook.matches_tool("Read"));
    }
}
