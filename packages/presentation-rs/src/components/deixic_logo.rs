//! Dex's warm orb, drawn with foreground dots so it works in ordinary terminals.
//! The desktop SVG and this terminal mark share the same silhouette and blue orbit.

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

use crate::shimmer::{
    DEIXIC_LOGO_BASE, DEIXIC_LOGO_HILITE, DEIXIC_MUTED, DEIXIC_TEXT, shimmer_spans,
};

/// Product title used by the launch and onboarding surfaces.
pub const PRODUCT_TITLE: &str = "Dex Code";
/// Empty-composer hint shared by the welcome and status surfaces.
pub const COMPOSER_HINT: &str = "Type a message or press ? for commands.";

/// A transparent dot silhouette: two negative-space eyes, a tiny nub at right,
/// and a blue arc across the lower edge. Each glyph occupies one terminal cell.
pub const LOGO_FULL: &str = r"
        ·••●••·
     ·••●••●•●•••·
   ·•●••●•●••●•●•••·
  •••●•●••●•●••●•●••
 •●•●••●  ●●  ●●●●•●•
 •••●•●••●•●••●•●•••••
  •●••●•●••●•●••●••·
   ·•●••●•●••●•●••·
     ·•●••●•●•••·
        ·•••••·
";

const LOGO_FULL_WORKING: &str = LOGO_FULL;

/// Four-row mark for ordinary short panes.
pub const LOGO_COMPACT: &str = r"
    ·••●••·
  •●●●●●●●●•
 •●●  ●  ●●●••
   ·•●●●●●•·
";
const LOGO_COMPACT_WORKING: &str = LOGO_COMPACT;

/// Two-row mark when there is little vertical room.
pub const LOGO_TINY: &str = r"
  ·•●●●•·
  ·•●●●•·
";
const LOGO_TINY_WORKING: &str = LOGO_TINY;

/// One-line fallback that remains legible without color.
pub const LOGO_MICRO: &str = "  ·•●•·";
const LOGO_MICRO_WORKING: &str = LOGO_MICRO;

/// Minimum area height (rows) to show the one-line mark.
pub const MICRO_MIN_HEIGHT: u16 = 4;
/// Minimum area height (rows) to show the tiny mark.
pub const TINY_MIN_HEIGHT: u16 = 7;
/// Minimum area height for the compact mark.
pub const COMPACT_MIN_HEIGHT: u16 = 10;
/// Minimum area height for the full mark.
pub const FULL_MIN_HEIGHT: u16 = 14;

/// Reserve enough rows for the orb and the prompt; short panes use four dots rows.
#[must_use]
pub const fn welcome_logo_height(area_height: u16) -> u16 {
    if area_height >= FULL_MIN_HEIGHT {
        FULL_MIN_HEIGHT
    } else {
        COMPACT_MIN_HEIGHT
    }
}

/// The first row below the portrait and its breathing room.
#[must_use]
pub fn welcome_prompt_row(area: Rect) -> u16 {
    area.y + logo_line_count(welcome_logo_height(area.height)) + 2
}

/// Preserve the stable orb silhouette across activity states.
#[must_use]
pub fn portrait_expression(line: &str, eyes: &str) -> String {
    let _ = eyes;
    line.to_owned()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LaunchState {
    Idle,
    Working,
}

fn pick_logo_for_state(area_height: u16, state: LaunchState) -> Option<&'static str> {
    if area_height < MICRO_MIN_HEIGHT {
        return None;
    }

    Some(match (area_height, state) {
        (height, LaunchState::Idle) if height < TINY_MIN_HEIGHT => LOGO_MICRO,
        (height, LaunchState::Working) if height < TINY_MIN_HEIGHT => LOGO_MICRO_WORKING,
        (height, LaunchState::Idle) if height < COMPACT_MIN_HEIGHT => LOGO_TINY,
        (height, LaunchState::Working) if height < COMPACT_MIN_HEIGHT => LOGO_TINY_WORKING,
        (height, LaunchState::Idle) if height < FULL_MIN_HEIGHT => LOGO_COMPACT,
        (height, LaunchState::Working) if height < FULL_MIN_HEIGHT => LOGO_COMPACT_WORKING,
        (_, LaunchState::Idle) => LOGO_FULL,
        (_, LaunchState::Working) => LOGO_FULL_WORKING,
    })
}

