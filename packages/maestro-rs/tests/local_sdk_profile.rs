//! Canonical-process conformance for the local SDK stdio profile.
//!
//! The fixture binary is feature-gated and injects `ScriptedClient`, while
//! the process below still enters the production headless stdin parser and
//! stdout event bridge. No provider credential or external network is used.

#[cfg(feature = "test-support")]
use std::ffi::OsString;
#[cfg(feature = "test-support")]
use std::fs;
#[cfg(feature = "test-support")]
use std::io::{BufRead, BufReader, Read, Write};
#[cfg(feature = "test-support")]
use std::process::{Child, ChildStdin, Command, Stdio};
#[cfg(feature = "test-support")]
use std::sync::{Arc, Mutex, mpsc};
#[cfg(feature = "test-support")]
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

#[cfg(feature = "test-support")]
const EVENT_TIMEOUT: Duration = Duration::from_secs(20);

#[cfg(feature = "test-support")]
fn conformance_binary() -> OsString {
    std::env::var_os("CARGO_BIN_EXE_maestro-local-sdk-conformance")
        .expect("Cargo must provide the local SDK conformance binary with test-support")
}

#[cfg(feature = "test-support")]
struct FixtureProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    events: mpsc::Receiver<Result<Value, String>>,
    observed: Vec<Value>,
    stderr: Arc<Mutex<String>>,
}

#[cfg(feature = "test-support")]
impl FixtureProcess {
    fn start(workspace: &std::path::Path) -> Self {
        let home = workspace.join("home");
        fs::create_dir_all(&home).expect("create fixture home");

        let mut command = Command::new(conformance_binary());
        command
            .current_dir(workspace)
            .env_clear()
            .env("HOME", &home)
            .env("MAESTRO_HOME", home.join("maestro-home"))
            .env("MAESTRO_DISABLE_KEYCHAIN", "1")
            .env("MAESTRO_TELEMETRY", "0")
            .env("MAESTRO_AUTO_UPDATE", "0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for key in ["PATH", "LANG", "TMPDIR"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        let mut child = command.spawn().expect("spawn local SDK fixture");
        let stdout = child.stdout.take().expect("fixture stdout");
        let stderr = child.stderr.take().expect("fixture stderr");
        let stdin = child.stdin.take().expect("fixture stdin");

        let (event_tx, events) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let event = line
                    .map_err(|error| format!("read fixture stdout: {error}"))
                    .and_then(|line| {
                        serde_json::from_str::<Value>(&line)
                            .map_err(|error| format!("parse fixture stdout {line:?}: {error}"))
                    });
                if event_tx.send(event).is_err() {
                    return;
                }
            }
        });
        let stderr_text = Arc::new(Mutex::new(String::new()));
        let stderr_target = Arc::clone(&stderr_text);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut captured = Vec::with_capacity(8 * 1024);
            let mut buffer = [0_u8; 1024];
            while let Ok(read) = reader.read(&mut buffer) {
                if read == 0 {
                    break;
                }
                let remaining = (8_usize * 1024).saturating_sub(captured.len());
                captured.extend_from_slice(&buffer[..read.min(remaining)]);
            }
            *stderr_target.lock().expect("fixture stderr lock") =
                String::from_utf8_lossy(&captured).into_owned();
        });

        Self {
            child,
            stdin: Some(stdin),
            events,
            observed: Vec::new(),
            stderr: stderr_text,
        }
    }

    fn send(&mut self, message: Value) {
        let stdin = self.stdin.as_mut().expect("fixture stdin remains open");
        serde_json::to_writer(&mut *stdin, &message).expect("serialize fixture input");
        stdin.write_all(b"\n").expect("terminate fixture input");
        stdin.flush().expect("flush fixture input");
    }

    fn wait_for(&mut self, label: &str, predicate: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let event = self
                .events
                .recv_timeout(remaining)
                .unwrap_or_else(|error| {
                    panic!(
                        "timed out waiting for {label}: {error}; observed={}; stderr={}",
                        serde_json::to_string(&self.observed).unwrap_or_default(),
                        self.stderr.lock().expect("fixture stderr lock")
                    )
                })
                .unwrap_or_else(|error| {
                    panic!(
                        "fixture protocol failure while waiting for {label}: {error}; observed={}; stderr={}",
                        serde_json::to_string(&self.observed).unwrap_or_default(),
                        self.stderr.lock().expect("fixture stderr lock")
                    )
                });
            self.observed.push(event.clone());
            if predicate(&event) {
                return event;
            }
        }
    }

    fn finish(mut self) {
        drop(self.stdin.take());
        let deadline = Instant::now() + EVENT_TIMEOUT;
        let status = loop {
            match self.child.try_wait().expect("poll local SDK fixture") {
                Some(status) => break status,
                None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
                None => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    panic!(
                        "fixture did not exit after shutdown; observed={}; stderr={}",
                        serde_json::to_string(&self.observed).unwrap_or_default(),
                        self.stderr.lock().expect("fixture stderr lock")
                    );
                }
            }
        };
        assert!(
            status.success(),
            "fixture exited {status}; observed={}; stderr={}",
            serde_json::to_string(&self.observed).unwrap_or_default(),
            self.stderr.lock().expect("fixture stderr lock")
        );
    }
}

#[cfg(feature = "test-support")]
impl Drop for FixtureProcess {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[cfg(feature = "test-support")]
fn fixture_workspace() -> std::path::PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "maestro-local-sdk-profile-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("create fixture workspace");
    path
}

