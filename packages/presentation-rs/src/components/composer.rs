//! Shared composer. Editing and queue effects remain with the caller.
use maestro_ui::{
    UiTheme,
    textarea::{TextArea, TextAreaWidget},
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    text::Line,
    widgets::{Block, BorderType, Borders, Paragraph, Widget},
};

pub const PROMPT_WIDTH: u16 = 2;

/// Borrow the real editor and preformatted queue rows, rather than copying state.
pub struct Composer<'a> {
    pub editor: &'a TextArea,
    pub queued: &'a [Line<'static>],
    pub busy: bool,
    pub footer: Option<&'a str>,
    pub completion: Option<&'a str>,
    pub theme: UiTheme,
}

impl Composer<'_> {
    fn inner(&self, area: Rect) -> Rect {
        let area = self.frame_area(area);
        Rect::new(
            area.x.saturating_add(1),
            area.y.saturating_add(1),
            area.width.saturating_sub(2),
            area.height.saturating_sub(2),
        )
    }

    fn frame_area(&self, area: Rect) -> Rect {
        Rect {
            height: area
                .height
                .saturating_sub(u16::from(self.footer.is_some() && area.height >= 4)),
            ..area
        }
    }

    fn queue_height(&self, area: Rect) -> u16 {
        (self.queued.len().min(u16::MAX as usize) as u16)
            .min(self.inner(area).height.saturating_sub(1))
    }

    /// The exact viewport used by rendering and terminal cursor placement.
    pub fn editor_area(&self, area: Rect) -> Rect {
        let inner = self.inner(area);
        let queued = self.queue_height(area);
        Rect::new(
            inner.x.saturating_add(PROMPT_WIDTH),
            inner.y.saturating_add(queued),
            inner.width.saturating_sub(PROMPT_WIDTH),
            inner.height.saturating_sub(queued),
        )
    }

    pub fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        if area.width < 3 || area.height < 3 {
            return None;
        }
        let editor = self.editor_area(area);
        if self.editor.is_empty() {
            let inner = self.inner(area);
            return Some((
                inner
                    .x
                    .saturating_add(PROMPT_WIDTH - 1)
                    .min(area.right() - 2),
                editor.y,
            ));
        }
        if editor.is_empty() {
            return None;
        }
        let (row, col) = self.editor.cursor_line_col(editor.width)?;
        let scroll = row
            .saturating_add(1)
            .saturating_sub(usize::from(editor.height));
        Some((
            editor.x + col.min(editor.width - 1),
            editor.y + (row - scroll) as u16,
        ))
    }
}

impl Widget for Composer<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(buf.area);
        if area.is_empty() {
            return;
        }
        let theme = self.theme.on_panel();
        let frame = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(if self.busy { theme.muted } else { theme.border }))
            .style(Style::default().bg(theme.surface));
        let frame_area = self.frame_area(area);
        frame.render(frame_area, buf);
        if frame_area.height < area.height {
            if let Some(footer) = self.footer {
                Paragraph::new(footer)
                    .style(Style::default().fg(theme.muted))
                    .render(
                        Rect::new(
                            area.x + 1,
                            area.bottom() - 1,
                            area.width.saturating_sub(2),
                            1,
                        ),
                        buf,
                    );
            }
        }
        if area.height < 3 || area.width < 3 {
            return;
        }
        let inner = self.inner(area);
        let queued = self.queue_height(area);
        if queued > 0 {
            let mut lines = self.queued[..usize::from(queued)].to_vec();
            if self.queued.len() > usize::from(queued) {
                lines[usize::from(queued) - 1] = Line::styled(
                    maestro_ui::localization::tr("… more queued input"),
                    Style::default().fg(theme.muted),
                );
            }
            Paragraph::new(lines).render(
                Rect {
                    height: queued,
                    ..inner
                },
                buf,
            );
        }
        let editor = self.editor_area(area);
        buf.set_stringn(
            inner.x,
            editor.y,
            "> ",
            usize::from(inner.width),
            Style::default().fg(theme.focus),
        );
        if !editor.is_empty() {
            let scroll = self
                .editor
                .cursor_line_col(editor.width)
                .map_or(0, |(row, _)| {
                    row.saturating_add(1)
                        .saturating_sub(usize::from(editor.height))
                });
            TextAreaWidget::new(self.editor)
                .scroll(scroll)
                .style(Style::default().fg(theme.text))
                .render(editor, buf);
            if let (Some(completion), Some((x, y))) = (self.completion, self.cursor_pos(area)) {
                let x = x.max(editor.x);
                buf.set_stringn(
                    x,
                    y,
                    completion,
                    usize::from(editor.right().saturating_sub(x)),
                    Style::default().fg(theme.muted),
                );
            }
        }
    }
}
