use super::*;
#[test]
fn codex_shortcut_uses_live_subscription_switch_action() {
    let registry = build_command_registry();
    assert!(matches!(
        registry.execute("/codex", "/tmp", None, None).unwrap(),
        CommandOutput::Action(CommandAction::SwitchToCodex)
    ));
}
#[test]
fn subscription_shortcuts_select_provider_owned_routes() {
    let registry = build_command_registry();
    for (command, route) in [
        ("/claude", "claude-code/sonnet"),
        ("/copilot", "github-copilot/auto"),
    ] {
        assert!(matches!(
            registry.execute(command, "/tmp", None, None).unwrap(),
            CommandOutput::Action(CommandAction::SetModel(model)) if model == route
        ));
    }
}
#[test]
fn primary_menu_and_compatibility_commands_share_one_registry() {
    let registry = build_command_registry();
    let names: Vec<_> = registry
        .primary_commands()
        .iter()
        .map(|c| c.name.clone())
        .collect();
    assert_eq!(
        names,
        [
            "new", "resume", "fork", "rewind", "model", "review", "tasks", "settings", "context",
            "help", "quit"
        ]
    );
    assert_eq!(registry.get("clear").unwrap().name, "new");
    for name in [
        "always-approve",
        "auto",
        "ask",
        "prompt-audit",
        "mcp-config",
        "magic-trace",
        "stats",
        "toolhistory",
        "limits",
        "harness",
        "rlm",
    ] {
        assert!(registry.get(name).unwrap().browse_order.is_none(), "{name}");
    }
}
#[test]
fn bare_settings_open_without_mutating() {
    let registry = build_command_registry();
    for input in [
        "/approvals",
        "/footer",
        "/thinking",
        "/model",
        "/settings",
        "/context",
    ] {
        assert!(
            matches!(
                registry.execute(input, "/tmp", None, None).unwrap(),
                CommandOutput::Action(CommandAction::OpenPanel(_))
            ),
            "{input}"
        );
    }
    assert!(
        matches!(registry.execute("/approvals safe", "/tmp", None, None).unwrap(), CommandOutput::Action(CommandAction::SetApprovalMode(mode)) if mode == "safe")
    );
    assert!(matches!(
        registry
            .execute("/footer solo", "/tmp", None, None)
            .unwrap(),
        CommandOutput::Action(CommandAction::SetFooterStyle(FooterStyle::Solo))
    ));
    assert!(
        registry
            .execute("/settings typo", "/tmp", None, None)
            .is_err()
    );
    assert!(
        registry
            .execute("/output typo", "/tmp", None, None)
            .is_err()
    );
}
