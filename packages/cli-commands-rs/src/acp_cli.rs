//! Native Agent Client Protocol (ACP v1) stdio adapter.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, oneshot};
use uuid::Uuid;

use crate::session::{
    AppMessage, ContentBlock, MessageContent, MessageEntry, SessionEntry, SessionHeader,
    SessionManager, SessionWriter, ThinkingLevel,
};

type SharedStdout = Arc<Mutex<tokio::io::Stdout>>;
type ActivePrompts = Arc<Mutex<HashMap<String, Option<oneshot::Sender<()>>>>>;
type SharedSessions = Arc<Mutex<HashMap<String, AcpSession>>>;

#[derive(Default)]
struct Utf8StreamDecoder {
    pending: Vec<u8>,
}

impl Utf8StreamDecoder {
    fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        self.take_available(false)
    }

    fn finish(&mut self) -> String {
        self.take_available(true)
    }

    fn take_available(&mut self, eof: bool) -> String {
        let mut output = String::new();
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(text) => {
                    output.push_str(text);
                    self.pending.clear();
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if valid > 0 {
                        output.push_str(
                            std::str::from_utf8(&self.pending[..valid])
                                .expect("validated UTF-8 prefix"),
                        );
                        self.pending.drain(..valid);
                    }
                    match error.error_len() {
                        Some(length) => {
                            output.push(char::REPLACEMENT_CHARACTER);
                            self.pending.drain(..length);
                        }
                        None if eof => {
                            output.push_str(&String::from_utf8_lossy(&self.pending));
                            self.pending.clear();
                            break;
                        }
                        None => break,
                    }
                }
            }
        }
        output
    }
}

#[derive(Debug)]
struct AcpSession {
    id: String,
    cwd: PathBuf,
    path: PathBuf,
    history: Vec<(String, String)>,
}

