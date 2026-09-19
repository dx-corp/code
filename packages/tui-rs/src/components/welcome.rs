//! Welcome/Onboarding Screen Component
//!
//! Empty-session and onboarding chrome. Brand art is the Dex-derived mark
//! ([`super::deixic_logo`]); the launch title is Dex Code.
//!
//! # Example
//!
//! ```rust,ignore
//! use maestro_tui::components::WelcomeScreen;
//!
//! let welcome = WelcomeScreen::new()
//!     .with_version("0.1.0")
//!     .with_model("claude-sonnet-4-20250514")
//!     .animations(true);
//!
//! welcome.render(frame, area);
//! ```

use ratatui::{
    prelude::*,
    widgets::{Clear, Paragraph, Widget},
};

/// Welcome screen widget
#[derive(Debug, Clone)]
pub struct WelcomeScreen {
    /// Whether Deixic sheen animations are enabled
    animations_enabled: bool,
    /// Presentation intensity; independent of model and activity.
    personality: super::dex_companion::DexPersonality,
    /// Application version
    version: Option<String>,
    session_id: Option<String>,
    summary: Option<(String, String)>,
    /// Current model name
    model: Option<String>,
    /// Custom welcome message (replaces product title)
    welcome_message: Option<String>,
    /// Show keyboard hints (default hint is always present on the brand block)
    show_hints: bool,
}

impl Default for WelcomeScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl WelcomeScreen {
    /// Create a new welcome screen
    #[must_use]
    pub fn new() -> Self {
        Self {
            animations_enabled: false,
            personality: super::dex_companion::DexPersonality::default(),
            version: None,
            session_id: None,
            summary: None,
            model: None,
            welcome_message: None,
            show_hints: false,
        }
    }

    /// Display the existing session identity without inventing a placeholder.
    #[must_use]
    pub fn with_session(mut self, session_id: Option<String>) -> Self {
        self.session_id = session_id;
        self
    }

    /// Preserve the current runtime and workspace facts on compact presentations.
    #[must_use]
    pub fn with_summary(mut self, runtime: String, location: String) -> Self {
        self.summary = Some((runtime, location));
        self
    }

    /// Set the version string
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }

    /// Set the current model
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Set custom welcome message
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.welcome_message = Some(message.into());
        self
    }

    /// Enable/disable Deixic sheen animations
    #[must_use]
    pub fn animations(mut self, enabled: bool) -> Self {
        self.animations_enabled = enabled;
        self
    }

    /// Set Dex presentation intensity without changing agent behavior.
    #[must_use]
    pub fn personality(mut self, personality: super::dex_companion::DexPersonality) -> Self {
        self.personality = personality;
        self
    }

    /// Enable/disable keyboard hints
    #[must_use]
    pub fn show_hints(mut self, show: bool) -> Self {
        self.show_hints = show;
        self
    }

    /// Build the content lines
    fn build_content(&self, area: Rect) -> Vec<Line<'static>> {
        if self.personality == super::dex_companion::DexPersonality::Quiet {
            let theme = crate::themes::current_ui_theme();
            let mut lines = vec![Line::styled(
                self.welcome_message
                    .as_deref()
                    .unwrap_or(super::deixic_logo::PRODUCT_TITLE)
                    .to_owned(),
                Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
            )];
            for (label, value) in [("version", &self.version), ("model", &self.model)] {
                if let Some(value) = value {
                    lines.push(Line::styled(
                        format!("{label} {value}"),
                        Style::default().fg(theme.muted),
                    ));
                }
            }
            if let Some((runtime, location)) = &self.summary {
                lines.push(Line::styled(
                    format!("{runtime} · {location}"),
                    Style::default().fg(theme.muted),
                ));
            }
            return lines;
        }
        // Brand block: Dex mark + Dex Code title.
        // Custom welcome_message replaces only the product title line.
        // Reserve rows for optional onboarding metadata before selecting the
        // logo tier. Otherwise a 17-row area picks the full logo and clips the
        // version/model rows appended below it.
        let reserved_rows = u16::from(self.version.is_some())
            + u16::from(self.model.is_some())
            + u16::from(self.session_id.is_some())
            + 1;
        let brand_height = area.height.saturating_sub(reserved_rows);
        let mut lines = super::deixic_logo::welcome_content_lines(brand_height, false);
        // An empty session is ready, even when decorative motion is enabled.
        lines.push(
            super::dex_companion::DexCompanion::new(super::dex_companion::DexCompanionState::Ready)
                .personality(self.personality)
                .theme(
                    crate::themes::current_theme()
                        .canvas_style()
                        .bg
                        .map(|_| crate::themes::current_ui_theme()),
                )
                .status_line(),
        );

        if let Some(ref custom) = self.welcome_message {
            for line in &mut lines {
                if line.to_string() == super::deixic_logo::PRODUCT_TITLE {
                    *line = if self.animations_enabled
                        && self.personality != super::dex_companion::DexPersonality::Quiet
                    {
                        Line::from(crate::shimmer::shimmer_spans(custom))
                            .alignment(Alignment::Center)
                    } else {
                        Line::from(Span::styled(
                            custom.clone(),
                            Style::default()
                                .fg(Color::White)
                                .add_modifier(Modifier::BOLD),
                        ))
                        .alignment(Alignment::Center)
                    };
                    break;
                }
            }
        }

        if let Some(ref version) = self.version {
            lines.push(Line::from(vec![
                Span::raw(maestro_ui::localization::tr("version ")),
                Span::styled(version.clone(), Style::default().fg(Color::DarkGray)),
            ]));
        }

        if let Some(ref model) = self.model {
            lines.push(Line::from(vec![
                Span::raw(maestro_ui::localization::tr("model ")),
                Span::styled(model.clone(), Style::default().fg(Color::DarkGray)),
            ]));
        }

        if let Some((runtime, location)) = &self.summary {
            lines.push(Line::from(runtime.clone()));
            lines.push(Line::from(location.clone()));
        }
        if let Some(session_id) = &self.session_id {
            lines.push(Line::from(maestro_ui::localization::format(
                "session {0}",
                std::slice::from_ref(session_id),
            )));
        }

        let _ = self.show_hints;

        lines
    }
}

