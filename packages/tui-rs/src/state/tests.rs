use super::*;

// ============================================================
// Message and MessageRole Tests
// ============================================================

#[test]
fn test_message_role_equality() {
    assert_eq!(MessageRole::User, MessageRole::User);
    assert_eq!(MessageRole::Assistant, MessageRole::Assistant);
    assert_ne!(MessageRole::User, MessageRole::Assistant);
}

#[test]
fn test_message_role_copy() {
    let role = MessageRole::User;
    let copied = role; // Copy, not move
    assert_eq!(role, copied);
}

#[test]
fn test_tool_call_status_transitions() {
    assert_eq!(ToolCallStatus::Pending, ToolCallStatus::Pending);
    assert_ne!(ToolCallStatus::Pending, ToolCallStatus::Running);
    assert_ne!(ToolCallStatus::Running, ToolCallStatus::Completed);
    assert_ne!(ToolCallStatus::Completed, ToolCallStatus::Failed);
}

// ============================================================
// ApprovalMode Tests
// ============================================================

#[test]
fn test_approval_mode_default() {
    assert_eq!(ApprovalMode::default(), ApprovalMode::Selective);
}

#[test]
fn test_approval_mode_parse() {
    // Yolo mode aliases
    assert_eq!(ApprovalMode::parse("yolo"), Some(ApprovalMode::Yolo));
    assert_eq!(ApprovalMode::parse("auto"), Some(ApprovalMode::Selective));
    assert_eq!(ApprovalMode::parse("trust"), Some(ApprovalMode::Yolo));
    assert_eq!(ApprovalMode::parse("YOLO"), Some(ApprovalMode::Yolo)); // case insensitive

    // Selective mode aliases
    assert_eq!(
        ApprovalMode::parse("selective"),
        Some(ApprovalMode::Selective)
    );
    assert_eq!(
        ApprovalMode::parse("default"),
        Some(ApprovalMode::Selective)
    );
    assert_eq!(ApprovalMode::parse("normal"), Some(ApprovalMode::Selective));

    // Safe mode aliases
    assert_eq!(ApprovalMode::parse("safe"), Some(ApprovalMode::Safe));
    assert_eq!(ApprovalMode::parse("always"), Some(ApprovalMode::Safe));
    assert_eq!(
        ApprovalMode::parse("always-approve"),
        Some(ApprovalMode::Yolo)
    );
    assert_eq!(ApprovalMode::parse("ask"), Some(ApprovalMode::Safe));
    assert_eq!(ApprovalMode::parse("paranoid"), Some(ApprovalMode::Safe));

    // Invalid
    assert_eq!(ApprovalMode::parse("invalid"), None);
    assert_eq!(ApprovalMode::parse(""), None);
}

#[test]
fn test_approval_mode_next_cycle() {
    assert_eq!(ApprovalMode::Yolo.next(), ApprovalMode::Selective);
    assert_eq!(ApprovalMode::Selective.next(), ApprovalMode::Safe);
    assert_eq!(ApprovalMode::Safe.next(), ApprovalMode::Yolo);
}

#[test]
fn test_approval_mode_label() {
    assert!(ApprovalMode::Yolo.label().contains("YOLO"));
    assert!(ApprovalMode::Selective.label().contains("Selective"));
    assert!(ApprovalMode::Safe.label().contains("Safe"));
}

#[test]
fn test_queue_mode_parse() {
    assert_eq!(QueueMode::parse("all"), Some(QueueMode::All));
    assert_eq!(QueueMode::parse("one"), Some(QueueMode::One));
    assert_eq!(QueueMode::parse("single"), Some(QueueMode::One));
    assert_eq!(QueueMode::parse("unknown"), None);
}

// ============================================================
// AppState Creation Tests
// ============================================================

#[test]
fn test_app_state_new() {
    let state = AppState::new();
    assert!(state.messages.is_empty());
    assert!(state.model.is_none());
    assert!(state.provider.is_none());
    assert!(state.cwd.is_none());
    assert!(state.git_branch.is_none());
    assert!(state.session_id.is_none());
    assert!(!state.busy);
    assert!(state.busy_since.is_none());
    assert!(state.status.is_none());
    assert_eq!(state.scroll_offset, 0);
    assert!(state.expanded_tool_calls.is_empty());
    assert!(state.error.is_none());
    assert!(state.thinking_header.is_none());
    assert!(!state.zen_mode);
    assert_eq!(state.approval_mode, ApprovalMode::Selective);
    assert_eq!(state.steering_mode, QueueMode::All);
    assert_eq!(state.follow_up_mode, QueueMode::All);
}

#[test]
fn test_app_state_default() {
    let state = AppState::default();
    assert!(state.messages.is_empty());
    assert!(
        state.compact_tool_outputs,
        "new sessions should show short tool previews"
    );
}

// ============================================================
// Text Input Tests (UTF-8 handling)
// ============================================================

