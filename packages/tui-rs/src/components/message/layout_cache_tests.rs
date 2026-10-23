use super::*;

fn transcript_layout_message(id: &str, content: &str) -> Message {
    Message {
        id: id.to_string(),
        role: MessageRole::Assistant,
        kind: MessageKind::Regular,
        content: content.to_string(),
        thinking: String::new(),
        streaming: false,
        tool_calls: vec![],
        usage: None,
        timestamp: SystemTime::UNIX_EPOCH,
        thinking_expanded: false,
    }
}

#[test]
fn transcript_layout_reuses_stable_heights_and_remeasures_streaming_tail() {
    let mut state = crate::state::AppState::default();
    state.messages = vec![
        transcript_layout_message("one", "# First\n\nA stable message."),
        transcript_layout_message("two", "# Second\n\nA streaming message."),
    ];
    let area = Rect::new(0, 0, 80, 24);

    ChatView::new(&state).render(area, &mut Buffer::empty(area));
    let cold_measurements = state.transcript_layout_measurements();
    assert_eq!(cold_measurements, 2);

    ChatView::new(&state).render(area, &mut Buffer::empty(area));
    assert_eq!(state.transcript_layout_measurements(), cold_measurements);

    state.messages[1].content.push_str(" More streamed text.");
    ChatView::new(&state).render(area, &mut Buffer::empty(area));
    assert_eq!(
        state.transcript_layout_measurements(),
        cold_measurements + 1
    );
}

#[test]
fn transcript_layout_width_change_remeasures_every_message() {
    let mut state = crate::state::AppState::default();
    state.messages = vec![
        transcript_layout_message("one", "A stable message."),
        transcript_layout_message("two", "Another stable message."),
    ];

    let wide = Rect::new(0, 0, 80, 24);
    ChatView::new(&state).render(wide, &mut Buffer::empty(wide));
    let cold_measurements = state.transcript_layout_measurements();

    let narrow = Rect::new(0, 0, 60, 24);
    ChatView::new(&state).render(narrow, &mut Buffer::empty(narrow));
    assert_eq!(
        state.transcript_layout_measurements(),
        cold_measurements + 2
    );
}
