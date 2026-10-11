//! Command hook execution with bounded output and process deadlines.

use super::integration::{
    CapturedHookOutput, MAX_EXTERNAL_HOOK_OUTPUT_BYTES, parse_external_hook_output,
};
use super::types::{HookEventType, HookResult};
#[cfg(not(unix))]
use std::io::Read;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command as ProcessCommand, Stdio};
#[cfg(any(not(unix), test))]
use std::sync::{
    Arc, Mutex,
    mpsc::{self, RecvTimeoutError},
};
use std::time::Duration;
#[cfg(any(not(unix), test))]
use std::time::Instant;
#[cfg(not(unix))]
use wait_timeout::ChildExt;

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
pub(super) const MAX_HOOK_INPUT_JSON_ENV_BYTES: usize = 64 * 1024;

/// How long to keep draining a hook's stdout/stderr once the hook itself has
/// finished or been killed.
///
/// A hook can leave a backgrounded descendant holding the pipes open, in which
/// case collection never observes EOF. This bounds that wait; the
/// partial capture retains the bytes ready at the bound. 100ms
/// matches the slack `ExternalHook::execute` already allows past `timeout`
/// before it abandons the worker, so a bounded drain cannot by itself turn a
/// hook that answered in time into a reported timeout.
const HOOK_OUTPUT_DRAIN_GRACE: Duration = Duration::from_millis(100);

pub(super) fn run_external_command(
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

    #[cfg(unix)]
    let (status, timed_out, wait_error, stdout, stderr) = {
        let output = match unix::capture(
            &mut child,
            timeout,
            HOOK_OUTPUT_DRAIN_GRACE,
            MAX_EXTERNAL_HOOK_OUTPUT_BYTES,
            terminate_hook_process,
        ) {
            Ok(output) => output,
            Err((stream, error)) => {
                return HookResult::Block {
                    reason: format!("Failed reading external {event_name} hook {stream}: {error}"),
                };
            }
        };
        let captured = |(bytes, truncated): (Vec<u8>, bool)| CapturedHookOutput {
            text: String::from_utf8_lossy(&bytes).into_owned(),
            truncated,
        };
        (
            output.status,
            output.timed_out,
            output.wait_error,
            captured(output.stdout),
            captured(output.stderr),
        )
    };
    #[cfg(not(unix))]
    let (status, timed_out, wait_error, stdout, stderr) = {
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
        (status, timed_out, wait_error, stdout, stderr)
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

#[cfg(any(not(unix), test))]
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
#[cfg(any(not(unix), test))]
struct HookOutputReader {
    buffer: Arc<Mutex<HookOutputBuffer>>,
    finished: mpsc::Receiver<std::io::Result<()>>,
}

#[cfg(not(unix))]
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
#[cfg(not(unix))]
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
        |finished, remaining| finished.recv_timeout(remaining),
    )
}

