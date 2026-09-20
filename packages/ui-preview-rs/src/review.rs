//! Portable review artifacts from production terminal buffers. No runtime startup.
use crate::Scene;
use ratatui::{
    Frame, Terminal,
    backend::TestBackend,
    buffer::Buffer,
    style::{Color, Modifier},
};
use serde::Serialize;
use unicode_width::UnicodeWidthStr;

/// One terminal cell, retaining color kind, modifiers, and display width.
#[derive(Debug, Serialize)]
pub struct Cell {
    pub text: String,
    pub foreground: String,
    pub background: String,
    pub modifiers: u16,
    pub columns: usize,
}

/// Deterministic supplied-state capture. This is a fixture, never live readiness.
#[derive(Debug, Serialize)]
pub struct Capture {
    pub scene: Scene,
    pub source: String,
    pub cells: Vec<Cell>,
}

fn color(value: Color) -> String {
    match value {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        Color::Indexed(n) => format!("indexed:{n}"),
        Color::Reset => "reset".into(),
        value => {
            let index = match value {
                Color::Black => 0,
                Color::Red => 1,
                Color::Green => 2,
                Color::Yellow => 3,
                Color::Blue => 4,
                Color::Magenta => 5,
                Color::Cyan => 6,
                Color::Gray => 7,
                Color::DarkGray => 8,
                Color::LightRed => 9,
                Color::LightGreen => 10,
                Color::LightYellow => 11,
                Color::LightBlue => 12,
                Color::LightMagenta => 13,
                Color::LightCyan => 14,
                _ => 15,
            };
            format!("indexed:{index}")
        }
    }
}

/// Capture an existing buffer without translating its layout into browser widgets.
pub fn from_buffer(scene: Scene, buffer: &Buffer) -> Result<Capture, String> {
    if buffer.area.width != scene.width || buffer.area.height != scene.height {
        return Err("scene dimensions differ from rendered buffer".into());
    }
    Ok(Capture {
        scene,
        source: String::new(),
        cells: buffer
            .content
            .iter()
            .map(|cell| Cell {
                text: cell.symbol().into(),
                foreground: color(cell.fg),
                background: color(cell.bg),
                modifiers: cell.modifier.bits(),
                columns: cell.symbol().width().max(1),
            })
            .collect(),
    })
}

/// Render a production widget with caller-supplied state and a fixed scene clock.
pub fn capture(scene: Scene, draw: impl FnOnce(&mut Frame<'_>)) -> Result<Capture, String> {
    if !(8..=240).contains(&scene.width) || !(3..=100).contains(&scene.height) {
        return Err("width must be 8..240 and height 3..100".into());
    }
    let mut terminal =
        Terminal::new(TestBackend::new(scene.width, scene.height)).map_err(|e| e.to_string())?;
    terminal.draw(draw).map_err(|e| e.to_string())?;
    from_buffer(scene, terminal.backend().buffer())
}

/// Machine-readable snapshots for fixture inspection and caller-owned assertions.
pub fn json(captures: &[Capture]) -> Result<String, String> {
    serde_json::to_string(captures).map_err(|e| e.to_string())
}

/// A line-oriented reading of a capture for assistive technology and review.
/// Hidden cells stay in the visual receipt but never enter readable text.
#[must_use]
pub fn transcript(capture: &Capture) -> String {
    let mut lines = Vec::with_capacity(capture.scene.height as usize);
    for y in 0..capture.scene.height as usize {
        let mut line = String::new();
        let mut x = 0usize;
        while x < capture.scene.width as usize {
            let cell = &capture.cells[y * capture.scene.width as usize + x];
            if cell.modifiers & Modifier::HIDDEN.bits() == 0 {
                line.push_str(&cell.text);
            } else {
                line.extend(std::iter::repeat_n(' ', cell.columns));
            }
            x += cell.columns;
        }
        lines.push(line.trim_end().to_owned());
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines.join("\n")
}

/// Self-contained review page; escaped data is interpreted only as text cells.
pub fn html(captures: &[Capture]) -> Result<String, String> {
    if captures.is_empty() {
        return Err("a review needs at least one capture".into());
    }
    let data = json(captures)?
        .replace('<', "\\u003c")
        .replace('&', "\\u0026");
    Ok(include_str!("review.html").replace("__CAPTURES__", &data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{
        layout::Rect,
        style::{Modifier, Style},
    };
    #[test]
    fn export_retains_terminal_semantics_and_escapes_fixture_markup() {
        let scene = Scene {
            id: "fixture".into(),
            label: "</script>".into(),
            width: 8,
            height: 3,
            time_ms: 0,
        };
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 3));
        buffer.set_string(
            0,
            0,
            "猫x",
            Style::default()
                .fg(Color::Indexed(200))
                .bg(Color::Rgb(1, 2, 3))
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        );
        let capture = from_buffer(scene, &buffer).unwrap();
        assert_eq!(capture.cells[0].columns, 2);
        assert_eq!(capture.cells[0].foreground, "indexed:200");
        assert_eq!(capture.cells[0].background, "#010203");
        assert_eq!(
            capture.cells[0].modifiers,
            (Modifier::BOLD | Modifier::REVERSED).bits()
        );
        let page = html(&[capture]).unwrap();
        assert!(page.contains("\\u003c/script>"));
        assert!(!page.contains("\"</script>\""));
    }
    #[test]
    fn callback_capture_matches_direct_production_render() {
        let scene = crate::catalog()[0].clone();
        let direct = crate::render(&scene).unwrap();
        let captured = capture(scene.clone(), |frame| {
            *frame.buffer_mut() = direct.clone();
        })
        .unwrap();
        assert_eq!(
            json(&[captured]).unwrap(),
            json(&[from_buffer(scene, &direct).unwrap()]).unwrap()
        );
    }
    #[test]
    fn rejects_empty_and_mismatched_artifacts() {
        assert!(html(&[]).is_err());
        let scene = crate::catalog()[0].clone();
        assert!(from_buffer(scene, &Buffer::empty(Rect::new(0, 0, 8, 3))).is_err());
    }

    #[test]
    fn transcript_is_line_oriented_and_omits_hidden_glyphs() {
        let scene = Scene {
            id: "fixture".into(),
            label: "Fixture".into(),
            width: 8,
            height: 3,
            time_ms: 0,
        };
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 3));
        buffer.set_string(0, 0, "Visible", Style::default());
        buffer.set_string(
            0,
            1,
            "secret",
            Style::default().add_modifier(Modifier::HIDDEN),
        );
        let capture = from_buffer(scene, &buffer).unwrap();
        assert_eq!(transcript(&capture), "Visible");
        assert!(capture.cells.iter().any(|cell| cell.text == "s"));
    }
}
