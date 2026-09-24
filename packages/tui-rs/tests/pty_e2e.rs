//! PTY end-to-end tests for the interactive TUI.
//!
//! Adopted from grok-build's mock-model PTY harness
//! (`crates/codegen/xai-grok-pager-pty-harness`): spawn the real `maestro-tui`
//! binary in a pseudo-terminal, point the agent at a mock OpenAI-compatible
//! server that serves scripted streaming responses, poll the terminal output
//! until expected content appears, and dump the captured output on failure.
//!
//! A virtual terminal reconstructs cursor positioning, differential repaints,
//! and scrollback before assertions inspect text. Recent screen snapshots also
//! retain transient dialogs that disappear between assertion polls.
//!
//! The tests need no network access, no real API key, and no display; they
//! only require a Unix PTY.

#![cfg(unix)]

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[path = "support/terminal_capture.rs"]
mod terminal_capture;
use terminal_capture::TerminalCapture;

use portable_pty::native_pty_system;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};

/// Generous ceiling for binary startup (first frame + agent init).
const READY_TIMEOUT: Duration = Duration::from_mins(1);
/// Ceiling for a single agent turn against the local mock server.
const TURN_TIMEOUT: Duration = Duration::from_secs(30);

// ─────────────────────────────────────────────────────────────────────────────
// Mock OpenAI-compatible server
// ─────────────────────────────────────────────────────────────────────────────

/// One scripted streaming response: the raw SSE body to serve for a single
/// `POST /v1/chat/completions` request.
struct ScriptedTurn {
    status: &'static str,
    sse_body: String,
}

/// Serve `data:` lines, one JSON chunk each, terminated by `[DONE]`.
fn sse_body(chunks: &[serde_json::Value]) -> String {
    let mut body = String::new();
    for chunk in chunks {
        body.push_str("data: ");
        body.push_str(&chunk.to_string());
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    body
}

fn chunk(delta: serde_json::Value, finish_reason: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl-pty-e2e",
        "object": "chat.completion.chunk",
        "created": 1_700_000_000,
        "model": "gpt-4o",
        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish_reason }],
        "usage": null,
    })
}

/// A streamed assistant text answer.
fn text_turn(text: &str) -> ScriptedTurn {
    ScriptedTurn {
        status: "200 OK",
        sse_body: sse_body(&[
            chunk(
                serde_json::json!({"role": "assistant", "content": text}),
                None,
            ),
            chunk(serde_json::json!({}), Some("stop")),
        ]),
    }
}

/// A streamed assistant tool call (Chat Completions `tool_calls` deltas).
fn tool_call_turn(name: &str, arguments: &serde_json::Value) -> ScriptedTurn {
    ScriptedTurn {
        status: "200 OK",
        sse_body: sse_body(&[
            chunk(
                serde_json::json!({
                    "role": "assistant",
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_pty_e2e_1",
                        "type": "function",
                        "function": { "name": name, "arguments": arguments.to_string() },
                    }],
                }),
                None,
            ),
            chunk(serde_json::json!({}), Some("tool_calls")),
        ]),
    }
}

struct MockState {
    script: VecDeque<ScriptedTurn>,
    /// Bodies of every `chat/completions` request received, in order.
    requests: Vec<String>,
}

/// Minimal HTTP/1.1 stub serving scripted SSE responses from a queue.
///
/// Each request pops the next scripted turn; when the script is exhausted the
/// server answers 500 so a stuck test fails fast with a clear cause instead of
/// hanging on a dead agent.
struct MockOpenAiServer {
    base_url: String,
    identity_base_url: String,
    managed_setup_base_url: String,
    state: Arc<Mutex<MockState>>,
}

impl MockOpenAiServer {
    fn start(script: Vec<ScriptedTurn>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let addr = listener.local_addr().expect("mock server addr");
        let state = Arc::new(Mutex::new(MockState {
            script: script.into(),
            requests: Vec::new(),
        }));
        let thread_state = Arc::clone(&state);
        std::thread::Builder::new()
            .name("pty-e2e-mock-openai".to_owned())
            .spawn(move || {
                for stream in listener.incoming() {
                    match stream {
                        Ok(stream) => Self::serve(stream, &thread_state),
                        Err(_) => break,
                    }
                }
            })
            .expect("spawn mock server thread");
        Self {
            base_url: format!("http://{addr}/v1"),
            identity_base_url: start_mock_identity_server(),
            managed_setup_base_url: start_mock_managed_setup_server(),
            state,
        }
    }

    fn serve(mut stream: TcpStream, state: &Arc<Mutex<MockState>>) {
        let Ok(body) = read_request_body(&mut stream) else {
            return;
        };
        // Doctor's GET /models has no body and must not consume a model turn.
        if body.is_empty() {
            let payload = r#"{"data":[{"id":"gpt-4o"}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                payload.len()
            );
            let _ = stream.write_all(response.as_bytes());
            return;
        }
        let next = {
            let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
            state.requests.push(body);
            state.script.pop_front()
        };
        let (status, content_type, payload) = match next {
            Some(turn) => (turn.status, "text/event-stream", turn.sse_body),
            None => (
                "500 Internal Server Error",
                "text/plain",
                "pty-e2e mock script exhausted".to_owned(),
            ),
        };
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
            payload.len()
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    }

    fn request_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .requests
            .len()
    }
}

/// Serve a valid empty managed-setup document for PTY scenarios. The
/// production default points at the first-party Platform origin, but these
/// tests must remain deterministic and never reach the public network.
fn start_mock_managed_setup_server() -> String {
    start_mock_managed_setup_server_with_gate(None)
}

fn start_mock_managed_setup_server_with_gate(
    mut gate: Option<std::sync::mpsc::Receiver<()>>,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock managed setup server");
    let address = listener
        .local_addr()
        .expect("mock managed setup server address");
    std::thread::Builder::new()
        .name("pty-e2e-mock-managed-setup".to_owned())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    break;
                };
                let request = read_request_body(&mut stream).expect("managed setup request");
                assert_eq!(request.as_bytes(), b"\x0a\x20\x0a\x0bpty-e2e-org\x12\x11pty-e2e-workspace");
                if let Some(gate) = gate.take() {
                    // The test releases policy only after observing editable input.
                    // Dropping the sender on assertion failure also releases the stub.
                    let _ = gate.recv();
                }
                // Public GetClientSetupResponse: scope=1, version=2, mcp=6.
                // The real native client
                // must decode protobuf here, exactly as it does with Platform.
                let body = b"\x0a\x20\x0a\x0bpty-e2e-org\x12\x11pty-e2e-workspace\x10\x01\x32\x02\x08\x02";
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/proto\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(body);
                let _ = stream.flush();
            }
        })
        .expect("spawn mock managed setup server thread");
    format!("http://{address}")
}

/// Serve the minimal signed-Identity projection required by the real Maestro
/// admission boundary. PTY scenarios exercise interaction behavior, but they
/// still must start through the same live verification path as production.
fn start_mock_identity_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock Identity server");
    let address = listener.local_addr().expect("mock Identity server address");
    std::thread::Builder::new()
        .name("pty-e2e-mock-identity".to_owned())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    break;
                };
                let _ = read_request_body(&mut stream);
                let body = r#"{"active":true,"subject":"pty-e2e-user","token_type":"access","organization_id":"pty-e2e-org","workspace_id":"pty-e2e-workspace","scopes":["llm_gateway:invoke"]}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        })
        .expect("spawn mock Identity server thread");
    format!("http://{address}")
}