#[test]
fn test_insert_char_ascii() {
    let mut state = AppState::new();
    state.insert_char('h');
    state.insert_char('i');
    assert_eq!(state.input(), "hi");
    assert_eq!(state.cursor(), 2);
}

#[test]
fn test_insert_char_unicode() {
    let mut state = AppState::new();
    state.insert_char('こ');
    state.insert_char('ん');
    assert_eq!(state.input(), "こん");
    // Each Japanese char is 3 bytes in UTF-8
    assert_eq!(state.cursor(), 6);
}

#[test]
fn test_insert_char_emoji() {
    let mut state = AppState::new();
    state.insert_char('🎉');
    assert_eq!(state.input(), "🎉");
    // Emoji is 4 bytes in UTF-8
    assert_eq!(state.cursor(), 4);
}

#[test]
fn test_insert_str() {
    let mut state = AppState::new();
    state.insert_str("hello");
    assert_eq!(state.input(), "hello");
    assert_eq!(state.cursor(), 5);
}

#[test]
fn test_insert_str_unicode() {
    let mut state = AppState::new();
    state.insert_str("日本語");
    assert_eq!(state.input(), "日本語");
    assert_eq!(state.cursor(), 9); // 3 chars * 3 bytes
}

#[test]
fn test_backspace_ascii() {
    let mut state = AppState::new();
    state.set_input("hello");
    state.backspace();
    assert_eq!(state.input(), "hell");
    assert_eq!(state.cursor(), 4);
}

#[test]
fn test_backspace_unicode() {
    let mut state = AppState::new();
    state.set_input("日本語");
    state.backspace();
    assert_eq!(state.input(), "日本");
    assert_eq!(state.cursor(), 6); // 2 remaining chars * 3 bytes
}

#[test]
fn test_backspace_emoji() {
    let mut state = AppState::new();
    state.set_input("hi🎉");
    state.backspace();
    assert_eq!(state.input(), "hi");
    assert_eq!(state.cursor(), 2);
}

#[test]
fn test_backspace_at_start() {
    let mut state = AppState::new();
    state.set_input("hi");
    state.move_home();
    state.backspace(); // Should do nothing
    assert_eq!(state.input(), "hi");
    assert_eq!(state.cursor(), 0);
}

#[test]
fn test_delete_ascii() {
    let mut state = AppState::new();
    state.set_input("hello");
    state.move_home();
    state.delete();
    assert_eq!(state.input(), "ello");
}

#[test]
fn test_delete_at_end() {
    let mut state = AppState::new();
    state.set_input("hi");
    state.delete(); // Cursor at end, should do nothing
    assert_eq!(state.input(), "hi");
}

#[test]
fn test_move_left_ascii() {
    let mut state = AppState::new();
    state.set_input("abc");
    state.move_left();
    assert_eq!(state.cursor(), 2);
    state.move_left();
    assert_eq!(state.cursor(), 1);
}

#[test]
fn test_move_left_unicode() {
    let mut state = AppState::new();
    state.set_input("日本");
    // Cursor at end (6 bytes)
    state.move_left();
    assert_eq!(state.cursor(), 3); // After first char
    state.move_left();
    assert_eq!(state.cursor(), 0); // At start
}

#[test]
fn test_move_left_at_start() {
    let mut state = AppState::new();
    state.set_input("hi");
    state.move_home();
    state.move_left(); // Should stay at 0
    assert_eq!(state.cursor(), 0);
}

#[test]
fn test_move_right_ascii() {
    let mut state = AppState::new();
    state.set_input("abc");
    state.move_home();
    state.move_right();
    assert_eq!(state.cursor(), 1);
    state.move_right();
    assert_eq!(state.cursor(), 2);
}

#[test]
fn test_move_right_unicode() {
    let mut state = AppState::new();
    state.set_input("日本");
    state.move_home();
    state.move_right();
    assert_eq!(state.cursor(), 3); // Past first 3-byte char
    state.move_right();
    assert_eq!(state.cursor(), 6); // At end
}

#[test]
fn test_move_right_at_end() {
    let mut state = AppState::new();
    state.set_input("hi");
    state.move_right(); // Should stay at end
    assert_eq!(state.cursor(), 2);
}

#[test]
fn test_move_home_end() {
    let mut state = AppState::new();
    state.set_input("hello world");
    assert_eq!(state.cursor(), 11); // At end after set_input
    state.move_home();
    assert_eq!(state.cursor(), 0);
    state.move_end();
    assert_eq!(state.cursor(), 11);
}

#[test]
fn test_move_home_smart_toggle() {
    let mut state = AppState::new();
    state.set_input("  hello");
    state.move_end();
    state.move_home_smart();
    assert_eq!(state.cursor(), 2);
    state.move_home_smart();
    assert_eq!(state.cursor(), 0);
}

#[test]
fn test_move_word_left_across_lines() {
    let mut state = AppState::new();
    state.set_input("hello\nworld");
    state.textarea.set_cursor(6); // start of second line
    state.move_word_left();
    assert_eq!(state.cursor(), 5); // end of "hello"
}