pub async fn run_acp(_args: &[String]) -> Result<i32> {
    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    let stdout = Arc::new(Mutex::new(tokio::io::stdout()));
    let sessions: SharedSessions = Arc::new(Mutex::new(HashMap::new()));
    let active_prompts: ActivePrompts = Arc::new(Mutex::new(HashMap::new()));

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(error) => {
                write_message(
                    &stdout,
                    &json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":error.to_string()}}),
                )
                .await?;
                continue;
            }
        };
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
        match method {
            "initialize" => {
                if params.get("protocolVersion").and_then(Value::as_u64) != Some(1) {
                    write_error(&stdout, id, -32602, "unsupported ACP protocol version").await?;
                    continue;
                }
                write_result(
                    &stdout,
                    id,
                    json!({
                        "protocolVersion": 1,
                        "agentCapabilities": {
                            "loadSession": true,
                            "promptCapabilities": {
                                "image": false,
                                "audio": false,
                                "embeddedContext": true
                            },
                            "mcpCapabilities": {"http": false, "sse": false}
                        },
                        "agentInfo": {
                            "name": "maestro",
                            "title": "Deixic Code",
                            "version": env!("CARGO_PKG_VERSION")
                        }
                    }),
                )
                .await?;
            }
            "session/new" => {
                let cwd = params
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(PathBuf::from)
                    .unwrap_or(std::env::current_dir()?);
                if !cwd.is_dir() {
                    write_error(&stdout, id, -32602, "session cwd is not a directory").await?;
                    continue;
                }
                let session_id = Uuid::new_v4().to_string();
                let session = match create_acp_session(&session_id, &cwd) {
                    Ok(session) => session,
                    Err(error) => {
                        write_error(&stdout, id, -32000, &error.to_string()).await?;
                        continue;
                    }
                };
                sessions.lock().await.insert(session_id.clone(), session);
                write_result(&stdout, id, json!({"sessionId": session_id})).await?;
            }
            "session/load" | "session/resume" => {
                let Some(session_id) = params.get("sessionId").and_then(Value::as_str) else {
                    write_error(&stdout, id, -32602, "missing sessionId").await?;
                    continue;
                };
                let requested_cwd = params
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(PathBuf::from)
                    .unwrap_or(std::env::current_dir()?);
                let session = match load_acp_session(session_id, &requested_cwd) {
                    Ok(session) => session,
                    Err(error) => {
                        write_error(&stdout, id, -32602, &error.to_string()).await?;
                        continue;
                    }
                };
                let should_replay = method == "session/load";
                let loaded_id = session.id.clone();
                let history = session.history.clone();
                sessions.lock().await.insert(loaded_id.clone(), session);
                if should_replay {
                    for (user, assistant) in history {
                        write_user_chunk(&stdout, &loaded_id, &user).await?;
                        write_agent_chunk(&stdout, &loaded_id, &assistant).await?;
                    }
                }
                write_result(&stdout, id, json!({"sessionId": loaded_id})).await?;
            }
            "session/list" => {
                let cwd = params
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(PathBuf::from)
                    .unwrap_or(std::env::current_dir()?);
                let manager = SessionManager::new(cwd.to_string_lossy().to_string());
                let summaries = manager
                    .list_sessions()
                    .map_err(|error| anyhow::anyhow!(error.to_string()))?
                    .into_iter()
                    .map(|session| {
                        json!({
                            "sessionId": session.id,
                            "cwd": session.cwd,
                            "title": session.title(),
                            "updatedAt": session.timestamp
                        })
                    })
                    .collect::<Vec<_>>();
                write_result(&stdout, id, json!({"sessions": summaries})).await?;
            }
            "session/prompt" => {
                let Some(session_id) = params.get("sessionId").and_then(Value::as_str) else {
                    write_error(&stdout, id, -32602, "missing sessionId").await?;
                    continue;
                };
                let prompt = prompt_text(&params);
                if prompt.is_empty() {
                    write_error(&stdout, id, -32602, "prompt contains no text").await?;
                    continue;
                }
                let Some((cwd, execution_prompt)) =
                    sessions.lock().await.get(session_id).map(|session| {
                        (
                            session.cwd.clone(),
                            prompt_with_history(&session.history, &prompt),
                        )
                    })
                else {
                    write_error(&stdout, id, -32602, "unknown sessionId").await?;
                    continue;
                };
                let (cancel_tx, cancel_rx) = oneshot::channel();
                if active_prompts.lock().await.contains_key(session_id) {
                    write_error(&stdout, id, -32600, "session already has an active prompt")
                        .await?;
                    continue;
                }
                active_prompts
                    .lock()
                    .await
                    .insert(session_id.to_string(), Some(cancel_tx));
                let task_stdout = Arc::clone(&stdout);
                let task_active = Arc::clone(&active_prompts);
                let task_sessions = Arc::clone(&sessions);
                let task_session_id = session_id.to_string();
                tokio::spawn(async move {
                    let result = execute_prompt(
                        &cwd,
                        &execution_prompt,
                        cancel_rx,
                        &task_stdout,
                        &task_session_id,
                    )
                    .await;
                    task_active.lock().await.remove(&task_session_id);
                    match result {
                        Ok(Some(text)) => {
                            let session =
                                task_sessions
                                    .lock()
                                    .await
                                    .get(&task_session_id)
                                    .map(|session| AcpSession {
                                        id: session.id.clone(),
                                        cwd: session.cwd.clone(),
                                        path: session.path.clone(),
                                        history: session.history.clone(),
                                    });
                            match session {
                                Some(session) => match append_acp_turn(&session, &prompt, &text) {
                                    Ok(()) => {
                                        if let Some(session) =
                                            task_sessions.lock().await.get_mut(&task_session_id)
                                        {
                                            session.history.push((prompt, text.clone()));
                                        }
                                        let _ = write_result(
                                            &task_stdout,
                                            id,
                                            json!({"stopReason":"end_turn"}),
                                        )
                                        .await;
                                    }
                                    Err(error) => {
                                        let _ = write_error(
                                            &task_stdout,
                                            id,
                                            -32000,
                                            &format!("failed to persist ACP session: {error}"),
                                        )
                                        .await;
                                    }
                                },
                                None => {
                                    let _ = write_error(
                                        &task_stdout,
                                        id,
                                        -32000,
                                        "session was closed before the prompt completed",
                                    )
                                    .await;
                                }
                            }
                        }
                        Ok(None) => {
                            let _ =
                                write_result(&task_stdout, id, json!({"stopReason":"cancelled"}))
                                    .await;
                        }
                        Err(error) => {
                            let _ = write_error(&task_stdout, id, -32000, &error.to_string()).await;
                        }
                    }
                });
            }
            "session/cancel" => {
                if let Some(session_id) = params.get("sessionId").and_then(Value::as_str) {
                    if let Some(sender) = active_prompts
                        .lock()
                        .await
                        .get_mut(session_id)
                        .and_then(Option::take)
                    {
                        let _ = sender.send(());
                    }
                }
                if !id.is_null() {
                    write_result(&stdout, id, json!({})).await?;
                }
            }
            "session/close" => {
                let Some(session_id) = params.get("sessionId").and_then(Value::as_str) else {
                    write_error(&stdout, id, -32602, "missing sessionId").await?;
                    continue;
                };
                if let Some(sender) = active_prompts
                    .lock()
                    .await
                    .get_mut(session_id)
                    .and_then(Option::take)
                {
                    let _ = sender.send(());
                }
                sessions.lock().await.remove(session_id);
                if !id.is_null() {
                    write_result(&stdout, id, json!({})).await?;
                }
            }
            _ if request.get("id").is_none() => {}
            _ => write_error(&stdout, id, -32601, "method not found").await?,
        }
    }
    Ok(0)
}

