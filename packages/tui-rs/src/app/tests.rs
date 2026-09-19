use super::*;

use crate::keybindings::{
    keybindings_test_env_lock, queued_follow_up_edit_binding_for_terminal_name,
};
use crate::state::{ApprovalMode, QueueMode};
use tempfile::tempdir;

fn acquire_keybindings_test_lock() -> tokio::sync::MutexGuard<'static, ()> {
    keybindings_test_env_lock().blocking_lock()
}

async fn acquire_keybindings_test_lock_async() -> tokio::sync::MutexGuard<'static, ()> {
    keybindings_test_env_lock().lock().await
}

// ─────────────────────────────────────────────────────────────────────────
// ActiveModal Tests
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn test_active_modal_default() {
    let modal = ActiveModal::None;
    assert_eq!(modal, ActiveModal::None);
}

#[test]
fn test_active_modal_equality() {
    assert_eq!(ActiveModal::FileSearch, ActiveModal::FileSearch);
    assert_ne!(ActiveModal::FileSearch, ActiveModal::CommandPalette);
}

#[test]
fn test_active_modal_copy() {
    let modal = ActiveModal::Approval;
    let copy = modal;
    assert_eq!(modal, copy);
}

// ─────────────────────────────────────────────────────────────────────────
// Key Event Filtering Tests
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn test_should_handle_key_event_press_and_repeat() {
    // Kitty-protocol terminals report Press/Repeat/Release; repeats must be
    // handled so held keys auto-repeat, releases must be ignored.
    assert!(should_handle_key_event(KeyEventKind::Press));
    assert!(should_handle_key_event(KeyEventKind::Repeat));
    assert!(!should_handle_key_event(KeyEventKind::Release));
}

#[test]
fn uncurses_input_kill_switch_defaults_on_and_accepts_only_zero_as_off() {
    assert!(uncurses_input_enabled(None));
    assert!(uncurses_input_enabled(Some(std::ffi::OsStr::new("1"))));
    assert!(!uncurses_input_enabled(Some(std::ffi::OsStr::new("0"))));
}

#[test]
fn terminal_reporting_shutdown_requires_reader_and_theme_follower() {
    assert!(terminal_reporting_shutdown_needed(true, true));
    assert!(!terminal_reporting_shutdown_needed(true, false));
    assert!(!terminal_reporting_shutdown_needed(false, true));
    assert!(!terminal_reporting_shutdown_needed(false, false));
}

#[test]
fn terminal_theme_query_requires_fallback_and_respects_throttle() {
    assert!(!terminal_theme_query_due(
        false,
        true,
        false,
        Some(Duration::from_secs(3))
    ));
    assert!(!terminal_theme_query_due(
        true,
        false,
        false,
        Some(Duration::from_secs(3))
    ));
    assert!(!terminal_theme_query_due(
        true,
        true,
        true,
        Some(Duration::from_secs(3))
    ));
    assert!(!terminal_theme_query_due(
        true,
        true,
        false,
        Some(Duration::from_millis(1999))
    ));
    assert!(terminal_theme_query_due(
        true,
        true,
        false,
        Some(Duration::from_secs(2))
    ));
    assert!(terminal_theme_query_due(true, true, false, None));
}

#[test]
fn terminal_theme_events_respect_theme_follow_opt_out() {
    let mut app = new_test_app();
    app.state.theme_follower = None;
    let scheme = if crate::themes::current_theme_name() == "light" {
        uncurses::event::ColorScheme::Light
    } else {
        uncurses::event::ColorScheme::Dark
    };

    assert!(!app.apply_terminal_theme_event(&AppTerminalEvent::ColorScheme(scheme)));
    assert!(app.state.theme_follower.is_none());
}

#[test]
fn terminal_theme_events_apply_typed_and_hysteretic_updates() {
    let original = crate::themes::current_theme_name();
    crate::themes::set_theme_by_name("dark").expect("built-in dark theme");
    let mut app = new_test_app();
    app.state.theme_follower = Some(crate::themes::osc11::AutoThemeFollower::new("dark"));

    assert!(
        app.apply_terminal_theme_event(&AppTerminalEvent::ColorScheme(
            uncurses::event::ColorScheme::Light
        ))
    );
    assert_eq!(crate::themes::current_theme_name(), "light");
    assert_eq!(
        app.state
            .theme_follower
            .as_ref()
            .map(|follower| follower.current()),
        Some("light")
    );

    crate::themes::set_theme_by_name("dark").expect("built-in dark theme");
    app.state.theme_follower = Some(crate::themes::osc11::AutoThemeFollower::new("dark"));
    let light_background = AppTerminalEvent::BackgroundColor {
        red: 255,
        green: 255,
        blue: 255,
    };
    assert!(!app.apply_terminal_theme_event(&light_background));
    assert!(app.apply_terminal_theme_event(&light_background));
    assert_eq!(crate::themes::current_theme_name(), "light");

    crate::themes::set_theme_by_name(&original).expect("restore original theme");
}

#[test]
fn test_active_modal_variants_exist() {
    // Ensure all modal variants are defined correctly
    let modals = [
        ActiveModal::None,
        ActiveModal::FileSearch,
        ActiveModal::SessionSwitcher,
        ActiveModal::Operations,
        ActiveModal::CommandPalette,
        ActiveModal::Approval,
        ActiveModal::ModelSelector,
        ActiveModal::ThemeSelector,
        ActiveModal::Setup,
    ];
    assert_eq!(modals.len(), 9);
}

// ─────────────────────────────────────────────────────────────────────────
// CommandOutput Handling Tests
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn test_command_output_message_variants() {
    // Test that CommandOutput variants can be constructed
    let msg = CommandOutput::Message("test".to_string());
    assert!(matches!(msg, CommandOutput::Message(_)));

    let help = CommandOutput::Help("help text".to_string());
    assert!(matches!(help, CommandOutput::Help(_)));

    let warn = CommandOutput::Warning("warning".to_string());
    assert!(matches!(warn, CommandOutput::Warning(_)));

    let silent = CommandOutput::Silent;
    assert!(matches!(silent, CommandOutput::Silent));
}

#[test]
fn test_command_output_multi() {
    let outputs = CommandOutput::Multi(vec![
        CommandOutput::Message("first".to_string()),
        CommandOutput::Warning("second".to_string()),
    ]);
    if let CommandOutput::Multi(items) = outputs {
        assert_eq!(items.len(), 2);
    } else {
        panic!("Expected Multi variant");
    }
}

// ─────────────────────────────────────────────────────────────────────────
// CommandAction Handling Tests
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn test_command_action_clear_messages() {
    let action = CommandAction::ClearMessages;
    assert!(matches!(action, CommandAction::ClearMessages));
}

#[test]
fn test_command_action_toggle_zen_mode() {
    let action = CommandAction::ToggleZenMode;
    assert!(matches!(action, CommandAction::ToggleZenMode));
}

#[test]
fn test_command_action_set_approval_mode() {
    let action = CommandAction::SetApprovalMode("yolo".to_string());
    if let CommandAction::SetApprovalMode(mode) = action {
        assert_eq!(mode, "yolo");
    } else {
        panic!("Expected SetApprovalMode");
    }
}

#[test]
fn test_command_action_set_thinking_level() {
    let action = CommandAction::SetThinkingLevel("high".to_string());
    if let CommandAction::SetThinkingLevel(level) = action {
        assert_eq!(level, "high");
    } else {
        panic!("Expected SetThinkingLevel");
    }
}

#[test]
fn test_command_action_quit() {
    let action = CommandAction::Quit;
    assert!(matches!(action, CommandAction::Quit));
}

#[test]
fn test_command_action_refresh_workspace() {
    let action = CommandAction::RefreshWorkspace;
    assert!(matches!(action, CommandAction::RefreshWorkspace));
}

#[test]
fn test_command_action_copy_last_message() {
    let action = CommandAction::CopyLastMessage;
    assert!(matches!(action, CommandAction::CopyLastMessage));
}

#[test]
fn test_command_action_compact_conversation() {
    let action = CommandAction::CompactConversation(Some("focus".to_string()));
    if let CommandAction::CompactConversation(instructions) = action {
        assert_eq!(instructions, Some("focus".to_string()));
    } else {
        panic!("Expected CompactConversation");
    }
}

// ─────────────────────────────────────────────────────────────────────────
// ModalType Tests
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn test_modal_type_variants() {
    let types = [
        ModalType::ThemeSelector,
        ModalType::ModelSelector,
        ModalType::SessionList,
        ModalType::Operations,
        ModalType::FileSearch,
        ModalType::CommandPalette,
        ModalType::Help,
    ];
    assert_eq!(types.len(), 7);
}

// ─────────────────────────────────────────────────────────────────────────
// ApprovalMode Tests
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn test_approval_mode_parse() {
    assert_eq!(ApprovalMode::parse("yolo"), Some(ApprovalMode::Yolo));
    assert_eq!(ApprovalMode::parse("safe"), Some(ApprovalMode::Safe));
    assert_eq!(
        ApprovalMode::parse("selective"),
        Some(ApprovalMode::Selective)
    );
    assert_eq!(ApprovalMode::parse("invalid"), None);
}

#[test]
fn test_approval_mode_next() {
    assert_eq!(ApprovalMode::Yolo.next(), ApprovalMode::Selective);
    assert_eq!(ApprovalMode::Selective.next(), ApprovalMode::Safe);
    assert_eq!(ApprovalMode::Safe.next(), ApprovalMode::Yolo);
}

#[test]
fn test_approval_mode_label() {
    assert!(!ApprovalMode::Yolo.label().is_empty());
    assert!(!ApprovalMode::Safe.label().is_empty());
    assert!(!ApprovalMode::Selective.label().is_empty());
}

// ─────────────────────────────────────────────────────────────────────────
// ThinkingLevel Tests
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn test_thinking_level_parse() {
    assert!(ThinkingLevel::parse("off").is_some());
    assert!(ThinkingLevel::parse("minimal").is_some());
    assert!(ThinkingLevel::parse("low").is_some());
    assert!(ThinkingLevel::parse("medium").is_some());
    assert!(ThinkingLevel::parse("high").is_some());
    assert!(ThinkingLevel::parse("max").is_some());
    assert!(ThinkingLevel::parse("invalid").is_none());
}

#[test]
fn test_thinking_level_to_config() {
    let off = ThinkingLevel::parse("off").unwrap();
    let (enabled, _budget) = off.to_config();
    assert!(!enabled);

    let high = ThinkingLevel::parse("high").unwrap();
    let (enabled, budget) = high.to_config();
    assert!(enabled);
    assert!(budget > 0);
}

#[test]
fn test_thinking_level_label() {
    let medium = ThinkingLevel::parse("medium").unwrap();
    assert!(!medium.label().is_empty());
}

// ─────────────────────────────────────────────────────────────────────────
// AppState Tests (Integration with app.rs logic)
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn test_app_state_message_operations() {
    let mut state = AppState::new();
    assert!(state.messages.is_empty());

    state.add_user_message("Hello".to_string());
    assert_eq!(state.messages.len(), 1);
    assert_eq!(state.messages[0].role, MessageRole::User);
    assert_eq!(state.messages[0].content, "Hello");

    state.add_system_message("System response".to_string());
    assert_eq!(state.messages.len(), 2);
}

#[test]
fn test_app_state_input_operations() {
    let mut state = AppState::new();
    assert!(state.input().is_empty());

    state.insert_char('H');
    state.insert_char('i');
    assert_eq!(state.input(), "Hi");

    state.backspace();
    assert_eq!(state.input(), "H");

    state.set_input("New input");
    assert_eq!(state.input(), "New input");

    let taken = state.take_input();
    assert_eq!(taken, "New input");
    assert!(state.input().is_empty());
}

#[test]
fn test_app_state_scroll_operations() {
    let mut state = AppState::new();
    assert_eq!(state.scroll_offset, 0);

    // scroll_down increases offset (scrolls toward older messages)
    state.scroll_down(5);
    assert_eq!(state.scroll_offset, 5);

    // scroll_up decreases offset (scrolls toward newer messages)
    state.scroll_up(3);
    assert_eq!(state.scroll_offset, 2);

    // scroll_up with larger amount clamps to 0
    state.scroll_up(10);
    assert_eq!(state.scroll_offset, 0);
}

#[test]
fn test_app_state_zen_mode() {
    let mut state = AppState::new();
    assert!(!state.zen_mode);

    state.zen_mode = true;
    assert!(state.zen_mode);
}

#[test]
fn test_app_state_busy_flag() {
    let mut state = AppState::new();
    assert!(!state.busy);

    state.busy = true;
    assert!(state.busy);
}

// ─────────────────────────────────────────────────────────────────────────
// Slash Command State Tests
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn test_slash_cycle_state_new() {
    let state = SlashCycleState::new();
    assert!(!state.has_completions());
    assert!(state.current().is_none());
}

#[test]
fn test_slash_cycle_state_reset() {
    let mut state = SlashCycleState::new();
    state.reset();
    assert!(!state.has_completions());
}

#[test]
fn slash_popup_renders_command_descriptions_and_controls() {
    let registry = Arc::new(crate::commands::build_command_registry());
    let matcher = SlashCommandMatcher::new(Arc::clone(&registry));
    let mut state = SlashCycleState::new();
    state.set_query("he", &matcher);
    let description = registry
        .get("help")
        .expect("help command")
        .description
        .clone();

    let backend = ratatui::backend::TestBackend::new(100, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            App::render_slash_completions_static(
                &mut state,
                &registry,
                frame,
                frame.area(),
                crate::themes::current_ui_theme(),
            );
        })
        .expect("render slash popup");
    let buffer = terminal.backend().buffer();
    let rendered = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(rendered.contains("Commands"));
    assert!(rendered.contains("/help"));
    assert!(rendered.contains(&description));
    assert!(rendered.contains("Enter run"));
}

#[test]
fn slash_popup_uses_theme_colors_for_selected_and_unselected_rows() {
    use ratatui::style::Color;
    for theme in [
        maestro_ui::UiTheme {
            surface: Color::Rgb(24, 24, 24),
            text: Color::White,
            muted: Color::Gray,
            border: Color::Blue,
            focus: Color::Magenta,
            selection: Some(Color::Rgb(45, 45, 45)),
            ..Default::default()
        },
        maestro_ui::UiTheme {
            surface: Color::White,
            text: Color::Black,
            muted: Color::DarkGray,
            border: Color::Gray,
            focus: Color::Blue,
            selection: Some(Color::Rgb(225, 225, 225)),
            ..Default::default()
        },
        maestro_ui::UiTheme::default(),
    ] {
        let registry = Arc::new(crate::commands::build_command_registry());
        let matcher = SlashCommandMatcher::new(Arc::clone(&registry));
        let mut state = SlashCycleState::new();
        state.set_query("", &matcher);
        assert!(state.completions().len() > 1);
        let selected = state.list_state_mut().selected().expect("selected command");
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
        terminal
            .draw(|frame| {
                App::render_slash_completions_static(
                    &mut state,
                    &registry,
                    frame,
                    frame.area(),
                    theme,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let popup = App::slash_popup_area(buffer.area, state.completions().len());
        assert_eq!(buffer[(popup.x, popup.y)].fg, theme.border);
        assert_eq!(buffer[(popup.x, popup.y)].bg, theme.surface);
        for row in 0..popup.height - 2 {
            let y = popup.y + 1 + row;
            let expected_bg = if usize::from(row) == selected {
                theme.selection.unwrap_or(theme.surface)
            } else {
                theme.surface
            };
            let expected_fg = if usize::from(row) == selected {
                theme.focus
            } else {
                theme.text
            };
            let command_x = (popup.x + 1..popup.right() - 1)
                .find(|&x| buffer[(x, y)].symbol() == "/")
                .expect("command label");
            assert_eq!(buffer[(command_x, y)].fg, expected_fg);
            for x in popup.x + 1..popup.right() - 1 {
                assert_eq!(buffer[(x, y)].bg, expected_bg, "row background");
            }
            assert!(
                (command_x..popup.right() - 1)
                    .any(|x| buffer[(x, y)].symbol() != " " && buffer[(x, y)].fg == theme.muted),
                "description retains muted color"
            );
        }
        let footer_y = popup.bottom() - 1;
        let hint_x = (popup.x..popup.right())
            .find(|&x| buffer[(x, footer_y)].symbol() == "↑")
            .expect("keyboard hint");
        assert_eq!(buffer[(hint_x, footer_y)].fg, theme.muted);
    }
}

#[test]
fn failed_model_default_save_preserves_session_choice() {
    if crate::config::test_reexec_for_process_isolation() {
        return;
    }
    let temp = tempdir().unwrap();
    std::env::set_var("MAESTRO_HOME", temp.path());
    let mut app = new_test_app();
    let current = app.current_model.clone();
    let model = app.state.model.clone();
    let spawn = app.pending_agent_spawn;
    let user_set = app.current_model_user_set;
    std::fs::create_dir(temp.path().join("config.toml")).unwrap();
    app.switch_model("gpt-5.6", true);
    assert_eq!(app.current_model, current);
    assert_eq!(app.state.model, model);
    assert_eq!(app.pending_agent_spawn, spawn);
    assert_eq!(app.current_model_user_set, user_set);
    assert!(app.pending_model_change.is_none());
    assert!(
        app.state
            .error
            .as_deref()
            .unwrap()
            .contains("Failed to save default model")
    );
    assert!(
        !app.state
            .status
            .as_deref()
            .unwrap_or_default()
            .contains("saved")
    );
    std::fs::remove_dir(temp.path().join("config.toml")).unwrap();
    app.switch_model("gpt-5.6", true);
    assert!(app.state.error.is_none());
    assert_eq!(app.current_model, "gpt-5.6");
    assert!(app.pending_agent_spawn);
}

#[test]
fn failed_footer_save_preserves_runtime_choice_and_recovers() {
    if crate::config::test_reexec_for_process_isolation() {
        return;
    }
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let temp = tempdir().unwrap();
        std::env::set_var("MAESTRO_HOME", temp.path());
        let ui_path = temp.path().join("ui.json");
        let mut app = new_test_app();
        let original = app.footer_style;
        std::fs::create_dir(&ui_path).unwrap();

        app.handle_command_action(CommandAction::SetFooterStyle(FooterStyle::Solo))
            .await;
        assert_eq!(app.footer_style, original);
        assert!(
            app.state
                .error
                .as_deref()
                .is_some_and(|error| error.contains("failed to persist"))
        );
        assert!(ui_path.is_dir());

        std::fs::remove_dir(&ui_path).unwrap();
        app.handle_command_action(CommandAction::SetFooterStyle(FooterStyle::Solo))
            .await;
        assert_eq!(app.footer_style, FooterStyle::Solo);
        assert_eq!(app.ui_prefs.footer_style(), FooterStyle::Solo);
        assert!(app.state.error.is_none());
        assert_eq!(
            crate::ui_prefs::UiPrefs::load_default().footer_style(),
            FooterStyle::Solo
        );
    });
}

#[test]
fn saved_preference_retries_clear_previous_errors() {
    if crate::config::test_reexec_for_process_isolation() {
        return;
    }
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let temp = tempdir().unwrap();
        std::env::set_var("MAESTRO_HOME", temp.path());
        let ui_path = temp.path().join("ui.json");
        let mut app = new_test_app();
        let original_output = app.state.output_detail();

        std::fs::create_dir(&ui_path).unwrap();
        app.handle_command_action(CommandAction::SetOutputDetail(
            crate::state::OutputDetail::Expanded,
        ))
        .await;
        assert_eq!(app.state.output_detail(), original_output);
        assert!(app.state.error.is_some());
        std::fs::remove_dir(&ui_path).unwrap();
        app.handle_command_action(CommandAction::SetOutputDetail(
            crate::state::OutputDetail::Expanded,
        ))
        .await;
        assert_eq!(
            app.state.output_detail(),
            crate::state::OutputDetail::Expanded
        );
        assert!(app.state.error.is_none());
        assert_eq!(
            crate::ui_prefs::UiPrefs::load_default().output_detail(),
            crate::state::OutputDetail::Expanded
        );

        std::fs::remove_file(&ui_path).unwrap();
        std::fs::create_dir(&ui_path).unwrap();
        app.handle_command_action(CommandAction::SetFooterStyle(FooterStyle::History))
            .await;
        assert!(app.state.error.is_some());
        std::fs::remove_dir(&ui_path).unwrap();
        app.handle_command_action(CommandAction::SetFooterStyle(FooterStyle::History))
            .await;
        assert_eq!(app.footer_style, FooterStyle::History);
        assert!(app.state.error.is_none());
        assert_eq!(
            crate::ui_prefs::UiPrefs::load_default().footer_style(),
            FooterStyle::History
        );

        std::fs::remove_file(&ui_path).unwrap();
        std::fs::create_dir(&ui_path).unwrap();
        let original_animation = app.ui_prefs.animations;
        app.handle_dex_command("motion-on");
        assert_eq!(app.ui_prefs.animations, original_animation);
        assert!(app.state.error.is_some());
        std::fs::remove_dir(&ui_path).unwrap();
        app.handle_dex_command("motion-on");
        assert_eq!(app.ui_prefs.animations, Some(true));
        assert!(app.state.error.is_none());
        assert_eq!(
            crate::ui_prefs::UiPrefs::load_default().animations,
            Some(true)
        );
    });
}

#[test]
fn switch_model_without_agent_remembers_the_route() {
    let mut app = new_test_app();
    assert!(app.native_agent.is_none());
    app.switch_model("openrouter/openai/gpt-4o-mini", false);
    assert_eq!(app.current_model, "openrouter/openai/gpt-4o-mini");
    assert_eq!(
        app.state.model.as_deref(),
        Some("openrouter/openai/gpt-4o-mini")
    );
    assert!(app.state.error.is_none(), "{:?}", app.state.error);
    assert!(
        app.state
            .status
            .as_deref()
            .is_some_and(|status| status.contains("openrouter/openai/gpt-4o-mini")),
        "{:?}",
        app.state.status
    );
    assert!(app.pending_agent_spawn, "missing agent should retry spawn");
    assert!(app.current_model_user_set);
}

#[test]
fn test_normalize_slash_completion_never_doubles() {
    use super::command_handlers::normalize_slash_completion;

    assert_eq!(normalize_slash_completion("/help"), "/help");
    assert_eq!(normalize_slash_completion("help"), "/help");
    assert_eq!(normalize_slash_completion("//help"), "/help");
    assert_eq!(normalize_slash_completion("///plan"), "/plan");
    assert_eq!(normalize_slash_completion("  /theme  "), "/theme");
    assert_eq!(normalize_slash_completion("/"), "/");
    assert_eq!(normalize_slash_completion("//"), "/");
    assert_eq!(normalize_slash_completion(""), "/");
}

#[test]
fn test_cleanup_result_message_surfaces_errors_even_when_nothing_was_removed() {
    use super::command_handlers::cleanup_result_message;

    // The common, happy-path case: nothing was eligible to prune.
    assert_eq!(cleanup_result_message(0, 0), "No sessions to prune.");

    // Regression case: every eligible session hit a real (non-contention)
    // lock-acquisition error, so `removed == 0` but real work was
    // attempted and failed. Before the fix this collapsed to the same
    // "No sessions to prune." message as the happy-path case above,
    // discarding the error count and reading as success.
    assert_eq!(
        cleanup_result_message(0, 3),
        "Failed to prune sessions: 3 error(s)."
    );

    // Success with no errors.
    assert_eq!(cleanup_result_message(5, 0), "Pruned 5 session(s).");

    // Partial success: some pruned, some errored.
    assert_eq!(
        cleanup_result_message(5, 2),
        "Pruned 5 session(s). 2 error(s)."
    );
}