#[test]
fn test_move_word_right_across_lines() {
    let mut state = AppState::new();
    state.set_input("hello\nworld");
    state.textarea.set_cursor(5); // end of first line
    state.move_word_right();
    assert_eq!(state.cursor(), 11); // end of "world"
}

#[test]
fn test_move_word_left_unicode() {
    let mut state = AppState::new();
    state.set_input("hi 🙂 there");
    state.move_word_left();
    let text = state.input().to_string();
    assert_eq!(&text[state.cursor()..], "there");
    state.move_word_left();
    assert_eq!(state.cursor(), text.find('🙂').unwrap());
}

#[test]
fn test_delete_word_backward() {
    let mut state = AppState::new();
    state.set_input("hello world");
    state.delete_word_backward();
    assert_eq!(state.input(), "hello ");
    assert_eq!(state.cursor(), 6);
}

#[test]
fn test_delete_to_start_of_line_merges() {
    let mut state = AppState::new();
    state.set_input("hello\nworld");
    state.textarea.set_cursor(6); // start of second line
    state.delete_to_start_of_line();
    assert_eq!(state.input(), "helloworld");
    assert_eq!(state.cursor(), 5);
}

#[test]
fn test_yank_kill_ring() {
    let mut state = AppState::new();
    state.set_input("hello world");
    state.delete_word_backward();
    state.yank_kill_ring();
    assert_eq!(state.input(), "hello world");
    assert_eq!(state.cursor(), 11);
}

#[test]
fn test_kill_ring_appends_consecutive_kills() {
    let mut state = AppState::new();
    state.set_input("hello world test");
    // Kill "test"
    state.delete_word_backward();
    // Consecutive kill should append (prepend for backward kill)
    state.delete_word_backward();
    state.yank_kill_ring();
    assert_eq!(state.input(), "hello world test");
}

#[test]
fn test_move_up_down_wrapped_lines() {
    let mut state = AppState::new();
    state.set_input_width(5);
    state.set_input("0123456789");
    assert_eq!(state.cursor(), 10);
    state.move_up();
    assert_eq!(state.cursor(), 5);
    state.move_down();
    assert_eq!(state.cursor(), 10);
}

#[test]
fn test_take_input() {
    let mut state = AppState::new();
    state.set_input("hello");
    let taken = state.take_input();
    assert_eq!(taken, "hello");
    assert_eq!(state.input(), "");
    assert_eq!(state.cursor(), 0);
}

#[test]
fn test_set_input_moves_cursor_to_end() {
    let mut state = AppState::new();
    state.set_input("hello");
    assert_eq!(state.cursor(), 5);
}

// ============================================================
// Mixed UTF-8 Text Editing Tests
// ============================================================

#[test]
fn test_insert_in_middle_of_unicode() {
    let mut state = AppState::new();
    state.set_input("日語"); // 6 bytes total
    state.move_home();
    state.move_right(); // After first char (position 3)
    state.insert_char('本'); // Insert in middle
    assert_eq!(state.input(), "日本語");
}

#[test]
fn test_backspace_mixed_content() {
    let mut state = AppState::new();
    state.set_input("a日b本c");
    state.backspace(); // Delete 'c'
    assert_eq!(state.input(), "a日b本");
    state.backspace(); // Delete '本'
    assert_eq!(state.input(), "a日b");
    state.backspace(); // Delete 'b'
    assert_eq!(state.input(), "a日");
}

#[test]
fn test_cursor_movement_through_mixed_content() {
    let mut state = AppState::new();
    state.set_input("a日b"); // 'a' = 1 byte, '日' = 3 bytes, 'b' = 1 byte = 5 bytes total
    state.move_home();
    assert_eq!(state.cursor(), 0);
    state.move_right(); // Past 'a'
    assert_eq!(state.cursor(), 1);
    state.move_right(); // Past '日'
    assert_eq!(state.cursor(), 4);
    state.move_right(); // Past 'b'
    assert_eq!(state.cursor(), 5);
}

// ============================================================
// Message Management Tests
// ============================================================

#[test]
fn test_add_user_message() {
    let mut state = AppState::new();
    let id = state.add_user_message("Hello!".to_string());

    assert!(!id.is_empty());
    assert_eq!(state.messages.len(), 1);
    assert_eq!(state.messages[0].content, "Hello!");
    assert_eq!(state.messages[0].role, MessageRole::User);
    assert!(!state.messages[0].streaming);
    assert!(state.busy);
    assert!(state.busy_since.is_some());
}

#[test]
fn test_add_system_message() {
    let mut state = AppState::new();
    state.add_system_message("System info".to_string());

    assert_eq!(state.messages.len(), 1);
    assert_eq!(state.messages[0].content, "System info");
    assert_eq!(state.messages[0].role, MessageRole::Assistant);
    assert_eq!(state.messages[0].kind, MessageKind::System);
    assert!(!state.messages[0].streaming);
}

