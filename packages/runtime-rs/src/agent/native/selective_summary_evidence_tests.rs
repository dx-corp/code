//! Provider-boundary coverage for selective-summary evidence guidance.

use super::*;

#[tokio::test]
async fn selective_summary_uses_only_selected_history_without_tools_and_applies_conditionally() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_scripted_provider_request(&mut stream).await;
        let body = chat_sse_response("summary-fixture", "Selected facts only.", false);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        request
    });
    let workspace = tempfile::tempdir().unwrap();
    let config = NativeAgentConfig {
        model: "openai/gpt-4o".into(),
        model_dynamics: ModelDynamicsConfig {
            summary_model: Some("openai/gpt-4o-mini".into()),
            ..Default::default()
        },
        cwd: workspace.path().display().to_string(),
        ..NativeAgentConfig::default()
    };
    let client = UnifiedClient::OpenAI(
        crate::ai::OpenAiClient::with_base_url("test-key", format!("http://{address}/v1")).unwrap(),
    );
    let (agent, _events) =
        NativeAgent::new_with_external_tools(config, vec![], None, client).unwrap();
    let messages = vec![
        Message {
            role: Role::User,
            content: MessageContent::text("PRIVATE_UNSELECTED_PREFIX"),
        },
        Message {
            role: Role::Assistant,
            content: MessageContent::text("prefix answer"),
        },
        Message {
            role: Role::User,
            content: MessageContent::text("SELECTED_TURN_FACT"),
        },
        Message {
            role: Role::Assistant,
            content: MessageContent::text("selected answer"),
        },
    ];
    agent.replace_history_preserving_credentials(messages);
    let preview = agent
        .start_selective_summary_preview()
        .unwrap()
        .await
        .unwrap()
        .unwrap();
    let request = agent
        .start_selective_summary_with_instructions(
            crate::agent::RangeSelection::FromTurn(2),
            preview.history_digest.clone(),
            Some("Retain selected evidence".into()),
        )
        .unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(5), request.receiver)
        .await
        .unwrap()
        .unwrap();
    let proposed = outcome.result.unwrap();
    assert_eq!(proposed.summary, "Selected facts only.");
    let unchanged = agent
        .start_selective_summary_preview()
        .unwrap()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unchanged.history_digest, preview.history_digest);
    let captured = server.await.unwrap();
    let sent = serde_json::to_string(&captured["messages"]).unwrap();
    assert!(sent.contains("SELECTED_TURN_FACT"));
    assert!(sent.contains("Retain selected evidence"));
    assert!(sent.contains(maestro_context::compaction::SUMMARY_EVIDENCE_GUIDANCE));
    assert!(!sent.contains("PRIVATE_UNSELECTED_PREFIX"));
    assert!(
        captured
            .get("tools")
            .is_none_or(|v| v.as_array().is_some_and(Vec::is_empty))
    );
    assert_eq!(
        captured["model"], "gpt-4o-mini",
        "summary uses the configured model on the existing fixture connection"
    );
    assert!(
        captured["max_tokens"]
            .as_u64()
            .unwrap_or_else(|| captured["max_completion_tokens"].as_u64().unwrap())
            <= 2048
    );
    assert!(
        agent
            .apply_selective_summary(proposed.messages.clone(), "stale".into())
            .unwrap()
            .await
            .unwrap()
            .is_err()
    );
    assert_eq!(
        agent
            .start_selective_summary_preview()
            .unwrap()
            .await
            .unwrap()
            .unwrap()
            .history_digest,
        preview.history_digest
    );
    let orphan = vec![Message {
        role: Role::User,
        content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
            tool_use_id: "missing".into(),
            content: "orphan".into(),
            is_error: Some(false),
        }]),
    }];
    assert!(
        agent
            .apply_selective_summary(orphan, preview.history_digest.clone())
            .unwrap()
            .await
            .unwrap()
            .is_err()
    );
    agent
        .apply_selective_summary(proposed.messages, preview.history_digest.clone())
        .unwrap()
        .await
        .unwrap()
        .unwrap();
    assert_ne!(
        agent
            .start_selective_summary_preview()
            .unwrap()
            .await
            .unwrap()
            .unwrap()
            .history_digest,
        preview.history_digest
    );
    agent.shutdown().await;
}