#[test]
fn test_slash_cycle_apply_path_no_double_slash() {
    use super::command_handlers::normalize_slash_completion;
    use crate::commands::{SlashCommandMatcher, SlashCycleState, build_command_registry};
    use std::sync::Arc;

    let registry = Arc::new(build_command_registry());
    let matcher = SlashCommandMatcher::new(registry);
    let mut cycle = SlashCycleState::new();
    cycle.set_query("he", &matcher);
    assert!(cycle.has_completions());
    let applied = normalize_slash_completion(cycle.current().unwrap());
    assert!(applied.starts_with('/'), "{applied}");
    assert!(!applied.starts_with("//"), "double slash bug: {applied}");
    // Simulate the old buggy path for documentation:
    let buggy = format!("/{}", cycle.current().unwrap());
    assert!(
        buggy.starts_with("//"),
        "expected old path to double-slash when completion already has /"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// Usage Action Tests
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn test_usage_action_variants() {
    use crate::commands::UsageAction;

    let summary = UsageAction::Summary;
    assert!(matches!(summary, UsageAction::Summary));

    let detailed = UsageAction::Detailed;
    assert!(matches!(detailed, UsageAction::Detailed));

    let reset = UsageAction::Reset;
    assert!(matches!(reset, UsageAction::Reset));
}

// ─────────────────────────────────────────────────────────────────────────
// Export Action Tests
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn test_export_action_variants() {
    use crate::commands::ExportAction;

    let md = ExportAction::Markdown(None);
    assert!(matches!(md, ExportAction::Markdown(None)));

    let html = ExportAction::Html(Some("test.html".to_string()));
    if let ExportAction::Html(path) = html {
        assert_eq!(path, Some("test.html".to_string()));
    }

    let json = ExportAction::Json(None);
    assert!(matches!(json, ExportAction::Json(_)));

    let txt = ExportAction::PlainText(None);
    assert!(matches!(txt, ExportAction::PlainText(_)));
}

// ─────────────────────────────────────────────────────────────────────────
// History Action Tests
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn test_history_action_variants() {
    use crate::commands::HistoryAction;

    let recent = HistoryAction::Recent(10);
    if let HistoryAction::Recent(count) = recent {
        assert_eq!(count, 10);
    }

    let search = HistoryAction::Search("query".to_string());
    if let HistoryAction::Search(q) = search {
        assert_eq!(q, "query");
    }

    let clear = HistoryAction::Clear;
    assert!(matches!(clear, HistoryAction::Clear));
}

// ─────────────────────────────────────────────────────────────────────────
// Tool History Action Tests
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn test_tool_history_action_variants() {
    use crate::commands::ToolHistoryAction;

    let recent = ToolHistoryAction::Recent(5);
    if let ToolHistoryAction::Recent(count) = recent {
        assert_eq!(count, 5);
    }

    let stats = ToolHistoryAction::Stats;
    assert!(matches!(stats, ToolHistoryAction::Stats));

    let for_tool = ToolHistoryAction::ForTool("bash".to_string());
    if let ToolHistoryAction::ForTool(name) = for_tool {
        assert_eq!(name, "bash");
    }

    let clear = ToolHistoryAction::Clear;
    assert!(matches!(clear, ToolHistoryAction::Clear));
}

// ─────────────────────────────────────────────────────────────────────────
// Hooks Action Tests
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn test_hooks_action_variants() {
    use crate::commands::HooksAction;

    let actions = [
        HooksAction::List,
        HooksAction::Toggle,
        HooksAction::Reload,
        HooksAction::Metrics,
        HooksAction::Enable,
        HooksAction::Disable,
    ];
    assert_eq!(actions.len(), 6);
}

// ─────────────────────────────────────────────────────────────────────────
// Message Role Tests
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn test_message_role_equality() {
    assert_eq!(MessageRole::User, MessageRole::User);
    assert_eq!(MessageRole::Assistant, MessageRole::Assistant);
    assert_ne!(MessageRole::User, MessageRole::Assistant);
}

// ─────────────────────────────────────────────────────────────────────────
// Integration Tests for State Transitions
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn test_message_content_preview() {
    let long_content = "a".repeat(200);
    let chars: Vec<char> = long_content.chars().collect();
    let preview = if chars.len() > 100 {
        format!("{}...", chars[..97].iter().collect::<String>())
    } else {
        long_content.clone()
    };
    assert_eq!(preview.len(), 100); // 97 chars + "..."
}

#[test]
fn test_compact_conversation_logic() {
    // Test the logic used in CompactConversation action
    let msg_count = 10;
    let keep_count = 2;

    if msg_count > 4 {
        let to_summarize = msg_count - keep_count;
        assert_eq!(to_summarize, 8);
    }

    // Edge case: exactly 4 messages shouldn't compact
    let msg_count_small = 4;
    assert!(msg_count_small <= 4);
}

#[test]
fn test_restore_visible_session_messages_applies_compactions() {
    let mut state = AppState::new();
    let session = ParsedSession {
        selective_summary_context: None,
        header: SessionHeader {
            version: Some(2),
            id: "session-1".to_string(),
            timestamp: "2026-03-31T12:00:00Z".to_string(),
            cwd: "/tmp".to_string(),
            model: "anthropic/claude-sonnet-4-5".to_string(),
            subject: None,
            model_metadata: None,
            thinking_level: ThinkingLevel::Medium,
            system_prompt: None,
            prompt_metadata: None,
            prompt_context_manifest: None,
            unified_context_manifest: None,
            tools: Vec::new(),
            branched_from: None,
            parent_session: None,
        },
        messages: vec![
            AppMessage::User {
                content: MessageContent::Text("User one".to_string()),
                attachments: None,
                timestamp: 1,
            },
            AppMessage::Assistant {
                content: vec![SessionContentBlock::Text {
                    text: "Assistant one".to_string(),
                }],
                api: None,
                provider: None,
                model: None,
                usage: None,
                stop_reason: None,
                timestamp: 2,
            },
            AppMessage::User {
                content: MessageContent::Text("User two".to_string()),
                attachments: None,
                timestamp: 3,
            },
            AppMessage::Assistant {
                content: vec![SessionContentBlock::Text {
                    text: "Assistant two".to_string(),
                }],
                api: None,
                provider: None,
                model: None,
                usage: None,
                stop_reason: None,
                timestamp: 4,
            },
            AppMessage::User {
                content: MessageContent::Text("User three".to_string()),
                attachments: None,
                timestamp: 5,
            },
            AppMessage::Assistant {
                content: vec![SessionContentBlock::Text {
                    text: "Assistant three".to_string(),
                }],
                api: None,
                provider: None,
                model: None,
                usage: None,
                stop_reason: None,
                timestamp: 6,
            },
        ],
        meta: None,
        stats: Default::default(),
        thinking_level_changes: Vec::new(),
        model_changes: Vec::new(),
        compactions: vec![CompactionEntry {
            id: None,
            parent_id: None,
            timestamp: "2026-03-31T12:05:00Z".to_string(),
            summary: "## Conversation Summary".to_string(),
            first_kept_entry_id: None,
            first_kept_entry_index: Some(4),
            tokens_before: 1000,
            auto: true,
            custom_instructions: None,
            continuation: None,
        }],
        session_events: Vec::new(),
        context_budget_snapshots: Vec::new(),
        lifecycle_notifications: Vec::new(),
        pending_lifecycle_agent_notes: Vec::new(),
        side_questions: Vec::new(),
        plan_review_events: Vec::new(),
        usage_entries: Vec::new(),
        file_path: "/tmp/session-1.jsonl".to_string(),
    };

    restore_visible_session_messages(&mut state, &session);

    assert_eq!(state.messages.len(), 3);
    assert!(state.messages[0].is_compaction_boundary());
    assert_eq!(state.messages[0].content, "## Conversation Summary");
    assert_eq!(
        state.messages[0].timestamp,
        parse_rfc3339_system_time("2026-03-31T12:05:00Z").unwrap()
    );
    assert_eq!(state.messages[1].content, "User three");
    assert_eq!(state.messages[2].content, "Assistant three");
}

#[test]
fn test_restore_visible_session_messages_applies_multiple_compactions_in_order() {
    let mut state = AppState::new();
    let session = ParsedSession {
        selective_summary_context: None,
        header: SessionHeader {
            version: Some(2),
            id: "session-2".to_string(),
            timestamp: "2026-03-31T12:00:00Z".to_string(),
            cwd: "/tmp".to_string(),
            model: "anthropic/claude-sonnet-4-5".to_string(),
            subject: None,
            model_metadata: None,
            thinking_level: ThinkingLevel::Medium,
            system_prompt: None,
            prompt_metadata: None,
            prompt_context_manifest: None,
            unified_context_manifest: None,
            tools: Vec::new(),
            branched_from: None,
            parent_session: None,
        },
        messages: vec![
            AppMessage::User {
                content: MessageContent::Text("User one".to_string()),
                attachments: None,
                timestamp: 1,
            },
            AppMessage::Assistant {
                content: vec![SessionContentBlock::Text {
                    text: "Assistant one".to_string(),
                }],
                api: None,
                provider: None,
                model: None,
                usage: None,
                stop_reason: None,
                timestamp: 2,
            },
            AppMessage::User {
                content: MessageContent::Text("User two".to_string()),
                attachments: None,
                timestamp: 3,
            },
            AppMessage::Assistant {
                content: vec![SessionContentBlock::Text {
                    text: "Assistant two".to_string(),
                }],
                api: None,
                provider: None,
                model: None,
                usage: None,
                stop_reason: None,
                timestamp: 4,
            },
            AppMessage::User {
                content: MessageContent::Text("User three".to_string()),
                attachments: None,
                timestamp: 5,
            },
            AppMessage::Assistant {
                content: vec![SessionContentBlock::Text {
                    text: "Assistant three".to_string(),
                }],
                api: None,
                provider: None,
                model: None,
                usage: None,
                stop_reason: None,
                timestamp: 6,
            },
            AppMessage::User {
                content: MessageContent::Text("User four".to_string()),
                attachments: None,
                timestamp: 7,
            },
            AppMessage::Assistant {
                content: vec![SessionContentBlock::Text {
                    text: "Assistant four".to_string(),
                }],
                api: None,
                provider: None,
                model: None,
                usage: None,
                stop_reason: None,
                timestamp: 8,
            },
        ],
        meta: None,
        stats: Default::default(),
        thinking_level_changes: Vec::new(),
        model_changes: Vec::new(),
        compactions: vec![
            CompactionEntry {
                id: None,
                parent_id: None,
                timestamp: "2026-03-31T12:05:00Z".to_string(),
                summary: "## First Summary".to_string(),
                first_kept_entry_id: None,
                first_kept_entry_index: Some(4),
                tokens_before: 1000,
                auto: true,
                custom_instructions: None,
                continuation: None,
            },
            CompactionEntry {
                id: None,
                parent_id: None,
                timestamp: "2026-03-31T12:10:00Z".to_string(),
                summary: "## Second Summary".to_string(),
                first_kept_entry_id: None,
                first_kept_entry_index: Some(2),
                tokens_before: 1200,
                auto: true,
                custom_instructions: None,
                continuation: None,
            },
        ],
        session_events: Vec::new(),
        context_budget_snapshots: Vec::new(),
        lifecycle_notifications: Vec::new(),
        pending_lifecycle_agent_notes: Vec::new(),
        side_questions: Vec::new(),
        plan_review_events: Vec::new(),
        usage_entries: Vec::new(),
        file_path: "/tmp/session-2.jsonl".to_string(),
    };

    restore_visible_session_messages(&mut state, &session);

    assert_eq!(state.messages.len(), 3);
    assert!(state.messages[0].is_compaction_boundary());
    assert_eq!(state.messages[0].content, "## Second Summary");
    assert_eq!(state.messages[1].content, "User four");
    assert_eq!(state.messages[2].content, "Assistant four");
}

#[test]
fn test_restore_lifecycle_notifications_in_compacted_transcript_order() {
    let mut state = AppState::new();
    let session = ParsedSession {
        selective_summary_context: None,
        header: SessionHeader {
            version: Some(2),
            id: "session-lifecycle-order".to_string(),
            timestamp: "2026-03-31T12:00:00Z".to_string(),
            cwd: "/tmp".to_string(),
            model: "openai/gpt-5.2".to_string(),
            subject: None,
            model_metadata: None,
            thinking_level: ThinkingLevel::Medium,
            system_prompt: None,
            prompt_metadata: None,
            prompt_context_manifest: None,
            unified_context_manifest: None,
            tools: Vec::new(),
            branched_from: None,
            parent_session: None,
        },
        messages: vec![
            AppMessage::User {
                content: MessageContent::Text("First".to_string()),
                attachments: None,
                timestamp: 1,
            },
            AppMessage::Assistant {
                content: vec![SessionContentBlock::Text {
                    text: "First reply".to_string(),
                }],
                api: None,
                provider: None,
                model: None,
                usage: None,
                stop_reason: None,
                timestamp: 2,
            },
            AppMessage::User {
                content: MessageContent::Text("Second".to_string()),
                attachments: None,
                timestamp: 3,
            },
            AppMessage::Assistant {
                content: vec![SessionContentBlock::Text {
                    text: "Second reply".to_string(),
                }],
                api: None,
                provider: None,
                model: None,
                usage: None,
                stop_reason: None,
                timestamp: 4,
            },
        ],
        meta: None,
        stats: Default::default(),
        thinking_level_changes: Vec::new(),
        model_changes: Vec::new(),
        compactions: vec![CompactionEntry {
            id: None,
            parent_id: None,
            timestamp: "2026-03-31T12:05:00Z".to_string(),
            summary: "## Conversation Summary".to_string(),
            first_kept_entry_id: None,
            first_kept_entry_index: Some(2),
            tokens_before: 1000,
            auto: true,
            custom_instructions: None,
            continuation: None,
        }],
        session_events: Vec::new(),
        context_budget_snapshots: Vec::new(),
        lifecycle_notifications: vec![
            crate::session::LifecycleNotificationEntry {
                id: "notice-a".to_string(),
                content: "Notice A".to_string(),
                timestamp: "2026-03-31T12:04:00Z".to_string(),
                visible_index: 1,
            },
            crate::session::LifecycleNotificationEntry {
                id: "notice-b".to_string(),
                content: "Notice B".to_string(),
                timestamp: "2026-03-31T12:04:01Z".to_string(),
                visible_index: 1,
            },
            crate::session::LifecycleNotificationEntry {
                id: "notice-after".to_string(),
                content: "Notice after".to_string(),
                timestamp: "2026-03-31T12:06:00Z".to_string(),
                visible_index: 2,
            },
        ],
        pending_lifecycle_agent_notes: Vec::new(),
        side_questions: Vec::new(),
        plan_review_events: Vec::new(),
        usage_entries: Vec::new(),
        file_path: "/tmp/session-lifecycle-order.jsonl".to_string(),
    };

    restore_visible_session_messages(&mut state, &session);

    assert_eq!(
        state
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        vec![
            "## Conversation Summary",
            "Second",
            "Notice A",
            "Notice B",
            "Second reply",
            "Notice after",
        ]
    );
    assert_eq!(state.messages[2].kind, MessageKind::System);
    assert_eq!(state.messages[3].kind, MessageKind::System);
    assert_eq!(state.messages[5].kind, MessageKind::System);
}

#[test]
fn test_scroll_boundary_handling() {
    let mut state = AppState::new();

    // scroll_up at 0 should stay at 0 (can't go below 0)
    state.scroll_up(100);
    assert_eq!(state.scroll_offset, 0);

    // scroll_down increases offset (scroll toward history)
    state.scroll_down(50);
    assert_eq!(state.scroll_offset, 50);

    // scroll_up decreases offset (scroll toward recent)
    state.scroll_up(30);
    assert_eq!(state.scroll_offset, 20);

    // scroll_up by more than current clamps to 0
    state.scroll_up(100);
    assert_eq!(state.scroll_offset, 0);
}

// ─────────────────────────────────────────────────────────────────────────
// Queue State Tests
// ─────────────────────────────────────────────────────────────────────────

/// Find the session `.jsonl` file in `dir`, filtering out its sidecar
/// `.lock` (created by every `SessionWriter::create`/`open_existing`, see
/// `session::writer::SessionLock`). A bare `read_dir().next()` over a
/// directory that now legitimately contains two files (the session
/// transcript and its lock sidecar) picks whichever one the filesystem
/// happens to list first -- not necessarily the transcript -- making a
/// test that reads "the first entry" nondeterministically read an empty
/// lock file instead.
fn find_session_jsonl(dir: &std::path::Path) -> std::path::PathBuf {
    std::fs::read_dir(dir)
        .expect("read temp session directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .expect("directory should contain a session .jsonl file")
}

pub(super) fn new_test_app() -> App {
    new_test_app_with_platform_resolution(PlatformSessionResolution::UseNoPlatformSession)
}

fn new_test_app_with_platform_resolution(resolution: PlatformSessionResolution) -> App {
    let fallback_path = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(fallback_path)
        .expect("open fallback terminal");
    let viewport_height = 24;
    let viewport_top = 1;
    let backend = ratatui::backend::CrosstermBackend::new(file);
    let terminal = ratatui::Terminal::with_options(
        backend,
        ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(
                0,
                0,
                80,
                viewport_height,
            )),
        },
    )
    .expect("create fallback terminal");
    let capabilities = crate::terminal::TerminalCapabilities {
        enhanced_keys: false,
        viewport_top,
        viewport_height,
    };
    let mut app = App::new_with_terminal_with_history_and_platform_session(
        terminal,
        capabilities,
        crate::history::PromptHistory::default(),
        None,
        None,
        false,
        resolution,
    );
    app.state.steering_mode = QueueMode::default();
    app.state.follow_up_mode = QueueMode::default();
    app
}

#[tokio::test]
async fn startup_prompt_waits_for_initial_local_discovery() {
    let mut app = new_test_app();
    let route = "llamacpp/startup-budget-model";
    app.current_model = route.to_owned();
    app.initial_prompt = Some("use the live budget".to_owned());

    let (tx, rx) = std::sync::mpsc::channel();
    app.local_model_discovery_rx = rx;
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(25));
        tx.send(crate::local_models::LocalDiscoveryBatch {
            generation: 99,
            runtimes: Vec::new(),
            models: vec![crate::model_catalog::ModelInfo {
                id: "startup-budget-model".to_owned(),
                name: "startup-budget-model".to_owned(),
                provider: "llamacpp".to_owned(),
                description: "test runtime model".to_owned(),
                capabilities: crate::model_catalog::ModelCapabilities {
                    protocol: crate::model_catalog::ModelProtocol::OpenAiChat,
                    tools: false,
                    vision: false,
                    reasoning: false,
                    streaming: true,
                    context_tokens: 8_192,
                    output_tokens: None,
                },
                verification: crate::model_catalog::ModelVerification {
                    state: crate::model_catalog::VerificationState::Verified,
                    source: "local-runtime".to_owned(),
                    detail: None,
                },
            }],
        })
        .expect("send discovery batch");
    });

    let prompt = app.take_initial_prompt_after_discovery().await;

    assert_eq!(prompt.as_deref(), Some("use the live budget"));
    assert_eq!(
        crate::local_models::find_discovered_model(route)
            .map(|model| model.capabilities.context_tokens),
        Some(8_192)
    );
}

#[tokio::test]
async fn operations_modal_routes_page_keys_to_the_focused_pane() {
    let mut app = new_test_app();
    app.active_modal = ActiveModal::Operations;
    app.operations.focus_next();

    app.handle_key(KeyCode::PageDown, CrosstermModifiers::NONE)
        .await
        .unwrap();
    assert_eq!(app.operations.scroll_offsets(), (3, 0));

    app.operations.focus_next();
    app.handle_key(KeyCode::PageDown, CrosstermModifiers::NONE)
        .await
        .unwrap();
    assert_eq!(app.operations.scroll_offsets(), (3, 3));

    app.handle_key(KeyCode::PageUp, CrosstermModifiers::NONE)
        .await
        .unwrap();
    assert_eq!(app.operations.scroll_offsets(), (3, 0));
}

#[test]
fn test_poll_workspace_scan_applies_results_and_confirms_refresh() {
    let mut app = new_test_app();
    let (tx, rx) = std::sync::mpsc::channel();
    app.workspace_scan_rx = Some(rx);

    // Nothing yet: poll is a no-op and keeps the receiver.
    assert!(!app.poll_workspace_scan());
    assert!(app.workspace_scan_rx.is_some());

    let files = vec![crate::files::WorkspaceFile {
        path: std::path::PathBuf::from("/ws/src/main.rs"),
        relative_path: "src/main.rs".to_string(),
        name: "main.rs".to_string(),
        extension: Some("rs".to_string()),
        is_dir: false,
    }];
    tx.send(files).unwrap();

    // Startup-style scan: applies files silently.
    assert!(app.poll_workspace_scan());
    assert_eq!(app.workspace_files.len(), 1);
    assert_eq!(app.workspace_files[0].relative_path, "src/main.rs");
    assert!(app.workspace_scan_rx.is_none());
    assert!(app.state.status.is_none());

    // /refresh-workspace scan: completion confirms in the status line.
    let (tx, rx) = std::sync::mpsc::channel::<Vec<crate::files::WorkspaceFile>>();
    app.workspace_scan_rx = Some(rx);
    app.workspace_refresh_pending = true;
    tx.send(Vec::new()).unwrap();
    assert!(app.poll_workspace_scan());
    assert!(app.workspace_files.is_empty());
    assert_eq!(
        app.state.status.as_deref(),
        Some("Workspace files refreshed")
    );

    // A dead scan thread (sender dropped) clears the pending refresh flag.
    let (tx, rx) = std::sync::mpsc::channel::<Vec<crate::files::WorkspaceFile>>();
    app.workspace_scan_rx = Some(rx);
    app.workspace_refresh_pending = true;
    drop(tx);
    assert!(!app.poll_workspace_scan());
    assert!(app.workspace_scan_rx.is_none());
    assert!(!app.workspace_refresh_pending);
}

// ─────────────────────────────────────────────────────────────────────────
// Spawned Tool Execution Tests
// ─────────────────────────────────────────────────────────────────────────

/// Poll until a spawned tool completion lands, failing after `secs`.
#[test]
fn test_update_viewport_capabilities_on_resize() {
    let mut app = new_test_app();
    // new_test_app starts with viewport_top 1, viewport_height 24.

    // Growing the terminal recomputes both fields.
    assert!(app.update_viewport_capabilities(40));
    assert_eq!(app.capabilities.viewport_height, 38);
    assert_eq!(app.capabilities.viewport_top, 3);

    // Same height again: no change (the caller skips terminal recreation).
    assert!(!app.update_viewport_capabilities(40));

    // Very small terminals clamp to the 10-row minimum.
    assert!(app.update_viewport_capabilities(8));
    assert_eq!(app.capabilities.viewport_height, 10);
    assert_eq!(app.capabilities.viewport_top, 1);
}

#[test]
fn test_handle_paste_routes_to_modal_or_main_input() {
    let mut app = new_test_app();

    // No modal: paste goes to the main input and strips carriage returns.
    app.handle_paste("hello\r\nworld");
    assert_eq!(app.state.input(), "hello\nworld");

    // A modal with a text input consumes the paste; the main input is
    // left untouched (previously the paste vanished into the catch-all).
    for modal in [
        ActiveModal::FileSearch,
        ActiveModal::SessionSwitcher,
        ActiveModal::CommandPalette,
        ActiveModal::ModelSelector,
        ActiveModal::ThemeSelector,
        ActiveModal::Setup,
    ] {
        app.active_modal = modal;
        app.handle_paste("query text");
        assert_eq!(
            app.state.input(),
            "hello\nworld",
            "paste leaked into main input while {modal:?} was open"
        );
    }

    // A modal without a text input ignores pastes entirely.
    app.active_modal = ActiveModal::ShortcutsHelp;
    app.handle_paste("ignored");
    assert_eq!(app.state.input(), "hello\nworld");
}

#[tokio::test]
async fn test_queue_counts_sync_on_response_start() {
    let mut app = new_test_app();

    let follow_up_id = app.reserve_queue_id();
    let steer_id = app.reserve_queue_id();
    app.enqueue_pending_prompt(
        follow_up_id,
        "follow-up".to_string(),
        PromptKind::FollowUp,
        false,
    );
    app.enqueue_pending_prompt(steer_id, "steer".to_string(), PromptKind::Steer, false);
    assert_eq!(app.state.queued_prompt_count, 2);
    assert_eq!(app.state.queued_follow_up_count, 1);
    assert_eq!(app.state.queued_steering_count, 1);

    app.state.busy = false;
    app.handle_agent_message(FromAgent::ResponseStart {
        response_id: "resp-1".to_string(),
    })
    .await
    .expect("handle response start");

    assert_eq!(app.state.queued_prompt_count, 1);
    assert_eq!(app.state.queued_follow_up_count, 0);
    assert_eq!(app.state.queued_steering_count, 1);
}

#[tokio::test]
async fn test_queue_response_start_drains_all_leading_steers_in_all_mode() {
    let mut app = new_test_app();

    let steer_one = app.reserve_queue_id();
    let steer_two = app.reserve_queue_id();
    let follow_up = app.reserve_queue_id();
    app.enqueue_pending_prompt(steer_one, "steer one".to_string(), PromptKind::Steer, true);
    app.enqueue_pending_prompt(steer_two, "steer two".to_string(), PromptKind::Steer, true);
    app.enqueue_pending_prompt(
        follow_up,
        "follow-up".to_string(),
        PromptKind::FollowUp,
        false,
    );

    app.handle_agent_message(FromAgent::ResponseStart {
        response_id: "resp-batch".to_string(),
    })
    .await
    .expect("handle response start");

    assert_eq!(app.state.queued_prompt_count, 1);
    assert_eq!(app.state.queued_steering_count, 0);
    assert_eq!(app.state.queued_follow_up_count, 1);
    assert_eq!(
        app.queued_prompt_active.as_ref().map(|prompt| prompt.id),
        Some(steer_one)
    );
}

#[tokio::test]
async fn test_queued_prompt_activates_matching_skills() {
    let mut app = new_test_app();
    app.skill_registry.register(
        crate::skills::SkillDefinition::new("parser", "Parser")
            .with_triggers(vec!["parser".into()])
            .with_system_prompt("Use parser conventions."),
    );

    // Busy, so the follow-up is queued for the runner to drain instead of
    // being submitted through `submit_prompt_with_kind`.
    app.state.busy = true;
    app.handle_follow_up_submit("Review the parser changes".to_string())
        .await
        .expect("queue the follow-up");

    assert!(
        app.skill_registry
            .get("parser")
            .expect("skill is registered")
            .is_active(),
        "a queued prompt must activate the skills its triggers match"
    );
    assert!(
        app.skill_registry
            .active_system_prompt_additions()
            .contains("parser conventions")
    );
    assert!(
        app.state.messages.iter().any(|message| message
            .content
            .contains("Automatically activated skills: parser")),
        "the activation must be reported the same way a direct submission reports it"
    );
}

#[tokio::test]
async fn test_queued_side_question_does_not_activate_skills() {
    let mut app = new_test_app();
    app.skill_registry.register(
        crate::skills::SkillDefinition::new("parser", "Parser")
            .with_triggers(vec!["parser".into()])
            .with_system_prompt("Use parser conventions."),
    );

    app.state.busy = true;
    app.queue_prompt(
        "What does the parser do?".to_string(),
        PromptKind::SideQuestion,
        false,
    )
    .await
    .expect("queue the side question");

    assert!(
        !app.skill_registry
            .get("parser")
            .expect("skill is registered")
            .is_active(),
        "side questions do not activate skills on the direct path either"
    );
}

#[tokio::test]
async fn side_question_terminal_returns_app_to_idle_on_success_or_error() {
    for error in [None, Some("unexpected eof".to_string())] {
        let mut app = new_test_app();
        app.state.busy = true;
        app.queued_prompt_inflight = Some(QueuedPromptCursor { id: 7 });
        app.queued_prompt_active = Some(QueuedPrompt {
            id: 7,
            content: "side question".to_string(),
            kind: PromptKind::SideQuestion,
        });

        app.handle_agent_message(FromAgent::SideQuestionEnd {
            side_id: "side-1".to_string(),
            question: "side question".to_string(),
            answer: "answer".to_string(),
            standalone: true,
            error,
            provider_error_kind: None,
            usage: None,
        })
        .await
        .expect("side-question terminal");

        assert!(!app.state.busy);
        assert!(app.queued_prompt_inflight.is_none());
        assert!(app.queued_prompt_active.is_none());
    }

    let mut app = new_test_app();
    app.state.busy = true;
    app.handle_agent_message(FromAgent::SideQuestionEnd {
        side_id: "queued-side".to_string(),
        question: "queued side question".to_string(),
        answer: "answer".to_string(),
        standalone: false,
        error: None,
        provider_error_kind: None,
        usage: None,
    })
    .await
    .expect("queued side-question terminal");
    assert!(
        app.state.busy,
        "queued side question must not end the main turn"
    );
}

#[test]
fn test_queue_enqueue_pending_prompt_preserves_steer_fifo() {
    let mut app = new_test_app();

    let steer_one = app.reserve_queue_id();
    let follow_up = app.reserve_queue_id();
    let steer_two = app.reserve_queue_id();
    app.enqueue_pending_prompt(steer_one, "steer one".to_string(), PromptKind::Steer, true);
    app.enqueue_pending_prompt(
        follow_up,
        "follow-up".to_string(),
        PromptKind::FollowUp,
        false,
    );
    app.enqueue_pending_prompt(steer_two, "steer two".to_string(), PromptKind::Steer, true);

    let ordered: Vec<(u64, PromptKind)> = app
        .queued_prompts
        .iter()
        .map(|prompt| (prompt.id, prompt.kind))
        .collect();
    assert_eq!(
        ordered,
        vec![
            (steer_one, PromptKind::Steer),
            (steer_two, PromptKind::Steer),
            (follow_up, PromptKind::FollowUp),
        ]
    );
}

#[test]
fn test_describe_next_queue_batch_is_mode_aware() {
    assert_eq!(
        App::describe_next_queue_batch(0, QueueMode::All, "after turn end"),
        None
    );
    assert_eq!(
        App::describe_next_queue_batch(1, QueueMode::All, "after turn end"),
        Some("1 message after turn end".to_string())
    );
    assert_eq!(
        App::describe_next_queue_batch(3, QueueMode::All, "after turn end"),
        Some("all 3 messages after turn end".to_string())
    );
    assert_eq!(
        App::describe_next_queue_batch(3, QueueMode::One, "at the next tool boundary"),
        Some("1 of 3 messages at the next tool boundary".to_string())
    );
}

#[test]
fn test_merge_queued_prompt_batch_joins_non_empty_segments() {
    let merged = App::merge_queued_prompt_batch(&[
        QueuedPrompt {
            id: 1,
            content: "steer first".to_string(),
            kind: PromptKind::Steer,
        },
        QueuedPrompt {
            id: 2,
            content: "   ".to_string(),
            kind: PromptKind::Steer,
        },
        QueuedPrompt {
            id: 3,
            content: "steer second".to_string(),
            kind: PromptKind::Steer,
        },
    ]);

    assert_eq!(merged, "steer first\n\nsteer second");
}

#[test]
fn test_drain_queued_steering_batch_for_interrupt_respects_mode() {
    let mut app = new_test_app();
    let steer_one = app.reserve_queue_id();
    let steer_two = app.reserve_queue_id();
    let follow_up = app.reserve_queue_id();
    app.enqueue_pending_prompt(steer_one, "steer one".to_string(), PromptKind::Steer, true);
    app.enqueue_pending_prompt(steer_two, "steer two".to_string(), PromptKind::Steer, true);
    app.enqueue_pending_prompt(
        follow_up,
        "follow-up".to_string(),
        PromptKind::FollowUp,
        false,
    );

    let drained_all = app.drain_queued_steering_batch_for_interrupt();
    assert_eq!(drained_all.len(), 2);
    assert_eq!(app.queued_prompts.len(), 1);
    assert_eq!(app.queued_prompts.front().unwrap().id, follow_up);

    let mut app = new_test_app();
    app.state.steering_mode = QueueMode::One;
    let steer_one = app.reserve_queue_id();
    let steer_two = app.reserve_queue_id();
    app.enqueue_pending_prompt(steer_one, "steer one".to_string(), PromptKind::Steer, true);
    app.enqueue_pending_prompt(steer_two, "steer two".to_string(), PromptKind::Steer, true);

    let drained_one = app.drain_queued_steering_batch_for_interrupt();
    assert_eq!(drained_one.len(), 1);
    assert_eq!(drained_one[0].id, steer_one);
    assert_eq!(app.queued_prompts.front().unwrap().id, steer_two);
}

#[tokio::test]
async fn test_queue_counts_clear_on_interrupt() {
    let mut app = new_test_app();

    let follow_up_id = app.reserve_queue_id();
    app.enqueue_pending_prompt(
        follow_up_id,
        "follow-up".to_string(),
        PromptKind::FollowUp,
        false,
    );
    app.state.busy = true;

    app.handle_key(KeyCode::Char('c'), CrosstermModifiers::CONTROL)
        .await
        .expect("interrupt");

    assert_eq!(app.state.input(), "follow-up");
    assert_eq!(app.state.queued_prompt_count, 0);
    assert!(app.queued_prompts.is_empty());
}

#[tokio::test]
async fn test_interrupt_restore_merges_existing_input_after_queued_prompts() {
    let mut app = new_test_app();

    let follow_up_id = app.reserve_queue_id();
    app.enqueue_pending_prompt(
        follow_up_id,
        "follow-up".to_string(),
        PromptKind::FollowUp,
        false,
    );
    app.state.set_input("existing draft");
    app.state.busy = true;

    app.handle_key(KeyCode::Char('c'), CrosstermModifiers::CONTROL)
        .await
        .expect("interrupt");

    assert_eq!(app.state.input(), "follow-up\n\nexisting draft");
    assert_eq!(app.state.queued_prompt_count, 0);
    assert!(app.queued_prompts.is_empty());
}

#[tokio::test]
async fn test_interrupt_with_queued_steer_restores_steering_batch_without_agent() {
    let mut app = new_test_app();

    let steer_id = app.reserve_queue_id();
    let follow_up_id = app.reserve_queue_id();
    app.enqueue_pending_prompt(steer_id, "steer".to_string(), PromptKind::Steer, true);
    app.enqueue_pending_prompt(
        follow_up_id,
        "follow-up".to_string(),
        PromptKind::FollowUp,
        false,
    );
    app.state.busy = true;

    app.handle_key(KeyCode::Char('c'), CrosstermModifiers::CONTROL)
        .await
        .expect("interrupt");

    assert_eq!(app.state.input(), "steer");
    assert_eq!(app.state.queued_prompt_count, 1);
    assert_eq!(app.state.queued_follow_up_count, 1);
    assert_eq!(app.queued_prompts.front().unwrap().id, follow_up_id);
}

#[tokio::test]
async fn test_queue_overflow_with_inflight_does_not_drop() {
    let mut app = new_test_app();

    for _ in 0..MAX_PENDING_MESSAGES {
        let id = app.reserve_queue_id();
        app.enqueue_pending_prompt(id, "follow-up".to_string(), PromptKind::FollowUp, false);
    }

    let inflight_id = app.queued_prompts.front().unwrap().id;
    app.queued_prompt_inflight = Some(QueuedPromptCursor { id: inflight_id });
    app.sync_queue_prompt_count();

    let new_id = app.reserve_queue_id();
    let dropped =
        app.enqueue_pending_prompt(new_id, "extra".to_string(), PromptKind::FollowUp, false);

    assert!(dropped.is_none());
    assert_eq!(app.queued_prompts.len(), MAX_PENDING_MESSAGES + 1);
    assert_eq!(app.state.queued_prompt_count, MAX_PENDING_MESSAGES);
}