#[test]
fn test_toggle_thinking() {
    let mut state = AppState::new();
    let id = state.add_user_message("test".to_string());

    // Initially not expanded
    let msg = state.messages.iter().find(|m| m.id == id).unwrap();
    assert!(!msg.thinking_expanded);

    // Toggle on
    state.toggle_thinking(&id);
    let msg = state.messages.iter().find(|m| m.id == id).unwrap();
    assert!(msg.thinking_expanded);

    // Toggle off
    state.toggle_thinking(&id);
    let msg = state.messages.iter().find(|m| m.id == id).unwrap();
    assert!(!msg.thinking_expanded);
}

#[test]
fn test_toggle_thinking_nonexistent_id() {
    let mut state = AppState::new();
    state.add_user_message("test".to_string());

    // Should not panic
    state.toggle_thinking("nonexistent-id");
}

// ============================================================
// Scroll Tests
// ============================================================

#[test]
fn test_scroll_up() {
    let mut state = AppState::new();
    state.scroll_offset = 10;
    state.scroll_up(3);
    assert_eq!(state.scroll_offset, 7);
}

#[test]
fn test_scroll_up_saturating() {
    let mut state = AppState::new();
    state.scroll_offset = 2;
    state.scroll_up(10); // Should not go below 0
    assert_eq!(state.scroll_offset, 0);
}

#[test]
fn test_scroll_down() {
    let mut state = AppState::new();
    state.scroll_down(5);
    assert_eq!(state.scroll_offset, 5);
}

#[test]
fn test_scroll_down_saturating() {
    let mut state = AppState::new();
    state.scroll_offset = usize::MAX - 5;
    state.scroll_down(10); // Should saturate at MAX
    assert_eq!(state.scroll_offset, usize::MAX);
}

// ============================================================
// Tool Call Expansion Tests
// ============================================================

#[test]
fn test_toggle_tool_call() {
    let mut state = AppState::new();
    state.compact_tool_outputs = false;
    let call_id = "call-123";

    // Default: expanded when compact mode is off
    assert!(state.is_tool_call_expanded(call_id));

    state.toggle_tool_call(call_id);
    assert!(!state.is_tool_call_expanded(call_id));

    state.toggle_tool_call(call_id);
    assert!(state.is_tool_call_expanded(call_id));

    // Compact mode flips default to collapsed
    state.compact_tool_outputs = true;
    state.expanded_tool_calls.clear();
    assert!(!state.is_tool_call_expanded(call_id));

    state.toggle_tool_call(call_id);
    assert!(state.is_tool_call_expanded(call_id));
}

#[test]
fn test_multiple_tool_calls_expanded() {
    let mut state = AppState::new();
    state.compact_tool_outputs = true;

    state.toggle_tool_call("call-1");
    state.toggle_tool_call("call-2");

    assert!(state.is_tool_call_expanded("call-1"));
    assert!(state.is_tool_call_expanded("call-2"));
    assert!(!state.is_tool_call_expanded("call-3"));
}

// ============================================================
// Elapsed Time Tests
// ============================================================

#[test]
fn test_elapsed_busy_secs_not_busy() {
    let state = AppState::new();
    assert_eq!(state.elapsed_busy_secs(), 0);
}

#[test]
fn test_elapsed_busy_secs_when_busy() {
    let mut state = AppState::new();
    state.busy = true;
    state.busy_since = Some(Instant::now());
    // Should be 0 or very small (just started)
    assert!(state.elapsed_busy_secs() < 2);
}

#[test]
fn test_can_queue_follow_up_shortcut_requires_busy_queueable_non_command_input() {
    let mut state = AppState::new();
    state.textarea.set_text("follow-up");
    assert!(!state.can_queue_follow_up_shortcut());

    state.busy = true;
    state.follow_up_mode = QueueMode::One;
    assert!(!state.can_queue_follow_up_shortcut());

    state.follow_up_mode = QueueMode::All;
    state.textarea.set_text("/help");
    assert!(!state.can_queue_follow_up_shortcut());

    state.textarea.set_text("!ls");
    assert!(!state.can_queue_follow_up_shortcut());

    state.textarea.set_text("   ");
    assert!(!state.can_queue_follow_up_shortcut());

    state.textarea.set_text("follow-up");
    assert!(state.can_queue_follow_up_shortcut());
}

// ============================================================
// Tool Call State Tests
// ============================================================

#[test]
fn test_fail_tool_call() {
    let mut state = AppState::new();

    // Add a message with a tool call
    state.messages.push(Message {
        id: "msg-1".to_string(),
        role: MessageRole::Assistant,
        kind: MessageKind::Regular,
        content: String::new(),
        thinking: String::new(),
        streaming: false,
        tool_calls: vec![ToolCallState {
            call_id: "call-1".to_string(),
            tool: "bash".to_string(),
            args: serde_json::json!({"command": "ls"}),
            status: ToolCallStatus::Pending,
            output: String::new(),
        }],
        usage: None,
        timestamp: SystemTime::now(),
        thinking_expanded: false,
    });

    state.fail_tool_call("call-1", "User rejected");

    let tc = &state.messages[0].tool_calls[0];
    assert_eq!(tc.status, ToolCallStatus::Failed);
    assert!(tc.output.contains("User rejected"));
}