#[test]
fn profile_fixture_matches_the_native_protocol_version_and_restrictions() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../test/fixtures/local-sdk/profile-v1.json"
    ))
    .expect("parse local SDK profile fixture");

    assert_eq!(
        fixture["schemaVersion"],
        "evalops.maestro.local-sdk-profile.v1"
    );
    assert_eq!(
        fixture["protocolVersion"],
        maestro_tui::headless::HEADLESS_PROTOCOL_VERSION
    );
    assert_eq!(
        fixture["transport"]["command"],
        json!(["maestro", "--headless"])
    );
    assert_eq!(fixture["clientMessages"]["init"]["approvalMode"], "prompt");
    assert_eq!(
        fixture["clientMessages"]["tool_response"]["forbidden"],
        json!(["tool_execution_id", "result"])
    );
    assert!(
        fixture["excluded"]
            .as_array()
            .expect("fixture exclusions")
            .iter()
            .any(|value| value == "client_tool_result")
    );
}

#[cfg(feature = "test-support")]
#[test]
fn canonical_process_exercises_local_profile_with_native_tool_and_cancel() {
    let workspace = fixture_workspace();
    let mut fixture = FixtureProcess::start(&workspace);

    let ready = fixture.wait_for("ready", |event| event["type"] == "ready");
    assert_eq!(
        ready["protocol_version"],
        maestro_tui::headless::HEADLESS_PROTOCOL_VERSION
    );
    assert_eq!(ready["model"], "gpt-4o");

    fixture.send(json!({
        "type": "hello",
        "protocol_version": maestro_tui::headless::HEADLESS_PROTOCOL_VERSION,
        "client_info": {"name": "local-sdk-profile-test", "version": "1.0.0"}
    }));
    let hello_ok = fixture.wait_for("hello_ok", |event| event["type"] == "hello_ok");
    assert_eq!(
        hello_ok["protocol_version"],
        maestro_tui::headless::HEADLESS_PROTOCOL_VERSION
    );

    fixture.send(json!({"type": "init", "approval_mode": "prompt"}));
    fixture.wait_for("init applied", |event| {
        event["type"] == "status" && event["message"] == "init applied"
    });

    fixture.send(json!({"type": "prompt", "content": "exercise native approval"}));
    let tool_call = fixture.wait_for("approval-gated native tool", |event| {
        event["type"] == "tool_call" && event["requires_approval"] == true
    });
    assert_eq!(tool_call["tool"], "bash");
    assert_eq!(
        tool_call["args"]["command"],
        "printf local-sdk-native-tool > local-sdk-native-tool.txt && cat local-sdk-native-tool.txt"
    );
    assert!(tool_call.get("tool_execution_id").is_none());
    let output = workspace.join("local-sdk-native-tool.txt");
    assert!(!output.exists(), "native write must wait for approval");

    fixture.send(json!({
        "type": "tool_response",
        "call_id": tool_call["call_id"],
        "approved": true
    }));
    fixture.wait_for("native tool start", |event| event["type"] == "tool_start");
    let tool_end = fixture.wait_for("native tool end", |event| event["type"] == "tool_end");
    assert_eq!(tool_end["success"], true);
    assert_eq!(
        fs::read_to_string(&output).expect("read approved native write"),
        "local-sdk-native-tool"
    );
    fixture.wait_for("completed native turn", |event| {
        event["type"] == "turn_completed"
    });

    fixture.send(json!({"type": "prompt", "content": "exercise cancellation"}));
    fixture.wait_for("pending response start", |event| {
        event["type"] == "response_start"
    });
    fixture.send(json!({"type": "cancel"}));
    let cancelled = fixture.wait_for("cancelled terminal", |event| {
        event["type"] == "error" && event["terminal"] == true && event["error_type"] == "cancelled"
    });
    assert_eq!(cancelled["fatal"], false);

    fixture.send(json!({"type": "shutdown"}));
    fixture.wait_for("shutdown status", |event| {
        event["type"] == "status" && event["message"] == "shutting down"
    });
    fixture.finish();
    fs::remove_dir_all(workspace).expect("remove fixture workspace");
}

#[cfg(feature = "test-support")]
#[test]
fn canonical_process_denies_native_write_without_executing_it() {
    let workspace = fixture_workspace();
    let mut fixture = FixtureProcess::start(&workspace);
    fixture.wait_for("ready", |event| event["type"] == "ready");
    fixture.send(json!({
        "type": "hello",
        "protocol_version": maestro_tui::headless::HEADLESS_PROTOCOL_VERSION,
        "client_info": {"name": "local-sdk-profile-test", "version": "1.0.0"}
    }));
    fixture.wait_for("hello_ok", |event| event["type"] == "hello_ok");
    fixture.send(json!({"type": "init", "approval_mode": "prompt"}));
    fixture.wait_for("init applied", |event| {
        event["type"] == "status" && event["message"] == "init applied"
    });
    fixture.send(json!({"type": "prompt", "content": "deny native write"}));
    let tool_call = fixture.wait_for("approval-gated native write", |event| {
        event["type"] == "tool_call" && event["requires_approval"] == true
    });
    let output = workspace.join("local-sdk-native-tool.txt");
    assert!(!output.exists(), "native write must wait for approval");
    fixture.send(json!({
        "type": "tool_response",
        "call_id": tool_call["call_id"],
        "approved": false
    }));
    fixture.wait_for("completed denied turn", |event| {
        event["type"] == "turn_completed"
    });
    assert!(!output.exists(), "denied native write must not execute");
    assert!(
        !fixture.observed.iter().any(|event| {
            event["type"] == "tool_start" && event["call_id"] == tool_call["call_id"]
        }),
        "denied tool must not start"
    );
    fixture.send(json!({"type": "shutdown"}));
    fixture.wait_for("shutdown status", |event| {
        event["type"] == "status" && event["message"] == "shutting down"
    });
    fixture.finish();
    fs::remove_dir_all(workspace).expect("remove fixture workspace");
}