impl Widget for WelcomeScreen {
    fn render(self, area: Rect, buf: &mut Buffer) {
        Clear.render(area, buf);
        buf.set_style(area, crate::themes::current_theme().canvas_style());

        let quiet = self.personality == super::dex_companion::DexPersonality::Quiet;
        let width = area.width.saturating_sub(if quiet { 2 } else { 0 });
        let content =
            crate::wrapping::word_wrap_lines(&self.build_content(area), usize::from(width.max(1)));
        let content_height = content.len().min(usize::from(area.height)) as u16;
        let paragraph = Paragraph::new(content).alignment(if quiet {
            Alignment::Left
        } else {
            Alignment::Center
        });
        let y_offset = if quiet {
            1.min(area.height.saturating_sub(content_height))
        } else if area.height > content_height {
            (area.height - content_height) / 2
        } else {
            0
        };

        let content_area = Rect::new(
            area.x + if quiet { 2.min(area.width) } else { 0 },
            area.y + y_offset,
            area.width.saturating_sub(if quiet { 2 } else { 0 }),
            content_height.min(area.height),
        );

        paragraph.render(content_area, buf);
    }
}

/// Onboarding step state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnboardingStep {
    /// Welcome screen
    Welcome,
    /// Authentication
    Auth,
    /// Directory trust
    TrustDirectory,
    /// Configuration
    Configure,
    /// Complete
    Complete,
}

impl OnboardingStep {
    /// Get the next step
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Self::Welcome => Self::Auth,
            Self::Auth => Self::TrustDirectory,
            Self::TrustDirectory => Self::Configure,
            Self::Configure => Self::Complete,
            Self::Complete => Self::Complete,
        }
    }

    /// Get the previous step
    #[must_use]
    pub fn prev(self) -> Self {
        match self {
            Self::Welcome => Self::Welcome,
            Self::Auth => Self::Welcome,
            Self::TrustDirectory => Self::Auth,
            Self::Configure => Self::TrustDirectory,
            Self::Complete => Self::Configure,
        }
    }

    /// Get step index (0-based)
    #[must_use]
    pub fn index(self) -> usize {
        match self {
            Self::Welcome => 0,
            Self::Auth => 1,
            Self::TrustDirectory => 2,
            Self::Configure => 3,
            Self::Complete => 4,
        }
    }

    /// Get total number of steps
    #[must_use]
    pub fn total() -> usize {
        5
    }

    /// Get step label
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Welcome => maestro_ui::localization::tr("Welcome"),
            Self::Auth => maestro_ui::localization::tr("Authentication"),
            Self::TrustDirectory => maestro_ui::localization::tr("Trust Directory"),
            Self::Configure => maestro_ui::localization::tr("Configuration"),
            Self::Complete => maestro_ui::localization::tr("Complete"),
        }
    }
}

/// Onboarding flow manager
#[derive(Debug)]
pub struct OnboardingFlow {
    /// Current step
    pub current_step: OnboardingStep,
    /// Whether auth is complete
    pub auth_complete: bool,
    /// Whether directory is trusted
    pub directory_trusted: bool,
    /// Welcome screen instance
    pub welcome: WelcomeScreen,
}