#[test]
fn test_fail_tool_call_with_existing_output() {
    let mut state = AppState::new();

    state.messages.push(Message {
        id: "msg-1".to_string(),
        role: MessageRole::Assistant,
        kind: MessageKind::Regular,
        content: String::new(),
        thinking: String::new(),
        streaming: false,
        tool_calls: vec![ToolCallState {
            call_id: "call-1".to_string(),
            tool: "bash".to_string(),
            args: serde_json::json!({}),
            status: ToolCallStatus::Running,
            output: "partial output".to_string(),
        }],
        usage: None,
        timestamp: SystemTime::now(),
        thinking_expanded: false,
    });

    state.fail_tool_call("call-1", "Timeout");

    let tc = &state.messages[0].tool_calls[0];
    assert_eq!(tc.status, ToolCallStatus::Failed);
    assert!(tc.output.contains("partial output"));
    assert!(tc.output.contains("Timeout"));
}

#[test]
fn test_fail_tool_call_nonexistent() {
    let mut state = AppState::new();
    state.add_user_message("test".to_string());

    // Should not panic
    state.fail_tool_call("nonexistent", "error");
}

// ============================================================
// Handle Agent Message Tests
// ============================================================

#[test]
fn test_handle_ready_message() {
    let mut state = AppState::new();
    state.busy = true;

    state.handle_agent_message(FromAgent::Ready {
        model: "claude-3".to_string(),
        provider: "anthropic".to_string(),
    });

    assert_eq!(state.model, Some("claude-3".to_string()));
    assert_eq!(state.provider, Some("anthropic".to_string()));
    assert!(!state.busy);
}

#[test]
fn test_handle_response_start() {
    let mut state = AppState::new();

    state.handle_agent_message(FromAgent::ResponseStart {
        response_id: "resp-1".to_string(),
    });

    assert!(state.busy);
    assert!(state.busy_since.is_some());
    assert_eq!(state.messages.len(), 1);
    assert_eq!(state.messages[0].id, "resp-1");
    assert_eq!(state.messages[0].role, MessageRole::Assistant);
    assert!(state.messages[0].streaming);
}

#[test]
fn test_handle_response_chunk() {
    let mut state = AppState::new();

    // Start response first
    state.handle_agent_message(FromAgent::ResponseStart {
        response_id: "resp-1".to_string(),
    });

    // Add content chunk
    state.handle_agent_message(FromAgent::ResponseChunk {
        response_id: "resp-1".to_string(),
        content: "Hello ".to_string(),
        is_thinking: false,
    });

    state.handle_agent_message(FromAgent::ResponseChunk {
        response_id: "resp-1".to_string(),
        content: "world!".to_string(),
        is_thinking: false,
    });

    assert_eq!(state.messages[0].content, "Hello world!");
}

#[test]
fn test_handle_response_chunk_thinking() {
    let mut state = AppState::new();

    state.handle_agent_message(FromAgent::ResponseStart {
        response_id: "resp-1".to_string(),
    });

    state.handle_agent_message(FromAgent::ResponseChunk {
        response_id: "resp-1".to_string(),
        content: "**Analyzing**".to_string(),
        is_thinking: true,
    });

    assert_eq!(state.messages[0].thinking, "**Analyzing**");
    assert_eq!(state.thinking_header, Some("Analyzing".to_string()));
}

#[test]
fn test_handle_response_end() {
    let mut state = AppState::new();
    state.busy = true;

    state.handle_agent_message(FromAgent::ResponseStart {
        response_id: "resp-1".to_string(),
    });

    state.handle_agent_message(FromAgent::ResponseEnd {
        response_id: "resp-1".to_string(),
        usage: None,
    });

    assert!(!state.messages[0].streaming);
    assert!(state.busy);
    assert!(state.thinking_header.is_none());

    state.handle_agent_message(FromAgent::ResponseEnd {
        response_id: "done".to_string(),
        usage: None,
    });
    assert!(state.busy);

    state.handle_agent_message(FromAgent::TurnCompleted {
        response_id: "done".to_string(),
        coding_completion: None,
        coding_child_records: Vec::new(),
    });
    assert!(!state.busy);
}

#[test]
fn test_handle_tool_call() {
    let mut state = AppState::new();

    // Add an assistant message first
    state.handle_agent_message(FromAgent::ResponseStart {
        response_id: "resp-1".to_string(),
    });

    state.handle_agent_message(FromAgent::ToolCall {
        call_id: "call-1".to_string(),
        tool: "bash".to_string(),
        args: serde_json::json!({"command": "ls"}),
        requires_approval: true,
        approval_inline_env: None,
    });

    assert_eq!(state.messages[0].tool_calls.len(), 1);
    let tc = &state.messages[0].tool_calls[0];
    assert_eq!(tc.call_id, "call-1");
    assert_eq!(tc.tool, "bash");
    assert_eq!(tc.status, ToolCallStatus::Pending);
}

