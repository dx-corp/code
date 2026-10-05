//! Test-only provider-shaped transcripts through the native stdio parser.
//!
//! The fixture subprocess is this Rust test binary, never a model SDK or a
//! second runtime. See fixtures/provider_replay/README.md for provenance.

use super::*;
use std::io::{BufRead, Write};
use std::path::PathBuf;

const TRANSCRIPT: &str = include_str!("fixtures/provider_replay/restart.ndjson");
const SUBPROCESS_TEST: &str =
    "agent::codex_app_server_turns::provider_replay::provider_replay_subprocess";

fn materialize(frame: &mut Value, workspace: &str, request_id: &Value) {
    match frame {
        Value::String(value) if value == "$workspace" => *frame = json!(workspace),
        Value::String(value) if value == "$request_id" => *frame = request_id.clone(),
        Value::Object(fields) => {
            for value in fields.values_mut() {
                materialize(value, workspace, request_id);
            }
        }
        Value::Array(values) => {
            for value in values {
                materialize(value, workspace, request_id);
            }
        }
        _ => {}
    }
}

// Match all declared contract fields while permitting provider/client metadata
// outside the fixture. Arrays are exact, so an extra prompt cannot be hidden.
fn assert_frame(expected: &Value, actual: &Value) {
    match expected {
        Value::Object(fields) => {
            let actual = actual.as_object().expect("expected JSON object");
            for (key, value) in fields {
                assert_frame(
                    value,
                    actual.get(key).unwrap_or_else(|| panic!("missing {key}")),
                );
            }
        }
        Value::Array(values) => {
            let actual = actual.as_array().expect("expected JSON array");
            assert_eq!(values.len(), actual.len(), "unexpected additional input");
            for (expected, actual) in values.iter().zip(actual) {
                assert_frame(expected, actual);
            }
        }
        _ => assert_eq!(expected, actual, "outbound frame differs from transcript"),
    }
}

