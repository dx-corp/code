use crossterm::event::KeyCode;
use maestro_ui::{ActionPicker, PickerOutcome};

#[test]
fn navigation_returns_the_typed_item_only_once_and_cancel_never_selects() {
    let mut picker = ActionPicker::new(vec![10, 20]);
    picker.open();
    assert_eq!(
        picker.handle_key(KeyCode::Down, false),
        PickerOutcome::Changed(Some(20))
    );
    assert_eq!(
        picker.handle_key(KeyCode::Enter, false),
        PickerOutcome::Selected(20)
    );
    assert_eq!(
        picker.handle_key(KeyCode::Enter, false),
        PickerOutcome::Pending
    );
    picker.open();
    assert_eq!(
        picker.handle_key(KeyCode::Esc, false),
        PickerOutcome::Cancelled
    );
    assert_eq!(
        picker.handle_key(KeyCode::Enter, false),
        PickerOutcome::Pending
    );
}

#[test]
fn search_paste_and_cursor_editing_keep_unicode_boundaries_and_empty_results_safe() {
    let mut picker = ActionPicker::new(vec!["日本", "dark"]).searchable(|s| s);
    picker.open();
    picker.insert_str("日本");
    picker.handle_key(KeyCode::Left, false);
    picker.handle_key(KeyCode::Backspace, false);
    assert_eq!(picker.query(), "本");
    assert_eq!(picker.selected(), Some(&"日本"));
    picker.handle_key(KeyCode::Char('x'), true);
    assert_eq!(picker.query(), "本");
    picker.insert_str("missing");
    assert_eq!(picker.selected(), None);
    assert_eq!(
        picker.handle_key(KeyCode::Enter, false),
        PickerOutcome::Cancelled
    );
    picker.open();
    assert_eq!(picker.query(), "");
    assert_eq!(picker.selected(), Some(&"日本"));
}

#[test]
fn rendering_scrolls_with_navigation_and_clips_to_the_given_area() {
    use maestro_ui::{PickerHelp, PickerOptions, UiTheme};
    use ratatui::{
        Terminal,
        backend::TestBackend,
        layout::Rect,
        style::Color,
        widgets::{ListItem, Paragraph},
    };
    let theme = UiTheme {
        panel: None,
        selection: None,
        surface: Color::Black,
        text: Color::White,
        muted: Color::Gray,
        border: Color::Blue,
        focus: Color::Magenta,
        success: Color::Green,
        attention: Color::Yellow,
        error: Color::Red,
    };
    let mut picker = ActionPicker::new((0..20).collect::<Vec<_>>());
    picker.open();
    for _ in 0..30 {
        picker.handle_key(KeyCode::Down, false);
    }
    let mut terminal = Terminal::new(TestBackend::new(40, 9)).unwrap();
    terminal
        .draw(|frame| {
            frame.render_widget(Paragraph::new("outside"), Rect::new(0, 0, 40, 1));
            picker.render(
                frame,
                Rect::new(2, 2, 35, 6),
                theme,
                PickerOptions {
                    position_when_clipped: true,
                    help: PickerHelp {
                        navigation: "choose",
                        confirm: "save",
                        key_separator: " ",
                    },
                    ..PickerOptions::default()
                },
                |value| ListItem::new(format!("item {value}")),
            );
        })
        .unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(text.contains("outside"));
    assert!(text.contains("› item 19"));
    assert!(text.contains("Enter save"));
    assert!(text.contains("↑↓ 20/20"));
    assert_eq!(
        picker.handle_key(KeyCode::Enter, false),
        PickerOutcome::Selected(19)
    );
}

#[test]
fn long_unicode_search_keeps_the_edited_suffix_and_cursor_visible() {
    use maestro_ui::{PickerOptions, UiTheme};
    use ratatui::{Terminal, backend::TestBackend, style::Color, widgets::ListItem};
    let theme = UiTheme {
        panel: None,
        selection: None,
        surface: Color::Black,
        text: Color::White,
        muted: Color::Gray,
        border: Color::Blue,
        focus: Color::Magenta,
        success: Color::Green,
        attention: Color::Yellow,
        error: Color::Red,
    };
    let mut picker = ActionPicker::new(vec!["example"]).searchable(|s| s);
    picker.open();
    picker.insert_str("日本日本日本日本TAIL");
    let mut terminal = Terminal::new(TestBackend::new(16, 8)).unwrap();
    terminal
        .draw(|frame| {
            picker.render(frame, frame.area(), theme, PickerOptions::default(), |s| {
                ListItem::new(*s)
            })
        })
        .unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(
        text.contains("TAIL"),
        "the edited end of the query must be visible"
    );
    let end = terminal.get_cursor_position().unwrap();
    assert_eq!(end.y, 0);
    // Use a query that fits to distinguish caret movement from viewport scrolling.
    picker.open();
    picker.insert_str("日本TAIL");
    terminal
        .draw(|frame| {
            picker.render(frame, frame.area(), theme, PickerOptions::default(), |s| {
                ListItem::new(*s)
            })
        })
        .unwrap();
    let short_end = terminal.get_cursor_position().unwrap();
    picker.handle_key(KeyCode::Left, false);
    picker.handle_key(KeyCode::Left, false);
    terminal
        .draw(|frame| {
            picker.render(frame, frame.area(), theme, PickerOptions::default(), |s| {
                ListItem::new(*s)
            })
        })
        .unwrap();
    assert_eq!(terminal.get_cursor_position().unwrap().x, short_end.x - 2);
}