#[test]
fn test_queue_cancel_by_id() {
    let mut app = new_test_app();

    let first_id = app.reserve_queue_id();
    let second_id = app.reserve_queue_id();
    app.enqueue_pending_prompt(first_id, "first".to_string(), PromptKind::FollowUp, false);
    app.enqueue_pending_prompt(second_id, "second".to_string(), PromptKind::Steer, false);

    app.handle_queue_action(QueueAction::Cancel { id: first_id });

    assert_eq!(app.queued_prompts.len(), 1);
    assert_eq!(app.queued_prompts.front().unwrap().id, second_id);
    assert_eq!(app.state.queued_prompt_count, 1);
}

#[test]
fn test_queue_reorder_and_send_now_preserve_all_prompts() {
    let mut app = new_test_app();
    let first_id = app.reserve_queue_id();
    let second_id = app.reserve_queue_id();
    let third_id = app.reserve_queue_id();
    app.enqueue_pending_prompt(first_id, "first".to_string(), PromptKind::FollowUp, false);
    app.enqueue_pending_prompt(second_id, "second".to_string(), PromptKind::FollowUp, false);
    app.enqueue_pending_prompt(third_id, "third".to_string(), PromptKind::FollowUp, false);

    assert!(
        app.move_queued_prompt(second_id, QueueMoveDirection::Down)
            .is_some()
    );
    assert_eq!(
        app.queued_prompts
            .iter()
            .map(|prompt| prompt.id)
            .collect::<Vec<_>>(),
        vec![first_id, third_id, second_id]
    );
    assert!(
        app.move_queued_prompt(second_id, QueueMoveDirection::Now)
            .is_some()
    );
    assert_eq!(
        app.queued_prompts
            .iter()
            .map(|prompt| prompt.id)
            .collect::<Vec<_>>(),
        vec![second_id, first_id, third_id]
    );
}

#[tokio::test]
async fn test_alt_up_restores_most_recent_queued_follow_up() {
    let mut app = new_test_app();
    app.queued_follow_up_edit_binding = crate::key_hints::alt(KeyCode::Up);
    app.state.queued_follow_up_edit_binding_label = "Alt+Up".to_string();

    let follow_up_id = app.reserve_queue_id();
    let steer_id = app.reserve_queue_id();
    app.enqueue_pending_prompt(
        follow_up_id,
        "follow-up draft".to_string(),
        PromptKind::FollowUp,
        false,
    );
    app.enqueue_pending_prompt(
        steer_id,
        "pending steer".to_string(),
        PromptKind::Steer,
        true,
    );

    app.handle_key(KeyCode::Up, CrosstermModifiers::ALT)
        .await
        .expect("restore queued follow-up");

    assert_eq!(app.state.input(), "follow-up draft");
    assert_eq!(app.state.queued_follow_up_count, 0);
    assert_eq!(app.state.queued_steering_count, 1);
    assert_eq!(app.queued_prompts.len(), 1);
    assert_eq!(app.queued_prompts.front().unwrap().id, steer_id);
}

#[tokio::test]
async fn test_alt_up_cycles_to_older_queued_follow_up_without_losing_current_draft() {
    let mut app = new_test_app();
    app.queued_follow_up_edit_binding = crate::key_hints::alt(KeyCode::Up);
    app.state.queued_follow_up_edit_binding_label = "Alt+Up".to_string();

    let first_id = app.reserve_queue_id();
    let second_id = app.reserve_queue_id();
    app.enqueue_pending_prompt(
        first_id,
        "follow-up first".to_string(),
        PromptKind::FollowUp,
        false,
    );
    app.enqueue_pending_prompt(
        second_id,
        "follow-up second".to_string(),
        PromptKind::FollowUp,
        false,
    );

    app.handle_key(KeyCode::Up, CrosstermModifiers::ALT)
        .await
        .expect("restore newest queued follow-up");
    assert_eq!(app.state.input(), "follow-up second");

    app.state.set_input("follow-up second edited");

    app.handle_key(KeyCode::Up, CrosstermModifiers::ALT)
        .await
        .expect("cycle to older queued follow-up");

    assert_eq!(app.state.input(), "follow-up first");
    let queued: Vec<(u64, String)> = app
        .queued_prompts
        .iter()
        .map(|prompt| (prompt.id, prompt.content.clone()))
        .collect();
    assert_eq!(
        queued,
        vec![(second_id, "follow-up second edited".to_string())]
    );
}

#[test]
fn test_queued_follow_up_edit_binding_prefers_shift_left_for_special_terminals() {
    assert_eq!(
        queued_follow_up_edit_binding_for_terminal_name("Apple_Terminal", false),
        crate::key_hints::shift(KeyCode::Left)
    );
    assert_eq!(
        queued_follow_up_edit_binding_for_terminal_name("WarpTerminal", false),
        crate::key_hints::shift(KeyCode::Left)
    );
    assert_eq!(
        queued_follow_up_edit_binding_for_terminal_name("vscode", false),
        crate::key_hints::shift(KeyCode::Left)
    );
    assert_eq!(
        queued_follow_up_edit_binding_for_terminal_name("WezTerm", false),
        crate::key_hints::alt(KeyCode::Up)
    );
    assert_eq!(
        queued_follow_up_edit_binding_for_terminal_name("iTerm.app", true),
        crate::key_hints::shift(KeyCode::Left)
    );
}

#[test]
fn test_should_queue_follow_up_on_tab_only_when_busy_with_non_command_input() {
    let mut app = new_test_app();
    assert!(!app.should_queue_follow_up_on_tab());

    app.state.busy = true;
    app.state.follow_up_mode = QueueMode::One;
    app.state.set_input("follow-up");
    assert!(!app.should_queue_follow_up_on_tab());

    app.state.follow_up_mode = QueueMode::All;
    app.state.set_input("");
    assert!(!app.should_queue_follow_up_on_tab());

    app.state.set_input("/help");
    assert!(!app.should_queue_follow_up_on_tab());

    app.state.set_input("!ls");
    assert!(!app.should_queue_follow_up_on_tab());

    app.state.set_input("follow-up");
    assert!(app.should_queue_follow_up_on_tab());
}

#[test]
fn test_should_submit_on_tab_only_when_idle_with_non_blocked_input() {
    let mut app = new_test_app();
    assert!(!app.should_submit_on_tab());

    app.state.busy = true;
    app.state.set_input("submit me");
    assert!(!app.should_submit_on_tab());

    app.state.busy = false;
    app.state.set_input("");
    assert!(!app.should_submit_on_tab());

    app.state.set_input("   ");
    assert!(!app.should_submit_on_tab());

    app.state.set_input("!ls");
    assert!(!app.should_submit_on_tab());

    app.state.set_input("/help");
    assert!(!app.should_submit_on_tab());

    app.state.set_input(" /help");
    assert!(!app.should_submit_on_tab());

    app.state.set_input("submit me");
    assert!(app.should_submit_on_tab());
}

#[test]
fn test_show_help_uses_maestro_branding() {
    let mut app = new_test_app();
    app.show_help();

    let last = app.state.messages.last().expect("help message");
    assert!(
        last.content
            .contains("Deixic Code TUI - Keyboard Shortcuts")
    );
    assert!(!last.content.contains("Composer TUI - Keyboard Shortcuts"));
}

#[test]
fn test_show_help_uses_rebound_shortcut_labels() {
    let _guard = acquire_keybindings_test_lock();
    let temp = tempdir().expect("tempdir");
    let config_path = temp.path().join("keybindings.json");
    std::fs::write(
        &config_path,
        r#"{
  "version": 1,
  "rustBindings": {
"command-palette": "Ctrl+O",
"file-search": "Ctrl+P",
"toggle-tool-outputs": "Shift+Left"
  }
}"#,
    )
    .expect("write keybindings");

    std::env::set_var("MAESTRO_KEYBINDINGS_FILE", &config_path);
    let mut app = new_test_app();
    app.show_help();
    std::env::remove_var("MAESTRO_KEYBINDINGS_FILE");

    let last = app.state.messages.last().expect("help message");
    assert!(
        last.content
            .lines()
            .any(|line| line.contains("Open command palette") && !line.contains("Ctrl+P"))
    );
    assert!(
        last.content
            .lines()
            .any(|line| line.contains("Open file search") && !line.contains("Ctrl+O"))
    );
    assert!(
        last.content.lines().any(|line| {
            line.contains("Toggle tool call expansion") && !line.contains("Ctrl+T")
        })
    );
}

#[test]
fn test_invalid_keybindings_config_surfaces_startup_warning() {
    let _guard = acquire_keybindings_test_lock();
    let temp = tempdir().expect("tempdir");
    let config_path = temp.path().join("keybindings.json");
    std::fs::write(
        &config_path,
        r#"{
  "version": 1,
  "rustBindings": {
"command-palette": "Ctrl+O",
"file-search": "Ctrl+O"
  }
}"#,
    )
    .expect("write keybindings");

    std::env::set_var("MAESTRO_KEYBINDINGS_FILE", &config_path);
    let app = new_test_app();
    std::env::remove_var("MAESTRO_KEYBINDINGS_FILE");

    let first = app.state.messages.first().expect("startup warning");
    assert!(
        first
            .content
            .contains("Keyboard shortcuts config has 1 issue. Run /hotkeys validate.")
    );
}

#[tokio::test]
async fn test_rebound_shortcuts_open_expected_modals() {
    let _guard = acquire_keybindings_test_lock_async().await;
    let temp = tempdir().expect("tempdir");
    let config_path = temp.path().join("keybindings.json");
    std::fs::write(
        &config_path,
        r#"{
  "version": 1,
  "rustBindings": {
"command-palette": "Ctrl+O",
"file-search": "Ctrl+P"
  }
}"#,
    )
    .expect("write keybindings");

    std::env::set_var("MAESTRO_KEYBINDINGS_FILE", &config_path);
    let mut app = new_test_app();
    std::env::remove_var("MAESTRO_KEYBINDINGS_FILE");

    app.handle_key(KeyCode::Char('o'), CrosstermModifiers::CONTROL)
        .await
        .expect("open command palette");
    assert_eq!(app.active_modal, ActiveModal::CommandPalette);

    app.active_modal = ActiveModal::None;
    app.handle_key(KeyCode::Char('p'), CrosstermModifiers::CONTROL)
        .await
        .expect("open file search");
    assert_eq!(app.active_modal, ActiveModal::FileSearch);
}

#[tokio::test]
async fn question_mark_opens_palette_only_from_empty_composer() {
    let mut app = new_test_app();

    app.handle_key(KeyCode::Char('?'), CrosstermModifiers::NONE)
        .await
        .expect("open command palette");
    assert_eq!(app.active_modal, ActiveModal::CommandPalette);
    assert!(app.state.input().is_empty());

    app.active_modal = ActiveModal::None;
    app.state.set_input("explain ");
    app.handle_key(KeyCode::Char('?'), CrosstermModifiers::NONE)
        .await
        .expect("type question mark");
    assert_eq!(app.active_modal, ActiveModal::None);
    assert_eq!(app.state.input(), "explain ?");
}

#[tokio::test]
async fn test_handle_config_event_reloads_keybindings() {
    let _guard = acquire_keybindings_test_lock_async().await;
    let temp = tempdir().expect("tempdir");
    let config_path = temp.path().join("keybindings.json");
    std::fs::write(
        &config_path,
        r#"{
  "version": 1,
  "rustBindings": {
"command-palette": "Ctrl+O",
"file-search": "Ctrl+P",
"toggle-tool-outputs": "Shift+Left"
  }
}"#,
    )
    .expect("write keybindings");

    std::env::set_var("MAESTRO_KEYBINDINGS_FILE", &config_path);
    let mut app = new_test_app();

    std::fs::write(
        &config_path,
        r#"{
  "version": 1,
  "rustBindings": {
"command-palette": "Ctrl+P",
"file-search": "Ctrl+O",
"toggle-tool-outputs": "Ctrl+T",
"edit-last-follow-up": "Shift+Left"
  }
}"#,
    )
    .expect("rewrite keybindings");

    app.handle_config_event(ConfigEvent::Changed(config_path.clone()))
        .await;
    std::env::remove_var("MAESTRO_KEYBINDINGS_FILE");

    let expected_command_palette = crate::key_hints::ctrl(KeyCode::Char('p')).display();
    let expected_file_search = crate::key_hints::ctrl(KeyCode::Char('o')).display();
    let expected_toggle_tool_outputs = crate::key_hints::ctrl(KeyCode::Char('t')).display();
    let expected_edit_last_follow_up = crate::key_hints::shift(KeyCode::Left).display();

    assert_eq!(
        app.command_palette_binding.display(),
        expected_command_palette
    );
    assert_eq!(app.file_search_binding.display(), expected_file_search);
    assert_eq!(
        app.toggle_tool_outputs_binding.display(),
        expected_toggle_tool_outputs
    );
    assert_eq!(
        app.state.queued_follow_up_edit_binding_label,
        expected_edit_last_follow_up
    );

    app.show_help();
    let last = app.state.messages.last().expect("help message");
    assert!(
        last.content
            .lines()
            .any(|line| line.contains("Open command palette")
                && line.contains(expected_command_palette.as_str()))
    );
    assert!(
        last.content
            .lines()
            .any(|line| line.contains("Open file search")
                && line.contains(expected_file_search.as_str()))
    );
}

#[tokio::test]
async fn test_tab_submits_when_idle_with_non_shell_input() {
    let mut app = new_test_app();
    app.state.set_input("ship it");

    app.handle_key(KeyCode::Tab, CrosstermModifiers::NONE)
        .await
        .unwrap();

    assert_eq!(app.state.input(), "");
    let last = app.state.messages.last().expect("user message");
    assert_eq!(last.role, MessageRole::User);
    assert_eq!(last.content, "ship it");
}

#[tokio::test]
async fn test_tab_does_not_submit_idle_shell_draft() {
    let mut app = new_test_app();
    let initial_message_count = app.state.messages.len();
    app.state.set_input("!ls");

    app.handle_key(KeyCode::Tab, CrosstermModifiers::NONE)
        .await
        .unwrap();

    assert_eq!(app.state.input(), "!ls");
    assert_eq!(app.state.messages.len(), initial_message_count);
}

#[tokio::test]
async fn test_tab_does_not_submit_idle_slash_draft_with_leading_space() {
    let mut app = new_test_app();
    let initial_message_count = app.state.messages.len();
    app.state.set_input(" /help");

    app.handle_key(KeyCode::Tab, CrosstermModifiers::NONE)
        .await
        .unwrap();

    assert_eq!(app.state.input(), " /help");
    assert_eq!(app.state.messages.len(), initial_message_count);
}

#[tokio::test]
async fn test_alt_enter_inserts_newline_when_idle() {
    let mut app = new_test_app();
    app.state.set_input("hello");

    app.handle_key(KeyCode::Enter, CrosstermModifiers::ALT)
        .await
        .unwrap();

    assert_eq!(app.state.input(), "hello\n");
}

#[tokio::test]
async fn test_alt_enter_inserts_newline_for_busy_shell_draft() {
    let mut app = new_test_app();
    app.state.busy = true;
    app.state.set_input("!ls");

    app.handle_key(KeyCode::Enter, CrosstermModifiers::ALT)
        .await
        .unwrap();

    assert_eq!(app.state.input(), "!ls\n");
    assert_eq!(app.state.queued_prompt_count, 0);
}

#[tokio::test]
async fn test_queue_counts_clear_on_error() {
    let mut app = new_test_app();

    let id = app.reserve_queue_id();
    app.enqueue_pending_prompt(id, "follow-up".to_string(), PromptKind::FollowUp, false);
    app.queued_prompt_inflight = Some(QueuedPromptCursor { id });
    app.sync_queue_prompt_count();

    app.state.busy = true;
    app.handle_agent_message(FromAgent::Error {
        message: "oops".to_string(),
        fatal: false,
        terminal: true,
        retryable: false,
    })
    .await
    .expect("handle error");

    assert!(!app.state.busy);
    assert!(app.queued_prompt_inflight.is_none());
    assert_eq!(app.state.queued_prompt_count, 1);
}

#[tokio::test]
async fn test_alerts_command_lists_recorded_alerts() {
    use crate::commands::CommandAction;

    let mut app = new_test_app();
    app.handle_agent_message(FromAgent::Error {
        message: "API error 400 Bad Request: invalid_request_error: messages.0: empty".to_string(),
        fatal: false,
        terminal: false,
        retryable: false,
    })
    .await
    .expect("handle error");
    app.handle_agent_message(FromAgent::Error {
        message: "API error 429 Too Many Requests: rate_limit_error: slow down".to_string(),
        fatal: false,
        terminal: false,
        retryable: false,
    })
    .await
    .expect("handle error");
    assert_eq!(app.state.unseen_alerts, 2);

    app.handle_command_action(CommandAction::ShowAlerts).await;

    let listing = &app
        .state
        .messages
        .last()
        .expect("alerts listing message")
        .content;
    assert!(listing.contains("2 recorded"), "listing: {listing}");
    assert!(
        listing.contains("invalid_request_error: messages.0: empty"),
        "listing: {listing}"
    );
    assert!(
        listing.contains("rate_limit_error: slow down"),
        "listing: {listing}"
    );
    assert_eq!(app.state.unseen_alerts, 0);
}

#[tokio::test]
async fn test_alerts_command_without_alerts() {
    use crate::commands::CommandAction;

    let mut app = new_test_app();
    app.handle_command_action(CommandAction::ShowAlerts).await;

    let listing = &app
        .state
        .messages
        .last()
        .expect("alerts listing message")
        .content;
    assert!(listing.contains("No alerts recorded"), "listing: {listing}");
}

#[test]
fn a_startup_restored_session_adopts_its_own_scope() {
    // A session restored at startup exists before the runner does, so it never
    // passed through any transition that adopts a session context: the executor
    // kept its initial random scope and completions parked for this session's
    // own children were never drained.
    let mut app = new_test_app();
    let initial = app.tool_executor.subagent_parent_scope_id();
    app.state.session_id = Some("restored-alpha".to_string());

    app.adopt_session_context(Some("restored-alpha"), "restore");

    assert_ne!(app.tool_executor.subagent_parent_scope_id(), initial);
    assert_eq!(
        app.tool_executor.subagent_parent_scope_id(),
        "session:restored-alpha",
        "a restored session must drain the scope its own children carry"
    );
}

#[tokio::test]
async fn a_new_session_rotates_the_subagent_scope() {
    use crate::commands::{CommandAction, SessionAction};

    // The tool executor lives behind an `Arc` for the whole process, so
    // without a rotation a child still running under the previous conversation
    // reported its completion into this one, and that summary went on to the
    // next model request.
    let mut app = new_test_app();
    app.adopt_session_context(Some("alpha"), "resume");
    assert_eq!(
        app.tool_executor.subagent_parent_scope_id(),
        "session:alpha"
    );

    app.handle_command_action(CommandAction::Session(SessionAction::New))
        .await;

    assert_ne!(
        app.tool_executor.subagent_parent_scope_id(),
        "session:alpha",
        "a new session must not drain the previous session's children"
    );
}

#[tokio::test]
async fn resuming_a_session_re_adopts_the_scope_it_rotated_away_from() {
    use crate::commands::{CommandAction, SessionAction};

    // Completions from a session's own children are parked rather than
    // discarded, so re-adopting the same scope is what surfaces them.
    let mut app = new_test_app();
    app.adopt_session_context(Some("alpha"), "resume");
    app.handle_command_action(CommandAction::Session(SessionAction::New))
        .await;
    assert_ne!(
        app.tool_executor.subagent_parent_scope_id(),
        "session:alpha"
    );

    app.adopt_session_context(Some("alpha"), "resume");

    assert_eq!(
        app.tool_executor.subagent_parent_scope_id(),
        "session:alpha",
        "resuming a session must return to the scope its children carry"
    );
}

#[test]
fn a_session_scope_is_derived_from_its_id() {
    assert_eq!(subagent_scope_for_session(Some("alpha")), "session:alpha");
    assert_eq!(
        subagent_scope_for_session(Some("alpha")),
        subagent_scope_for_session(Some("alpha")),
        "resuming the same session must produce the same scope"
    );
    assert_ne!(
        subagent_scope_for_session(None),
        subagent_scope_for_session(None),
        "a session with no id yet must not collide with another"
    );
}