#[test]
fn test_handle_tool_call_no_approval() {
    let mut state = AppState::new();

    state.handle_agent_message(FromAgent::ResponseStart {
        response_id: "resp-1".to_string(),
    });

    state.handle_agent_message(FromAgent::ToolCall {
        call_id: "call-1".to_string(),
        tool: "read".to_string(),
        args: serde_json::json!({}),
        requires_approval: false,
        approval_inline_env: None,
    });

    let tc = &state.messages[0].tool_calls[0];
    assert_eq!(tc.status, ToolCallStatus::Running);
}

#[test]
fn test_handle_tool_lifecycle() {
    let mut state = AppState::new();

    state.handle_agent_message(FromAgent::ResponseStart {
        response_id: "resp-1".to_string(),
    });

    state.handle_agent_message(FromAgent::ToolCall {
        call_id: "call-1".to_string(),
        tool: "bash".to_string(),
        args: serde_json::json!({}),
        requires_approval: false,
        approval_inline_env: None,
    });

    // Tool started
    state.handle_agent_message(FromAgent::ToolStart {
        call_id: "call-1".to_string(),
    });
    assert_eq!(
        state.messages[0].tool_calls[0].status,
        ToolCallStatus::Running
    );

    // Tool output
    state.handle_agent_message(FromAgent::ToolOutput {
        call_id: "call-1".to_string(),
        content: "file1.txt\n".to_string(),
    });
    assert!(state.messages[0].tool_calls[0].output.contains("file1.txt"));

    // Tool completed
    state.handle_agent_message(FromAgent::ToolEnd {
        call_id: "call-1".to_string(),
        success: true,
        result: None,
        receipt: None,
    });
    assert_eq!(
        state.messages[0].tool_calls[0].status,
        ToolCallStatus::Completed
    );
}

#[test]
fn test_handle_tool_failure() {
    let mut state = AppState::new();

    state.handle_agent_message(FromAgent::ResponseStart {
        response_id: "resp-1".to_string(),
    });

    state.handle_agent_message(FromAgent::ToolCall {
        call_id: "call-1".to_string(),
        tool: "bash".to_string(),
        args: serde_json::json!({}),
        requires_approval: false,
        approval_inline_env: None,
    });

    state.handle_agent_message(FromAgent::ToolEnd {
        call_id: "call-1".to_string(),
        success: false,
        result: None,
        receipt: None,
    });

    assert_eq!(
        state.messages[0].tool_calls[0].status,
        ToolCallStatus::Failed
    );
}

#[test]
fn test_handle_error_respects_terminal_flag() {
    let mut state = AppState::new();
    state.busy = true;

    state.handle_agent_message(FromAgent::Error {
        message: "Connection failed".to_string(),
        fatal: false,
        terminal: false,
        retryable: false,
    });

    assert_eq!(state.error, Some("Connection failed".to_string()));
    assert!(state.busy);
    assert_eq!(state.alerts.len(), 1);
    assert_eq!(state.alerts[0], "Connection failed");
    assert_eq!(state.unseen_alerts, 1);

    state.handle_agent_message(FromAgent::Error {
        message: "Turn failed".to_string(),
        fatal: false,
        terminal: true,
        retryable: false,
    });
    assert!(!state.busy);
}

#[test]
fn test_record_alert_bounds_history() {
    let mut state = AppState::new();
    for i in 0..(crate::state::MAX_ALERT_HISTORY + 10) {
        state.record_alert(format!("alert {i}"));
    }
    assert_eq!(state.alerts.len(), crate::state::MAX_ALERT_HISTORY);
    // Oldest entries are dropped first.
    assert_eq!(state.alerts[0], "alert 10");
    assert_eq!(state.unseen_alerts, crate::state::MAX_ALERT_HISTORY + 10);

    state.mark_alerts_seen();
    assert_eq!(state.unseen_alerts, 0);
}

#[test]
fn test_handle_status() {
    let mut state = AppState::new();

    state.handle_agent_message(FromAgent::Status {
        message: "Loading...".to_string(),
    });

    assert_eq!(state.status, Some("Loading...".to_string()));
}

#[test]
fn test_handle_session_info() {
    let mut state = AppState::new();

    state.handle_agent_message(FromAgent::SessionInfo {
        session_id: Some("sess-123".to_string()),
        cwd: "/home/user".to_string(),
        git_branch: Some("main".to_string()),
    });

    assert_eq!(state.session_id, Some("sess-123".to_string()));
    assert_eq!(state.cwd, Some("/home/user".to_string()));
    assert_eq!(state.git_branch, Some("main".to_string()));
}

