//! Deterministic component scenes using the widgets linked by the native TUI.
extern crate self as maestro_ui_preview;

pub mod adapters;
pub mod authoring;
pub mod contract;
mod conversation;
mod menus;
pub mod registry;
pub mod review;
pub mod schema;
mod stories;

use maestro_presentation::{
    appearance::{Appearance, LOOKS},
    clock::{ViewClock, pet_frame},
    components::{
        deixic_logo::render_welcome_with_summary,
        dex_companion::{DexCompanion, DexCompanionState, DexPersonality, render_welcome_portrait},
        session_header::SessionHeaderWidget,
    },
    dex_delight::DexLook,
    shimmer::{DEIXIC_SURFACE, DEIXIC_TEXT},
};
use ratatui::{prelude::*, widgets::Paragraph};
use serde::Serialize;
use std::time::Duration;

#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
pub enum StoryFilter {
    Story(String),
    Adapter(String),
}

#[derive(Clone, Debug, Serialize)]
pub struct Scene {
    pub id: String,
    pub label: String,
    pub width: u16,
    pub height: u16,
    pub time_ms: u64,
}
/// Appearance scenes come from the same stable IDs as native commands.
fn legacy_catalog() -> Vec<Scene> {
    let mut scenes = Vec::new();
    for width in [40, 60, 100] {
        for (id, label) in [
            ("startup", "Startup"),
            ("header", "Session header"),
            ("quiet", "Quiet companion"),
            ("motion-off", "Motion disabled"),
            ("working", "Working"),
            ("finished", "Finished"),
            ("failed", "Failed"),
            ("needs-input", "Needs input"),
        ] {
            scenes.push(Scene {
                id: id.into(),
                label: label.into(),
                width,
                height: 10,
                time_ms: 400,
            });
        }
        for action in &LOOKS {
            scenes.push(Scene {
                id: action.id.into(),
                label: action.label.into(),
                width: width.max(44),
                height: 10,
                time_ms: 0,
            });
        }
        for time_ms in [0, 200, 400, 600, 900] {
            scenes.push(Scene {
                id: "pet".into(),
                label: "Pet reaction".into(),
                width: width.max(44),
                height: 10,
                time_ms,
            });
        }
    }
    for (width, height) in [(60, 20), (100, 30)] {
        for id in ["picker", "picker-scrolled"] {
            scenes.push(Scene {
                id: id.into(),
                label: if id == "picker-scrolled" {
                    "Appearance picker · scrolled"
                } else {
                    "Appearance picker"
                }
                .into(),
                width,
                height,
                time_ms: 0,
            });
        }
    }
    scenes.extend(conversation::scenes());
    scenes
}