#[tokio::test]
async fn test_new_session_clears_error_surface_and_alerts() {
    use crate::commands::{CommandAction, SessionAction};

    let mut app = new_test_app();
    app.state.error = Some("stale API error".to_string());
    app.state.record_alert("stale API error".to_string());
    app.state
        .add_system_message("old transcript line".to_string());

    app.handle_command_action(CommandAction::Session(SessionAction::New))
        .await;

    assert!(app.state.error.is_none(), "error surface must be cleared");
    assert!(app.state.alerts.is_empty(), "alert history resets");
    assert_eq!(app.state.unseen_alerts, 0);
    assert!(
        !app.state
            .messages
            .iter()
            .any(|m| m.content.contains("old transcript line")),
        "old transcript must not survive /clear"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// Input Cursor Tests
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn test_cursor_movement() {
    let mut state = AppState::new();
    state.set_input("Hello World");

    // Cursor starts at end
    let initial_cursor = state.cursor();

    state.move_left();
    assert!(state.cursor() < initial_cursor || initial_cursor == 0);

    state.move_right();
    // Cursor should move right (or stay at end)

    state.move_home();
    assert_eq!(state.cursor(), 0);

    state.move_end();
    assert_eq!(state.cursor(), state.input().len());
}

#[test]
fn test_delete_operation() {
    let mut state = AppState::new();
    state.set_input("Hello");
    state.move_home();
    state.delete(); // Delete 'H'
    assert_eq!(state.input(), "ello");
}

// ─────────────────────────────────────────────────────────────────────────
// Error and Status Message Tests

#[test]
fn session_persistence_errors_explain_disk_full_recovery() {
    let disk_full = format_session_persistence_error(
        "persist session data",
        std::io::Error::other("No space left on device"),
    );
    assert!(disk_full.starts_with("Disk full:"));
    assert!(disk_full.contains("Free disk space"));
    assert!(disk_full.contains("persist session data"));

    let ordinary = format_session_persistence_error(
        "flush the session transcript",
        std::io::Error::other("permission denied"),
    );
    assert_eq!(
        ordinary,
        "Failed to flush the session transcript: permission denied"
    );
}

#[tokio::test]
async fn turn_summary_waits_for_idle_sentinel_and_includes_tool_facts() {
    let mut app = new_test_app();
    app.state
        .add_user_message("inspect the workspace".to_string());

    app.handle_agent_message(FromAgent::ResponseStart {
        response_id: "response-summary".to_string(),
    })
    .await
    .expect("response start");
    app.handle_agent_message(FromAgent::ToolCall {
        call_id: "call-summary".to_string(),
        tool: "read".to_string(),
        args: serde_json::json!({"file_path": "README.md"}),
        requires_approval: false,
        approval_inline_env: None,
    })
    .await
    .expect("tool call");
    app.state
        .complete_tool_call("call-summary", "read complete");

    app.handle_agent_message(FromAgent::ResponseEnd {
        response_id: "response-summary".to_string(),
        usage: Some(crate::agent::TokenUsage {
            input_tokens: 1_200,
            output_tokens: 300,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cost: None,
        }),
    })
    .await
    .expect("response end");
    assert!(
        !app.state
            .status
            .as_deref()
            .is_some_and(|status| status.starts_with("Turn complete")),
        "a model response ending is not the idle turn sentinel"
    );

    app.handle_agent_message(FromAgent::ResponseEnd {
        response_id: "done".to_string(),
        usage: None,
    })
    .await
    .expect("legacy done response end");
    assert!(
        !app.state
            .status
            .as_deref()
            .is_some_and(|status| status.starts_with("Turn complete")),
        "the legacy done response is not a turn terminal"
    );

    app.handle_agent_message(FromAgent::TurnCompleted {
        response_id: "done".to_string(),
        coding_completion: None,
        coding_child_records: Vec::new(),
    })
    .await
    .expect("explicit turn completion");
    let status = app.state.status.as_deref().unwrap_or_default();
    assert!(
        status.starts_with("Turn complete"),
        "unexpected status: {status:?}"
    );
    assert!(status.contains("1 tool (1 ok)"));
    assert!(status.contains("1.2k in / 300 out"));
}
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn test_error_and_status_fields() {
    let mut state = AppState::new();

    assert!(state.error.is_none());
    assert!(state.status.is_none());

    state.error = Some("Test error".to_string());
    assert_eq!(state.error, Some("Test error".to_string()));

    state.status = Some("Connected".to_string());
    assert_eq!(state.status, Some("Connected".to_string()));
}

#[test]
fn test_render_mcp_status_lines_include_source_transport_and_error() {
    let lines = render_mcp_status_lines(&[crate::tools::McpServerStatus {
        name: "remote".to_string(),
        state: crate::tools::McpLifecycleState::Failed,
        connected: false,
        scope: McpConfigScope::Project,
        transport: McpTransport::Sse,
        error: Some("Connection refused".to_string()),
        tools: Vec::new(),
        disabled_tools: Vec::new(),
        resources: Vec::new(),
        prompts: Vec::new(),
    }]);

    let rendered = lines.join("\n");
    assert!(rendered.contains("- remote (disconnected)"));
    assert!(rendered.contains("Source: Project config"));
    assert!(rendered.contains("Transport: SSE"));
    assert!(rendered.contains("Error: Connection refused"));
}

#[test]
fn test_render_mcp_status_lines_use_blank_error_fallback() {
    let lines = render_mcp_status_lines(&[crate::tools::McpServerStatus {
        name: "offline".to_string(),
        state: crate::tools::McpLifecycleState::Failed,
        connected: false,
        scope: McpConfigScope::User,
        transport: McpTransport::Stdio,
        error: Some("   ".to_string()),
        tools: Vec::new(),
        disabled_tools: Vec::new(),
        resources: Vec::new(),
        prompts: Vec::new(),
    }]);

    let rendered = lines.join("\n");
    assert!(rendered.contains("Error: Connection failed."));
}

#[test]
fn test_render_mcp_prompt_lines_include_metadata_and_argument_summaries() {
    let lines = render_mcp_prompt_lines(
        &[(
            "docs".to_string(),
            vec![McpPrompt {
                name: "summarize".to_string(),
                title: Some("Summarize Docs".to_string()),
                description: Some("Summarize the selected documentation.".to_string()),
                arguments: Some(vec![crate::mcp::McpPromptArgument {
                    name: "topic".to_string(),
                    description: Some("Topic to summarize".to_string()),
                    required: true,
                }]),
            }],
        )],
        None,
    );

    let rendered = lines.join("\n");
    assert!(rendered.contains("docs:"));
    assert!(rendered.contains("  summarize"));
    assert!(rendered.contains("    title: Summarize Docs"));
    assert!(rendered.contains("    description: Summarize the selected documentation."));
    assert!(rendered.contains("    args: topic (required): Topic to summarize"));
    assert!(rendered.contains("Usage: /mcp prompts <server> <name> [KEY=value ...]"));
}

#[test]
fn test_render_mcp_prompt_lines_show_server_specific_empty_state() {
    let lines = render_mcp_prompt_lines(&[], Some("docs"));
    let rendered = lines.join("\n");
    assert!(rendered.contains("Server 'docs' does not expose prompts."));
}

#[test]
fn test_is_mcp_config_path_matches_supported_files() {
    assert!(is_mcp_config_path(std::path::Path::new(
        ".composer/mcp.json"
    )));
    assert!(is_mcp_config_path(std::path::Path::new(
        ".composer/mcp.local.json"
    )));
    assert!(is_mcp_config_path(std::path::Path::new(
        "/tmp/home/.composer/enterprise/mcp.json"
    )));
    assert!(!is_mcp_config_path(std::path::Path::new(
        ".composer/config.toml"
    )));
}

#[test]
fn test_update_mcp_badge_counts_tracks_failures() {
    let mut app = new_test_app();
    app.update_mcp_badge_counts(&[
        crate::tools::McpServerStatus {
            name: "connected".to_string(),
            state: crate::tools::McpLifecycleState::Ready,
            connected: true,
            scope: McpConfigScope::Project,
            transport: McpTransport::Stdio,
            error: None,
            tools: vec!["read".to_string(), "write".to_string()],
            disabled_tools: Vec::new(),
            resources: Vec::new(),
            prompts: Vec::new(),
        },
        crate::tools::McpServerStatus {
            name: "failed".to_string(),
            state: crate::tools::McpLifecycleState::Failed,
            connected: false,
            scope: McpConfigScope::User,
            transport: McpTransport::Http,
            error: Some("timed out".to_string()),
            tools: Vec::new(),
            disabled_tools: Vec::new(),
            resources: Vec::new(),
            prompts: Vec::new(),
        },
    ]);

    assert_eq!(app.state.mcp_connected, 1);
    assert_eq!(app.state.mcp_tool_count, 2);
    assert_eq!(app.state.mcp_failed, 1);
}

#[test]
fn test_format_mcp_server_transition_status_for_connection() {
    let status = format_mcp_server_transition_status(
        None,
        Some(&crate::tools::McpServerStatus {
            name: "docs".to_string(),
            state: crate::tools::McpLifecycleState::Ready,
            connected: true,
            scope: McpConfigScope::Project,
            transport: McpTransport::Stdio,
            error: None,
            tools: vec!["read".to_string(), "write".to_string()],
            disabled_tools: Vec::new(),
            resources: Vec::new(),
            prompts: Vec::new(),
        }),
    );

    assert_eq!(
        status.as_deref(),
        Some("MCP server \"docs\" connected (2 tools)")
    );
}

#[test]
fn test_format_mcp_server_transition_status_for_disconnection() {
    let previous = crate::tools::McpServerStatus {
        name: "docs".to_string(),
        state: crate::tools::McpLifecycleState::Ready,
        connected: true,
        scope: McpConfigScope::Project,
        transport: McpTransport::Stdio,
        error: None,
        tools: vec!["read".to_string()],
        disabled_tools: Vec::new(),
        resources: Vec::new(),
        prompts: Vec::new(),
    };

    let status = format_mcp_server_transition_status(Some(&previous), None);

    assert_eq!(status.as_deref(), Some("MCP server \"docs\" disconnected"));
}

#[test]
fn test_format_mcp_server_transition_status_for_error_change() {
    let previous = crate::tools::McpServerStatus {
        name: "docs".to_string(),
        state: crate::tools::McpLifecycleState::Failed,
        connected: false,
        scope: McpConfigScope::Project,
        transport: McpTransport::Stdio,
        error: Some("timed out".to_string()),
        tools: Vec::new(),
        disabled_tools: Vec::new(),
        resources: Vec::new(),
        prompts: Vec::new(),
    };
    let current = crate::tools::McpServerStatus {
        name: "docs".to_string(),
        state: crate::tools::McpLifecycleState::Failed,
        connected: false,
        scope: McpConfigScope::Project,
        transport: McpTransport::Stdio,
        error: Some(String::new()),
        tools: Vec::new(),
        disabled_tools: Vec::new(),
        resources: Vec::new(),
        prompts: Vec::new(),
    };

    let status = format_mcp_server_transition_status(Some(&previous), Some(&current));

    assert_eq!(
        status.as_deref(),
        Some("MCP server \"docs\" error: Connection failed.")
    );
}

#[tokio::test]
async fn passive_mcp_refresh_waits_for_native_agent_admission() {
    let mut app = new_test_app();
    assert!(app.native_agent.is_none());
    for force in [false, true] {
        assert!(!app.refresh_mcp_badges_with_force(force).await);
        assert!(!app.mcp_status_refresh_in_flight);
        assert!(app.last_mcp_status_refresh.is_none());
        assert!(app.mcp_status_rx.try_recv().is_err());
    }
}

#[test]
fn returning_user_without_an_agent_keeps_the_composer_and_pending_prompt() {
    let mut app = new_test_app();
    app.ui_prefs.onboarding_seen = true;
    app.state.set_input("offline draft é stays");
    app.initial_prompt = Some("explicit pending request".into());
    assert!(app.native_agent.is_none());
    assert!(!app.maybe_open_first_run_setup());
    assert_eq!(app.active_modal, ActiveModal::None);
    assert_eq!(app.state.input(), "offline draft é stays");
    assert_eq!(
        app.initial_prompt.as_deref(),
        Some("explicit pending request")
    );
    assert!(app.native_agent.is_none());
}

#[tokio::test]
async fn test_handle_config_event_forces_mcp_badge_refresh() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(temp.path().join(".composer")).expect("create config dir");

    let mut app = new_test_app();
    let (agent, _events) = crate::agent::NativeAgent::new_with_test_client(
        crate::agent::NativeAgentConfig {
            model: "openai/gpt-4o".into(),
            cwd: temp.path().display().to_string(),
            ..Default::default()
        },
        crate::ai::UnifiedClient::OpenAI(
            crate::ai::OpenAiClient::with_base_url("fixture", "http://127.0.0.1:1/v1").unwrap(),
        ),
    )
    .unwrap();
    app.native_agent = Some(agent);
    app.tool_executor = Arc::new(ToolExecutor::new(temp.path().display().to_string()));
    app.last_mcp_status_refresh = Some(Instant::now());
    app.state.mcp_connected = 7;
    app.state.mcp_tool_count = 9;
    app.state.mcp_failed = 2;

    app.handle_config_event(ConfigEvent::Changed(std::path::PathBuf::from(
        ".composer/mcp.json",
    )))
    .await;

    assert!(app.mcp_status_refresh_in_flight);
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if app.poll_mcp_status_refresh() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("background MCP status refresh");
    assert_eq!(app.state.mcp_connected, 0);
    assert_eq!(app.state.mcp_tool_count, 0);
    assert_eq!(app.state.mcp_failed, 0);
    assert!(app.last_mcp_status_refresh.is_some());
}

#[tokio::test]
async fn test_handle_config_event_reports_watcher_errors() {
    let mut app = new_test_app();

    app.handle_config_event(ConfigEvent::Error("watch failed".to_string()))
        .await;

    assert_eq!(
        app.state.status.as_deref(),
        Some("Config watcher error: watch failed")
    );
}

#[test]
fn test_format_mcp_runtime_event_status_for_progress() {
    let status = format_mcp_runtime_event_status(&McpRuntimeEvent::Progress {
        server: "docs".to_string(),
        progress: 5.0,
        total: Some(8.0),
        message: Some("Indexing".to_string()),
    });

    assert_eq!(status.as_deref(), Some("MCP docs: Indexing (63%)"));
}

#[test]
fn test_format_mcp_runtime_event_status_for_warning_logs() {
    let status = format_mcp_runtime_event_status(&McpRuntimeEvent::Log {
        server: "docs".to_string(),
        level: "warning".to_string(),
        logger: Some("mcp".to_string()),
        data: serde_json::json!({"detail":"slow response"}),
    });

    assert_eq!(
        status.as_deref(),
        Some(r#"[docs] {"detail":"slow response"}"#)
    );
}

#[test]
fn test_format_mcp_runtime_event_status_ignores_info_logs() {
    let status = format_mcp_runtime_event_status(&McpRuntimeEvent::Log {
        server: "docs".to_string(),
        level: "info".to_string(),
        logger: None,
        data: serde_json::Value::String("ready".to_string()),
    });

    assert!(status.is_none());
}

// ─────────────────────────────────────────────────────────────────────────
// Session ID Tests
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn test_session_id_handling() {
    let mut state = AppState::new();
    assert!(state.session_id.is_none());

    state.session_id = Some("session-123".to_string());
    assert_eq!(state.session_id, Some("session-123".to_string()));
}

// ─────────────────────────────────────────────────────────────────────────
// Tool Call Toggle Tests
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn test_tool_call_toggle() {
    for compact in [true, false] {
        let mut state = AppState::new();
        state.compact_tool_outputs = compact;
        let call_id = "call-123";

        assert_eq!(state.is_tool_call_expanded(call_id), !compact);

        state.toggle_tool_call(call_id);
        assert_eq!(state.is_tool_call_expanded(call_id), compact);

        state.toggle_tool_call(call_id);
        assert_eq!(state.is_tool_call_expanded(call_id), !compact);
    }
}

#[test]
fn test_multiple_tool_calls_expansion() {
    let mut state = AppState::new();
    state.compact_tool_outputs = true;

    state.toggle_tool_call("call-1");
    state.toggle_tool_call("call-2");

    assert!(state.is_tool_call_expanded("call-1"));
    assert!(state.is_tool_call_expanded("call-2"));
    assert!(!state.is_tool_call_expanded("call-3"));
}

// ─────────────────────────────────────────────────────────────────────────
// Thinking Toggle Tests
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn test_thinking_toggle() {
    let mut state = AppState::new();

    // Add a user message and get its ID
    let msg_id = state.add_user_message("test".to_string());

    // Initially not expanded
    let msg = state.messages.iter().find(|m| m.id == msg_id).unwrap();
    assert!(!msg.thinking_expanded);

    // Toggle on
    state.toggle_thinking(&msg_id);
    let msg = state.messages.iter().find(|m| m.id == msg_id).unwrap();
    assert!(msg.thinking_expanded);

    // Toggle off
    state.toggle_thinking(&msg_id);
    let msg = state.messages.iter().find(|m| m.id == msg_id).unwrap();
    assert!(!msg.thinking_expanded);
}

#[test]
fn test_thinking_toggle_nonexistent() {
    let mut state = AppState::new();
    state.add_user_message("test".to_string());

    // Should not panic on nonexistent ID
    state.toggle_thinking("nonexistent-id");
}

// ─────────────────────────────────────────────────────────────────────────
// System Prompt Building Tests
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn test_system_prompt_contains_tools() {
    // Test that system prompts mention available tools
    let prompt_template = App::build_base_system_prompt("/tmp");

    assert!(prompt_template.contains("bash"));
    assert!(prompt_template.contains("read"));
    assert!(prompt_template.contains("write"));
    assert!(prompt_template.contains("glob"));
    assert!(prompt_template.contains("grep"));
}

#[test]
fn test_system_prompt_routes_explicit_computer_requests_to_hosted_backend() {
    let prompt_template = App::build_base_system_prompt("/tmp");

    assert!(prompt_template.contains("work on this in a Computer"));
    assert!(prompt_template.contains("backend: \"computer\""));
    assert!(prompt_template.contains("runtime resolves the active workspace"));
    assert!(prompt_template.contains("silently fall back to the native backend"));
}

#[test]
fn test_system_prompt_includes_year_hint() {
    let prompt = App::build_system_prompt_with_context("/tmp", 2026, None, None, None, None, "");

    assert!(prompt.contains("websearch/codesearch"));
    assert!(prompt.contains("current year (2026)"));
}

#[test]
fn omitted_skill_catalog_budget_failure_remains_in_prompt_audit() {
    let source = "skills.loader.available_skills#omitted_budget_exceeded?budget_tokens=1&required_tokens=12&protected_count=2";
    let assembly = App::build_system_prompt_assembly_with_context(SystemPromptAssemblyContext {
        cwd: "/tmp",
        current_year: 2026,
        skills: SkillsPromptSection {
            content: None,
            source: source.to_string(),
            audit_only: true,
        },
        harness_section: None,
        rlm_section: None,
        mailbox_section: None,
        managed_rules_section: None,
        active_prompt: "",
    });

    let rendered = assembly.render();
    let report = assembly.audit(Some("test-model"), Vec::new());
    let skills = report
        .fragments
        .iter()
        .find(|fragment| fragment.name == "loaded_skills")
        .expect("omitted catalog must remain visible in the audit");

    assert!(!rendered.contains(source));
    assert_eq!(skills.source, source);
    assert_eq!(skills.byte_count, 0);
    assert_eq!(skills.token_count, 0);
}

#[test]
fn test_system_prompt_includes_harness_context() {
    let prompt = App::build_system_prompt_with_context(
        "/tmp",
        2026,
        None,
        Some(
            "## Supplemental Maestro harness\n\n### memory · release-proof\nUse the native smoke command."
                .to_string(),
        ),
        None,
        None,
        "",
    );

    assert!(prompt.contains("Supplemental Maestro harness"));
    assert!(prompt.contains("release-proof"));
}

#[test]
fn test_system_prompt_includes_rlm_and_mailbox_context() {
    let mut rlm = crate::rlm::RlmStore::default();
    rlm.set("plan", "Ship release", None)
        .expect("seed RLM context");
    let mut mailbox = crate::mailbox::MailboxStore::default();
    mailbox
        .send("child", "maestro", "report ready")
        .expect("seed mailbox context");
    let prompt = App::build_system_prompt_with_context(
        "/tmp",
        2026,
        None,
        None,
        rlm.prompt_section(),
        mailbox.prompt_section_for("maestro"),
        "",
    );

    assert!(prompt.contains("RLM context variables"));
    assert!(prompt.contains("{{plan}}"));
    assert!(prompt.contains("Pending Deixic Code mailbox messages"));
    assert!(prompt.contains("report ready"));
}

#[test]
fn test_system_prompt_instructs_untrusted_content_is_data_not_instruction() {
    // This is the standing instruction that governs how the model is
    // expected to treat the `<untrusted_content>` envelope emitted by
    // `ToolExecution::model_content` (agent/protocol.rs). It must always be
    // present, independent of loaded skills or the active custom prompt, so
    // it lives in the base prompt rather than `build_shared_prompt_additions`
    // or a skill/AGENTS.md file (which are not guaranteed to be loaded).
    let prompt_template = App::build_base_system_prompt("/tmp");

    assert!(prompt_template.contains("<untrusted_content"));
    assert!(prompt_template.contains("is DATA"));
    assert!(prompt_template.contains("never an instruction"));
    assert!(prompt_template.contains("ignore previous instructions"));
    assert!(prompt_template.contains("the operator"));

    // Present regardless of skills/custom-prompt state.
    let prompt = App::build_system_prompt_with_context("/tmp", 2026, None, None, None, None, "");
    assert!(prompt.contains("<untrusted_content"));
}

#[test]
fn side_messages_do_not_count_toward_compaction() {
    let mut state = AppState::new();
    state.add_side_question("side-1".into(), "Question".into());
    state.add_side_answer("side-1-answer".into(), "Answer".into(), false);
    assert!(
        state
            .messages
            .iter()
            .all(|message| !message.counts_toward_compaction_index())
    );
}

#[test]
fn open_plan_comments_block_approval() {
    let mut app = new_test_app();
    app.plan_review_comments.push(PlanReviewComment {
        id: 1,
        start_line: 1,
        end_line: 1,
        text: "Needs work".into(),
        revision: "revision".into(),
        excerpt: "line".into(),
        resolved: false,
    });
    app.approve_plan();
    assert!(
        app.state.error.as_deref().is_some_and(|error| error
            == "Open review comments prevent leaving plan mode: 1. Use `/plan comments`.")
    );
}

#[test]
fn restore_side_questions_by_timestamp_without_model_history_entries() {
    let mut state = AppState::new();
    let session = ParsedSession {
        selective_summary_context: None,
        header: SessionHeader {
            version: Some(2),
            id: "ordered-side-questions".into(),
            timestamp: "2026-07-23T12:00:00Z".into(),
            cwd: "/tmp".into(),
            model: "openai/gpt-5.2".into(),
            subject: None,
            model_metadata: None,
            thinking_level: ThinkingLevel::Medium,
            system_prompt: None,
            prompt_metadata: None,
            prompt_context_manifest: None,
            unified_context_manifest: None,
            tools: Vec::new(),
            branched_from: None,
            parent_session: None,
        },
        messages: vec![
            AppMessage::User {
                content: MessageContent::Text("before".into()),
                attachments: None,
                timestamp: 1_000,
            },
            AppMessage::User {
                content: MessageContent::Text("after".into()),
                attachments: None,
                timestamp: 3_000,
            },
        ],
        meta: None,
        stats: Default::default(),
        thinking_level_changes: Vec::new(),
        model_changes: Vec::new(),
        compactions: Vec::new(),
        session_events: Vec::new(),
        context_budget_snapshots: Vec::new(),
        lifecycle_notifications: Vec::new(),
        pending_lifecycle_agent_notes: Vec::new(),
        side_questions: vec![SideQuestionEntry {
            id: "side-1".into(),
            timestamp: "1970-01-01T00:00:02Z".into(),
            question: "side".into(),
            answer: String::new(),
            error: Some("provider unavailable".into()),
        }],
        plan_review_events: Vec::new(),
        usage_entries: Vec::new(),
        file_path: "/tmp/ordered-side-questions.jsonl".into(),
    };

    restore_visible_session_messages(&mut state, &session);

    assert_eq!(
        state
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        vec![
            "before",
            "side",
            "Side question failed: provider unavailable",
            "after"
        ]
    );
    assert_eq!(state.messages[1].kind, MessageKind::SideQuestion);
    assert_eq!(state.messages[2].kind, MessageKind::SideAnswer);
}

#[test]
fn stale_plan_comments_cannot_be_resolved_or_approved() {
    let dir = tempdir().unwrap();
    let plan_dir = dir.path().join(".maestro");
    std::fs::create_dir_all(&plan_dir).unwrap();
    let plan_path = plan_dir.join("plan.md");
    std::fs::write(&plan_path, "first\nselected\nlast\n").unwrap();

    let mut app = new_test_app();
    app.state.cwd = Some(dir.path().to_string_lossy().to_string());
    crate::plan_mode::set_active_session_id(None);
    app.handle_plan_review(PlanReviewAction::Comment {
        start_line: 2,
        end_line: 2,
        text: "review this".into(),
    });
    assert_eq!(app.plan_review_comments[0].excerpt, "selected");
    assert_eq!(
        app.plan_review_comments[0].revision,
        crate::plan_mode::plan_revision("first\nselected\nlast\n")
    );

    std::fs::write(&plan_path, "first\nchanged\nlast\n").unwrap();
    app.handle_plan_review(PlanReviewAction::Resolve { id: 1 });
    assert!(!app.plan_review_comments[0].resolved);
    assert!(
        app.state
            .error
            .as_deref()
            .is_some_and(|error| error.contains("stale"))
    );

    app.plan_review_comments[0].resolved = true;
    app.state.error = None;
    app.approve_plan();
    assert!(
        app.state
            .error
            .as_deref()
            .is_some_and(|error| error == "Review comments on an earlier plan: 1. Recreate stale comments before leaving plan mode.")
    );

    app.handle_plan_review(PlanReviewAction::List);
    assert!(
        app.state
            .messages
            .last()
            .is_some_and(|message| message.content.contains("[stale]"))
    );
    crate::plan_mode::set_active_session_id(None);
}

#[test]
fn open_plan_comments_block_off_cycle_and_approval() {
    let mut app = new_test_app();
    app.plan_review_comments.push(PlanReviewComment {
        id: 1,
        start_line: 1,
        end_line: 1,
        text: "Needs work".into(),
        revision: "revision".into(),
        excerpt: "line".into(),
        resolved: false,
    });
    let _plan_mode = crate::safety::PlanModeOverride::enable();
    app.state.interaction_mode = crate::state::InteractionMode::Plan;

    app.apply_plan_mode(false);
    assert!(crate::safety::is_plan_mode());
    app.cycle_interaction_mode();
    assert!(crate::safety::is_plan_mode());
    app.approve_plan();
    assert!(crate::safety::is_plan_mode());

    crate::safety::set_plan_satisfied(true);
}

#[test]
fn fork_snapshot_keeps_parent_plan_state_and_session_selected() {
    let mut app = new_test_app();
    app.ensure_session_started().unwrap();
    let parent_session = app.state.session_id.clone();
    app.plan_review_comments.push(PlanReviewComment {
        id: 1,
        start_line: 1,
        end_line: 1,
        text: "Needs work".into(),
        revision: "revision".into(),
        excerpt: "line".into(),
        resolved: false,
    });
    app.fork_session();

    assert_eq!(app.plan_review_comments.len(), 1);
    assert_eq!(app.state.session_id, parent_session);
    assert!(
        app.state
            .status
            .as_deref()
            .is_some_and(|status| status.contains("created without switching"))
    );
}

#[tokio::test]
async fn slash_unique_prefix_expands_and_executes() {
    let mut app = new_test_app();
    app.state.set_input("/qui");

    app.handle_key(KeyCode::Enter, CrosstermModifiers::NONE)
        .await
        .unwrap();

    assert!(app.should_quit);
    assert_eq!(app.state.status.as_deref(), Some("Expanded /qui → /quit"));
}

#[tokio::test]
async fn slash_typo_rescue_expands_and_executes() {
    let mut app = new_test_app();
    app.state.set_input("/quti");

    app.handle_key(KeyCode::Enter, CrosstermModifiers::NONE)
        .await
        .unwrap();

    assert!(app.should_quit);
    assert_eq!(
        app.state.status.as_deref(),
        Some("Interpreted /quti as /quit")
    );
}

#[tokio::test]
async fn slash_ambiguous_prefix_restores_input_and_opens_dropdown() {
    let mut app = new_test_app();
    app.state.set_input("/qu");

    app.handle_key(KeyCode::Enter, CrosstermModifiers::NONE)
        .await
        .unwrap();

    assert!(!app.should_quit);
    assert_eq!(app.state.input(), "/qu");
    let error = app.state.error.as_deref().expect("ambiguity error");
    assert!(error.contains("Ambiguous command: /qu"));
    assert!(error.contains("/queue"));
    assert!(error.contains("/quit"));
    // The completion dropdown stays open so the user can pick a candidate.
    assert!(app.slash_state.has_completions());
}

#[tokio::test]
async fn slash_unknown_command_fallback_disabled_shows_error() {
    let mut app = new_test_app();
    app.state.unknown_slash_command_fallback = false;
    app.state.set_input("/zzznotacommand");

    app.handle_key(KeyCode::Enter, CrosstermModifiers::NONE)
        .await
        .unwrap();

    assert!(!app.state.busy);
    let error = app.state.error.as_deref().expect("unknown command error");
    assert!(error.contains("Unknown command"));
}

#[tokio::test]
async fn slash_ghost_completion_accepted_with_right_arrow() {
    let mut app = new_test_app();
    app.state.set_input("/qui");
    app.update_slash_state();
    assert_eq!(app.state.ghost_completion.as_deref(), Some("t"));

    app.handle_key(KeyCode::Right, CrosstermModifiers::NONE)
        .await
        .unwrap();

    assert_eq!(app.state.input(), "/quit");
    assert!(app.state.ghost_completion.is_none());
}

#[tokio::test]
async fn slash_ghost_completion_hidden_when_cursor_not_at_end() {
    let mut app = new_test_app();
    app.state.set_input("/qui");
    app.update_slash_state();
    assert!(app.state.ghost_completion.is_some());

    app.handle_key(KeyCode::Left, CrosstermModifiers::NONE)
        .await
        .unwrap();
    app.update_slash_state();

    assert!(app.state.ghost_completion.is_none());
}

#[tokio::test]
async fn interrupted_escape_sequence_preserves_edited_draft() {
    let mut app = new_test_app();
    for paste in [false, true] {
        app.state.set_input("draft");
        app.last_esc_at = Some(Instant::now());
        if paste {
            app.handle_paste("x");
        } else {
            app.handle_key(KeyCode::Char('x'), CrosstermModifiers::NONE)
                .await
                .unwrap();
        }
        assert!(app.last_esc_at.is_none());
        app.handle_key(KeyCode::Esc, CrosstermModifiers::NONE)
            .await
            .unwrap();
        assert_eq!(app.state.input(), "draftx");
        app.handle_key(KeyCode::Esc, CrosstermModifiers::NONE)
            .await
            .unwrap();
        assert!(app.state.input().is_empty());
    }
}

#[tokio::test]
async fn modal_escape_and_new_session_end_the_previous_escape_sequence() {
    let mut app = new_test_app();
    app.state.set_input("draft");
    app.last_esc_at = Some(Instant::now());
    app.show_control_panel(crate::commands::ControlPanel::Settings);
    app.handle_key(KeyCode::Esc, CrosstermModifiers::NONE)
        .await
        .unwrap();
    assert!(app.last_esc_at.is_none());
    app.handle_key(KeyCode::Esc, CrosstermModifiers::NONE)
        .await
        .unwrap();
    assert_eq!(app.state.input(), "draft");
    app.handle_command_action(CommandAction::ClearMessages)
        .await;
    assert!(app.last_esc_at.is_none());
}

#[tokio::test]
async fn double_esc_clears_input() {
    let mut app = new_test_app();
    app.state.set_input("draft text");

    app.handle_key(KeyCode::Esc, CrosstermModifiers::NONE)
        .await
        .unwrap();
    // First Esc: draft kept, hint shown.
    assert_eq!(app.state.input(), "draft text");

    app.handle_key(KeyCode::Esc, CrosstermModifiers::NONE)
        .await
        .unwrap();
    assert_eq!(app.state.input(), "");
}

#[tokio::test]
async fn esc_with_completions_only_dismisses_menu() {
    let mut app = new_test_app();
    app.state.set_input("/qu");
    app.update_slash_state();
    assert!(app.slash_state.has_completions());

    app.handle_key(KeyCode::Esc, CrosstermModifiers::NONE)
        .await
        .unwrap();

    assert!(!app.slash_state.has_completions());
    assert_eq!(app.state.input(), "/qu");
}

#[tokio::test]
async fn loop_command_start_and_stop_via_handler() {
    let mut app = new_test_app();
    app.state.set_input("/loop 10m check the build");

    app.handle_key(KeyCode::Enter, CrosstermModifiers::NONE)
        .await
        .unwrap();
    assert!(app.loop_schedule.is_some());
    assert_eq!(
        app.loop_schedule.as_ref().map(|s| s.interval.as_secs()),
        Some(600)
    );

    app.state.set_input("/loop stop");
    app.handle_key(KeyCode::Enter, CrosstermModifiers::NONE)
        .await
        .unwrap();
    assert!(app.loop_schedule.is_none());
}

// ─────────────────────────────────────────────────────────────────────────
// Rewind Picker Tests (double-Esc on empty input)
// ─────────────────────────────────────────────────────────────────────────

/// Point the app's session manager at a temp sessions dir and give it a
/// session id so file checkpoints resolve to the fixture, not `$HOME`.
fn setup_rewind_session(app: &mut App) -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let sessions = tmp.path().join("sessions");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&sessions).unwrap();
    app.session_manager = SessionManager::with_sessions_dir(
        repo.to_string_lossy().to_string(),
        sessions.to_string_lossy().to_string(),
    );
    app.state.session_id = Some("rewind-test".to_string());
    (tmp, repo)
}

/// Write a one-file checkpoint manifest (plus its pre-turn blob) directly
/// into the session's checkpoint store and set the file's current content.
fn write_rewind_checkpoint(
    app: &App,
    repo: &std::path::Path,
    id: &str,
    created_at: &str,
    pre: &str,
    post: &str,
) {
    use sha2::{Digest, Sha256};
    let hash = |content: &str| format!("{:x}", Sha256::digest(content.as_bytes()));

    let store =
        crate::checkpoints::CheckpointStore::new(app.session_manager.sessions_dir(), "rewind-test");
    let dir = store.root().join(id);
    std::fs::create_dir_all(dir.join("blobs")).unwrap();
    std::fs::write(dir.join("blobs").join(hash(pre)), pre).unwrap();
    std::fs::write(repo.join("a.rs"), post).unwrap();

    let checkpoint = crate::checkpoints::Checkpoint {
        id: id.to_string(),
        created_at: created_at.to_string(),
        prompt: format!("prompt for {id}"),
        repo_root: dunce::canonicalize(repo).unwrap(),
        head: None,
        user_turn_index: None,
        entries: vec![crate::checkpoints::FileEntry {
            path: "a.rs".to_string(),
            kind: crate::checkpoints::EntryKind::Modified,
            pre_blob: Some(hash(pre)),
            post_hash: Some(hash(post)),
        }],
    };
    let bytes = serde_json::to_vec_pretty(&checkpoint).unwrap();
    std::fs::write(dir.join("checkpoint.json"), bytes).unwrap();
}

async fn press_esc(app: &mut App) {
    app.handle_key(KeyCode::Esc, CrosstermModifiers::NONE)
        .await
        .unwrap();
}

#[test]
fn ephemeral_sessions_keep_file_checkpoints_without_turn_coordinates() {
    let mut app = new_test_app();
    let (_temp, repo) = setup_rewind_session(&mut app);
    assert!(
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success()
    );
    assert!(app.session_manager.writer().is_none());
    app.capture_file_checkpoint(&repo, "ephemeral edit");
    assert!(
        app.pending_checkpoint
            .as_ref()
            .is_some_and(|pending| pending.user_turn_index.is_none())
    );
    std::fs::write(repo.join("new.txt"), "created in ephemeral turn").unwrap();
    app.finalize_file_checkpoint();
    let store =
        crate::checkpoints::CheckpointStore::new(app.session_manager.sessions_dir(), "rewind-test");
    assert_eq!(store.list().len(), 1);
    let restored = crate::checkpoints::restore_latest(&store).unwrap().unwrap();
    assert!(restored.failed.is_empty());
    assert!(!repo.join("new.txt").exists());
}

#[test]
fn rewind_both_reopens_before_two_edits_and_keeps_manual_changes() {
    let mut app = new_test_app();
    let (_tmp, repo) = setup_rewind_session(&mut app);
    app.state.session_id = None;
    app.record_user_message("first edit");
    app.record_user_message("second edit");
    app.session_manager.flush().unwrap();
    let source_id = app.state.session_id.clone().unwrap();
    // The fixture helper writes to rewind-test; move its completed store to
    // this real saved session after assigning explicit turn coordinates.
    write_rewind_checkpoint(&app, &repo, "cp-0", "2026-07-24T00:00:00Z", "v0", "v1");
    write_rewind_checkpoint(&app, &repo, "cp-1", "2026-07-24T01:00:00Z", "v1", "v2");
    let fixture_store =
        crate::checkpoints::CheckpointStore::new(app.session_manager.sessions_dir(), "rewind-test");
    for index in 0..2 {
        let manifest = fixture_store
            .root()
            .join(format!("cp-{index}/checkpoint.json"));
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
        value["user_turn_index"] = index.into();
        let mut second_file = value["entries"][0].clone();
        second_file["path"] = "b.rs".into();
        value["entries"].as_array_mut().unwrap().push(second_file);
        std::fs::write(manifest, serde_json::to_vec(&value).unwrap()).unwrap();
    }
    let source_store =
        crate::checkpoints::CheckpointStore::new(app.session_manager.sessions_dir(), &source_id);
    std::fs::rename(fixture_store.root(), source_store.root()).unwrap();
    std::fs::write(repo.join("a.rs"), "later manual edit").unwrap();
    std::fs::write(repo.join("b.rs"), "v2").unwrap();
    app.rewind_saved_turns(2, false, true);
    assert!(app.state.error.is_none(), "{:?}", app.state.error);
    assert_ne!(app.state.session_id.as_deref(), Some(source_id.as_str()));
    let branch = app.session_manager.current_session_path().unwrap();
    app.session_manager.flush().unwrap();
    let reopened = crate::session::SessionReader::read_file(branch).unwrap();
    assert_eq!(reopened.stats.user_messages, 0);
    assert_eq!(
        std::fs::read_to_string(repo.join("a.rs")).unwrap(),
        "later manual edit"
    );
    assert_eq!(std::fs::read_to_string(repo.join("b.rs")).unwrap(), "v0");
}

