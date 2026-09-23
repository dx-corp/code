//! Stateless Dex presentation. Callers supply observed activity; visual preferences
//! never select activity, model identity, instructions, or execution behavior.

use ratatui::{prelude::*, widgets::Paragraph};

/// Activity observed by the caller, never inferred from animation or elapsed time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DexCompanionState {
    /// The session can accept a request.
    Ready,
    /// An accepted request is actively running.
    Working,
    /// A user response or approval is required.
    NeedsInput,
    /// Accepted work is waiting on an external prerequisite.
    Waiting,
    /// The current request completed successfully.
    Finished,
    /// The current request failed.
    Failed,
}

impl DexCompanionState {
    /// Plain state text remains visible even without color or animation.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Ready => maestro_ui::localization::tr("ready"),
            Self::Working => maestro_ui::localization::tr("working"),
            Self::NeedsInput => maestro_ui::localization::tr("needs input"),
            Self::Waiting => maestro_ui::localization::tr("waiting"),
            Self::Finished => maestro_ui::localization::tr("finished"),
            Self::Failed => maestro_ui::localization::tr("failed"),
        }
    }
}

/// Presentation intensity only; never changes prompts or capabilities.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum DexPersonality {
    /// Text only; always static.
    Quiet,
    /// The selected Dex accent and plain state text.
    #[default]
    Standard,
    /// Add a small state signal beside the same identity and text.
    Expressive,
}

/// A frame-local widget with stable Dex identity across models and activities.
#[derive(Debug, Clone, Copy)]
pub struct DexCompanion {
    state: DexCompanionState,
    personality: DexPersonality,
    animations: bool,
    frame: u64,
    look: crate::dex_delight::DexLook,
    theme: Option<maestro_ui::UiTheme>,
}

impl DexCompanion {
    /// Render only the state supplied by the activity owner. Motion defaults off.
    #[must_use]
    pub fn new(state: DexCompanionState) -> Self {
        Self {
            state,
            personality: DexPersonality::Standard,
            animations: false,
            frame: 0,
            look: Default::default(),
            theme: None,
        }
    }

    /// Select presentation intensity independently of activity.
    #[must_use]
    pub const fn personality(mut self, personality: DexPersonality) -> Self {
        self.personality = personality;
        self
    }

    /// Disable for reduced motion. Quiet presentation remains static regardless.
    #[must_use]
    pub const fn animations(mut self, enabled: bool) -> Self {
        self.animations = enabled;
        self
    }

    /// Supply the application's animation frame; this widget keeps no clock.
    #[must_use]
    pub const fn frame(mut self, frame: u64) -> Self {
        self.frame = frame;
        self
    }

    /// Apply cosmetics without altering the observed activity.
    pub const fn look(mut self, look: crate::dex_delight::DexLook) -> Self {
        self.look = look;
        self
    }

    /// Match an opaque application palette; absent palettes retain the chosen cosmetics.
    pub const fn theme(mut self, theme: Option<maestro_ui::UiTheme>) -> Self {
        self.theme = theme;
        self
    }

    fn accent(&self) -> Color {
        self.theme
            .map_or(self.look.accent.color(), |theme| theme.focus)
    }

