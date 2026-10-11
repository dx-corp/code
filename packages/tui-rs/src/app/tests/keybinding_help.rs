use super::*;

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
"toggle-tool-outputs": "Shift+Left",
"edit-last-follow-up": "Ctrl+T"
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
