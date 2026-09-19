use maestro_presentation::components::{
    composer::Composer,
    tool_result::{ToolPhase, ToolResult},
};
use maestro_ui::{UiTheme, textarea::TextArea};
use ratatui::{buffer::Buffer, layout::Rect, text::Line, widgets::Widget};

fn text(buf: &Buffer) -> String {
    (buf.area.y..buf.area.bottom())
        .map(|y| {
            (buf.area.x..buf.area.right())
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn composer_reserves_editor_space_after_queued_content_and_resize() {
    let mut editor = TextArea::new();
    editor.set_text("Ship 世界");
    editor.set_cursor(editor.text().len());
    let queued = vec![Line::from("Follow-up"); 10];
    for area in [Rect::new(2, 3, 30, 5), Rect::new(2, 3, 12, 3)] {
        let view = Composer {
            editor: &editor,
            queued: &queued,
            busy: true,
            footer: Some("Gemini · normal"),
            completion: None,
            theme: UiTheme::default(),
        };
        let cursor = view.cursor_pos(area).expect("editor stays visible");
        assert!(cursor.0 < area.right() && cursor.1 < area.bottom() - 1);
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 12));
        view.render(area, &mut buf);
        if area.width == 30 {
            assert!(text(&buf).contains("Ship"));
        }
        assert!(text(&buf).contains("界"));
        assert_eq!(buf[(0, 0)].symbol(), " ");
    }
}

#[test]
fn tool_output_text_cannot_override_failure_and_clipped_content_is_disclosed() {
    let output = (1..=8)
        .map(|n| format!("Success line {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    let view = ToolResult {
        phase: ToolPhase::Failed,
        summary: "Run checks",
        arguments: "",
        output: &output,
        expanded: false,
        detail: "bash #private",
        truncation: Some("Output limited by the caller"),
        theme: UiTheme::default(),
    };
    let area = Rect::new(0, 0, 60, view.height(60));
    let mut buf = Buffer::empty(area);
    view.render(area, &mut buf);
    let rendered = text(&buf);
    assert!(rendered.contains("Failed · Run checks"));
    assert!(rendered.contains("Success line 5"));
    assert!(!rendered.contains("Success line 6"));
    assert!(rendered.contains("+3 lines"));
    assert!(rendered.contains("Output limited by the caller"));
    assert!(!rendered.contains("private"));
    assert_eq!(area.height, 8);
}

#[test]
fn expanded_results_preserve_blank_lines_and_show_execution_identity() {
    let view = ToolResult {
        phase: ToolPhase::Completed,
        summary: "Read README.md",
        arguments: "README.md",
        output: "first\n\nlast",
        expanded: true,
        detail: "read #read-1",
        truncation: None,
        theme: UiTheme::default(),
    };
    assert_eq!(view.height(60), 5);
    let mut buf = Buffer::empty(Rect::new(0, 0, 60, 5));
    view.render(buf.area, &mut buf);
    let rendered = text(&buf);
    assert!(rendered.contains("read #read-1"));
    assert!(rendered.contains("last"));
    assert_eq!(rendered.matches("README.md").count(), 1);
}

#[test]
fn narrow_composer_does_not_paint_a_wide_glyph_outside_its_editor() {
    let mut editor = TextArea::new();
    editor.set_text("界");
    let mut buf = Buffer::empty(Rect::new(0, 0, 12, 5));
    buf[(5, 1)].set_symbol("x");
    Composer {
        editor: &editor,
        queued: &[],
        busy: false,
        footer: None,
        completion: None,
        theme: UiTheme::default(),
    }
    .render(Rect::new(0, 0, 5, 3), &mut buf);
    assert_eq!(buf[(5, 1)].symbol(), "x");
    assert_eq!(
        buf[(3, 1)].symbol(),
        " ",
        "wide glyph cannot cross the editor's right inset"
    );
}

#[test]
fn empty_editor_suggestion_preserves_prompt_spacing() {
    let editor = TextArea::new();
    let mut buf = Buffer::empty(Rect::new(0, 0, 40, 4));
    Composer {
        editor: &editor,
        queued: &[],
        busy: false,
        footer: None,
        completion: Some("Summarize the changes"),
        theme: UiTheme::default(),
    }
    .render(buf.area, &mut buf);
    assert!(text(&buf).contains("> Summarize the changes"));
}

#[test]
fn composer_places_context_below_the_editor_frame() {
    let mut editor = TextArea::new();
    editor.set_text("Review the changes");
    for busy in [false, true] {
        let area = Rect::new(2, 1, 60, 4);
        let view = Composer {
            editor: &editor,
            queued: &[],
            busy,
            footer: Some("Mode: Plan · Claude"),
            completion: None,
            theme: UiTheme::default(),
        };
        assert_eq!(view.editor_area(area), Rect::new(5, 2, 56, 1));
        let mut buf = Buffer::empty(Rect::new(0, 0, 64, 6));
        view.render(area, &mut buf);
        assert_eq!(buf[(2, 1)].symbol(), "╭");
        assert_eq!(buf[(61, 1)].symbol(), "╮");
        assert_eq!(buf[(2, 3)].symbol(), "╰");
        assert_eq!(buf[(61, 3)].symbol(), "╯");
        let top: String = (2..62).map(|x| buf[(x, 1)].symbol()).collect();
        assert!(!top.contains("Mode:"));
        let footer: String = (2..62).map(|x| buf[(x, 4)].symbol()).collect();
        assert!(footer.contains("Mode: Plan · Claude"));
        assert!(text(&buf).contains("> Review the changes"));
        assert_eq!(buf[(3, 2)].fg, UiTheme::default().focus);
    }
}

#[test]
fn composer_context_and_unicode_stay_inside_tiny_frames() {
    let editor = TextArea::new();
    for width in 0..24 {
        for height in 0..5 {
            let area = Rect::new(2, 2, width, height);
            let mut buf = Buffer::empty(Rect::new(0, 0, 28, 9));
            buf[(area.right(), 2)].set_symbol("x");
            let view = Composer {
                editor: &editor,
                queued: &[],
                busy: false,
                footer: Some("Mode: Plan · 世界"),
                completion: None,
                theme: UiTheme::default(),
            };
            if let Some((x, y)) = view.cursor_pos(area) {
                assert!(x > area.x && x < area.right() - 1);
                assert!(y > area.y && y < area.bottom() - 1);
            }
            view.render(area, &mut buf);
            assert_eq!(buf[(area.right(), 2)].symbol(), "x");
            if width >= 2 && height >= 2 {
                assert_eq!(buf[(area.right() - 1, 2)].symbol(), "╮");
                let frame_bottom = area.bottom() - if height >= 4 { 2 } else { 1 };
                assert_eq!(buf[(2, frame_bottom)].symbol(), "╰");
            }
        }
    }
}

#[test]
fn completed_results_disclose_details_without_hiding_truncation() {
    let mut view = ToolResult {
        phase: ToolPhase::Completed,
        summary: "Read README.md",
        arguments: "README.md",
        output: "first\nlast",
        expanded: false,
        detail: "read #read-1",
        truncation: None,
        theme: UiTheme::default(),
    };
    assert_eq!(view.height(60), 1);
    view.expanded = true;
    assert!(
        view.lines(60)
            .iter()
            .any(|line| line.to_string().contains("last"))
    );
    view.expanded = false;
    view.truncation = Some("Output limited by caller");
    assert!(
        view.lines(60)
            .iter()
            .any(|line| line.to_string().contains("Output limited"))
    );
}

#[test]
fn footer_and_wrapped_cursor_use_the_same_viewport() {
    let mut editor = TextArea::new();
    editor.set_text("one two three four five six seven");
    editor.set_cursor(editor.text().len());
    let view = Composer {
        editor: &editor,
        queued: &[],
        busy: false,
        footer: Some("Mode: Act"),
        completion: None,
        theme: UiTheme::default(),
    };
    let area = Rect::new(0, 0, 16, 4);
    assert_eq!(view.editor_area(area).height, 1);
    assert_eq!(view.cursor_pos(area).unwrap().1, 1);
    let mut buf = Buffer::empty(area);
    view.render(area, &mut buf);
    assert!(text(&buf).contains("seven"));
    assert!(text(&buf).contains("Mode: Act"));
}