#[cfg(any(not(unix), test))]
fn collect_hook_output_with_wait(
    reader: Option<HookOutputReader>,
    stream_name: &str,
    deadline: Instant,
    now: Instant,
    wait: impl FnOnce(
        &mpsc::Receiver<std::io::Result<()>>,
        Duration,
    ) -> Result<std::io::Result<()>, RecvTimeoutError>,
) -> Result<CapturedHookOutput, String> {
    let Some(reader) = reader else {
        return Ok(CapturedHookOutput::default());
    };
    let remaining = deadline.saturating_duration_since(now);
    match wait(&reader.finished, remaining) {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}

#[cfg(unix)]
mod unix {
    use std::io::{self, Read};
    use std::os::fd::AsRawFd;
    use std::process::{Child, ExitStatus};
    use std::time::{Duration, Instant};
    use wait_timeout::ChildExt;

    const POLL_INTERVAL: Duration = Duration::from_millis(5);

    pub(super) struct CommandOutput {
        pub status: Option<ExitStatus>,
        pub timed_out: bool,
        pub wait_error: Option<String>,
        pub stdout: (Vec<u8>, bool),
        pub stderr: (Vec<u8>, bool),
    }

    struct OutputPipe<R> {
        reader: R,
        bytes: Vec<u8>,
        truncated: bool,
        closed: bool,
        limit: usize,
    }

    impl<R: Read> OutputPipe<R> {
        fn new(reader: R, limit: usize) -> Self {
            Self {
                reader,
                bytes: Vec::new(),
                truncated: false,
                closed: false,
                limit,
            }
        }

        /// Read ready bytes before consulting a deadline, including after expiry.
        /// Each pass is bounded even if a descendant writes continuously. Discard
        /// excess bytes while supervising so a full pipe cannot prevent child exit.
        fn drain_ready(&mut self) -> io::Result<()> {
            if self.closed {
                return Ok(());
            }
            let mut chunk = [0_u8; 8192];
            let budget = self.limit.saturating_add(1);
            let mut remaining = budget;
            for _ in 0..budget {
                let read_limit = remaining.min(chunk.len());
                match self.reader.read(&mut chunk[..read_limit]) {
                    Ok(0) => {
                        self.closed = true;
                        break;
                    }
                    Ok(read) => {
                        let keep = read.min(self.limit.saturating_sub(self.bytes.len()));
                        self.bytes.extend_from_slice(&chunk[..keep]);
                        self.truncated |= keep < read;
                        remaining -= read;
                        if remaining == 0 {
                            break;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                }
            }
            Ok(())
        }
    }

    fn nonblocking<R: Read + AsRawFd>(reader: R, limit: usize) -> io::Result<OutputPipe<R>> {
        let fd = reader.as_raw_fd();
        // SAFETY: reader owns this live descriptor for both fcntl calls. F_GETFL
        // and F_SETFL only inspect/update its flags and do not access memory.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the descriptor remains live and its existing flags are preserved.
        if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(OutputPipe::new(reader, limit))
    }

    fn drain<R: Read>(pipe: &mut Option<OutputPipe<R>>) -> io::Result<()> {
        if let Some(pipe) = pipe {
            pipe.drain_ready()?;
        }
        Ok(())
    }

    fn closed<R>(pipe: &Option<OutputPipe<R>>) -> bool {
        pipe.as_ref().is_none_or(|pipe| pipe.closed)
    }

    fn collected<R>(pipe: Option<OutputPipe<R>>) -> (Vec<u8>, bool) {
        pipe.map_or_else(|| (Vec::new(), false), |pipe| (pipe.bytes, pipe.truncated))
    }

    pub(super) fn capture(
        child: &mut Child,
        timeout: Duration,
        grace: Duration,
        limit: usize,
        terminate: fn(&mut Child),
    ) -> Result<CommandOutput, (&'static str, io::Error)> {
        let deadline = Instant::now() + timeout;
        let result = (|| {
            let mut stdout = child
                .stdout
                .take()
                .map(|pipe| nonblocking(pipe, limit))
                .transpose()
                .map_err(|error| ("stdout", error))?;
            let mut stderr = child
                .stderr
                .take()
                .map(|pipe| nonblocking(pipe, limit))
                .transpose()
                .map_err(|error| ("stderr", error))?;
            let mut timed_out = false;
            let mut wait_error = None;
            let status = loop {
                drain(&mut stdout).map_err(|error| ("stdout", error))?;
                drain(&mut stderr).map_err(|error| ("stderr", error))?;
                match child.try_wait() {
                    Ok(Some(status)) => break Some(status),
                    Ok(None) => {}
                    Err(error) => {
                        wait_error = Some(error.to_string());
                        terminate(child);
                        break None;
                    }
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    timed_out = true;
                    terminate(child);
                    break None;
                }
                match child.wait_timeout(remaining.min(POLL_INTERVAL)) {
                    Ok(Some(status)) => break Some(status),
                    Ok(None) => {}
                    Err(error) => {
                        wait_error = Some(error.to_string());
                        terminate(child);
                        break None;
                    }
                }
            };

            // A single cutoff covers both streams. Always read ready bytes first:
            // a scheduler delay beyond the cutoff must not erase the child's answer.
            drain_until(&mut stdout, &mut stderr, Instant::now() + grace)?;
            Ok(CommandOutput {
                status,
                timed_out,
                wait_error,
                stdout: collected(stdout),
                stderr: collected(stderr),
            })
        })();
        if result.is_err() {
            terminate(child);
        }
        result
    }

    fn drain_until<O: Read, E: Read>(
        stdout: &mut Option<OutputPipe<O>>,
        stderr: &mut Option<OutputPipe<E>>,
        deadline: Instant,
    ) -> Result<(), (&'static str, io::Error)> {
        loop {
            drain(stdout).map_err(|error| ("stdout", error))?;
            drain(stderr).map_err(|error| ("stderr", error))?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if (closed(stdout) && closed(stderr)) || remaining.is_zero() {
                return Ok(());
            }
            std::thread::sleep(remaining.min(POLL_INTERVAL));
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        struct OpenPipe(io::Cursor<Vec<u8>>);

        impl Read for OpenPipe {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                match self.0.read(bytes)? {
                    0 => Err(io::ErrorKind::WouldBlock.into()),
                    read => Ok(read),
                }
            }
        }

        #[test]
        fn ready_json_survives_an_expired_cutoff_without_eof() {
            let json = br#"{"additional_context":"answered"}"#.to_vec();
            let mut stdout = Some(OutputPipe::new(
                OpenPipe(io::Cursor::new(json.clone())),
                1024,
            ));
            let mut stderr = Some(OutputPipe::new(OpenPipe(io::Cursor::new(Vec::new())), 1024));
            drain_until(&mut stdout, &mut stderr, Instant::now()).unwrap();
            let (bytes, truncated) = collected(stdout);
            assert_eq!(bytes, json);
            assert!(!truncated);
            assert!(matches!(
                crate::hooks::integration::parse_external_hook_output(
                    crate::hooks::types::HookEventType::PreToolUse,
                    std::str::from_utf8(&bytes).unwrap(),
                ),
                crate::hooks::types::HookResult::InjectContext { context } if context == "answered"
            ));
        }

        #[test]
        fn an_empty_open_pipe_returns_at_the_cutoff() {
            let mut stdout = Some(OutputPipe::new(OpenPipe(io::Cursor::new(Vec::new())), 1024));
            let mut stderr = None::<OutputPipe<OpenPipe>>;
            drain_until(&mut stdout, &mut stderr, Instant::now()).unwrap();
            let pipe = stdout.unwrap();
            assert!(pipe.bytes.is_empty());
            assert!(!pipe.closed, "WouldBlock must not be treated as EOF");
        }

        #[test]
        fn an_expired_cutoff_preserves_the_output_limit() {
            let mut stdout = Some(OutputPipe::new(
                OpenPipe(io::Cursor::new(vec![b'x'; 1025])),
                1024,
            ));
            let mut stderr = None::<OutputPipe<OpenPipe>>;
            drain_until(&mut stdout, &mut stderr, Instant::now()).unwrap();
            let (bytes, truncated) = collected(stdout);
            assert_eq!(bytes.len(), 1024);
            assert!(truncated);
        }
    }
}
