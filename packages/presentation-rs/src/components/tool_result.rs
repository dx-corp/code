//! Tool output presentation; execution status and output limits are caller-owned.
use maestro_ui::UiTheme;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use unicode_width::UnicodeWidthStr;

/// A projection of the execution owner's status, never inferred from output text.
#[derive(Clone, Copy, Debug)]
pub enum ToolPhase {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
    Blocked,
}

/// A bounded, already-authorized tool result supplied by the application.
pub struct ToolResult<'a> {
    pub phase: ToolPhase,
    pub summary: &'a str,
    pub arguments: &'a str,
    pub output: &'a str,
    pub expanded: bool,
    pub detail: &'a str,
    pub truncation: Option<&'a str>,
    pub theme: UiTheme,
}

/// Compact previews omit framing-only lines; expanded output preserves them.
pub fn preview_lines(text: &str, expanded: bool) -> Vec<String> {
    text.lines()
        .filter(|line| {
            expanded
                || (!line.trim().starts_with("```")
                    && !line.trim().is_empty()
                    && !line.split_once('\t').is_some_and(|(number, content)| {
                        number.trim().parse::<usize>().is_ok() && content.trim().is_empty()
                    }))
        })
        .map(|line| line.replace('\t', "  "))
        .collect()
}

impl ToolResult<'_> {
    /// Layout and height share these exact rows so a clipped result cannot shift
    /// the following message or disagree with the transcript's scroll geometry.
    pub fn lines(&self, width: u16) -> Vec<Line<'static>> {
        if width == 0 {
            return Vec::new();
        }
        let t = self.theme;
        let (symbol, label, color) = match self.phase {
            ToolPhase::Pending => ("○", maestro_ui::localization::tr("Pending · "), t.attention),
            ToolPhase::Running => ("●", maestro_ui::localization::tr("Running · "), t.focus),
            ToolPhase::Completed => ("✓", "", t.success),
            ToolPhase::Failed => ("!", maestro_ui::localization::tr("Failed · "), t.error),
            ToolPhase::Cancelled => (
                "⊘",
                maestro_ui::localization::tr("Cancelled · "),
                t.attention,
            ),
            ToolPhase::Blocked => ("!", maestro_ui::localization::tr("Blocked · "), t.attention),
        };
        let title = format!("{label}{}", self.summary);
        let mut header = vec![
            Span::styled(format!("  {symbol} "), Style::default().fg(color)),
            Span::styled(title.clone(), Style::default().fg(t.text)),
        ];
        let hint = if self.expanded {
            maestro_ui::localization::tr("[−] collapse")
        } else {
            maestro_ui::localization::tr("[+] expand")
        };
        // Only show the complete action hint when it fits beside the outcome.
        let used = 4 + title.width();
        if used + hint.width() + 2 <= usize::from(width) {
            header.push(Span::raw(
                " ".repeat(usize::from(width) - used - hint.width()),
            ));
            header.push(Span::styled(hint.to_owned(), Style::default().fg(t.muted)));
        }
        let mut lines = vec![Line::from(header)];
        // Successful routine output stays available through expansion. Failed,
        // blocked and pending operations retain their actionable preview.
        if !self.expanded && matches!(self.phase, ToolPhase::Completed) && self.truncation.is_none()
        {
            return lines;
        }
        let row = |text: String, color| {
            Line::from(vec![
                Span::styled("  │ ", Style::default().fg(t.border)),
                Span::styled(text, Style::default().fg(color)),
            ])
        };
        if self.expanded && !self.detail.is_empty() {
            lines.push(row(self.detail.to_owned(), t.muted));
        }
        if !self.arguments.is_empty() && !self.summary.contains(self.arguments) {
            lines.push(row(self.arguments.to_owned(), t.muted));
        }
        let output = preview_lines(self.output, self.expanded);
        let limit = if self.expanded { 50 } else { 5 };
        for text in output.iter().take(limit) {
            lines.push(row(text.clone(), t.muted));
        }
        if output.len() > limit {
            lines.push(row(format!("… +{} lines", output.len() - limit), t.muted));
        }
        if let Some(truncation) = self.truncation {
            lines.push(row(truncation.to_owned(), t.attention));
        }
        lines
    }

    pub fn height(&self, width: u16) -> u16 {
        self.lines(width).len().min(u16::MAX as usize) as u16
    }
}

impl Widget for ToolResult<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(buf.area);
        Paragraph::new(self.lines(area.width)).render(area, buf);
    }
}
