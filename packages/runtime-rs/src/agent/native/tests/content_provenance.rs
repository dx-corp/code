use super::*;

#[tokio::test]
async fn provenance_at_real_prompt_and_tool_boundaries_survives_the_checkpoint() {
    let workspace = tempfile::tempdir().unwrap();
    let attachment = workspace.path().join("AGENTS.md");
    std::fs::write(
        &attachment,
        "Repository guidance is not the user's request.",
    )
    .unwrap();
    let scripted = crate::ai::ScriptedClient::new(
        "runtime-test/provenance",
        vec![
            scripted_tool_turn("source-read", "read", json!({"path": "AGENTS.md"})),
            crate::ai::ScriptedResponse::text("done"),
        ],
    );
    let config = NativeAgentConfig {
        model: "runtime-test/provenance".into(),
        cwd: workspace.path().display().to_string(),
        approval_mode: ApprovalMode::Yolo,
        ..NativeAgentConfig::default()
    };
    let mut host = RuntimeTestHost::new(config.cwd.clone(), UnifiedClient::Scripted(scripted));
    host.post_tool_context = Some("Hook context stays separate.".into());
    let (agent, mut events) = new_runtime_test_agent_with_host(config, host).unwrap();
    agent
        .prompt(
            "Read the repository guidance.".into(),
            vec![attachment.display().to_string()],
        )
        .await
        .unwrap();
    let messages = tokio::time::timeout(Duration::from_secs(10), async {
        let mut checkpoint = None;
        loop {
            match events.recv().await {
                Some(FromAgent::ConversationSnapshot { messages, .. }) => {
                    checkpoint = Some(messages)
                }
                Some(FromAgent::TurnCompleted { .. }) => {
                    break checkpoint.expect("durable checkpoint");
                }
                Some(FromAgent::Error {
                    message,
                    terminal: true,
                    ..
                }) => panic!("{message}"),
                Some(_) => {}
                None => panic!("missing completion"),
            }
        }
    })
    .await
    .unwrap();
    agent.shutdown().await;
    let mut user = false;
    let mut repository = false;
    let mut hook = false;
    let mut tool = false;
    for message in &messages {
        let MessageContent::Blocks(parts) = &message.content else {
            continue;
        };
        for part in parts {
            match part {
                ContentBlock::AttributedText { text, provenance } => match provenance.source {
                    maestro_ai::ContentSource::User if text == "Read the repository guidance." => {
                        user = true
                    }
                    maestro_ai::ContentSource::Repository
                        if text.contains("Repository guidance") =>
                    {
                        assert_eq!(provenance.origin.as_deref(), attachment.to_str());
                        repository = true;
                    }
                    maestro_ai::ContentSource::Hook
                        if text.contains("Hook context stays separate.") =>
                    {
                        hook = true
                    }
                    _ => {}
                },
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } if tool_use_id == "source-read" => {
                    assert!(!content.contains("Hook context stays separate."));
                    tool = true;
                }
                _ => {}
            }
        }
    }
    assert!(
        user && repository && hook && tool,
        "missing source boundary: user={user} repository={repository} hook={hook} tool={tool}"
    );
    let record = crate::agent::compaction::build_continuation_record(&messages);
    assert_eq!(record.user_requests, vec!["Read the repository guidance."]);
}

#[test]
fn codex_prompt_projection_preserves_mixed_text_and_internal_provenance() {
    let message = Message {
        role: Role::User,
        content: MessageContent::Blocks(vec![
            ContentBlock::attributed_text(
                "Read this image.",
                maestro_ai::ContentSource::User,
                None,
            ),
            ContentBlock::Image {
                source: maestro_ai::ImageSource::Url {
                    url: "https://example.com/image.png".to_owned(),
                },
            },
            ContentBlock::attributed_text(
                "Repository context.",
                maestro_ai::ContentSource::Repository,
                Some("AGENTS.md".to_owned()),
            ),
            ContentBlock::Text {
                text: "Legacy context.".to_owned(),
            },
            ContentBlock::attributed_text("Hook context.", maestro_ai::ContentSource::Hook, None),
        ]),
    };
    let original = serde_json::to_value(&message).unwrap();
    let expected = "Read this image.\nRepository context.\nLegacy context.\nHook context.";
    // Initial prompts and active steering share this projection. Sources describe
    // internal parts; they do not filter text or become provider wire fields.
    assert_eq!(user_message_text(&message).as_deref(), Some(expected));
    assert_eq!(
        codex_app_server_user_text(std::slice::from_ref(&message), &[], Some(0)),
        expected
    );
    assert_eq!(
        codex_app_server_user_text(std::slice::from_ref(&message), &[], None),
        expected
    );
    assert_eq!(serde_json::to_value(&message).unwrap(), original);
    assert!(
        user_message_text(&Message {
            role: Role::Assistant,
            ..message
        })
        .is_none()
    );
}