/// Pick idle logo art for the given available height. `None` if too short.
#[must_use]
pub fn pick_logo(area_height: u16) -> Option<&'static str> {
    pick_logo_for_state(area_height, LaunchState::Idle)
}

/// Non-empty lines of a logo string.
#[must_use]
pub fn logo_lines(logo: &str) -> Vec<&str> {
    logo.lines().filter(|line| !line.is_empty()).collect()
}

/// Line count of the selected logo (0 when hidden).
#[must_use]
pub fn logo_line_count(area_height: u16) -> u16 {
    pick_logo(area_height)
        .map(|logo| logo_lines(logo).len() as u16)
        .unwrap_or(0)
}

/// Visual width of the selected logo (display columns).
#[must_use]
pub fn logo_visual_width(area_height: u16) -> u16 {
    pick_logo(area_height)
        .map(|logo| {
            logo_lines(logo)
                .iter()
                .map(|line| UnicodeWidthStr::width(*line))
                .max()
                .unwrap_or(12) as u16
        })
        .unwrap_or(12)
}

/// Keep working-state art stable while status text reports activity.
#[must_use]
pub fn shimmered_logo_lines(area_height: u16) -> Vec<Line<'static>> {
    static_logo_lines_for_state(area_height, LaunchState::Working)
}

/// Static idle orb lines.
#[must_use]
pub fn static_logo_lines(area_height: u16) -> Vec<Line<'static>> {
    static_logo_lines_for_state(area_height, LaunchState::Idle)
}

fn static_logo_lines_for_state(area_height: u16, state: LaunchState) -> Vec<Line<'static>> {
    let _ = state;
    orb_logo_lines(area_height, None)
}

/// Render the orb with warm dots on dark surfaces and deeper ochre on light ones.
#[must_use]
pub fn orb_logo_lines(area_height: u16, theme: Option<maestro_ui::UiTheme>) -> Vec<Line<'static>> {
    let Some(logo) = pick_logo(area_height) else {
        return Vec::new();
    };
    logo_lines(logo)
        .into_iter()
        .enumerate()
        .map(|(row, line)| {
            let spans = line
                .chars()
                .enumerate()
                .map(|(col, glyph)| {
                    if glyph == ' ' {
                        Span::raw(" ")
                    } else {
                        Span::styled(
                            glyph.to_string(),
                            Style::default().fg(orb_color(row, col, theme)),
                        )
                    }
                })
                .collect::<Vec<_>>();
            Line::from(spans)
        })
        .collect()
}

fn orb_color(row: usize, col: usize, theme: Option<maestro_ui::UiTheme>) -> Color {
    let light_surface = matches!(theme.map(|t| t.surface), Some(Color::Rgb(r, g, b)) if u16::from(r) + u16::from(g) + u16::from(b) > 480);
    let blue_arc = (row == 6 && (2..=6).contains(&col))
        || (row == 7 && (4..=15).contains(&col))
        || (row == 8 && (13..=18).contains(&col));
    if blue_arc {
        return if light_surface {
            Color::Rgb(23, 112, 161)
        } else {
            Color::Rgb(97, 206, 255)
        };
    }
    if light_surface {
        const WARM: [(u8, u8, u8); 10] = [
            (144, 86, 10),
            (151, 91, 10),
            (158, 95, 12),
            (165, 99, 15),
            (174, 101, 17),
            (181, 103, 18),
            (181, 97, 17),
            (172, 85, 17),
            (159, 74, 20),
            (146, 66, 22),
        ];
        let (r, g, b) = WARM[row.min(9)];
        Color::Rgb(r, g, b)
    } else {
        const WARM: [(u8, u8, u8); 10] = [
            (255, 236, 155),
            (255, 227, 132),
            (255, 217, 103),
            (255, 207, 79),
            (255, 199, 70),
            (255, 185, 62),
            (252, 167, 54),
            (246, 150, 49),
            (238, 134, 44),
            (224, 119, 41),
        ];
        let (r, g, b) = WARM[row.min(9)];
        Color::Rgb(r, g, b)
    }
}