#[test]
fn identity_selection_and_atomic_replacement_preserve_current_choice() {
    let mut picker = ActionPicker::new(vec![("a", 1), ("b", 2)])
        .identified_by(|row| row.0)
        .unwrap()
        .searchable(|row| row.0);
    picker.open();
    assert!(picker.select_id("b"));
    assert_eq!(
        picker.replace_items(vec![("b", 3), ("a", 4)]).unwrap(),
        PickerOutcome::Changed(Some(("b", 3)))
    );
    assert!(picker.replace_items(vec![("b", 5), ("b", 6)]).is_err());
    assert_eq!(picker.selected(), Some(&("b", 3)));
    assert_eq!(
        picker.insert_str("a"),
        PickerOutcome::Changed(Some(("a", 4)))
    );
    assert!(!picker.select_id("b"));
    assert_eq!(
        picker.replace_items(vec![("b", 7)]).unwrap(),
        PickerOutcome::Changed(None)
    );
    assert_eq!(picker.query(), "a");
    assert!(picker.is_open());
    assert!(
        ActionPicker::new(vec!["a", "a"])
            .identified_by(|s| s)
            .is_err()
    );
}

#[test]
fn changed_reports_item_changes_only_and_custom_matching_receives_raw_query() {
    let mut picker =
        ActionPicker::new(vec!["Alpha", "Beta"]).matching(|item, query| item.starts_with(query));
    picker.open();
    assert_eq!(
        picker.handle_key(KeyCode::Up, false),
        PickerOutcome::Pending
    );
    assert_eq!(
        picker.handle_key(KeyCode::Down, false),
        PickerOutcome::Changed(Some("Beta"))
    );
    assert_eq!(
        picker.handle_key(KeyCode::Down, false),
        PickerOutcome::Pending
    );
    assert_eq!(
        picker.insert_str("A"),
        PickerOutcome::Changed(Some("Alpha"))
    );
    assert_eq!(
        picker.handle_key(KeyCode::Left, false),
        PickerOutcome::Pending
    );
    assert_eq!(picker.insert_str("a"), PickerOutcome::Changed(None));
    assert_eq!(
        picker.handle_key(KeyCode::Backspace, false),
        PickerOutcome::Changed(Some("Alpha"))
    );
    assert_eq!(picker.insert_str(""), PickerOutcome::Pending);
}

#[test]
fn unavailable_status_cannot_confirm_stale_items_and_escape_still_closes() {
    use maestro_ui::PickerStatus;
    let mut picker = ActionPicker::new(vec!["stale"]);
    picker.open();
    for status in [
        PickerStatus::Loading("Loading models".into()),
        PickerStatus::Error("Unavailable".into()),
    ] {
        picker.set_status(status);
        assert_eq!(picker.selected(), None);
        assert_eq!(
            picker.handle_key(KeyCode::Enter, false),
            PickerOutcome::Pending
        );
        assert!(picker.is_open());
    }
    picker.set_status(PickerStatus::Ready);
    assert_eq!(picker.selected(), Some(&"stale"));
    picker.set_status(PickerStatus::Loading("Loading".into()));
    assert_eq!(
        picker.handle_key(KeyCode::Esc, false),
        PickerOutcome::Cancelled
    );
}

#[test]
fn loading_and_error_replace_rendered_rows_and_preserve_host_help() {
    use maestro_ui::{PickerOptions, PickerStatus, UiTheme};
    use ratatui::{Terminal, backend::TestBackend, style::Color, widgets::ListItem};
    let theme = UiTheme {
        panel: None,
        selection: None,
        surface: Color::Black,
        text: Color::White,
        muted: Color::Gray,
        border: Color::Blue,
        focus: Color::Magenta,
        success: Color::Green,
        attention: Color::Yellow,
        error: Color::Red,
    };
    for searchable in [false, true] {
        let mut picker = ActionPicker::new(vec!["hidden stale row"]);
        if searchable {
            picker = picker.searchable(|s| s);
        }
        picker.open();
        for status in [
            PickerStatus::Loading("Loading catalog".into()),
            PickerStatus::Error("Catalog unavailable".into()),
        ] {
            let expected = match &status {
                PickerStatus::Loading(s) | PickerStatus::Error(s) => s.clone(),
                PickerStatus::Ready => unreachable!(),
            };
            picker.set_status(status);
            let mut terminal = Terminal::new(TestBackend::new(100, 8)).unwrap();
            terminal
                .draw(|frame| {
                    picker.render(
                        frame,
                        frame.area(),
                        theme,
                        PickerOptions {
                            help_text: Some("Tab: provider"),
                            ..PickerOptions::default()
                        },
                        |s| ListItem::new(*s),
                    )
                })
                .unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert!(text.contains(&expected));
            assert!(text.contains("Tab: provider"));
            assert!(!text.contains("Enter: select"));
            assert!(!text.contains("navigate"));
            assert!(!text.contains("hidden stale row"));
        }
    }
}

#[test]
fn filtering_keeps_current_item_when_its_visible_position_changes() {
    let mut picker = ActionPicker::new(vec!["a", "b", "c"])
        .identified_by(|item| item)
        .unwrap()
        .matching(|item, query| query.is_empty() || *item != "a");
    picker.open();
    assert!(picker.select_id("b"));
    assert_eq!(picker.insert_str("narrow"), PickerOutcome::Pending);
    assert_eq!(picker.selected(), Some(&"b"));
    picker.open();
    assert_eq!(picker.selected(), Some(&"a"));
}