    /// Six compact expressions; the explicit state label remains the authority.
    #[must_use]
    pub const fn face(&self) -> &'static str {
        match self.state {
            DexCompanionState::Ready => "⠸• •⠇",
            DexCompanionState::Working => "⠸¬ ¬⠇",
            DexCompanionState::NeedsInput => "⠸• ?⠇",
            DexCompanionState::Waiting => "⠸− −⠇",
            DexCompanionState::Finished => "⠸^ ^⠇",
            DexCompanionState::Failed => "⠸⠂ ⠕⠇",
        }
    }

    /// A single hop, never a loop. Frames are 100 ms since the observed transition.
    #[must_use]
    pub const fn hopping(&self) -> bool {
        self.animations
            && !matches!(self.personality, DexPersonality::Quiet)
            && matches!(self.state, DexCompanionState::Finished)
            && self.frame >= 2
            && self.frame < 6
    }

    /// Render a tiny portrait in a fixed two-row slot without moving status text.
    pub fn render_face(&self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() || self.personality == DexPersonality::Quiet {
            return;
        }
        let y = area.y + if self.hopping() { 0 } else { area.height - 1 };
        let motion = self.animations && self.personality != DexPersonality::Quiet;
        let style = Style::default().fg(self.accent());
        let face = Line::from(vec![
            Span::styled("⠸", style),
            Span::styled(
                self.look.eyes(self.state, motion),
                eye_style(self.accent(), self.theme, self.state),
            ),
            Span::styled(format!("⠇{}", self.look.prop()), style),
        ]);
        if !self.hopping() && area.height > 1 {
            Paragraph::new(self.look.cap())
                .style(style)
                .render(Rect::new(area.x, area.y, area.width, 1), buf);
        }
        Paragraph::new(face)
            .style(style)
            .render(Rect::new(area.x, y, area.width, 1), buf);
    }

    /// Compact, explicit activity line suitable for existing status surfaces.
    #[must_use]
    pub fn status_line(&self) -> Line<'static> {
        let mut spans = vec![Span::styled(
            "Dex",
            Style::default()
                .fg(self.accent())
                .add_modifier(Modifier::BOLD),
        )];
        if self.personality == DexPersonality::Expressive {
            let signal = match self.state {
                DexCompanionState::Working if self.animations => {
                    ["·", "•", "●", "•"][(self.frame / 4 % 4) as usize]
                }
                DexCompanionState::Working => "●",
                DexCompanionState::NeedsInput => "?",
                DexCompanionState::Waiting => "…",
                DexCompanionState::Finished => "✓",
                DexCompanionState::Failed => "!",
                DexCompanionState::Ready => "·",
            };
            spans.push(Span::raw(format!(" {signal}")));
        }
        spans.push(Span::raw(format!(" · {}", self.state.label())));
        let line = Line::from(spans);
        match self.theme {
            Some(theme) => line.style(theme.text_style()),
            None => line,
        }
    }
}

impl Widget for DexCompanion {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }
        let mut lines = if self.personality == DexPersonality::Quiet {
            Vec::new()
        } else {
            super::deixic_logo::orb_logo_lines(area.height.saturating_sub(1), self.theme)
        };
        // Center the sprite as one rectangle, not each differently sized row.
        let width = lines.iter().map(Line::width).max().unwrap_or(0);
        let inset = usize::from(area.width).saturating_sub(width) / 2;
        for line in &mut lines {
            line.spans.insert(0, Span::raw(" ".repeat(inset)));
        }
        // Preserve the state row even if the shared mark gains a taller tier.
        lines.truncate(usize::from(area.height.saturating_sub(1)));
        lines.push(self.status_line().alignment(Alignment::Center));
        Paragraph::new(lines)
            .alignment(Alignment::Left)
            .render(area, buf);
    }
}

fn eye_style(accent: Color, theme: Option<maestro_ui::UiTheme>, state: DexCompanionState) -> Style {
    let style = Style::default().fg(accent);
    match state {
        DexCompanionState::Ready | DexCompanionState::Waiting | DexCompanionState::Finished => {
            theme.map_or(
                style.add_modifier(Modifier::DIM),
                maestro_ui::UiTheme::resting_focus_style,
            )
        }
        DexCompanionState::Failed => theme.map_or(style, |theme| style.fg(theme.error)),
        DexCompanionState::NeedsInput => theme.map_or(style, |theme| style.fg(theme.attention)),
        DexCompanionState::Working => style,
    }
}

/// Apply the production startup portrait to the shared compact welcome mark.
pub fn render_welcome_portrait(
    area: Rect,
    buf: &mut Buffer,
    look: crate::dex_delight::DexLook,
    state: DexCompanionState,
    animations: bool,
) {
    render_welcome_portrait_with_theme(area, buf, look, state, animations, None);
}