/// Canonical product title line.
#[must_use]
pub fn product_title_line(animate: bool) -> Line<'static> {
    if animate {
        Line::from(shimmer_spans(PRODUCT_TITLE)).alignment(Alignment::Center)
    } else {
        Line::from(Span::styled(
            PRODUCT_TITLE,
            Style::default()
                .fg(Color::Rgb(DEIXIC_TEXT.0, DEIXIC_TEXT.1, DEIXIC_TEXT.2))
                .add_modifier(Modifier::BOLD),
        ))
        .alignment(Alignment::Center)
    }
}

/// Hint line under the brand block.
#[must_use]
pub fn hint_line() -> Line<'static> {
    Line::from(Span::styled(
        COMPOSER_HINT,
        Style::default().fg(Color::Rgb(DEIXIC_MUTED.0, DEIXIC_MUTED.1, DEIXIC_MUTED.2)),
    ))
    .alignment(Alignment::Center)
}

/// Quiet session metadata that anchors the brand block without adding chrome.
#[must_use]
pub fn session_meta_line(session_id: &str, ready: bool) -> Line<'static> {
    let status = if ready { "• ready" } else { "• working" };
    Line::from(vec![
        Span::styled(
            status,
            Style::default().fg(Color::Rgb(
                DEIXIC_LOGO_HILITE.0,
                DEIXIC_LOGO_HILITE.1,
                DEIXIC_LOGO_HILITE.2,
            )),
        ),
        Span::styled(
            maestro_ui::localization::format("  ·  session {0}", &[(session_id).to_string()]),
            Style::default()
                .fg(Color::Rgb(
                    DEIXIC_LOGO_BASE.0,
                    DEIXIC_LOGO_BASE.1,
                    DEIXIC_LOGO_BASE.2,
                ))
                .add_modifier(Modifier::DIM),
        ),
    ])
    .alignment(Alignment::Center)
}

/// Full welcome content: logo + product title + hint.
///
/// `area_height` drives logo tier. `animate` permits sheen while working.
#[must_use]
pub fn welcome_content_lines(area_height: u16, animate: bool) -> Vec<Line<'static>> {
    welcome_content_lines_with_metadata(area_height, animate, None, !animate)
}

/// Build welcome content with session metadata from the live application state.
///
/// A missing session id intentionally omits the metadata row rather than
/// presenting a fabricated session label.
#[must_use]
pub fn welcome_content_lines_with_metadata(
    area_height: u16,
    animate: bool,
    session_id: Option<&str>,
    ready: bool,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let state = if ready {
        LaunchState::Idle
    } else {
        LaunchState::Working
    };
    let should_animate = animate && state == LaunchState::Working;
    let micro_logo = area_height < TINY_MIN_HEIGHT;

    let logo = if should_animate {
        shimmered_logo_lines(area_height)
    } else {
        static_logo_lines_for_state(area_height, state)
    };
    if !logo.is_empty() {
        for line in logo {
            lines.push(line.alignment(Alignment::Center));
        }
        if !micro_logo {
            lines.push(Line::from(""));
        }
    }

    lines.push(product_title_line(should_animate));
    lines.push(hint_line());
    if area_height >= COMPACT_MIN_HEIGHT {
        let Some(session_id) = session_id else {
            return lines;
        };
        lines.push(Line::from(""));
        lines.push(session_meta_line(session_id, ready));
    }
    lines
}

/// Render the Deixic Code welcome block into `area` (centered vertically).
pub fn render_welcome(area: Rect, buf: &mut Buffer, animate: bool) {
    render_welcome_with_metadata(area, buf, animate, None, !animate);
}

/// Render a compact startup summary beside the mark, above the conversation.
pub fn render_welcome_with_metadata(
    area: Rect,
    buf: &mut Buffer,
    animate: bool,
    session_id: Option<&str>,
    ready: bool,
) {
    render_welcome_with_summary(area, buf, animate, session_id, ready, None);
}

