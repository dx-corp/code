use super::super::{MessageWidget, calculate_message_height};
use super::*;
use crate::{agent::FromAgent, state::AppState};
use maestro_runtime_contracts::ToolResult;
use ratatui::{buffer::Buffer, layout::Rect, widgets::Widget};
use serde_json::json;

#[test]
fn auto_chart_surfaces_posthog_query_from_a_collapsed_script() {
    let mut state = AppState::new();
    state.handle_agent_message(FromAgent::ResponseStart {
        response_id: "posthog".into(),
    });
    for (id, tool) in [("script", "codemode"), ("query", "mcp__posthog__exec")] {
        state.handle_agent_message(FromAgent::ToolCall {
            call_id: id.into(),
            tool: tool.into(),
            args: json!({}),
            requires_approval: false,
            approval_inline_env: None,
        });
    }
    state.handle_agent_message(serde_json::from_value(json!({
        "type": "code_mode_progress",
        "call_id": "script",
        "children": [{"call_id": "query", "tool": "mcp__posthog__exec", "status": {"status": "succeeded"}}]
    })).unwrap());
    let output = "Date|daily_turns\n2026-11-17 08:00|4\n2026-11-17 09:00|8";
    state.handle_agent_message(FromAgent::ToolOutput {
        call_id: "query".into(),
        content: output.into(),
    });
    state.handle_agent_message(FromAgent::ToolEnd {
        call_id: "query".into(),
        success: true,
        result: Some(ToolResult::success(output).with_details(json!({
            "server": "posthog",
            "tool": "exec",
            "content": [{"type": "text", "text": output}],
            "isError": false
        }))),
        receipt: None,
    });
    let width = 80;
    let collapsed = HashSet::new();
    assert!(latest_chart_call(&state.messages[0], Some(&collapsed), true, width).is_none());

    state.handle_agent_message(FromAgent::ToolEnd {
        call_id: "script".into(),
        success: true,
        result: Some(ToolResult::success("Query complete")),
        receipt: None,
    });
    let message = &state.messages[0];
    assert_eq!(message.tool_calls[1].output, output);
    assert!(!visible_script_child(
        &message.tool_calls[1],
        Some(&collapsed)
    ));
    let (id, chart) = latest_chart_call(message, Some(&collapsed), true, width).unwrap();
    assert_eq!(id, "script");
    let expanded = HashSet::from(["script".to_string()]);
    assert_eq!(
        latest_chart_call(message, Some(&expanded), true, width).map(|(id, _)| id),
        Some("query")
    );
    let message_height = |auto_charts| {
        calculate_message_height(
            message,
            width,
            &collapsed,
            true,
            false,
            &HashSet::new(),
            auto_charts,
        )
    };
    assert_eq!(message_height(true) - message_height(false), chart.height());
    let area = Rect::new(0, 0, width, message_height(true));
    let mut buffer = Buffer::empty(area);
    MessageWidget::new(message)
        .with_compact_tool_outputs(true)
        .with_auto_charts(true)
        .render(area, &mut buffer);
    let labels = (0..area.height).map(|y| {
        (0..width)
            .filter_map(|x| buffer.cell((x, y)))
            .map(|cell| cell.symbol())
            .collect::<String>()
    });
    assert_eq!(labels.filter(|row| row.contains("daily_turns")).count(), 1);
}

#[test]
fn expanding_a_structured_chart_shows_its_readings() {
    let mut message = Message {
        id: "chart".into(),
        role: crate::state::MessageRole::Assistant,
        kind: crate::state::MessageKind::Regular,
        content: "Done.".into(),
        thinking: String::new(),
        streaming: false,
        tool_calls: vec![],
        usage: None,
        timestamp: std::time::SystemTime::UNIX_EPOCH,
        thinking_expanded: false,
    };
    let details = serde_json::json!({"structuredContent": {
        "columns": ["day", "latest_metric"],
        "results": [["2026-11-08", 3], ["2026-11-18", 4]],
    }});
    message.tool_calls.push(crate::state::ToolCallState {
        auto_chart: AutoChart::from_result("Query complete", Some(&details))
            .map(std::sync::Arc::new),
        parent_call_id: None,
        duration_ms: None,
        call_id: "structured".into(),
        tool: "bash".into(),
        args: serde_json::json!({"command": "query"}),
        status: ToolCallStatus::Completed,
        output: "Query complete".into(),
    });
    let width = 80;
    let render = |expanded: &HashSet<String>| {
        let height = calculate_message_height(
            &message,
            width,
            expanded,
            true,
            false,
            &HashSet::new(),
            true,
        );
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        MessageWidget::new(&message)
            .with_expanded_tools(expanded)
            .with_compact_tool_outputs(true)
            .with_auto_charts(true)
            .render(area, &mut buffer);
        (0..area.height)
            .map(|y| {
                (0..width)
                    .filter_map(|x| buffer.cell((x, y)))
                    .map(|cell| cell.symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
    };
    let readings = maestro_ui::localization::tr("Chart readings");
    let collapsed = render(&HashSet::new());
    assert!(collapsed.iter().any(|row| row.contains("Auto chart")));
    assert!(collapsed.iter().all(|row| !row.contains(readings)));

    let expanded = render(&HashSet::from(["structured".to_owned()]));
    assert!(expanded.iter().all(|row| !row.contains("Auto chart")));
    assert!(expanded.iter().any(|row| row.contains(readings)));
    assert!(
        expanded
            .iter()
            .any(|row| row.contains("time | latest_metric"))
    );
    assert!(expanded.iter().any(|row| row.contains("2026-11-18 | 4")));
}
