use super::*;

#[tokio::test]
async fn test_handle_config_event_forces_mcp_badge_refresh() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(temp.path().join(".composer")).expect("create config dir");

    let mut app = new_test_app();
    let (agent, _events) = crate::agent::NativeAgent::new_with_test_client(
        std::sync::Arc::new(maestro_local_host::agent::tool_host::BuiltInToolHost),
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
    app.tool_executor = Arc::new(ToolExecutor::new(temp.path().display().to_string()));
    app.last_mcp_status_refresh = Some(Instant::now());
    app.state.mcp_connected = 7;
    app.state.mcp_tool_count = 9;
    app.state.mcp_failed = 2;

    app.handle_config_event(ConfigEvent::Changed(std::path::PathBuf::from(
        ".composer/mcp.json",
    )))
    .await;

    assert!(app.mcp_status_refresh_in_flight);
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if app.poll_mcp_status_refresh() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("background MCP status refresh");
    assert_eq!(app.state.mcp_connected, 0);
    assert_eq!(app.state.mcp_tool_count, 0);
    assert_eq!(app.state.mcp_failed, 0);
    assert!(app.last_mcp_status_refresh.is_some());
}
