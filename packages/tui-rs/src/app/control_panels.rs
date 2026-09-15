//! Navigation over existing settings and runtime owners; opening a view has no effects.
use super::*;
use crate::commands::ControlPanel;
use crate::palette_resource::{PaletteResource, PaletteResourceKind};
use crate::state::OutputDetail;

impl App {
    pub(super) fn apply_language(&mut self, locale: crate::localization::Locale) {
        let mut prefs = crate::ui_prefs::UiPrefs::load_default();
        prefs.display_language = Some(locale.code().into());
        if let Err(error) = prefs.save_default() {
            self.state.error = Some(format!(
                "{}: {error}",
                self.state
                    .locale
                    .text(crate::localization::TextKey::SaveFailed)
            ));
            return;
        }
        self.state.locale = locale;
        self.mcp_manager.locale = locale;
        self.ui_prefs = prefs;
        self.state.error = None;
        self.show_control_panel(ControlPanel::Language);
    }

    pub(super) fn apply_output_detail(&mut self, detail: OutputDetail) {
        let mut prefs = crate::ui_prefs::UiPrefs::load_default();
        prefs.set_output_detail(detail);
        if let Err(error) = prefs.save_default() {
            self.state.error = Some(
                self.state
                    .locale
                    .format("Could not save output detail: {0}", &[(error).to_string()]),
            );
            return;
        }
        self.state.set_output_detail(detail);
        self.ui_prefs = prefs;
        self.state.error = None;
        self.state.status = Some(self.state.locale.format(
            "Output detail: {0} (saved)",
            &[(detail.as_str()).to_string()],
        ));
    }