/// Draw the welcome portrait using the same palette as the surrounding mark.
pub fn render_welcome_portrait_with_theme(
    area: Rect,
    buf: &mut Buffer,
    _look: crate::dex_delight::DexLook,
    _state: DexCompanionState,
    _animations: bool,
    theme: Option<maestro_ui::UiTheme>,
) {
    if let Some(mark) = crate::dex_delight::welcome_portrait_area(area) {
        let height = super::deixic_logo::welcome_logo_height(area.height);
        for (row, line) in super::deixic_logo::orb_logo_lines(height, theme)
            .into_iter()
            .enumerate()
        {
            Paragraph::new(line).render(Rect::new(mark.x, mark.y + row as u16, mark.width, 1), buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eye_signal_follows_observed_state_without_motion() {
        for surface in [Color::Rgb(0, 0, 0), Color::Rgb(255, 255, 255)] {
            let theme = maestro_ui::UiTheme {
                focus: Color::Rgb(100, 120, 200),
                surface,
                ..Default::default()
            };
            let resting = eye_style(theme.focus, Some(theme), DexCompanionState::Ready);
            let active = eye_style(theme.focus, Some(theme), DexCompanionState::Working);
            assert_ne!(resting.fg, active.fg);
            assert_eq!(active.fg, Some(theme.focus));
            for state in [
                DexCompanionState::Ready,
                DexCompanionState::Working,
                DexCompanionState::Waiting,
                DexCompanionState::Failed,
                DexCompanionState::NeedsInput,
            ] {
                let area = Rect::new(0, 0, 8, 2);
                let render = |frame| {
                    let mut buf = Buffer::empty(area);
                    DexCompanion::new(state)
                        .theme(Some(theme))
                        .frame(frame)
                        .render_face(area, &mut buf);
                    buf
                };
                assert_eq!(render(0), render(99));
                assert_eq!(
                    render(0)[(1, 1)].fg,
                    eye_style(theme.focus, Some(theme), state).fg.unwrap()
                );
            }
            assert_eq!(
                eye_style(theme.focus, Some(theme), DexCompanionState::Failed).fg,
                Some(theme.error)
            );
            assert_eq!(
                eye_style(theme.focus, Some(theme), DexCompanionState::NeedsInput).fg,
                Some(theme.attention)
            );
        }
        assert!(
            eye_style(Color::Cyan, None, DexCompanionState::Ready)
                .add_modifier
                .contains(Modifier::DIM)
        );
        assert!(
            !eye_style(Color::Cyan, None, DexCompanionState::Working)
                .add_modifier
                .contains(Modifier::DIM)
        );
    }

    #[test]
    fn centered_portrait_keeps_a_shared_origin_for_every_row() {
        for height in [11, 18] {
            let area = Rect::new(2, 3, 40, height);
            let mut buf = Buffer::empty(area);
            DexCompanion::new(DexCompanionState::Ready).render(area, &mut buf);
            let lines = super::super::deixic_logo::static_logo_lines(height - 1);
            let width = lines.iter().map(Line::width).max().unwrap();
            let origin = area.x + (area.width - width as u16) / 2;
            for (row, line) in lines.iter().enumerate() {
                for (col, ch) in line.to_string().chars().enumerate() {
                    if ch != ' ' {
                        assert_eq!(
                            buf[(origin + col as u16, area.y + row as u16)].symbol(),
                            ch.to_string(),
                            "height {height}, row {row}, col {col}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn orb_has_distinct_contrast_in_light_and_dark_themes() {
        for surface in [Color::Rgb(20, 20, 30), Color::Rgb(255, 255, 255)] {
            let theme = maestro_ui::UiTheme {
                surface,
                ..Default::default()
            };
            let lines = super::super::deixic_logo::orb_logo_lines(14, Some(theme));
            assert_eq!(lines.len(), 10);
            assert!(
                lines
                    .iter()
                    .flat_map(|line| &line.spans)
                    .any(|span| { span.content.contains('●') && span.style.fg.is_some() })
            );
            assert!(
                lines
                    .iter()
                    .flat_map(|line| &line.spans)
                    .any(|span| { matches!(span.style.fg, Some(Color::Rgb(_, g, b)) if b > g) })
            );
        }
    }

    #[test]
    fn welcome_portrait_uses_the_same_orb_as_the_startup_block() {
        let theme = maestro_ui::UiTheme::default();
        let area = Rect::new(0, 0, 100, 30);
        let mut buf = Buffer::empty(area);
        render_welcome_portrait_with_theme(
            area,
            &mut buf,
            Default::default(),
            DexCompanionState::Ready,
            false,
            Some(theme),
        );
        let mark = crate::dex_delight::welcome_portrait_area(area).unwrap();
        let lines = super::super::deixic_logo::orb_logo_lines(14, Some(theme));
        for (row, line) in lines.iter().enumerate() {
            for (col, ch) in line.to_string().chars().enumerate() {
                if ch != ' ' {
                    assert_eq!(
                        buf[(mark.x + col as u16, mark.y + row as u16)].symbol(),
                        ch.to_string()
                    );
                }
            }
        }
    }

    #[test]
    fn status_keeps_selected_accent_while_orb_keeps_brand_palette() {
        use crate::dex_delight::{DexAccent, DexLook};
        for accent in [
            DexAccent::Violet,
            DexAccent::Mint,
            DexAccent::Amber,
            DexAccent::Rose,
        ] {
            let companion = DexCompanion::new(DexCompanionState::Ready).look(DexLook {
                accent,
                ..DexLook::default()
            });
            assert_eq!(
                companion.status_line().spans[0].style.fg,
                Some(accent.color())
            );
            let area = Rect::new(0, 0, 30, 14);
            let mut buf = Buffer::empty(area);
            companion.render(area, &mut buf);
            assert!((0..area.width).any(|x| buf[(x, 0)].symbol() != " "));
        }
    }

    #[test]
    fn poses_are_distinct_and_hop_is_bounded_and_optional() {
        let states = [
            DexCompanionState::Ready,
            DexCompanionState::Working,
            DexCompanionState::NeedsInput,
            DexCompanionState::Waiting,
            DexCompanionState::Finished,
            DexCompanionState::Failed,
        ];
        let faces: std::collections::HashSet<_> = states
            .into_iter()
            .map(|state| DexCompanion::new(state).face())
            .collect();
        assert_eq!(faces.len(), 6);
        for personality in [
            DexPersonality::Quiet,
            DexPersonality::Standard,
            DexPersonality::Expressive,
        ] {
            for state in states {
                for motion in [false, true] {
                    for frame in [0, 2, 5, 6, 10_000] {
                        let dex = DexCompanion::new(state)
                            .personality(personality)
                            .animations(motion)
                            .frame(frame);
                        assert_eq!(
                            dex.hopping(),
                            motion
                                && personality != DexPersonality::Quiet
                                && state == DexCompanionState::Finished
                                && (2..6).contains(&frame)
                        );
                        let area = Rect::new(0, 0, 5, 2);
                        let mut buf = Buffer::empty(area);
                        dex.render_face(area, &mut buf);
                        if personality != DexPersonality::Quiet {
                            let row = u16::from(!dex.hopping());
                            let text: String = (0..5).map(|x| buf[(x, row)].symbol()).collect();
                            assert_eq!(text, dex.face());
                        } else {
                            assert!(buf.content.iter().all(|cell| cell.symbol() == " "));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn all_states_remain_explicit_across_personality_and_motion() {
        for state in [
            DexCompanionState::Ready,
            DexCompanionState::Working,
            DexCompanionState::NeedsInput,
            DexCompanionState::Waiting,
            DexCompanionState::Finished,
            DexCompanionState::Failed,
        ] {
            for personality in [
                DexPersonality::Quiet,
                DexPersonality::Standard,
                DexPersonality::Expressive,
            ] {
                for animate in [false, true] {
                    let text = DexCompanion::new(state)
                        .personality(personality)
                        .animations(animate)
                        .frame(8)
                        .status_line()
                        .to_string();
                    assert!(text.starts_with("Dex"));
                    assert!(text.ends_with(state.label()));
                }
            }
        }
    }

    #[test]
    fn reduced_motion_and_quiet_are_frame_independent() {
        let expressive =
            DexCompanion::new(DexCompanionState::Working).personality(DexPersonality::Expressive);
        assert_eq!(
            expressive.frame(0).status_line(),
            expressive.frame(8).status_line()
        );
        let quiet = expressive
            .personality(DexPersonality::Quiet)
            .animations(true);
        assert_eq!(quiet.frame(0).status_line(), quiet.frame(8).status_line());
        let animated = expressive.animations(true);
        assert_ne!(
            animated.frame(0).status_line(),
            animated.frame(8).status_line()
        );
        let ready = DexCompanion::new(DexCompanionState::Ready)
            .personality(DexPersonality::Expressive)
            .animations(true);
        assert_eq!(ready.frame(0).status_line(), ready.frame(8).status_line());
    }

    #[test]
    fn widget_keeps_state_readable_in_a_single_row() {
        let area = Rect::new(0, 0, 32, 1);
        let mut buf = Buffer::empty(area);
        DexCompanion::new(DexCompanionState::NeedsInput).render(area, &mut buf);
        let text: String = (0..area.width).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(text.contains("Dex · needs input"));
    }

    #[test]
    fn short_companion_preserves_state_at_every_logo_tier() {
        for height in 1..=14 {
            let area = Rect::new(0, 0, 32, height);
            let mut buf = Buffer::empty(area);
            DexCompanion::new(DexCompanionState::NeedsInput).render(area, &mut buf);
            let text: String = buf.content.iter().map(|cell| cell.symbol()).collect();
            assert!(
                text.contains("Dex · needs input"),
                "height {height}: {text}"
            );
        }
    }

    #[test]
    fn widget_uses_existing_mark_and_state_without_model_identity() {
        let area = Rect::new(0, 0, 40, 14);
        let mut buf = Buffer::empty(area);
        DexCompanion::new(DexCompanionState::Failed).render(area, &mut buf);
        let text: String = buf.content.iter().map(|cell| cell.symbol()).collect();
        let mark = super::super::deixic_logo::static_logo_lines(area.height.saturating_sub(1));
        assert!(!mark.is_empty());
        assert!(text.contains("·••●••·"));
        assert!(text.contains("Dex · failed"));
    }
}

#[cfg(test)]
mod delight_render_tests {
    use super::*;
    use crate::dex_delight::{DexAccent, DexAccessory, DexActivity, DexLook};

    #[test]
    fn cosmetics_and_reactions_keep_fixed_bounds_and_quiet_is_empty() {
        let area = Rect::new(0, 0, 6, 2);
        let look = DexLook {
            accessory: DexAccessory::Beanie,
            accent: DexAccent::Mint,
            activity: DexActivity::Running,
            pet_frame: Some(4),
        };
        let mut buffer = Buffer::empty(area);
        DexCompanion::new(DexCompanionState::Working)
            .look(look)
            .animations(true)
            .render_face(area, &mut buffer);
        let text: String = buffer.content.iter().map(|c| c.symbol()).collect();
        assert!(text.contains("╭─●─╮"));
        assert!(text.contains("^ ^"));
        assert!(text.contains('▤'));
        assert_eq!(buffer[(0, 1)].fg, DexAccent::Mint.color());
        let mut quiet = Buffer::empty(area);
        DexCompanion::new(DexCompanionState::Working)
            .look(look)
            .personality(DexPersonality::Quiet)
            .render_face(area, &mut quiet);
        assert!(quiet.content.iter().all(|c| c.symbol() == " "));
    }
}