#[test]
fn test_handle_batch_events() {
    let mut state = AppState::new();

    state.handle_agent_message(FromAgent::BatchStart { total: 5 });
    assert!(state.status.as_ref().unwrap().contains('5'));

    state.handle_agent_message(FromAgent::BatchEnd {
        total: 5,
        successes: 4,
        failures: 1,
    });
    assert!(state.status.as_ref().unwrap().contains('4'));
    assert!(state.status.as_ref().unwrap().contains("failed"));
}

#[test]
fn test_handle_hook_blocked() {
    let mut state = AppState::new();

    state.handle_agent_message(FromAgent::ResponseStart {
        response_id: "resp-1".to_string(),
    });

    state.handle_agent_message(FromAgent::ToolCall {
        call_id: "call-1".to_string(),
        tool: "bash".to_string(),
        args: serde_json::json!({}),
        requires_approval: false,
        approval_inline_env: None,
    });

    state.handle_agent_message(FromAgent::HookBlocked {
        call_id: "call-1".to_string(),
        tool: "bash".to_string(),
        reason: "Blocked by security hook".to_string(),
    });

    assert_eq!(
        state.messages[0].tool_calls[0].status,
        ToolCallStatus::Blocked
    );
}

// ============================================================
// Extract Thinking Header Tests
// ============================================================

#[test]
fn test_extract_thinking_header_basic() {
    let text = "Some text **Header** more text";
    assert_eq!(extract_thinking_header(text), Some("Header".to_string()));
}

#[test]
fn test_extract_thinking_header_multiple() {
    let text = "**First** some text **Second**";
    // Should extract the last complete header
    assert_eq!(extract_thinking_header(text), Some("Second".to_string()));
}

#[test]
fn test_extract_thinking_header_none() {
    assert_eq!(extract_thinking_header("no header here"), None);
    assert_eq!(extract_thinking_header("**incomplete"), None);
    assert_eq!(extract_thinking_header(""), None);
}

#[test]
fn test_extract_thinking_header_empty() {
    let text = "**** more";
    // Empty header between ** ** should not be extracted
    assert_eq!(extract_thinking_header(text), None);
}

#[test]
fn test_extract_thinking_header_long() {
    let long_header = "a".repeat(150);
    let text = format!("**{}**", long_header);
    // Headers > 100 chars are rejected
    assert_eq!(extract_thinking_header(&text), None);
}

#[test]
fn test_extract_thinking_header_multiline() {
    let text = "**Header\nwith newline**";
    // Should only take first line
    assert_eq!(extract_thinking_header(text), Some("Header".to_string()));
}

// ============================================================
// Edge Cases and Boundary Tests
// ============================================================

#[test]
fn test_empty_input_operations() {
    let mut state = AppState::new();

    // Operations on empty input should not panic
    state.backspace();
    state.delete();
    state.move_left();
    state.move_right();
    state.move_home();
    state.move_end();

    assert_eq!(state.input(), "");
    assert_eq!(state.cursor(), 0);
}

#[test]
fn test_very_long_input() {
    let mut state = AppState::new();
    let long_text = "a".repeat(100_000);
    state.set_input(&long_text);

    assert_eq!(state.input().len(), 100_000);
    assert_eq!(state.cursor(), 100_000);

    state.move_home();
    assert_eq!(state.cursor(), 0);
}

#[test]
fn test_combining_characters() {
    let mut state = AppState::new();
    // é can be composed as e + combining acute accent
    state.set_input("café");
    assert_eq!(state.input(), "café");

    // Cursor movement should work
    state.move_home();
    state.move_right();
    state.move_right();
    state.move_right();
    state.move_right();
}

#[test]
fn test_tool_call_state_clone() {
    let tc = ToolCallState {
        call_id: "call-1".to_string(),
        tool: "bash".to_string(),
        args: serde_json::json!({"command": "ls"}),
        status: ToolCallStatus::Completed,
        output: "output".to_string(),
    };

    let cloned = tc.clone();
    assert_eq!(cloned.call_id, tc.call_id);
    assert_eq!(cloned.status, tc.status);
}

#[test]
fn test_message_clone() {
    let msg = Message {
        id: "msg-1".to_string(),
        role: MessageRole::User,
        kind: MessageKind::Regular,
        content: "Hello".to_string(),
        thinking: String::new(),
        streaming: false,
        tool_calls: vec![],
        usage: None,
        timestamp: SystemTime::now(),
        thinking_expanded: false,
    };

    let cloned = msg.clone();
    assert_eq!(cloned.id, msg.id);
    assert_eq!(cloned.content, msg.content);
}