fn create_acp_session(session_id: &str, cwd: &Path) -> Result<AcpSession> {
    create_acp_session_in(session_id, cwd, None)
}

fn create_acp_session_in(
    session_id: &str,
    cwd: &Path,
    sessions_dir: Option<&Path>,
) -> Result<AcpSession> {
    let unified_context_manifest = crate::context_cli::load_unified_context_manifest_json(cwd)
        .context("failed to capture ACP context manifest")?;
    let header = SessionHeader {
        version: Some(1),
        id: session_id.to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        cwd: cwd.to_string_lossy().into_owned(),
        model: String::new(),
        subject: None,
        model_metadata: None,
        thinking_level: ThinkingLevel::Medium,
        system_prompt: None,
        prompt_metadata: None,
        prompt_context_manifest: None,
        unified_context_manifest: Some(Box::new(unified_context_manifest)),
        tools: Vec::new(),
        branched_from: None,
        parent_session: None,
    };
    let mut manager = sessions_dir.map_or_else(
        || SessionManager::new(cwd.to_string_lossy().to_string()),
        |sessions_dir| {
            SessionManager::with_sessions_dir(cwd.to_string_lossy().to_string(), sessions_dir)
        },
    );
    manager
        .start_session(header)
        .context("failed to create ACP session")?;
    manager
        .flush()
        .context("failed to flush ACP session header")?;
    let path = manager
        .current_session_path()
        .context("ACP session did not receive a durable path")?;
    drop(manager);
    Ok(AcpSession {
        id: session_id.to_string(),
        cwd: cwd.to_path_buf(),
        path,
        history: Vec::new(),
    })
}

fn load_acp_session(session_id: &str, cwd: &Path) -> Result<AcpSession> {
    let manager = SessionManager::new(cwd.to_string_lossy().to_string());
    let parsed = manager
        .load_session(session_id)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let session_id = parsed.header.id.clone();
    let saved_cwd = PathBuf::from(&parsed.header.cwd);
    let session_cwd = if saved_cwd.is_dir() {
        saved_cwd
    } else {
        cwd.to_path_buf()
    };
    Ok(AcpSession {
        id: session_id,
        cwd: session_cwd,
        path: PathBuf::from(parsed.file_path),
        history: history_from_messages(&parsed.messages),
    })
}

fn history_from_messages(messages: &[AppMessage]) -> Vec<(String, String)> {
    let mut history = Vec::new();
    let mut pending_user = None;
    for message in messages {
        match message {
            AppMessage::User { .. } => pending_user = Some(message.text_content()),
            AppMessage::Assistant { .. } => {
                if let Some(user) = pending_user.take() {
                    history.push((user, message.text_content()));
                }
            }
            AppMessage::ToolResult { .. } => {}
        }
    }
    history
}

fn append_acp_turn(session: &AcpSession, prompt: &str, response: &str) -> Result<()> {
    let timestamp = chrono::Utc::now().to_rfc3339();
    let timestamp_millis = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let entries = vec![
        SessionEntry::Message(MessageEntry {
            id: None,
            parent_id: None,
            timestamp: timestamp.clone(),
            message: AppMessage::User {
                content: MessageContent::Text(prompt.to_string()),
                attachments: None,
                timestamp: timestamp_millis,
            },
        }),
        SessionEntry::Message(MessageEntry {
            id: None,
            parent_id: None,
            timestamp,
            message: AppMessage::Assistant {
                content: vec![ContentBlock::Text {
                    text: response.to_string(),
                }],
                api: None,
                provider: None,
                model: None,
                usage: None,
                stop_reason: Some("end_turn".to_string()),
                timestamp: timestamp_millis,
            },
        }),
    ];
    let mut writer = SessionWriter::open_existing(&session.path)
        .context("failed to open ACP session for append")?;
    writer
        .write_entries(entries)
        .context("failed to append ACP session turn")?;
    writer.flush().context("failed to flush ACP session turn")?;
    Ok(())
}

