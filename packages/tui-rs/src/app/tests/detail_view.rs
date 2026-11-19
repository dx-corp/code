use super::*;

// ─────────────────────────────────────────────────────────────────────────
// Detail View Tests (Ctrl+E)
// ─────────────────────────────────────────────────────────────────────────

pub(super) fn push_tool_call_message(app: &mut App, output: String) {
    app.state.messages.push(Message {
        id: "m-detail".to_string(),
        role: MessageRole::Assistant,
        kind: MessageKind::Regular,
        content: String::new(),
        thinking: String::new(),
        streaming: false,
        tool_calls: vec![crate::state::ToolCallState {
            auto_chart: None,
            parent_call_id: None,
            duration_ms: None,
            call_id: "call-detail".to_string(),
            tool: "bash".to_string(),
            args: serde_json::json!({"command": "seq 1 120"}),
            status: crate::state::ToolCallStatus::Completed,
            output,
        }],
        usage: None,
        timestamp: SystemTime::now(),
        thinking_expanded: false,
    });
}

#[tokio::test]
async fn test_detail_view_opens_with_full_untruncated_tool_output() {
    let mut app = new_test_app();
    // The inline transcript caps tool output at 50 lines even when expanded;
    // the detail view must show every line.
    let output = (1..=120)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    push_tool_call_message(&mut app, output);

    app.handle_key(KeyCode::Char('e'), CrosstermModifiers::CONTROL)
        .await
        .expect("open detail view");

    assert_eq!(app.active_modal, ActiveModal::DetailView);
    let detail = app.detail_view.as_ref().expect("detail view open");
    assert_eq!(detail.title(), "Tool: bash");
    assert!(
        detail
            .content()
            .starts_with("Session evidence:\nLatest recorded checkpoint:")
    );
    assert!(detail.content().contains("tests not recorded"));
    assert!(detail.content().contains("\"command\": \"seq 1 120\""));
    assert!(detail.content().contains("line 1\n"));
    assert!(detail.content().contains("line 120"));
}

#[tokio::test]
async fn test_detail_view_esc_restores_transcript_view() {
    let mut app = new_test_app();
    push_tool_call_message(&mut app, "full output".to_string());

    app.handle_key(KeyCode::Char('e'), CrosstermModifiers::CONTROL)
        .await
        .expect("open detail view");
    assert_eq!(app.active_modal, ActiveModal::DetailView);

    app.handle_key(KeyCode::Esc, CrosstermModifiers::NONE)
        .await
        .expect("close detail view");
    assert_eq!(app.active_modal, ActiveModal::None);
    assert!(app.detail_view.is_none());
}

#[tokio::test]
async fn test_detail_view_includes_structured_chart_readings() {
    let mut app = new_test_app();
    app.state.messages.clear();
    push_tool_call_message(&mut app, String::new());
    let details = serde_json::json!({"structuredContent": {
        "columns": ["day", "turns"],
        "results": [["2026-11-08", 6], ["2026-11-09", 60]],
    }});
    let chart = crate::components::auto_chart::AutoChart::from_result("", Some(&details))
        .expect("structured rows chart");
    let readings = chart.structured_readings().expect("structured readings");
    app.state.messages[0].tool_calls[0].auto_chart = Some(std::sync::Arc::new(chart));

    app.open_detail_view();

    let detail = app.detail_view.as_ref().expect("detail view open");
    assert_eq!(detail.title(), "Tool: bash");
    assert!(detail.content().contains(&readings), "{}", detail.content());
}