#[test]
fn rewind_saved_conversation_reopens_at_selected_turn_and_keeps_source() {
    let mut app = new_test_app();
    let (_tmp, _) = setup_rewind_session(&mut app);
    app.state.session_id = None;
    app.record_user_message("first request");
    app.record_user_message("abandoned request");
    app.session_manager.flush().unwrap();
    let source_path = app.session_manager.current_session_path().unwrap();
    let original = std::fs::read(&source_path).unwrap();
    let source_id = app.state.session_id.clone();
    app.rewind_turns(1, true);
    assert_eq!(app.state.session_id, source_id);
    assert_eq!(std::fs::read(&source_path).unwrap(), original);
    app.rewind_turns(1, false);
    assert!(app.state.error.is_none(), "{:?}", app.state.error);
    assert_ne!(app.state.session_id, source_id);
    app.record_user_message("replacement request");
    app.session_manager.flush().unwrap();
    let branch = app.session_manager.current_session_path().unwrap();
    let reopened = crate::session::SessionReader::read_file(&branch).unwrap();
    assert_eq!(
        reopened
            .messages
            .iter()
            .map(AppMessage::text_content)
            .collect::<Vec<_>>(),
        ["first request", "replacement request"]
    );
    assert_eq!(std::fs::read(&source_path).unwrap(), original);
    let (history, _, _) = app.agent_context_for_spawn().unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[1].content.as_text(), Some("replacement request"));
}

#[tokio::test]
async fn double_esc_on_empty_input_opens_rewind_picker() {
    let mut app = new_test_app();
    let (_tmp, repo) = setup_rewind_session(&mut app);
    write_rewind_checkpoint(
        &app,
        &repo,
        "cp-1",
        "2026-07-24T00:00:00Z",
        "original\n",
        "edit\n",
    );

    press_esc(&mut app).await;
    assert_eq!(app.active_modal, ActiveModal::None);
    assert_eq!(
        app.state.status.as_deref(),
        Some("Press Esc again to rewind files")
    );

    press_esc(&mut app).await;
    assert_eq!(app.active_modal, ActiveModal::RewindPicker);
    assert!(app.rewind_picker.is_visible());
}

#[test]
fn rewind_picker_conversation_uses_the_saved_turn_coordinate() {
    let mut app = new_test_app();
    let (_temp, repo) = setup_rewind_session(&mut app);
    app.state.session_id = None;
    for prompt in ["kept first request", "second request", "third request"] {
        app.record_user_message(prompt);
    }
    app.session_manager.flush().unwrap();
    let source = app.session_manager.current_session_path().unwrap();
    let original = std::fs::read(&source).unwrap();
    app.rewind_picker.show(vec![crate::checkpoints::Checkpoint {
        id: "selected".into(),
        created_at: "2026-09-10T00:00:00Z".into(),
        prompt: "second request".into(),
        repo_root: repo,
        head: None,
        user_turn_index: Some(1),
        entries: vec![],
    }]);
    app.handle_rewind_picker_key(KeyCode::Char('c')).unwrap();
    assert!(app.state.error.is_none(), "{:?}", app.state.error);
    app.session_manager.flush().unwrap();
    let child = crate::session::SessionReader::read_file(
        app.session_manager.current_session_path().unwrap(),
    )
    .unwrap();
    assert_eq!(child.stats.user_messages, 1);
    assert_eq!(std::fs::read(source).unwrap(), original);
}

#[tokio::test]
async fn rewind_picker_enter_restores_selected_checkpoint() {
    let mut app = new_test_app();
    let (_tmp, repo) = setup_rewind_session(&mut app);
    write_rewind_checkpoint(&app, &repo, "cp-1", "2026-07-24T00:00:00Z", "v0\n", "v1\n");
    write_rewind_checkpoint(&app, &repo, "cp-2", "2026-07-24T01:00:00Z", "v1\n", "v2\n");

    press_esc(&mut app).await;
    press_esc(&mut app).await;
    assert_eq!(app.active_modal, ActiveModal::RewindPicker);

    // Newest checkpoint is selected by default; Enter restores it.
    app.handle_key(KeyCode::Enter, CrosstermModifiers::NONE)
        .await
        .unwrap();

    assert_eq!(app.active_modal, ActiveModal::None);
    assert!(!app.rewind_picker.is_visible());
    assert_eq!(std::fs::read_to_string(repo.join("a.rs")).unwrap(), "v1\n");
    assert_eq!(
        app.state.status.as_deref(),
        Some("Files restored from checkpoint.")
    );

    // Only the applied checkpoint was consumed; the older one remains.
    let store =
        crate::checkpoints::CheckpointStore::new(app.session_manager.sessions_dir(), "rewind-test");
    let remaining = store.list();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id, "cp-1");
}

#[tokio::test]
async fn rewind_picker_esc_dismisses_without_restoring() {
    let mut app = new_test_app();
    let (_tmp, repo) = setup_rewind_session(&mut app);
    write_rewind_checkpoint(
        &app,
        &repo,
        "cp-1",
        "2026-07-24T00:00:00Z",
        "original\n",
        "edit\n",
    );

    press_esc(&mut app).await;
    press_esc(&mut app).await;
    assert_eq!(app.active_modal, ActiveModal::RewindPicker);

    press_esc(&mut app).await;
    assert_eq!(app.active_modal, ActiveModal::None);
    assert!(!app.rewind_picker.is_visible());
    // Nothing was restored and the checkpoint was not consumed.
    assert_eq!(
        std::fs::read_to_string(repo.join("a.rs")).unwrap(),
        "edit\n"
    );
    let store =
        crate::checkpoints::CheckpointStore::new(app.session_manager.sessions_dir(), "rewind-test");
    assert_eq!(store.list().len(), 1);
}

#[tokio::test]
async fn double_esc_without_checkpoints_shows_status() {
    let mut app = new_test_app();
    let (_tmp, _repo) = setup_rewind_session(&mut app);

    press_esc(&mut app).await;
    press_esc(&mut app).await;

    assert_eq!(app.active_modal, ActiveModal::None);
    assert_eq!(
        app.state.status.as_deref(),
        Some("No file checkpoints recorded for this session.")
    );
}

#[tokio::test]
async fn double_esc_rewind_picker_blocked_while_busy() {
    let mut app = new_test_app();
    let (_tmp, repo) = setup_rewind_session(&mut app);
    write_rewind_checkpoint(
        &app,
        &repo,
        "cp-1",
        "2026-07-24T00:00:00Z",
        "original\n",
        "edit\n",
    );
    app.state.busy = true;

    press_esc(&mut app).await;
    press_esc(&mut app).await;

    assert_eq!(app.active_modal, ActiveModal::None);
    assert_eq!(
        app.state.status.as_deref(),
        Some("Wait for the active response to finish before rewinding.")
    );
}

// ─────────────────────────────────────────────────────────────────────────
// Detail View Tests (Ctrl+E)
// ─────────────────────────────────────────────────────────────────────────

fn push_tool_call_message(app: &mut App, output: String) {
    app.state.messages.push(Message {
        id: "m-detail".to_string(),
        role: MessageRole::Assistant,
        kind: MessageKind::Regular,
        content: String::new(),
        thinking: String::new(),
        streaming: false,
        tool_calls: vec![crate::state::ToolCallState {
            call_id: "call-detail".to_string(),
            tool: "bash".to_string(),
            args: serde_json::json!({"command": "seq 1 120"}),
            status: crate::state::ToolCallStatus::Completed,
            output,
        }],
        usage: None,
        timestamp: SystemTime::now(),
        thinking_expanded: false,
    });
}

#[tokio::test]
async fn test_detail_view_opens_with_full_untruncated_tool_output() {
    let mut app = new_test_app();
    // The inline transcript caps tool output at 50 lines even when expanded;
    // the detail view must show every line.
    let output = (1..=120)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    push_tool_call_message(&mut app, output);

    app.handle_key(KeyCode::Char('e'), CrosstermModifiers::CONTROL)
        .await
        .expect("open detail view");

    assert_eq!(app.active_modal, ActiveModal::DetailView);
    let detail = app.detail_view.as_ref().expect("detail view open");
    assert_eq!(detail.title(), "Tool: bash");
    assert!(
        detail
            .content()
            .starts_with("Session evidence:\nLatest recorded checkpoint:")
    );
    assert!(detail.content().contains("tests not recorded"));
    assert!(detail.content().contains("\"command\": \"seq 1 120\""));
    assert!(detail.content().contains("line 1\n"));
    assert!(detail.content().contains("line 120"));
}

#[tokio::test]
async fn test_detail_view_esc_restores_transcript_view() {
    let mut app = new_test_app();
    push_tool_call_message(&mut app, "full output".to_string());

    app.handle_key(KeyCode::Char('e'), CrosstermModifiers::CONTROL)
        .await
        .expect("open detail view");
    assert_eq!(app.active_modal, ActiveModal::DetailView);

    app.handle_key(KeyCode::Esc, CrosstermModifiers::NONE)
        .await
        .expect("close detail view");
    assert_eq!(app.active_modal, ActiveModal::None);
    assert!(app.detail_view.is_none());
}

#[tokio::test]
async fn test_detail_view_scrolls_with_paging_keys() {
    let mut app = new_test_app();
    let output = (1..=120)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    push_tool_call_message(&mut app, output);

    app.handle_key(KeyCode::Char('e'), CrosstermModifiers::CONTROL)
        .await
        .expect("open detail view");
    assert_eq!(app.detail_view.as_ref().expect("open").scroll_position(), 0);

    app.handle_key(KeyCode::PageDown, CrosstermModifiers::NONE)
        .await
        .expect("page down");
    let scrolled = app.detail_view.as_ref().expect("open").scroll_position();
    assert!(scrolled > 0, "PageDown should scroll the detail view");

    app.handle_key(KeyCode::Up, CrosstermModifiers::NONE)
        .await
        .expect("scroll up");
    assert_eq!(
        app.detail_view.as_ref().expect("open").scroll_position(),
        scrolled - 1
    );
}

#[tokio::test]
async fn test_detail_view_falls_back_to_latest_message_text() {
    let mut app = new_test_app();
    app.state.messages.clear();
    app.state
        .add_system_message("full system body that was clipped inline".to_string());

    app.handle_key(KeyCode::Char('e'), CrosstermModifiers::CONTROL)
        .await
        .expect("open detail view");

    assert_eq!(app.active_modal, ActiveModal::DetailView);
    let detail = app.detail_view.as_ref().expect("detail view open");
    assert_eq!(detail.title(), "System message");
    assert_eq!(detail.content(), "full system body that was clipped inline");
}

#[tokio::test]
async fn test_detail_evidence_follows_expanded_message_past_empty_notices() {
    let mut app = new_test_app();
    app.state.messages.clear();
    push_tool_call_message(&mut app, "recorded tool output".to_string());
    app.state.add_system_message(String::new());

    app.open_detail_view();

    let detail = app.detail_view.as_ref().expect("detail view open");
    assert_eq!(detail.title(), "Tool: bash");
    assert!(detail.content().starts_with("Session evidence:\n"));
    assert!(detail.content().contains("recorded tool output"));
}

#[tokio::test]
async fn test_detail_view_nothing_to_expand_sets_status() {
    let mut app = new_test_app();
    app.state.messages.clear();
    app.state.error = None;

    app.handle_key(KeyCode::Char('e'), CrosstermModifiers::CONTROL)
        .await
        .expect("handle key");

    assert_eq!(app.active_modal, ActiveModal::None);
    assert_eq!(app.state.status.as_deref(), Some("Nothing to expand"));
}

#[tokio::test]
async fn test_detail_view_from_approval_modal_expands_and_restores() {
    let mut app = new_test_app();
    let long_command = format!("run --flag {}", "x".repeat(400));
    app.approval_controller.enqueue(
        ApprovalRequest::new(
            "call-approve",
            "bash",
            serde_json::json!({"command": long_command.clone()}),
        )
        .with_reason("Needs a very long command reviewed"),
    );
    app.active_modal = ActiveModal::Approval;

    // Ctrl+E on the approval prompt expands the full command/args.
    app.handle_key(KeyCode::Char('e'), CrosstermModifiers::CONTROL)
        .await
        .expect("expand approval");

    assert_eq!(app.active_modal, ActiveModal::DetailView);
    let detail = app.detail_view.as_ref().expect("detail view open");
    assert_eq!(detail.title(), "Approval: bash");
    assert!(
        detail
            .content()
            .contains("Needs a very long command reviewed")
    );
    assert!(detail.content().contains(&long_command));
    // The approval is still pending: expanding must not decide anything.
    assert!(app.approval_controller.current().is_some());

    // Esc returns to the approval modal, not the plain transcript.
    app.handle_key(KeyCode::Esc, CrosstermModifiers::NONE)
        .await
        .expect("close detail view");
    assert_eq!(app.active_modal, ActiveModal::Approval);
    assert!(app.detail_view.is_none());
    assert!(app.approval_controller.current().is_some());
}
#[test]
fn resume_session_at_startup_restores_agent_context_for_spawn() {
    let temp = tempdir().unwrap();
    let dir = temp.path().join("sessions");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("2024-01-15T10-30-00-000Z_fork-target.jsonl");
    let mut file = std::fs::File::create(&path).unwrap();
    use std::io::Write;
    writeln!(
        file,
        r#"{{"type":"session","id":"fork-target","timestamp":"2024-01-15T10:30:00Z","cwd":"/tmp","model":"openai/gpt-5.2","thinkingLevel":"medium"}}"#
    )
    .unwrap();
    writeln!(
        file,
        r#"{{"type":"message","timestamp":"2024-01-15T10:30:01Z","message":{{"role":"user","content":"continue the fork","timestamp":0}}}}"#
    )
    .unwrap();
    writeln!(
        file,
        r#"{{"type":"message","timestamp":"2024-01-15T10:30:02Z","message":{{"role":"assistant","content":[{{"type":"text","text":"Fork continued."}}],"timestamp":1}}}}"#
    )
    .unwrap();
    writeln!(
        file,
        r#"{{"type":"custom","id":"pending-lifecycle","timestamp":"2024-01-15T10:30:03Z","customType":"subagent_lifecycle_applied","data":{{"content":"Child completed","agentNote":"Subagent child-1 completed."}}}}"#
    )
    .unwrap();
    drop(file);

    let mut app = new_test_app();
    app.session_manager = crate::session::SessionManager::with_sessions_dir("/tmp", &dir);
    app.resume_session_at_startup("fork-target");

    assert_eq!(app.state.session_id.as_deref(), Some("fork-target"));
    assert!(!app.session_resume_failed);
    assert!(app.state.error.is_none());
    let texts: Vec<&str> = app
        .state
        .messages
        .iter()
        .map(|message| message.content.as_str())
        .collect();
    assert!(texts.contains(&"continue the fork"));
    assert!(texts.contains(&"Fork continued."));
    // The writer resumed for appends under the forked session id.
    assert_eq!(
        app.session_manager.current_session_id(),
        Some("fork-target")
    );
    assert_eq!(app.current_model, "openai/gpt-5.2");
    assert_eq!(app.current_thinking_level, ThinkingLevel::Medium);

    let (history, session_id, thinking_level) = app.agent_context_for_spawn().unwrap();
    assert_eq!(session_id.as_deref(), Some("fork-target"));
    assert_eq!(thinking_level, ThinkingLevel::Medium);
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].role, crate::ai::Role::User);
    assert_eq!(history[0].content.as_text(), Some("continue the fork"));
    assert_eq!(history[1].role, crate::ai::Role::Assistant);
    assert_eq!(history[1].content.as_text(), Some("Fork continued."));
    assert_eq!(
        app.pending_agent_tool_notes,
        vec![PendingAgentToolNote {
            application_id: Some("pending-lifecycle".to_string()),
            content: "Subagent child-1 completed.".to_string(),
        }]
    );

    // Unknown ids surface an error instead of panicking.
    app.resume_session_at_startup("missing-session");
    assert!(app.state.error.is_some());
}

#[test]
fn locked_session_resume_does_not_restore_lifecycle_notes() {
    let temp = tempdir().unwrap();
    let dir = temp.path().join("sessions");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("2024-01-15T10-30-00-000Z_locked-target.jsonl");
    let mut file = std::fs::File::create(&path).unwrap();
    use std::io::Write;
    writeln!(
        file,
        r#"{{"type":"session","id":"locked-target","timestamp":"2024-01-15T10:30:00Z","cwd":"/tmp","model":"openai/gpt-5.2","thinkingLevel":"medium"}}"#
    )
    .unwrap();
    writeln!(
        file,
        r#"{{"type":"custom","id":"foreign-lifecycle","timestamp":"2024-01-15T10:30:03Z","customType":"subagent_lifecycle_applied","data":{{"content":"Child completed","agentNote":"Subagent child-1 completed."}}}}"#
    )
    .unwrap();
    drop(file);

    let mut lock_holder = crate::session::SessionManager::with_sessions_dir("/tmp", &dir);
    lock_holder
        .resume_session_by_path("locked-target", &path)
        .expect("hold session writer lock");
    let mut app = new_test_app();
    app.session_manager = crate::session::SessionManager::with_sessions_dir("/tmp", &dir);

    app.resume_session_at_startup("locked-target");

    assert!(app.session_resume_failed);
    assert!(app.pending_agent_tool_notes.is_empty());
}

#[test]
fn locked_session_resume_preserves_the_active_session() {
    assert_locked_resume_preserves_active(false);
}

#[test]
fn continue_locked_session_preserves_the_active_session() {
    assert_locked_resume_preserves_active(true);
}

fn assert_locked_resume_preserves_active(continue_latest: bool) {
    use std::io::Write;

    let temp = tempdir().unwrap();
    let dir = temp.path().join("sessions");
    std::fs::create_dir_all(&dir).unwrap();
    let write_session = |id: &str, model: &str, message: &str| {
        let path = dir.join(format!("2024-01-15T10-30-00-000Z_{id}.jsonl"));
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "session",
                "id": id,
                "timestamp": "2024-01-15T10:30:00Z",
                "cwd": "/tmp",
                "model": model,
                "thinkingLevel": "medium"
            })
        )
        .unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "message",
                "timestamp": "2024-01-15T10:30:01Z",
                "message": {
                    "role": "user",
                    "content": message,
                    "timestamp": 0
                }
            })
        )
        .unwrap();
        path
    };
    let active_path = write_session("active-session", "active-model", "keep active");
    let locked_path = write_session("locked-target", "locked-model", "do not adopt");

    let mut app = new_test_app();
    app.session_manager = crate::session::SessionManager::with_sessions_dir("/tmp", &dir);
    app.resume_session_at_startup("active-session");
    let active_messages: Vec<String> = app
        .state
        .messages
        .iter()
        .map(|message| message.content.clone())
        .collect();
    let active_model = app.current_model.clone();
    let active_state_model = app.state.model.clone();
    let active_session_id = app.state.session_id.clone();

    let mut lock_holder = crate::session::SessionManager::with_sessions_dir("/tmp", &dir);
    lock_holder
        .resume_session_by_path("locked-target", &locked_path)
        .expect("hold target session writer lock");
    let locked = app
        .session_manager
        .load_session("locked-target")
        .expect("load locked target transcript");

    if continue_latest {
        std::fs::File::open(&active_path)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH)
            .unwrap();
        app.continue_last_session();
    } else {
        app.resume_session_path(std::path::Path::new(&locked.file_path), &locked.header.id);
    }

    assert!(!app.session_resume_failed);
    assert_eq!(app.state.session_id, active_session_id);
    assert_eq!(app.current_model, active_model);
    assert_eq!(app.state.model, active_state_model);
    assert_eq!(
        app.session_manager.current_session_path().as_deref(),
        Some(active_path.as_path())
    );
    let messages: Vec<String> = app
        .state
        .messages
        .iter()
        .map(|message| message.content.clone())
        .collect();
    assert_eq!(messages, active_messages);
    assert!(crate::session::SessionWriter::open_existing(&active_path).is_err());
    assert!(
        app.state
            .status
            .as_deref()
            .is_some_and(|status| status.contains("current session unchanged"))
    );

    // The retained writer remains usable after the failed switch.
    app.record_user_message("continue active");
    let active = crate::session::SessionReader::read_file(&active_path).unwrap();
    assert!(
        active
            .messages
            .iter()
            .any(|message| message.text_content() == "continue active")
    );
}

#[test]
fn invalidated_resume_selection_preserves_active_writer_and_history() {
    let temp = tempdir().unwrap();
    let mut app = new_test_app();
    app.session_manager = SessionManager::with_sessions_dir("/tmp", temp.path());
    app.ensure_session_started().unwrap();
    app.record_user_message("keep active history");
    let active = app.session_manager.current_session_path().unwrap();
    let active_id = app.state.session_id.clone();
    app.resume_session_path(&active, active_id.as_deref().unwrap());
    assert!(
        app.state
            .messages
            .iter()
            .any(|message| message.content == "keep active history")
    );
    let (child_id, child_path) = app.session_manager.fork_session_snapshot().unwrap();
    let selected = app.session_manager.find_session(&child_id).unwrap();
    // The picker metadata is only a locator. A target changed after selection
    // must fail before replacing either the transcript or the active writer.
    std::fs::write(&child_path, "{}\n").unwrap();
    let messages = app
        .state
        .messages
        .iter()
        .map(|message| message.content.clone())
        .collect::<Vec<_>>();
    app.resume_session_path(&selected.path, &selected.id);
    assert_eq!(app.state.session_id, active_id);
    assert_eq!(
        app.session_manager.current_session_path(),
        Some(active.clone())
    );
    assert_eq!(
        app.state
            .messages
            .iter()
            .map(|message| message.content.clone())
            .collect::<Vec<_>>(),
        messages
    );
    assert!(!app.session_resume_failed);
    assert!(app.state.error.is_some());
    assert!(crate::session::SessionWriter::open_existing(&active).is_err());
    app.record_user_message("still writable");
    assert!(
        crate::session::SessionReader::read_file(&active)
            .unwrap()
            .messages
            .iter()
            .any(|message| message.text_content() == "still writable")
    );
}

#[test]
fn reselecting_active_session_keeps_writer_and_clears_escape_sequence() {
    use std::io::Write;

    let temp = tempdir().unwrap();
    let dir = temp.path().join("sessions");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("2024-01-15T10-30-00-000Z_active-session.jsonl");
    let mut file = std::fs::File::create(&path).unwrap();
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "session",
            "id": "active-session",
            "timestamp": "2024-01-15T10:30:00Z",
            "cwd": "/tmp",
            "model": "active-model",
            "thinkingLevel": "medium"
        })
    )
    .unwrap();
    drop(file);

    let mut app = new_test_app();
    app.session_manager = crate::session::SessionManager::with_sessions_dir("/tmp", &dir);
    app.resume_session_at_startup("active-session");
    app.last_esc_at = Some(std::time::Instant::now());
    let parsed = app
        .session_manager
        .load_session("active-session")
        .expect("load active transcript");

    app.record_user_message("written after selection");
    app.resume_session_path(std::path::Path::new(&parsed.file_path), &parsed.header.id);

    assert!(!app.session_resume_failed);
    assert_eq!(
        app.session_manager.current_session_id(),
        Some("active-session")
    );
    assert_eq!(
        app.session_manager.current_session_path().as_deref(),
        Some(path.as_path())
    );
    assert!(crate::session::SessionWriter::open_existing(&path).is_err());
    assert!(app.last_esc_at.is_none());
    assert!(
        app.state
            .messages
            .iter()
            .any(|message| message.content == "written after selection")
    );
}

#[test]
fn ephemeral_session_applies_and_deduplicates_lifecycle_events_in_memory() {
    let mut app = new_test_app();
    app.session_manager
        .start_ephemeral_session_for_test("ephemeral-session");
    let event: crate::tools::SubagentLifecycleEvent = serde_json::from_value(serde_json::json!({
        "mailbox_message_id": "lifecycle-1",
        "subagent_id": "child-1",
        "attempt": 1,
        "parent_scope_id": "ephemeral-session",
        "parent_call_id": "call-1",
        "status": "completed",
        "summary": "done",
        "error": null,
        "finished_at_ms": 1
    }))
    .expect("lifecycle event");

    assert_eq!(
        app.subagent_lifecycle_application_exists(&event),
        Some(false)
    );
    assert!(app.record_subagent_lifecycle_application(
        &event,
        "Child completed".to_string(),
        "Subagent child-1 completed.".to_string(),
    ));
    assert_eq!(
        app.subagent_lifecycle_application_exists(&event),
        Some(true)
    );
    assert_eq!(
        app.session_manager.current_session_id(),
        Some("ephemeral-session")
    );
    assert!(app.state.error.is_none());
}

#[test]
fn consumed_lifecycle_marker_stays_pending_until_persistence_succeeds() {
    let mut app = new_test_app();
    app.pending_consumed_agent_tool_notes
        .push("lifecycle-1".to_string());

    app.record_agent_tool_note_progress(false);

    assert_eq!(
        app.pending_consumed_agent_tool_notes,
        vec!["lifecycle-1".to_string()]
    );

    app.session_manager
        .start_ephemeral_session_for_test("ephemeral-session");
    app.record_agent_tool_note_progress(false);
    assert!(app.pending_consumed_agent_tool_notes.is_empty());
}

#[test]
fn closed_consumption_receiver_requeues_note_after_history_replacement() {
    let mut app = new_test_app();
    let (consumed_sender, consumed) = tokio::sync::oneshot::channel();
    drop(consumed_sender);
    app.pending_agent_note_consumptions
        .push(PendingAgentNoteConsumption {
            application_id: "lifecycle-1".to_string(),
            content: "Subagent child-1 completed.".to_string(),
            consumed,
        });

    app.record_agent_tool_note_progress(false);

    assert!(app.pending_agent_note_consumptions.is_empty());
    assert_eq!(
        app.pending_agent_tool_notes,
        vec![PendingAgentToolNote {
            application_id: Some("lifecycle-1".to_string()),
            content: "Subagent child-1 completed.".to_string(),
        }]
    );
}

#[tokio::test]
async fn lifecycle_consumption_waits_for_durable_turn_completion() {
    let mut app = new_test_app();
    app.session_manager
        .start_ephemeral_session_for_test("ephemeral-session");
    let (consumed_sender, consumed) = tokio::sync::oneshot::channel();
    consumed_sender.send(()).expect("signal consumption");
    app.pending_agent_note_consumptions
        .push(PendingAgentNoteConsumption {
            application_id: "lifecycle-1".to_string(),
            content: "Subagent child-1 completed.".to_string(),
            consumed,
        });

    app.record_agent_tool_note_progress(false);

    assert_eq!(
        app.ready_consumed_agent_tool_notes,
        vec!["lifecycle-1".to_string()]
    );
    assert!(app.pending_consumed_agent_tool_notes.is_empty());

    app.handle_agent_message(FromAgent::TurnCompleted {
        response_id: "done".to_string(),
        coding_completion: None,
        coding_child_records: Vec::new(),
    })
    .await
    .expect("turn completion");

    assert!(app.ready_consumed_agent_tool_notes.is_empty());
    assert!(app.pending_consumed_agent_tool_notes.is_empty());
}

