use super::*;

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