/// Read one HTTP request and return its body. Only the small, well-formed
/// requests `reqwest` sends to this stub are supported.
fn read_request_body(stream: &mut TcpStream) -> std::io::Result<String> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut buf = Vec::new();
    let mut tmp = [0_u8; 8192];
    let headers_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos;
        }
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed before headers completed",
            ));
        }
        buf.extend_from_slice(&tmp[..n]);
    };
    let headers = String::from_utf8_lossy(&buf[..headers_end]).to_lowercase();
    let content_length = headers
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let body_start = headers_end + 4;
    while buf.len() < body_start + content_length {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let end = (body_start + content_length).min(buf.len());
    Ok(String::from_utf8_lossy(&buf[body_start..end]).into_owned())
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

// ─────────────────────────────────────────────────────────────────────────────
// PTY session driving the real binary
// ─────────────────────────────────────────────────────────────────────────────

struct PtySession {
    child: Box<dyn Child + Send + Sync>,
    // Kept alive so the PTY master (and the reader thread's source) stays open.
    _master: Box<dyn MasterPty + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    output: Arc<Mutex<TerminalCapture>>,
}

impl PtySession {
    /// Spawn `maestro-tui` in a 120x36 PTY, wired to the mock server and an
    /// isolated HOME/MAESTRO_HOME so user config, history, and keychains are
    /// never touched.
    ///
    /// `initial_prompt` is passed as trailing argv: the app submits it itself
    /// once the agent reports ready, which gives a deterministic readiness
    /// signal no typed-first-prompt race can match. Interactive keys (`y`,
    /// Ctrl+C, follow-up prompts) still go through the real PTY input path.
    fn spawn(mock: &MockOpenAiServer, workdir: &std::path::Path, initial_prompt: &str) -> Self {
        Self::spawn_with_args(
            mock,
            workdir,
            &[
                "--model",
                "gpt-4o",
                "--api-key",
                "pty-e2e-key",
                initial_prompt,
            ],
        )
    }

    /// Spawn the real binary with an explicit argv vector.
    ///
    /// Fork is a fast-path subcommand and therefore cannot use the regular
    /// interactive flags prepended by [`Self::spawn`].
    fn spawn_with_args(mock: &MockOpenAiServer, workdir: &std::path::Path, args: &[&str]) -> Self {
        Self::spawn_with_args_and_env(mock, workdir, args, &[])
    }

    fn spawn_with_args_and_env(
        mock: &MockOpenAiServer,
        workdir: &std::path::Path,
        args: &[&str],
        extra_env: &[(&str, &str)],
    ) -> Self {
        Self::spawn_with_size_and_env(mock, workdir, args, extra_env, 120)
    }

    fn spawn_with_size_and_env(
        mock: &MockOpenAiServer,
        workdir: &std::path::Path,
        args: &[&str],
        extra_env: &[(&str, &str)],
        columns: u16,
    ) -> Self {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 36,
                cols: columns,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open PTY");

        let maestro_home = workdir.join("maestro-home");
        std::fs::create_dir_all(&maestro_home).expect("create MAESTRO_HOME");
        let preferences = maestro_home.join("ui.json");
        if !preferences.exists() {
            std::fs::write(&preferences, r#"{"onboardingSeen":true}"#).unwrap();
        }

        let mut command = CommandBuilder::new(
            std::env::var_os("CARGO_BIN_EXE_maestro-tui")
                .expect("Cargo must provide the maestro-tui integration-test binary"),
        );
        command.args(args);
        command.cwd(workdir);
        // CommandBuilder starts from an empty environment; pass through only
        // what the child needs and pin everything else explicitly.
        for key in ["PATH", "LANG", "USER", "LOGNAME", "TMPDIR"] {
            if let Ok(value) = std::env::var(key) {
                command.env(key, value);
            }
        }
        command.env("TERM", "xterm-256color");
        command.env("HOME", workdir);
        command.env("MAESTRO_HOME", &maestro_home);
        command.env("MAESTRO_TELEMETRY", "0");
        command.env("MAESTRO_AUTO_UPDATE", "0");

        command.env("OPENAI_BASE_URL", &mock.base_url);
        command.env("OPENAI_API_KEY", "pty-e2e-key");
        command.env("MAESTRO_IDENTITY_URL", &mock.identity_base_url);
        command.env("MAESTRO_MANAGED_SETUP_URL", &mock.managed_setup_base_url);
        command.env(maestro_tui::init_cli::TEST_IDENTITY_AUTHORITY_ENV, "1");
        command.env(
            maestro_tui::credential_mode::ACCESS_TOKEN_ENV,
            "pty-e2e-identity-token",
        );
        command.env(maestro_tui::credential_mode::ORG_ID_ENV, "pty-e2e-org");
        command.env(
            maestro_tui::credential_mode::WORKSPACE_ID_ENV,
            "pty-e2e-workspace",
        );
        command.env("MAESTRO_DISABLE_KEYCHAIN", "1");
        command.env(
            "MAESTRO_PROMPT_HISTORY_FILE",
            workdir.join("prompt-history.json"),
        );
        for (name, value) in extra_env {
            command.env(name, value);
        }

        let child = pair
            .slave
            .spawn_command(command)
            .expect("spawn maestro-tui");
        drop(pair.slave);

        let output = Arc::new(Mutex::new(TerminalCapture::new(36, columns)));
        let mut reader = pair.master.try_clone_reader().expect("clone PTY reader");
        let writer = Arc::new(Mutex::new(
            pair.master.take_writer().expect("take PTY writer") as Box<dyn Write + Send>,
        ));
        let reader_output = Arc::clone(&output);
        let reader_writer = Arc::clone(&writer);
        std::thread::Builder::new()
            .name("pty-e2e-reader".to_owned())
            .spawn(move || {
                // The TUI probes the "terminal" with a cursor-position
                // report (DSR, ESC[6n) at startup and on each inline-viewport
                // frame; a real terminal answers, so we must too, or init
                // fails with "cursor position could not be read". Init moves
                // the cursor to the last row before the first query, so the
                // truthful answer for this 36-row PTY is the bottom row.
                const DSR_QUERY: &[u8] = b"\x1b[6n";
                const DSR_REPLY: &[u8] = b"\x1b[36;1R";
                // Also answer primary device attributes like a real terminal.
                // Otherwise keyboard detection waits for its two-second timeout.
                const DA_QUERY: &[u8] = b"\x1b[c";
                const DA_REPLY: &[u8] = b"\x1b[?1;2c";
                let mut tail: Vec<u8> = Vec::new();
                let mut buf = [0_u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            reader_output
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .process(&buf[..n]);
                            let mut window = std::mem::take(&mut tail);
                            window.extend_from_slice(&buf[..n]);
                            let query_count = window
                                .windows(DSR_QUERY.len())
                                .filter(|window| *window == DSR_QUERY)
                                .count();
                            let da_count = window
                                .windows(DA_QUERY.len())
                                .filter(|window| *window == DA_QUERY)
                                .count();
                            if query_count > 0 || da_count > 0 {
                                let mut writer =
                                    reader_writer.lock().unwrap_or_else(|e| e.into_inner());
                                for _ in 0..query_count {
                                    let _ = writer.write_all(DSR_REPLY);
                                }
                                for _ in 0..da_count {
                                    let _ = writer.write_all(DA_REPLY);
                                }
                                let _ = writer.flush();
                            }
                            tail = window
                                .get(window.len().saturating_sub(DSR_QUERY.len() - 1)..)
                                .unwrap_or_default()
                                .to_vec();
                        }
                    }
                }
            })
            .expect("spawn PTY reader thread");

        Self {
            child,
            _master: pair.master,
            writer,
            output,
        }
    }

    /// Reconstructed screen, scrollback, and recent observed screens.
    fn screen_text(&self) -> String {
        self.output.lock().unwrap_or_else(|e| e.into_inner()).text()
    }

    /// Poll until `needle` appears in the terminal capture; panic with a dump
    /// of the captured output on timeout (grok-build's screen dump on failure).
    fn wait_for_text(&mut self, needle: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            let screen = self.screen_text();
            if screen.contains(needle) {
                return;
            }
            if Instant::now() >= deadline {
                let tail: String = screen
                    .chars()
                    .rev()
                    .take(12_000)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                let alive = self
                    .child
                    .try_wait()
                    .map(|status| status.is_none())
                    .unwrap_or(false);
                panic!(
                    "timed out after {timeout:?} waiting for {needle:?}\n\
                     child still running: {alive}\n\
                     --- captured output (tail) ---\n{tail}\n\
                     --- end captured output ---"
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Compare prose across terminal line wraps without weakening its wording.
    fn wait_for_wrapped_text(&mut self, needle: &str, timeout: Duration) {
        let expected = needle.split_whitespace().collect::<Vec<_>>().join(" ");
        let deadline = Instant::now() + timeout;
        loop {
            let visible = self
                .screen_text()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if visible.contains(&expected) {
                return;
            }
            if Instant::now() >= deadline {
                self.wait_for_text(needle, Duration::ZERO);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn send_bytes(&mut self, bytes: &[u8]) {
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        writer.write_all(bytes).expect("write to PTY");
        writer.flush().expect("flush PTY");
    }

    /// Send `bytes` (1s apart) until `needle` appears on screen.
    ///
    /// The TUI reads cursor-position replies straight from stdin; a key that
    /// lands in that read window is consumed as probe noise and lost, exactly
    /// like a keystroke raced by a real terminal's reply. Re-pressing is what
    /// a user would do, so the harness does the same instead of flaking.
    fn send_bytes_until(&mut self, bytes: &[u8], needle: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            self.send_bytes(bytes);
            let resend_at = Instant::now() + Duration::from_secs(1);
            while Instant::now() < resend_at {
                if self.screen_text().contains(needle) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            if Instant::now() >= deadline {
                // Reuse the dump-on-failure path.
                self.wait_for_text(needle, Duration::ZERO);
            }
        }
    }

    /// Type a prompt and submit it.
    fn submit_prompt(&mut self, prompt: &str) {
        self.send_bytes(prompt.as_bytes());
        self.send_bytes(b"\r");
    }

    fn ctrl_c(&mut self) {
        self.send_bytes(b"\x03");
    }

    /// Deliver a real Unix signal and wait for the process to terminate.
    fn signal_and_wait(
        &mut self,
        signal: libc::c_int,
        timeout: Duration,
    ) -> portable_pty::ExitStatus {
        let pid = self.child.process_id().expect("PTY child process id");
        // SAFETY: `pid` is the live child owned by this harness and `signal`
        // is supplied by the test as a standard Unix process signal.
        assert_eq!(
            unsafe { libc::kill(pid as libc::pid_t, signal) },
            0,
            "deliver signal {signal} to PTY child {pid}"
        );

        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return status,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Ok(None) => {
                    let screen = self.screen_text();
                    panic!(
                        "PTY child {pid} did not exit within {timeout:?} after signal {signal}\n\
                         --- captured output ---\n{screen}\n--- end captured output ---"
                    );
                }
                Err(error) => panic!("wait for PTY child {pid}: {error}"),
            }
        }
    }

    /// True if a descendant of the TUI process has `needle` in its command
    /// line. Used to prove a tool call is actually executing: the transcript
    /// line keeps its pre-approval `Pending · …` label while the tool runs,
    /// so the process table is the only reliable execution signal.
    fn has_running_tool(&self, needle: &str) -> bool {
        let Some(root) = self.child.process_id() else {
            return false;
        };
        let table = process_table();
        table
            .iter()
            .any(|(pid, _, args)| args.contains(needle) && is_descendant(&table, *pid, root))
    }

    /// Ask the TUI to quit (Ctrl+D), then fall back to killing the child.
    fn shutdown(mut self) {
        self.send_bytes(b"\x04");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                _ => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    return;
                }
            }
        }
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        // Best-effort cleanup so a failed scenario leaves no tool processes
        // (e.g. a `sleep 600` the interrupted bash call spawned) behind.
        if let Some(root) = self.child.process_id() {
            let table = process_table();
            for (pid, _, _) in table
                .iter()
                .filter(|(pid, _, _)| is_descendant(&table, *pid, root))
            {
                let _ = std::process::Command::new("kill")
                    .arg(pid.to_string())
                    .status();
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Snapshot of `ps` as `(pid, ppid, args)` rows.
fn process_table() -> Vec<(u32, u32, String)> {
    let Ok(output) = std::process::Command::new("ps")
        .args(["-eo", "pid=,ppid=,args"])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid = parts.next()?.parse::<u32>().ok()?;
            let ppid = parts.next()?.parse::<u32>().ok()?;
            Some((pid, ppid, parts.collect::<Vec<_>>().join(" ")))
        })
        .collect()
}

fn is_descendant(table: &[(u32, u32, String)], mut pid: u32, root: u32) -> bool {
    while let Some(&(_, ppid, _)) = table.iter().find(|(p, _, _)| *p == pid) {
        if ppid == root {
            return true;
        }
        if ppid <= 1 {
            return false;
        }
        pid = ppid;
    }
    false
}

/// Create one source session in the exact directory `SessionManager::new`
/// derives from the isolated PTY HOME and current working directory.
fn write_fork_fixture(workdir: &std::path::Path, session_id: &str) -> std::path::PathBuf {
    let sanitized_cwd = workdir
        .to_string_lossy()
        .replace(['/', '\\', ':'], "-")
        .trim_matches('-')
        .to_owned();
    let sessions_dir = workdir
        .join(".composer")
        .join("agent")
        .join("sessions")
        .join(format!("--{sanitized_cwd}--"));
    std::fs::create_dir_all(&sessions_dir).expect("create fixture sessions directory");
    let path = sessions_dir.join(format!("2026-07-29T00-00-00-000Z_{session_id}.jsonl"));
    let header = serde_json::json!({
        "type": "session",
        "version": 2,
        "id": session_id,
        "timestamp": "2026-07-29T00:00:00Z",
        "cwd": workdir,
        "model": "gpt-4o",
        "thinkingLevel": "medium"
    });
    let message = serde_json::json!({
        "type": "message",
        "timestamp": "2026-07-29T00:00:01Z",
        "message": {
            "role": "user",
            "content": "PTY_FORK_SOURCE_READY",
            "timestamp": 1
        }
    });
    std::fs::write(&path, format!("{header}\n{message}\n")).expect("write fork fixture");
    path
}

fn wait_continuity_frame(session: &PtySession, needle: &str) {
    let deadline = Instant::now() + TURN_TIMEOUT;
    loop {
        let frame = session
            .output
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .current_text();
        if frame.contains(needle) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "current terminal frame never contained {needle:?}: {frame}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn send_continuity_bytes_until(session: &mut PtySession, bytes: &[u8], needle: &str) {
    let deadline = Instant::now() + TURN_TIMEOUT;
    loop {
        session.send_bytes(bytes);
        let retry = Instant::now() + Duration::from_secs(1);
        while Instant::now() < retry {
            if session
                .output
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .current_text()
                .contains(needle)
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            Instant::now() < deadline,
            "current terminal frame never contained {needle:?}: {}",
            session.screen_text()
        );
    }
}

fn save_continuity_frame(session: &PtySession, name: &str) {
    if let Some(dir) = std::env::var_os("MAESTRO_PTY_EVIDENCE_DIR") {
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).unwrap();
        let frame = session
            .output
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        std::fs::write(dir.join(format!("{name}.txt")), frame.current_text()).unwrap();
        std::fs::write(dir.join(format!("{name}.ansi")), frame.current_formatted()).unwrap();
    }
}

fn submit_continuity_prompt(session: &mut PtySession, prompt: &str, result: &str) {
    let input = format!("\x15{prompt}");
    send_continuity_bytes_until(session, input.as_bytes(), &format!("> {prompt}"));
    send_continuity_bytes_until(session, b"\r", result);
}

fn close_continuity_dialog(session: &mut PtySession, key: &[u8], title: &str) {
    let deadline = Instant::now() + TURN_TIMEOUT;
    loop {
        session.send_bytes(key);
        let retry = Instant::now() + Duration::from_secs(1);
        while Instant::now() < retry {
            if !session
                .output
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .current_text()
                .contains(title)
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(Instant::now() < deadline, "dialog {title:?} did not close");
    }
}

fn approve_continuity_sleep(session: &mut PtySession) -> Vec<u32> {
    session.wait_for_text("Action Approval Required", READY_TIMEOUT);
    let deadline = Instant::now() + TURN_TIMEOUT;
    while !session.has_running_tool("sleep 6") {
        session.send_bytes(b"y");
        std::thread::sleep(Duration::from_millis(300));
        assert!(Instant::now() < deadline, "sleep tool never started");
    }
    let table = process_table();
    let root = session.child.process_id().unwrap();
    let processes: Vec<_> = table
        .iter()
        .filter(|(pid, _, args)| args.contains("sleep 6") && is_descendant(&table, *pid, root))
        .map(|(pid, _, _)| *pid)
        .collect();
    assert!(!processes.is_empty(), "capture the running tool processes");
    session.send_bytes(b"\x15");
    processes
}

fn assert_continuity_processes_stopped(processes: &[u32]) {
    for &pid in processes {
        // SAFETY: signal zero only probes existence; it cannot signal any process.
        let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        let error = std::io::Error::last_os_error();
        assert!(
            result == -1 && error.raw_os_error() == Some(libc::ESRCH),
            "tool process {pid} must be gone before switching, including after reparenting"
        );
    }
}

#[test]
fn pty_session_content_search_and_persisted_fork_navigation_preserve_draft() {
    let _serial = PTY_TEST_SERIAL
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    for columns in [40, 120] {
        let mock = MockOpenAiServer::start(vec![]);
        let workdir = tempfile::tempdir().unwrap();
        write_fork_fixture(workdir.path(), "search-parent");
        let path = write_fork_fixture(workdir.path(), "search-child");
        let text = std::fs::read_to_string(&path).unwrap();
        let mut lines = text.lines();
        let mut header: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        header["parentSession"] = "search-parent".into();
        let tool = serde_json::json!({"type":"message","timestamp":"2026-07-29T00:00:02Z","message":{"role":"toolResult","toolCallId":"tool-search","toolName":"bash","content":"TOOL_CONTENT_ONLY_NEEDLE","isError":false,"timestamp":2}});
        std::fs::write(
            &path,
            format!(
                "{header}\n{}\n{tool}\n",
                lines.collect::<Vec<_>>().join("\n")
            ),
        )
        .unwrap();
        let mut session = PtySession::spawn_with_size_and_env(
            &mock,
            workdir.path(),
            &["--model", "gpt-4o", "--api-key", "pty-e2e-key"],
            &[],
            columns,
        );
        session.wait_for_text("Mode: Act", READY_TIMEOUT);
        session.send_bytes("draft é stays".as_bytes());
        session.wait_for_text("draft é stays", TURN_TIMEOUT);
        send_continuity_bytes_until(&mut session, b"\x1b\x12", "Sessions (2)");
        session.send_bytes(b"TOOL_CONTENT_ONLY_NEEDLE");
        session.wait_for_text("Sessions (1/2)", TURN_TIMEOUT);
        let visible = session
            .output
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .current_text();
        assert!(
            visible.contains("Enter") && visible.contains("Esc") && visible.contains("Ctrl+F"),
            "selection, cancellation, and branch controls must remain visible: {visible}"
        );
        save_continuity_frame(&session, &format!("content-search-{columns}"));
        session.send_bytes(b"\r");
        wait_continuity_frame(&session, "› You");
        session.wait_for_text("PTY_FORK_SOURCE_READY", TURN_TIMEOUT);
        assert_eq!(
            mock.request_count(),
            0,
            "search and resume must not submit a model prompt"
        );
        session.wait_for_text("draft é stays", TURN_TIMEOUT);
        send_continuity_bytes_until(&mut session, b"\x1b\x12", "Sessions (2)");
        send_continuity_bytes_until(&mut session, b"\x06", "Session branches (2)");
        save_continuity_frame(&session, &format!("fork-tree-{columns}"));
        close_continuity_dialog(&mut session, b"\x1b", "Session branches");
        submit_continuity_prompt(&mut session, "/fork", "Resume fork:");
        save_continuity_frame(&session, &format!("fork-resume-{columns}"));
        let forks: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let text = std::fs::read_to_string(entry.path()).ok()?;
                let header: serde_json::Value = serde_json::from_str(text.lines().next()?).ok()?;
                (header["parentSession"] == "search-child")
                    .then(|| header["id"].as_str().unwrap().to_owned())
            })
            .collect();
        assert_eq!(forks.len(), 1);
        let mut resumed =
            PtySession::spawn_with_args(&mock, workdir.path(), &["--resume-session", &forks[0]]);
        resumed.wait_for_text("PTY_FORK_SOURCE_READY", READY_TIMEOUT);
        assert_eq!(mock.request_count(), 0);
        resumed.shutdown();
        session.shutdown();
    }
}

#[test]
fn pty_confirmed_new_session_stops_tool_and_excludes_parent_history() {
    let _serial = PTY_TEST_SERIAL
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    for columns in [40, 120] {
        let mock = MockOpenAiServer::start(vec![
            tool_call_turn("bash", &serde_json::json!({"command":"sleep 600"})),
            text_turn("NEW_SESSION_READY"),
        ]);
        let workdir = tempfile::tempdir().unwrap();
        let mut session = PtySession::spawn_with_size_and_env(
            &mock,
            workdir.path(),
            &[
                "--model",
                "gpt-4o",
                "--api-key",
                "pty-e2e-key",
                "PARENT_LONG_TOOL_PROMPT",
            ],
            &[],
            columns,
        );
        let processes = approve_continuity_sleep(&mut session);
        submit_continuity_prompt(&mut session, "/fork", "Resume fork:");
        assert!(
            session.has_running_tool("sleep 6"),
            "fork must leave the parent running"
        );
        assert_eq!(mock.request_count(), 1, "fork must not submit a prompt");
        save_continuity_frame(&session, &format!("busy-fork-{columns}"));
        submit_continuity_prompt(&mut session, "/new", "Change conversation");
        session.wait_for_text("Esc:", READY_TIMEOUT);
        save_continuity_frame(&session, &format!("new-confirm-{columns}"));
        let visible = session
            .output
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .current_text();
        assert!(
            visible.contains("Enter:") && visible.contains("Esc:"),
            "confirmation controls must fit narrow terminals: {visible}"
        );
        close_continuity_dialog(&mut session, b"\x1b", "Change conversation");
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            session.has_running_tool("sleep 6"),
            "dismissal must leave the tool running"
        );
        submit_continuity_prompt(&mut session, "/new", "Change conversation");
        session.send_bytes_until(b"\r", "New session started.", TURN_TIMEOUT);
        assert!(
            !session.has_running_tool("sleep 6"),
            "switching must await process cleanup"
        );
        assert_continuity_processes_stopped(&processes);
        submit_continuity_prompt(&mut session, "CHILD_NEW_PROMPT", "NEW_SESSION_READY");
        let requests = mock.state.lock().unwrap_or_else(|error| error.into_inner());
        assert_eq!(requests.requests.len(), 2);
        assert!(!requests.requests[1].contains("PARENT_LONG_TOOL_PROMPT"));
        assert!(!requests.requests[1].contains("sleep 600"));
        drop(requests);
        save_continuity_frame(&session, &format!("new-completed-{columns}"));
        session.shutdown();
    }
}

#[test]
fn pty_confirmed_rewind_preserves_earlier_turn_and_persists_child_lineage() {
    let _serial = PTY_TEST_SERIAL
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let mock = MockOpenAiServer::start(vec![
        text_turn("EARLIER_REPLY_KEEP"),
        tool_call_turn("bash", &serde_json::json!({"command":"sleep 600"})),
        text_turn("REWIND_CHILD_READY"),
    ]);
    let workdir = tempfile::tempdir().unwrap();
    let mut session = PtySession::spawn(&mock, workdir.path(), "EARLIER_PROMPT_KEEP");
    session.wait_for_text("EARLIER_REPLY_KEEP", READY_TIMEOUT);
    submit_continuity_prompt(
        &mut session,
        "REWIND_RUNNING_PROMPT_DROP",
        "Action Approval Required",
    );
    let processes = approve_continuity_sleep(&mut session);
    submit_continuity_prompt(&mut session, "/rewind 1", "Change conversation");
    save_continuity_frame(&session, "rewind-confirm-120");
    close_continuity_dialog(&mut session, b"\r", "Change conversation");
    wait_continuity_frame(&session, "EARLIER_REPLY_KEEP");
    assert!(!session.has_running_tool("sleep 6"));
    assert_continuity_processes_stopped(&processes);
    submit_continuity_prompt(&mut session, "REWIND_NEW_PROMPT", "REWIND_CHILD_READY");
    let requests = mock.state.lock().unwrap_or_else(|error| error.into_inner());
    assert_eq!(requests.requests.len(), 3);
    assert!(requests.requests[2].contains("EARLIER_PROMPT_KEEP"));
    assert!(requests.requests[2].contains("EARLIER_REPLY_KEEP"));
    assert!(!requests.requests[2].contains("REWIND_RUNNING_PROMPT_DROP"));
    drop(requests);
    session.shutdown();
    let root = workdir.path().join(".composer/agent/sessions");
    let headers: Vec<serde_json::Value> = std::fs::read_dir(root)
        .unwrap()
        .filter_map(Result::ok)
        .flat_map(|entry| std::fs::read_dir(entry.path()).unwrap())
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .map(|entry| {
            let text = std::fs::read_to_string(entry.path()).unwrap();
            serde_json::from_str(text.lines().next().unwrap()).unwrap()
        })
        .collect();
    assert_eq!(headers.len(), 2);
    let child = headers
        .iter()
        .find(|header| header["parentSession"].is_string())
        .unwrap();
    assert!(
        headers
            .iter()
            .any(|parent| parent["id"] == child["parentSession"])
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Scenarios
// ─────────────────────────────────────────────────────────────────────────────

/// PTY scenarios run one at a time: concurrent TUI spinners (each repainting
/// at ~30fps, every frame a cursor-position probe) starve the harness reader
/// thread, stretching the probe-reply window until keystrokes get eaten by
/// the app's position reads.
static PTY_TEST_SERIAL: Mutex<()> = Mutex::new(());

fn bad_gateway_turn() -> ScriptedTurn {
    ScriptedTurn {
        status: "502 Bad Gateway",
        sse_body: "error code: 502".into(),
    }
}

#[test]
fn pty_502_after_tool_recovers_without_reexecuting_tool() {
    let _serial = PTY_TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockOpenAiServer::start(vec![
        tool_call_turn(
            "bash",
            &serde_json::json!({"command": "printf x >> once.txt"}),
        ),
        bad_gateway_turn(),
        text_turn("PTY_502_RECOVERED"),
    ]);
    let workdir = tempfile::tempdir().expect("temp workdir");
    let mut session = PtySession::spawn(&mock, workdir.path(), "write one x to once.txt");
    session.wait_for_text("Action Approval Required", READY_TIMEOUT);
    session.send_bytes_until(b"y", "PTY_502_RECOVERED", TURN_TIMEOUT);
    assert_eq!(mock.request_count(), 3);
    assert_eq!(
        std::fs::read_to_string(workdir.path().join("once.txt")).unwrap(),
        "x"
    );
    assert!(session.child.try_wait().unwrap().is_none());
    session.shutdown();
}

#[test]
fn pty_exhausted_502_keeps_session_alive_for_next_prompt() {
    let _serial = PTY_TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockOpenAiServer::start(vec![
        bad_gateway_turn(),
        bad_gateway_turn(),
        bad_gateway_turn(),
        text_turn("PTY_502_NEXT_PROMPT_OK"),
    ]);
    let workdir = tempfile::tempdir().expect("temp workdir");
    let mut session = PtySession::spawn(&mock, workdir.path(), "first prompt");
    session.wait_for_text("API error 502 Bad Gateway", READY_TIMEOUT);
    assert_eq!(mock.request_count(), 3);
    assert!(session.child.try_wait().unwrap().is_none());
    session.submit_prompt("try again");
    session.wait_for_text("PTY_502_NEXT_PROMPT_OK", TURN_TIMEOUT);
    assert_eq!(mock.request_count(), 4);
    session.shutdown();
}

/// prompt → streamed answer renders on screen.
#[test]
fn pty_prompt_streams_answer() {
    let _serial = PTY_TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockOpenAiServer::start(vec![text_turn("PTY_E2E_ANSWER_OK")]);
    let workdir = tempfile::tempdir().expect("temp workdir");
    let mut session = PtySession::spawn(&mock, workdir.path(), "say the token");

    session.wait_for_text("PTY_E2E_ANSWER_OK", READY_TIMEOUT);
    assert_eq!(
        mock.request_count(),
        1,
        "a plain answer turn should hit the mock exactly once"
    );

    session.shutdown();
}

/// The first composer must remain usable while the policy response is held.
/// Enter cannot dispatch either a slash command or a provider request here.
#[test]
fn pty_startup_composer_edits_before_managed_setup_finishes() {
    let _serial = PTY_TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (release, gate) = std::sync::mpsc::channel();
    let mut mock = MockOpenAiServer::start(vec![text_turn("STARTUP_DRAFT_ACCEPTED")]);
    mock.managed_setup_base_url = start_mock_managed_setup_server_with_gate(Some(gate));
    let workdir = tempfile::tempdir().expect("temp workdir");
    let started = Instant::now();
    let mut session =
        PtySession::spawn_with_args(&mock, workdir.path(), &["--model", "openai/gpt-4o"]);
    session.wait_for_text("Starting…", Duration::from_secs(5));
    let first_frame = started.elapsed();
    session.send_bytes(b"startup draft survives");
    session.wait_for_text("startup draft survives", Duration::from_secs(2));
    session.send_bytes(b"\r");
    session.wait_for_text("press Enter when ready", Duration::from_secs(2));
    assert_eq!(
        mock.request_count(),
        0,
        "no execution before verified setup"
    );
    eprintln!(
        "startup first editable frame: {first_frame:?}; policy still held; provider requests=0"
    );
    release.send(()).unwrap();
    session.send_bytes_until(b"\r", "STARTUP_DRAFT_ACCEPTED", TURN_TIMEOUT);
    assert_eq!(
        mock.request_count(),
        1,
        "the retained draft must submit once"
    );
    session.shutdown();
}

/// An explicit local route must paint a usable shell without waiting for
/// Identity. The compact local badge is the visible airplane-mode contract.
#[test]
fn pty_local_model_loads_when_identity_is_unreachable() {
    let _serial = PTY_TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut mock = MockOpenAiServer::start(Vec::new());
    mock.identity_base_url = "http://127.0.0.1:9".to_owned();
    let workdir = tempfile::tempdir().expect("temp workdir");
    let started = Instant::now();
    let mut session =
        PtySession::spawn_with_args(&mock, workdir.path(), &["--model", "ollama/qwen3"]);

    session.wait_for_text("qwen3 · Local", Duration::from_secs(5));
    assert!(
        !session.screen_text().contains("Sign in to choose a model"),
        "local startup must not render an Identity gate"
    );
    assert_eq!(
        mock.request_count(),
        0,
        "opening a local shell must not call a model gateway"
    );
    eprintln!("offline local shell ready in {:?}", started.elapsed());
    session.shutdown();
}

/// A cloud-default launch must expose the real model picker before managed
/// setup completes. Selecting a discovered local route abandons the stale
/// cloud preparation and opens the full local shell without releasing it.
#[test]
fn pty_cloud_startup_can_switch_to_discovered_local_model_before_identity() {
    let _serial = PTY_TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    for (provider, base_url_env) in [
        ("ollama", "OLLAMA_BASE_URL"),
        ("lmstudio", "LM_STUDIO_BASE_URL"),
        ("llamacpp", "LLAMA_CPP_BASE_URL"),
    ] {
        let (release, gate) = std::sync::mpsc::channel();
        let mut mock = MockOpenAiServer::start(vec![text_turn("LOCAL_DRAFT_ACCEPTED")]);
        mock.managed_setup_base_url = start_mock_managed_setup_server_with_gate(Some(gate));
        let workdir = tempfile::tempdir().expect("temp workdir");
        let mut session = PtySession::spawn_with_args_and_env(
            &mock,
            workdir.path(),
            &["--model", "openai/gpt-4o"],
            &[(base_url_env, mock.base_url.as_str())],
        );
        session.wait_for_text("local models available", Duration::from_secs(5));
        session.send_bytes("draft é survives".as_bytes());
        session.wait_for_text("draft é survives", Duration::from_secs(2));
        session.send_bytes(b"\x10"); // configured default Ctrl+P
        session.wait_for_text("Select Model", Duration::from_secs(2));
        // A provider search also matches catalog recommendations. Select the
        // exact discovered route so the assertion exercises this mock runtime.
        session.send_bytes(format!("{provider}/gpt-4o").as_bytes());
        session.wait_for_text(&format!("gpt-4o ({provider})"), Duration::from_secs(2));
        session.send_bytes(b"\r");
        session.wait_for_text("gpt-4o · Local", Duration::from_secs(5));
        session.wait_for_text("draft é survives", Duration::from_secs(2));
        assert_eq!(
            mock.request_count(),
            0,
            "choosing a route must not submit the draft"
        );
        session.send_bytes_until(b"\r", "LOCAL_DRAFT_ACCEPTED", TURN_TIMEOUT);
        assert_eq!(mock.request_count(), 1, "local draft submits exactly once");
        release.send(()).unwrap();
        session.shutdown();
    }
}

/// Passive MCP initialization must wait for cloud or local agent admission.
#[test]
fn pty_cloud_without_admission_does_not_initialize_configured_mcp() {
    let _serial = PTY_TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockOpenAiServer::start(Vec::new());
    let workdir = tempfile::tempdir().expect("temp workdir");
    let config = workdir.path().join("mcp.json");
    std::fs::write(
        &config,
        serde_json::json!({
            "mcpServers": {
                "pre-admission-probe": {"url": format!("{}/mcp", mock.base_url), "timeout": 500}
            }
        })
        .to_string(),
    )
    .unwrap();
    let mut session = PtySession::spawn_with_args_and_env(
        &mock,
        workdir.path(),
        &["--model", "openai/gpt-4o"],
        &[
            ("OPENAI_API_KEY", ""),
            (maestro_tui::credential_mode::ACCESS_TOKEN_ENV, ""),
            (maestro_tui::credential_mode::ORG_ID_ENV, ""),
            (maestro_tui::credential_mode::WORKSPACE_ID_ENV, ""),
            ("MAESTRO_USER_MCP_PATH", config.to_str().unwrap()),
            ("OLLAMA_BASE_URL", "http://127.0.0.1:9"),
            ("LM_STUDIO_BASE_URL", "http://127.0.0.1:9/v1"),
            ("LLAMA_CPP_BASE_URL", "http://127.0.0.1:9/v1"),
        ],
    );
    // The denied returning-user shell can suppress the setup error text;
    // wait for its composer rather than a label in the dismissed walkthrough.
    session.wait_for_text("> ", Duration::from_secs(5));
    session.send_bytes("offline draft é stays".as_bytes());
    session.wait_for_text("offline draft é stays", Duration::from_secs(2));
    assert!(!session.screen_text().contains("Guided setup"));
    session.send_bytes(b"\x0b"); // Ctrl+K, keeping the composer intact
    session.wait_for_text("Search commands, files, sessions", Duration::from_secs(2));
    session.send_bytes(b">model");
    session.wait_for_text("/model", Duration::from_secs(2));
    session.send_bytes(b"\r");
    session.wait_for_text("Model and effort", Duration::from_secs(2));
    session.send_bytes(b"\r");
    session.wait_for_text("Select Model", Duration::from_secs(2));
    session.send_bytes(b"\x1b");
    session.wait_for_text("offline draft é stays", Duration::from_secs(2));
    std::thread::sleep(Duration::from_millis(250));
    assert_eq!(
        mock.request_count(),
        0,
        "passive MCP initialization must wait for admission"
    );
    session.shutdown();
}

/// Browsing local history must remain possible while cloud setup is held.
#[test]
fn pty_startup_history_preserves_draft_and_stays_open_after_preparation() {
    let _serial = PTY_TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (release, gate) = std::sync::mpsc::channel();
    let mut mock = MockOpenAiServer::start(Vec::new());
    mock.managed_setup_base_url = start_mock_managed_setup_server_with_gate(Some(gate));
    let workdir = tempfile::tempdir().expect("temp workdir");
    let mut session =
        PtySession::spawn_with_args(&mock, workdir.path(), &["--model", "openai/gpt-4o"]);
    session.wait_for_text("Starting…", Duration::from_secs(5));
    session.send_bytes(b"retained history draft");
    session.wait_for_text("retained history draft", Duration::from_secs(2));
    session.send_bytes(b"\x1b\x12"); // Ctrl+Alt+R, matching the normal shell
    session.wait_for_text("Sessions (", Duration::from_secs(2));
    release.send(()).unwrap();
    session.send_bytes(b"nonexistent history");
    session.wait_for_text("No matching sessions", Duration::from_secs(2));
    assert!(session.screen_text().contains("Sessions ("));
    session.send_bytes(b"\x1b");
    session.wait_for_text("retained history draft", Duration::from_secs(5));
    assert_eq!(mock.request_count(), 0);
    session.shutdown();
}

#[test]
fn pty_startup_resumes_local_history_without_cloud_preparation() {
    let _serial = PTY_TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (release, gate) = std::sync::mpsc::channel();
    let mut mock = MockOpenAiServer::start(vec![text_turn("RESUMED_LOCAL_DRAFT")]);
    mock.managed_setup_base_url = start_mock_managed_setup_server_with_gate(Some(gate));
    let workdir = tempfile::tempdir().expect("temp workdir");
    let path = write_fork_fixture(workdir.path(), "airplane-history");
    let contents = std::fs::read_to_string(&path).unwrap();
    let mut lines = contents.lines();
    let mut header: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    header["model"] = serde_json::json!("ollama/gpt-4o");
    std::fs::write(
        &path,
        format!("{header}\n{}\n", lines.collect::<Vec<_>>().join("\n")),
    )
    .unwrap();
    let mut session = PtySession::spawn_with_args_and_env(
        &mock,
        workdir.path(),
        &["--model", "openai/gpt-4o"],
        &[("OLLAMA_BASE_URL", mock.base_url.as_str())],
    );
    session.wait_for_text("Starting…", Duration::from_secs(5));
    session.send_bytes(b"resume draft");
    session.wait_for_text("resume draft", Duration::from_secs(2));
    session.send_bytes(b"\x1b\x12");
    session.wait_for_text("PTY_FORK_SOURCE_READY", Duration::from_secs(5));
    session.send_bytes(b"\r");
    session.wait_for_text("gpt-4o · Local", Duration::from_secs(5));
    session.wait_for_text("resume draft", Duration::from_secs(2));
    assert_eq!(mock.request_count(), 0);
    session.send_bytes_until(b"\r", "RESUMED_LOCAL_DRAFT", TURN_TIMEOUT);
    assert_eq!(mock.request_count(), 1);
    release.send(()).unwrap();
    session.shutdown();
}

/// The grouped `/model` menu must open its child selector and let Escape
/// return to chat without issuing a provider request. A follow-up turn proves
/// the modal stack was actually dismissed rather than only painted away.
#[test]
fn pty_grouped_model_menu_opens_selector_and_escape_returns_to_chat() {
    let _serial = PTY_TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockOpenAiServer::start(vec![
        text_turn("PTY_GROUPED_MODEL_MENU_READY"),
        text_turn("PTY_GROUPED_MODEL_MENU_FOLLOWUP_OK"),
    ]);
    let workdir = tempfile::tempdir().expect("temp workdir");
    let mut session = PtySession::spawn(&mock, workdir.path(), "start grouped model menu");

    session.wait_for_text("PTY_GROUPED_MODEL_MENU_READY", READY_TIMEOUT);
    session.submit_prompt("/model");
    session.wait_for_text("Model and effort", TURN_TIMEOUT);
    session.wait_for_text("Choose model", TURN_TIMEOUT);
    let grouped_menu = session.screen_text();
    assert!(
        grouped_menu.contains("Model and effort")
            && grouped_menu.contains("Choose model")
            && grouped_menu.contains("Effort"),
        "grouped model menu labels were not rendered exactly:\n{grouped_menu}"
    );
    assert_eq!(
        mock.request_count(),
        1,
        "opening the grouped menu must not issue a provider request"
    );

    // Enter selects the first grouped row, which must keep the child selector
    // open rather than treating the parent palette as the final destination.
    session.send_bytes_until(b"\r", "Select Model", TURN_TIMEOUT);
    session.wait_for_text("Enter select · Esc cancel", TURN_TIMEOUT);
    assert_eq!(
        mock.request_count(),
        1,
        "opening the model selector must not issue a provider request"
    );

    // The first Escape can race a cursor-position probe. Send it twice so the
    // cancellation remains deterministic without depending on stale screen
    // history as a post-cancel marker.
    session.send_bytes(b"\x1b");
    session.send_bytes(b"\x1b");
    session.submit_prompt("PTY_GROUPED_MODEL_MENU_FOLLOWUP");
    session.wait_for_text("PTY_GROUPED_MODEL_MENU_FOLLOWUP_OK", TURN_TIMEOUT);
    assert_eq!(
        mock.request_count(),
        2,
        "canceling the grouped selector must allow exactly one follow-up provider request"
    );

    session.shutdown();
}

/// `/mcp` opens the native manager and returning to chat remains responsive.
/// The disabled fixture proves the manager lists configured servers without
/// dialing an external process during the scenario.
#[test]
fn pty_mcp_manager_opens_and_returns_to_chat() {
    let _serial = PTY_TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockOpenAiServer::start(vec![
        text_turn("PTY_MCP_READY"),
        text_turn("PTY_MCP_CHAT_STILL_RESPONSIVE"),
    ]);
    let workdir = tempfile::tempdir().expect("temp workdir");
    let config_dir = workdir.path().join(".composer");
    std::fs::create_dir_all(&config_dir).expect("create MCP config directory");
    std::fs::write(
        config_dir.join("mcp.json"),
        r#"{"mcpServers":{"demo":{"command":"demo-mcp","disabled":true}}}"#,
    )
    .expect("write MCP fixture");
    let mut session = PtySession::spawn(&mock, workdir.path(), "start MCP scenario");

    session.wait_for_text("PTY_MCP_READY", READY_TIMEOUT);
    session.submit_prompt("/mcp");
    session.wait_for_text("MCP servers", TURN_TIMEOUT);
    session.wait_for_text("demo", TURN_TIMEOUT);

    // Use the manager's explicit custom-add exit as a visible synchronization
    // point. A lone Escape can be consumed by the terminal DSR probe.
    session.send_bytes_until(b"a", "/mcp config add ", TURN_TIMEOUT);
    session.send_bytes(b"\x15");
    session.submit_prompt("confirm chat still works");
    session.wait_for_text("PTY_MCP_CHAT_STILL_RESPONSIVE", TURN_TIMEOUT);
    assert_eq!(
        mock.request_count(),
        2,
        "slash command must not call the model"
    );

    session.shutdown();
}

/// Regression for forked interactive sessions bypassing the registered
/// shutdown lifecycle: a real fork is resumed in the PTY, accepts a new
/// turn, and must handle SIGTERM through orderly teardown rather than the
/// operating system's default immediate termination.
#[test]
fn pty_fork_sigterm_exits_143_and_flushes_fork_session() {
    let _serial = PTY_TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockOpenAiServer::start(vec![text_turn("PTY_FORK_RESPONSE_OK")]);
    let workdir = tempfile::tempdir().expect("temp workdir");
    let source_id = "pty-fork-source";
    let source_path = write_fork_fixture(workdir.path(), source_id);
    let sessions_dir = source_path.parent().expect("fixture sessions directory");
    let mut session = PtySession::spawn_with_args(&mock, workdir.path(), &["fork", source_id]);

    // Seeing restored history proves App construction and the fork-specific
    // startup resume both completed before the signal is delivered.
    session.wait_for_text("PTY_FORK_SOURCE_READY", READY_TIMEOUT);
    session.submit_prompt("PTY_FORK_SIGTERM_FLUSH");
    let request_deadline = Instant::now() + TURN_TIMEOUT;
    while mock.request_count() < 1 {
        assert!(
            Instant::now() < request_deadline,
            "forked session never submitted the post-resume prompt"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let status = session.signal_and_wait(libc::SIGTERM, TURN_TIMEOUT);
    assert_eq!(
        status.exit_code(),
        143,
        "registered SIGTERM path must return the conventional 128 + SIGTERM exit code"
    );
    session.wait_for_text("[shutdown] received SIGTERM", Duration::from_secs(2));

    let fork_paths: Vec<_> = std::fs::read_dir(sessions_dir)
        .expect("list sessions after fork shutdown")
        .map(|entry| entry.expect("session directory entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .filter(|path| path != &source_path)
        .collect();
    assert_eq!(
        fork_paths.len(),
        1,
        "fork command should create exactly one independent session"
    );
    let fork_contents =
        std::fs::read_to_string(&fork_paths[0]).expect("read fork after orderly shutdown");
    let fork_header: serde_json::Value =
        serde_json::from_str(fork_contents.lines().next().expect("fork session header"))
            .expect("parse fork session header");
    assert_eq!(
        fork_header["parentSession"], source_id,
        "fork must retain its durable source-session lineage"
    );
    assert!(
        fork_contents.contains("PTY_FORK_SIGTERM_FLUSH"),
        "post-resume turn was not durable after SIGTERM:\n{fork_contents}"
    );
    assert!(
        !std::fs::read_to_string(&source_path)
            .expect("read source after fork shutdown")
            .contains("PTY_FORK_SIGTERM_FLUSH"),
        "fork shutdown must never append to the source session"
    );
}

/// Exercise persisted rewind through terminal input, then prove the next
/// provider request and saved branch exclude the abandoned turn.
#[test]
fn pty_rewind_preserves_source_and_continues_from_saved_prefix() {
    let _serial = PTY_TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockOpenAiServer::start(vec![text_turn("PTY_REWIND_CONTINUED")]);
    let workdir = tempfile::tempdir().expect("temp workdir");
    let source_id = "pty-rewind-source";
    let source_path = write_fork_fixture(workdir.path(), source_id);
    let abandoned = serde_json::json!({
        "type": "message", "timestamp": "2026-07-29T00:00:02Z",
        "message": {"role": "user", "content": "PTY_ABANDONED_TURN", "timestamp": 2}
    });
    writeln!(
        std::fs::OpenOptions::new()
            .append(true)
            .open(&source_path)
            .unwrap(),
        "{abandoned}"
    )
    .unwrap();
    let mut session =
        PtySession::spawn_with_args(&mock, workdir.path(), &["--resume-session", source_id]);
    session.wait_for_text("PTY_ABANDONED_TURN", READY_TIMEOUT);
    session.submit_prompt("/rewind 1");
    // Status text can be replaced by the next ready event before a frame is
    // painted. Wait for durable branch publication; the provider assertions
    // below separately prove that the branch was adopted by the live actor.
    let deadline = Instant::now() + TURN_TIMEOUT;
    loop {
        let published = std::fs::read_dir(source_path.parent().unwrap())
            .unwrap()
            .any(|entry| {
                entry.is_ok_and(|entry| {
                    let path = entry.path();
                    path != source_path && path.extension().is_some_and(|ext| ext == "jsonl")
                })
            });
        if published {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "rewind did not publish a saved branch"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(mock.request_count(), 0);
    // Rewind repaints and probes the terminal. Use the harness's input
    // acknowledgement before Enter so a cursor-position probe cannot consume
    // the follow-up text. Repeating Enter on the cleared composer is a no-op.
    session.send_bytes_until(
        b"\x15PTY_NEW_BRANCH_REQUEST",
        "PTY_NEW_BRANCH_REQUEST",
        TURN_TIMEOUT,
    );
    session.send_bytes_until(b"\r", "PTY_REWIND_CONTINUED", TURN_TIMEOUT);
    let requests = mock.state.lock().unwrap();
    assert_eq!(requests.requests.len(), 1);
    assert!(requests.requests[0].contains("PTY_FORK_SOURCE_READY"));
    assert!(!requests.requests[0].contains("PTY_ABANDONED_TURN"));
    drop(requests);
    session.shutdown();
    let source = std::fs::read_to_string(&source_path).unwrap();
    assert!(source.contains("PTY_ABANDONED_TURN"));
    assert!(!source.contains("PTY_NEW_BRANCH_REQUEST"));
    let branches: Vec<_> = std::fs::read_dir(source_path.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl") && path != &source_path)
        .collect();
    assert_eq!(branches.len(), 1);
    let branch = std::fs::read_to_string(&branches[0]).unwrap();
    assert!(branch.contains("PTY_FORK_SOURCE_READY"));
    assert!(branch.contains("PTY_NEW_BRANCH_REQUEST"));
    assert!(!branch.contains("PTY_ABANDONED_TURN"));
}

/// tool call → approval modal appears (selective mode) → approve → result
/// renders after the follow-up turn.
#[test]
fn pty_tool_call_approval_flow() {
    let _serial = PTY_TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockOpenAiServer::start(vec![
        tool_call_turn(
            "bash",
            &serde_json::json!({"command": "printf pty-e2e-ran"}),
        ),
        text_turn("PTY_E2E_TOOL_DONE_OK"),
    ]);
    let workdir = tempfile::tempdir().expect("temp workdir");
    let mut session = PtySession::spawn(&mock, workdir.path(), "run the printf command");

    // Default approval mode is Selective: `printf` is not on the read-only
    // safe list, so the modal must appear before anything executes.
    session.wait_for_text("Action Approval Required", READY_TIMEOUT);
    session.wait_for_text("printf pty-e2e-ran", TURN_TIMEOUT);

    session.send_bytes_until(b"y", "PTY_E2E_TOOL_DONE_OK", TURN_TIMEOUT);
    assert_eq!(
        mock.request_count(),
        2,
        "tool call turn + follow-up turn after tool result"
    );

    session.shutdown();
}

/// Regression pin for #3071: Ctrl+C cancels a long-running tool call and the
/// UI stays responsive enough to run another turn immediately.
#[test]
fn pty_ctrl_c_interrupts_long_tool_and_stays_responsive() {
    let _serial = PTY_TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockOpenAiServer::start(vec![
        tool_call_turn("bash", &serde_json::json!({"command": "sleep 600"})),
        text_turn("PTY_E2E_RECOVERED_OK"),
    ]);
    let workdir = tempfile::tempdir().expect("temp workdir");
    let mut session = PtySession::spawn(&mock, workdir.path(), "run the sleep command");

    session.wait_for_text("Action Approval Required", READY_TIMEOUT);

    // Approve, retrying until the tool process is actually running (a key can
    // race the terminal probe reads and get eaten; the retried keys land in
    // the input box and are cleared below before typing).
    let approve_deadline = Instant::now() + TURN_TIMEOUT;
    loop {
        session.send_bytes(b"y");
        let probe_until = Instant::now() + Duration::from_secs(1);
        while Instant::now() < probe_until {
            if session.has_running_tool("sleep 6") {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if session.has_running_tool("sleep 6") {
            break;
        }
        assert!(
            Instant::now() < approve_deadline,
            "approval never started the sleep tool"
        );
    }

    // Without the #3071 fix the interrupt only took effect after the tool
    // timed out. Ctrl+C can race terminal probe reads just like any key, so
    // retry it only while the sleep process proves the app is still busy.
    // Retrying Ctrl+C after the process exits is incorrect: once the app is
    // idle, Ctrl+C intentionally quits the TUI and closes the PTY.
    let deadline = Instant::now() + TURN_TIMEOUT;
    while session.has_running_tool("sleep 6") {
        session.ctrl_c();
        let probe_until = Instant::now() + Duration::from_secs(1);
        while Instant::now() < probe_until && session.has_running_tool("sleep 6") {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            Instant::now() < deadline,
            "Ctrl+C did not stop the sleep tool within {TURN_TIMEOUT:?}"
        );
    }

    // The follow-up turn must complete within the same bound, far below the
    // 600s sleep. Retry only the prompt if a terminal probe consumes input.
    loop {
        // Clear any stray input-box keys before typing the follow-up.
        session.send_bytes(b"\x15");
        session.submit_prompt("are you still there");
        let probe_until = Instant::now() + Duration::from_secs(6);
        while Instant::now() < probe_until {
            if session.screen_text().contains("PTY_E2E_RECOVERED_OK") {
                session.shutdown();
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if Instant::now() >= deadline {
            // Reuse the dump-on-failure path.
            session.wait_for_text("PTY_E2E_RECOVERED_OK", Duration::ZERO);
        }
    }
}

/// The shortcut must change the next provider request, not only footer text.
#[test]
fn pty_shift_tab_changes_request_effort() {
    let _serial = PTY_TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockOpenAiServer::start(vec![
        text_turn("THINKING_READY"),
        text_turn("THINKING_MEDIUM_DONE"),
        text_turn("THINKING_HIGH_DONE"),
    ]);
    let workdir = tempfile::tempdir().expect("temp workdir");
    let mut session = PtySession::spawn_with_args(
        &mock,
        workdir.path(),
        &["--model", "o1", "--api-key", "pty-e2e-key", "say ready"],
    );
    session.wait_for_text("THINKING_READY", READY_TIMEOUT);
    session.submit_prompt("/thinking low");
    session.wait_for_text("(low)", TURN_TIMEOUT);
    session.send_bytes(b"\x1b[Z");
    session.submit_prompt("say medium done");
    session.wait_for_text("THINKING_MEDIUM_DONE", TURN_TIMEOUT);
    session.send_bytes(b"\x1b[Z");
    session.submit_prompt("say high done");
    session.wait_for_text("THINKING_HIGH_DONE", TURN_TIMEOUT);
    let requests = mock.state.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(requests.requests.len(), 3);
    for (body, effort) in requests.requests[1..].iter().zip(["medium", "high"]) {
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["reasoning_effort"], effort);
    }
    drop(requests);
    session.shutdown();
}

#[test]
fn specialist_exec_applies_focus_model_and_tool_ceiling_to_the_request() {
    let mock = MockOpenAiServer::start(vec![text_turn("SPECIALIST_DONE")]);
    let workdir = tempfile::tempdir().unwrap();
    let profiles = workdir.path().join("maestro-home/agent-profiles");
    std::fs::create_dir_all(&profiles).unwrap();
    std::fs::write(
        profiles.join("billing.md"),
        "---\nname: billing\nmodel: gpt-4o\ntools: [read]\n---\nBILLING_FOCUS_CONTRACT",
    )
    .unwrap();
    let mut session = PtySession::spawn_with_args(
        &mock,
        workdir.path(),
        &[
            "exec",
            "--specialist",
            "billing",
            "Inspect the invoice journey",
        ],
    );
    session.wait_for_text("SPECIALIST_DONE", TURN_TIMEOUT);
    let state = mock.state.lock().unwrap_or_else(|error| error.into_inner());
    let request: serde_json::Value = serde_json::from_str(&state.requests[0]).unwrap();
    assert_eq!(request["model"], "gpt-4o");
    let messages = request["messages"].as_array().unwrap();
    assert!(messages.iter().any(|m| {
        m["role"] == "system"
            && m["content"]
                .as_str()
                .is_some_and(|text| text.contains("BILLING_FOCUS_CONTRACT"))
    }));
    assert!(messages.iter().any(|m| {
        m["role"] == "user"
            && m["content"]
                .to_string()
                .contains("Inspect the invoice journey")
    }));
    let tools = request["tools"].as_array().unwrap();
    assert!(!tools.is_empty());
    assert!(tools.iter().all(|tool| tool["function"]["name"] == "read"));
}

/// Resume must rebuild the executor, not just change the displayed transcript.
#[test]
fn pty_resume_in_saved_workspace_executes_relative_tool_in_that_workspace() {
    let _serial = PTY_TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockOpenAiServer::start(vec![
        tool_call_turn(
            "bash",
            &serde_json::json!({"command": "printf resumed > resume-marker.txt"}),
        ),
        text_turn("PTY_RESUME_WORKSPACE_OK"),
    ]);
    let workdir = tempfile::tempdir().expect("temp workdir");
    let saved = workdir.path().join("retained worktree");
    std::fs::create_dir(&saved).unwrap();
    let id = "pty-workspace-resume";
    let path = write_fork_fixture(workdir.path(), id);
    let source = std::fs::read_to_string(&path).unwrap();
    let mut lines = source.lines();
    let mut header: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    header["cwd"] = serde_json::json!(saved);
    std::fs::write(
        &path,
        format!("{header}\n{}\n", lines.collect::<Vec<_>>().join("\n")),
    )
    .unwrap();
    let mut session = PtySession::spawn_with_args(&mock, workdir.path(), &["--resume-session", id]);
    session.wait_for_text("PTY_FORK_SOURCE_READY", READY_TIMEOUT);
    session.submit_prompt("write the resume marker");
    session.wait_for_text("Action Approval Required", TURN_TIMEOUT);
    session.send_bytes_until(b"y", "PTY_RESUME_WORKSPACE_OK", TURN_TIMEOUT);
    assert_eq!(
        std::fs::read_to_string(saved.join("resume-marker.txt")).unwrap(),
        "resumed"
    );
    assert!(!workdir.path().join("resume-marker.txt").exists());
    session.shutdown();
}

/// The report flow stays in the terminal and never sends a model prompt.
#[test]
fn pty_bug_report_draft_review_and_dismiss() {
    let _serial = PTY_TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockOpenAiServer::start(vec![text_turn("PTY_BUG_READY")]);
    let workdir = tempfile::tempdir().expect("temp workdir");
    let mut session = PtySession::spawn(&mock, workdir.path(), "start bug report scenario");
    session.wait_for_text("PTY_BUG_READY", READY_TIMEOUT);
    session.submit_prompt("/bug draft The terminal stopped responding");
    session.wait_for_text("Bug report drafted", TURN_TIMEOUT);
    session.submit_prompt("/bug review");
    session.wait_for_text("What happened:", TURN_TIMEOUT);
    session.wait_for_text("Diagnostics: None", TURN_TIMEOUT);
    session.send_bytes(b"0");
    wait_for_feedback_status(workdir.path(), "Dismissed");
    let mut paths = vec![workdir.path().join(".composer/agent/sessions")];
    let mut dismissed = false;
    while let Some(path) = paths.pop() {
        if path.is_dir() {
            paths.extend(
                std::fs::read_dir(path)
                    .unwrap()
                    .map(|entry| entry.unwrap().path()),
            );
        } else if path.extension().is_some_and(|ext| ext == "jsonl") {
            for line in std::fs::read_to_string(path).unwrap().lines() {
                let value: serde_json::Value = serde_json::from_str(line).unwrap();
                dismissed |= value["customType"] == "product_issue_draft_v1"
                    && value["data"]["status"] == "Dismissed";
            }
        }
    }
    assert!(
        dismissed,
        "dismiss must be persisted in the real session log"
    );
    assert_eq!(
        mock.request_count(),
        1,
        "report commands must never become model prompts"
    );
    session.shutdown();
}

#[test]
fn pty_model_feedback_card_review_edit_and_discard() {
    let _serial = PTY_TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockOpenAiServer::start(vec![
        tool_call_turn(
            "draft_feedback",
            &serde_json::json!({"description":"The tool repeated a corrected mistake", "expected_behavior":"Use the corrected instruction", "reproduction_steps":"Correct the tool and retry"}),
        ),
        text_turn("PTY_FEEDBACK_DRAFTED"),
    ]);
    let workdir = tempfile::tempdir().unwrap();
    let mut session = PtySession::spawn(&mock, workdir.path(), "Draft feedback for this failure");
    session.wait_for_text("PTY_FEEDBACK_DRAFTED", READY_TIMEOUT);
    session.wait_for_text("Bug report drafted", TURN_TIMEOUT);
    session.send_bytes(b"1");
    session.wait_for_text("Reproduction steps:", TURN_TIMEOUT);
    session.send_bytes(b"r");
    session.wait_for_text("Edit repro", TURN_TIMEOUT);
    session.send_bytes(b" and inspect the output\r");
    session.wait_for_text("and inspect the output", TURN_TIMEOUT);
    session.send_bytes(b"0");
    wait_for_feedback_status(workdir.path(), "Dismissed");
    assert_eq!(
        mock.request_count(),
        2,
        "feedback controls must not trigger model requests"
    );
    session.shutdown();
}

// Ratatui diffs may reuse characters already on the screen. The durable report
// status is the authoritative dismissal result, independent of paint encoding.
fn wait_for_feedback_status(root: &std::path::Path, expected: &str) {
    let deadline = Instant::now() + TURN_TIMEOUT;
    loop {
        let mut paths = vec![root.join(".composer/agent/sessions")];
        while let Some(path) = paths.pop() {
            if path.is_dir() {
                paths.extend(
                    std::fs::read_dir(path)
                        .unwrap()
                        .map(|entry| entry.unwrap().path()),
                );
            } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                let text = std::fs::read_to_string(path).unwrap();
                if text
                    .lines()
                    .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                    .any(|entry| {
                        entry["customType"] == "product_issue_draft_v1"
                            && entry["data"]["status"] == expected
                    })
                {
                    return;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "feedback status {expected} was not persisted"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn onboarding_first_boot_persists_artwork_without_completing_setup() {
    let _guard = PTY_TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("maestro-home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("ui.json"),
        r#"{"onboardingSeen":false,"animations":false}"#,
    )
    .unwrap();
    let mock = MockOpenAiServer::start(vec![]);
    for _ in 0..2 {
        let mut session = PtySession::spawn_with_args_and_env(
            &mock,
            temp.path(),
            &[],
            &[
                (maestro_tui::credential_mode::ACCESS_TOKEN_ENV, ""),
                (maestro_tui::credential_mode::ORG_ID_ENV, ""),
            ],
        );
        session.wait_for_text("Connect your account. Choose your model.", READY_TIMEOUT);
        let prefs: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(home.join("ui.json")).unwrap()).unwrap();
        assert_eq!(prefs["bootSeen"], true);
        assert_eq!(prefs["onboardingSeen"], false);
        assert_eq!(mock.request_count(), 0);
        // A process interruption must not turn viewing artwork into completed setup.
        session.child.kill().unwrap();
        session.child.wait().unwrap();
    }
}

#[test]
fn onboarding_first_run_checks_fixed_prompt_and_persists_display_choice() {
    let _guard = PTY_TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("maestro-home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("ui.json"),
        r#"{"onboardingSeen":false,"animations":false}"#,
    )
    .unwrap();
    let mock = MockOpenAiServer::start(vec![text_turn("ready")]);
    let mut session = PtySession::spawn(&mock, temp.path(), "");
    session.wait_for_text("Connect your account. Choose your model.", READY_TIMEOUT);
    session.send_bytes(b"\x04");
    session.wait_for_text("Share setup information: off", TURN_TIMEOUT);
    session.send_bytes(b"\r");
    session.wait_for_text("What is your role?", TURN_TIMEOUT);
    session.send_bytes(b"\r");
    session.wait_for_text("What do you want to do first?", TURN_TIMEOUT);
    session.send_bytes(b"\r");
    session.wait_for_text("How do you plan to run", TURN_TIMEOUT);
    session.send_bytes(b"\r");
    session.wait_for_wrapped_text("incur usage charges.", TURN_TIMEOUT);
    assert_eq!(
        mock.request_count(),
        0,
        "no model request before explicit test confirmation"
    );
    session.send_bytes(b"\r");
    session.wait_for_text(
        "model access and the native read test passed",
        READY_TIMEOUT,
    );
    assert_eq!(mock.request_count(), 1);
    let requests = mock.state.lock().unwrap().requests.clone();
    let request: serde_json::Value = serde_json::from_str(&requests[0]).unwrap();
    assert_eq!(request["messages"].as_array().unwrap().len(), 1);
    assert_eq!(
        request["messages"][0]["content"],
        "Reply with the single word ready."
    );
    session.send_bytes(b"\r");
    let deadline = Instant::now() + TURN_TIMEOUT;
    loop {
        let prefs: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(home.join("ui.json")).unwrap()).unwrap();
        if prefs["onboardingSeen"] == true {
            assert_eq!(prefs["onboardingShareDiagnostics"], false);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "onboarding preference was not saved"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    session.shutdown();
}

#[test]
fn onboarding_failed_model_requires_retry_and_never_claims_verified() {
    let _guard = PTY_TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("maestro-home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("ui.json"),
        r#"{"onboardingSeen":false,"animations":false}"#,
    )
    .unwrap();
    let mock = MockOpenAiServer::start(vec![text_turn("")]);
    let mut session = PtySession::spawn(&mock, temp.path(), "");
    session.wait_for_text("Connect your account. Choose your model.", READY_TIMEOUT);
    for expected in [
        "What is your role?",
        "What do you want to do first?",
        "How do you plan to run",
        "incur usage charges.",
    ] {
        session.send_bytes(b"\r");
        session.wait_for_wrapped_text(expected, TURN_TIMEOUT);
    }
    session.send_bytes(b"\r");
    session.wait_for_text("Setup needs attention before your first run", READY_TIMEOUT);
    assert_eq!(mock.request_count(), 1);
    let prefs: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("ui.json")).unwrap()).unwrap();
    assert_eq!(prefs["onboardingSeen"], false);
    session.send_bytes(b"\x1b");
    session.wait_for_text("Managed inference", TURN_TIMEOUT);
    session.shutdown();
}

#[test]
fn pty_experiments_preferences_save_real_consent_and_show_write_failure() {
    // Check current screens, not historical snapshots: Off must be visible
    // again after withdrawal or a failed save, not merely earlier in the test.
    fn screen(session: &PtySession) -> String {
        session
            .output
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .current_text()
    }
    fn wait_screen(session: &PtySession, matches: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + TURN_TIMEOUT;
        loop {
            let current = screen(session);
            if matches(&current) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "expected current screen was not shown:\n{current}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    fn press_until(session: &mut PtySession, keys: &[u8], expected: &str) {
        let deadline = Instant::now() + TURN_TIMEOUT;
        loop {
            session.send_bytes(keys);
            let retry_at = Instant::now() + Duration::from_secs(1);
            while Instant::now() < retry_at {
                if screen(session).contains(expected) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            assert!(
                Instant::now() < deadline,
                "expected {expected:?}:\n{}",
                screen(session)
            );
        }
    }
    fn open_from_settings(session: &mut PtySession, expected: &str) {
        session.submit_prompt("/settings");
        wait_screen(session, |current| {
            current.contains("Experiments") && current.contains("Account and inference")
        });
        press_until(session, b"\r", "Preferences");
        wait_screen(session, |current| {
            current.contains(expected) && current.contains("next safe turn")
        });
    }
    fn saved(config: &std::path::Path) -> bool {
        let consent: toml::Value =
            toml::from_str(&std::fs::read_to_string(config).unwrap()).unwrap();
        consent["experiments"]["enabled"].as_bool().unwrap()
    }
    fn confirm(session: &mut PtySession) {
        session.send_bytes(b"\r");
        wait_screen(session, |current| !current.contains("Preferences"));
    }

    let _serial = PTY_TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockOpenAiServer::start(vec![text_turn("EXPERIMENTS_READY")]);
    let workdir = tempfile::tempdir().unwrap();
    let mut session =
        PtySession::spawn(&mock, workdir.path(), "start experiment controls scenario");
    session.wait_for_text("EXPERIMENTS_READY", READY_TIMEOUT);
    let config = workdir.path().join("maestro-home/config.toml");

    open_from_settings(&mut session, "Experiments  Off");
    eprintln!(
        "EXPERIMENTS_CAPTURE_OFF_BEGIN\n{}\nEXPERIMENTS_CAPTURE_OFF_END",
        screen(&session)
    );
    assert!(!config.exists());
    press_until(&mut session, b"\x1b[C", "Experiments  On");
    assert!(!config.exists(), "draft selection must not persist consent");
    session.send_bytes(b"\x1b");
    wait_screen(&session, |current| !current.contains("Preferences"));
    assert!(!config.exists(), "cancel must not enroll");

    open_from_settings(&mut session, "Experiments  Off");
    press_until(&mut session, b"\x1b[C", "Experiments  On");
    confirm(&mut session);
    assert!(saved(&config), "Enter must durably enable participation");

    open_from_settings(&mut session, "Experiments  On");
    eprintln!(
        "EXPERIMENTS_CAPTURE_ON_BEGIN\n{}\nEXPERIMENTS_CAPTURE_ON_END",
        screen(&session)
    );
    press_until(&mut session, b"\x1b[D", "Experiments  Off");
    assert!(
        saved(&config),
        "withdrawal draft must not save before Enter"
    );
    confirm(&mut session);
    assert!(!saved(&config), "Enter must durably withdraw participation");

    open_from_settings(&mut session, "Experiments  Off");
    // Make the owner's lock path unwritable without relying on OS permissions.
    let lock = workdir.path().join("maestro-home/config.lock");
    std::fs::remove_file(&lock).unwrap();
    std::fs::create_dir(&lock).unwrap();
    press_until(&mut session, b"\x1b[C", "Experiments  On");
    session.send_bytes(b"\r");
    wait_screen(&session, |current| {
        current.contains("Could not save Experiments") && current.contains("Experiments  Off")
    });
    assert!(
        !saved(&config),
        "failed save must preserve withdrawn consent"
    );
    assert_eq!(
        mock.request_count(),
        1,
        "settings must not trigger inference"
    );
    session.shutdown();
}
