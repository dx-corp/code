#[test]
fn all_setup_pages_use_shared_theme() {
    for theme in crate::components::theme_test::palettes() {
        let mut modal = SetupModal::new();
        modal.show();
        for page in [
            SetupPage::Welcome,
            SetupPage::Role,
            SetupPage::UseCase,
            SetupPage::Workflow,
            SetupPage::Verify,
            SetupPage::Checking,
            SetupPage::Results,
            SetupPage::Mode,
            SetupPage::Provider,
            SetupPage::Key,
            SetupPage::WaitingEvalops,
        ] {
            modal.page = page;
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
            terminal
                .draw(|frame| modal.render_with_theme(frame, frame.area(), theme))
                .unwrap();
            crate::components::theme_test::assert_palette(terminal.backend().buffer(), theme);
        }
    }
}

use super::*;

#[test]
fn optional_profile_is_skipped_without_silent_defaults() {
    let mut modal = SetupModal::new();
    modal.show();
    assert_eq!(modal.page(), SetupPage::Welcome);
    assert_eq!(modal.confirm(), None);
    assert_eq!(modal.confirm(), None);
    assert_eq!(modal.confirm(), None);
    assert_eq!(modal.confirm(), Some(SetupAdvance::ProfileSaved));
    assert_eq!(modal.profile(), &OnboardingProfile::default());
    assert_eq!(modal.page(), SetupPage::Mode);
}

#[test]
fn existing_connection_still_requires_explicit_runtime_test() {
    let mut modal = SetupModal::new();
    modal.set_connection_available(true);
    modal.show();
    for _ in 0..4 {
        modal.confirm();
    }
    assert_eq!(modal.page(), SetupPage::Verify);
    assert_eq!(modal.dex_state(), DexCompanionState::NeedsInput);
    assert_eq!(modal.confirm(), Some(SetupAdvance::StartChecks));
    modal.set_checking();
    assert_eq!(modal.dex_state(), DexCompanionState::Working);
    assert_eq!(modal.confirm(), None);
}

#[test]
fn only_successful_report_can_finish() {
    let mut modal = SetupModal::new();
    modal.show();
    modal.set_check_results(OnboardingReadiness {
        checks: vec![],
        ready: false,
        elapsed_ms: 1,
    });
    assert_eq!(modal.dex_state(), DexCompanionState::Failed);
    assert_eq!(modal.confirm(), Some(SetupAdvance::StartChecks));
    modal.set_check_results(OnboardingReadiness {
        checks: vec![],
        ready: true,
        elapsed_ms: 2,
    });
    assert_eq!(modal.dex_state(), DexCompanionState::Finished);
    assert_eq!(modal.confirm(), Some(SetupAdvance::Finish));
}

#[test]
fn sharing_opt_out_survives_reopening_and_is_visible() {
    let mut modal = SetupModal::new();
    modal.show();
    modal.toggle_share_diagnostics();
    modal.hide();
    modal.show();
    assert!(!modal.share_diagnostics());
    let text = modal
        .onboarding_lines(crate::themes::current_ui_theme())
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Share setup information: off"));
    assert!(text.contains("No screens"));
}

#[test]
fn tiny_terminals_and_scroll_never_leak_key() {
    let mut modal = SetupModal::new();
    modal.show();
    for (width, height) in [(1, 1), (8, 4), (24, 8), (72, 24)] {
        for page in [
            SetupPage::Welcome,
            SetupPage::Role,
            SetupPage::Key,
            SetupPage::Results,
        ] {
            modal.page = page;
            modal.secret = "secret-never-rendered".to_owned();
            for scroll in [0, 2, u16::MAX] {
                modal.scroll = scroll;
                let mut terminal =
                    ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                        .unwrap();
                terminal
                    .draw(|frame| modal.render(frame, frame.area()))
                    .unwrap();
                let text: String = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect();
                assert!(!text.contains("secret-never-rendered"));
            }
        }
    }
}

#[test]
fn setup_modal_starts_hidden_on_mode() {
    let modal = SetupModal::new();
    assert!(!modal.is_visible());
    assert_eq!(modal.page(), SetupPage::Mode);
}

