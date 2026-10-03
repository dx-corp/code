//! Rewind picker modal
//!
//! Lists the current session's file checkpoints so the user can pick one to
//! restore. Opened by pressing Esc twice with an empty composer.

use crossterm::event::KeyCode;
use maestro_ui::{ActionPicker, KeyHint, Modal, PickerOptions, PickerOutcome, UiTheme};
use ratatui::{
    Frame,
    layout::Rect,
    style::Modifier,
    text::{Line, Span},
    widgets::ListItem,
};

use crate::checkpoints::Checkpoint;

/// Rewind picker modal state
pub struct RewindPicker {
    picker: ActionPicker<Checkpoint>,
    checkpoint_count: usize,
}

impl Default for RewindPicker {
    fn default() -> Self {
        Self::new()
    }
}

impl RewindPicker {
    /// Create a new rewind picker
    #[must_use]
    pub fn new() -> Self {
        Self {
            picker: ActionPicker::new(Vec::new()),
            checkpoint_count: 0,
        }
    }

    /// Show the modal with the given checkpoints (newest first)
    pub fn show(&mut self, checkpoints: Vec<Checkpoint>) {
        self.checkpoint_count = checkpoints.len();
        self.picker = ActionPicker::new(checkpoints);
        self.picker.open();
    }

    /// Hide the modal
    pub fn hide(&mut self) {
        self.picker.close();
    }

