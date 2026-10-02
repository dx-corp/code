use super::*;

#[tokio::test]
async fn selective_summary_continue_restores_exact_provider_history() {
    assert_session_restore_provider_history(false).await;
}

#[tokio::test]
async fn interactive_resume_restores_exact_provider_history() {
    assert_session_restore_provider_history(true).await;
}

struct RestoreEnv(Vec<(String, Option<std::ffi::OsString>)>);
impl Drop for RestoreEnv {
    fn drop(&mut self) {
        for (name, value) in &self.0 {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

async fn assert_session_restore_provider_history(interactive: bool) {
    use crate::agent::{NativeAgent, NativeAgentConfig};
    let _env_lock = crate::config::test_process_env_lock_async().await;
    let temp = tempfile::tempdir().unwrap();
    let names = crate::credential_mode::TEST_IDENTITY_ENV_VARS
        .iter()
        .copied()
        .chain([
            "MAESTRO_HOME",
            "MAESTRO_OAUTH_STORAGE_MODE",
            "MAESTRO_DISABLE_KEYCHAIN",
            "OPENAI_API_KEY",
            "OPENAI_BASE_URL",
        ]);
    let restore = RestoreEnv(
        names
            .map(|name| (name.to_string(), std::env::var_os(name)))
            .collect(),
    );
    for (name, _) in &restore.0 {
        std::env::remove_var(name);
    }
    let _restore = restore;
    // Resuming also resolves Identity credentials; an injected provider client
    // does not isolate that owner or the host's native credential store.
    std::env::set_var("MAESTRO_HOME", temp.path());
    std::env::set_var("MAESTRO_OAUTH_STORAGE_MODE", "file");
    std::env::set_var("MAESTRO_DISABLE_KEYCHAIN", "1");
    crate::credential_mode::install_test_identity_env();
    std::env::set_var("OPENAI_API_KEY", "fixture");
    std::env::set_var("OPENAI_BASE_URL", "http://127.0.0.1:1/v1");
    let mut app = new_test_app();
    app.session_manager = SessionManager::with_sessions_dir("/tmp", temp.path());
    app.current_model = "gpt-6-astra".into();
    app.current_thinking_level = ThinkingLevel::Low;
    app.ensure_session_started().unwrap();
    let (_, child_path) = app.session_manager.fork_session_snapshot().unwrap();
    let history: Vec<crate::ai::Message> = serde_json::from_value(serde_json::json!([
        {"role":"user", "content":crate::agent::compaction::render_context_summary("previous work")},
        {"role":"user", "content":"retained request"},
        {"role":"assistant", "content":[{"type":"tool_use","id":"read-1","name":"read","input":{"path":"test.txt"}}]},
        {"role":"user", "content":[{"type":"tool_result","tool_use_id":"read-1","content":"result","is_error":false}]},
        {"role":"assistant", "content":"retained answer"}
    ])).unwrap();
    crate::session::append_selective_summary_checkpoint(&child_path, &history).unwrap();
    let original = app.session_manager.current_session_path().unwrap();
    std::fs::File::open(&original)
        .unwrap()
        .set_modified(std::time::UNIX_EPOCH)
        .unwrap();
    let client = crate::ai::UnifiedClient::OpenAI(
        crate::ai::OpenAiClient::with_base_url("fixture", "http://127.0.0.1:1/v1").unwrap(),
    );
    let (agent, _events) = NativeAgent::new_with_test_client(
        NativeAgentConfig {
            model: "openai/gpt-5.5".into(),
            cwd: "/tmp".into(),
            ..Default::default()
        },
        client,
    )
    .unwrap();
    let saved = crate::session::SessionReader::read_file(&child_path).unwrap();
    app.current_model = "gpt-5.6".into();
    app.current_thinking_level = ThinkingLevel::High;
    app.native_agent = Some(agent);
    if interactive {
        let session = crate::session::SessionReader::read_file(&child_path).unwrap();
        app.resume_session_path(std::path::Path::new(&session.file_path), &session.header.id);
    } else {
        app.continue_last_session();
    }
    assert_eq!(
        app.session_manager.current_session_path().as_ref(),
        Some(&child_path)
    );
    assert_eq!(app.current_model, saved.header.model);
    assert_eq!(app.current_thinking_level, saved.header.thinking_level);
    let preview = app
        .native_agent
        .as_ref()
        .unwrap()
        .start_selective_summary_preview()
        .unwrap();
    let actual = tokio::time::timeout(Duration::from_secs(5), preview)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let expected = crate::agent::selective_summary::preview(&history).unwrap();
    assert_eq!(actual.history_digest, expected.history_digest);
    app.native_agent.take().unwrap().shutdown().await;
}

#[tokio::test]
async fn resumed_continuation_survives_the_next_compaction() {
    use crate::agent::{NativeAgent, NativeAgentConfig};
    let temp = tempfile::tempdir().unwrap();
    let _env_lock = crate::config::test_process_env_lock_async().await;
    let names = crate::credential_mode::TEST_IDENTITY_ENV_VARS
        .iter()
        .copied()
        .chain(["MAESTRO_HOME", "OPENAI_API_KEY", "OPENAI_BASE_URL"]);
    let restore = RestoreEnv(
        names
            .map(|name| (name.to_string(), std::env::var_os(name)))
            .collect(),
    );
    for (name, _) in &restore.0 {
        std::env::remove_var(name);
    }
    let _restore = restore;
    crate::credential_mode::install_test_identity_env();
    std::env::set_var("MAESTRO_HOME", temp.path());
    std::env::set_var("OPENAI_API_KEY", "fixture");
    // Resume reauthorizes the saved model. Serve one real streaming response
    // locally so the normal post-response compaction boundary is exercised.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    std::env::set_var("OPENAI_BASE_URL", &base_url);
    let server = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        tokio::time::timeout(Duration::from_secs(20), async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 8192];
            loop {
                let count = stream.read(&mut buffer).await.unwrap();
                assert!(count > 0);
                request.extend_from_slice(&buffer[..count]);
                assert!(request.len() < 1024 * 1024);
                if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    let headers = std::str::from_utf8(&request[..end]).unwrap();
                    let length: usize = headers.lines().find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse().unwrap())
                    }).unwrap();
                    if request.len() >= end + 4 + length { break; }
                }
            }
            let chunk = serde_json::json!({
                "id": "rewind-test", "object": "chat.completion.chunk", "created": 1, "model": "gpt-4o",
                "choices": [{"index": 0, "delta": {"role": "assistant", "content": "done"}, "finish_reason": "stop"}]
            });
            let body = format!("data: {chunk}\n\ndata: [DONE]\n\n");
            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            stream.write_all(response.as_bytes()).await.unwrap();
        }).await.unwrap();
    });
    let mut app = new_test_app();
    app.session_manager = SessionManager::with_sessions_dir("/tmp", temp.path());
    app.current_model = "openai/gpt-4o".into();
    app.ensure_session_started().unwrap();
    let (_, child_path) = app.session_manager.fork_session_snapshot().unwrap();
    let mut writer = crate::session::SessionWriter::open_existing(&child_path).unwrap();
    let record = crate::agent::compaction::ContinuationRecord {
        user_requests: vec!["exact original request absent from summary".into()],
        ..Default::default()
    };
    writer
        .write_entry(
            serde_json::from_value(serde_json::json!({
                "type": "compaction", "timestamp": "2026-09-10T00:00:00Z",
                "summary": "earlier work", "firstKeptEntryIndex": 0,
                "tokensBefore": 5000, "continuation": record
            }))
            .unwrap(),
        )
        .unwrap();
    for index in 0..48 {
        writer.write_entry(serde_json::from_value(serde_json::json!({
            "type": "message", "timestamp": "2026-09-10T00:00:00Z",
            "message": {"role": "user", "content": format!("request {index}: {}", "evidence ".repeat(128))}
        })).unwrap()).unwrap();
    }
    writer.flush().unwrap();
    drop(writer);
    let saved = crate::session::SessionReader::read_file(&child_path).unwrap();

    let (agent, mut events) = NativeAgent::new_with_test_client(
        NativeAgentConfig {
            model: "openai/gpt-4o".into(),
            cwd: "/tmp".into(),
            context_window: Some(4096),
            max_tokens: 512,
            max_tokens_source: maestro_runtime::agent::MaxTokensSource::Explicit,
            ..Default::default()
        },
        crate::ai::UnifiedClient::OpenAI(
            crate::ai::OpenAiClient::with_base_url("fixture", &base_url).unwrap(),
        ),
    )
    .unwrap();
    app.native_agent = Some(agent);
    app.resume_session_path(std::path::Path::new(&saved.file_path), &saved.header.id);
    app.native_agent
        .as_ref()
        .unwrap()
        .prompt("continue".into(), vec![])
        .await
        .unwrap();
    let continuation = tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(event) = events.recv().await {
            match event {
                FromAgent::Compaction {
                    continuation: Some(record),
                    ..
                } => return record,
                FromAgent::TurnCompleted { .. } => panic!("large restored history did not compact"),
                FromAgent::Error { message, .. } | FromAgent::ProviderError { message, .. } => {
                    panic!("{message}")
                }
                _ => {}
            }
        }
        panic!("agent closed before compaction");
    })
    .await
    .unwrap();
    app.native_agent.take().unwrap().shutdown().await;
    server.await.unwrap();
    assert!(
        continuation
            .user_requests
            .iter()
            .any(|request| request == "exact original request absent from summary")
    );
}