#[tokio::test]
async fn test_detail_view_from_approval_sanitizes_format_controls_in_args() {
    let mut app = new_test_app();
    app.approval_controller.enqueue(
        ApprovalRequest::new(
            "call-inline",
            "deploy",
            serde_json::json!({"target": "safe\u{202e}txt\u{2066}\u{200b}"}),
        )
        .with_inline_tool_source(
            "./deploy.sh",
            ".composer/tools.json",
            "project",
            None,
            &std::collections::HashMap::new(),
        ),
    );
    app.active_modal = ActiveModal::Approval;

    app.handle_key(KeyCode::Char('e'), CrosstermModifiers::CONTROL)
        .await
        .expect("expand approval");

    let content = app
        .detail_view
        .as_ref()
        .expect("detail view open")
        .content();
    assert!(content.contains(r#""safe\u{202e}txt\u{2066}\u{200b}""#));
    assert!(
        !content.contains(['\u{202e}', '\u{2066}', '\u{200b}']),
        "{content:?}"
    );
}

#[tokio::test]
async fn test_detail_view_from_batched_approval_shows_selected_inline_context() {
    let mut app = new_test_app();
    app.approval_controller.enqueue(ApprovalRequest::new(
        "call-first",
        "read",
        serde_json::json!({}),
    ));
    let env = std::collections::HashMap::from([
        ("A".to_string(), "/first/value".to_string()),
        ("PATH".to_string(), "/tmp/attacker/evil".to_string()),
        ("Z".to_string(), "/last/value".to_string()),
    ]);
    app.approval_controller.enqueue(
        ApprovalRequest::new("call-inline", "deploy", serde_json::json!({}))
            .with_inline_tool_source(
                "./deploy.sh",
                "/long/project/.composer/tools.json",
                "project",
                Some("/attacker/workdir"),
                &env,
            ),
    );
    app.approval_controller.select_next();
    app.active_modal = ActiveModal::Approval;

    app.handle_key(KeyCode::Char('e'), CrosstermModifiers::CONTROL)
        .await
        .expect("expand selected approval");

    let detail = app.detail_view.as_ref().expect("detail view open");
    assert_eq!(detail.title(), "Approval: deploy");
    assert!(
        detail
            .content()
            .contains("/long/project/.composer/tools.json")
    );
    assert!(detail.content().contains("/attacker/workdir"));
    assert!(detail.content().contains("PATH=/tmp/attacker/evil"));
}

#[tokio::test]
async fn test_second_approval_upgrades_open_modal_to_batched() {
    // Regression for #3085: the agent emits one ToolCall event per
    // approval-needing call; when a second approval arrives while the
    // single-call modal is open, the visible modal must upgrade to the
    // batched variant without losing the first request.
    let mut app = new_test_app();
    app.state.approval_mode = ApprovalMode::Safe;

    let tool_call = |call_id: &str, command: &str| FromAgent::ToolCall {
        call_id: call_id.to_string(),
        tool: "bash".to_string(),
        args: serde_json::json!({ "command": command }),
        requires_approval: true,
        approval_inline_env: None,
    };

    // First approval opens the modal in the single-call variant.
    app.handle_agent_message(tool_call("call-1", "git status"))
        .await
        .expect("first tool call");
    assert_eq!(app.active_modal, ActiveModal::Approval);
    assert_eq!(
        approval_modal_kind(&app.approval_controller),
        ApprovalModalKind::Single
    );

    // Second approval arrives while the modal is open: it enqueues and the
    // render path now selects the batched variant.
    app.handle_agent_message(tool_call("call-2", "cargo test"))
        .await
        .expect("second tool call");
    assert_eq!(app.active_modal, ActiveModal::Approval);
    assert_eq!(app.approval_controller.total_count(), 2);
    assert_eq!(
        approval_modal_kind(&app.approval_controller),
        ApprovalModalKind::Batched
    );

    // The first request is still the head of the queue.
    assert_eq!(
        app.approval_controller
            .current()
            .map(|r| r.call_id.as_str()),
        Some("call-1")
    );

    // The batched modal the render path builds shows both calls at once.
    let modal = BatchedApprovalModal::new(app.approval_controller.pending())
        .selected(app.approval_controller.selected_index());
    let area = ratatui::layout::Rect::new(0, 0, 100, 30);
    let mut buf = ratatui::buffer::Buffer::empty(area);
    ratatui::widgets::Widget::render(modal, area, &mut buf);
    let text: String = buf
        .content
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(text.contains("2 Actions Require Approval"));
}

// ─────────────────────────────────────────────────────────────────────────
// Guardian approval tests
// ─────────────────────────────────────────────────────────────────────────

/// A guardian whose LLM transport is stubbed to return a fixed raw response.
fn guardian_stub(raw: &'static str) -> crate::safety::guardian::Guardian {
    crate::safety::guardian::Guardian::new(
        std::sync::Arc::new(move |_| {
            Box::pin(async move { Ok(raw.to_string()) })
                as std::pin::Pin<
                    Box<
                        dyn std::future::Future<
                                Output = Result<String, crate::safety::guardian::GuardianError>,
                            > + Send,
                    >,
                >
        }),
        crate::safety::guardian::GUARDIAN_TIMEOUT,
    )
}

/// A guardian whose review never resolves (exercises the fail-closed timeout).
fn guardian_never_responds() -> crate::safety::guardian::Guardian {
    crate::safety::guardian::Guardian::new(
        std::sync::Arc::new(|_| {
            Box::pin(std::future::pending::<
                Result<String, crate::safety::guardian::GuardianError>,
            >())
        }),
        crate::safety::guardian::GUARDIAN_TIMEOUT,
    )
}

/// Drive a bash tool call through the agent-message handler in Safe mode so
/// every call requires approval.
async fn drive_guarded_tool_call(app: &mut App, call_id: &str) {
    app.handle_agent_message(FromAgent::ToolCall {
        call_id: call_id.to_string(),
        tool: "bash".to_string(),
        args: serde_json::json!({ "command": "true" }),
        requires_approval: true,
        approval_inline_env: None,
    })
    .await
    .expect("handle tool call");
}

/// Wait for the spawned guardian review to be applied.
async fn settle_guardian(app: &mut App) {
    for _ in 0..1000 {
        if app
            .poll_guardian_verdicts()
            .await
            .expect("apply guardian verdicts")
        {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("guardian review did not complete");
}

fn assert_fell_back_to_modal(app: &App, call_id: &str) {
    let request = app
        .approval_controller
        .current()
        .expect("approval request queued for the human");
    assert_eq!(request.call_id, call_id);
    assert_eq!(app.active_modal, ActiveModal::Approval);
    assert!(
        !app.pending_guardian_reviews.contains(call_id),
        "no guardian review may still be in flight for the call"
    );
    let exec = app.tool_history.get(call_id).expect("history entry");
    assert_ne!(exec.approved, Some(true), "call must not be auto-approved");
}

#[tokio::test]
async fn guardian_auto_approve_executes_without_modal() {
    let mut app = new_test_app();
    app.state.approval_mode = ApprovalMode::Safe;
    app.guardian = Some(guardian_stub(
        r#"{"decision":"allow","reason":"routine no-op"}"#,
    ));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    app.tool_response_tx = Some(tx);

    drive_guarded_tool_call(&mut app, "call-g1").await;
    // The modal is not shown while the guardian reviews.
    assert!(app.approval_controller.current().is_none());
    assert_eq!(app.active_modal, ActiveModal::None);

    settle_guardian(&mut app).await;

    // Auto-allow: no modal, approval recorded, the approval is relayed to
    // the native agent (which owns execution), and a transcript note records
    // the silent approval.
    assert!(app.approval_controller.current().is_none());
    assert_eq!(app.active_modal, ActiveModal::None);
    let exec = app.tool_history.get("call-g1").expect("history entry");
    assert_eq!(exec.approved, Some(true));
    let (relayed_call_id, relayed_approved, relayed_result, relayed_source, _consumed) =
        rx.try_recv().expect("approval relayed to the native agent");
    assert_eq!(relayed_call_id, "call-g1");
    assert!(relayed_approved);
    assert!(relayed_result.is_none());
    assert_eq!(relayed_source, crate::agent::ExecutionSource::Native);
    assert!(
        app.state
            .messages
            .iter()
            .any(|m| m.content.contains("auto-approved by guardian"))
    );
}

#[tokio::test]
async fn guardian_deny_falls_back_to_human_modal() {
    let mut app = new_test_app();
    app.state.approval_mode = ApprovalMode::Safe;
    app.guardian = Some(guardian_stub(
        r#"{"decision":"deny","reason":"cannot establish purpose"}"#,
    ));

    drive_guarded_tool_call(&mut app, "call-g2").await;
    settle_guardian(&mut app).await;

    assert_fell_back_to_modal(&app, "call-g2");
}

#[tokio::test]
async fn guardian_malformed_output_fails_closed_to_human_modal() {
    let mut app = new_test_app();
    app.state.approval_mode = ApprovalMode::Safe;
    app.guardian = Some(guardian_stub("looks fine to me, go ahead"));

    drive_guarded_tool_call(&mut app, "call-g3").await;
    settle_guardian(&mut app).await;

    assert_fell_back_to_modal(&app, "call-g3");
    assert!(
        app.state
            .messages
            .iter()
            .any(|m| m.content.contains("Guardian review of 'bash' failed"))
    );
}

#[tokio::test(start_paused = true)]
async fn guardian_timeout_fails_closed_to_human_modal() {
    let mut app = new_test_app();
    app.state.approval_mode = ApprovalMode::Safe;
    app.guardian = Some(guardian_never_responds());

    drive_guarded_tool_call(&mut app, "call-g4").await;
    // Paused clock: park the test task on a timer so the runtime auto-advances
    // past the guardian's 10s deadline without real waiting.
    tokio::time::sleep(
        crate::safety::guardian::GUARDIAN_TIMEOUT + std::time::Duration::from_secs(1),
    )
    .await;
    settle_guardian(&mut app).await;

    assert_fell_back_to_modal(&app, "call-g4");
    assert!(
        app.state
            .messages
            .iter()
            .any(|m| m.content.contains("Guardian review of 'bash' failed"))
    );
}

#[tokio::test]
async fn guardian_disabled_leaves_approval_flow_untouched() {
    let mut app = new_test_app();
    app.state.approval_mode = ApprovalMode::Safe;
    app.guardian = None;

    drive_guarded_tool_call(&mut app, "call-g5").await;

    // The human modal shows immediately; no guardian review is in flight.
    assert_fell_back_to_modal(&app, "call-g5");
    assert!(
        !app.poll_guardian_verdicts()
            .await
            .expect("apply guardian verdicts")
    );
    assert!(
        !app.state
            .messages
            .iter()
            .any(|m| m.content.contains("guardian"))
    );
}

#[tokio::test]
async fn yolo_mode_surfaces_soft_firewall_findings_after_auto_approval() {
    let mut app = new_test_app();
    app.state.approval_mode = ApprovalMode::Yolo;

    app.handle_agent_message(FromAgent::ToolCall {
        call_id: "call-auto-security".to_string(),
        tool: "read".to_string(),
        args: serde_json::json!({"file_path": "/etc/passwd"}),
        requires_approval: false,
        approval_inline_env: None,
    })
    .await
    .expect("auto-approved tool event");

    assert!(app.state.messages.iter().any(|message| {
        message
            .content
            .contains("Auto mode allowed Read passwd. Security finding: Reading system file")
    }));
}

#[tokio::test]
async fn guardian_review_interrupted_before_completion_is_ignored() {
    let mut app = new_test_app();
    app.state.approval_mode = ApprovalMode::Safe;
    app.guardian = Some(guardian_stub(
        r#"{"decision":"allow","reason":"routine no-op"}"#,
    ));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    app.tool_response_tx = Some(tx);

    drive_guarded_tool_call(&mut app, "call-g6").await;
    assert!(app.pending_guardian_reviews.contains("call-g6"));

    // Ctrl+C while the review is in flight.
    app.cancel_pending_guardian_reviews();
    settle_guardian(&mut app).await;

    // The late allow verdict must neither relay an approval to the native
    // agent nor show a modal.
    assert!(app.approval_controller.current().is_none());
    assert_eq!(app.active_modal, ActiveModal::None);
    assert!(
        rx.try_recv().is_err(),
        "no approval may be relayed after the interrupt"
    );
    let exec = app.tool_history.get("call-g6").expect("history entry");
    assert_ne!(exec.approved, Some(true));
}

/// Regression test for the review finding on #3128: mutating/destructive
/// tools must never be guardian-eligible, no matter what the guardian would
/// have said. Wired at the `spawn_guardian_review` call site via
/// `guardian_may_auto_approve`, not left to the review model's own
/// judgment. A stub that would say "allow" to anything proves the ceiling
/// is enforced in code: if it were not, this call would be silently
/// executed instead of reaching the human modal.
#[tokio::test]
async fn guardian_ceiling_denies_write_even_when_guardian_would_allow() {
    let mut app = new_test_app();
    app.state.approval_mode = ApprovalMode::Safe;
    app.guardian = Some(guardian_stub(
        r#"{"decision":"allow","reason":"looks fine"}"#,
    ));

    app.handle_agent_message(FromAgent::ToolCall {
        call_id: "call-write-1".to_string(),
        tool: "write".to_string(),
        args: serde_json::json!({"file_path": "notes.txt", "content": "hi"}),
        requires_approval: true,
        approval_inline_env: None,
    })
    .await
    .expect("handle tool call");

    // No guardian review was even spawned for this tool.
    assert!(!app.pending_guardian_reviews.contains("call-write-1"));
    assert_fell_back_to_modal(&app, "call-write-1");
}

/// Same ceiling, for the sandbox-bypass escape hatch specifically: a
/// `bypass_sandbox` request must reach the human modal even with the
/// guardian enabled and stubbed to allow.
#[tokio::test]
async fn guardian_ceiling_denies_sandbox_bypass_even_when_guardian_would_allow() {
    let mut app = new_test_app();
    app.state.approval_mode = ApprovalMode::Safe;
    app.guardian = Some(guardian_stub(
        r#"{"decision":"allow","reason":"looks fine"}"#,
    ));

    app.handle_agent_message(FromAgent::ToolCall {
        call_id: "call-bypass-1".to_string(),
        tool: "bash".to_string(),
        args: serde_json::json!({"command": "true", "bypass_sandbox": true}),
        requires_approval: true,
        approval_inline_env: None,
    })
    .await
    .expect("handle tool call");

    assert!(!app.pending_guardian_reviews.contains("call-bypass-1"));
    assert_fell_back_to_modal(&app, "call-bypass-1");
}

/// Regression test for the review finding on #3128: every guardian verdict
/// (allow, deny, or a fail-closed error) must leave a durable audit record
/// in the session file, not just a transcript banner that scrolls away and
/// an in-memory `ToolHistory` entry that eventually ages out.
#[tokio::test]
async fn guardian_allow_writes_a_durable_audit_record() {
    let mut app = new_test_app();
    app.state.approval_mode = ApprovalMode::Safe;
    app.guardian = Some(guardian_stub(
        r#"{"decision":"allow","reason":"routine test run"}"#,
    ));
    let temp = tempdir().expect("temp session directory");
    app.session_manager =
        SessionManager::with_sessions_dir("guardian-audit-test", temp.path().to_path_buf());
    app.ensure_session_started()
        .expect("session should start for the audit test");

    drive_guarded_tool_call(&mut app, "call-audit-1").await;
    settle_guardian(&mut app).await;

    app.flush_session();
    let session_id = app.state.session_id.clone().expect("session id set");
    let session = app
        .session_manager
        .load_session(&session_id)
        .expect("reload the session file the audit record was written to");
    let context_manifest = session
        .header
        .unified_context_manifest
        .as_ref()
        .expect("interactive sessions persist their context generation");
    assert_eq!(
        context_manifest
            .get("manifestSha256")
            .and_then(serde_json::Value::as_str)
            .map(str::len),
        Some(71)
    );

    // `SessionEntry::Custom` entries are intentionally excluded from
    // `ParsedSession` (they are extension-owned, opaque payloads), so this
    // reads the raw JSONL directly rather than through the normal replay
    // path -- exactly how an external auditor would inspect the record.
    let raw = std::fs::read_to_string(&session.file_path).expect("read session file");
    let audit_entries: Vec<crate::session::CustomEntry> = raw
        .lines()
        .filter_map(|line| serde_json::from_str::<SessionEntry>(line).ok())
        .filter_map(|entry| match entry {
            SessionEntry::Custom(custom) if custom.custom_type == "guardian_decision" => {
                Some(custom)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        audit_entries.len(),
        1,
        "exactly one guardian_decision entry expected, found: {audit_entries:?}"
    );
    let data = audit_entries[0]
        .data
        .as_ref()
        .expect("guardian_decision entry carries structured data");
    assert_eq!(data["callId"], "call-audit-1");
    assert_eq!(data["tool"], "bash");
    assert_eq!(data["outcome"], "allow");
    assert_eq!(data["reason"], "routine test run");
}

// ─────────────────────────────────────────────────────────────────────────
// Dual-executor fix (issues #3149, #3156): the native agent is the sole
// owner of the approve/execute decision. `app.rs` must trust
// `FromAgent::ToolCall`'s `requires_approval` field instead of recomputing
// its own verdict, and must never execute a tool the native agent has
// already auto-executed inline.
// ─────────────────────────────────────────────────────────────────────────

/// Regression test for #3156: before the fix, receiving a `ToolCall` with
/// `requires_approval: false` (the native agent's own auto-approve
/// decision, which had already executed the tool inline) made `app.rs`
/// recompute its own verdict and, agreeing it needed no approval, execute
/// the tool a second time. The app now owns no tool-execution path: it only
/// records the event and leaves execution to the native agent.
#[tokio::test]
async fn auto_approved_tool_call_does_not_execute_a_second_time() {
    let mut app = new_test_app();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    app.tool_response_tx = Some(tx);

    app.handle_agent_message(FromAgent::ToolCall {
        call_id: "call-1".to_string(),
        tool: "bash".to_string(),
        args: serde_json::json!({"command": "ls -la"}),
        requires_approval: false,
        approval_inline_env: None,
    })
    .await
    .expect("handle auto-approved tool call");

    assert!(rx.try_recv().is_err());
    assert_ne!(
        app.active_modal,
        ActiveModal::Approval,
        "an auto-approved call must never pop the approval modal"
    );
}

/// Approval-gated native execution reports completion through ToolOutput/
/// ToolEnd after the agent runs it in order. Those events must complete the
/// durable history and persisted session just like auto-approved calls.
#[tokio::test]
async fn approved_native_tool_events_complete_tool_history() {
    let mut app = new_test_app();
    let temp = tempdir().expect("temp session directory");
    app.session_manager =
        SessionManager::with_sessions_dir("native-tool-test", temp.path().to_path_buf());

    app.handle_agent_message(FromAgent::ResponseStart {
        response_id: "response-native-tool".to_string(),
    })
    .await
    .expect("handle response start");
    app.handle_agent_message(FromAgent::ToolCall {
        call_id: "call-native".to_string(),
        tool: "bash".to_string(),
        args: serde_json::json!({"command": "printf native-output"}),
        requires_approval: true,
        approval_inline_env: None,
    })
    .await
    .expect("handle approval-gated tool call");
    app.handle_agent_message(FromAgent::ToolOutput {
        call_id: "call-native".to_string(),
        content: "native-output".to_string(),
    })
    .await
    .expect("handle native tool output");
    app.handle_agent_message(FromAgent::ToolEnd {
        call_id: "call-native".to_string(),
        success: true,
        result: None,
        receipt: None,
    })
    .await
    .expect("handle native tool end");

    let execution = app
        .tool_history
        .get("call-native")
        .expect("native execution should remain in tool history");
    assert!(execution.success, "native execution should be completed");
    assert_eq!(execution.output.as_deref(), Some("native-output"));
    assert!(
        execution.duration.is_some(),
        "completion must stop the running timer"
    );
    assert_eq!(app.tool_history.global_stats().successes, 1);

    let session_path = find_session_jsonl(temp.path());
    let session = std::fs::read_to_string(session_path).expect("read persisted session");
    assert!(session.contains(r#""role":"toolResult""#));
    assert!(session.contains(r#""toolCallId":"call-native""#));
    assert!(session.contains("native-output"));
}

#[tokio::test]
async fn cancelled_native_tool_stays_cancelled_without_failure_stats() {
    let mut app = new_test_app();
    let temp = tempdir().expect("temp session directory");
    app.session_manager =
        SessionManager::with_sessions_dir("native-cancel-test", temp.path().to_path_buf());
    app.handle_agent_message(FromAgent::ResponseStart {
        response_id: "response-native-cancel".to_string(),
    })
    .await
    .expect("handle response start");
    app.handle_agent_message(FromAgent::ToolCall {
        call_id: "call-cancelled".to_string(),
        tool: "bash".to_string(),
        args: serde_json::json!({"command": "sleep 30"}),
        requires_approval: true,
        approval_inline_env: None,
    })
    .await
    .expect("handle approval-gated tool call");
    app.handle_agent_message(FromAgent::ToolEnd {
        call_id: "call-cancelled".to_string(),
        success: false,
        result: Some(ToolResult::failure(
            "Tool execution cancelled during Running",
        )),
        receipt: Some(
            ToolExecution::cancelled(
                "call-cancelled",
                "bash",
                crate::agent::ExecutionSource::Native,
                crate::agent::ExecutionPhase::Running,
            )
            .receipt,
        ),
    })
    .await
    .expect("handle cancelled native tool");

    let row = app
        .state
        .messages
        .iter()
        .flat_map(|message| &message.tool_calls)
        .find(|call| call.call_id == "call-cancelled")
        .expect("cancelled tool row");
    assert_eq!(row.status, crate::state::ToolCallStatus::Cancelled);

    let execution = app
        .tool_history
        .get("call-cancelled")
        .expect("cancelled execution should leave tool history");
    assert!(execution.duration.is_some());
    assert_eq!(app.tool_history.global_stats().failures, 0);
}

#[tokio::test]
async fn signal_shutdown_drains_final_agent_events_before_session_flush() {
    let mut app = new_test_app();
    let temp = tempdir().expect("temp session directory");
    app.session_manager =
        SessionManager::with_sessions_dir("shutdown-drain-test", temp.path().to_path_buf());

    app.handle_agent_message(FromAgent::ResponseStart {
        response_id: "response-shutdown".to_string(),
    })
    .await
    .expect("handle response start");
    app.handle_agent_message(FromAgent::ToolCall {
        call_id: "call-shutdown".to_string(),
        tool: "bash".to_string(),
        args: serde_json::json!({"command": "sleep 30"}),
        requires_approval: false,
        approval_inline_env: None,
    })
    .await
    .expect("handle native tool call");

    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
    app.native_event_rx = Some(event_rx);
    event_tx
        .send(FromAgent::ToolEnd {
            call_id: "call-shutdown".to_string(),
            success: false,
            result: Some(ToolResult::failure(
                "Tool execution cancelled during Running",
            )),
            receipt: Some(
                ToolExecution::cancelled(
                    "call-shutdown",
                    "bash",
                    crate::agent::ExecutionSource::Native,
                    crate::agent::ExecutionPhase::Running,
                )
                .receipt,
            ),
        })
        .expect("queue terminal tool event");
    event_tx
        .send(FromAgent::ResponseEnd {
            response_id: "done".to_string(),
            usage: None,
        })
        .expect("queue response boundary event");
    event_tx
        .send(FromAgent::TurnInterrupted {
            response_id: "done".to_string(),
            reason: "shutdown".to_string(),
        })
        .expect("queue interrupted turn terminal");
    drop(event_tx);

    let _ = app.signal_shutdown_teardown().await;

    let execution = app
        .tool_history
        .get("call-shutdown")
        .expect("shutdown must drain terminal tool completion");
    assert!(
        execution.duration.is_some(),
        "drained cancellation must terminalize tool history"
    );
    let row = app
        .state
        .messages
        .iter()
        .flat_map(|message| &message.tool_calls)
        .find(|call| call.call_id == "call-shutdown")
        .expect("drained cancellation must update the tool row");
    assert_eq!(row.status, crate::state::ToolCallStatus::Cancelled);
    assert!(!app.state.busy, "interrupted turn terminal must be applied");

    let session_path = find_session_jsonl(temp.path());
    let session = std::fs::read_to_string(session_path).expect("read persisted session");
    assert!(
        session.contains(r#""toolCallId":"call-shutdown""#),
        "shutdown must persist the drained terminal tool result"
    );
}

#[tokio::test]
async fn auto_approved_extract_document_persists_attachment_text() {
    let mut app = new_test_app();
    let temp = tempdir().expect("temp session directory");
    app.session_manager =
        SessionManager::with_sessions_dir("native-extract-test", temp.path().to_path_buf());

    app.handle_agent_message(FromAgent::ResponseStart {
        response_id: "response-native-extract".to_string(),
    })
    .await
    .expect("handle response start");
    app.handle_agent_message(FromAgent::ToolCall {
        call_id: "call-document".to_string(),
        tool: "extract_document".to_string(),
        args: serde_json::json!({"url": "https://example.com/document.pdf"}),
        requires_approval: false,
        approval_inline_env: None,
    })
    .await
    .expect("handle auto-approved document extraction");
    app.handle_agent_message(FromAgent::ToolOutput {
        call_id: "call-document".to_string(),
        content: "extracted document text".to_string(),
    })
    .await
    .expect("handle extracted text");
    app.handle_agent_message(FromAgent::ToolEnd {
        call_id: "call-document".to_string(),
        success: true,
        result: Some(ToolResult {
            success: true,
            output: "extracted document text".to_string(),
            error: None,
            details: Some(serde_json::json!({"url": "attachment-document"})),
        }),
        receipt: None,
    })
    .await
    .expect("handle document extraction end");

    let session_path = find_session_jsonl(temp.path());
    let session = std::fs::read_to_string(session_path).expect("read persisted session");
    assert!(session.contains(r#""type":"attachment_extract""#));
    assert!(session.contains(r#""attachmentId":"attachment-document""#));
    assert!(session.contains(r#""extractedText":"extracted document text""#));
}

#[tokio::test]
async fn auto_approved_native_tool_failure_preserves_the_exact_error() {
    let mut app = new_test_app();
    let temp = tempdir().expect("temp session directory");
    app.session_manager =
        SessionManager::with_sessions_dir("native-failure-test", temp.path().to_path_buf());

    app.handle_agent_message(FromAgent::ResponseStart {
        response_id: "response-native-failure".to_string(),
    })
    .await
    .expect("handle response start");
    app.handle_agent_message(FromAgent::ToolCall {
        call_id: "call-failure".to_string(),
        tool: "web_fetch".to_string(),
        args: serde_json::json!({"url": "not-a-url"}),
        requires_approval: false,
        approval_inline_env: None,
    })
    .await
    .expect("handle auto-approved tool call");
    app.handle_agent_message(FromAgent::ToolEnd {
        call_id: "call-failure".to_string(),
        success: false,
        result: Some(ToolResult::failure("Invalid URL: not-a-url")),
        receipt: None,
    })
    .await
    .expect("handle native tool failure");

    let execution = app
        .tool_history
        .get("call-failure")
        .expect("failed native execution should remain in tool history");
    assert_eq!(execution.error.as_deref(), Some("Invalid URL: not-a-url"));

    let session_path = find_session_jsonl(temp.path());
    let session = std::fs::read_to_string(session_path).expect("read persisted session");
    assert!(session.contains("Invalid URL: not-a-url"));
}

/// Regression test for #3149: before the fix, `app.rs` recomputed its own
/// `needs_approval` from `state.approval_mode` instead of trusting the
/// agent's decision. Now that the native agent's gate is mode-aware (see
/// `tool_requires_approval` in `agent/native.rs`), Safe mode's `ToolCall`
/// events arrive with `requires_approval: true`; `app.rs` must show the
/// approval modal and must not execute before the user decides.
#[tokio::test]
async fn requires_approval_tool_call_waits_for_the_modal_before_executing() {
    let mut app = new_test_app();

    app.handle_agent_message(FromAgent::ToolCall {
        call_id: "call-2".to_string(),
        tool: "bash".to_string(),
        args: serde_json::json!({"command": "ls"}),
        requires_approval: true,
        approval_inline_env: None,
    })
    .await
    .expect("handle approval-required tool call");

    assert_eq!(app.active_modal, ActiveModal::Approval);
    assert!(app.approval_controller.current().is_some());
}

#[tokio::test]
async fn batched_approvals_relay_fifo_decisions_without_starting_parallel_tools() {
    let mut app = new_test_app();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    app.tool_response_tx = Some(tx);

    for (call_id, command) in [("call-a", "printf a"), ("call-b", "printf b")] {
        app.handle_agent_message(FromAgent::ToolCall {
            call_id: call_id.to_string(),
            tool: "bash".to_string(),
            args: serde_json::json!({"command": command}),
            requires_approval: true,
            approval_inline_env: None,
        })
        .await
        .expect("queue approval-required tool call");
    }

    app.handle_key(KeyCode::Char('a'), CrosstermModifiers::NONE)
        .await
        .expect("approve batch");

    let decisions = [rx.try_recv().unwrap(), rx.try_recv().unwrap()];
    assert_eq!(decisions[0].0, "call-a");
    assert!(decisions[0].1);
    assert!(decisions[0].2.is_none());
    assert_eq!(decisions[1].0, "call-b");
    assert!(decisions[1].1);
    assert!(decisions[1].2.is_none());
    assert!(rx.try_recv().is_err());
}

/// Regression test for #3149: Deny must not execute the tool, and must
/// relay a `(call_id, false, None, _)` denial back to the native agent so its
/// `wait_for_tool_response` resolves to a denial instead of hanging (see
/// `denied_tool_response_is_an_error_result_and_never_executes` in
/// `agent/native.rs`, which proves that tuple becomes an error result for
/// the model).
#[tokio::test]
async fn deny_never_executes_and_relays_the_denial_to_the_agent() {
    let mut app = new_test_app();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    app.tool_response_tx = Some(tx);

    app.handle_tool_approval(
        "call-3".to_string(),
        "bash".to_string(),
        serde_json::json!({"command": "rm -rf /"}),
        false,
    )
    .await
    .expect("handle deny");

    let (call_id, approved, result, _source, _consumed) = rx
        .try_recv()
        .expect("deny must send a response back to the native agent");
    assert_eq!(call_id, "call-3");
    assert!(!approved);
    assert!(result.is_none());
}

// ─────────────────────────────────────────────────────────────────────────
// Approval reason combining (review finding on #3144)
// ─────────────────────────────────────────────────────────────────────────

/// Regression test: when both a firewall reason and a sandbox-bypass
/// warning apply to the same request, `BatchedApprovalModal` renders only
/// `ApprovalRequest::summary()`'s first line. The bypass warning must be
/// that first line, or a user approving from a batch never sees it and can
/// unknowingly approve an unsandboxed command.
#[test]
fn combined_reason_puts_bypass_warning_on_the_first_line() {
    let firewall = "Blocked by action firewall: writes outside workspace".to_string();
    let bypass = "Agent is asking to run this command WITHOUT Maestro's native \
                  sandbox (a sandboxed attempt likely just failed)."
        .to_string();

    let combined = combine_approval_reason(Some(firewall.clone()), Some(bypass.clone()))
        .expect("both reasons present must combine to Some");

    let request = ApprovalRequest::new(
        "call-1".to_string(),
        "bash".to_string(),
        serde_json::json!({"command": "rm -rf /tmp/x", "bypass_sandbox": true}),
    )
    .with_reason(combined);

    let summary = request.summary(200);
    assert!(
        summary.contains("WITHOUT Maestro's native"),
        "batched-modal summary must surface the bypass warning, got: {summary:?}"
    );
    assert!(
        !summary.contains("action firewall"),
        "the firewall reason must not occupy the first line ahead of the bypass \
         warning, got: {summary:?}"
    );
}

#[test]
fn combine_approval_reason_handles_single_and_no_reason_cases() {
    assert_eq!(combine_approval_reason(None, None), None);
    assert_eq!(
        combine_approval_reason(Some("firewall".to_string()), None),
        Some("firewall".to_string())
    );
    assert_eq!(
        combine_approval_reason(None, Some("bypass".to_string())),
        Some("bypass".to_string())
    );
}

#[test]
fn signal_shutdown_only_ends_a_started_terminal_session() {
    let mut app = new_test_app();
    app.terminal_notifier = crate::notifications::TerminalStateNotifier::new(false, true, false);

    assert!(
        app.terminal_session_ended_sequences().is_empty(),
        "shutdown before run_inner starts must not pop the title stack"
    );

    app.terminal_session_started = true;
    let sequences = app.terminal_session_ended_sequences();
    assert!(
        sequences
            .iter()
            .any(|sequence| sequence == crate::notifications::TITLE_STACK_POP),
        "a started terminal session must restore the saved title"
    );
    assert!(!app.terminal_session_started);
}

#[test]
fn queued_agent_activity_skips_the_terminal_poll_delay() {
    assert_eq!(terminal_poll_timeout(true, true), Duration::ZERO);
    assert_eq!(terminal_poll_timeout(false, true), Duration::ZERO);
    assert_eq!(
        terminal_poll_timeout(true, false),
        Duration::from_millis(33)
    );
    assert_eq!(
        terminal_poll_timeout(false, false),
        Duration::from_millis(100)
    );
}

#[tokio::test]
async fn dex_lifecycle_requires_explicit_terminal_event() {
    use crate::components::dex_companion::DexCompanionState;
    let mut app = new_test_app();
    app.handle_agent_message(FromAgent::ResponseStart {
        response_id: "dex-turn".into(),
    })
    .await
    .unwrap();
    assert_eq!(app.dex_terminal, None);
    app.handle_agent_message(FromAgent::ResponseEnd {
        response_id: "dex-turn".into(),
        usage: None,
    })
    .await
    .unwrap();
    assert_eq!(
        app.dex_terminal, None,
        "a response boundary is not completion"
    );
    app.handle_agent_message(FromAgent::TurnCompleted {
        response_id: "dex-turn".into(),
        coding_completion: None,
        coding_child_records: Vec::new(),
    })
    .await
    .unwrap();
    assert_eq!(app.dex_terminal, Some(DexCompanionState::Finished));
    // An old, completed pose cannot restart because a completion is delivered twice.
    let settled = Instant::now()
        .checked_sub(Duration::from_secs(10))
        .expect("settled animation timestamp");
    app.dex_pose_started = settled;
    app.handle_agent_message(FromAgent::TurnCompleted {
        response_id: "dex-turn".into(),
        coding_completion: None,
        coding_child_records: Vec::new(),
    })
    .await
    .unwrap();
    assert_eq!(app.dex_pose_started, settled);
    app.handle_agent_message(FromAgent::ResponseStart {
        response_id: "dex-next".into(),
    })
    .await
    .unwrap();
    assert_eq!(app.dex_terminal, None);
    app.handle_agent_message(FromAgent::TurnInterrupted {
        response_id: "dex-next".into(),
        reason: "user".into(),
    })
    .await
    .unwrap();
    assert_eq!(app.dex_terminal, Some(DexCompanionState::Ready));
}

#[tokio::test]
async fn dex_standalone_side_question_owns_its_terminal_state() {
    use crate::components::dex_companion::DexCompanionState;
    let mut app = new_test_app();
    for error in [Some("failure".to_owned()), None] {
        app.dex_terminal = Some(DexCompanionState::Finished);
        app.handle_agent_message(FromAgent::SideQuestionStart {
            side_id: "dex-side".into(),
            question: "question".into(),
            standalone: true,
        })
        .await
        .unwrap();
        assert_eq!(app.dex_terminal, None);
        app.handle_agent_message(FromAgent::SideQuestionEnd {
            side_id: "dex-side".into(),
            question: "question".into(),
            answer: "answer".into(),
            standalone: true,
            error: error.clone(),
            provider_error_kind: None,
            usage: None,
        })
        .await
        .unwrap();
        assert_eq!(
            app.dex_terminal,
            Some(if error.is_some() {
                DexCompanionState::Failed
            } else {
                DexCompanionState::Finished
            })
        );
    }
    app.dex_terminal = None;
    app.handle_agent_message(FromAgent::SideQuestionEnd {
        side_id: "dex-side".into(),
        question: "question".into(),
        answer: "answer".into(),
        standalone: false,
        error: Some("failure".into()),
        provider_error_kind: None,
        usage: None,
    })
    .await
    .unwrap();
    assert_eq!(
        app.dex_terminal, None,
        "concurrent side question does not finish main turn"
    );
}

#[tokio::test]
async fn shift_tab_cycles_thinking_while_busy_and_preserves_draft_and_approvals() {
    let mut app = new_test_app();
    app.current_model = "fixture/custom-thinking".into();
    app.current_thinking_level = ThinkingLevel::Off;
    app.state.set_input("unfinished prompt");
    app.state.busy = true;
    let mode = app.state.interaction_mode;
    let approvals = app.state.approval_mode;
    for expected in [
        ThinkingLevel::Minimal,
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
        ThinkingLevel::Max,
        ThinkingLevel::Off,
    ] {
        app.handle_key(KeyCode::BackTab, CrosstermModifiers::SHIFT)
            .await
            .unwrap();
        assert_eq!(app.current_thinking_level, expected);
        assert_eq!(app.state.thinking_level, expected);
        assert_eq!(app.state.input(), "unfinished prompt");
        assert!(app.state.busy);
        assert_eq!(app.state.interaction_mode, mode);
        assert_eq!(app.state.approval_mode, approvals);
    }
}

#[tokio::test]
async fn shift_tab_terminal_encodings_persist_thinking_changes() {
    let mut app = new_test_app();
    let dir = tempfile::tempdir().unwrap();
    app.session_manager = SessionManager::with_sessions_dir("thinking-test", dir.path());
    app.current_model = "fixture/custom-thinking".into();
    app.current_thinking_level = ThinkingLevel::Off;
    app.ensure_session_started().unwrap();
    for (code, modifiers) in [
        (KeyCode::BackTab, CrosstermModifiers::NONE),
        (KeyCode::Tab, CrosstermModifiers::SHIFT),
    ] {
        app.handle_key(code, modifiers).await.unwrap();
    }
    assert_eq!(app.current_thinking_level, ThinkingLevel::Low);
    app.session_manager.flush().unwrap();
    let content = std::fs::read_to_string(find_session_jsonl(dir.path())).unwrap();
    let changes: Vec<_> = content
        .lines()
        .filter_map(
            |line| match serde_json::from_str::<SessionEntry>(line).unwrap() {
                SessionEntry::ThinkingLevelChange(change) => Some(change.thinking_level),
                _ => None,
            },
        )
        .collect();
    assert_eq!(changes, vec![ThinkingLevel::Minimal, ThinkingLevel::Low]);
}

#[tokio::test]
async fn shift_tab_in_modal_does_not_change_thinking() {
    let mut app = new_test_app();
    app.active_modal = ActiveModal::ShortcutsHelp;
    let original = app.current_thinking_level;
    app.handle_key(KeyCode::BackTab, CrosstermModifiers::SHIFT)
        .await
        .unwrap();
    assert_eq!(app.current_thinking_level, original);
}

#[tokio::test]
async fn dex_suggestion_acceptance_only_fills_composer_and_respects_dismissal() {
    use crate::components::dex_companion::DexCompanionState;
    let mut app = new_test_app();
    app.ui_prefs.dex_suggestions_disabled = false;
    app.active_modal = ActiveModal::None;
    app.state.add_user_message("Edit a file".into());
    app.state
        .messages
        .last_mut()
        .unwrap()
        .tool_calls
        .push(crate::state::ToolCallState {
            call_id: "edit-1".into(),
            tool: "edit".into(),
            args: serde_json::json!({}),
            status: crate::state::ToolCallStatus::Completed,
            output: String::new(),
        });
    app.handle_agent_message(FromAgent::TurnCompleted {
        response_id: "dex-suggest".into(),
        coding_completion: None,
        coding_child_records: Vec::new(),
    })
    .await
    .unwrap();
    assert_eq!(app.dex_terminal, Some(DexCompanionState::Finished));
    let count = app.state.messages.len();
    assert_eq!(app.dex_next_prompt(), Some("/diff"));
    app.handle_key(KeyCode::Right, CrosstermModifiers::NONE)
        .await
        .unwrap();
    assert_eq!(app.state.input(), "/diff");
    assert!(!app.state.busy);
    assert_eq!(app.state.messages.len(), count);
    app.state.set_input("");
    assert_eq!(app.dex_next_prompt(), None);
}

#[test]
fn dex_return_recap_is_factual_and_consumed_once() {
    let mut app = new_test_app();
    app.ui_prefs.dex_recap_disabled = false;
    app.dex_terminal = Some(crate::components::dex_companion::DexCompanionState::Finished);
    app.dex_delight.clock = maestro_presentation::clock::ViewClock::Fixed(Duration::from_secs(181));
    app.dex_delight.attention.update(
        maestro_interaction::Event::FocusLost,
        Duration::ZERO,
        maestro_interaction::Policy::default(),
    );
    app.dex_observe_attention(crate::components::dex_companion::DexCompanionState::Finished);
    app.dex_focus_returned();
    assert!(
        app.dex_delight
            .notice
            .as_deref()
            .unwrap()
            .contains("Last request completed")
    );
    app.dex_delight.notice = None;
    app.dex_focus_returned();
    assert!(app.dex_delight.notice.is_none());
    app.ui_prefs.dex_personality = Some("quiet".into());
    app.ui_prefs.animations = Some(true);
    app.pet_dex();
    assert!(!app.dex_pet_active());
}

#[test]
fn dex_notifications_require_opt_in_and_away_transition_and_deduplicate() {
    use crate::components::dex_companion::DexCompanionState as State;
    use crate::notifications::DexAttention;
    let mut app = new_test_app();
    app.ui_prefs.dex_notifications = true;
    assert_eq!(
        app.dex_observe_attention(State::Finished),
        None,
        "unknown focus is quiet"
    );
    app.terminal_notifier.record_focus(false);
    app.dex_focus_lost();
    assert_eq!(
        app.dex_observe_attention(State::Finished),
        None,
        "old completion is not replayed"
    );
    assert_eq!(app.dex_observe_attention(State::Working), None);
    assert_eq!(
        app.dex_observe_attention(State::NeedsInput),
        Some(DexAttention::NeedsInput)
    );
    assert_eq!(app.dex_observe_attention(State::NeedsInput), None);
    app.dex_begin_turn();
    assert_eq!(
        app.dex_observe_attention(State::NeedsInput),
        Some(DexAttention::NeedsInput),
        "a new turn may need attention without an intervening frame"
    );
    app.state.busy = true;
    assert!(
        crate::dex_delight::recap(&app.state, Some(State::NeedsInput))
            .starts_with("Your answer is needed")
    );
    app.ui_prefs.dex_notifications = false;
    assert_eq!(app.dex_observe_attention(State::Failed), None);
    assert!(
        app.dex_delight.attention.changed_while_away(),
        "recap works with notifications disabled"
    );
    app.ui_prefs.dex_notifications = true;
    app.terminal_notifier.record_focus(true);
    app.dex_focus_returned();
    assert_eq!(app.dex_observe_attention(State::Finished), None);
}

#[tokio::test]
async fn dex_paste_dismisses_suggestions_and_modal_keys_do_not_accept_them() {
    let mut app = new_test_app();
    app.dex_delight.notice = Some("Welcome back".into());
    app.handle_paste("My own draft");
    assert_eq!(app.dex_delight.suggestion.visible(Some("candidate")), None);
    assert!(app.dex_delight.notice.is_none());
    assert_eq!(app.state.input(), "My own draft");
    app.handle_dex_command("appearance");
    app.handle_key(KeyCode::Down, CrosstermModifiers::NONE)
        .await
        .unwrap();
    assert_eq!(
        app.dex_delight.picker.selected().map(|action| action.id),
        Some("accessory-glasses")
    );
    app.handle_key(KeyCode::Tab, CrosstermModifiers::NONE)
        .await
        .unwrap();
    assert_eq!(app.state.input(), "My own draft");
    app.handle_key(KeyCode::Esc, CrosstermModifiers::NONE)
        .await
        .unwrap();
    assert_eq!(app.active_modal, ActiveModal::None);
}

#[test]
fn dex_return_recap_observes_completion_before_the_next_frame() {
    use crate::components::dex_companion::DexCompanionState;
    let mut app = new_test_app();
    app.ui_prefs.dex_recap_disabled = false;
    app.dex_begin_turn();
    app.dex_delight.clock = maestro_presentation::clock::ViewClock::Fixed(Duration::from_secs(181));
    app.dex_delight.attention.update(
        maestro_interaction::Event::FocusLost,
        Duration::ZERO,
        maestro_interaction::Policy::default(),
    );
    app.dex_terminal = Some(DexCompanionState::Finished);
    app.state.busy = false;
    app.dex_focus_returned();
    assert!(
        app.dex_delight
            .notice
            .as_deref()
            .unwrap()
            .contains("Last request completed")
    );
}

#[tokio::test]
async fn dex_attention_updates_from_events_without_rendering() {
    let mut app = new_test_app();
    app.state.busy = true;
    app.dex_begin_turn();
    app.handle_agent_message(FromAgent::Error {
        message: "fixture failure".into(),
        fatal: false,
        terminal: true,
        retryable: false,
    })
    .await
    .unwrap();
    assert_eq!(
        app.dex_delight.attention.last_observed(),
        Some(crate::components::dex_companion::DexCompanionState::Failed)
    );
}

#[test]
fn dex_appearance_command_uses_stable_id_without_executing_work() {
    let registry = crate::commands::build_command_registry();
    for id in ["accent-mint", "accessory-crown", "accessory-bow"] {
        let output = registry
            .execute(&format!("/dex {id}"), ".", None, None)
            .unwrap();
        assert!(
            matches!(output, crate::commands::CommandOutput::Action(crate::commands::CommandAction::SetDexPresentation(value)) if value == id)
        );
    }
    assert!(
        registry
            .execute("/dex not-a-real-setting", ".", None, None)
            .is_err()
    );
}

#[test]
fn dex_pet_reactions_cycle_without_noise_during_work_or_quiet_mode() {
    let mut app = new_test_app();
    app.active_modal = ActiveModal::None;
    app.ui_prefs.dex_personality = Some("standard".into());
    for expected in [
        "Dex appreciates the boop.",
        "A tiny bow from Dex.",
        "Dex is all ears.",
        "Dex appreciates the boop.",
    ] {
        app.pet_dex();
        assert_eq!(app.dex_delight.notice.as_deref(), Some(expected));
    }
    app.dex_delight.notice = None;
    app.state.busy = true;
    app.pet_dex();
    assert!(app.dex_delight.notice.is_none());
    app.state.busy = false;
    app.ui_prefs.dex_personality = Some("quiet".into());
    app.pet_dex();
    assert!(app.dex_delight.notice.is_none());
    app.ui_prefs.dex_personality = Some("standard".into());
    app.active_modal = ActiveModal::ShortcutsHelp;
    app.pet_dex();
    assert!(app.dex_delight.notice.is_none());
    app.active_modal = ActiveModal::None;
    app.pet_dex();
    assert_eq!(
        app.dex_delight.notice.as_deref(),
        Some("A tiny bow from Dex.")
    );
}

#[test]
fn dex_picker_previews_current_choice_without_changing_preferences_and_cancel_restores() {
    use crate::dex_delight::{DexAccent, DexAccessory};
    let mut app = new_test_app();
    app.ui_prefs.dex_accessory = DexAccessory::Crown;
    app.ui_prefs.dex_accent = DexAccent::Mint;
    app.handle_dex_command("appearance");
    assert_eq!(
        app.dex_delight.picker.selected().unwrap().id,
        "accessory-crown"
    );
    app.handle_dex_appearance_key(KeyCode::Down).unwrap();
    assert_eq!(app.dex_look().accessory, DexAccessory::Bow);
    assert_eq!(app.dex_look().accent, DexAccent::Mint);
    assert_eq!(app.ui_prefs.dex_accessory, DexAccessory::Crown);
    assert!(app.dex_delight.notice.is_none());
    app.handle_dex_appearance_key(KeyCode::Esc).unwrap();
    assert_eq!(app.dex_look().accessory, DexAccessory::Crown);
    assert_eq!(app.active_modal, ActiveModal::None);
}

#[tokio::test]
async fn incoming_approval_cancels_the_open_theme_preview() {
    let mut app = new_test_app();
    app.theme_selector.show();
    app.active_modal = ActiveModal::ThemeSelector;
    // Exercise the real asynchronous modal replacement without executing a tool.
    app.handle_agent_message(FromAgent::ToolCall {
        call_id: "preview-approval".into(),
        tool: "read".into(),
        args: serde_json::json!({"file_path": "README.md"}),
        requires_approval: true,
        approval_inline_env: None,
    })
    .await
    .unwrap();
    assert_eq!(app.active_modal, ActiveModal::Approval);
    assert!(
        !app.theme_selector.is_visible(),
        "the displaced preview must be cancelled"
    );
}

#[test]
fn dex_reactions_respect_attention_and_reduced_motion() {
    let mut app = new_test_app();
    app.active_modal = ActiveModal::None;
    app.ui_prefs.dex_personality = Some("standard".into());
    app.ui_prefs.animations = Some(true);
    app.pet_dex();
    assert!(app.dex_pet_active());
    assert!(app.dex_look().pet_frame.is_some());
    app.state.error = Some("Request failed".into());
    assert!(!app.dex_can_delight());
    assert!(!app.dex_pet_active());
    assert!(app.dex_look().pet_frame.is_none());
    app.dex_delight.notice = None;
    app.pet_dex();
    assert!(app.dex_delight.notice.is_none());
    app.state.error = None;
    app.active_modal = ActiveModal::Approval;
    assert!(!app.dex_can_delight());
    assert!(app.dex_look().pet_frame.is_none());
    app.active_modal = ActiveModal::None;
    app.ui_prefs.animations = Some(false);
    app.pet_dex();
    assert!(!app.dex_pet_active());
    assert!(app.dex_look().pet_frame.is_none());
}

#[test]
fn cross_workspace_resume_defers_transcript_until_fresh_agent_and_missing_workspace_stays_open() {
    use std::io::Write;
    let root = tempdir().unwrap();
    let target = root.path().join("retained-worktree");
    std::fs::create_dir(&target).unwrap();
    let sessions = root.path().join("sessions");
    std::fs::create_dir(&sessions).unwrap();
    let mut file =
        std::fs::File::create(sessions.join("2024-01-15T10-30-00-000Z_target.jsonl")).unwrap();
    writeln!(file, "{}", serde_json::json!({"type":"session","id":"target","timestamp":"2024-01-15T10:30:00Z","cwd":target,"model":"openai/gpt-5.2","thinkingLevel":"medium"})).unwrap();
    drop(file);
    let mut app = new_test_app();
    app.session_manager =
        crate::session::SessionManager::with_sessions_dir(root.path().to_str().unwrap(), &sessions);
    app.state.session_id = Some("original".into());
    app.resume_session_at_startup("target");
    assert!(app.should_quit);
    assert_eq!(app.resume_target, Some((target.clone(), "target".into())));
    assert_eq!(app.state.session_id.as_deref(), Some("original"));
    // A vanished worktree must not close the current conversation.
    app.should_quit = false;
    app.resume_target = None;
    std::fs::remove_dir(&target).unwrap();
    app.resume_session_at_startup("target");
    assert!(!app.should_quit);
    assert!(app.resume_target.is_none());
    assert_eq!(app.state.session_id.as_deref(), Some("original"));
    assert!(app.state.error.as_deref().unwrap().contains("workspace"));
}

#[tokio::test]
async fn bug_report_draft_persists_and_dismiss_suppresses_repeated_suggestions() {
    use crate::bug_report::{self, DraftStatus};
    let temp = tempfile::tempdir().unwrap();
    let mut app = new_test_app();
    app.session_manager = SessionManager::with_sessions_dir("/tmp", temp.path());
    app.handle_bug_report("draft The terminal stopped responding")
        .await
        .unwrap();
    app.handle_bug_report("expected The next turn should start")
        .await
        .unwrap();
    let saved = bug_report::load(app.session_manager.current_session_path().as_deref())
        .unwrap()
        .unwrap();
    assert_eq!(saved.description, "The terminal stopped responding");
    assert_eq!(saved.expected_behavior, "The next turn should start");
    assert_eq!(saved.status, DraftStatus::Draft);
    app.handle_bug_report("dismiss").await.unwrap();
    app.suggest_bug_report();
    assert_eq!(
        bug_report::load(app.session_manager.current_session_path().as_deref())
            .unwrap()
            .unwrap()
            .status,
        DraftStatus::Dismissed
    );
    app.handle_bug_report("draft A different problem")
        .await
        .unwrap();
    let next = bug_report::load(app.session_manager.current_session_path().as_deref())
        .unwrap()
        .unwrap();
    assert_ne!(next.id, saved.id);
}

#[tokio::test]
async fn bug_report_suggestion_never_copies_error_payload_and_does_not_replace_a_draft() {
    use crate::bug_report;
    let temp = tempfile::tempdir().unwrap();
    let mut app = new_test_app();
    app.session_manager = SessionManager::with_sessions_dir("/tmp", temp.path());
    app.ensure_session_started().unwrap();
    app.handle_agent_message(FromAgent::Error {
        message: "private provider payload".into(),
        fatal: false,
        terminal: true,
        retryable: false,
    })
    .await
    .unwrap();
    let first = bug_report::load(app.session_manager.current_session_path().as_deref())
        .unwrap()
        .unwrap();
    assert!(!first.description.contains("private provider payload"));
    app.suggest_bug_report();
    assert_eq!(
        first.id,
        bug_report::load(app.session_manager.current_session_path().as_deref())
            .unwrap()
            .unwrap()
            .id
    );
}

#[tokio::test]
async fn composer_recall_stash_swaps_complete_drafts_without_losing_attachments() {
    let mut app = new_test_app();
    let pasted = "世界 pasted\n".repeat(12);
    app.state.insert_paste(&pasted);
    app.state.textarea.set_cursor(3);
    let folded = app.state.textarea.display_text().into_owned();
    app.pending_attachments = vec!["/tmp/first.png".into()];
    app.handle_key(KeyCode::Char('s'), CrosstermModifiers::CONTROL)
        .await
        .unwrap();
    assert!(app.state.input().is_empty());
    assert!(app.pending_attachments.is_empty());
    app.state.set_input("second draft");
    app.state.textarea.set_cursor(2);
    app.pending_attachments = vec!["/tmp/second.png".into()];
    app.handle_key(KeyCode::Char('s'), CrosstermModifiers::CONTROL)
        .await
        .unwrap();
    assert_eq!(app.state.input(), pasted);
    assert_eq!(app.state.cursor(), 3);
    assert_eq!(app.state.textarea.display_text(), folded);
    assert_eq!(app.pending_attachments, ["/tmp/first.png"]);
    app.handle_key(KeyCode::Char('s'), CrosstermModifiers::CONTROL)
        .await
        .unwrap();
    assert_eq!(app.state.input(), "second draft");
    assert_eq!(app.state.cursor(), 2);
    assert_eq!(app.pending_attachments, ["/tmp/second.png"]);
}

#[tokio::test]
async fn composer_recall_search_cancel_preserves_draft_and_enter_never_submits() {
    let mut app = new_test_app();
    app.prompt_history.add("deploy staging safely");
    app.prompt_history.add("review production changes");
    app.state.set_input("original 世界 draft");
    app.state.textarea.set_cursor(9);
    app.pending_attachments = vec!["/tmp/image.png".into()];
    let messages = app.state.messages.len();
    app.handle_key(KeyCode::Char('r'), CrosstermModifiers::CONTROL)
        .await
        .unwrap();
    app.handle_paste("dss"); // Existing history fuzzy subsequence matching.
    app.handle_key(KeyCode::Esc, CrosstermModifiers::NONE)
        .await
        .unwrap();
    assert_eq!(app.state.input(), "original 世界 draft");
    assert_eq!(app.state.cursor(), 9);
    assert_eq!(app.pending_attachments, ["/tmp/image.png"]);
    app.handle_key(KeyCode::Char('r'), CrosstermModifiers::CONTROL)
        .await
        .unwrap();
    app.handle_paste("dss");
    app.handle_key(KeyCode::Enter, CrosstermModifiers::NONE)
        .await
        .unwrap();
    assert_eq!(app.state.input(), "deploy staging safely");
    assert_eq!(app.pending_attachments, ["/tmp/image.png"]);
    assert_eq!(app.state.messages.len(), messages);
    assert!(!app.state.busy);
    assert!(app.history_search.is_none());
}

#[tokio::test]
async fn composer_recall_search_selection_and_empty_results_stay_in_search() {
    let mut app = new_test_app();
    app.prompt_history.add("older prompt");
    app.prompt_history.add("newer prompt");
    app.state.set_input("draft");
    app.handle_key(KeyCode::Char('r'), CrosstermModifiers::CONTROL)
        .await
        .unwrap();
    app.handle_key(KeyCode::Down, CrosstermModifiers::NONE)
        .await
        .unwrap();
    app.handle_key(KeyCode::Enter, CrosstermModifiers::NONE)
        .await
        .unwrap();
    assert_eq!(app.state.input(), "older prompt");
    app.handle_key(KeyCode::Char('r'), CrosstermModifiers::CONTROL)
        .await
        .unwrap();
    app.handle_paste("zzzz_no_match");
    app.handle_key(KeyCode::Enter, CrosstermModifiers::NONE)
        .await
        .unwrap();
    assert!(app.history_search.is_some());
    assert_eq!(app.state.input(), "older prompt");
    app.handle_key(KeyCode::Esc, CrosstermModifiers::NONE)
        .await
        .unwrap();
    app.active_modal = ActiveModal::ShortcutsHelp;
    app.handle_key(KeyCode::Char('s'), CrosstermModifiers::CONTROL)
        .await
        .unwrap();
    assert!(app.draft_stash.is_none());
    assert_eq!(app.state.input(), "older prompt");
}

#[tokio::test]
async fn composer_recall_attachment_only_stash_restores_into_empty_composer() {
    let mut app = new_test_app();
    app.pending_attachments = vec!["/tmp/image.png".into()];
    app.handle_key(KeyCode::Char('s'), CrosstermModifiers::CONTROL)
        .await
        .unwrap();
    assert!(app.draft_stash.is_some());
    assert!(app.pending_attachments.is_empty());
    app.handle_key(KeyCode::Char('s'), CrosstermModifiers::CONTROL)
        .await
        .unwrap();
    assert!(app.draft_stash.is_none());
    assert_eq!(app.pending_attachments, ["/tmp/image.png"]);
    assert!(app.state.input().is_empty());
}

#[test]
fn correction_memory_review_save_edit_forget_survives_reload() {
    use crate::commands::HarnessAction;
    use crate::harness::{HarnessKind, HarnessScope, HarnessStore};
    use std::path::Path;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("harness.json");
    let mut app = new_test_app();
    app.harness_store = HarnessStore::with_path(&path);
    app.state.cwd = Some(root.path().display().to_string());
    let scope = HarnessStore::scope_key(HarnessScope::Workspace, root.path(), None).unwrap();
    let proposal = app
        .harness_store
        .propose(
            HarnessKind::Memory,
            HarnessScope::Workspace,
            scope,
            "local-work",
            "Run builds locally",
            "User correction in session s1 turns 3 and 7",
        )
        .unwrap();
    assert!(
        app.harness_store
            .visible_entries(root.path(), None)
            .is_empty()
    );
    app.handle_harness_action(HarnessAction::Apply(proposal));
    app.harness_store = HarnessStore::load_from_path(&path).unwrap();
    let entry = app.harness_store.visible_entries(root.path(), None)[0].clone();
    assert_eq!(
        entry.evidence.as_deref(),
        Some("User correction in session s1 turns 3 and 7")
    );
    assert!(
        app.harness_store
            .visible_entries(Path::new("/other-workspace"), None)
            .is_empty()
    );
    app.handle_harness_action(HarnessAction::Update {
        id: entry.id.clone(),
        content: "Use local builds with two jobs".into(),
        evidence: None,
    });
    app.harness_store = HarnessStore::load_from_path(&path).unwrap();
    assert_eq!(
        app.harness_store.visible_entries(root.path(), None)[0].content,
        "Use local builds with two jobs"
    );
    assert_eq!(
        app.harness_store.visible_entries(root.path(), None)[0].evidence,
        entry.evidence
    );
    app.handle_harness_action(HarnessAction::Apply("missing-proposal".into()));
    assert!(
        app.state
            .error
            .as_ref()
            .is_some_and(|error| error.contains("unknown"))
    );
    app.handle_harness_action(HarnessAction::Delete(entry.id));
    assert!(
        HarnessStore::load_from_path(&path)
            .unwrap()
            .visible_entries(root.path(), None)
            .is_empty()
    );
}

#[tokio::test]
async fn selective_summary_failed_adoption_removes_child_and_keeps_original() {
    // Other tests fork subprocesses, which can temporarily inherit a just-
    // released flock until exec. Exercise the intended failure points in an
    // isolated process without weakening their assertions or retrying saves.
    if std::env::var_os("MAESTRO_SUMMARY_ADOPTION_TEST_CHILD").is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "app::tests::selective_summary_failed_adoption_removes_child_and_keeps_original",
                "--nocapture",
            ])
            .env("MAESTRO_SUMMARY_ADOPTION_TEST_CHILD", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    use crate::agent::selective_summary::SelectiveSummaryResult;
    let temp = tempfile::tempdir().unwrap();
    let mut app = new_test_app();
    app.session_manager = SessionManager::with_sessions_dir("/tmp", temp.path());
    app.ensure_session_started().unwrap();
    app.flush_session();
    let original = app.session_manager.current_session_path().unwrap();
    let before = std::fs::read(&original).unwrap();
    for messages in [
        Vec::new(),
        vec![crate::ai::Message {
            role: crate::ai::Role::User,
            content: crate::ai::MessageContent::text("summary"),
        }],
    ] {
        let error = app
            .save_selective_summary(
                &SelectiveSummaryResult {
                    messages,
                    summary: "summary".into(),
                    first_turn: 1,
                    last_turn: 1,
                    total_turns: 1,
                },
                "stale".into(),
            )
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("empty") || error.to_string().contains("Agent stopped"),
            "{error:#}"
        );
        assert_eq!(
            app.session_manager.current_session_path().as_ref(),
            Some(&original)
        );
        assert_eq!(std::fs::read(&original).unwrap(), before);
        let transcripts = std::fs::read_dir(original.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .collect::<Vec<_>>();
        assert_eq!(transcripts, vec![original.clone()]);
    }
}

#[tokio::test]
async fn selective_summary_continue_restores_exact_provider_history() {
    assert_session_restore_provider_history(false).await;
}

#[tokio::test]
async fn interactive_resume_restores_exact_provider_history() {
    assert_session_restore_provider_history(true).await;
}

async fn assert_session_restore_provider_history(interactive: bool) {
    use crate::agent::{NativeAgent, NativeAgentConfig};
    let temp = tempfile::tempdir().unwrap();
    let mut app = new_test_app();
    app.session_manager = SessionManager::with_sessions_dir("/tmp", temp.path());
    app.current_model = "gpt-6-astra".into();
    app.current_thinking_level = ThinkingLevel::Low;
    app.ensure_session_started().unwrap();
    let (_, child_path) = app.session_manager.fork_session_snapshot().unwrap();
    let history: Vec<crate::ai::Message> = serde_json::from_value(serde_json::json!([
        {"role":"user", "content":crate::agent::compaction::render_context_summary("previous work")},
        {"role":"user", "content":"retained request"},
        {"role":"assistant", "content":[{"type":"tool_use","id":"read-1","name":"read","input":{"path":"test.txt"}}]},
        {"role":"user", "content":[{"type":"tool_result","tool_use_id":"read-1","content":"result","is_error":false}]},
        {"role":"assistant", "content":"retained answer"}
    ])).unwrap();
    crate::session::append_selective_summary_checkpoint(&child_path, &history).unwrap();
    let original = app.session_manager.current_session_path().unwrap();
    std::fs::File::open(&original)
        .unwrap()
        .set_modified(std::time::UNIX_EPOCH)
        .unwrap();
    let client = crate::ai::UnifiedClient::OpenAI(
        crate::ai::OpenAiClient::with_base_url("fixture", "http://127.0.0.1:1/v1").unwrap(),
    );
    let (agent, _events) = NativeAgent::new_with_test_client(
        NativeAgentConfig {
            model: "openai/gpt-5.5".into(),
            cwd: "/tmp".into(),
            ..Default::default()
        },
        client,
    )
    .unwrap();
    let saved = crate::session::SessionReader::read_file(&child_path).unwrap();
    app.current_model = "gpt-5.6".into();
    app.current_thinking_level = ThinkingLevel::High;
    app.native_agent = Some(agent);
    if interactive {
        let session = crate::session::SessionReader::read_file(&child_path).unwrap();
        app.resume_session_path(std::path::Path::new(&session.file_path), &session.header.id);
    } else {
        app.continue_last_session();
    }
    assert_eq!(
        app.session_manager.current_session_path().as_ref(),
        Some(&child_path)
    );
    assert_eq!(app.current_model, saved.header.model);
    assert_eq!(app.current_thinking_level, saved.header.thinking_level);
    let preview = app
        .native_agent
        .as_ref()
        .unwrap()
        .start_selective_summary_preview()
        .unwrap();
    let actual = tokio::time::timeout(Duration::from_secs(5), preview)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let expected = crate::agent::selective_summary::preview(&history).unwrap();
    assert_eq!(actual.history_digest, expected.history_digest);
    app.native_agent.take().unwrap().shutdown().await;
}

#[tokio::test]
async fn resumed_continuation_survives_the_next_compaction() {
    use crate::agent::{NativeAgent, NativeAgentConfig};
    let temp = tempfile::tempdir().unwrap();
    let _env_lock = crate::config::test_process_env_lock_async().await;
    struct RestoreEnv(Vec<(String, Option<std::ffi::OsString>)>);
    impl Drop for RestoreEnv {
        fn drop(&mut self) {
            for (name, value) in &self.0 {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }
    let names = crate::credential_mode::TEST_IDENTITY_ENV_VARS
        .iter()
        .copied()
        .chain(["MAESTRO_HOME", "OPENAI_API_KEY", "OPENAI_BASE_URL"]);
    let restore = RestoreEnv(
        names
            .map(|name| (name.to_string(), std::env::var_os(name)))
            .collect(),
    );
    for (name, _) in &restore.0 {
        std::env::remove_var(name);
    }
    let _restore = restore;
    crate::credential_mode::install_test_identity_env();
    std::env::set_var("MAESTRO_HOME", temp.path());
    std::env::set_var("OPENAI_API_KEY", "fixture");
    // Resume reauthorizes the saved model. Serve one real streaming response
    // locally so the normal post-response compaction boundary is exercised.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    std::env::set_var("OPENAI_BASE_URL", &base_url);
    let server = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        tokio::time::timeout(Duration::from_secs(20), async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 8192];
            loop {
                let count = stream.read(&mut buffer).await.unwrap();
                assert!(count > 0);
                request.extend_from_slice(&buffer[..count]);
                assert!(request.len() < 1024 * 1024);
                if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    let headers = std::str::from_utf8(&request[..end]).unwrap();
                    let length: usize = headers.lines().find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse().unwrap())
                    }).unwrap();
                    if request.len() >= end + 4 + length { break; }
                }
            }
            let chunk = serde_json::json!({
                "id": "rewind-test", "object": "chat.completion.chunk", "created": 1, "model": "gpt-4o",
                "choices": [{"index": 0, "delta": {"role": "assistant", "content": "done"}, "finish_reason": "stop"}]
            });
            let body = format!("data: {chunk}\n\ndata: [DONE]\n\n");
            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            stream.write_all(response.as_bytes()).await.unwrap();
        }).await.unwrap();
    });
    let mut app = new_test_app();
    app.session_manager = SessionManager::with_sessions_dir("/tmp", temp.path());
    app.current_model = "openai/gpt-4o".into();
    app.ensure_session_started().unwrap();
    let (_, child_path) = app.session_manager.fork_session_snapshot().unwrap();
    let mut writer = crate::session::SessionWriter::open_existing(&child_path).unwrap();
    let record = crate::agent::compaction::ContinuationRecord {
        user_requests: vec!["exact original request absent from summary".into()],
        ..Default::default()
    };
    writer
        .write_entry(
            serde_json::from_value(serde_json::json!({
                "type": "compaction", "timestamp": "2026-09-10T00:00:00Z",
                "summary": "earlier work", "firstKeptEntryIndex": 0,
                "tokensBefore": 5000, "continuation": record
            }))
            .unwrap(),
        )
        .unwrap();
    for index in 0..48 {
        writer.write_entry(serde_json::from_value(serde_json::json!({
            "type": "message", "timestamp": "2026-09-10T00:00:00Z",
            "message": {"role": "user", "content": format!("request {index}: {}", "evidence ".repeat(128))}
        })).unwrap()).unwrap();
    }
    writer.flush().unwrap();
    drop(writer);
    let saved = crate::session::SessionReader::read_file(&child_path).unwrap();

    let (agent, mut events) = NativeAgent::new_with_test_client(
        NativeAgentConfig {
            model: "openai/gpt-4o".into(),
            cwd: "/tmp".into(),
            context_window: Some(4096),
            max_tokens: 512,
            max_tokens_source: maestro_runtime::agent::MaxTokensSource::Explicit,
            ..Default::default()
        },
        crate::ai::UnifiedClient::OpenAI(
            crate::ai::OpenAiClient::with_base_url("fixture", &base_url).unwrap(),
        ),
    )
    .unwrap();
    app.native_agent = Some(agent);
    app.resume_session_path(std::path::Path::new(&saved.file_path), &saved.header.id);
    app.native_agent
        .as_ref()
        .unwrap()
        .prompt("continue".into(), vec![])
        .await
        .unwrap();
    let continuation = tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(event) = events.recv().await {
            match event {
                FromAgent::Compaction {
                    continuation: Some(record),
                    ..
                } => return record,
                FromAgent::TurnCompleted { .. } => panic!("large restored history did not compact"),
                FromAgent::Error { message, .. } | FromAgent::ProviderError { message, .. } => {
                    panic!("{message}")
                }
                _ => {}
            }
        }
        panic!("agent closed before compaction");
    })
    .await
    .unwrap();
    app.native_agent.take().unwrap().shutdown().await;
    server.await.unwrap();
    assert!(
        continuation
            .user_requests
            .iter()
            .any(|request| request == "exact original request absent from summary")
    );
}

#[tokio::test]
async fn feedback_model_drafts_queue_hide_and_edit_without_sending() {
    use crate::bug_report::{self, DraftStatus};
    let temp = tempfile::tempdir().unwrap();
    let mut app = new_test_app();
    app.session_manager = SessionManager::with_sessions_dir("/tmp", temp.path().join("sessions"));
    app.ensure_session_started().unwrap();
    let tool = bug_report::draft_tool(
        serde_json::json!({"description":"Ignored correction", "expected_behavior":"Follow correction", "reproduction_steps":"Correct and retry"}),
    );
    app.accept_feedback_tool("draft_feedback", &tool);
    app.handle_bug_report("hide").await.unwrap();
    let path = app.session_manager.current_session_path().unwrap();
    let draft = bug_report::load(Some(&path)).unwrap().unwrap();
    assert!(draft.hidden);
    assert_eq!(draft.status, DraftStatus::Draft);
    app.handle_bug_report("queue").await.unwrap();
    assert_eq!(app.active_modal, ActiveModal::Feedback);
    app.handle_feedback_key(KeyCode::Enter).await.unwrap();
    app.handle_feedback_key(KeyCode::Char('r')).await.unwrap();
    app.handle_paste(" then observe the second failure");
    app.handle_feedback_key(KeyCode::Enter).await.unwrap();
    let saved = bug_report::load(Some(&path)).unwrap().unwrap();
    assert!(saved.context.reproduction_steps.ends_with("second failure"));
    assert!(!matches!(
        saved.status,
        DraftStatus::Sending | DraftStatus::Sent { .. }
    ));
    app.handle_bug_report("new Another failure").await.unwrap();
    assert_eq!(bug_report::load_all(&path).unwrap().len(), 2);
}

#[test]
fn error_banner_uses_shared_theme() {
    use ratatui::widgets::Widget;
    for theme in crate::components::theme_test::palettes() {
        let area = ratatui::layout::Rect::new(0, 0, 12, 3);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        super::error_banner("Connection failed. Try again.", theme).render(area, &mut buffer);
        crate::components::theme_test::assert_palette(&buffer, theme);
        assert_eq!(buffer[(0, 0)].fg, theme.error);
        assert_eq!(buffer[(0, 1)].fg, theme.error);
    }
}

#[test]
fn opening_control_panels_preserves_approval_and_preferences() {
    use crate::commands::ControlPanel;
    let mut app = new_test_app();
    app.state.approval_mode = ApprovalMode::Safe;
    let footer = app.footer_style;
    let output = app.state.output_detail();
    for panel in [
        ControlPanel::Settings,
        ControlPanel::Permissions,
        ControlPanel::Footer,
        ControlPanel::Output,
        ControlPanel::Model,
        ControlPanel::Effort,
        ControlPanel::Tasks,
        ControlPanel::Context,
        ControlPanel::Review,
        ControlPanel::Help,
    ] {
        app.show_control_panel(panel);
        assert_eq!(app.state.approval_mode, ApprovalMode::Safe);
        assert_eq!(app.footer_style, footer);
        assert_eq!(app.state.output_detail(), output);
        assert_eq!(app.active_modal, ActiveModal::CommandPalette);
    }
}

#[tokio::test]
async fn control_panel_enter_keeps_child_model_picker_open() {
    let mut app = new_test_app();
    app.state.set_input("draft é stays");
    app.state.move_left();
    let cursor = app.state.cursor();
    app.show_command_palette();
    app.command_palette.insert_str(">model");
    app.handle_command_palette_key(crossterm::event::KeyCode::Enter, false)
        .await
        .unwrap();
    assert_eq!(app.active_modal, ActiveModal::CommandPalette);
    assert_eq!(app.state.input(), "draft é stays");
    assert_eq!(app.state.cursor(), cursor);
    assert_eq!(
        app.command_palette.selected_resource().unwrap().id,
        "model select"
    );
    app.handle_command_palette_key(crossterm::event::KeyCode::Enter, false)
        .await
        .unwrap();
    assert_eq!(app.active_modal, ActiveModal::ModelSelector);
    assert_eq!(app.state.input(), "draft é stays");
    assert_eq!(app.state.cursor(), cursor);
    app.handle_model_selector_key(crossterm::event::KeyCode::Esc, false)
        .await
        .unwrap();
    assert_eq!(app.state.input(), "draft é stays");
    assert_eq!(app.state.cursor(), cursor);
}

#[tokio::test]
async fn permissions_escape_preserves_safe_policy() {
    let mut app = new_test_app();
    app.state.approval_mode = ApprovalMode::Safe;
    app.show_control_panel(crate::commands::ControlPanel::Permissions);
    app.handle_command_palette_key(crossterm::event::KeyCode::Esc, false)
        .await
        .unwrap();
    assert_eq!(app.state.approval_mode, ApprovalMode::Safe);
    assert_eq!(app.active_modal, ActiveModal::None);
}

#[test]
fn tasks_schedule_action_resolves_to_read_only_status() {
    let mut app = new_test_app();
    app.show_control_panel(crate::commands::ControlPanel::Tasks);
    for _ in 0..20 {
        let resource = app.command_palette.selected_resource().unwrap();
        if resource.label == "Schedules" {
            let output = app
                .command_registry
                .execute(&format!("/{}", resource.id), "/tmp", None, None)
                .unwrap();
            assert!(matches!(
                output,
                CommandOutput::Action(CommandAction::Loop(crate::commands::LoopAction::Status))
            ));
            return;
        }
        app.command_palette.move_down();
    }
    panic!("Tasks must expose schedule status");
}

#[tokio::test]
async fn categorical_feedback_command_explains_choices_without_opening_raw_draft() {
    let mut app = new_test_app();
    let before = app.state.messages.len();
    app.handle_command_action(CommandAction::BugReport("rating".into()))
        .await;
    assert_eq!(app.active_modal, ActiveModal::None);
    assert!(app.session_manager.current_session_path().is_none());
    assert_eq!(app.state.messages.len(), before + 1);
    let message = &app.state.messages.last().unwrap().content;
    assert!(message.contains("/feedback rating useful"));
    assert!(message.contains("Only this category is sent"));
    assert!(message.contains("seven days"));
}

#[tokio::test]
async fn categorical_feedback_unknown_answer_never_becomes_a_free_text_report() {
    let mut app = new_test_app();
    let private_text = "rating private customer task contents";
    app.handle_command_action(CommandAction::BugReport(private_text.into()))
        .await;
    assert_eq!(app.active_modal, ActiveModal::None);
    assert!(app.session_manager.current_session_path().is_none());
    assert!(
        !app.state
            .messages
            .iter()
            .any(|message| message.content.contains("private customer task"))
    );
    assert!(
        app.state
            .messages
            .last()
            .unwrap()
            .content
            .contains("Only this category is sent")
    );
}

#[tokio::test]
async fn categorical_feedback_results_are_polled_without_waiting_on_collection() {
    for (result, expected) in [
        (
            visibility::FeedbackSubmission::Queued,
            "queued for the product team",
        ),
        (
            visibility::FeedbackSubmission::Disabled,
            "telemetry is turned off",
        ),
        (
            visibility::FeedbackSubmission::Unavailable,
            "workspace changed",
        ),
        (
            visibility::FeedbackSubmission::Failed,
            "could not be queued",
        ),
    ] {
        let mut app = new_test_app();
        let (tx, rx) = tokio::sync::oneshot::channel();
        app.feedback_rating_rx = Some(rx);
        let count = app.state.messages.len();
        app.poll_feedback_rating();
        assert_eq!(app.state.messages.len(), count);
        assert!(app.feedback_rating_rx.is_some());
        // An unresolved submission leaves command handling available.
        app.handle_command_action(CommandAction::BugReport("rating".into()))
            .await;
        assert!(
            app.state
                .messages
                .last()
                .unwrap()
                .content
                .contains("Only this category is sent")
        );
        tx.send(result).unwrap();
        app.poll_feedback_rating();
        assert!(app.feedback_rating_rx.is_none());
        assert!(
            app.state
                .messages
                .last()
                .unwrap()
                .content
                .contains(expected)
        );
        assert_eq!(app.active_modal, ActiveModal::None);
    }
}

#[tokio::test]
async fn successful_model_retry_clears_late_error_from_earlier_request() {
    let mut app = new_test_app();
    let failed_model = "openai/failed-model".to_owned();
    let retried_model = "openai/retried-model".to_owned();
    app.pending_model_change = Some(PendingModelChange {
        model: retried_model.clone(),
    });

    // The first request can report both events after the retry is already
    // pending. Its error must remain visible until the retry succeeds.
    app.handle_agent_message(FromAgent::Error {
        message: "first model failed".to_owned(),
        fatal: false,
        terminal: false,
        retryable: false,
    })
    .await
    .expect("handle failed model error");
    app.handle_agent_message(FromAgent::ModelChangeFailed {
        model: failed_model,
        reason: "first model failed".to_owned(),
    })
    .await
    .expect("handle failed model change");
    assert_eq!(
        app.pending_model_change
            .as_ref()
            .map(|pending| pending.model.as_str()),
        Some("openai/retried-model")
    );
    assert_eq!(app.state.error.as_deref(), Some("first model failed"));

    app.handle_agent_message(FromAgent::ModelChanged {
        model: retried_model.clone(),
        provider: "openai".to_owned(),
    })
    .await
    .expect("handle successful retry");

    assert_eq!(app.current_model, retried_model);
    assert_eq!(app.state.model.as_deref(), Some("openai/retried-model"));
    assert_eq!(app.state.provider.as_deref(), Some("openai"));
    assert!(app.pending_model_change.is_none());
    assert!(app.state.error.is_none());
}

#[tokio::test]
async fn configured_cycle_binding_respects_busy_and_pending_state() {
    let mut app = new_test_app();
    app.current_model = "gpt-5.6".into();
    app.cycle_model_binding = crate::key_hints::alt(KeyCode::Char('m'));
    app.state.busy = true;
    app.handle_key(KeyCode::Char('m'), CrosstermModifiers::ALT)
        .await
        .unwrap();
    assert_eq!(app.current_model, "gpt-5.6");
    assert!(app.state.status.as_deref().unwrap().contains("Wait for"));
    app.state.busy = false;
    app.pending_model_change = Some(PendingModelChange {
        model: "claude-fable-5-1".into(),
    });
    app.cycle_model(true);
    assert_eq!(
        app.pending_model_change.as_ref().unwrap().model,
        "claude-fable-5-1"
    );
    assert_eq!(app.current_model, "gpt-5.6");
}

#[tokio::test]
async fn local_response_content_persists_tool_calls_and_signed_thinking_before_dispatch() {
    let mut app = new_test_app();
    let dir = tempfile::tempdir().unwrap();
    app.session_manager = SessionManager::with_sessions_dir("handoff-test", dir.path());
    app.current_model = "claude-fable-5-1".into();
    app.record_user_message("Read both files");
    let response_id = "tool-response".to_string();
    app.handle_agent_message(FromAgent::ResponseStart {
        response_id: response_id.clone(),
    })
    .await
    .unwrap();
    let blocks = vec![
        crate::ai::ContentBlock::Thinking {
            thinking: String::new(),
            signature: Some("empty-signature".into()),
        },
        crate::ai::ContentBlock::Thinking {
            thinking: "reasoning".into(),
            signature: Some("visible-signature".into()),
        },
        crate::ai::ContentBlock::ToolUse {
            id: "call|fc_a".into(),
            name: "read".into(),
            input: serde_json::json!({"path":"a"}),
            gemini_context: None,
        },
        crate::ai::ContentBlock::ToolUse {
            id: "call|fc_b".into(),
            name: "read".into(),
            input: serde_json::json!({"path":"b"}),
            gemini_context: None,
        },
    ];
    let event = FromAgent::LocalAssistantContent {
        response_id: response_id.clone(),
        content: blocks.clone(),
    };
    assert!(
        serde_json::to_value(&event).is_err(),
        "private content cannot enter a serialized transport"
    );
    app.handle_agent_message(event).await.unwrap();
    app.handle_agent_message(FromAgent::ResponseEnd {
        response_id,
        usage: None,
    })
    .await
    .unwrap();
    let path = app.session_manager.current_session_path().unwrap();
    let session = crate::session::SessionReader::read_file(path).unwrap();
    let history = crate::session::model_history(&session);
    assert_eq!(
        serde_json::to_value(&history[1].content).unwrap(),
        serde_json::to_value(crate::ai::MessageContent::Blocks(blocks)).unwrap()
    );
    assert!(app.pending_assistant_content.is_none());
}

#[test]
fn shifted_cycle_fallback_accepts_terminal_uppercase_key_events() {
    let app = new_test_app();
    for modifier in [CrosstermModifiers::CONTROL, CrosstermModifiers::ALT] {
        let modifiers = modifier | CrosstermModifiers::SHIFT;
        let binding = crate::key_hints::KeyBinding {
            key: KeyCode::Char('p'),
            modifiers,
        };
        assert!(app.matches_binding(binding, KeyCode::Char('P'), modifiers));
        assert!(app.matches_binding(binding, KeyCode::Char('p'), modifiers));
        assert!(!app.matches_binding(binding, KeyCode::Char('P'), modifier));
        assert!(!app.matches_binding(binding, KeyCode::Char('Q'), modifiers));
    }
}

#[tokio::test]
async fn configuration_visibility_uses_live_oauth_scope_and_rejects_stale_origins() {
    let _guard = crate::config::test_process_env_lock_async().await;
    struct Restore(Vec<(&'static str, Option<std::ffi::OsString>)>);
    impl Drop for Restore {
        fn drop(&mut self) {
            for (name, value) in &self.0 {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }
    let names = [
        "MAESTRO_HOME",
        "MAESTRO_OAUTH_STORAGE_MODE",
        "MAESTRO_HOSTED_RUNNER_MODE",
        "MAESTRO_EVALOPS_ACCESS_TOKEN",
        "MAESTRO_EVALOPS_ACCESS_TOKEN_FILE",
        "MAESTRO_EVALOPS_ORG_ID",
        "MAESTRO_EVALOPS_WORKSPACE_ID",
        "MAESTRO_IDENTITY_URL",
        crate::init_cli::TEST_IDENTITY_AUTHORITY_ENV,
        "MAESTRO_MANAGED_SETUP_URL",
        "MAESTRO_INTERNAL_TELEMETRY_DISABLED",
        "EVALOPS_INTERNAL_TELEMETRY_DISABLED",
        "MAESTRO_TELEMETRY",
    ];
    let _restore = Restore(
        names
            .iter()
            .map(|name| (*name, std::env::var_os(name)))
            .collect(),
    );
    for name in names {
        std::env::remove_var(name);
    }
    let home = tempdir().unwrap();
    std::env::set_var("MAESTRO_HOME", home.path());
    std::env::set_var("MAESTRO_OAUTH_STORAGE_MODE", "file");
    std::env::set_var("MAESTRO_TELEMETRY", "1");
    std::env::set_var(
        "MAESTRO_IDENTITY_URL",
        crate::credential_mode::test_identity_base_url(),
    );
    std::env::set_var(crate::init_cli::TEST_IDENTITY_AUTHORITY_ENV, "1");
    // A real loopback response rejects the managed lookup; no external service
    // or existing user's configuration participates in this boundary test.
    std::env::set_var(
        "MAESTRO_MANAGED_SETUP_URL",
        crate::credential_mode::test_identity_base_url(),
    );
    let oauth = serde_json::to_vec(&serde_json::json!({"evalops": {
        "type": "oauth", "access": "fixture-token", "refresh": "fixture-refresh",
        "expires": chrono::Utc::now().timestamp_millis() + 3_600_000,
        "metadata": {"organizationId": "mutable-local-org", "userId": "user-test",
            "identityBaseUrl": crate::credential_mode::test_identity_base_url()}
    }}))
    .unwrap();
    std::fs::write(home.path().join("oauth.json"), &oauth).unwrap();
    let snapshot = crate::init_cli::load_evalops_snapshot().unwrap().unwrap();
    assert!(snapshot.agent_mcp.is_none());
    let mut app = new_test_app_with_platform_resolution(PlatformSessionResolution::Detect);
    let origin = crate::telemetry::TelemetryIdentityScope::new("org-test", Some("workspace-test"));
    assert_eq!(app.managed_setup_identity_scope, origin);
    assert_eq!(app.managed_setup.setup().organization_id, "org-test");
    assert_eq!(app.managed_setup.setup().workspace_id, "workspace-test");
    assert_eq!(
        std::fs::read(home.path().join("oauth.json")).unwrap(),
        oauth
    );
    let event = crate::telemetry::VisibilityEvent::configuration(
        crate::telemetry::ConfigurationReceipt::from_managed_setup(
            &app.managed_setup,
            chrono::Utc::now().timestamp(),
        ),
    );
    let session = crate::credential_mode::current_verified_identity_session().unwrap();
    // Background runtime telemetry uses MAESTRO_HOME. Keep this recorder's
    // exact-count assertions on its own outbox when the suite runs in parallel.
    let isolated_outbox = tempdir().unwrap();
    let outbox = isolated_outbox.path().join("outbox");
    assert_eq!(
        crate::telemetry::test_record_visibility_with_session(
            &event,
            origin.clone(),
            session.clone(),
            &outbox,
        ),
        crate::telemetry::OnboardingCollectionStatus::Queued
    );
    let queued_paths = std::fs::read_dir(&outbox)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect::<Vec<_>>();
    assert_eq!(queued_paths.len(), 1);
    let persisted = std::fs::read_to_string(&queued_paths[0]).unwrap();
    assert!(persisted.contains("org-test"));
    assert!(persisted.contains("workspace-test"));
    assert!(!persisted.contains("fixture-token"));
    let wrong_origin =
        crate::telemetry::TelemetryIdentityScope::new("other-org", Some("other-workspace"));
    assert_eq!(
        crate::telemetry::test_record_visibility_with_session(
            &event,
            wrong_origin,
            session,
            &outbox
        ),
        crate::telemetry::OnboardingCollectionStatus::Unavailable
    );

    std::env::set_var("MAESTRO_EVALOPS_ORG_ID", "new-local-org");
    for token in ["inactive-token", "unscoped-token"] {
        std::env::set_var("MAESTRO_EVALOPS_ACCESS_TOKEN", token);
        app.managed_setup_identity_scope = origin.clone();
        assert!(app.refresh_managed_setup_after_identity_login().is_err());
        assert!(app.managed_setup_identity_scope.is_none());
        let denied = new_test_app_with_platform_resolution(PlatformSessionResolution::Detect);
        assert!(denied.managed_setup_identity_scope.is_none());
        assert_eq!(
            *denied.managed_setup.mcp_policy(),
            crate::managed_setup::McpPolicy::deny_all()
        );
    }
    std::env::set_var("MAESTRO_EVALOPS_ACCESS_TOKEN", "missing-workspace-token");
    let incomplete = new_test_app_with_platform_resolution(PlatformSessionResolution::Detect);
    assert!(incomplete.managed_setup_identity_scope.is_none());
    let session = crate::credential_mode::current_verified_identity_session().unwrap();
    assert_eq!(
        crate::telemetry::test_record_visibility_with_session(&event, origin, session, &outbox),
        crate::telemetry::OnboardingCollectionStatus::Unavailable
    );
    assert_eq!(
        std::fs::read_dir(outbox)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path
                .extension()
                .is_some_and(|extension| extension == "json"))
            .count(),
        1
    );
}

#[test]
fn display_language_saves_reloads_and_does_not_change_model_or_transcript() {
    if crate::config::test_reexec_for_process_isolation() {
        return;
    }
    let temp = tempdir().unwrap();
    std::env::set_var("MAESTRO_HOME", temp.path());
    let mut app = new_test_app();
    app.state
        .add_user_message("Keep this prompt unchanged".into());
    let model = app.current_model.clone();
    let messages = app.state.messages.len();
    app.apply_language(crate::localization::Locale::Japanese);
    assert_eq!(app.state.locale, crate::localization::Locale::Japanese);
    assert_eq!(app.current_model, model);
    assert_eq!(app.state.messages.len(), messages);
    assert_eq!(
        app.state.messages.last().unwrap().content,
        "Keep this prompt unchanged"
    );
    assert_eq!(
        new_test_app().state.locale,
        crate::localization::Locale::Japanese
    );
    let ui_path = temp.path().join("ui.json");
    std::fs::remove_file(&ui_path).unwrap();
    std::fs::create_dir(&ui_path).unwrap();
    app.apply_language(crate::localization::Locale::Spanish);
    assert_eq!(app.state.locale, crate::localization::Locale::Japanese);
    assert!(app.state.error.is_some());
}

#[test]
fn catalog_typing_does_not_trigger_manager_actions() {
    if crate::config::test_reexec_for_process_isolation() {
        return;
    }
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let temp = tempdir().unwrap();
        std::env::set_var("MAESTRO_HOME", temp.path());
        let mut app = new_test_app();
        app.active_modal = ActiveModal::McpManager;
        app.mcp_manager.enter_catalog();
        for ch in "linear".chars() {
            app.handle_mcp_manager_key(KeyCode::Char(ch)).await.unwrap();
        }
        assert_eq!(app.active_modal, ActiveModal::McpManager);
        assert!(app.mcp_manager.in_catalog());
        assert_eq!(app.mcp_manager.selected_catalog().unwrap().id, "linear");
        assert!(!temp.path().join("mcp.json").exists());
    });
}

#[tokio::test]
async fn settings_experiments_opens_the_native_control_and_escape_closes_it() {
    let mut app = new_test_app();
    app.show_control_panel(crate::commands::ControlPanel::Settings);
    assert_eq!(
        app.command_palette.selected_resource().unwrap().id,
        "preferences"
    );
    app.handle_command_palette_key(crossterm::event::KeyCode::Enter, false)
        .await
        .unwrap();
    assert_eq!(app.active_modal, ActiveModal::Preferences);
    assert!(app.config_selector.is_visible());
    app.handle_key(
        crossterm::event::KeyCode::Esc,
        crossterm::event::KeyModifiers::NONE,
    )
    .await
    .unwrap();
    assert_eq!(app.active_modal, ActiveModal::None);
    assert!(!app.config_selector.is_visible());
}