/// Launched by the parent tests with child-only environment, over real stdio.
#[test]
#[ignore = "provider fixture subprocess; invoked by provider_replay tests"]
fn provider_replay_subprocess() {
    let Ok(phase) = env::var("MAESTRO_REPLAY_PHASE") else {
        return;
    };
    let result = std::panic::catch_unwind(|| {
        let workspace = env::var("MAESTRO_REPLAY_WORKSPACE").expect("workspace");
        let audit = env::var("MAESTRO_REPLAY_AUDIT").expect("audit path");
        let mut audit = std::fs::File::create(audit).expect("create audit");
        // A hung parent cannot leave a fixture child blocked indefinitely on stdin.
        let (line_tx, line_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                if line_tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stdout = std::io::stdout();
        let mut stdout = stdout.lock();
        let mut request_id = Value::Null;
        let mut steps = 0;
        for line in TRANSCRIPT.lines() {
            let step: Value = serde_json::from_str(line).expect("fixture JSON");
            if step["phase"] != phase {
                continue;
            }
            steps += 1;
            let mut frame = step["frame"].clone();
            materialize(&mut frame, &workspace, &request_id);
            match step["direction"].as_str().expect("direction") {
                "outbound" => {
                    let line = line_rx
                        .recv_timeout(Duration::from_secs(5))
                        .expect("expected bounded client frame")
                        .expect("read stdin");
                    let actual: Value = serde_json::from_str(&line).expect("client JSON");
                    assert_frame(&frame, &actual);
                    if actual.get("method").is_some() && actual["id"].is_number() {
                        request_id = actual["id"].clone();
                    }
                    writeln!(audit, "{actual}").expect("audit frame");
                    audit.flush().expect("flush audit");
                }
                "inbound" => {
                    writeln!(stdout, "{frame}").expect("write provider frame");
                    stdout.flush().expect("flush provider frame");
                }
                direction => panic!("unknown fixture direction {direction}"),
            }
        }
        assert!(steps > 0, "unknown transcript phase {phase}");
        // Completion is authoritative only after the parent has dropped its
        // client and the child has exhausted stdin, including any extra frames.
        assert!(
            matches!(
                line_rx.recv_timeout(Duration::from_secs(5)),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
            ),
            "unexpected client frame or no EOF after transcript"
        );
        writeln!(audit, "{{\"complete\":true}}").expect("audit completion");
        audit.flush().expect("flush completion");
    });
    let address = env::var("MAESTRO_REPLAY_COMPLETION")
        .expect("completion address")
        .parse()
        .expect("socket address");
    let mut completion = std::net::TcpStream::connect_timeout(&address, Duration::from_secs(5))
        .expect("completion connection");
    completion
        .set_write_timeout(Some(Duration::from_secs(5)))
        .expect("write timeout");
    completion
        .write_all(if result.is_ok() {
            b"complete\n"
        } else {
            b"failed\n"
        })
        .expect("completion signal");
    assert!(result.is_ok(), "transcript subprocess failed");
}

struct ReplayProcess {
    client: CodexAppServerClient,
    audit: PathBuf,
    completion: tokio::net::TcpListener,
}

async fn replay_process(phase: &str, workspace: &Path, audit_root: &Path) -> ReplayProcess {
    // Session keys normalize aliases before the real client sends cwd. The
    // provider fixture must expect that same wire path, including on macOS.
    let workspace = CodexSessionKey::new("fixture-profile", workspace, "gpt-5.5")
        .expect("canonical fixture workspace")
        .workspace;
    let audit = audit_root.join(format!("{phase}.ndjson"));
    let completion = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("completion listener");
    let child_env = HashMap::from([
        (
            "MAESTRO_REPLAY_COMPLETION".to_owned(),
            completion.local_addr().expect("address").to_string(),
        ),
        ("MAESTRO_REPLAY_PHASE".to_owned(), phase.to_owned()),
        (
            "MAESTRO_REPLAY_WORKSPACE".to_owned(),
            workspace.to_string_lossy().into_owned(),
        ),
        (
            "MAESTRO_REPLAY_AUDIT".to_owned(),
            audit.to_string_lossy().into_owned(),
        ),
    ]);
    let client = CodexAppServerClient::spawn_with_env(
        Some(
            env::current_exe()
                .expect("test binary")
                .to_string_lossy()
                .into_owned(),
        ),
        Some(vec![
            "--exact".to_owned(),
            SUBPROCESS_TEST.to_owned(),
            "--ignored".to_owned(),
            "--nocapture".to_owned(),
            "--format=terse".to_owned(),
        ]),
        Some(5_000),
        &child_env,
    )
    .await
    .expect("spawn Rust provider fixture");
    ReplayProcess {
        client,
        audit,
        completion,
    }
}

fn manifest(workspace: &Path, session_id: &str) -> CodexSessionManifest {
    CodexSessionManifest {
        key: CodexSessionKey::new("fixture-profile", workspace, "gpt-5.5")
            .expect("session key")
            .with_session_id(Some(session_id)),
        approval_policy: "on-request".to_owned(),
        sandbox: "read-only".to_owned(),
        capabilities: CodexCapabilities::default(),
    }
}

async fn completion_signal(listener: tokio::net::TcpListener) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    tokio::time::timeout(Duration::from_secs(5), async {
        let (mut socket, _) = listener.accept().await.expect("child completion");
        let mut signal = Vec::new();
        socket
            .read_to_end(&mut signal)
            .await
            .expect("completion signal");
        signal
    })
    .await
    .expect("child must exhaust stdin and acknowledge completion")
}

async fn completed_audit(listener: tokio::net::TcpListener, path: &Path) -> Vec<Value> {
    assert_eq!(completion_signal(listener).await, b"complete\n");
    let frames = audit_frames(path);
    assert_eq!(frames.last(), Some(&json!({"complete": true})));
    frames
}

fn audit_frames(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .expect("read audit")
        .lines()
        .map(|line| serde_json::from_str(line).expect("audit JSON"))
        .collect()
}

async fn replay_turn(
    session: &CodexAppServerTurnSession,
    prompt: &str,
    expected_text: &str,
    calls: &mut Vec<(String, String, Value)>,
    approvals: &mut Vec<Value>,
) {
    let turn_id = session
        .start_text_turn(prompt, Some(5_000))
        .await
        .expect("start turn");
    loop {
        match session
            .wait_server_request_or_turn_complete(&turn_id, Some(5_000))
            .await
            .expect("provider event")
        {
            TurnWaitEvent::ServerRequest(request) => {
                assert_eq!(
                    request.params.as_ref().unwrap()["threadId"],
                    session.thread_id()
                );
                assert_eq!(request.params.as_ref().unwrap()["turnId"], turn_id);
                match request.method.as_str() {
                    "item/tool/call" => {
                        let call = parse_tool_call_params(request.params.as_ref().unwrap())
                            .expect("production tool parser");
                        let result = match call.0.as_str() {
                            "spawn_subagent" | "resume_subagent" => "child-session",
                            "read" => "fixture contents",
                            name => panic!("unexpected tool {name}"),
                        };
                        calls.push(call);
                        request.respond(tool_call_success_result(result));
                    }
                    "item/commandExecution/requestApproval" => {
                        approvals.push(request.id.clone());
                        request.respond(approval_decision(false));
                    }
                    method => panic!("unexpected server request {method}"),
                }
            }
            TurnWaitEvent::Completed(result) => {
                assert_eq!(result.thread_id, session.thread_id());
                assert_eq!(result.turn_id, turn_id);
                assert_eq!(
                    result.assistant_text, expected_text,
                    "delta and completion must reconcile once"
                );
                assert!(result.assistant_text_is_full);
                assert!(result.provider_failure().is_none());
                break;
            }
            TurnWaitEvent::Pending => panic!("transcript did not complete"),
        }
    }
    // A real provider method fences the transcript: unexpected injection or
    // duplicated starts/replies fail the subprocess before this response.
    session
        .client()
        .request(
            "thread/read",
            Some(json!({
                "threadId": session.thread_id(), "includeTurns": false
            })),
            Some(5_000),
        )
        .await
        .expect("transcript fence");
}