#[test]
fn setup_modal_byok_starts_evalops_identity_before_provider_setup() {
    let mut modal = SetupModal::new();
    modal.show();
    modal.page = SetupPage::Mode;
    modal.move_down();
    assert_eq!(modal.confirm(), Some(SetupAdvance::StartEvalops));
    assert!(modal.continue_to_byok_after_identity());
    assert_eq!(modal.page(), SetupPage::Provider);
    assert_eq!(modal.selected_provider().id, "openrouter");
    assert_eq!(modal.confirm(), None);
    assert_eq!(modal.page(), SetupPage::Key);
    modal.insert_str("sk-or-test");
    match modal.confirm() {
        Some(SetupAdvance::SaveKey {
            provider_id,
            secret,
        }) => {
            assert_eq!(provider_id, "openrouter");
            assert_eq!(secret, "sk-or-test");
        }
        other => panic!("expected save key, got {other:?}"),
    }
}

#[test]
fn setup_modal_evalops_starts_login() {
    let mut modal = SetupModal::new();
    modal.show();
    modal.page = SetupPage::Mode;
    assert_eq!(modal.confirm(), Some(SetupAdvance::StartEvalops));
}

#[test]
fn setup_modal_back_from_welcome_closes() {
    let mut modal = SetupModal::new();
    modal.show();
    assert!(modal.back());
}

#[test]
fn setup_modal_masks_and_strips_secret_paste() {
    let mut modal = SetupModal::new();
    modal.show();
    modal.page = SetupPage::Mode;
    modal.move_down();
    assert_eq!(modal.confirm(), Some(SetupAdvance::StartEvalops));
    assert!(modal.continue_to_byok_after_identity());
    assert_eq!(modal.confirm(), None);
    modal.insert_str(" sk-or-one\nsk-or-two ");
    assert_eq!(modal.secret(), "sk-or-onesk-or-two");
    modal.backspace();
    assert!(modal.secret().ends_with('w'));
}

#[test]
fn reconnect_discards_previous_readiness() {
    let mut modal = SetupModal::new();
    modal.show();
    modal.set_check_results(OnboardingReadiness {
        checks: vec![],
        ready: true,
        elapsed_ms: 1,
    });
    assert!(!modal.back());
    assert!(modal.checks().is_none());
    modal.set_connection_ready();
    assert!(modal.checks().is_none());
    assert_eq!(modal.confirm(), Some(SetupAdvance::StartChecks));
}

#[test]
fn verification_status_is_visible_and_overscroll_is_bounded() {
    let mut modal = SetupModal::new();
    modal.show();
    modal.set_connection_ready();
    modal.set_status("The selected account changed. Run checks again.");
    assert!(
        modal
            .onboarding_lines(crate::themes::current_ui_theme())
            .iter()
            .any(|line| line.to_string().contains("selected account changed"))
    );
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 12)).unwrap();
    terminal
        .draw(|frame| modal.render(frame, frame.area()))
        .unwrap();
    for _ in 0..1000 {
        modal.scroll_down();
    }
    assert_eq!(modal.scroll, modal.max_scroll);
    assert!(modal.scroll > 0);
    modal.scroll_up();
    assert_eq!(modal.scroll, modal.max_scroll - 1);
}

#[test]
fn narrow_verification_keeps_escape_action_visible() {
    let mut modal = SetupModal::new();
    modal.show();
    modal.set_connection_ready();
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 20)).unwrap();
    terminal
        .draw(|frame| modal.render(frame, frame.area()))
        .unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(text.contains("esc reconnect"));
}

#[test]
fn welcome_owns_canvas_and_motion_respects_preferences() {
    fn render(animations: bool, tick: u64, personality: DexPersonality) -> ratatui::buffer::Buffer {
        let mut modal = SetupModal::new();
        modal.show();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(
                    Paragraph::new("SESSION CONTENT MUST BE HIDDEN"),
                    frame.area(),
                );
                modal.render_with_dex(
                    frame,
                    frame.area(),
                    SetupPresentation {
                        animations,
                        animation_frame: tick,
                        personality,
                        ..SetupPresentation::default()
                    },
                );
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }
    let still = render(false, 0, DexPersonality::Expressive);
    assert_eq!(still, render(false, 62, DexPersonality::Expressive));
    // Enabled welcome motion uses the embedded frame sequence.
    assert_ne!(
        render(true, 0, DexPersonality::Expressive),
        render(true, 62, DexPersonality::Expressive)
    );
    let text: String = still.content.iter().map(|cell| cell.symbol()).collect();
    assert!(text.contains("Deixic Code"));
    assert!(text.contains("Press Enter to continue"));
    assert!(!text.contains("SESSION CONTENT"));
    let quiet = render(true, 62, DexPersonality::Quiet);
    assert!(!quiet.content.iter().any(|cell| cell.symbol() == "█"));
}
