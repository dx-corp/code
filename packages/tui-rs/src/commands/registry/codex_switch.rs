use super::*;

pub(super) fn register(registry: &mut CommandRegistry) {
    registry.register(
        Command::new(
            "codex",
            maestro_ui::localization::tr("Use ChatGPT subscription"),
            CommandCategory::Config,
            Box::new(|_| Ok(CommandOutput::Action(CommandAction::SwitchToCodex))),
        )
        .localized()
        .usage("/codex"),
    );
    for (name, label, route) in [
        ("claude", "Use Claude subscription", "claude-code/sonnet"),
        (
            "copilot",
            "Use GitHub Copilot subscription",
            "github-copilot/auto",
        ),
    ] {
        registry.register(
            Command::new(
                name,
                maestro_ui::localization::tr(label),
                CommandCategory::Config,
                Box::new(move |_| {
                    Ok(CommandOutput::Action(CommandAction::SetModel(
                        route.to_owned(),
                    )))
                }),
            )
            .localized()
            .usage(format!("/{name}")),
        );
    }
}
