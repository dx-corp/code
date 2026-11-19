use super::{
    Command, CommandAction, CommandCategory, CommandError, CommandOutput, CommandRegistry,
    ControlPanel,
};

pub(super) fn register_output_commands(registry: &mut CommandRegistry) {
    registry.register(
        Command::new(
            "output",
            maestro_ui::localization::tr("Choose summary, compact, or expanded tool output"),
            CommandCategory::Ui,
            Box::new(|ctx| {
                if ctx.raw_args.trim().is_empty() {
                    return Ok(CommandOutput::Action(CommandAction::OpenPanel(
                        ControlPanel::Output,
                    )));
                }
                let detail =
                    crate::state::OutputDetail::parse(ctx.raw_args.trim()).ok_or_else(|| {
                        CommandError::new(maestro_ui::localization::tr(
                            "Usage: /output [summary|compact|expanded]",
                        ))
                    })?;
                Ok(CommandOutput::Action(CommandAction::SetOutputDetail(
                    detail,
                )))
            }),
        )
        .localized()
        .usage("/output [summary|compact|expanded]"),
    );

    registry.register(
        Command::new(
            "autochart",
            maestro_ui::localization::tr("Show charts for time-series tool results"),
            CommandCategory::Ui,
            Box::new(|ctx| {
                let enabled = match ctx.raw_args.trim() {
                    "on" => true,
                    "off" => false,
                    _ => {
                        return Err(CommandError::new(maestro_ui::localization::tr(
                            "Usage: /autochart <on|off>",
                        )));
                    }
                };
                Ok(CommandOutput::Action(CommandAction::SetAutoCharts(enabled)))
            }),
        )
        .localized()
        .usage("/autochart <on|off>"),
    );
}
