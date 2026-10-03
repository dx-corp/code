use super::*;

#[tokio::test]
async fn test_tab_submits_when_idle_with_non_shell_input() {
    let mut app = new_test_app();
    // This binding tests an admitted turn; startup failures have a separate
    // regression below and must never invent a user-history entry.
    let temp = tempdir().unwrap();
    let (agent, _events) = crate::agent::NativeAgent::new_with_test_client(
        crate::agent::NativeAgentConfig {
            model: "openai/gpt-4o".into(),
            cwd: temp.path().display().to_string(),
            ..Default::default()
        },
        crate::ai::UnifiedClient::OpenAI(
            crate::ai::OpenAiClient::with_base_url("fixture", "http://127.0.0.1:1/v1").unwrap(),
        ),
    )
    .unwrap();
    app.native_agent = Some(agent);
    app.state.set_input("ship it");

    app.handle_key(KeyCode::Tab, CrosstermModifiers::NONE)
        .await
        .unwrap();

    assert_eq!(app.state.input(), "");
    let last = app.state.messages.last().expect("user message");
    assert_eq!(last.role, MessageRole::User);
    assert_eq!(last.content, "ship it");
}

#[tokio::test]
async fn failed_startup_keeps_recovery_error_and_does_not_record_phantom_turn() {
    let mut app = new_test_app();
    app.state.error = Some("Identity session revoked. Run `maestro login`.".to_owned());
    let count = app.state.messages.len();
    assert!(
        !app.submit_prompt_with_kind("Hello".to_owned(), PromptKind::Prompt)
            .await
            .unwrap()
    );
    assert_eq!(
        app.state.error.as_deref(),
        Some("Identity session revoked. Run `maestro login`.")
    );
    assert_eq!(app.state.messages.len(), count);
    assert!(!app.state.busy);
}

#[tokio::test]
async fn tab_after_failed_startup_keeps_input_and_recovery_action() {
    let mut app = new_test_app();
    app.state.error = Some("Identity expired. Run `maestro login`.".to_owned());
    app.state.set_input("retry this request");
    let count = app.state.messages.len();
    app.handle_key(KeyCode::Tab, CrosstermModifiers::NONE)
        .await
        .unwrap();
    assert_eq!(app.state.input(), "retry this request");
    assert_eq!(app.state.messages.len(), count);
    assert_eq!(
        app.state.error.as_deref(),
        Some("Identity expired. Run `maestro login`.")
    );
}
