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
}