    /// Check if visible
    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.picker.is_open()
    }

    /// Move selection up
    pub fn move_up(&mut self) {
        self.picker.handle_key(KeyCode::Up, false);
    }

    /// Move selection down
    pub fn move_down(&mut self) {
        self.picker.handle_key(KeyCode::Down, false);
    }

    /// Confirm selection and return the chosen checkpoint
    pub fn confirm(&mut self) -> Option<Checkpoint> {
        match self.picker.handle_key(KeyCode::Enter, false) {
            PickerOutcome::Selected(checkpoint) => Some(checkpoint),
            _ => None,
        }
    }

    /// Summary of the files a checkpoint touched
    fn files_summary(checkpoint: &Checkpoint) -> String {
        const MAX_LISTED: usize = 3;
        let total = checkpoint.entries.len();
        let listed: Vec<&str> = checkpoint
            .entries
            .iter()
            .take(MAX_LISTED)
            .map(|entry| entry.path.as_str())
            .collect();
        let mut summary = maestro_ui::localization::format(
            if total == 1 {
                "{0} file: {1}"
            } else {
                "{0} files: {1}"
            },
            &[total.to_string(), listed.join(", ")],
        );
        if total > MAX_LISTED {
            summary.push_str(&maestro_ui::localization::format(
                ", +{0} more",
                &[(total - MAX_LISTED).to_string()],
            ));
        }
        summary
    }

    /// Render the modal using the active application palette.
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        self.render_themed(frame, area, crate::themes::current_ui_theme());
    }

    fn render_themed(&mut self, frame: &mut Frame, area: Rect, theme: UiTheme) {
        if !self.is_visible() {
            return;
        }
        let height = self.checkpoint_count.saturating_mul(2).saturating_add(3);
        let height = u16::try_from(height).unwrap_or(u16::MAX).max(5);
        let inner = Modal::new(
            maestro_ui::localization::tr("Rewind to checkpoint"),
            72,
            height,
        )
        .theme(theme)
        .render(frame, area);
        let row_theme = theme.on_panel();
        self.picker.render(
            frame,
            inner,
            theme,
            PickerOptions {
                empty: maestro_ui::localization::tr("No checkpoints to rewind to"),
                hints: Some(&[
                    KeyHint::new("↑↓", maestro_ui::localization::tr("navigate")),
                    KeyHint::new("Enter", maestro_ui::localization::tr("files")),
                    KeyHint::new("c", "conversation"),
                    KeyHint::new("b", "both"),
                    KeyHint::new("Esc", maestro_ui::localization::tr("cancel")),
                ]),
                ..PickerOptions::default()
            },
            |checkpoint| {
                let title = Line::from(vec![
                    Span::styled(
                        checkpoint.short_id().to_string(),
                        row_theme.text_style().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!(
                        "  {}  \"{}\"",
                        checkpoint.created_at, checkpoint.prompt
                    )),
                ]);
                let files = Line::from(Span::styled(
                    format!("  {}", Self::files_summary(checkpoint)),
                    row_theme.muted_style(),
                ));
                ListItem::new(vec![title, files])
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoints::{EntryKind, FileEntry};
    use std::path::PathBuf;

    fn checkpoint(id: &str, files: &[&str]) -> Checkpoint {
        Checkpoint {
            id: id.to_string(),
            created_at: "2026-07-24T00:00:00Z".to_string(),
            prompt: "do things".to_string(),
            repo_root: PathBuf::from("/tmp/repo"),
            head: None,
            user_turn_index: None,
            entries: files
                .iter()
                .map(|path| FileEntry {
                    path: (*path).to_string(),
                    kind: EntryKind::Modified,
                    pre_blob: Some("pre".to_string()),
                    post_hash: Some("post".to_string()),
                    post_snapshot_oversized: false,
                })
                .collect(),
        }
    }

    #[test]
    fn show_selects_newest_and_navigation_clamps() {
        let mut picker = RewindPicker::new();
        picker.show(vec![
            checkpoint("newest-xxx", &["a.rs"]),
            checkpoint("oldest-xxx", &["b.rs"]),
        ]);
        assert!(picker.is_visible());
        assert_eq!(picker.picker.selected().unwrap().id, "newest-xxx");

        picker.move_up();
        assert_eq!(picker.picker.selected().unwrap().id, "newest-xxx");
        picker.move_down();
        assert_eq!(picker.picker.selected().unwrap().id, "oldest-xxx");
        picker.move_down();
        assert_eq!(picker.picker.selected().unwrap().id, "oldest-xxx");

        let chosen = picker.confirm().expect("a checkpoint is selected");
        assert_eq!(chosen.id, "oldest-xxx");
        assert!(!picker.is_visible());
    }

    #[test]
    fn files_summary_truncates_long_lists() {
        let cp = checkpoint("id", &["a.rs", "b.rs", "c.rs", "d.rs"]);
        assert_eq!(
            RewindPicker::files_summary(&cp),
            "4 files: a.rs, b.rs, c.rs, +1 more"
        );
        let one = checkpoint("id", &["a.rs"]);
        assert_eq!(RewindPicker::files_summary(&one), "1 file: a.rs");
    }

    #[test]
    fn rewind_picker_uses_light_and_opaque_palettes_at_wide_and_narrow_widths() {
        use ratatui::{Terminal, backend::TestBackend, style::Color};
        let opaque = UiTheme {
            surface: Color::Rgb(20, 25, 30),
            panel: Some(Color::Rgb(30, 35, 40)),
            selection: Some(Color::Rgb(50, 55, 60)),
            text: Color::Rgb(230, 235, 240),
            muted: Color::Rgb(160, 165, 170),
            border: Color::Rgb(100, 105, 110),
            ..UiTheme::default()
        };
        for theme in [crate::themes::light_theme().ui_theme(), opaque] {
            for width in [100, 40] {
                let mut picker = RewindPicker::new();
                picker.show(vec![
                    checkpoint("newest-xxx", &["a.rs"]),
                    checkpoint("oldest-xxx", &["b.rs"]),
                ]);
                picker.move_down();
                let mut terminal = Terminal::new(TestBackend::new(width, 20)).unwrap();
                terminal
                    .draw(|frame| picker.render_themed(frame, frame.area(), theme))
                    .unwrap();
                let buffer = terminal.backend().buffer();
                let rendered: Vec<String> = (0..20)
                    .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
                    .collect();
                let selected_y = rendered
                    .iter()
                    .position(|line| line.contains("oldest"))
                    .unwrap() as u16;
                let file_y = selected_y + 1;
                let file_x = rendered[file_y as usize].find("1 file").unwrap();
                // Locate by cells, because the selection marker is multibyte UTF-8.
                let file_x = rendered[file_y as usize][..file_x].chars().count() as u16;
                assert_eq!(buffer[(file_x, file_y)].fg, theme.muted);
                assert_eq!(
                    buffer[(file_x, file_y)].bg,
                    theme.selection.unwrap_or(theme.on_panel().surface)
                );
                let old_x = rendered[selected_y as usize].find("oldest").unwrap();
                let old_x = rendered[selected_y as usize][..old_x].chars().count() as u16;
                assert_eq!(buffer[(old_x, selected_y)].fg, theme.text);
                assert_eq!(
                    buffer[(old_x, selected_y)].bg,
                    theme.selection.unwrap_or(theme.on_panel().surface)
                );
                let outer = Modal::new("Rewind to checkpoint", 72, 7)
                    .theme(theme)
                    .area(buffer.area);
                assert_eq!(buffer[(outer.x, outer.y)].fg, theme.border);
                assert_eq!(buffer[(outer.x, outer.y)].bg, theme.on_panel().surface);
                assert_eq!(picker.confirm().unwrap().id, "oldest-xxx");
                if width == 100 {
                    assert!(rendered.iter().any(|line| line.contains("do things")));
                    assert!(rendered.iter().any(|line| line.contains("conversation")));
                    assert!(rendered.iter().any(|line| line.contains("both")));
                }
            }
        }
    }

    #[test]
    fn rewind_picker_empty_and_tiny_areas_are_safe() {
        use ratatui::{Terminal, backend::TestBackend};
        for (width, height) in [(100, 20), (4, 3), (1, 1)] {
            let mut picker = RewindPicker::new();
            picker.show(Vec::new());
            picker.move_up();
            picker.move_down();
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    picker.render_themed(
                        frame,
                        frame.area(),
                        crate::themes::light_theme().ui_theme(),
                    );
                })
                .unwrap();
            if width == 100 {
                let text: String = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect();
                assert!(text.contains("No checkpoints to rewind to"));
            }
            assert!(picker.confirm().is_none());
            assert!(!picker.is_visible());
        }
    }
}
