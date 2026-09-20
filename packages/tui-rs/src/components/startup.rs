//! First-launch identity. Artwork is presentation only; no progress is inferred from time.
use maestro_ui::UiTheme;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, Clear, Paragraph},
};

// Frames are authored offline; startup only selects an embedded string.
macro_rules! frames_for {
    ($size:literal) => {
        [
            include_str!(concat!("../../frames/deixic/", $size, "/frame_01.txt")),
            include_str!(concat!("../../frames/deixic/", $size, "/frame_02.txt")),
            include_str!(concat!("../../frames/deixic/", $size, "/frame_03.txt")),
            include_str!(concat!("../../frames/deixic/", $size, "/frame_04.txt")),
            include_str!(concat!("../../frames/deixic/", $size, "/frame_05.txt")),
            include_str!(concat!("../../frames/deixic/", $size, "/frame_06.txt")),
            include_str!(concat!("../../frames/deixic/", $size, "/frame_07.txt")),
            include_str!(concat!("../../frames/deixic/", $size, "/frame_08.txt")),
            include_str!(concat!("../../frames/deixic/", $size, "/frame_09.txt")),
            include_str!(concat!("../../frames/deixic/", $size, "/frame_10.txt")),
            include_str!(concat!("../../frames/deixic/", $size, "/frame_11.txt")),
            include_str!(concat!("../../frames/deixic/", $size, "/frame_12.txt")),
            include_str!(concat!("../../frames/deixic/", $size, "/frame_13.txt")),
            include_str!(concat!("../../frames/deixic/", $size, "/frame_14.txt")),
            include_str!(concat!("../../frames/deixic/", $size, "/frame_15.txt")),
            include_str!(concat!("../../frames/deixic/", $size, "/frame_16.txt")),
            include_str!(concat!("../../frames/deixic/", $size, "/frame_17.txt")),
            include_str!(concat!("../../frames/deixic/", $size, "/frame_18.txt")),
            include_str!(concat!("../../frames/deixic/", $size, "/frame_19.txt")),
            include_str!(concat!("../../frames/deixic/", $size, "/frame_20.txt")),
            include_str!(concat!("../../frames/deixic/", $size, "/frame_21.txt")),
            include_str!(concat!("../../frames/deixic/", $size, "/frame_22.txt")),
            include_str!(concat!("../../frames/deixic/", $size, "/frame_23.txt")),
            include_str!(concat!("../../frames/deixic/", $size, "/frame_24.txt")),
            include_str!(concat!("../../frames/deixic/", $size, "/frame_25.txt")),
            include_str!(concat!("../../frames/deixic/", $size, "/frame_26.txt")),
            include_str!(concat!("../../frames/deixic/", $size, "/frame_27.txt")),
            include_str!(concat!("../../frames/deixic/", $size, "/frame_28.txt")),
            include_str!(concat!("../../frames/deixic/", $size, "/frame_29.txt")),
            include_str!(concat!("../../frames/deixic/", $size, "/frame_30.txt")),
            include_str!(concat!("../../frames/deixic/", $size, "/frame_31.txt")),
            include_str!(concat!("../../frames/deixic/", $size, "/frame_32.txt")),
            include_str!(concat!("../../frames/deixic/", $size, "/frame_33.txt")),
            include_str!(concat!("../../frames/deixic/", $size, "/frame_34.txt")),
            include_str!(concat!("../../frames/deixic/", $size, "/frame_35.txt")),
            include_str!(concat!("../../frames/deixic/", $size, "/frame_36.txt")),
        ]
    };
}
const FRAMES: [&str; 36] = frames_for!("normal");
const COMPACT_FRAMES: [&str; 36] = frames_for!("compact");

/// Embedded, shaded Deixic aperture; one frame per 80 ms. `None` is still artwork.
pub fn render_aperture(frame: &mut Frame, area: Rect, theme: UiTheme, tick: Option<u64>) {
    let (frames, width, height) = if area.width >= 48 && area.height >= 18 {
        (&FRAMES, 48, 18)
    } else if area.width >= 40 && area.height >= 12 {
        (&COMPACT_FRAMES, 40, 12)
    } else {
        return;
    };
    let index = tick.map_or(0, |t| (t % frames.len() as u64) as usize);
    frame.render_widget(
        Paragraph::new(frames[index]).style(Style::default().fg(theme.focus)),
        Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        ),
    );
}

/// Paint the first-run preparation screen without claiming account or model readiness.
pub fn render_startup(frame: &mut Frame, area: Rect, theme: UiTheme, tick: Option<u64>) {
    frame.render_widget(Clear, area);
    frame.render_widget(Block::default().style(theme.text_style()), area);
    let width = area.width.min(64);
    let x = area.x + (area.width - width) / 2;
    let top = area.y + area.height.saturating_sub(26) / 2;
    let mut lines = vec![
        Line::styled(
            "D E I X I C",
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ),
        Line::from(""),
        Line::styled(
            maestro_ui::localization::tr("Preparing workspace search"),
            Style::default().fg(theme.muted),
        ),
        Line::from(""),
        Line::styled(
            maestro_ui::localization::tr("Enter / Esc  continue    Ctrl+C  quit"),
            Style::default().fg(theme.muted),
        ),
    ];
    let label_y = if area.height >= 22 && width >= 40 {
        let art_height = if area.height >= 28 && width >= 48 {
            18
        } else {
            12
        };
        render_aperture(frame, Rect::new(x, top, width, art_height), theme, tick);
        top + art_height + 1
    } else {
        lines.remove(1);
        area.y
    };
    frame.render_widget(
        Paragraph::new(lines).centered(),
        Rect::new(x, label_y, width, area.bottom().saturating_sub(label_y)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    fn render(tick: Option<u64>, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|f| render_startup(f, f.area(), UiTheme::default(), tick))
            .unwrap();
        terminal.backend().buffer().clone()
    }
    #[test]
    fn embedded_frames_have_fixed_ascii_footprints() {
        for (frames, width, height) in [(&FRAMES, 48, 18), (&COMPACT_FRAMES, 40, 12)] {
            for frame in frames {
                assert!(frame.is_ascii());
                assert_eq!(frame.lines().count(), height);
                assert!(frame.lines().all(|line| line.len() <= width));
            }
            assert!(frames.windows(2).any(|pair| pair[0] != pair[1]));
        }
    }

    #[test]
    fn startup_motion_is_optional_and_never_claims_readiness() {
        assert_ne!(render(Some(0), 80, 30), render(Some(8), 80, 30));
        assert_eq!(render(None, 80, 30), render(None, 80, 30));
        let still = render(None, 80, 30);
        let text: String = still.content.iter().map(|c| c.symbol()).collect();
        assert!(text.contains("Preparing workspace search"));
        assert!(text.contains("Ctrl+C"));
        assert!(!text.contains("Ready"));
        for (w, h) in [(1, 1), (24, 8), (60, 20), (120, 40)] {
            render(Some(3), w, h);
        }
    }
}