    pub(super) fn show_control_panel(&mut self, panel: ControlPanel) {
        let mut rows = Vec::new();
        let mut add = |command: &str, label: &str, description: String| {
            rows.push(
                PaletteResource::new(PaletteResourceKind::Command, command, label)
                    .description(description),
            );
        };
        let locale = self.state.locale;
        use crate::localization::TextKey;
        let title = match panel {
            ControlPanel::Language => {
                for choice in crate::localization::Locale::ALL {
                    add(
                        &format!("language {}", choice.code()),
                        choice.name(),
                        format!(
                            "{}{}",
                            choice.code(),
                            if choice == locale { " ✓" } else { "" }
                        ),
                    );
                }
                locale.text(TextKey::Language).to_string()
            }

            ControlPanel::Settings => {
                add(
                    "preferences",
                    "Experiments",
                    "Review and change participation in randomized tool experiments".into(),
                );
                add(
                    "settings account",
                    self.state.locale.translate("Account and inference"),
                    self.state
                        .locale
                        .translate("Sign-in, workspace, model, usage")
                        .into(),
                );
                add(
                    "settings permissions",
                    self.state.locale.translate("Permissions"),
                    self.state
                        .locale
                        .translate(self.state.approval_mode.label())
                        .into(),
                );
                add(
                    "settings capabilities",
                    self.state.locale.translate("Connections and capabilities"),
                    self.state
                        .locale
                        .translate("Tools, skills, plugins, hooks")
                        .into(),
                );
                add(
                    "settings appearance",
                    locale.text(TextKey::Appearance),
                    self.state.locale.format(
                        "Output: {0}",
                        &[self
                            .state
                            .locale
                            .translate(self.state.output_detail().as_str())
                            .to_string()],
                    ),
                );
                add(
                    "settings advanced",
                    self.state.locale.translate("Advanced"),
                    self.state
                        .locale
                        .translate("Diagnostics and runtime limits")
                        .into(),
                );
                locale.text(TextKey::Settings).into()
            }
            ControlPanel::Account => {
                add(
                    "setup",
                    self.state.locale.translate("Sign-in and inference"),
                    self.state
                        .locale
                        .translate("Manage account and provider setup")
                        .into(),
                );
                add(
                    "model",
                    self.state.locale.translate("Model and effort"),
                    self.current_model.clone(),
                );
                add(
                    "cost",
                    self.state.locale.translate("Usage"),
                    self.state
                        .locale
                        .translate("Recorded tokens and cost")
                        .into(),
                );
                add(
                    "status",
                    self.state.locale.translate("Connection status"),
                    self.state
                        .locale
                        .translate("Effective session configuration")
                        .into(),
                );
                self.state.locale.translate("Account and inference").into()
            }
            ControlPanel::Permissions => {
                add(
                    "approvals safe",
                    self.state.locale.translate("Apply: ask for every tool"),
                    self.state
                        .locale
                        .translate("Session approval policy")
                        .into(),
                );
                add(
                    "approvals selective",
                    self.state.locale.translate("Apply: ask for risky tools"),
                    self.state
                        .locale
                        .translate("Session approval policy")
                        .into(),
                );
                add(
                    "approvals yolo",
                    self.state.locale.translate("Apply: auto-approve tools"),
                    self.state
                        .locale
                        .translate("Tools run without approval prompts")
                        .into(),
                );
                add(
                    "sandbox",
                    self.state.locale.translate("Sandbox policy"),
                    self.state
                        .locale
                        .translate("Inspect effective execution restrictions")
                        .into(),
                );
                add(
                    "trust status",
                    self.state.locale.translate("Workspace trust"),
                    self.state
                        .locale
                        .translate("Inspect the workspace and admitted extensions")
                        .into(),
                );
                add(
                    "trust grant",
                    self.state.locale.translate("Grant workspace trust"),
                    self.state
                        .locale
                        .translate("Allow this workspace's skills, plugins, and hooks")
                        .into(),
                );
                add(
                    "trust revoke",
                    self.state.locale.translate("Revoke workspace trust"),
                    self.state
                        .locale
                        .translate("Stop admitting workspace extensions")
                        .into(),
                );
                self.state.locale.format(
                    "Permissions · {0}",
                    &[self
                        .state
                        .locale
                        .translate(self.state.approval_mode.label())
                        .to_string()],
                )
            }
            ControlPanel::Appearance => {
                add(
                    "language",
                    locale.text(TextKey::Language),
                    locale.name().into(),
                );
                add(
                    "settings output",
                    self.state.locale.translate("Output detail"),
                    self.state.locale.format(
                        "{0} · saved preference",
                        &[self
                            .state
                            .locale
                            .translate(self.state.output_detail().as_str())
                            .to_string()],
                    ),
                );
                add(
                    "theme",
                    self.state.locale.translate("Theme"),
                    self.state
                        .locale
                        .translate("Preview and select a color theme")
                        .into(),
                );
                add(
                    "zen",
                    self.state.locale.translate("Toggle minimal layout"),
                    self.state.locale.format(
                        "{0} · this session",
                        &[(if self.state.zen_mode { "On" } else { "Off" }).to_string()],
                    ),
                );
                add(
                    "settings footer",
                    self.state.locale.translate("Status bar"),
                    self.state.locale.format(
                        "{0} · saved preference",
                        &[(self.footer_style.as_str()).to_string()],
                    ),
                );
                add(
                    "dex appearance",
                    self.state.locale.translate("Dex appearance"),
                    self.state
                        .locale
                        .translate("Avatar, accent, and reactions")
                        .into(),
                );
                add(
                    "hotkeys",
                    self.state.locale.translate("Keyboard shortcuts"),
                    self.state
                        .locale
                        .translate("Effective bindings and customization")
                        .into(),
                );
                self.state
                    .locale
                    .translate("Appearance and keyboard")
                    .into()
            }
            ControlPanel::Output => {
                for detail in [
                    OutputDetail::Summary,
                    OutputDetail::Compact,
                    OutputDetail::Expanded,
                ] {
                    let command = format!("output {}", detail.as_str());
                    let label = self
                        .state
                        .locale
                        .format("Apply: {0}", &[(detail.as_str()).to_string()]);
                    add(
                        &command,
                        &label,
                        match detail {
                            OutputDetail::Summary => self
                                .state
                                .locale
                                .translate("Summarize tool turns; expand individually"),
                            OutputDetail::Compact => {
                                self.state.locale.translate("Fold individual tool output")
                            }
                            OutputDetail::Expanded => self
                                .state
                                .locale
                                .translate("Show tool output; leave summary mode"),
                        }
                        .into(),
                    );
                }
                self.state.locale.format(
                    "Output · {0} · saved preference",
                    &[self
                        .state
                        .locale
                        .translate(self.state.output_detail().as_str())
                        .to_string()],
                )
            }
            ControlPanel::Footer => {
                for style in [
                    FooterStyle::Rich,
                    FooterStyle::Solo,
                    FooterStyle::History,
                    FooterStyle::Clear,
                ] {
                    let style = style.as_str();
                    add(
                        &format!("footer {style}"),
                        &self
                            .state
                            .locale
                            .format("Apply: {0}", &[(style).to_string()]),
                        self.state
                            .locale
                            .translate("Saved status-bar preference")
                            .into(),
                    );
                }
                self.state.locale.format(
                    "Status bar · {0}",
                    &[(self.footer_style.as_str()).to_string()],
                )
            }
            ControlPanel::Capabilities => {
                add(
                    "mcp",
                    self.state.locale.translate("Connections"),
                    self.state
                        .locale
                        .translate("Connected systems, authentication, and tools")
                        .into(),
                );
                add(
                    "tools",
                    self.state.locale.translate("Effective tools"),
                    self.state
                        .locale
                        .translate("Built-in and connected capabilities")
                        .into(),
                );
                add(
                    "skills",
                    self.state.locale.translate("Skills"),
                    self.state
                        .locale
                        .translate("Inspect and manage specialized instructions")
                        .into(),
                );
                add(
                    "plugins",
                    self.state.locale.translate("Plugins"),
                    self.state
                        .locale
                        .translate("Inspect installed extensions and catalog")
                        .into(),
                );
                add(
                    "hooks",
                    self.state.locale.translate("Hooks"),
                    self.state
                        .locale
                        .translate("Advanced extension configuration")
                        .into(),
                );
                add(
                    "a2a peers",
                    self.state.locale.translate("Agent connections"),
                    self.state.locale.translate("Inspect paired peers").into(),
                );
                self.state
                    .locale
                    .translate("Connections and capabilities")
                    .into()
            }
            ControlPanel::Advanced => {
                add(
                    "diag",
                    self.state.locale.translate("Diagnostics"),
                    self.state
                        .locale
                        .translate("Session health and configuration")
                        .into(),
                );
                add(
                    "alerts",
                    self.state.locale.translate("Errors"),
                    self.state
                        .locale
                        .translate("Recorded agent and API errors")
                        .into(),
                );
                add(
                    "limits",
                    self.state.locale.translate("Runtime limits"),
                    self.state
                        .locale
                        .translate("Effective values; environment changes require restart")
                        .into(),
                );
                add(
                    "context audit",
                    self.state.locale.translate("Prompt provenance"),
                    self.state
                        .locale
                        .translate("Inspect context sources and effective tools")
                        .into(),
                );
                add(
                    "about",
                    self.state.locale.translate("Build information"),
                    self.state
                        .locale
                        .translate("Version and environment")
                        .into(),
                );
                self.state.locale.translate("Advanced").into()
            }
            ControlPanel::Tasks => {
                add(
                    "operations",
                    self.state.locale.translate("Activity"),
                    self.state
                        .locale
                        .translate("Persisted tool executions and approvals")
                        .into(),
                );
                add(
                    "workers",
                    self.state.locale.translate("Workers"),
                    self.state
                        .locale
                        .translate("Inspect, redirect, cancel, or resume existing work")
                        .into(),
                );
                add(
                    "queue",
                    self.state.locale.translate("Queued prompts"),
                    self.state
                        .locale
                        .format("{0} pending", &[(self.queued_prompts.len()).to_string()]),
                );
                add(
                    "decision",
                    self.state.locale.translate("Pending decisions"),
                    self.state
                        .locale
                        .translate("Answer or cancel background decisions")
                        .into(),
                );
                add(
                    "goal",
                    self.state.locale.translate("Objective and budget"),
                    self.state
                        .locale
                        .translate("Inspect explicit goals and continuation bounds")
                        .into(),
                );
                add(
                    "workflow",
                    self.state.locale.translate("Workflows"),
                    self.state
                        .locale
                        .translate("Durable runs and their controls")
                        .into(),
                );
                add(
                    "loop",
                    self.state.locale.translate("Schedules"),
                    self.state
                        .locale
                        .translate("Inspect recurring prompts; creation stays explicit")
                        .into(),
                );
                add(
                    "monitor",
                    self.state.locale.translate("Background output"),
                    self.state
                        .locale
                        .translate("Inspect existing task monitors")
                        .into(),
                );
                add(
                    "computer",
                    self.state.locale.translate("Computer tasks"),
                    self.state
                        .locale
                        .translate("Inspect hosted work and availability")
                        .into(),
                );
                add(
                    "a2a tasks",
                    self.state.locale.translate("Delegated tasks"),
                    self.state
                        .locale
                        .translate("Inspect work accepted by connected peers")
                        .into(),
                );
                add(
                    "mailbox",
                    self.state.locale.translate("Task messages"),
                    self.state
                        .locale
                        .translate("Inspect pending inter-agent messages")
                        .into(),
                );
                add(
                    "settings session",
                    self.state.locale.translate("Session actions"),
                    self.state
                        .locale
                        .translate("Fork, recover, summarize, and export")
                        .into(),
                );
                self.state.locale.translate("Tasks").into()
            }
            ControlPanel::Context => {
                add(
                    "context usage",
                    self.state.locale.translate("Included context"),
                    self.state
                        .locale
                        .translate("Token budget, sources, and tools")
                        .into(),
                );
                add(
                    "context audit",
                    self.state.locale.translate("Provenance"),
                    self.state
                        .locale
                        .translate("Inspect effective prompt sources")
                        .into(),
                );
                add(
                    "memory",
                    self.state.locale.translate("Memory"),
                    self.state
                        .locale
                        .translate("Review, save, edit, or forget notes explicitly")
                        .into(),
                );
                add(
                    "harness review",
                    self.state.locale.translate("Instruction proposals"),
                    self.state
                        .locale
                        .translate("Review before applying durable changes")
                        .into(),
                );
                add(
                    "harness",
                    self.state.locale.translate("Instructions"),
                    self.state
                        .locale
                        .translate("Inspect revisions and rollback controls")
                        .into(),
                );
                add(
                    "rlm",
                    self.state.locale.translate("Advanced variables"),
                    self.state
                        .locale
                        .translate("Inspect named context values")
                        .into(),
                );
                add(
                    "compact",
                    self.state.locale.translate("Compact now"),
                    self.state
                        .locale
                        .translate("Manual override; automatic compaction remains enabled")
                        .into(),
                );
                self.state.locale.translate("Context").into()
            }
            ControlPanel::Model => {
                add(
                    "model select",
                    self.state.locale.translate("Choose model"),
                    self.state.locale.format(
                        "{0} · this session",
                        std::slice::from_ref(&(self.current_model)),
                    ),
                );
                add(
                    "thinking",
                    self.state.locale.translate("Effort"),
                    self.state.locale.format(
                        "{0} · this session",
                        &[(self.current_thinking_level.label()).to_string()],
                    ),
                );
                add(
                    "boost",
                    self.state.locale.translate("Boost this task"),
                    self.state
                        .locale
                        .translate("One-task intelligence override")
                        .into(),
                );
                add(
                    &format!("model default {}", self.current_model),
                    self.state.locale.translate("Save current as default"),
                    self.state
                        .locale
                        .translate("Applies to new sessions")
                        .into(),
                );
                add(
                    "setup",
                    self.state.locale.translate("Account and provider setup"),
                    self.state
                        .locale
                        .translate("Configure an available inference route")
                        .into(),
                );
                self.state.locale.translate("Model and effort").into()
            }
            ControlPanel::Effort => {
                for level in supported_efforts(&self.current_model) {
                    let name = level.label().to_lowercase();
                    add(
                        &format!("thinking {name}"),
                        &self
                            .state
                            .locale
                            .format("Apply: {0}", std::slice::from_ref(&(name))),
                        self.state
                            .locale
                            .translate("This session; next model request")
                            .into(),
                    );
                }
                format!(
                    "Effort · {} · {}",
                    self.current_thinking_level.label(),
                    self.current_model
                )
            }
            ControlPanel::Review => {
                add(
                    "git review",
                    self.state.locale.translate("Changes"),
                    self.state
                        .locale
                        .translate("Git status and staged/worktree summary")
                        .into(),
                );
                add(
                    "diff",
                    self.state.locale.translate("Diff"),
                    self.state
                        .locale
                        .translate("Inspect the working tree diff")
                        .into(),
                );
                add(
                    "rubber-duck",
                    self.state.locale.translate("Request second opinion"),
                    self.state
                        .locale
                        .translate("Run model review of uncommitted changes")
                        .into(),
                );
                self.state.locale.translate("Review").into()
            }
            ControlPanel::Help => {
                add(
                    "help commands",
                    self.state.locale.translate("Commands"),
                    self.state
                        .locale
                        .translate("Generated command help and usage")
                        .into(),
                );
                add(
                    "hotkeys",
                    self.state.locale.translate("Keyboard shortcuts"),
                    self.state.locale.translate("Effective keybindings").into(),
                );
                add(
                    "settings advanced",
                    self.state.locale.translate("Diagnostics"),
                    self.state
                        .locale
                        .translate("Errors, limits, and build information")
                        .into(),
                );
                add(
                    "cost",
                    self.state.locale.translate("Usage"),
                    self.state
                        .locale
                        .translate("Recorded tokens and cost")
                        .into(),
                );
                add(
                    "bug",
                    self.state.locale.translate("Report a bug"),
                    self.state
                        .locale
                        .translate("Draft and review before sending")
                        .into(),
                );
                add(
                    "settings session",
                    self.state.locale.translate("Session actions"),
                    self.state
                        .locale
                        .translate("History, recovery, and export")
                        .into(),
                );
                self.state.locale.translate("Help").into()
            }
            ControlPanel::Session => {
                add(
                    "session",
                    self.state.locale.translate("Session details"),
                    self.state
                        .locale
                        .translate("Workspace, model, and permission state")
                        .into(),
                );
                add(
                    "resume",
                    self.state.locale.translate("Resume a session"),
                    self.state
                        .locale
                        .translate("Browse saved conversations")
                        .into(),
                );
                add(
                    "continue",
                    self.state.locale.translate("Continue most recent"),
                    self.state
                        .locale
                        .translate("Most recent conversation in this workspace")
                        .into(),
                );
                add(
                    "fork",
                    self.state.locale.translate("Fork conversation"),
                    self.state
                        .locale
                        .translate("Create a separate conversation branch")
                        .into(),
                );
                add(
                    "rewind",
                    self.state.locale.translate("Rewind"),
                    self.state
                        .locale
                        .translate("Preview conversation or file recovery")
                        .into(),
                );
                add(
                    "summarize",
                    self.state.locale.translate("Summarize selected turns"),
                    self.state
                        .locale
                        .translate("Create a saved conversation")
                        .into(),
                );
                add(
                    "export",
                    self.state.locale.translate("Export"),
                    self.state
                        .locale
                        .translate("Choose a format and destination")
                        .into(),
                );
                add(
                    "history",
                    self.state.locale.translate("Prompt history"),
                    self.state
                        .locale
                        .translate("Search previous prompts")
                        .into(),
                );
                add(
                    "view-plan",
                    self.state.locale.translate("View plan"),
                    self.state.locale.translate("Current plan artifact").into(),
                );
                self.state.locale.translate("Session actions").into()
            }
        };
        self.command_palette.show_panel(title, rows);
        self.active_modal = ActiveModal::CommandPalette;
    }
}

fn supported_efforts(model: &str) -> Vec<ThinkingLevel> {
    [
        ThinkingLevel::Off,
        ThinkingLevel::Minimal,
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
        ThinkingLevel::Max,
    ]
    .into_iter()
    .filter(|level| crate::model_dynamics::normalize_thinking(model, *level) == *level)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn effort_choices_match_the_model_request_contract() {
        assert_eq!(supported_efforts("gpt-4o"), vec![ThinkingLevel::Off]);
        assert_eq!(
            supported_efforts("anthropic/claude-fable-5-1"),
            vec![
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
                ThinkingLevel::Max
            ]
        );
        for level in supported_efforts("o1") {
            assert_eq!(
                crate::model_dynamics::normalize_thinking("o1", level),
                level
            );
        }
    }
}