impl Default for OnboardingFlow {
    fn default() -> Self {
        Self::new()
    }
}

impl OnboardingFlow {
    /// Create a new onboarding flow
    #[must_use]
    pub fn new() -> Self {
        Self {
            current_step: OnboardingStep::Welcome,
            auth_complete: false,
            directory_trusted: false,
            welcome: WelcomeScreen::new(),
        }
    }

    /// Advance to next step
    pub fn next(&mut self) {
        self.current_step = self.current_step.next();
    }

    /// Go to previous step
    pub fn prev(&mut self) {
        self.current_step = self.current_step.prev();
    }

    /// Skip to end
    pub fn skip(&mut self) {
        self.current_step = OnboardingStep::Complete;
    }

    /// Check if complete
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.current_step == OnboardingStep::Complete
    }

    /// Get progress percentage
    #[must_use]
    pub fn progress(&self) -> f64 {
        (self.current_step.index() as f64 / (OnboardingStep::total() - 1) as f64) * 100.0
    }
}

/// Simple splash screen for quick display
#[derive(Debug, Clone)]
pub struct SplashScreen {
    /// Title text
    pub title: String,
    /// Subtitle text
    pub subtitle: Option<String>,
    /// When true, prepend the Deixic Dex ghost mark
    pub show_logo: bool,
}

impl Default for SplashScreen {
    fn default() -> Self {
        Self {
            title: super::deixic_logo::PRODUCT_TITLE.to_owned(),
            subtitle: None,
            show_logo: false,
        }
    }
}

impl SplashScreen {
    /// Create a new splash screen
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            ..Default::default()
        }
    }

    /// Set subtitle
    pub fn with_subtitle(mut self, subtitle: impl Into<String>) -> Self {
        self.subtitle = Some(subtitle.into());
        self
    }

    /// Show/hide the Deixic ghost mark
    #[must_use]
    pub fn show_logo(mut self, show: bool) -> Self {
        self.show_logo = show;
        self
    }
}

impl Widget for SplashScreen {
    fn render(self, area: Rect, buf: &mut Buffer) {
        Clear.render(area, buf);

        let mut lines: Vec<Line<'static>> = Vec::new();

        if self.show_logo {
            for mut line in super::deixic_logo::static_logo_lines(area.height) {
                line.alignment = Some(Alignment::Center);
                lines.push(line);
            }
            if !lines.is_empty() {
                lines.push(Line::from(""));
            }
        }

        lines.push(Line::from(Span::styled(
            self.title.clone(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )));

        if let Some(subtitle) = self.subtitle {
            lines.push(Line::from(Span::styled(
                subtitle,
                Style::default().fg(Color::DarkGray),
            )));
        }

        let content_height = lines.len() as u16;
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

        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .render(content_area, buf);
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn test_welcome_screen_default() {
        let welcome = WelcomeScreen::new();
        assert!(!welcome.animations_enabled);
        assert!(!welcome.show_hints);
    }

    #[test]
    fn test_welcome_screen_builder() {
        let welcome = WelcomeScreen::new()
            .with_version("1.0.0")
            .with_model("claude-sonnet")
            .with_message("Hello!")
            .animations(false);

        assert_eq!(welcome.version.as_deref(), Some("1.0.0"));
        assert_eq!(welcome.model.as_deref(), Some("claude-sonnet"));
        assert_eq!(welcome.welcome_message.as_deref(), Some("Hello!"));
        assert!(!welcome.animations_enabled);
    }

    #[test]
    fn test_welcome_screen_uses_deixic_code_title() {
        let welcome = WelcomeScreen::new().animations(false);
        let lines = welcome.build_content(Rect::new(0, 0, 80, 24));
        let rendered = lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains(crate::components::deixic_logo::PRODUCT_TITLE));
        assert!(!rendered.contains("Maestro"));
        assert!(rendered.contains(crate::components::deixic_logo::COMPOSER_HINT));
        assert!(!rendered.contains("Welcome to Composer"));
        assert!(!rendered.contains("Getting Started"));
    }

    #[test]
    fn test_welcome_screen_reserves_rows_for_optional_metadata() {
        let welcome = WelcomeScreen::new()
            .with_version("1.0.0")
            .with_model("claude-sonnet");
        let lines = welcome.build_content(Rect::new(0, 0, 80, 17));

        assert!(lines.len() <= 17);
        assert_eq!(
            lines.last().map(Line::to_string).as_deref(),
            Some("model claude-sonnet")
        );
    }

    #[test]
    fn welcome_animation_never_claims_work_and_model_does_not_change_dex() {
        for model in ["model-a", "model-b"] {
            let lines = WelcomeScreen::new()
                .with_model(model)
                .animations(true)
                .build_content(Rect::new(0, 0, 80, 24));
            let text = lines
                .iter()
                .map(Line::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(text.contains("Dex · ready"));
            assert!(!text.contains("working"));
            assert!(!text.contains("· · ·"));
            assert!(text.contains(model));
        }
    }

    #[test]
    fn quiet_welcome_keeps_identity_without_art_or_redundant_status() {
        let lines = WelcomeScreen::new()
            .personality(super::super::dex_companion::DexPersonality::Quiet)
            .animations(true)
            .build_content(Rect::new(0, 0, 80, 24));
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0].to_string(),
            super::super::deixic_logo::PRODUCT_TITLE
        );
        assert!(!lines.iter().any(|line| line.to_string().contains("ready")));
    }