#[test]
fn test_apply_compaction_replaces_older_transcript_messages() {
    let mut state = AppState::new();
    state.add_system_message("System note".to_string());
    state.add_user_message("User one".to_string());
    state.add_system_message("Another note".to_string());
    state.messages.push(Message {
        id: "assistant-1".to_string(),
        role: MessageRole::Assistant,
        kind: MessageKind::Regular,
        content: "Assistant one".to_string(),
        thinking: String::new(),
        streaming: false,
        tool_calls: Vec::new(),
        usage: None,
        timestamp: SystemTime::now(),
        thinking_expanded: false,
    });
    state.add_user_message("User two".to_string());
    state.messages.push(Message {
        id: "assistant-2".to_string(),
        role: MessageRole::Assistant,
        kind: MessageKind::Regular,
        content: "Assistant two".to_string(),
        thinking: String::new(),
        streaming: false,
        tool_calls: Vec::new(),
        usage: None,
        timestamp: SystemTime::now(),
        thinking_expanded: false,
    });

    state.apply_compaction(
        "## Conversation Summary\n\n1. **User**: User one".to_string(),
        2,
        SystemTime::UNIX_EPOCH,
    );

    assert_eq!(state.messages.len(), 3);
    assert!(state.messages[0].is_compaction_boundary());
    assert_eq!(state.messages[0].timestamp, SystemTime::UNIX_EPOCH);
    assert_eq!(state.messages[1].content, "User two");
    assert_eq!(state.messages[2].content, "Assistant two");
}

#[test]
fn interaction_mode_cycles() {
    use super::InteractionMode;
    assert_eq!(
        InteractionMode::Normal.next(),
        InteractionMode::AlwaysApprove
    );
    assert_eq!(
        InteractionMode::AlwaysApprove.next(),
        InteractionMode::Normal
    );
    assert_eq!(
        InteractionMode::AlwaysApprove.approval_mode(),
        ApprovalMode::Yolo
    );
}

// ============================================================
// Paste Folding Tests
// ============================================================

fn large_paste() -> String {
    (1..=10)
        .map(|i| format!("line{i}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn test_large_paste_folds_into_chip() {
    let mut state = AppState::new();
    let pasted = large_paste();
    state.insert_paste(&pasted);

    // Display is folded, full text is retained.
    assert_eq!(state.input(), pasted);
    assert_eq!(state.textarea.display_text(), "[Pasted: 10 lines]");
    assert_eq!(state.textarea.folded_paste_lines(), Some(10));
    // Cursor sits right after the pasted block (end of the chip).
    assert_eq!(state.cursor(), pasted.len());
}

#[test]
fn test_small_paste_does_not_fold() {
    let mut state = AppState::new();
    state.insert_paste("line1\nline2\nline3");

    assert!(state.textarea.paste_folds().is_empty());
    assert_eq!(state.textarea.display_text(), "line1\nline2\nline3");
    assert_eq!(state.textarea.folded_paste_lines(), None);
}

#[test]
fn test_large_single_line_paste_folds_by_char_count() {
    let mut state = AppState::new();
    let pasted = "x".repeat(500);
    state.insert_paste(&pasted);

    assert_eq!(state.input(), pasted);
    assert_eq!(state.textarea.display_text(), "[Pasted: 1 line]");
}

#[test]
fn test_paste_submission_is_byte_identical() {
    let mut state = AppState::new();
    state.insert_str("explain this:\n");
    let pasted = large_paste();
    state.insert_paste(&pasted);
    state.insert_str("\nthanks");

    // Typing after the paste unfolds the display...
    assert!(state.textarea.paste_folds().is_empty());
    // ...but the submitted content is exactly what was pasted/typed.
    let expected = format!("explain this:\n{pasted}\nthanks");
    assert_eq!(state.take_input(), expected);
}

#[test]
fn test_paste_normalizes_carriage_returns() {
    let mut state = AppState::new();
    state.insert_paste("a\r\nb\r\nc");
    assert_eq!(state.input(), "a\nb\nc");
}

#[test]
fn test_backspace_after_large_paste_deletes_whole_block() {
    let mut state = AppState::new();
    state.insert_str("keep ");
    let pasted = large_paste();
    state.insert_paste(&pasted);

    // Cursor is right after the folded paste: one Backspace removes it all.
    state.backspace();
    assert_eq!(state.input(), "keep ");
    assert_eq!(state.cursor(), 5);
    assert!(state.textarea.paste_folds().is_empty());
}

#[test]
fn test_backspace_after_small_paste_deletes_one_char() {
    let mut state = AppState::new();
    state.insert_paste("short");

    state.backspace();
    assert_eq!(state.input(), "shor");
}

#[test]
fn test_backspace_inside_folded_paste_unfolds_first() {
    let mut state = AppState::new();
    let pasted = large_paste();
    state.insert_paste(&pasted);
    // Move into the middle of the pasted block, then delete: the fold is
    // dropped and a single character is removed.
    state.move_home();
    for _ in 0..3 {
        state.move_right();
    }
    state.backspace();
    assert!(state.textarea.paste_folds().is_empty());
    assert_eq!(state.input().len(), pasted.len() - 1);
}

#[test]
fn test_edit_after_paste_unfolds_display() {
    let mut state = AppState::new();
    let pasted = large_paste();
    state.insert_paste(&pasted);
    assert_eq!(state.textarea.paste_folds().len(), 1);

    state.insert_char('!');
    assert!(state.textarea.paste_folds().is_empty());
    assert_eq!(state.textarea.display_text(), format!("{pasted}!"));
}