fn legacy_render(scene: &Scene) -> Result<Buffer, String> {
    if !(8..=240).contains(&scene.width)
        || !(3..=100).contains(&scene.height)
        || scene.time_ms > 86_400_000
    {
        return Err("width must be 8..240, height 3..100, time-ms 0..86400000".into());
    }
    if scene.id.starts_with("conversation-") {
        return conversation::render(scene);
    }
    if matches!(scene.id.as_str(), "picker" | "picker-scrolled") {
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(
            scene.width,
            scene.height,
        ))
        .map_err(|e| e.to_string())?;
        let mut picker = maestro_ui::ActionPicker::new(LOOKS.to_vec())
            .identified_by(|action| action.id)
            .expect("Dex appearance IDs are unique");
        picker.open();
        if scene.id == "picker-scrolled" {
            for _ in 1..LOOKS.len() {
                picker.handle_key(crossterm::event::KeyCode::Down, false);
            }
        }
        let mut look = DexLook::default();
        if let Some(action) = picker.selected() {
            match action.value {
                Appearance::Accessory(value) => look.accessory = value,
                Appearance::Accent(value) => look.accent = value,
            }
        }
        terminal
            .draw(|frame| {
                maestro_presentation::components::appearance_picker::render_appearance(
                    frame,
                    frame.area(),
                    &mut picker,
                    look,
                    maestro_presentation::palette::default_controls(),
                );
            })
            .map_err(|e| e.to_string())?;
        return Ok(terminal.backend().buffer().clone());
    }
    let area = Rect::new(0, 0, scene.width, scene.height);
    let mut buf = Buffer::empty(area);
    let (r, g, b) = DEIXIC_SURFACE;
    let (tr, tg, tb) = DEIXIC_TEXT;
    buf.set_style(
        area,
        Style::default()
            .bg(Color::Rgb(r, g, b))
            .fg(Color::Rgb(tr, tg, tb)),
    );
    let mut look = DexLook::default();
    let action = LOOKS.iter().find(|action| action.id == scene.id);
    if let Some(action) = action {
        match action.value {
            Appearance::Accessory(value) => look.accessory = value,
            Appearance::Accent(value) => look.accent = value,
        }
    }
    let state = match scene.id.as_str() {
        "working" => DexCompanionState::Working,
        "finished" => DexCompanionState::Finished,
        "failed" => DexCompanionState::Failed,
        "needs-input" => DexCompanionState::NeedsInput,
        "startup" | "header" | "quiet" | "motion-off" | "pet" => DexCompanionState::Ready,
        _ if action.is_some() => DexCompanionState::Ready,
        _ => return Err(format!("unknown scene ID: {}", scene.id)),
    };
    let clock = ViewClock::Fixed(Duration::from_millis(scene.time_ms));
    let mut reaction = maestro_interaction::Reaction::default();
    if matches!(
        scene.id.as_str(),
        "pet" | "motion-off" | "quiet" | "failed" | "needs-input"
    ) {
        reaction.start(Duration::ZERO);
        look.pet_frame = pet_frame(&reaction, clock.now());
    }
    let motion = scene.id != "motion-off";
    let personality = if scene.id == "quiet" {
        DexPersonality::Quiet
    } else {
        DexPersonality::Standard
    };
    if scene.id == "header" {
        SessionHeaderWidget::new(Some("~/projects/maestro"), Some("main"))
            .with_context(Some(9500), Some(500000))
            .render(Rect::new(0, 0, area.width, 1), &mut buf);
    } else if scene.id == "startup" || action.is_some() || scene.id == "pet" {
        // The startup title has no animation; the explicit clock controls the portrait.
        render_welcome_with_summary(
            area,
            &mut buf,
            false,
            None,
            true,
            Some(("Example model · Build", "~/projects/maestro · main")),
        );
        render_welcome_portrait(area, &mut buf, look, state, motion);
    } else {
        let companion = DexCompanion::new(state)
            .look(look)
            .personality(personality)
            .animations(motion)
            .frame(scene.time_ms / 100);
        companion.render_face(Rect::new(2, 2, 8, 2), &mut buf);
        Paragraph::new(companion.status_line())
            .render(Rect::new(2, 5, area.width.saturating_sub(4), 1), &mut buf);
    }
    Ok(buf)
}