fn prompt_with_history(history: &[(String, String)], prompt: &str) -> String {
    if history.is_empty() {
        return prompt.to_string();
    }
    let mut context =
        String::from("Continue this conversation. Preserve all prior context and decisions.\n");
    for (user, assistant) in history {
        context.push_str("\n<user>\n");
        context.push_str(user);
        context.push_str("\n</user>\n<assistant>\n");
        context.push_str(assistant);
        context.push_str("\n</assistant>\n");
    }
    context.push_str("\n<user>\n");
    context.push_str(prompt);
    context.push_str("\n</user>");
    context
}

fn prompt_text(params: &Value) -> String {
    params
        .get("prompt")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|block| match block.get("type").and_then(Value::as_str) {
            Some("text") => block.get("text").and_then(Value::as_str).map(str::to_owned),
            Some("resource_link") => {
                let name = block.get("name").and_then(Value::as_str)?;
                let uri = block.get("uri").and_then(Value::as_str)?;
                Some(format!("{name} ({uri})"))
            }
            Some("resource") => block
                .get("resource")
                .and_then(|resource| resource.get("text"))
                .or_else(|| block.get("text"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn execute_prompt(
    cwd: &Path,
    prompt: &str,
    mut cancelled: oneshot::Receiver<()>,
    acp_stdout: &SharedStdout,
    session_id: &str,
) -> Result<Option<String>> {
    let executable = std::env::current_exe()?;
    let mut command = Command::new(executable);
    command
        .args(exec_arguments())
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .context("failed to launch Deixic Code agent")?;
    let mut child_stdin = child.stdin.take().context("missing Deixic Code stdin")?;
    child_stdin
        .write_all(prompt.as_bytes())
        .await
        .context("failed to send prompt to Deixic Code agent")?;
    child_stdin
        .shutdown()
        .await
        .context("failed to close Deixic Code agent stdin")?;
    let mut child_stdout = child.stdout.take().context("missing Deixic Code stdout")?;
    let mut child_stderr = child.stderr.take().context("missing Deixic Code stderr")?;
    let stderr_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        child_stderr.read_to_end(&mut bytes).await.map(|_| bytes)
    });
    let mut response = Vec::new();
    let mut decoder = Utf8StreamDecoder::default();
    let mut buffer = [0_u8; 4096];
    loop {
        let read = tokio::select! {
            read = child_stdout.read(&mut buffer) => read.context("failed to read Deixic Code output")?,
            _ = &mut cancelled => {
                let _ = child.kill().await;
                return Ok(None);
            },
        };
        if read == 0 {
            break;
        }
        response.extend_from_slice(&buffer[..read]);
        let chunk = decoder.push(&buffer[..read]);
        if !chunk.is_empty() {
            write_agent_chunk(acp_stdout, session_id, &chunk).await?;
        }
    }
    let final_chunk = decoder.finish();
    if !final_chunk.is_empty() {
        write_agent_chunk(acp_stdout, session_id, &final_chunk).await?;
    }
    let status = child
        .wait()
        .await
        .context("failed to wait for Deixic Code agent")?;
    let stderr = stderr_task
        .await
        .context("failed to join Deixic Code stderr reader")??;
    if !status.success() {
        let message = String::from_utf8_lossy(&stderr).trim().to_string();
        anyhow::bail!(
            "Deixic Code agent exited with {}: {message}",
            status.code().unwrap_or(1)
        );
    }
    Ok(Some(String::from_utf8_lossy(&response).trim().to_string()))
}

fn exec_arguments() -> [&'static str; 4] {
    ["exec", "--approval-mode", "fail", "--prompt-stdin"]
}

async fn write_agent_chunk(stdout: &SharedStdout, session_id: &str, text: &str) -> Result<()> {
    write_message(
        stdout,
        &json!({
            "jsonrpc":"2.0",
            "method":"session/update",
            "params":{
                "sessionId":session_id,
                "update":{
                    "sessionUpdate":"agent_message_chunk",
                    "content":{"type":"text","text":text}
                }
            }
        }),
    )
    .await
}

async fn write_user_chunk(stdout: &SharedStdout, session_id: &str, text: &str) -> Result<()> {
    write_message(
        stdout,
        &json!({
            "jsonrpc":"2.0",
            "method":"session/update",
            "params":{
                "sessionId":session_id,
                "update":{
                    "sessionUpdate":"user_message_chunk",
                    "content":{"type":"text","text":text}
                }
            }
        }),
    )
    .await
}

async fn write_result(stdout: &SharedStdout, id: Value, result: Value) -> Result<()> {
    write_message(stdout, &json!({"jsonrpc":"2.0","id":id,"result":result})).await
}

async fn write_error(stdout: &SharedStdout, id: Value, code: i64, message: &str) -> Result<()> {
    write_message(
        stdout,
        &json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}),
    )
    .await
}