/// Render startup facts supplied by the live chat state beside the compact mark.
pub fn render_welcome_with_summary(
    area: Rect,
    buf: &mut Buffer,
    animate: bool,
    session_id: Option<&str>,
    ready: bool,
    facts: Option<(&str, &str)>,
) {
    render_welcome_with_theme(area, buf, animate, session_id, ready, facts, None);
}

/// Render the same welcome layout using an explicitly supplied application palette.
pub fn render_welcome_with_theme(
    area: Rect,
    buf: &mut Buffer,
    animate: bool,
    session_id: Option<&str>,
    ready: bool,
    facts: Option<(&str, &str)>,
    theme: Option<maestro_ui::UiTheme>,
) {
    if area.is_empty() {
        return;
    }
    if area.width >= 44 && area.height >= 5 {
        let logo_height = welcome_logo_height(area.height);
        let logo = orb_logo_lines(logo_height, theme);
        let mut summary = [
            product_title_line(false).alignment(Alignment::Left),
            facts.map_or_else(
                || hint_line().alignment(Alignment::Left),
                |(runtime, _)| {
                    Line::styled(
                        runtime.to_string(),
                        Style::default().fg(Color::Rgb(
                            DEIXIC_MUTED.0,
                            DEIXIC_MUTED.1,
                            DEIXIC_MUTED.2,
                        )),
                    )
                },
            ),
            facts.map_or_else(
                || {
                    if let Some(id) = session_id {
                        return session_meta_line(id, ready).alignment(Alignment::Left);
                    }
                    Line::styled(
                        if ready {
                            maestro_ui::localization::tr("Ready when you are.")
                        } else {
                            maestro_ui::localization::tr("Getting ready…")
                        },
                        Style::default().fg(Color::Rgb(
                            DEIXIC_MUTED.0,
                            DEIXIC_MUTED.1,
                            DEIXIC_MUTED.2,
                        )),
                    )
                },
                |(_, location)| {
                    Line::styled(
                        location.to_string(),
                        Style::default().fg(Color::Rgb(
                            DEIXIC_MUTED.0,
                            DEIXIC_MUTED.1,
                            DEIXIC_MUTED.2,
                        )),
                    )
                },
            ),
        ];
        if let Some(theme) = theme {
            for (row, line) in summary.iter_mut().enumerate() {
                let color = if row == 0 { theme.text } else { theme.muted };
                line.style = line.style.fg(color);
                for span in &mut line.spans {
                    span.style = span.style.fg(color);
                }
            }
        }
        if ready && facts.is_some() && welcome_prompt_row(area) < area.bottom() {
            Paragraph::new(maestro_ui::localization::tr("Objective?"))
                .style(Style::default().fg(theme.map_or(
                    Color::Rgb(DEIXIC_MUTED.0, DEIXIC_MUTED.1, DEIXIC_MUTED.2),
                    |theme| theme.muted,
                )))
                .render(
                    Rect::new(
                        area.x + 3,
                        welcome_prompt_row(area),
                        area.width.saturating_sub(3),
                        1,
                    ),
                    buf,
                );
        }
        let logo_width = logo_visual_width(logo_height) + 3;
        for (row, line) in logo.into_iter().enumerate() {
            Paragraph::new(line).render(
                Rect::new(area.x + 1, area.y + 1 + row as u16, logo_width, 1),
                buf,
            );
        }
        for (row, line) in summary.into_iter().enumerate() {
            Paragraph::new(line).render(
                Rect::new(
                    area.x + logo_width + 1,
                    area.y + 1 + row as u16,
                    area.width.saturating_sub(logo_width + 2),
                    1,
                ),
                buf,
            );
        }
        return;
    }
    let mut content = welcome_content_lines_with_metadata(area.height, animate, session_id, ready);
    if let Some(theme) = theme {
        for line in &mut content {
            line.style = line.style.fg(theme.text);
            for span in &mut line.spans {
                span.style = span.style.fg(theme.text);
            }
        }
    }
    let content_height = content.len() as u16;
    let y_offset = if area.height > content_height {
        (area.height - content_height) / 2
    } else {
        0
    };
    let content_area = Rect::new(
        area.x,
        area.y + y_offset,
        area.width,
        content_height.min(area.height),
    );
    Paragraph::new(content)
        .alignment(Alignment::Center)
        .render(content_area, buf);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expressions_keep_sprite_dimensions_for_all_supported_eyes() {
        for height in [
            MICRO_MIN_HEIGHT,
            TINY_MIN_HEIGHT,
            COMPACT_MIN_HEIGHT,
            FULL_MIN_HEIGHT,
        ] {
            let original = logo_lines(pick_logo(height).unwrap());
            for eyes in [
                "• •", "¬ ¬", "• ?", "− −", "^ ^", "⠶ ⠶", "⠂ ⠕", "o-o", "• O", "o o", "^ −",
            ] {
                for line in &original {
                    let posed = portrait_expression(line, eyes);
                    assert_eq!(
                        posed.width(),
                        line.width(),
                        "height {height}, eyes {eyes}: {posed}"
                    );
                }
            }
        }
        assert_eq!(
            logo_lines(LOGO_FULL).len(),
            logo_lines(LOGO_FULL_WORKING).len()
        );
        for (idle, working) in logo_lines(LOGO_FULL)
            .iter()
            .zip(logo_lines(LOGO_FULL_WORKING))
        {
            assert_eq!(idle.width(), working.width());
        }
    }

    #[test]
    fn welcome_geometry_keeps_art_and_prompt_clear_of_summary_and_input() {
        for width in [44, 60, 100] {
            for height in 5..=30 {
                let area = Rect::new(2, 3, width, height);
                let mut buf = Buffer::empty(Rect::new(0, 0, 110, 40));
                render_welcome_with_summary(
                    area,
                    &mut buf,
                    false,
                    None,
                    true,
                    Some(("runtime", "workspace")),
                );
                let mark = crate::dex_delight::welcome_portrait_area(area).unwrap();
                assert!(mark.right() < area.right());
                assert!(mark.bottom() <= area.bottom());
                assert_eq!(mark.height, if height >= FULL_MIN_HEIGHT { 10 } else { 4 });
                let summary_row: String = (mark.right()..area.right())
                    .map(|x| buf[(x, area.y + 1)].symbol())
                    .collect();
                assert!(summary_row.contains(PRODUCT_TITLE));
                for y in area.bottom()..40 {
                    assert!((0..110).all(|x| buf[(x, y)].symbol() == " "));
                }
                let prompt_y = welcome_prompt_row(area);
                assert!(prompt_y > mark.bottom());
                if prompt_y < area.bottom() {
                    let row: String = (area.x..area.right())
                        .map(|x| buf[(x, prompt_y)].symbol())
                        .collect();
                    assert!(row.contains("Objective?"));
                }
            }
        }
    }

    #[test]
    fn welcome_summary_sits_beside_small_mark_at_top() {
        let area = Rect::new(0, 0, 100, 30);
        let mut buffer = Buffer::empty(area);
        render_welcome_with_metadata(area, &mut buffer, false, Some("session-1"), true);
        let row = |y| {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };
        assert!(row(1).contains(PRODUCT_TITLE));
        assert!(row(3).contains("session session-1"));
        let crown = logo_lines(LOGO_FULL)[0].trim();
        assert!(row(1).find(crown).unwrap() < row(1).find(PRODUCT_TITLE).unwrap());
        assert!(row(15).trim().is_empty());
    }

    #[test]
    fn logo_tiers_by_height() {
        assert!(pick_logo(MICRO_MIN_HEIGHT - 1).is_none());
        assert_eq!(pick_logo(MICRO_MIN_HEIGHT), Some(LOGO_MICRO));
        assert_eq!(pick_logo(TINY_MIN_HEIGHT - 1), Some(LOGO_MICRO));
        assert_eq!(pick_logo(TINY_MIN_HEIGHT), Some(LOGO_TINY));
        assert_eq!(pick_logo(COMPACT_MIN_HEIGHT), Some(LOGO_COMPACT));
        assert_eq!(pick_logo(FULL_MIN_HEIGHT), Some(LOGO_FULL));
    }

    #[test]
    fn full_logo_is_taller_than_compact() {
        assert!(logo_lines(LOGO_FULL).len() > logo_lines(LOGO_COMPACT).len());
        assert!(logo_lines(LOGO_COMPACT).len() > logo_lines(LOGO_TINY).len());
        assert!(logo_lines(LOGO_TINY).len() > logo_lines(LOGO_MICRO).len());
    }

    #[test]
    fn welcome_content_uses_dex_code_title_and_hint() {
        let lines = welcome_content_lines(24, false);
        let text = lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Dex Code"));
        assert!(!text.contains("Maestro"));
        assert!(text.contains(COMPOSER_HINT));
    }

    #[test]
    fn every_pose_preserves_row_widths() {
        for height in [
            MICRO_MIN_HEIGHT,
            TINY_MIN_HEIGHT,
            COMPACT_MIN_HEIGHT,
            FULL_MIN_HEIGHT,
        ] {
            let widths = |state| {
                logo_lines(pick_logo_for_state(height, state).unwrap())
                    .into_iter()
                    .map(UnicodeWidthStr::width)
                    .collect::<Vec<_>>()
            };
            assert_eq!(widths(LaunchState::Idle), widths(LaunchState::Working));
        }
    }

    #[test]
    fn full_mark_keeps_orb_eyes_and_nub() {
        assert_eq!(logo_lines(LOGO_FULL).len(), 10);
        assert!(LOGO_FULL.contains("  ●●  "));
        assert!(LOGO_FULL.contains("•••"));
        assert_eq!(logo_lines(LOGO_COMPACT).len(), 4);
    }

    #[test]
    fn full_welcome_includes_quiet_session_metadata() {
        let lines =
            welcome_content_lines_with_metadata(FULL_MIN_HEIGHT, false, Some("restore-42"), true);
        let text = lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("ready"));
        assert!(text.contains("session restore-42"));
        assert!(!text.contains("session 01"));
    }

    #[test]
    fn launch_motion_and_inspection_layer_follow_working_state() {
        let idle =
            welcome_content_lines_with_metadata(FULL_MIN_HEIGHT, true, Some("restore-42"), true);
        let working =
            welcome_content_lines_with_metadata(FULL_MIN_HEIGHT, true, Some("restore-42"), false);
        let idle_text = idle
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        let working_text = working
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");

        assert_eq!(logo_lines(LOGO_FULL), logo_lines(LOGO_FULL_WORKING));
        assert!(idle_text.contains("• ready"));
        assert!(working_text.contains("• working"));
    }

    #[test]
    fn convenience_welcome_keeps_orb_stable_during_work() {
        let idle = welcome_content_lines(FULL_MIN_HEIGHT, false)
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        let working = welcome_content_lines(FULL_MIN_HEIGHT, true)
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(idle.contains("·••●••·"));
        assert!(working.contains("·••●••·"));
    }

    #[test]
    fn welcome_without_session_hides_session_metadata() {
        let lines = welcome_content_lines(FULL_MIN_HEIGHT, false);
        let text = lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!text.contains("ready"));
        assert!(!text.contains("session "));
    }

    #[test]
    fn welcome_short_height_hides_logo_keeps_title() {
        let lines = welcome_content_lines(MICRO_MIN_HEIGHT - 1, false);
        let text = lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains(PRODUCT_TITLE));
        assert!(text.contains(COMPOSER_HINT));
    }

    #[test]
    fn welcome_micro_height_keeps_a_visible_mark() {
        let lines = welcome_content_lines(MICRO_MIN_HEIGHT, false);
        let text = lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("·•●•·"));
        assert!(text.contains(PRODUCT_TITLE));
        assert!(text.contains(COMPOSER_HINT));
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn logo_line_count_matches_art() {
        assert_eq!(
            logo_line_count(FULL_MIN_HEIGHT),
            logo_lines(LOGO_FULL).len() as u16
        );
        assert_eq!(logo_line_count(0), 0);
    }
}