/// Lossless cell-style output for the existing ANSI screenshot renderer.
pub fn ansi(buffer: &Buffer) -> String {
    use std::fmt::Write;
    use unicode_width::UnicodeWidthStr;
    fn color(out: &mut String, color: Color, foreground: bool) {
        let prefix = if foreground { 38 } else { 48 };
        match color {
            Color::Rgb(r, g, b) => {
                let _ = write!(out, "\x1b[{prefix};2;{r};{g};{b}m");
            }
            Color::Indexed(n) => {
                let _ = write!(out, "\x1b[{prefix};5;{n}m");
            }
            Color::Reset => out.push_str(if foreground { "\x1b[39m" } else { "\x1b[49m" }),
            c => {
                let n = match c {
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
                let _ = write!(out, "\x1b[{prefix};5;{n}m");
            }
        }
    }
    let mut out = String::from("\x1b[?25l\x1b[0m\x1b[2J");
    for y in buffer.area.y..buffer.area.bottom() {
        let _ = write!(out, "\x1b[{};1H", y - buffer.area.y + 1);
        let mut x = buffer.area.x;
        while x < buffer.area.right() {
            let cell = &buffer[(x, y)];
            out.push_str("\x1b[0m");
            color(&mut out, cell.fg, true);
            color(&mut out, cell.bg, false);
            for (modifier, code) in [
                (Modifier::BOLD, 1),
                (Modifier::DIM, 2),
                (Modifier::ITALIC, 3),
                (Modifier::UNDERLINED, 4),
                (Modifier::SLOW_BLINK, 5),
                (Modifier::RAPID_BLINK, 6),
                (Modifier::REVERSED, 7),
                (Modifier::HIDDEN, 8),
                (Modifier::CROSSED_OUT, 9),
            ] {
                if cell.modifier.contains(modifier) {
                    let _ = write!(out, "\x1b[{code}m");
                }
            }
            out.push_str(cell.symbol());
            x += cell.symbol().width().max(1) as u16;
        }
    }
    out.push_str("\x1b[0m");
    out
}

/// One registry powers CLI rendering, browser navigation and fixture exports.
pub fn registry() -> Result<registry::Registry, String> {
    let mut registry = registry::Registry::default();
    registry.import(
        legacy_catalog(),
        "products/maestro/packages/ui-preview-rs/src/lib.rs",
        legacy_render,
    )?;
    stories::register(&mut registry)?;
    authoring::register_menu_recipes(&mut registry)?;
    Ok(registry)
}
pub fn catalog() -> Vec<Scene> {
    registry().expect("valid built-in stories").scenes()
}
pub fn render(scene: &Scene) -> Result<Buffer, String> {
    registry()?.render(scene)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn generated_story_modules_can_use_the_public_crate_name() {
        let registry = maestro_ui_preview::registry::Registry::default();
        assert!(registry.scenes().is_empty());
    }

    #[test]
    fn catalog_renders_and_all_appearance_ids_are_present() {
        for scene in catalog() {
            assert_eq!(render(&scene).unwrap(), render(&scene).unwrap());
        }
        for action in &LOOKS {
            assert!(catalog().iter().any(|s| s.id == action.id));
        }
    }
    #[test]
    fn motion_and_authoritative_states_ignore_pet_frames() {
        for id in ["quiet", "motion-off", "failed", "needs-input"] {
            let mut scene = Scene {
                id: id.into(),
                label: id.into(),
                width: 60,
                height: 10,
                time_ms: 0,
            };
            let before = render(&scene).unwrap();
            scene.time_ms = 650;
            assert_eq!(before, render(&scene).unwrap(), "{id}");
        }
    }
    #[test]
    fn catalog_appearance_and_reaction_frames_are_visible() {
        for scene in catalog() {
            if scene.id.starts_with("accessory-") || scene.id.starts_with("accent-") {
                if matches!(scene.id.as_str(), "accessory-none" | "accent-violet") {
                    continue;
                }
                let mut baseline = scene.clone();
                baseline.id = "startup".into();
                assert_ne!(
                    render(&scene).unwrap(),
                    render(&baseline).unwrap(),
                    "{} at {}",
                    scene.id,
                    scene.width
                );
            }
            if scene.id == "pet" {
                let mut expired = scene.clone();
                expired.time_ms = 900;
                let actual = render(&scene).unwrap();
                let resting = render(&expired).unwrap();
                if scene.time_ms < 600 {
                    assert_ne!(actual, resting, "{}ms at {}", scene.time_ms, scene.width);
                } else {
                    assert_eq!(actual, resting, "{}ms at {}", scene.time_ms, scene.width);
                }
            }
        }
    }

    #[test]
    fn ansi_preserves_wide_cells_styles_and_row_positions() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 2));
        buffer.set_string(
            0,
            0,
            "猫x",
            Style::default()
                .fg(Color::Rgb(1, 2, 3))
                .bg(Color::Indexed(4))
                .add_modifier(Modifier::BOLD | Modifier::DIM),
        );
        let output = ansi(&buffer);
        assert!(output.contains("\x1b[38;2;1;2;3m"));
        assert!(output.contains("\x1b[48;5;4m"));
        assert!(output.contains("\x1b[1m\x1b[2m猫"));
        assert!(!output.contains("猫 "));
        assert!(output.contains("\x1b[2;1H"));
        assert!(output.contains("\x1b[39m\x1b[49m"));
    }

    #[test]
    fn rejects_unknown_and_unbounded_inputs() {
        let mut scene = catalog()[0].clone();
        scene.id = "../unknown".into();
        assert!(render(&scene).is_err());
        scene.id = "startup".into();
        scene.width = u16::MAX;
        assert!(render(&scene).is_err());
    }
}