#[tokio::test]
async fn provider_replay_parent_and_child_resume_after_runtime_recreation() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let state = tempfile::tempdir().expect("state");
        let workspace = tempfile::tempdir().expect("workspace");
        let parent = manifest(workspace.path(), "parent-session");
        let child = manifest(workspace.path(), "child-session");
        let mut calls = Vec::new();
        let mut approvals = Vec::new();
        let cases = [
            (
                "parent_created",
                &parent,
                "thread-parent",
                "Inspect the fixture with a child.",
                "Child queued.",
                CodexSessionOpen::Created,
            ),
            (
                "child_created",
                &child,
                "thread-child",
                "Inspect fixture.txt.",
                "Fixture inspected.",
                CodexSessionOpen::Created,
            ),
            (
                "parent_resumed",
                &parent,
                "thread-parent",
                "Ask the same child to report.",
                "Existing child resumed.",
                CodexSessionOpen::Resumed,
            ),
            (
                "child_resumed",
                &child,
                "thread-child",
                "Report the inspection.",
                "Fixture report complete.",
                CodexSessionOpen::Resumed,
            ),
        ];
        let mut wire_frames = Vec::new();
        for (phase, manifest, thread_id, prompt, text, open_kind) in cases {
            let process = replay_process(phase, workspace.path(), state.path()).await;
            let history = if open_kind == CodexSessionOpen::Resumed {
                vec![Message {
                    role: Role::User,
                    content: MessageContent::text("Previously accepted prompt must not replay"),
                }]
            } else {
                Vec::new()
            };
            let names = if manifest.key.session_id.as_deref() == Some("parent-session") {
                vec!["spawn_subagent", "resume_subagent"]
            } else {
                vec!["read"]
            };
            let tools = names
                .into_iter()
                .map(|name| DynamicToolSpec {
                    name: name.to_owned(),
                    description: format!("Fixture {name}"),
                    input_schema: json!({"type": "object"}),
                })
                .collect::<Vec<_>>();
            let session = CodexAppServerTurnSession::connect_with_client_and_manifest(
                process.client,
                manifest.clone(),
                state.path(),
                &tools,
                Some("Fixture replay instructions.".to_owned()),
                &history,
            )
            .await
            .expect("open persistent session");
            assert_eq!(session.thread_id(), thread_id);
            assert_eq!(session.open_kind(), open_kind);
            replay_turn(&session, prompt, text, &mut calls, &mut approvals).await;
            let binding = CodexThreadBinding::load_at(state.path(), &manifest.key)
                .expect("read durable binding")
                .expect("binding exists");
            assert_eq!(binding.thread_id, thread_id);
            // Read the audit only after a wire fence; no sleep/poll determines
            // correctness. The next case gets a fresh process and runtime.
            drop(session);
            let frames = completed_audit(process.completion, &process.audit).await;
            assert_eq!(
                frames
                    .iter()
                    .filter(|frame| frame["method"] == "turn/start")
                    .count(),
                1
            );
            assert!(
                !frames
                    .iter()
                    .any(|frame| frame["method"] == "thread/inject_items")
            );
            wire_frames.extend(frames);
        }
        assert_eq!(
            calls.iter().map(|call| call.1.as_str()).collect::<Vec<_>>(),
            ["call-spawn", "call-read", "call-resume"]
        );
        assert_eq!(calls[2].2["id"], child.key.session_id.as_deref().unwrap());
        assert_eq!(approvals, [json!("approval-command-1")]);
        assert_eq!(
            wire_frames
                .iter()
                .filter(|frame| frame["method"] == "thread/start")
                .count(),
            2
        );
        assert_eq!(
            wire_frames
                .iter()
                .filter(|frame| frame["method"] == "thread/resume")
                .count(),
            2
        );
    })
    .await
    .expect("bounded provider replay");
}

