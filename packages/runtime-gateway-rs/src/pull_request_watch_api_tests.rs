use super::*;

#[tokio::test]
async fn wrong_principal_cannot_list_stop_or_wake_an_owned_session() {
    let mut session = test_session_record("watch-session");
    session.owner = Some("owner".into());
    session.organization_id = Some("org".into());
    session.workspace_id = Some("workspace".into());
    let state = test_app_state_with_sessions(HashMap::from([(session.id.clone(), session)]));
    let auth = AuthContext {
        subject: Some("other".into()),
        organization_id: Some("org".into()),
        workspace_id: Some("workspace".into()),
        ..AuthContext::default()
    };
    for tool in [
        "list_pull_request_watches",
        "stop_pull_request_watch",
        "watch_pull_request",
    ] {
        let result = crate::pull_request_watch::handle_tool(
            &state,
            &auth,
            Some("watch-session"),
            tool,
            &serde_json::json!({"url":"https://github.com/dx-corp/mono/pull/1"}),
        )
        .await;
        assert!(!result.success);
    }
    assert!(
        crate::pull_request_watch::wake_native(&state, "watch-session", auth, "wake".into())
            .await
            .is_err()
    );
    assert!(
        state.sessions.lock().await.sessions["watch-session"]
            .messages
            .is_empty()
    );
}

#[test]
fn assistant_usage_cost_uses_contract_shape() {
    let message = composer_assistant_message(
        "done",
        "",
        Some(TokenUsage {
            input_tokens: 1,
            output_tokens: 2,
            cache_read_tokens: 3,
            cache_write_tokens: 4,
            cost: None,
        }),
    );

    assert!(message["usage"]["cost"]["input"].is_null());
    assert!(message["usage"]["cost"]["output"].is_null());
    assert!(message["usage"]["cost"]["cacheRead"].is_null());
    assert!(message["usage"]["cost"]["cacheWrite"].is_null());
    assert!(message["usage"]["cost"]["total"].is_null());
}
