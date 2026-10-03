use super::*;
use crate::state::ToolCallState;

fn script_message() -> Message {
    let call = |id: &str, tool: &str, status, parent: Option<&str>, path: &str| ToolCallState {
        call_id: id.into(),
        parent_call_id: parent.map(str::to_owned),
        duration_ms: Some(12),
        tool: tool.into(),
        args: serde_json::json!({"path":path}),
        status,
        output: if status == ToolCallStatus::Failed {
            "Needs attention: denied".into()
        } else {
            String::new()
        },
    };
    Message {
        id: "script-message".into(),
        role: MessageRole::Assistant,
        kind: MessageKind::Regular,
        content: String::new(),
        thinking: String::new(),
        streaming: false,
        tool_calls: vec![
            call("script", "codemode", ToolCallStatus::Completed, None, ""),
            call(
                "script/0",
                "read",
                ToolCallStatus::Completed,
                Some("script"),
                "completed.json",
            ),
            call(
                "script/1",
                "write",
                ToolCallStatus::Failed,
                Some("script"),
                "failed.json",
            ),
            call(
                "script/2",
                "write",
                ToolCallStatus::Pending,
                Some("script"),
                "pending.json",
            ),
        ],
        usage: None,
        timestamp: std::time::SystemTime::UNIX_EPOCH,
        thinking_expanded: false,
    }
}

fn rendered(message: &Message, expanded: &HashSet<String>) -> String {
    let area = Rect::new(0, 0, 100, 30);
    let mut buffer = Buffer::empty(area);
    MessageWidget::new(message)
        .with_expanded_tools(expanded)
        .render(area, &mut buffer);
    (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn codemode_render_collapses_completed_children_and_retains_attention() {
    let message = script_message();
    let collapsed = rendered(&message, &HashSet::new());
    assert!(collapsed.contains("Script"), "{collapsed}");
    assert!(!collapsed.contains("completed.json"), "{collapsed}");
    assert!(collapsed.contains("Needs attention"), "{collapsed}");
    assert!(
        visible_script_child(&message.tool_calls[3], Some(&HashSet::new())),
        "approval remains visible"
    );
    let expanded = rendered(
        &message,
        &HashSet::from(["script".into(), "script/0".into()]),
    );
    assert!(expanded.contains("completed.json"), "{expanded}");
    assert!(expanded.contains("12 ms"), "{expanded}");
}