#[tokio::test]
async fn provider_replay_resume_failure_preserves_binding_without_replacement() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let state = tempfile::tempdir().expect("state");
        let workspace = tempfile::tempdir().expect("workspace");
        let manifest = manifest(workspace.path(), "parent-session");
        let binding = CodexThreadBinding::new(
            manifest.key.clone(),
            "thread-parent",
            Some("2025-01-01".to_owned()),
            1,
        )
        .with_tool_projection(dynamic_tool_projection(&[]).unwrap());
        binding.store_at(state.path()).expect("store binding");
        let process = replay_process("resume_unavailable", workspace.path(), state.path()).await;
        let error = CodexAppServerTurnSession::connect_with_client_and_manifest(
            process.client,
            manifest.clone(),
            state.path(),
            &[],
            None,
            &[],
        )
        .await
        .err()
        .expect("resume must fail");
        assert!(
            error
                .to_string()
                .contains("Provider temporarily unavailable")
        );
        assert_eq!(
            CodexThreadBinding::load_at(state.path(), &manifest.key).expect("load binding"),
            Some(binding)
        );
        let frames = completed_audit(process.completion, &process.audit).await;
        assert_eq!(
            frames
                .iter()
                .filter(|frame| frame["method"] == "thread/resume")
                .count(),
            1
        );
        assert!(!frames.iter().any(|frame| matches!(
            frame["method"].as_str(),
            Some("thread/start" | "thread/inject_items" | "turn/start")
        )));
    })
    .await
    .expect("bounded provider failure replay");
}

#[cfg(unix)]
#[tokio::test]
async fn provider_replay_accepts_workspace_symlink_without_replacing_binding() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let alias = state.path().join("workspace-alias");
    std::os::unix::fs::symlink(workspace.path(), &alias).expect("workspace symlink");
    let manifest = manifest(&alias, "parent-session");
    assert_ne!(manifest.key.workspace, alias);
    let binding = CodexThreadBinding::new(
        manifest.key.clone(),
        "thread-parent",
        Some("2025-01-01".to_owned()),
        1,
    )
    .with_tool_projection(dynamic_tool_projection(&[]).unwrap());
    binding.store_at(state.path()).expect("store binding");
    let process = replay_process("resume_unavailable", &alias, state.path()).await;
    let error = CodexAppServerTurnSession::connect_with_client_and_manifest(
        process.client,
        manifest.clone(),
        state.path(),
        &[],
        None,
        &[],
    )
    .await
    .err()
    .expect("provider resume must fail");
    assert!(
        error
            .to_string()
            .contains("Provider temporarily unavailable")
    );
    assert_eq!(
        CodexThreadBinding::load_at(state.path(), &manifest.key).expect("load binding"),
        Some(binding)
    );
    let frames = completed_audit(process.completion, &process.audit).await;
    assert_eq!(
        frames[2]["params"]["cwd"],
        manifest.key.workspace.to_string_lossy().as_ref()
    );
}

#[tokio::test]
async fn provider_replay_rejects_extra_action_after_final_fence() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let state = tempfile::tempdir().expect("state");
        let workspace = tempfile::tempdir().expect("workspace");
        let process = replay_process("child_created", workspace.path(), state.path()).await;
        let tools = vec![DynamicToolSpec {
            name: "read".to_owned(),
            description: "Fixture read".to_owned(),
            input_schema: json!({"type": "object"}),
        }];
        let session = CodexAppServerTurnSession::connect_with_client_and_manifest(
            process.client,
            manifest(workspace.path(), "child-session"),
            state.path(),
            &tools,
            Some("Fixture replay instructions.".to_owned()),
            &[],
        )
        .await
        .expect("open child");
        replay_turn(
            &session,
            "Inspect fixture.txt.",
            "Fixture inspected.",
            &mut Vec::new(),
            &mut Vec::new(),
        )
        .await;
        let duplicate = session.client().request("turn/start", Some(json!({
            "threadId": "thread-child", "input": [{"type": "text", "text": "Inspect fixture.txt."}]
        })), Some(5_000)).await;
        assert!(
            duplicate.is_err(),
            "unsolicited action must terminate transcript"
        );
        drop(session);
        assert_eq!(completion_signal(process.completion).await, b"failed\n");
        assert!(
            !audit_frames(&process.audit)
                .iter()
                .any(|frame| frame["complete"] == true)
        );
    })
    .await
    .expect("bounded negative replay");
}