    fn rendered_welcome(welcome: WelcomeScreen, area: Rect) -> String {
        let mut buf = Buffer::empty(area);
        welcome.render(area, &mut buf);
        (area.y..area.bottom())
            .map(|y| {
                (area.x..area.right())
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn quiet_narrow_welcome_preserves_wrapped_project_context() {
        let rendered = rendered_welcome(
            WelcomeScreen::new()
                .personality(super::super::dex_companion::DexPersonality::Quiet)
                .with_summary("GPT-6 Astra".into(), "a-long-project-directory".into()),
            Rect::new(0, 0, 32, 12),
        );
        let normalized = rendered.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(normalized.contains("GPT-6 Astra"));
        let compact: String = rendered.chars().filter(|ch| !ch.is_whitespace()).collect();
        assert!(compact.contains("a-long-project-directory"), "{rendered}");
        assert!(!rendered.contains("Dex · ready"));
    }

    #[test]
    fn narrow_welcome_reserves_wrapped_custom_title_and_model_rows() {
        let rendered = rendered_welcome(
            WelcomeScreen::new()
                .personality(super::super::dex_companion::DexPersonality::Quiet)
                .with_message("A long custom welcome title that spans several terminal rows")
                .with_version("1.0.0")
                .with_model("a-long-model-name-that-needs-a-second-row"),
            Rect::new(0, 0, 32, 16),
        );
        assert!(!rendered.contains("Dex · ready"));
        assert!(rendered.contains("version 1.0.0"));
        let compact: String = rendered.chars().filter(|ch| !ch.is_whitespace()).collect();
        assert!(compact.contains("modela-long-model-name-that-needs-a-second-row"));
        assert!(compact.contains("terminalrows"), "{rendered}");
    }

    #[test]
    fn test_onboarding_step_navigation() {
        assert_eq!(OnboardingStep::Welcome.next(), OnboardingStep::Auth);
        assert_eq!(OnboardingStep::Auth.prev(), OnboardingStep::Welcome);
        assert_eq!(OnboardingStep::Complete.next(), OnboardingStep::Complete);
        assert_eq!(OnboardingStep::Welcome.prev(), OnboardingStep::Welcome);
    }

    #[test]
    fn test_onboarding_step_index() {
        assert_eq!(OnboardingStep::Welcome.index(), 0);
        assert_eq!(OnboardingStep::Complete.index(), 4);
    }

    #[test]
    fn test_onboarding_flow() {
        let mut flow = OnboardingFlow::new();
        assert_eq!(flow.current_step, OnboardingStep::Welcome);
        assert!(!flow.is_complete());

        flow.next();
        assert_eq!(flow.current_step, OnboardingStep::Auth);

        flow.skip();
        assert!(flow.is_complete());
    }

    #[test]
    fn test_onboarding_progress() {
        let mut flow = OnboardingFlow::new();
        assert_eq!(flow.progress(), 0.0);

        flow.current_step = OnboardingStep::Complete;
        assert_eq!(flow.progress(), 100.0);
    }

    #[test]
    fn test_splash_screen() {
        let splash = SplashScreen::new("Test")
            .with_subtitle("Loading...")
            .show_logo(false);

        assert_eq!(splash.title, "Test");
        assert_eq!(splash.subtitle.as_deref(), Some("Loading..."));
        assert!(!splash.show_logo);
    }

    #[test]
    fn test_splash_screen_default_title_uses_deixic_code() {
        let splash = SplashScreen::default();
        assert_eq!(splash.title, crate::components::deixic_logo::PRODUCT_TITLE);
        assert!(!splash.show_logo);
        assert_ne!(splash.title, "Composer");
    }
}
