use super::*;

pub(super) fn register(registry: &mut CommandRegistry) {
    // Persisted branch navigation and fork creation.
    registry.register(
        Command::new(
            "tree",
            maestro_ui::localization::tr("Browse and resume session branches"),
            CommandCategory::Session,
            Box::new(|_| {
                Ok(CommandOutput::Action(CommandAction::Session(
                    SessionAction::BrowseBranches,
                )))
            }),
        )
        .localized(),
    );

    registry.register(
        Command::new(
            "fork",
            maestro_ui::localization::tr("Fork the conversation into a new session branch"),
            CommandCategory::Session,
            Box::new(|_| {
                Ok(CommandOutput::Action(CommandAction::Session(
                    SessionAction::Fork,
                )))
            }),
        )
        .localized()
        .usage("/fork")
        .primary(2),
    );
}