async fn write_message(stdout: &SharedStdout, value: &Value) -> Result<()> {
    let mut stdout = stdout.lock().await;
    stdout
        .write_all(serde_json::to_string(value)?.as_bytes())
        .await?;
    stdout.write_all(b"\n").await?;
    stdout.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_only_supported_prompt_content() {
        let params = json!({
            "prompt":[
                {"type":"text","text":"hello"},
                {"type":"image","data":"ignored"},
                {"type":"resource_link","name":"design spec","uri":"file:///context.md"},
                {"type":"resource","resource":{"uri":"file:///context.md","text":"context"}},
                {"type":"resource","text":"legacy context"}
            ]
        });
        assert_eq!(
            prompt_text(&params),
            "hello\ndesign spec (file:///context.md)\ncontext\nlegacy context"
        );
    }

    #[test]
    fn resource_link_only_prompt_preserves_name_and_uri() {
        let params = json!({
            "prompt":[
                {"type":"resource_link","name":"runbook","uri":"https://example.com/runbook"}
            ]
        });

        assert_eq!(
            prompt_text(&params),
            "runbook (https://example.com/runbook)"
        );
    }

    #[test]
    fn subsequent_prompts_include_prior_turns() {
        let prompt = prompt_with_history(
            &[(
                "Remember the codename is Juniper.".to_string(),
                "I will remember Juniper.".to_string(),
            )],
            "What is the codename?",
        );

        assert!(prompt.contains("<user>\nRemember the codename is Juniper.\n</user>"));
        assert!(prompt.contains("<assistant>\nI will remember Juniper.\n</assistant>"));
        assert!(prompt.ends_with("<user>\nWhat is the codename?\n</user>"));
    }

    #[test]
    fn durable_history_pairs_user_and_assistant_messages() {
        let messages = vec![
            AppMessage::User {
                content: MessageContent::Text("hello".to_string()),
                attachments: None,
                timestamp: 1,
            },
            AppMessage::Assistant {
                content: vec![ContentBlock::Text {
                    text: "hi".to_string(),
                }],
                api: None,
                provider: None,
                model: None,
                usage: None,
                stop_reason: Some("end_turn".to_string()),
                timestamp: 2,
            },
        ];

        assert_eq!(
            history_from_messages(&messages),
            vec![("hello".into(), "hi".into())]
        );
    }

    #[test]
    fn new_acp_session_header_is_persisted() {
        let temp = tempfile::TempDir::new().unwrap();
        let sessions_dir = temp.path().join("sessions");
        let session =
            create_acp_session_in("acp-session", temp.path(), Some(&sessions_dir)).unwrap();
        let parsed = crate::session::SessionReader::read_file(&session.path).unwrap();

        assert_eq!(parsed.header.id, "acp-session");
        assert_eq!(parsed.header.cwd, temp.path().to_string_lossy());
        let context_manifest = parsed
            .header
            .unified_context_manifest
            .as_ref()
            .expect("ACP sessions persist their context generation");
        assert_eq!(
            context_manifest
                .get("manifestSha256")
                .and_then(Value::as_str)
                .map(str::len),
            Some(71)
        );
        assert!(parsed.messages.is_empty());
    }

    #[test]
    fn streaming_decoder_preserves_split_unicode() {
        let mut decoder = Utf8StreamDecoder::default();
        let bytes = "A€B".as_bytes();

        assert_eq!(decoder.push(&bytes[..2]), "A");
        assert_eq!(decoder.push(&bytes[2..3]), "");
        assert_eq!(decoder.push(&bytes[3..]), "€B");
        assert_eq!(decoder.finish(), "");
    }

    #[test]
    fn exec_prompt_is_transferred_over_stdin() {
        assert_eq!(
            exec_arguments(),
            ["exec", "--approval-mode", "fail", "--prompt-stdin"]
        );
    }

    #[test]
    fn cancelling_prompt_keeps_session_busy_until_task_cleanup() {
        let (sender, _receiver) = oneshot::channel();
        let mut active = HashMap::from([("session".to_string(), Some(sender))]);
        let sender = active.get_mut("session").and_then(Option::take).unwrap();
        let _ = sender.send(());

        assert!(active.contains_key("session"));
        assert!(active["session"].is_none());
    }
}
