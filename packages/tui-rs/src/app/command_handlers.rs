use super::*;
use crate::commands::{AttachAction, GoalAction, HarnessAction, OrbAction, RlmAction};
use crate::state::ApprovalMode;

/// Normalize a slash-completion string to a single leading `/`.
///
/// `get_completions` returns values like `/help`. Older call sites (and any
/// bare name without a slash) must still resolve to exactly one leading slash
/// so the input never becomes `//help`.
#[must_use]
pub(crate) fn normalize_slash_completion(cmd: &str) -> String {
    let trimmed = cmd.trim();
    if trimmed.is_empty() || trimmed.chars().all(|c| c == '/') {
        return "/".to_string();
    }
    let name = trimmed.trim_start_matches('/');
    format!("/{name}")
}

/// The system message reported for a `/session cleanup` outcome.
///
/// `SessionManager::prune_sessions` returns `(removed, errors)`.
/// `errors` counts real (non-contention) failures acquiring a session's
/// lock -- permission or descriptor failures, not "another Maestro process
/// has it open" -- so `removed == 0` does not mean nothing went wrong.
/// Before this fix, `removed == 0` always reported "No sessions to prune."
/// regardless of `errors`, discarding that count and reading as success
/// even when every eligible session failed to prune.
#[must_use]
pub(crate) fn cleanup_result_message(removed: usize, errors: usize) -> String {
    if removed == 0 && errors == 0 {
        maestro_ui::localization::tr("No sessions to prune.").to_string()
    } else if removed == 0 {
        maestro_ui::localization::format(
            "Failed to prune sessions: {0} error(s).",
            &[(errors).to_string()],
        )
    } else {
        let mut msg =
            maestro_ui::localization::format("Pruned {0} session(s).", &[(removed).to_string()]);
        if errors > 0 {
            msg.push_str(&format!(" {errors} error(s)."));
        }
        msg
    }
}

impl App {
    /// Update slash state based on current input
    pub(super) fn update_slash_state(&mut self) {
        if self.state.input().starts_with('/') {
            // Strip every leading `/` so `//help` still matches `help`.
            let query = self.state.input().trim_start_matches('/');
            self.slash_state.set_query(query, &self.slash_matcher);
            // Ghost text only makes sense at the end of the input.
            self.state.ghost_completion = if self.state.cursor() == self.state.input().len() {
                self.slash_matcher
                    .get_inline_completion(self.state.input())
                    .map(|completion| completion.suffix)
            } else {
                None
            };
        } else {
            self.slash_state.reset();
            self.state.ghost_completion = None;
        }
    }

    /// Handle tab for slash command completion
    pub(super) fn handle_slash_tab(&mut self) {
        if self.slash_state.has_completions() {
            self.slash_state.cycle_next();
        } else {
            let query = self.state.input().trim_start_matches('/');
            self.slash_state.set_query(query, &self.slash_matcher);
        }
        self.apply_slash_completion();
    }

    /// Apply the current slash completion to input.
    ///
    /// Completions from `SlashCommandMatcher::get_completions` already include a
    /// leading `/` (e.g. `/help`). Do **not** prefix another slash or the input
    /// becomes `//help` and the registry looks up command name `/help`.
    pub(super) fn apply_slash_completion(&mut self) {
        if let Some(cmd) = self.slash_state.current() {
            self.state.set_input(&normalize_slash_completion(cmd));
        }
    }

    /// Handle a command output from the registry
    pub(super) async fn handle_command_output(&mut self, output: CommandOutput) {
        let mut stack = vec![output];
        while let Some(current) = stack.pop() {
            match current {
                CommandOutput::Message(msg) => {
                    self.state.add_system_message(msg);
                }
                CommandOutput::Help(msg) => {
                    self.state.add_system_message(msg);
                }
                CommandOutput::Warning(msg) => {
                    self.state.error = Some(msg);
                }
                CommandOutput::OpenModal(modal_type) => match modal_type {
                    ModalType::ThemeSelector => {
                        self.theme_selector.show();
                        self.active_modal = ActiveModal::ThemeSelector;
                    }
                    ModalType::Setup => {
                        self.open_onboarding();
                    }
                    ModalType::ModelSelector => {
                        self.local_model_discovery
                            .refresh_if_stale(std::time::Duration::from_secs(5));
                        let current = if self.current_model.is_empty() {
                            self.state.model.clone()
                        } else {
                            Some(self.current_model.clone())
                        };
                        self.model_selector.set_current_model(current);
                        self.model_selector.show();
                        self.active_modal = ActiveModal::ModelSelector;
                    }
                    ModalType::SessionList => {
                        self.session_switcher.show();
                        self.active_modal = ActiveModal::SessionSwitcher;
                    }
                    ModalType::Operations => {
                        self.operations.show();
                        self.active_modal = ActiveModal::Operations;
                    }
                    ModalType::FileSearch => {
                        self.file_search.show();
                        self.active_modal = ActiveModal::FileSearch;
                    }
                    ModalType::CommandPalette => {
                        self.show_command_palette();
                    }
                    ModalType::ShortcutsHelp => {
                        self.shortcuts_help.show();
                        self.active_modal = ActiveModal::ShortcutsHelp;
                    }
                    ModalType::Help => {
                        self.show_help();
                    }
                },
                CommandOutput::Action(action) => {
                    self.handle_command_action(action).await;
                }
                CommandOutput::Silent => {}
                CommandOutput::Multi(outputs) => {
                    for out in outputs.into_iter().rev() {
                        stack.push(out);
                    }
                }
            }
        }
    }

    pub(super) fn cycle_model(&mut self, backward: bool) {
        if self.state.busy || self.pending_model_change.is_some() || self.pending_agent_spawn {
            self.state.status = Some(
                self.state
                    .locale
                    .translate("Wait for the current turn or model switch to finish.")
                    .into(),
            );
            return;
        }
        let config = crate::config::model_dynamics_config();
        self.model_selector
            .set_current_model(Some(self.current_model.clone()));
        let candidates = if config.cycle.is_empty() {
            self.model_selector.cycle_routes()
        } else {
            config
                .cycle
                .iter()
                .map(|choice| choice.model.clone())
                .collect()
        };
        let mut routes: Vec<String> = Vec::new();
        for route in candidates {
            if !routes
                .iter()
                .any(|old| crate::model_dynamics::same_model_route(old, &route))
                && (crate::model_dynamics::same_model_route(&route, &self.current_model)
                    || crate::model_catalog::verify_model_offline(&route).state
                        == crate::model_catalog::VerificationState::Verified)
            {
                routes.push(route);
            }
        }
        if let Some(next) =
            crate::model_dynamics::next_cycle_route(&routes, &self.current_model, backward)
        {
            self.switch_model(next, false);
        } else {
            self.state.status = Some(
                self.state
                    .locale
                    .translate("No other configured model is available to cycle to.")
                    .into(),
            );
        }
    }

    /// Handle a command action that modifies state
    /// Switch the session model, optionally persisting it as the user
    /// default in `~/.maestro/config.toml` first.
    pub(super) fn switch_model(&mut self, model_id: &str, persist_default: bool) {
        let policy_model = policy_model_id(model_id);
        if let Some(reason) = check_model_allowed(&policy_model) {
            self.state.error = Some(reason);
            return;
        }
        self.state.error = None;
        if persist_default {
            match crate::config_cli::persist_user_model_default(model_id) {
                Ok(path) => {
                    self.state.add_system_message(self.state.locale.format(
                        "Default model saved to {0}",
                        &[(path.display()).to_string()],
                    ));
                }
                Err(error) => {
                    self.state.error = Some(
                        self.state
                            .locale
                            .format("Failed to save default model: {0}", &[(error).to_string()]),
                    );
                    return;
                }
            }
        }
        if let Some(agent) = &self.native_agent {
            if let Err(e) = agent.set_model(model_id) {
                self.state.error = Some(
                    self.state
                        .locale
                        .format("Failed to set model: {0}", &[(e).to_string()]),
                );
            } else {
                self.pending_model_change = Some(PendingModelChange {
                    model: model_id.to_owned(),
                });
                self.state.status = Some(if persist_default {
                    self.state.locale.format(
                        "Switching model: {0} (saved as default)",
                        &[(model_id).to_string()],
                    )
                } else {
                    self.state
                        .locale
                        .format("Switching model: {0}", &[(model_id).to_string()])
                });
            }
        } else {
            // First-run `/setup` and `/model` both land here before spawn,
            // including after a failed start. Remember the route and retry
            // so a new key or model is actually applied.
            let requested = crate::config::model_dynamics_config()
                .effort_for_model(model_id)
                .unwrap_or(self.current_thinking_level);
            self.current_thinking_level =
                crate::model_dynamics::normalize_thinking(model_id, requested);
            self.state.thinking_level = self.current_thinking_level;
            self.current_model = model_id.to_owned();
            self.current_model_user_set = true;
            self.state.model = Some(model_id.to_owned());
            self.pending_agent_spawn = true;
            self.state.status = Some(if persist_default {
                self.state
                    .locale
                    .format("Default model saved: {0}", &[(model_id).to_string()])
            } else {
                self.state
                    .locale
                    .format("Model set to {0}", &[(model_id).to_string()])
            });
        }
    }

    pub(super) async fn handle_command_action(&mut self, action: CommandAction) {
        match action {
            CommandAction::OpenPanel(panel) => self.show_control_panel(panel),
            CommandAction::SetOutputDetail(detail) => self.apply_output_detail(detail),
            CommandAction::ClearMessages => {
                self.start_new_session(self.state.locale.translate("New session started."));
            }
            CommandAction::ToggleZenMode => {
                self.state.zen_mode = !self.state.zen_mode;
                if self.state.zen_mode {
                    self.state.status = Some(self.state.locale.translate("Zen mode enabled").to_string());
                } else {
                    self.state.status = Some(self.state.locale.translate("Zen mode disabled").to_string());
                }
            }
            CommandAction::SetCompactTools(mode) => {
                let compact = mode.unwrap_or(!self.state.compact_tool_outputs);
                self.apply_output_detail(if compact {
                    crate::state::OutputDetail::Compact
                } else {
                    crate::state::OutputDetail::Expanded
                });
            }
            CommandAction::SetApprovalMode(mode) => {
                if mode == "next" {
                    self.state.approval_mode = self.state.approval_mode.next();
                } else if let Some(m) = ApprovalMode::parse(&mode) {
                    self.state.approval_mode = m;
                } else {
                    self.state.error = Some(self.state.locale.format("Unknown approval mode: {0}. Use: yolo, selective, safe", std::slice::from_ref(&(mode))));
                    return;
                }
                self.sync_agent_approval_mode();
                // Keep interaction_mode loosely aligned with approval shortcuts.
                self.state.interaction_mode = match self.state.approval_mode {
                    ApprovalMode::Yolo => crate::state::InteractionMode::AlwaysApprove,
                    ApprovalMode::Safe | ApprovalMode::Selective => crate::state::InteractionMode::Normal,
                };
                self.state.status = Some(self.state.locale.format("Approval mode: {0}", &[(self.state.approval_mode.label()).to_string()]));
            }
            CommandAction::CycleInteractionMode => {
                self.cycle_interaction_mode();
            }
            CommandAction::SideQuestion(question) => {
                let _ = self.handle_side_question(question).await;
            }
            CommandAction::MagicTrace(action) => {
                self.handle_magic_trace(action);
            }
            CommandAction::Boost => {
                if let Some(agent) = &self.native_agent {
                    if let Err(error) = agent.boost() {
                        self.state.error = Some(self.state.locale.format("Could not request boost: {0}", &[(error).to_string()]));
                    }
                } else {
                    self.state.status = Some(self.state.locale.translate("Start a conversation before using /boost.").into());
                }
            }
            CommandAction::SetThinkingLevel(level_str) => {
                if let Some(level) = ThinkingLevel::parse(&level_str) {
                    let level = crate::model_dynamics::normalize_thinking(&self.current_model, level);
                    let (enabled, budget) = level.to_config();
                    if let Some(agent) = &self.native_agent {
                        if let Err(e) = agent.set_thinking(enabled, budget) {
                            self.state.error = Some(self.state.locale.format("Failed to set thinking: {0}", &[(e).to_string()]));
                            return;
                        }
                    }
                    self.current_thinking_level = level;
                    self.state.thinking_level = self.current_thinking_level;
                    self.record_thinking_level_change(level);
                    self.state.status =
                        Some(self.state.locale.format("Thinking: {0} (budget: {1})", &[(level.label()).to_string(), (budget).to_string()]));
                } else {
                    self.state.error = Some(self.state.locale.format("Unknown thinking level: {0}. Use: off, minimal, low, medium, high, max", std::slice::from_ref(&(level_str))));
                }
            }
            CommandAction::Quit => {
                self.should_quit = true;
            }
            CommandAction::RefreshWorkspace => {
                // Scan on a background thread like startup; the event loop
                // applies the result (and confirms it) when it arrives.
                self.spawn_workspace_scan();
                self.workspace_refresh_pending = true;
                self.state.status = Some(self.state.locale.translate("Refreshing workspace files...").to_string());
            }
            CommandAction::CopyLastMessage => self.copy_transcript(&crate::transcript_copy::CopyTarget::Response),
            CommandAction::CopyTranscript(target) => self.copy_transcript(&target),
            CommandAction::SetLanguage(locale) => self.apply_language(locale),
            CommandAction::SetTheme(theme_name) => {
                if let Err(e) = crate::themes::set_theme_by_name(&theme_name) {
                    self.state.error = Some(self.state.locale.format("Failed to set theme: {0}", &[(e).to_string()]));
                } else {
                    self.state.status = Some(self.state.locale.format("Theme set to: {0}", std::slice::from_ref(&(theme_name))));
                }
            }
            CommandAction::SetModel(model_id) => {
                self.switch_model(&model_id, false);
            }
            CommandAction::RubberDuck { model } => {
                self.start_rubber_duck_review(model);
            }
            CommandAction::SetDefaultModel(model_id) => {
                self.switch_model(&model_id, true);
            }
            CommandAction::BackgroundMonitor(action) => match action {
                BackgroundMonitorAction::Add { task_id, pattern } => {
                    match crate::tools::background_tasks::attach_monitor(&task_id, &pattern) {
                        Ok(monitor) => self.state.add_system_message(self.state.locale.format("Background monitor {0} attached to task {1}. Matches are notifications only.", &[(monitor.id).clone(), (monitor.task_id).clone()])),
                        Err(error) => self.state.error = Some(error),
                    }
                }
                BackgroundMonitorAction::List => {
                    let monitors = crate::tools::background_tasks::list_monitors();
                    let message = if monitors.is_empty() {
                        self.state.locale.translate("No background monitors.").to_string()
                    } else {
                        monitors
                            .iter()
                            .map(|monitor| {
                                format!(
                                    "{}  task={}  regex={}",
                                    monitor.id, monitor.task_id, monitor.pattern
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    };
                    self.state.add_system_message(message);
                }
                BackgroundMonitorAction::Remove { monitor_id } => {
                    match crate::tools::background_tasks::remove_monitor(&monitor_id) {
                        Ok(_) => self
                            .state
                            .add_system_message(self.state.locale.format("Removed background monitor {0}.", std::slice::from_ref(&(monitor_id)))),
                        Err(error) => self.state.error = Some(error),
                    }
                }
            },
            CommandAction::Loop(action) => match action {
                LoopAction::Start {
                    interval_secs,
                    prompt,
                } => {
                    self.loop_schedule = Some(LoopSchedule {
                        interval: Duration::from_secs(interval_secs),
                        prompt: prompt.clone(),
                        next_fire: Instant::now() + Duration::from_secs(interval_secs),
                    });
                    self.state.add_system_message(self.state.locale.format("Loop started: every {0}s — \"{1}\". Use /loop stop to cancel.", &[(interval_secs).to_string(), (prompt).clone()]));
                }
                LoopAction::Stop => {
                    if self.loop_schedule.take().is_some() {
                        self.state.add_system_message(self.state.locale.translate("Loop stopped.").to_string());
                    } else {
                        self.state.status = Some(self.state.locale.translate("No active loop.").to_string());
                    }
                }
                LoopAction::Status => match &self.loop_schedule {
                    Some(schedule) => self.state.add_system_message(self.state.locale.format("Loop active: every {0}s — \"{1}\"", &[(schedule.interval.as_secs()).to_string(), (schedule.prompt).clone()])),
                    None => self
                        .state
                        .add_system_message(self.state.locale.translate("No active loop. Usage: /loop <interval> <prompt>").to_string()),
                },
            },
            CommandAction::Goal(action) => self.handle_goal_action(action),
            CommandAction::Harness(action) => self.handle_harness_action(action),
            CommandAction::Rlm(action) => self.handle_rlm_action(action),
            CommandAction::Mailbox(action) => self.handle_mailbox_action(action),
            CommandAction::Worker(action) => {
                let (name, args) = action.tool_call();
                let call_id = format!("worker-command-{}", uuid::Uuid::new_v4());
                let result = self.tool_executor.execute(name, &args, None, &call_id).await;
                if result.success {
                    self.state.add_system_message(result.output);
                } else {
                    self.state.error = Some(result.error.unwrap_or(result.output));
                }
            }
            CommandAction::SetDexPresentation(setting) => self.handle_dex_command(&setting),
            CommandAction::SetFooterStyle(style) => {
                let mut prefs = crate::ui_prefs::UiPrefs::load_default();
                prefs.set_footer_style(style);
                if let Err(e) = prefs.save_default() {
                    self.state.error = Some(self.state.locale.format("Footer style unchanged; failed to persist {0}: {1}", &[(style.as_str()).to_string(), (e).to_string()]));
                } else {
                    self.footer_style = style;
                    self.ui_prefs = prefs;
                    self.state.error = None;
                    self.state
                        .status
                        .replace(self.state.locale.format("Footer style: {0} (saved)", &[(self.state.locale.translate(style.as_str())).to_string()]));
                }
            }
            CommandAction::Attach(action) => self.handle_attach_action(action),
            CommandAction::SummarizeConversation => self.open_selective_summary(),
            CommandAction::CompactConversation(instructions) => {
                self.open_selective_summary_with_instructions(instructions);
            }
            CommandAction::Mcp(action) => {
                self.handle_mcp_action(action).await;
            }
            CommandAction::Orb(action) => {
                self.handle_orb_action(action).await;
            }
            CommandAction::A2a(action) => {
                self.handle_a2a_action(action);
            }
            CommandAction::HooksManage(hooks_action) => {
                self.handle_hooks_action(hooks_action).await;
            }
            CommandAction::ShowUsage(usage_action) => {
                self.handle_usage_action(usage_action);
            }
            CommandAction::SetContextTool { name, excluded } => {
                if self.state.busy {
                    self.state.status = Some(self.state.locale.translate("Wait for the active response to finish before changing context tools.").into());
                } else if let Some(agent) = &self.native_agent {
                    if let Err(error) = agent.set_context_tool_excluded(name, excluded) {
                        self.state.error = Some(error.to_string());
                    }
                } else {
                    self.state.error = Some(self.state.locale.translate("The agent is not running.").into());
                }
            }
            CommandAction::ShowContext => {
                self.show_context_breakdown();
            }
            CommandAction::ShowPromptAudit { json } => {
                self.show_prompt_audit(json);
            }
            CommandAction::SetFocus(mode) => {
                let enabled = mode.unwrap_or(!self.state.focus_view);
                self.apply_output_detail(if enabled {
                    crate::state::OutputDetail::Summary
                } else {
                    crate::state::OutputDetail::Compact
                });
            }
            CommandAction::BugReport(args) => {
                let (action, value) = args.trim().split_once(char::is_whitespace).unwrap_or((args.trim(), ""));
                if action == "rating" {
                    self.handle_feedback_rating(value).await;
                    return;
                }
                if let Err(error) = self.handle_bug_report(&args).await {
                    self.state.add_system_message(self.state.locale.format("Bug report: {0}", &[(error).to_string()]));
                }
            }
            CommandAction::ExportSession(export_action) => {
                self.handle_export_action(export_action);
            }
            CommandAction::ShowHistory(history_action) => {
                self.handle_history_action(history_action);
            }
            CommandAction::ShowToolHistory(tool_history_action) => {
                self.handle_tool_history_action(tool_history_action);
            }
            CommandAction::Skills(skills_action) => {
                self.handle_skills_action(skills_action);
            }
            CommandAction::Plugins(plugins_action) => {
                self.handle_plugins_action(plugins_action);
            }
            CommandAction::InvokeSkill { name, args } => {
                self.handle_invoke_skill(&name, &args).await;
            }
            CommandAction::InvokePromptTemplate { name, args } => {
                self.handle_invoke_prompt_template(&name, &args).await;
            }
            CommandAction::InvokeExecCommand { name, args } => {
                self.handle_invoke_exec_command(&name, &args);
            }
            CommandAction::Queue(action) => {
                self.handle_queue_action(action);
            }
            CommandAction::Steer(text) => {
                let _ = self.handle_steer_submit(text).await;
            }
            CommandAction::Session(session_action) => {
                self.handle_session_action(session_action);
            }
            CommandAction::Trust(trust_action) => {
                self.handle_trust_action(trust_action);
            }
            CommandAction::ShowPreferences => {
                self.config_selector.show_experiments();
                self.active_modal = ActiveModal::Preferences;
            }
            CommandAction::ShowSandbox => {
                self.show_sandbox_status();
            }
            CommandAction::Init { force } => {
                self.handle_init_command(force).await;
            }
            CommandAction::ShowTools => {
                self.show_tools_list();
            }
            CommandAction::ShowMemory => {
                self.show_memory_status();
            }
            CommandAction::ShowAlerts => {
                self.show_alerts();
            }
            CommandAction::ShowDiagnostics => {
                let mut diag = String::new();
                diag.push_str("## Diagnostics\n\n");

                // Model & Provider
                diag.push_str(&self.state.locale.format("**Model:** {0}\n", &[self.state.model.as_deref().unwrap_or("(none)").to_string()]));
                diag.push_str(&self.state.locale.format("**Provider:** {0}\n", &[self.state.provider.as_deref().unwrap_or("(none)").to_string()]));

                // Working directory & Git
                diag.push_str(&format!(
                    "**CWD:** {}\n",
                    self.state.cwd.as_deref().unwrap_or("(unknown)")
                ));
                diag.push_str(&self.state.locale.format("**Git Branch:** {0}\n", &[self.state.git_branch.as_deref().unwrap_or("(not a repo)").to_string()]));

                // Session
                diag.push_str(&self.state.locale.format("**Session:** {0}\n", &[self.state.session_id.as_deref().unwrap_or("(ephemeral)").to_string()]));

                // Modes
                diag.push_str(&self.state.locale.format("**Approval Mode:** {0}\n", &[(self.state.approval_mode.label()).to_string()]));
                let sandbox = self
                    .sandbox_policy
                    .as_ref()
                    .map_or(self.state.locale.translate("none"), crate::sandbox::SandboxPolicy::mode_label);
                diag.push_str(&format!("**Sandbox:** {sandbox}\n"));
                let cwd_path = self
                    .state
                    .cwd
                    .as_deref()
                    .map(std::path::Path::new)
                    .unwrap_or_else(|| std::path::Path::new("."));
                diag.push_str(&format!(
                    "**Trust:** {}\n",
                    if crate::config::workspace_trusted_in_global_config(cwd_path) {
                        "trusted"
                    } else {
                        "untrusted"
                    }
                ));
                diag.push_str(&self.state.locale.format("**Zen Mode:** {0}\n", &[(if self.state.zen_mode { "on" } else { "off" }).to_string()]));
                diag.push_str(&self.state.locale.format("**Steering Mode:** {0}\n", &[(self.state.steering_mode.label()).to_string()]));
                diag.push_str(&self.state.locale.format("**Follow-up Mode:** {0}\n", &[(self.state.follow_up_mode.label()).to_string()]));

                // Terminal info
                if let Ok((cols, rows)) = crossterm::terminal::size() {
                    diag.push_str(&format!("**Terminal:** {cols}x{rows}\n"));
                }

                // Message count
                diag.push_str(&format!("**Messages:** {}\n", self.state.messages.len()));

                self.state.add_system_message(diag);
            }
        }
    }

    async fn handle_orb_action(&mut self, action: OrbAction) {
        let action = match action {
            OrbAction::List => crate::tools::orb_delegation::OrbConsoleAction::List,
            OrbAction::Status(id) => crate::tools::orb_delegation::OrbConsoleAction::Status { id },
            OrbAction::Followup { id, prompt } => {
                crate::tools::orb_delegation::OrbConsoleAction::Followup { id, prompt }
            }
            OrbAction::Pause(id) => crate::tools::orb_delegation::OrbConsoleAction::Pause { id },
            OrbAction::Resume(id) => crate::tools::orb_delegation::OrbConsoleAction::Resume { id },
            OrbAction::Cancel(id) => crate::tools::orb_delegation::OrbConsoleAction::Cancel { id },
            OrbAction::Collect(id) => {
                crate::tools::orb_delegation::OrbConsoleAction::Collect { id }
            }
            OrbAction::HandoffCreate {
                source_id,
                target_thread_id,
                files,
                artifact_ids,
                include_diff,
            } => crate::tools::orb_delegation::OrbConsoleAction::HandoffCreate {
                source_id,
                target_thread_id,
                files,
                artifact_ids,
                include_diff,
            },
            OrbAction::HandoffList { target_thread_id } => {
                crate::tools::orb_delegation::OrbConsoleAction::HandoffList { target_thread_id }
            }
            OrbAction::HandoffRead {
                target_thread_id,
                package_id,
            } => crate::tools::orb_delegation::OrbConsoleAction::HandoffRead {
                target_thread_id,
                package_id,
            },
        };
        let result = self.tool_executor.run_orb_console(action).await;
        if result.success {
            if !result.output.is_empty() {
                self.state.add_system_message(result.output);
            }
        } else {
            self.state.error = Some(result.error.unwrap_or_else(|| {
                self.state
                    .locale
                    .translate("Hosted Computer operation failed")
                    .to_string()
            }));
        }
    }

    /// Handle usage/cost display actions
    pub(super) fn handle_usage_action(&mut self, action: crate::commands::UsageAction) {
        use crate::commands::UsageAction;

        match action {
            UsageAction::Summary => {
                let summary = self.usage_tracker.summary();
                // Put the numbers on the first content line. The composer
                // covers the last line of the last message, so a heading-
                // then-body layout only showed "## Usage Summary".
                self.state.add_system_message(
                    self.state
                        .locale
                        .format("## Usage Summary — {0}", std::slice::from_ref(&(summary))),
                );
            }
            UsageAction::Detailed => {
                let detailed = self.usage_tracker.detailed_summary();
                self.state.add_system_message(self.state.locale.format(
                    "## Usage Details\n\n```\n{0}\n```",
                    std::slice::from_ref(&(detailed)),
                ));
            }
            UsageAction::Reset => {
                self.usage_tracker.reset();
                self.state.status = Some(
                    self.state
                        .locale
                        .translate("Usage tracking reset")
                        .to_string(),
                );
            }
        }
    }

    /// Show the `/context` breakdown: token usage of the current session by
    /// category, measured with the same estimator the compactor uses.
    pub(super) fn show_context_breakdown(&mut self) {
        let model = (!self.current_model.is_empty()).then_some(self.current_model.as_str());
        let mut breakdown = super::context_breakdown::ContextBreakdown::compute_for_model(
            &self.build_system_prompt(),
            &self.state.messages,
            model,
        );
        let mut tool_rows = Vec::new();
        let mut excluded = Vec::new();
        let mut basis = self
            .state
            .locale
            .translate("Visible transcript estimate; provider request framing is not included.");
        let mut report_model = self.current_model.clone();
        let mut cache_explanation = self
            .state
            .locale
            .translate("No comparison is available for the current request.");
        let mut response_reserve = None;
        let mut runtime_context_window = None;
        if let Some(agent) = &self.native_agent {
            let snapshot = agent.runtime_audit_snapshot();
            response_reserve = Some(u64::from(snapshot.max_output_tokens));
            runtime_context_window = snapshot.context_window;
            if let Some(reason) = snapshot.cache_reuse {
                cache_explanation = reason.explanation();
            }
            excluded.extend(snapshot.excluded_context_tools);
            if let Some(request) = snapshot.request_context {
                report_model = request.model;
                breakdown.system_prompt = request.system;
                breakdown.conversation = request.conversation;
                breakdown.tool_results = request.tool_results;
                breakdown.other = request.other;
                tool_rows = request.tools;
                basis = self.state.locale.translate("Last prepared request estimate, not provider-reported usage. Wire framing and provider-side context are not included.");
            } else {
                breakdown.system_prompt = snapshot.system_prompt.as_deref().map_or(0, |prompt| {
                    maestro_context::token_counting::count_tokens(prompt, model)
                });
                tool_rows = snapshot
                    .tools
                    .iter()
                    .map(|definition| {
                        (
                            definition.tool.name.clone(),
                            maestro_context::token_counting::count_tokens(
                                &serde_json::to_string(&definition.tool).unwrap_or_default(),
                                model,
                            ),
                        )
                    })
                    .collect();
                basis = self.state.locale.translate("Current context estimate; these tool schemas have not been counted as dispatched usage.");
            }
        }
        breakdown.tool_schemas = tool_rows.iter().map(|(_, tokens)| tokens).sum();
        let context_window = runtime_context_window
            .or(self.state.context_window)
            .or_else(|| {
                crate::model_catalog::find_model(&self.current_model)
                    .map(|info| u64::from(info.capabilities.context_tokens))
            });
        let remaining_headroom = context_window.map(|window| {
            window.saturating_sub(
                breakdown
                    .total()
                    .saturating_add(response_reserve.unwrap_or(0))
                    .saturating_add(maestro_runtime::agent::REQUEST_CONTEXT_SAFETY_TOKENS),
            )
        });
        let mut report = crate::localization::with_locale(self.state.locale, || {
            breakdown.render_with_budget(
                Some(&report_model),
                context_window,
                response_reserve,
                remaining_headroom,
            )
        });
        report.push_str(&format!("\n\n{basis}"));
        report.push_str(&self.state.locale.format("\n\nPrompt cache estimate: {0} Provider-reported cache usage is shown separately in /usage.", &[(cache_explanation).to_string()]));
        tool_rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        if !tool_rows.is_empty() {
            report.push_str(self.state.locale.translate("\n\nTool schemas:"));
            for (name, tokens) in tool_rows.iter().take(10) {
                report.push_str(&self.state.locale.format(
                    "\n- {0}: {1} tokens",
                    &[(name).clone(), (tokens).to_string()],
                ));
            }
            report.push_str(self.state.locale.translate("\n\nUse /context exclude TOOL to omit its schema for this session; /context include TOOL restores it."));
        }
        excluded.sort();
        if !excluded.is_empty() {
            report.push_str(
                &self
                    .state
                    .locale
                    .format("\nExcluded schemas: {0}.", &[(excluded.join(", ")).clone()]),
            );
        }
        let audit = self.build_system_prompt_assembly().audit(model, Vec::new());
        let mut fragments = audit.fragments;
        fragments.sort_by_key(|fragment| std::cmp::Reverse(fragment.token_count));
        report.push_str(
            self.state
                .locale
                .translate("\n\nCurrent instruction sources:"),
        );
        for fragment in fragments
            .iter()
            .filter(|fragment| fragment.token_count > 0)
            .take(8)
        {
            report.push_str(&self.state.locale.format(
                "\n- {0}: {1} tokens ({2})",
                &[
                    (fragment.name).clone(),
                    (fragment.token_count).to_string(),
                    (fragment.source).clone(),
                ],
            ));
        }
        self.state.add_system_message(report);
    }

    /// Audit the standing prompt and registered tool surface without exposing
    /// prompt bodies, tool descriptions, schemas, or credential material.
    pub(super) fn show_prompt_audit(&mut self, json: bool) {
        let report = self.build_prompt_audit_report();
        let value = match serde_json::to_value(&report) {
            Ok(value) => crate::agent::credential_store::redact_credentials_in_json(&value),
            Err(error) => {
                self.state.error = Some(
                    self.state
                        .locale
                        .format("Prompt audit failed: {0}", &[(error).to_string()]),
                );
                return;
            }
        };
        let rendered = if json {
            serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_string())
        } else {
            report.render_markdown()
        };
        self.state.add_system_message(rendered);
        self.write_session_entry(SessionEntry::Custom(CustomEntry {
            id: Some(uuid::Uuid::new_v4().to_string()),
            parent_id: None,
            timestamp: Utc::now().to_rfc3339(),
            custom_type: "prompt_audit".to_string(),
            data: Some(value),
        }));
        self.flush_session();
    }

    /// Handle session export actions
    pub(super) fn handle_session_action(&mut self, action: crate::commands::SessionAction) {
        use crate::commands::SessionAction;
        match action {
            SessionAction::Cleanup => {
                let max_sessions = std::env::var("MAESTRO_MAX_SESSIONS")
                    .ok()
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(100);
                let max_age_days = std::env::var("MAESTRO_MAX_SESSION_AGE_DAYS")
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(90);

                let (removed, errors) = self
                    .session_manager
                    .prune_sessions(max_sessions, max_age_days);
                self.state
                    .add_system_message(cleanup_result_message(removed, errors));
            }
            SessionAction::New => {
                self.start_new_session(self.state.locale.translate("New session started."));
            }
            SessionAction::Fork => {
                self.fork_session();
            }
            SessionAction::BrowseBranches => {
                self.session_switcher
                    .show_branches(self.state.session_id.as_deref());
                self.active_modal = ActiveModal::SessionSwitcher;
            }
            SessionAction::Rewind { turns, dry_run } => {
                self.rewind_turns(turns, dry_run);
            }
            SessionAction::RewindBoth { turns, dry_run } => {
                self.rewind_saved_turns(turns, dry_run, true);
            }
            SessionAction::RewindFiles => {
                self.rewind_files();
            }
            SessionAction::ListCheckpoints => {
                self.list_file_checkpoints();
            }
            SessionAction::Continue => {
                self.continue_last_session();
            }
            SessionAction::Status => {
                self.show_session_status();
            }
        }
    }

    fn handle_trust_action(&mut self, action: crate::commands::TrustAction) {
        use crate::commands::TrustAction;
        let cwd = self
            .state
            .cwd
            .as_deref()
            .map(std::path::Path::new)
            .unwrap_or_else(|| std::path::Path::new("."));
        match action {
            TrustAction::Status => {
                let trusted = crate::config::workspace_trusted_in_global_config(cwd);
                self.state.add_system_message(self.state.locale.format("Workspace trust for {0}: **{1}**\n\nUse `/trust grant` to load project skills/plugins/hooks, or `/trust revoke` to disable them.", &[(cwd.display()).to_string(), (if trusted { "trusted" } else { "untrusted" }).to_string()]));
            }
            TrustAction::Grant => match crate::config::set_workspace_trust_in_global_config(cwd, true)
            {
                Ok(path) => self.state.add_system_message(self.state.locale.format("Trusted {0}. Project skills/plugins/hooks will load on the next `/skills reload` or restart.\nWrote {1}.", &[(cwd.display()).to_string(), (path.display()).to_string()])),
                Err(error) => self.state.error = Some(error),
            },
            TrustAction::Revoke => {
                match crate::config::set_workspace_trust_in_global_config(cwd, false) {
                    Ok(path) => self.state.add_system_message(self.state.locale.format("Revoked trust for {0}. Project config will not load after restart.\nWrote {1}.", &[(cwd.display()).to_string(), (path.display()).to_string()])),
                    Err(error) => self.state.error = Some(error),
                }
            }
        }
    }

    fn show_sandbox_status(&mut self) {
        let mut msg = String::from("## Sandbox\n\n");
        match &self.sandbox_policy {
            None => {
                msg.push_str(
                    self.state
                        .locale
                        .translate("**Policy:** none (session is not OS-sandboxed)\n"),
                );
                msg.push_str(
                    self.state.locale.translate("Interactive default is gated (stage-1). Set `MAESTRO_SANDBOX_MODE` or config sandbox settings to enable.\n"),
                );
            }
            Some(policy) => {
                msg.push_str(&format!("**Policy:** `{}`\n", policy.mode_label()));
                match policy {
                    crate::sandbox::SandboxPolicy::ReadOnly => {
                        msg.push_str(self.state.locale.translate(
                            "Reads allowed; writes and many network tools are blocked.\n",
                        ));
                    }
                    crate::sandbox::SandboxPolicy::WorkspaceWrite { network_access, .. } => {
                        msg.push_str(
                            self.state.locale.translate("In-workspace writes allowed under existing trees; `.git` stays read-only.\n"),
                        );
                        msg.push_str(
                            self.state.locale.translate("Stage-1 note: writing content into a *new* file at the repo root can fail closed under Landlock (no WriteFile on root).\n"),
                        );
                        msg.push_str(
                            &self.state.locale.format(
                                "Network: {0}\n",
                                &[(if *network_access {
                                    "enabled"
                                } else {
                                    "disabled"
                                })
                                .to_string()],
                            ),
                        );
                    }
                    crate::sandbox::SandboxPolicy::DangerFullAccess => {
                        msg.push_str(
                            self.state
                                .locale
                                .translate("Full host access; native OS sandbox not applied.\n"),
                        );
                    }
                }
                if !crate::sandbox::is_sandbox_available() {
                    let reason =
                        crate::sandbox::sandbox_unavailable_reason().unwrap_or_else(|| {
                            self.state
                                .locale
                                .translate("native sandbox unavailable")
                                .to_string()
                        });
                    msg.push_str(&self.state.locale.format(
                        "\n**Host:** sandbox unavailable ({0})\n",
                        std::slice::from_ref(&(reason)),
                    ));
                }
            }
        }
        self.state.add_system_message(msg);
    }

    fn show_session_status(&mut self) {
        let mut msg = String::from(self.state.locale.translate("## Session\n\n"));
        msg.push_str(
            &self.state.locale.format(
                "**Session id:** {0}\n",
                &[self
                    .state
                    .session_id
                    .as_deref()
                    .unwrap_or("(ephemeral)")
                    .to_string()],
            ),
        );
        if let Some(path) = self.session_manager.current_session_path() {
            msg.push_str(&format!("**Path:** {}\n", path.display()));
        } else {
            msg.push_str(&self.state.locale.format(
                "**Sessions dir:** {0}\n",
                &[(self.session_manager.sessions_dir().display()).to_string()],
            ));
        }
        msg.push_str(&self.state.locale.format(
            "**Model:** {0}\n",
            &[self.state.model.as_deref().unwrap_or("(none)").to_string()],
        ));
        msg.push_str(
            &self.state.locale.format(
                "**Provider:** {0}\n",
                &[self
                    .state
                    .provider
                    .as_deref()
                    .unwrap_or("(none)")
                    .to_string()],
            ),
        );
        msg.push_str(&format!(
            "**Approval:** {}\n",
            self.state.approval_mode.label()
        ));
        let sandbox = self.sandbox_policy.as_ref().map_or(
            self.state.locale.translate("none"),
            crate::sandbox::SandboxPolicy::mode_label,
        );
        msg.push_str(&format!("**Sandbox:** {sandbox}\n"));
        let cwd = self
            .state
            .cwd
            .as_deref()
            .map(std::path::Path::new)
            .unwrap_or_else(|| std::path::Path::new("."));
        let trusted = crate::config::workspace_trusted_in_global_config(cwd);
        msg.push_str(&format!(
            "**Trust:** {}\n",
            if trusted { "trusted" } else { "untrusted" }
        ));
        msg.push_str(&format!("**Messages:** {}\n", self.state.messages.len()));
        self.state.add_system_message(msg);
    }

    fn show_tools_list(&mut self) {
        use crate::tools::ToolRegistry;
        let registry = ToolRegistry::new();
        let mut names: Vec<String> = registry
            .tools()
            .map(|def| {
                let name = def.tool.name.as_str();
                let desc = def.tool.description.lines().next().unwrap_or("").trim();
                if desc.is_empty() {
                    format!("- `{name}`")
                } else {
                    format!("- `{name}` — {desc}")
                }
            })
            .collect();
        names.sort();
        let mut msg = String::from(self.state.locale.translate("## Built-in tools\n\n"));
        if names.is_empty() {
            msg.push_str(self.state.locale.translate("*No tools registered*\n"));
        } else {
            msg.push_str(&names.join("\n"));
            msg.push('\n');
        }
        msg.push_str(
            self.state
                .locale
                .translate("\nMCP tools: `/mcp` · `/tools mcp` for status\n"),
        );
        self.state.add_system_message(msg);
    }

    fn show_memory_status(&mut self) {
        use crate::path_utils::maestro_home_dir;
        let mut msg = String::from(
            self.state.locale.translate("## Memory\n\nReview proposed corrections with `/memory review`; save one with `/memory save <proposal-id>`. Saved notes: `/memory list`, `/memory edit <entry-id> <text>`, `/memory forget <entry-id>`. Proposals include their scope and evidence and stay inactive until saved. These commands manage local supplemental context; account memory remains under its account controls.\n\n"),
        );
        msg.push_str(&format!(
            "**{}**\n\n",
            crate::memory_cli::local_account_memory_status()
        ));
        if let Some(home) = maestro_home_dir() {
            let memory_dir = home.join("memory");
            msg.push_str(&self.state.locale.format(
                "**Local dir:** `{0}`\n",
                &[(memory_dir.display()).to_string()],
            ));
            if memory_dir.is_dir() {
                match std::fs::read_dir(&memory_dir) {
                    Ok(entries) => {
                        let mut files: Vec<String> = entries
                            .flatten()
                            .filter(|e| e.path().is_file())
                            .map(|e| e.file_name().to_string_lossy().to_string())
                            .collect();
                        files.sort();
                        if files.is_empty() {
                            msg.push_str(
                                self.state
                                    .locale
                                    .translate("*No local memory files yet.*\n"),
                            );
                        } else {
                            msg.push_str(
                                &self
                                    .state
                                    .locale
                                    .format("**Files ({0}):**\n", &[(files.len()).to_string()]),
                            );
                            for f in files.iter().take(30) {
                                msg.push_str(&format!("- `{f}`\n"));
                            }
                            if files.len() > 30 {
                                msg.push_str(&self.state.locale.format(
                                    "- …and {0} more\n",
                                    &[(files.len() - 30).to_string()],
                                ));
                            }
                        }
                    }
                    Err(err) => msg.push_str(
                        &self
                            .state
                            .locale
                            .format("Could not list memory dir: {0}\n", &[(err).to_string()]),
                    ),
                }
            } else {
                msg.push_str(
                    self.state.locale.translate("*Directory not created yet.* Local notes land here when memory writes are enabled.\n"),
                );
            }
        } else {
            msg.push_str(
                self.state
                    .locale
                    .translate("*Could not resolve MAESTRO_HOME / ~/.maestro*\n"),
            );
        }
        if std::env::var("MAESTRO_SHARED_MEMORY_BASE").is_ok() {
            msg.push_str(
                self.state.locale.translate("\n**Shared memory sync:** configured (`MAESTRO_SHARED_MEMORY_BASE`). The account-memory commands use EvalOps instead.\n"),
            );
        } else {
            msg.push_str(
                self.state
                    .locale
                    .translate("\n**Shared memory sync:** not configured.\n"),
            );
        }
        self.state.add_system_message(msg);
    }

    pub(super) fn continue_last_session(&mut self) {
        if self.state.busy {
            self.state.status = Some(
                self.state
                    .locale
                    .translate(
                        "Wait for the active response to finish before continuing another session.",
                    )
                    .to_string(),
            );
            return;
        }
        match self
            .session_manager
            .recent_sessions(1)
            .map(|sessions| sessions.into_iter().next())
        {
            Ok(Some(session)) => self.resume_session_path(&session.path, &session.id),
            Ok(None) => {
                self.state.status.replace(
                    self.state
                        .locale
                        .translate("No previous session found for this workspace.")
                        .to_string(),
                );
            }
            Err(err) => {
                self.state.error = Some(
                    self.state
                        .locale
                        .format("Failed to load last session: {0}", &[(err).to_string()]),
                );
            }
        }
    }

    /// Prepare and read the selected transcript once, while retaining the
    /// current writer until in-memory adoption succeeds.
    pub(crate) fn resume_session_path(&mut self, path: &std::path::Path, target_session_id: &str) {
        if self.session_cleanup_pending() {
            self.state.error = Some(self.state.locale.translate(
                "Could not confirm that the response stopped. The current conversation was kept.",
            ).to_owned());
            return;
        }
        let same_active_session = self
            .session_manager
            .current_session_path()
            .as_deref()
            .is_some_and(|active| active == path);
        // Re-selecting the active path uses our existing lock. Other targets
        // are read under the prepared writer's lock, never before acquiring it.
        let prepared = if same_active_session {
            None
        } else {
            match self.session_manager.prepare_session_adoption(path) {
                Ok(prepared) => Some(prepared),
                Err(err) => {
                    self.report_session_resume_failure(target_session_id, err);
                    return;
                }
            }
        };
        let active_session;
        let session = if let Some(prepared) = prepared.as_ref() {
            prepared.session()
        } else {
            let result = self
                .session_manager
                .flush()
                .map_err(|error| error.to_string())
                .and_then(|()| {
                    crate::session::SessionReader::read_file(path)
                        .map_err(|error| error.to_string())
                });
            active_session = match result {
                Ok(session) => session,
                Err(err) => {
                    self.report_session_resume_failure(target_session_id, err);
                    return;
                }
            };
            &active_session
        };
        // Use the locked transcript's workspace, not potentially stale listing
        // metadata, to decide whether this requires a fresh runtime.
        let saved = std::path::Path::new(&session.header.cwd);
        let current = std::path::Path::new(self.session_manager.cwd());
        if saved != current
            && dunce::canonicalize(saved)
                .ok()
                .zip(dunce::canonicalize(current).ok())
                .is_none_or(|(saved, current)| saved != current)
        {
            if !saved.is_absolute() || !saved.is_dir() {
                self.state.error = Some(self.state.locale.format("Cannot resume: workspace {0} is missing. Restore that directory and try again.", &[(saved.display()).to_string()]));
                return;
            }
            // Tools, trust, hooks, and project configuration are bound at startup.
            // Keep this transcript untouched until orderly shutdown, then build
            // a fresh agent in the saved directory.
            self.resume_target = Some((saved.to_path_buf(), session.header.id.clone()));
            self.should_quit = true;
            return;
        }
        let session_id = session.header.id.clone();
        self.dex_terminal = None;
        self.dex_delight = Default::default();
        // Resuming a persisted transcript begins a new credential scope.
        // Historical references intentionally cannot resolve after reload.
        self.credential_vault.clear();
        // Drop the old session's error surface and force a full repaint so
        // its frames cannot linger beneath the restored transcript.
        self.reset_rendered_viewport();
        if let Some(agent) = &self.native_agent {
            // Drop any active-session state before the new transcript and
            // credential scope become visible.
            agent.clear_history();
        }
        restore_visible_session_messages(&mut self.state, session);
        crate::tools::tool_call_contract::restore_pending_contracts(
            unanswered_tool_call_contracts(session),
        );

        self.state.session_id = Some(session_id.clone());
        self.state.status = Some(
            self.state
                .locale
                .format("Resumed session: {0}", std::slice::from_ref(&(session_id))),
        );

        let mut model_applied = true;
        let mut thinking_applied = true;
        if let Some(agent) = &self.native_agent {
            if let Err(e) = agent.set_model(&session.header.model) {
                self.state.error = Some(
                    self.state
                        .locale
                        .format("Failed to set model: {0}", &[(e).to_string()]),
                );
                model_applied = false;
                thinking_applied = false;
            } else {
                let (enabled, budget) = session.header.thinking_level.to_config();
                if let Err(e) = agent.set_thinking(enabled, budget) {
                    self.state.error = Some(
                        self.state
                            .locale
                            .format("Failed to set thinking: {0}", &[(e).to_string()]),
                    );
                    thinking_applied = false;
                }
            }
        }

        self.session_started_at = chrono::DateTime::parse_from_rfc3339(&session.header.timestamp)
            .ok()
            .and_then(|dt| {
                let secs = dt.timestamp();
                if secs < 0 {
                    None
                } else {
                    Some(UNIX_EPOCH + Duration::new(secs as u64, dt.timestamp_subsec_nanos()))
                }
            })
            .unwrap_or_else(SystemTime::now);
        self.hydrate_usage_from_session(session);

        if model_applied {
            self.current_model = session.header.model.clone();
            self.state.model = Some(session.header.model.clone());
            self.usage_tracker.set_model(session.header.model.clone());
            if thinking_applied {
                self.current_thinking_level = session.header.thinking_level;
                self.state.thinking_level = self.current_thinking_level;
            }
        } else if !self.current_model.is_empty() {
            self.usage_tracker.set_model(self.current_model.clone());
        }

        self.restore_pending_lifecycle_agent_notes(session);
        let restored_history = self.native_agent.as_ref().map(|_| {
            (
                crate::session::model_history(session),
                session
                    .compactions
                    .last()
                    .and_then(|entry| entry.continuation.clone()),
            )
        });
        if let Some(prepared) = prepared {
            self.session_manager.adopt_prepared_session(prepared);
        }
        // Re-adopt this session's own scope: completions from children it
        // started earlier are parked, not discarded, and surface from here.
        self.adopt_session_context(Some(&session_id), "resume");
        if let (Some(agent), Some((messages, continuation))) =
            (&self.native_agent, restored_history)
        {
            // Session transition clears old tool-output references. Install the
            // target history and its continuation together after that transition.
            agent.replace_history_with_continuation(messages, continuation);
        }
        self.session_resume_failed = false;
        self.last_esc_at = None;
        if let Some(event) = self.new_session_event(
            maestro_runtime_contracts::SessionEventLane::Runtime,
            "session.resumed",
            maestro_runtime_contracts::SessionEventPhase::Completed,
        ) {
            self.record_session_event(event);
        }
    }

    fn report_session_resume_failure(
        &mut self,
        target_session_id: &str,
        err: impl std::fmt::Display,
    ) {
        let had_active_session = self.session_manager.current_session_id().is_some();
        if !had_active_session {
            self.session_resume_failed = true;
        }
        self.state.error = Some(super::format_session_persistence_error(
            self.state.locale.translate("resume the session writer"),
            err,
        ));
        self.state.status = Some(self.state.locale.format(
            if had_active_session {
                "Session resume failed ({0}); current session unchanged"
            } else {
                "Session resume failed ({0}); use /new to continue"
            },
            &[target_session_id.to_string()],
        ));
    }

    /// Resume a specific session before the event loop starts.
    ///
    /// Used by `maestro fork` to continue a freshly forked session. At this
    /// point the native agent has not spawned yet, so the agent-facing parts
    /// of [`App::resume_session_path`] are no-ops; the forked session's
    /// model is adopted through `MAESTRO_MODEL` by the caller instead.
    pub fn resume_session_at_startup(&mut self, session_id: &str) {
        match self.session_manager.find_session(session_id) {
            Ok(session) => self.resume_session_path(&session.path, &session.id),
            Err(err) => {
                self.state.error = Some(
                    self.state
                        .locale
                        .format("Failed to load session: {0}", &[(err).to_string()]),
                );
            }
        }
    }

    /// List recently recorded alerts in the transcript and mark them seen.
    fn show_alerts(&mut self) {
        const MAX_LISTED: usize = 10;
        if self.state.alerts.is_empty() {
            self.state.add_system_message(
                self.state
                    .locale
                    .translate("No alerts recorded this session.")
                    .to_string(),
            );
            return;
        }
        let total = self.state.alerts.len();
        let skipped = total.saturating_sub(MAX_LISTED);
        let mut text = if skipped > 0 {
            self.state.locale.format(
                "## Alerts ({0} recorded; latest {1} shown)\n",
                &[(total).to_string(), (MAX_LISTED).to_string()],
            )
        } else {
            format!("## Alerts ({total} recorded)\n")
        };
        for (index, alert) in self.state.alerts.iter().enumerate().skip(skipped) {
            text.push_str(&format!("\n{}. {alert}", index + 1));
        }
        self.state.add_system_message(text);
        self.state.mark_alerts_seen();
    }

    pub(super) fn cycle_interaction_mode(&mut self) {
        let next = self.state.interaction_mode.next();
        self.apply_interaction_mode(next);
    }

    fn apply_interaction_mode(&mut self, mode: crate::state::InteractionMode) {
        self.state.interaction_mode = mode;
        self.state.approval_mode = mode.approval_mode();
        self.sync_agent_approval_mode();
        self.update_agent_system_prompt();
        self.state.status = Some(self.state.locale.format(
            "Mode: {0} (approvals: {1})",
            &[
                mode.label().to_string(),
                self.state.approval_mode.label().to_string(),
            ],
        ));
    }

    fn plan_cwd(&self) -> String {
        self.state.cwd.clone().unwrap_or_else(|| ".".to_string())
    }

    /// Kick off a background `/rubber-duck` review of uncommitted changes with
    /// a different model. The finished review arrives via `poll_rubber_duck`.
    fn start_rubber_duck_review(&mut self, requested_model: Option<String>) {
        if self.rubber_duck_running {
            self.state.add_system_message(
                self.state
                    .locale
                    .translate("A rubber duck review is already running.")
                    .to_string(),
            );
            return;
        }
        let cwd = self.plan_cwd();
        let cwd_path = std::path::Path::new(&cwd);
        if !crate::git::is_git_repo(cwd_path) {
            self.state.add_system_message(
                self.state
                    .locale
                    .translate("Not a git repository; nothing to review.")
                    .to_string(),
            );
            return;
        }
        let review_model = match crate::rubber_duck::pick_review_model(
            &self.current_model,
            requested_model.as_deref(),
        ) {
            Ok(model) => model,
            Err(message) => {
                self.state.add_system_message(message);
                return;
            }
        };
        let (tx, rx) = std::sync::mpsc::channel();
        self.rubber_duck_rx = Some(rx);
        self.rubber_duck_running = true;
        self.state.status = Some(self.state.locale.format(
            "Rubber duck reviewing with {0}…",
            std::slice::from_ref(&(review_model)),
        ));
        self.state.add_system_message(self.state.locale.format("Rubber duck review started with {0} (current model: {1}). The result will appear here when done.", &[(review_model).clone(), (self.current_model).clone()]));
        tokio::spawn(crate::rubber_duck::run_review(
            review_model,
            cwd,
            self.current_model.clone(),
            tx,
        ));
    }

    fn handle_magic_trace(&mut self, action: crate::commands::MagicTraceAction) {
        use crate::commands::MagicTraceAction;
        match action {
            MagicTraceAction::Stop => {
                crate::magic_trace::stop_indicator();
                self.state.status = Some(
                    self.state
                        .locale
                        .translate("magic-trace: stop indicator fired (snapshot if attached)")
                        .into(),
                );
            }
            MagicTraceAction::EnableSlowFrame => {
                crate::magic_trace::set_slow_frame_trigger(true);
                self.state.status = Some(
                    self.state
                        .locale
                        .translate("magic-trace: slow-frame auto snapshot ON (attach first)")
                        .into(),
                );
            }
            MagicTraceAction::DisableSlowFrame => {
                crate::magic_trace::set_slow_frame_trigger(false);
                self.state.status = Some(
                    self.state
                        .locale
                        .translate("magic-trace: slow-frame auto snapshot OFF")
                        .into(),
                );
            }
            MagicTraceAction::Status => {
                let on = crate::magic_trace::slow_frame_trigger_enabled();
                self.state.add_system_message(self.state.locale.format("## magic-trace\n\nLinux + Intel PT only. Build: `cargo build --profile magic-trace`.\n\nAttach: `scripts/magic-trace-tui.sh attach`\n\nSlow-frame trigger: **{0}** (`/magic-trace on|off`)\n\nManual snapshot: `/magic-trace stop`", &[(if on { "on" } else { "off" }).to_string()]));
            }
        }
    }

    pub(super) fn handle_export_action(&mut self, action: crate::commands::ExportAction) {
        use crate::commands::ExportAction;
        use crate::session::{ExportFormat, ExportOptions, SessionReader};

        let (format, path) = match action {
            ExportAction::Markdown(p) => (ExportFormat::Markdown, p),
            ExportAction::Html(p) => (ExportFormat::Html, p),
            ExportAction::Json(p) => (ExportFormat::Json, p),
            ExportAction::PlainText(p) => (ExportFormat::PlainText, p),
            ExportAction::ShowOptions => {
                self.state.add_system_message(
                    self.state.locale.translate("## Session Export\n\nUsage: `/export <format> [path]`\n\n**Formats:**\n- `markdown` or `md` - Human-readable markdown\n- `html` - Styled HTML page\n- `json` - Structured JSON data\n- `text` or `txt` - Plain text\n\n**Examples:**\n- `/export markdown` - Output to terminal\n- `/export html session.html` - Save to file\n")
                        .to_string(),
                );
                return;
            }
        };

        let options = ExportOptions {
            format,
            ..Default::default()
        };

        let _ = self.session_manager.flush();

        let session_path = self.session_manager.current_session_path().or_else(|| {
            let session_id = self.state.session_id.as_ref()?;
            self.session_manager
                .list_all_sessions()
                .ok()?
                .into_iter()
                .find(|s| &s.id == session_id)
                .map(|s| s.path)
        });

        let Some(session_path) = session_path else {
            self.state.error = Some(
                self.state
                    .locale
                    .translate("No active session to export")
                    .to_string(),
            );
            return;
        };

        let output_path = if let Some(path) = path {
            let expanded = if let Some(stripped) = path.strip_prefix("~/") {
                let home = dirs::home_dir().unwrap_or_else(std::env::temp_dir);
                home.join(stripped)
            } else {
                std::path::PathBuf::from(path)
            };
            if expanded.is_absolute() {
                expanded
            } else {
                std::env::current_dir()
                    .unwrap_or_else(|_| std::path::PathBuf::from("."))
                    .join(expanded)
            }
        } else {
            let mut default_path = session_path.clone();
            default_path.set_extension(format.extension());
            default_path
        };

        if let Some(reason) = check_path_allowed(&output_path) {
            self.state.error = Some(reason);
            return;
        }

        if let Some(parent) = output_path.parent() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                self.state.error = Some(self.state.locale.format(
                    "Failed to create export directory: {0}",
                    &[(err).to_string()],
                ));
                return;
            }
        }

        let session = match SessionReader::read_file(&session_path) {
            Ok(session) => session,
            Err(err) => {
                self.state.error = Some(
                    self.state
                        .locale
                        .format("Failed to read session: {0}", &[(err).to_string()]),
                );
                return;
            }
        };

        let exporter = SessionExporter::from_session(&session, options);
        let output = exporter.export_to_string();
        if let Err(err) = std::fs::write(&output_path, output) {
            self.state.error = Some(
                self.state
                    .locale
                    .format("Failed to write export: {0}", &[(err).to_string()]),
            );
            return;
        }

        self.state.status = Some(self.state.locale.format(
            "Session exported to {0}",
            &[(output_path.to_string_lossy()).to_string()],
        ));
    }

    /// Handle prompt history actions
    pub(super) fn handle_history_action(&mut self, action: crate::commands::HistoryAction) {
        use crate::commands::HistoryAction;

        match action {
            HistoryAction::Recent(count) => {
                let recent = self.prompt_history.recent(count);
                if recent.is_empty() {
                    self.state.status =
                        Some(self.state.locale.translate("No prompt history").to_string());
                    return;
                }

                let mut msg = String::from(self.state.locale.translate("## Recent Prompts\n\n"));
                for (i, entry) in recent.iter().enumerate() {
                    let chars: Vec<char> = entry.prompt.chars().collect();
                    let preview = if chars.len() > 60 {
                        format!("{}...", chars[..57].iter().collect::<String>())
                    } else {
                        entry.prompt.clone()
                    };
                    msg.push_str(&format!("{}. {}\n", i + 1, preview));
                }
                self.state.add_system_message(msg);
            }
            HistoryAction::Search(query) => {
                let results = self.prompt_history.search(&query);
                if results.matches.is_empty() {
                    self.state.status = Some(
                        self.state
                            .locale
                            .format("No matches for '{0}'", std::slice::from_ref(&(query))),
                    );
                    return;
                }

                let mut msg = self.state.locale.format(
                    "## Search Results for '{0}'\n\n",
                    std::slice::from_ref(&(query)),
                );
                for (i, m) in results.matches.iter().take(10).enumerate() {
                    let chars: Vec<char> = m.entry.prompt.chars().collect();
                    let preview = if chars.len() > 60 {
                        format!("{}...", chars[..57].iter().collect::<String>())
                    } else {
                        m.entry.prompt.clone()
                    };
                    msg.push_str(&format!("{}. {} (score: {:.2})\n", i + 1, preview, m.score));
                }
                self.state.add_system_message(msg);
            }
            HistoryAction::Clear => {
                self.prompt_history.clear();
                let _ = self.prompt_history.delete_file();
                self.state.status = Some(
                    self.state
                        .locale
                        .translate("Prompt history cleared")
                        .to_string(),
                );
            }
        }
    }

    /// Handle tool history actions
    pub(super) fn handle_tool_history_action(
        &mut self,
        action: crate::commands::ToolHistoryAction,
    ) {
        use crate::commands::ToolHistoryAction;

        match action {
            ToolHistoryAction::Recent(count) => {
                let recent = self.tool_history.recent(count);
                if recent.is_empty() {
                    self.state.status =
                        Some(self.state.locale.translate("No tool history").to_string());
                    return;
                }

                let mut msg =
                    String::from(self.state.locale.translate("## Recent Tool Executions\n\n"));
                for exec in recent {
                    let status = if exec.success { "✓" } else { "✗" };
                    let duration = exec
                        .duration
                        .map_or_else(|| "?".to_string(), |d| format!("{:.0}ms", d.as_millis()));
                    msg.push_str(&format!(
                        "{} **{}** ({})\n",
                        status, exec.tool_name, duration
                    ));
                }
                self.state.add_system_message(msg);
            }
            ToolHistoryAction::Stats => {
                let summary = self.tool_history.summary();
                self.state.add_system_message(self.state.locale.format(
                    "## Tool Statistics\n\n```\n{0}\n```",
                    std::slice::from_ref(&(summary)),
                ));
            }
            ToolHistoryAction::ForTool(name) => {
                let execs = self.tool_history.for_tool(&name);
                if execs.is_empty() {
                    self.state.status = Some(
                        self.state
                            .locale
                            .format("No history for tool '{0}'", std::slice::from_ref(&(name))),
                    );
                    return;
                }

                let mut msg = self
                    .state
                    .locale
                    .format("## History for '{0}'\n\n", std::slice::from_ref(&(name)));
                for exec in execs.iter().take(10) {
                    let status = if exec.success { "✓" } else { "✗" };
                    let duration = exec
                        .duration
                        .map_or_else(|| "?".to_string(), |d| format!("{:.0}ms", d.as_millis()));
                    msg.push_str(&format!(
                        "{} {} - {}\n",
                        status,
                        duration,
                        exec.output_preview(50).unwrap_or_default()
                    ));
                }
                self.state.add_system_message(msg);
            }
            ToolHistoryAction::Clear => {
                self.tool_history.clear();
                self.state.status = Some(
                    self.state
                        .locale
                        .translate("Tool history cleared")
                        .to_string(),
                );
            }
        }
    }

    /// Handle MCP actions
    pub(super) async fn handle_mcp_action(&mut self, action: crate::commands::McpAction) {
        use crate::commands::McpAction;

        match action {
            McpAction::Configure { args } => {
                if args.first().is_some_and(|arg| arg == "auth") {
                    if self.mcp_config_in_flight {
                        self.state.add_system_message(
                            self.state
                                .locale
                                .translate("An MCP authentication flow is already in progress.")
                                .to_string(),
                        );
                        return;
                    }
                    self.mcp_config_in_flight = true;
                    self.state.add_system_message(
                        self.state
                            .locale
                            .translate("Finish MCP authentication in your browser.")
                            .to_string(),
                    );
                    let tx = self.mcp_config_tx.clone();
                    let wake = self.loop_wake.clone();
                    tokio::spawn(async move {
                        let result =
                            crate::mcp_config_cli::apply_mcp_config_async_quiet(&args).await;
                        let _ = tx.send(result.map_err(|error| error.to_string()));
                        wake.signal();
                    });
                } else {
                    match crate::mcp_config_cli::apply_mcp_config_async(&args).await {
                        Ok(message) => self.state.add_system_message(message),
                        Err(error) => self.state.add_system_message(
                            self.state
                                .locale
                                .format("MCP configuration failed: {0}", &[(error).to_string()]),
                        ),
                    }
                }
            }
            McpAction::Status => {
                self.mcp_manager.locale = self.state.locale;
                self.active_modal = ActiveModal::McpManager;
                self.last_mcp_status_refresh = None;
                self.refresh_mcp_badges_with_force(true).await;
            }
            McpAction::Resources { server, uri } => {
                let servers = match self.tool_executor.mcp_status().await {
                    Ok(servers) => servers,
                    Err(err) => {
                        self.state.add_system_message(self.state.locale.format(
                            "Failed to load MCP status: {0}",
                            std::slice::from_ref(&(err)),
                        ));
                        return;
                    }
                };
                self.update_mcp_badge_counts(&servers);

                if let (Some(server), Some(uri)) = (server, uri) {
                    let status = servers.iter().find(|s| s.name == server);
                    if let Some(status) = status {
                        if !status.connected {
                            self.state.add_system_message(self.state.locale.format(
                                "Server '{0}' not connected",
                                std::slice::from_ref(&(server)),
                            ));
                            return;
                        }
                    }

                    match self.tool_executor.mcp_read_resource(&server, &uri).await {
                        Ok(result) => {
                            let mut lines = Vec::new();
                            lines.push(
                                self.state
                                    .locale
                                    .format("Resource: {0}", std::slice::from_ref(&(uri))),
                            );
                            lines.push(String::new());
                            for content in &result.contents {
                                if let Some(text) = &content.text {
                                    lines.push(text.clone());
                                } else {
                                    let mime = content
                                        .mime_type
                                        .as_deref()
                                        .unwrap_or(self.state.locale.translate("unknown"));
                                    lines.push(
                                        self.state
                                            .locale
                                            .format("[Binary data: {0}]", &[(mime).to_string()]),
                                    );
                                }
                            }
                            self.state.add_system_message(lines.join("\n"));
                        }
                        Err(err) => {
                            self.state.add_system_message(self.state.locale.format(
                                "Failed to read resource: {0}",
                                std::slice::from_ref(&(err)),
                            ));
                        }
                    }
                    return;
                }

                let mut lines = vec![
                    self.state.locale.translate("MCP Resources").to_string(),
                    String::new(),
                ];
                let mut has_resources = false;
                for server in servers {
                    if !server.connected || server.resources.is_empty() {
                        continue;
                    }
                    has_resources = true;
                    lines.push(format!("{}:", server.name));
                    for uri in server.resources {
                        lines.push(format!("  {uri}"));
                    }
                    lines.push(String::new());
                }
                if !has_resources {
                    lines.push(
                        self.state
                            .locale
                            .translate("No resources available from connected servers.")
                            .to_string(),
                    );
                }
                lines.push(String::new());
                lines.push(
                    self.state
                        .locale
                        .translate("Usage: /mcp resources <server> <uri>")
                        .to_string(),
                );
                self.state.add_system_message(lines.join("\n"));
            }
            McpAction::Prompts {
                server,
                name,
                arguments,
            } => {
                let servers = match self.tool_executor.mcp_status().await {
                    Ok(servers) => servers,
                    Err(err) => {
                        self.state.add_system_message(self.state.locale.format(
                            "Failed to load MCP status: {0}",
                            std::slice::from_ref(&(err)),
                        ));
                        return;
                    }
                };
                self.update_mcp_badge_counts(&servers);

                let server_filter = server.clone();
                if let Some(server_name) = server_filter.as_deref() {
                    let Some(status) = servers.iter().find(|entry| entry.name == server_name)
                    else {
                        self.state.add_system_message(
                            self.state
                                .locale
                                .format("Server '{0}' not found", &[(server_name).to_string()]),
                        );
                        return;
                    };
                    if !status.connected {
                        self.state.add_system_message(
                            self.state
                                .locale
                                .format("Server '{0}' not connected", &[(server_name).to_string()]),
                        );
                        return;
                    }
                }

                let prompt_servers = match self
                    .tool_executor
                    .mcp_prompt_details(server_filter.as_deref())
                    .await
                {
                    Ok(entries) => entries,
                    Err(err) => {
                        self.state.add_system_message(self.state.locale.format(
                            "Failed to load MCP prompts: {0}",
                            std::slice::from_ref(&(err)),
                        ));
                        return;
                    }
                };

                if let (Some(server), Some(name), arguments) = (server, name, arguments) {
                    let prompt_exists = prompt_servers.iter().any(|(server_name, prompts)| {
                        server_name == &server && prompts.iter().any(|prompt| prompt.name == name)
                    });
                    if !prompt_exists {
                        self.state.add_system_message(self.state.locale.format(
                            "Prompt '{0}' not found on server '{1}'",
                            &[(name).clone(), (server).clone()],
                        ));
                        return;
                    }

                    let prompt_arguments = if arguments.is_empty() {
                        None
                    } else {
                        Some(arguments)
                    };

                    match self
                        .tool_executor
                        .mcp_get_prompt(&server, &name, prompt_arguments)
                        .await
                    {
                        Ok(result) => {
                            let mut lines = Vec::new();
                            lines.push(
                                self.state
                                    .locale
                                    .format("Prompt: {0}", std::slice::from_ref(&(name))),
                            );
                            if let Some(desc) = result.description {
                                lines.push(String::new());
                                lines.push(
                                    self.state
                                        .locale
                                        .format("Description: {0}", std::slice::from_ref(&(desc))),
                                );
                            }
                            lines.push(String::new());
                            for msg in result.messages {
                                lines.push(format!("[{}]", msg.role));
                                let content = msg
                                    .content
                                    .as_text()
                                    .unwrap_or(self.state.locale.translate("[non-text content]"));
                                lines.push(content.to_string());
                                lines.push(String::new());
                            }
                            self.state.add_system_message(lines.join("\n"));
                        }
                        Err(err) => {
                            self.state.add_system_message(
                                self.state.locale.format(
                                    "Failed to get prompt: {0}",
                                    std::slice::from_ref(&(err)),
                                ),
                            );
                        }
                    }
                    return;
                }

                self.state.add_system_message(
                    render_mcp_prompt_lines(&prompt_servers, server_filter.as_deref()).join("\n"),
                );
            }
        }
    }

    /// Handle A2A peer-pairing actions.
    pub(super) fn handle_a2a_action(&mut self, action: crate::commands::A2aAction) {
        use crate::commands::A2aAction;

        match action {
            A2aAction::Help => {
                self.state.add_system_message(
                    [
                        self.state.locale.translate("## A2A peer pairing"),
                        "",
                        "/a2a fleet",
                        "/a2a peers",
                        "/a2a tasks [peer] [--work-graph]",
                        "/a2a coordinate [peer] [--reply <text>] [--work-graph]",
                        "/a2a accept <pairing-code>",
                        "/a2a register --url <base-url> [--agent-id <id>]",
                        "/a2a delegate <peer> <text>",
                        "/a2a reply <peer> <task-id> <text>",
                        "/a2a send <peer> <text>",
                        "/handoff <text>",
                        "",
                        self.state.locale.translate("Native pairing codes, fleet views, and delegation ledgers are shared with the TypeScript CLI/TUI."),
                    ]
                    .join("\n"),
                );
            }
            A2aAction::Fleet => {
                self.state.add_system_message(
                    self.state.locale.translate("A2A fleet inspection uses the shared Maestro peer registry. Run `deixic-code a2a fleet` for live health and task summaries until the Rust fleet reader is wired into this view.")
                        .to_string(),
                );
            }
            A2aAction::Peers => {
                self.state.add_system_message(
                    self.state.locale.translate("A2A peer listing uses the shared Maestro peer registry. Run `deixic-code a2a peers` for the current registry until the Rust registry reader is wired into this view.")
                        .to_string(),
                );
            }
            A2aAction::Tasks {
                peer,
                include_work_graph,
            } => {
                let scope = peer
                    .as_deref()
                    .unwrap_or(self.state.locale.translate("all peers"));
                let graph_hint = if include_work_graph {
                    self.state
                        .locale
                        .translate(" with Platform work graph and Codex subagent summaries")
                } else {
                    ""
                };
                let graph_flag = if include_work_graph {
                    " --work-graph"
                } else {
                    ""
                };
                self.state.add_system_message(self.state.locale.format("A2A task ledger requested for {0}{1}. Run `deixic-code a2a tasks{2}` for the current durable ledger until the Rust task reader is wired into this view.", &[(scope).to_string(), (graph_hint).to_string(), (graph_flag).to_string()]));
            }
            A2aAction::Coordinate {
                peer,
                reply,
                include_work_graph,
            } => {
                let scope = peer
                    .as_deref()
                    .unwrap_or(self.state.locale.translate("all peers"));
                let reply_hint = reply
                    .as_ref()
                    .map(|value| {
                        self.state
                            .locale
                            .format(" with a {0} character reply", &[(value.len()).to_string()])
                    })
                    .unwrap_or_default();
                let graph_hint = if include_work_graph {
                    self.state
                        .locale
                        .translate(" and Platform work graph context")
                } else {
                    ""
                };
                let graph_flag = if include_work_graph {
                    " --work-graph"
                } else {
                    ""
                };
                self.state.add_system_message(self.state.locale.format("A2A coordination requested for {0}{1}{2}. Run `deixic-code a2a coordinate [peer] --reply <text> --wait{3}` while the Rust coordination controller is connected to the shared A2A client.", &[(scope).to_string(), (reply_hint).clone(), (graph_hint).to_string(), (graph_flag).to_string()]));
            }
            A2aAction::Accept { code } => {
                self.state.add_system_message(self.state.locale.format("A2A pairing code captured ({0} chars). Run `deixic-code a2a accept <code>` Persist it with `/a2a accept <code>` in this TUI, or `deixic-code a2a accept <code>` from the CLI.", &[(code.len()).to_string()]));
            }
            A2aAction::Register {
                agent_id,
                public_url,
                heartbeat_only,
            } => {
                let agent_hint = agent_id
                    .as_deref()
                    .map(|value| format!(" for `{value}`"))
                    .unwrap_or_default();
                let url_hint = public_url
                    .as_deref()
                    .map(|value| format!(" at `{value}`"))
                    .unwrap_or_default();
                let command_hint = if heartbeat_only {
                    "deixic-code a2a register --heartbeat-only --agent-id <id>"
                } else {
                    "deixic-code a2a register --url <base-url> [--agent-id <id>]"
                };
                let action_hint = if heartbeat_only {
                    self.state.locale.translate("refresh the existing Platform heartbeat without requiring a public A2A URL")
                } else {
                    self.state.locale.translate("publish the Rust Agent Card, Codex subagent lanes, and heartbeat to Agent Registry")
                };
                self.state.add_system_message(self.state.locale.format(
                    "Platform A2A peer registration prepared{0}{1}. Run `{2}` to {3}.",
                    &[
                        (agent_hint).clone(),
                        (url_hint).clone(),
                        (command_hint).to_string(),
                        (action_hint).to_string(),
                    ],
                ));
            }
            A2aAction::Delegate { peer, text } => {
                self.state.add_system_message(self.state.locale.format("A2A delegation prepared for `{0}` ({1} chars). Run `deixic-code a2a delegate {2} <text> --wait` while the Rust delegation controller is connected to the shared A2A client.", &[(peer).clone(), (text.len()).to_string(), (peer).clone()]));
            }
            A2aAction::Reply {
                peer,
                task_id,
                text,
            } => {
                self.state.add_system_message(self.state.locale.format("A2A task reply prepared for `{0}` task `{1}` ({2} chars). Run `deixic-code a2a reply {3} {4} <text> --wait` while the Rust task-continuation controller is connected to the shared A2A client.", &[(peer).clone(), (task_id).clone(), (text.len()).to_string(), (peer).clone(), (task_id).clone()]));
            }
            A2aAction::Send { peer, text } => {
                self.state.add_system_message(self.state.locale.format("A2A send request prepared for `{0}` ({1} chars). Run `deixic-code a2a send {2} <text> --wait` while the Rust send controller is connected to the shared A2A client.", &[(peer).clone(), (text.len()).to_string(), (peer).clone()]));
            }
            A2aAction::Handoff {
                peer,
                text,
                computer_package,
            } => {
                self.handle_live_handoff(peer, text, computer_package);
            }
        }
    }

    /// Handle hooks management actions
    pub(super) async fn handle_hooks_action(&mut self, action: crate::commands::HooksAction) {
        use crate::commands::HooksAction;

        let cwd = self.state.cwd.as_deref().unwrap_or(".");
        let snapshot = || crate::cli_commands::effective_hooks_snapshot(std::path::Path::new(cwd));
        let set_enabled = |enabled| {
            self.native_agent
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("The native agent is not running"))?
                .set_hooks_enabled(enabled)
        };
        match action {
            HooksAction::List => match snapshot() {
                Ok(snapshot) => {
                    let mut message = crate::cli_commands::format_effective_hooks(&snapshot);
                    if let Some(agent) = &self.native_agent {
                        match agent.inspect_hooks().await {
                            Ok(runtime) => message.push_str(&format!(
                                "Runtime: {} (native={}, lua={}, wasm={})\n",
                                if runtime.enabled { "enabled" } else { "disabled" },
                                runtime.native_hooks,
                                runtime.lua_scripts,
                                runtime.wasm_plugins,
                            )),
                            Err(error) => {
                                message.push_str(&format!("Runtime: unavailable ({error})\n"));
                            }
                        }
                    }
                    self.state.add_system_message(message);
                }
                Err(error) => self.state.error = Some(format!("Failed to load hooks: {error}")),
            },
            HooksAction::Metrics => match &self.native_agent {
                Some(agent) => match agent.inspect_hooks().await {
                    Ok(runtime) => self.state.add_system_message(self.state.locale.format(
                        "Hook metrics\nPreToolUse: {0}\nPostToolUse: {1}\nOverflow: {2}\nBlocks: {3}\nTotal duration: {4}ms\n",
                        &[
                            runtime.pre_tool_use_count.to_string(),
                            runtime.post_tool_use_count.to_string(),
                            runtime.overflow_count.to_string(),
                            runtime.blocks.to_string(),
                            runtime.total_duration_ms.to_string(),
                        ],
                    )),
                    Err(error) => self.state.error = Some(error.to_string()),
                },
                None => self.state.error = Some("The native agent is not running".to_string()),
            },
            HooksAction::Toggle => {
                let enabled = match &self.native_agent {
                    Some(agent) => agent.inspect_hooks().await.map(|runtime| runtime.enabled),
                    None => Err(anyhow::anyhow!("The native agent is not running")),
                };
                let enabled = match enabled {
                    Ok(enabled) => enabled,
                    Err(error) => {
                        self.state.error = Some(error.to_string());
                        return;
                    }
                };
                match set_enabled(!enabled) {
                    Ok(()) => {
                        self.state.status = Some(format!(
                            "Hooks {}",
                            if enabled { "disabled" } else { "enabled" }
                        ));
                    }
                    Err(error) => self.state.error = Some(error.to_string()),
                }
            }
            HooksAction::Reload => {
                match self
                    .native_agent
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("The native agent is not running"))
                    .and_then(|agent| agent.reload_hooks())
                {
                    Ok(()) => self.state.status = Some("Hook reload queued".to_string()),
                    Err(error) => self.state.error = Some(error.to_string()),
                }
            }
            HooksAction::Enable => match set_enabled(true) {
                Ok(()) => self.state.status = Some("Hooks enabled".to_string()),
                Err(error) => self.state.error = Some(error.to_string()),
            },
            HooksAction::Disable => match set_enabled(false) {
                Ok(()) => self.state.status = Some("Hooks disabled".to_string()),
                Err(error) => self.state.error = Some(error.to_string()),
            },
        }
    }

    /// Invoke a skill as a slash command (`/skillname args`).
    pub(super) async fn handle_invoke_skill(&mut self, name: &str, args: &str) {
        let id = match self.resolve_skill_id(name) {
            Ok(id) => id,
            Err(err) => {
                self.state.error = Some(err);
                return;
            }
        };
        let Some(loaded) = self.find_loaded_skill(&id) else {
            self.state.error = Some(
                self.state
                    .locale
                    .format("Skill '{0}' not found", &[(name).to_string()]),
            );
            return;
        };
        if !loaded.definition.user_invocable {
            self.state.error = Some(self.state.locale.format(
                "Skill '{0}' is not user-invocable (set user-invocable: true)",
                &[(name).to_string()],
            ));
            return;
        }
        let content = Self::format_skill_invoke(loaded, args);
        let _ = self.skill_registry.activate(&id);
        self.update_agent_system_prompt();
        let _ = self.submit_prompt(content).await;
    }

    /// Invoke a flat markdown prompt/command template as a slash command.
    pub(super) async fn handle_invoke_prompt_template(&mut self, name: &str, args: &str) {
        let Some(prompt) = crate::prompts::find_prompt(&self.custom_prompts, name) else {
            self.state.error = Some(
                self.state
                    .locale
                    .format("Prompt template '{0}' not found", &[(name).to_string()]),
            );
            return;
        };
        match crate::prompts::format_prompt_invoke(prompt, args) {
            Ok(content) => {
                let _ = self.submit_prompt(content).await;
            }
            Err(err) => {
                self.state.error = Some(err);
            }
        }
    }

    /// Handle skills system actions
    pub(super) fn handle_skills_action(&mut self, action: crate::commands::SkillsAction) {
        use crate::commands::SkillsAction;

        match action {
            SkillsAction::List => {
                let mut msg = String::from(self.state.locale.translate("## Available Skills\n\n"));
                let cwd = self
                    .state
                    .cwd
                    .as_deref()
                    .map(std::path::Path::new)
                    .unwrap_or_else(|| std::path::Path::new("."));
                if !crate::config::workspace_trusted_in_global_config(cwd) {
                    msg.push_str(
                        self.state.locale.translate("*Workspace is untrusted — project skill dirs are skipped. Use `/trust grant` to enable them.*\n\n"),
                    );
                }
                if self.loaded_skills.is_empty() && self.skill_load_errors.is_empty() {
                    msg.push_str(self.state.locale.translate("*No skills found*\n\n"));
                    msg.push_str(self.state.locale.translate("Skills are loaded from:\n"));
                    msg.push_str("- `~/.composer/skills/` (global)\n");
                    msg.push_str("- `.composer/skills/` (project)\n\n");
                    msg.push_str(self.state.locale.translate("Create a `SKILL.md` file following the [Agent Skills spec](https://agentskills.io/specification).\n"));
                } else {
                    msg.push_str("| Name | Description | Source | Active | Tools |\n");
                    msg.push_str("|------|-------------|--------|--------|-------|\n");

                    for loaded in &self.loaded_skills {
                        let skill = &loaded.definition;
                        let tools_count = skill.provided_tools.len();
                        let tools = if tools_count > 0 {
                            format!("{tools_count}")
                        } else {
                            "-".to_string()
                        };
                        let active = self
                            .skill_registry
                            .get(&skill.id)
                            .map(|s| s.is_active())
                            .unwrap_or(false);
                        let active_label = if active { "yes" } else { "no" };
                        msg.push_str(&format!(
                            "| {} | {} | {:?} | {} | {} |\n",
                            skill.name,
                            skill.description.chars().take(40).collect::<String>(),
                            skill.source,
                            active_label,
                            tools
                        ));
                    }

                    msg.push_str(&format!(
                        "\n*{} skill(s) found*\n",
                        self.loaded_skills.len()
                    ));
                    let active_ids: Vec<String> = self
                        .skill_registry
                        .active_skills()
                        .iter()
                        .map(|skill| skill.definition.name.clone())
                        .collect();
                    if !active_ids.is_empty() {
                        msg.push_str(
                            &self
                                .state
                                .locale
                                .format("Active: {0}\n", &[(active_ids.join(", ")).clone()]),
                        );
                    }
                }

                if !self.skill_load_errors.is_empty() {
                    msg.push_str(&self.state.locale.format(
                        "\n**{0} error(s) loading skills:**\n",
                        &[(self.skill_load_errors.len()).to_string()],
                    ));
                    for err in self.skill_load_errors.iter().take(5) {
                        msg.push_str(&format!("- {err}\n"));
                    }
                }

                self.state.add_system_message(msg);
            }
            SkillsAction::Activate(name) => {
                let id = match self.resolve_skill_id(&name) {
                    Ok(id) => id,
                    Err(err) => {
                        self.state.error = Some(err);
                        return;
                    }
                };
                let skill = match self.skill_registry.get(&id) {
                    Some(skill) => skill,
                    None => {
                        self.state.error = Some(
                            self.state
                                .locale
                                .format("Skill '{0}' not found", std::slice::from_ref(&(name))),
                        );
                        return;
                    }
                };
                let skill_name = skill.definition.name.clone();
                if skill.is_active() {
                    self.state.status = Some(self.state.locale.format(
                        "Skill '{0}' already active",
                        std::slice::from_ref(&(skill_name)),
                    ));
                    return;
                }
                if let Err(err) = self.skill_registry.activate(&id) {
                    self.state.error = Some(err);
                    return;
                }
                self.update_agent_system_prompt();
                self.state.status = Some(
                    self.state
                        .locale
                        .format("Activated skill '{0}'", std::slice::from_ref(&(skill_name))),
                );
                self.state.add_system_message(self.state.locale.format(
                    "Activated skill **{0}**. System prompt updated.",
                    std::slice::from_ref(&(skill_name)),
                ));
            }
            SkillsAction::Deactivate(name) => {
                let id = match self.resolve_skill_id(&name) {
                    Ok(id) => id,
                    Err(err) => {
                        self.state.error = Some(err);
                        return;
                    }
                };
                let skill = match self.skill_registry.get(&id) {
                    Some(skill) => skill,
                    None => {
                        self.state.error = Some(
                            self.state
                                .locale
                                .format("Skill '{0}' not found", std::slice::from_ref(&(name))),
                        );
                        return;
                    }
                };
                let skill_name = skill.definition.name.clone();
                if !skill.is_active() {
                    self.state.status = Some(self.state.locale.format(
                        "Skill '{0}' not active",
                        std::slice::from_ref(&(skill_name)),
                    ));
                    return;
                }
                if let Err(err) = self.skill_registry.deactivate(&id) {
                    self.state.error = Some(err);
                    return;
                }
                self.update_agent_system_prompt();
                self.state.status = Some(self.state.locale.format(
                    "Deactivated skill '{0}'",
                    std::slice::from_ref(&(skill_name)),
                ));
                self.state.add_system_message(self.state.locale.format(
                    "Deactivated skill **{0}**. System prompt updated.",
                    std::slice::from_ref(&(skill_name)),
                ));
            }
            SkillsAction::Reload => {
                self.refresh_skills(true);
                self.update_agent_system_prompt();
                if self.skill_load_errors.is_empty() {
                    self.state.status.replace(self.state.locale.format(
                        "Loaded {0} skill(s)",
                        &[(self.loaded_skills.len()).to_string()],
                    ));
                } else {
                    self.state.status.replace(self.state.locale.format(
                        "Loaded {0} skill(s), {1} error(s)",
                        &[
                            (self.loaded_skills.len()).to_string(),
                            (self.skill_load_errors.len()).to_string(),
                        ],
                    ));
                }
                let mut msg = self.state.locale.format(
                    "Reloaded skills from filesystem. Found {0} skill(s).",
                    &[(self.loaded_skills.len()).to_string()],
                );
                if !self.skill_load_errors.is_empty() {
                    msg.push_str(self.state.locale.translate("\n\nErrors:\n"));
                    for err in self.skill_load_errors.iter().take(5) {
                        msg.push_str(&format!("- {err}\n"));
                    }
                }
                self.state.add_system_message(msg);
            }
            SkillsAction::Info(name) => {
                let id = match self.resolve_skill_id(&name) {
                    Ok(id) => id,
                    Err(err) => {
                        self.state.error = Some(err);
                        return;
                    }
                };
                if let Some(loaded) = self.find_loaded_skill(&id) {
                    let skill = &loaded.definition;
                    let mut msg = self
                        .state
                        .locale
                        .format("## Skill: {0}\n\n", std::slice::from_ref(&(skill.name)));
                    msg.push_str(&self.state.locale.format(
                        "**Description:** {0}\n\n",
                        std::slice::from_ref(&(skill.description)),
                    ));
                    let active = self
                        .skill_registry
                        .get(&skill.id)
                        .map(|s| s.is_active())
                        .unwrap_or(false);
                    msg.push_str(&self.state.locale.format(
                        "**Status:** {0}\n\n",
                        &[(if active { "active" } else { "inactive" }).to_string()],
                    ));
                    msg.push_str(
                        &self
                            .state
                            .locale
                            .format("**Source:** {0}\n\n", &[format!("{:?}", skill.source)]),
                    );
                    msg.push_str(&format!("**Path:** `{}`\n\n", loaded.source_path.display()));

                    if !skill.provided_tools.is_empty() {
                        msg.push_str(&self.state.locale.format(
                            "**Tools:** {0}\n\n",
                            &[(skill.provided_tools.join(", ")).clone()],
                        ));
                    }

                    if !skill.trigger_patterns.is_empty() {
                        msg.push_str(&format!(
                            "**Triggers:** {}\n\n",
                            skill.trigger_patterns.join(", ")
                        ));
                    }

                    if let Some(ref prompt) = skill.system_prompt_additions {
                        let preview: String = prompt.chars().take(200).collect();
                        msg.push_str(&self.state.locale.format(
                            "**Instructions preview:**\n```\n{0}...\n```\n",
                            std::slice::from_ref(&(preview)),
                        ));
                    }

                    self.state.add_system_message(msg);
                } else {
                    self.state.error = Some(
                        self.state
                            .locale
                            .format("Skill '{0}' not found", std::slice::from_ref(&(name))),
                    );
                }
            }
        }
    }

    /// Handle plugin discovery actions (`/plugins`).
    pub(super) fn handle_plugins_action(&mut self, action: crate::commands::PluginsAction) {
        use crate::commands::PluginsAction;

        match action {
            PluginsAction::List => {
                let mut report = crate::localization::with_locale(self.state.locale, || {
                    self.plugin_registry.list_report()
                });
                let cwd = self
                    .state
                    .cwd
                    .as_deref()
                    .map(std::path::Path::new)
                    .unwrap_or_else(|| std::path::Path::new("."));
                if !crate::config::workspace_trusted_in_global_config(cwd) {
                    report = self.state.locale.format("*Workspace is untrusted — project plugin roots are skipped. Use `/trust grant` to enable them.*\n\n{0}", &[(report).clone()]);
                }
                if let Some(skip) = self.plugin_registry.untrusted_skip_notice() {
                    report.push_str("\n\n");
                    report.push_str(&skip);
                }
                self.state.add_system_message(report);
            }
            PluginsAction::Info(name) => match self.plugin_registry.get(&name) {
                Some(plugin) => {
                    self.state
                        .add_system_message(crate::localization::with_locale(
                            self.state.locale,
                            || plugin.detail_report(),
                        ));
                }
                None => {
                    self.state.error = Some(self.state.locale.format(
                        "Plugin '{0}' not found. Use `/plugins` to list discovered plugins.",
                        std::slice::from_ref(&(name)),
                    ));
                }
            },
            PluginsAction::Reload => {
                // Rediscover plugins and reload skills that may come from them.
                self.refresh_skills(true);
                let count = self.plugin_registry.len();
                self.state.status = Some(
                    self.state
                        .locale
                        .format("Discovered {0} plugin(s)", &[(count).to_string()]),
                );
                self.state.add_system_message(
                    self.state.locale.format(
                        "Reloaded plugins from filesystem. Found {0} plugin(s).\n\n{1}",
                        &[
                            (count).to_string(),
                            (crate::localization::with_locale(self.state.locale, || {
                                self.plugin_registry.list_report()
                            }))
                            .clone(),
                        ],
                    ),
                );
            }
            PluginsAction::MarketplaceList => {
                let mut catalog = crate::plugins::builtin_catalog();
                // Only this built-in display copy is translated; third-party manifests stay verbatim.
                for entry in &mut catalog {
                    entry.description = self.state.locale.translate(&entry.description).to_owned();
                }
                let installed: std::collections::HashSet<String> = self
                    .plugin_registry
                    .plugins()
                    .iter()
                    .map(|p| p.name.clone())
                    .collect();
                self.state
                    .add_system_message(crate::localization::with_locale(
                        self.state.locale,
                        || crate::plugins::format_catalog(&catalog, &installed),
                    ));
            }
            PluginsAction::MarketplaceInstall { id, trust } => {
                let catalog = crate::plugins::builtin_catalog();
                let Some(entry) = crate::plugins::find_entry(&catalog, &id) else {
                    self.state.error = Some(self.state.locale.format(
                        "Marketplace entry '{0}' not found. Use `/plugins marketplace list`.",
                        std::slice::from_ref(&(id)),
                    ));
                    return;
                };
                if entry.tier.requires_explicit_trust() && !trust {
                    self.state.error = Some(self.state.locale.format(
                        "Entry '{0}' ({1}) requires explicit trust. Re-run with `--trust`.",
                        &[(entry.id).clone(), (entry.tier.as_str()).to_string()],
                    ));
                    return;
                }
                let source = match crate::plugins::resolve_install_source(entry) {
                    Ok(s) => s,
                    Err(e) => {
                        self.state.error = Some(self.state.locale.format(
                            "Cannot install '{0}': {1}",
                            &[(entry.id).clone(), (e).to_string()],
                        ));
                        return;
                    }
                };
                let home = match crate::path_utils::maestro_home_dir() {
                    Some(h) => h,
                    None => {
                        self.state.error = Some(
                            self.state
                                .locale
                                .translate("Could not resolve ~/.maestro")
                                .to_string(),
                        );
                        return;
                    }
                };
                match crate::plugins::install_with_provenance(
                    &source,
                    &home.join("plugins"),
                    &home.join("plugin-state.json"),
                    trust || !entry.tier.requires_explicit_trust(),
                    Some(crate::plugins::InstallProvenance {
                        marketplace_id: Some(entry.id.clone()),
                        marketplace_tier: Some(entry.tier.as_str().to_string()),
                    }),
                ) {
                    Ok(preview) => {
                        self.refresh_skills(true);
                        self.state.add_system_message(self.state.locale.format("Installed marketplace plugin **{0}** (`{1}`, {2}) from `{3}`.\nCapabilities: {4}\n\nUse `/plugins list` to verify. Provenance written to plugin-state.json.", &[(preview.name).clone(), (entry.id).clone(), (entry.tier.as_str()).to_string(), (preview.source).clone(), format!("{:?}", preview.capabilities)]));
                        self.state.status = Some(self.state.locale.format(
                            "Installed plugin {0}",
                            std::slice::from_ref(&(preview.name)),
                        ));
                    }
                    Err(e) => {
                        self.state.error = Some(
                            self.state
                                .locale
                                .format("Install failed: {0}", &[(e).to_string()]),
                        );
                    }
                }
            }
        }
    }

    /// Handle `/attach` add|list|clear|remove.
    pub(super) fn handle_attach_action(&mut self, action: AttachAction) {
        match action {
            AttachAction::List => {
                if self.pending_attachments.is_empty() {
                    self.state.add_system_message(
                        self.state
                            .locale
                            .translate("No pending attachments. Use `/attach <path>`.")
                            .into(),
                    );
                } else {
                    let mut msg = self.state.locale.format(
                        "## Pending attachments ({0})\n\n",
                        &[(self.pending_attachments.len()).to_string()],
                    );
                    for (i, path) in self.pending_attachments.iter().enumerate() {
                        msg.push_str(&format!("{}. `{path}`\n", i + 1));
                    }
                    msg.push_str(
                        self.state.locale.translate("\nSent with the next user prompt. `/attach clear` drops all; `/attach remove <n>` drops one (1-based).\n"),
                    );
                    self.state.add_system_message(msg);
                }
            }
            AttachAction::Clear => {
                let n = self.pending_attachments.len();
                self.pending_attachments.clear();
                self.state.status = Some(
                    self.state
                        .locale
                        .format("Cleared {0} attachment(s)", &[(n).to_string()]),
                );
                self.state.add_system_message(
                    self.state
                        .locale
                        .format("Cleared {0} pending attachment(s).", &[(n).to_string()]),
                );
            }
            AttachAction::Remove { index } => {
                if index == 0 || index > self.pending_attachments.len() {
                    self.state.error = Some(self.state.locale.format(
                        "No attachment at index {0}. Use `/attach list` (1-based indices).",
                        &[(index).to_string()],
                    ));
                } else {
                    let removed = self.pending_attachments.remove(index - 1);
                    let n = self.pending_attachments.len();
                    self.state.status = Some(format!("Detached #{index}; {n} remaining"));
                    self.state.add_system_message(self.state.locale.format(
                        "Removed attachment #{0}: `{1}` ({2} remaining).",
                        &[(index).to_string(), (removed).clone(), (n).to_string()],
                    ));
                }
            }
            AttachAction::Add(path) => {
                let expanded = if let Some(rest) = path.strip_prefix("~/") {
                    dirs::home_dir()
                        .map(|h| h.join(rest).display().to_string())
                        .unwrap_or(path.clone())
                } else {
                    path.clone()
                };
                let p = std::path::Path::new(&expanded);
                if !p.exists() {
                    self.state.error = Some(self.state.locale.format(
                        "Attach path does not exist: {0}",
                        std::slice::from_ref(&(expanded)),
                    ));
                } else {
                    self.pending_attachments.push(expanded.clone());
                    let n = self.pending_attachments.len();
                    self.state.status = Some(format!("Attached ({n}): {expanded}"));
                    self.state.add_system_message(self.state.locale.format(
                        "Attached `{0}`. It will be sent with the next user prompt ({1} pending).",
                        &[(expanded).clone(), (n).to_string()],
                    ));
                }
            }
        }
    }

    /// Handle `/goal` lifecycle actions.
    pub(super) fn handle_goal_action(&mut self, action: GoalAction) {
        let previous_tools_visible = self.goal_store.tools_visible();
        match action {
            GoalAction::Status => {
                self.state
                    .add_system_message(crate::localization::with_locale(
                        self.state.locale,
                        || self.goal_store.report(),
                    ));
            }
            GoalAction::Create {
                text,
                replace,
                criteria,
                max_turns,
                token_budget,
                max_duration_secs,
            } => match self.goal_store.create_with_limits(
                text,
                criteria,
                replace,
                max_turns,
                token_budget,
                max_duration_secs,
            ) {
                Ok(goal) => {
                    // Arm only when an agent can actually take a turn. Without
                    // an agent, arming would immediately burn auto_continue
                    // count (bugbash: n=1 after create with no API key).
                    self.goal_auto_continue_armed =
                        goal.auto_continue && self.native_agent.is_some();
                    let budget = match goal.token_budget {
                        Some(b) => self
                            .state
                            .locale
                            .format("Token budget: {0}. ", &[(b).to_string()]),
                        None => String::new(),
                    };
                    let time_budget = match goal.max_duration_secs {
                        Some(seconds) => self
                            .state
                            .locale
                            .format("Wall-clock budget: {0}s. ", &[(seconds).to_string()]),
                        None => String::new(),
                    };
                    let kickoff = if self.native_agent.is_some() {
                        self.state
                            .locale
                            .translate("Auto-continue will inject a kickoff turn while idle.")
                    } else {
                        self.state.locale.translate("Agent is not available yet — send a prompt after login, or restart with credentials; auto-continue will arm after a successful worker turn.")
                    };
                    self.state.add_system_message(self.state.locale.format("Goal {0} set (**{1}**).\n\n{2}\n\nAuto-continue (Codex-style): after each turn, if the goal is still active the TUI injects a continuation prompt. Mark done with the **`update_goal`** tool (`complete` or `blocked`) — same model. {3}{4}Safety max_turns={5}. `/goal pause` stops. Skipped while `/loop` or queue is active. Goal tools are hidden from the model when no goal exists.\n\n{6}", &[(goal.id).clone(), (goal.status.as_str()).to_string(), (goal.text).clone(), (budget).clone(), (time_budget).clone(), (goal.max_turns).to_string(), (kickoff).to_string()]));
                    self.state.status = Some(
                        self.state
                            .locale
                            .format("Goal {0}", std::slice::from_ref(&(goal.id))),
                    );
                }
                Err(e) => self.state.error = Some(e.to_string()),
            },
            GoalAction::Pause => match self.goal_store.pause() {
                Ok(goal) => {
                    self.goal_auto_continue_armed = false;
                    self.state.status.replace(
                        self.state
                            .locale
                            .format("Goal {0} paused", std::slice::from_ref(&goal.id)),
                    );
                    self.state.add_system_message(
                        self.state
                            .locale
                            .format("Goal {0} paused.", std::slice::from_ref(&(goal.id))),
                    );
                }
                Err(e) => self.state.error = Some(e.to_string()),
            },
            GoalAction::Resume => match self.goal_store.resume() {
                Ok(goal) => {
                    self.goal_auto_continue_armed = true;
                    self.state.status.replace(
                        self.state
                            .locale
                            .format("Goal {0} resumed", std::slice::from_ref(&goal.id)),
                    );
                    self.state.add_system_message(self.state.locale.format(
                        "Goal {0} resumed (auto-continue on).",
                        std::slice::from_ref(&(goal.id)),
                    ));
                }
                Err(e) => self.state.error = Some(e.to_string()),
            },
            GoalAction::Block { reason } => match self.goal_store.block(reason) {
                Ok(goal) => {
                    self.goal_auto_continue_armed = false;
                    let msg = match &goal.block_reason {
                        Some(r) => self
                            .state
                            .locale
                            .format("Goal {0} blocked: {1}", &[(goal.id).clone(), (r).clone()]),
                        None => self
                            .state
                            .locale
                            .format("Goal {0} blocked.", std::slice::from_ref(&(goal.id))),
                    };
                    self.state.status.replace(msg.clone());
                    self.state.add_system_message(msg);
                }
                Err(e) => self.state.error = Some(e.to_string()),
            },
            GoalAction::Complete => match self.goal_store.complete() {
                Ok(done) => {
                    self.goal_auto_continue_armed = false;
                    self.state.status.replace(
                        self.state
                            .locale
                            .format("Goal {0} complete", std::slice::from_ref(&done.id)),
                    );
                    self.state.add_system_message(self.state.locale.format(
                        "Goal {0} marked complete.\n\n{1}",
                        &[(done.id).clone(), (done.text).clone()],
                    ));
                }
                Err(e) => self.state.error = Some(e.to_string()),
            },
            GoalAction::Clear => match self.goal_store.clear() {
                Ok(Some(prev)) => {
                    self.goal_auto_continue_armed = false;
                    self.state.status.replace(
                        self.state
                            .locale
                            .format("Goal {0} cleared", std::slice::from_ref(&prev.id)),
                    );
                    self.state.add_system_message(
                        self.state
                            .locale
                            .format("Cleared goal {0}.", std::slice::from_ref(&(prev.id))),
                    );
                }
                Ok(None) => {
                    self.state.status =
                        Some(self.state.locale.translate("No goal to clear").to_string());
                }
                Err(e) => self.state.error = Some(e.to_string()),
            },
            GoalAction::AutoContinue { enabled } => {
                match self.goal_store.set_auto_continue(enabled) {
                    Ok(goal) => {
                        self.goal_auto_continue_armed = enabled && goal.status.as_str() == "active";
                        let label = if enabled {
                            self.state.locale.translate("on")
                        } else {
                            self.state.locale.translate("off")
                        };
                        self.state.status.replace(
                            self.state
                                .locale
                                .format("Goal auto-continue {0}", &[(label).to_string()]),
                        );
                        self.state.add_system_message(self.state.locale.format(
                            "Goal {0} auto-continue: {1}.",
                            &[(goal.id).clone(), (label).to_string()],
                        ));
                    }
                    Err(e) => self.state.error = Some(e.to_string()),
                }
            }
        }
        if previous_tools_visible != self.goal_store.tools_visible() {
            self.sync_agent_goal_tools_visibility();
        }
    }

    /// Handle `/harness` and `/refine` continual-context actions.
    pub(super) fn handle_harness_action(&mut self, action: HarnessAction) {
        let workspace = self.state.cwd.clone().unwrap_or_else(|| ".".to_string());
        let workspace = std::path::Path::new(&workspace);
        let session_id = self.state.session_id.clone();
        let session_id = session_id.as_deref();

        match action {
            HarnessAction::Status => {
                self.state
                    .add_system_message(crate::localization::with_locale(
                        self.state.locale,
                        || self.harness_store.report(workspace, session_id),
                    ));
            }
            HarnessAction::List => {
                self.state
                    .add_system_message(crate::localization::with_locale(
                        self.state.locale,
                        || self.harness_store.list_report(workspace, session_id),
                    ));
            }
            HarnessAction::Review => {
                self.state
                    .add_system_message(crate::localization::with_locale(
                        self.state.locale,
                        || self.harness_store.proposal_report(),
                    ));
            }
            HarnessAction::Propose {
                scope,
                kind,
                name,
                content,
                evidence,
            } => {
                let scope = match crate::harness::HarnessScope::parse(&scope) {
                    Ok(scope) => scope,
                    Err(error) => {
                        self.state.error =
                            Some(self.state.locale.format(
                                "Refinement proposal failed: {0}",
                                &[format!("{:#}", error)],
                            ));
                        return;
                    }
                };
                let kind = match crate::harness::HarnessKind::parse(&kind) {
                    Ok(kind) => kind,
                    Err(error) => {
                        self.state.error =
                            Some(self.state.locale.format(
                                "Refinement proposal failed: {0}",
                                &[format!("{:#}", error)],
                            ));
                        return;
                    }
                };
                let scope_key =
                    match crate::harness::HarnessStore::scope_key(scope, workspace, session_id) {
                        Ok(scope_key) => scope_key,
                        Err(error) => {
                            self.state.error = Some(self.state.locale.format(
                                "Refinement proposal failed: {0}",
                                &[format!("{:#}", error)],
                            ));
                            return;
                        }
                    };
                let proposal_name = name.clone();
                match self
                    .harness_store
                    .propose(kind, scope, scope_key, name, content, evidence)
                {
                    Ok(id) => {
                        self.state.status = Some(self.state.locale.format(
                            "Refinement proposal {0} staged",
                            std::slice::from_ref(&(id)),
                        ));
                        self.state.add_system_message(self.state.locale.format("Staged refinement proposal `{0}` for {1} harness entry `{2}`. Use `/refine review`, then `/refine apply {3}`.", &[(id).clone(), (kind.as_str()).to_string(), (proposal_name).clone(), (id).clone()]));
                    }
                    Err(error) => {
                        self.state.error =
                            Some(self.state.locale.format(
                                "Refinement proposal failed: {0}",
                                &[format!("{:#}", error)],
                            ));
                    }
                }
            }
            HarnessAction::Add {
                scope,
                kind,
                name,
                content,
                evidence,
            } => {
                let scope = match crate::harness::HarnessScope::parse(&scope) {
                    Ok(scope) => scope,
                    Err(error) => {
                        self.state.error = Some(
                            self.state
                                .locale
                                .format("Harness add failed: {0}", &[format!("{:#}", error)]),
                        );
                        return;
                    }
                };
                let kind = match crate::harness::HarnessKind::parse(&kind) {
                    Ok(kind) => kind,
                    Err(error) => {
                        self.state.error = Some(
                            self.state
                                .locale
                                .format("Harness add failed: {0}", &[format!("{:#}", error)]),
                        );
                        return;
                    }
                };
                let scope_key =
                    match crate::harness::HarnessStore::scope_key(scope, workspace, session_id) {
                        Ok(scope_key) => scope_key,
                        Err(error) => {
                            self.state.error = Some(
                                self.state
                                    .locale
                                    .format("Harness add failed: {0}", &[format!("{:#}", error)]),
                            );
                            return;
                        }
                    };
                let entry_name = name.clone();
                match self
                    .harness_store
                    .add(kind, scope, scope_key, name, content, evidence)
                {
                    Ok(id) => {
                        self.update_agent_system_prompt();
                        self.state.status = Some(
                            self.state
                                .locale
                                .format("Harness entry {0} added", std::slice::from_ref(&(id))),
                        );
                        self.state.add_system_message(self.state.locale.format(
                            "Added {0} harness entry `{1}` in {2} scope as `{3}`.",
                            &[
                                (kind.as_str()).to_string(),
                                (entry_name).clone(),
                                (scope.as_str()).to_string(),
                                (id).clone(),
                            ],
                        ));
                    }
                    Err(error) => {
                        self.state.error = Some(
                            self.state
                                .locale
                                .format("Harness add failed: {0}", &[format!("{:#}", error)]),
                        );
                    }
                }
            }
            HarnessAction::Update {
                id,
                content,
                evidence,
            } => match self.harness_store.update(&id, content, evidence) {
                Ok(()) => {
                    self.update_agent_system_prompt();
                    self.state.status = Some(
                        self.state
                            .locale
                            .format("Harness entry {0} updated", std::slice::from_ref(&(id))),
                    );
                    self.state.add_system_message(
                        self.state
                            .locale
                            .format("Updated harness entry `{0}`.", std::slice::from_ref(&(id))),
                    );
                }
                Err(error) => {
                    self.state.error = Some(
                        self.state
                            .locale
                            .format("Harness update failed: {0}", &[format!("{:#}", error)]),
                    );
                }
            },
            HarnessAction::Delete(id) => match self.harness_store.delete(&id) {
                Ok(()) => {
                    self.update_agent_system_prompt();
                    self.state.status = Some(
                        self.state
                            .locale
                            .format("Harness entry {0} deleted", std::slice::from_ref(&(id))),
                    );
                    self.state.add_system_message(
                        self.state
                            .locale
                            .format("Deleted harness entry `{0}`.", std::slice::from_ref(&(id))),
                    );
                }
                Err(error) => {
                    self.state.error = Some(
                        self.state
                            .locale
                            .format("Harness delete failed: {0}", &[format!("{:#}", error)]),
                    );
                }
            },
            HarnessAction::Rollback(revision) => match self.harness_store.rollback(revision) {
                Ok(()) => {
                    self.update_agent_system_prompt();
                    self.state.status = Some(self.state.locale.format(
                        "Harness rolled back to revision {0}",
                        &[(revision).to_string()],
                    ));
                    self.state.add_system_message(self.state.locale.format(
                        "Restored harness snapshot at revision {0}.",
                        &[(revision).to_string()],
                    ));
                }
                Err(error) => {
                    self.state.error = Some(
                        self.state
                            .locale
                            .format("Harness rollback failed: {0}", &[format!("{:#}", error)]),
                    );
                }
            },
            HarnessAction::Apply(id) => match self.harness_store.apply_proposal(&id) {
                Ok(entry_id) => {
                    self.update_agent_system_prompt();
                    self.state.status = Some(self.state.locale.format(
                        "Refinement proposal {0} applied",
                        std::slice::from_ref(&(id)),
                    ));
                    self.state.add_system_message(self.state.locale.format(
                        "Applied refinement proposal `{0}` to harness entry `{1}`.",
                        &[(id).clone(), (entry_id).clone()],
                    ));
                }
                Err(error) => {
                    self.state.error = Some(
                        self.state
                            .locale
                            .format("Refinement apply failed: {0}", &[format!("{:#}", error)]),
                    );
                }
            },
            HarnessAction::Reject { id, note } => {
                match self.harness_store.reject_proposal(&id, note) {
                    Ok(()) => {
                        self.state.status = Some(self.state.locale.format(
                            "Refinement proposal {0} rejected",
                            std::slice::from_ref(&(id)),
                        ));
                        self.state.add_system_message(self.state.locale.format(
                            "Rejected refinement proposal `{0}`.",
                            std::slice::from_ref(&(id)),
                        ));
                    }
                    Err(error) => {
                        self.state.error = Some(
                            self.state
                                .locale
                                .format("Refinement reject failed: {0}", &[format!("{:#}", error)]),
                        );
                    }
                }
            }
        }
    }

    /// Handle `/rlm` persistent context variable actions.
    pub(super) fn handle_rlm_action(&mut self, action: RlmAction) {
        match action {
            RlmAction::List => self
                .state
                .add_system_message(crate::localization::with_locale(self.state.locale, || {
                    self.rlm_store.report()
                })),
            RlmAction::Set {
                name,
                value,
                description,
            } => match self.rlm_store.set(name.clone(), value, description) {
                Ok(()) => {
                    self.update_agent_system_prompt();
                    self.state.status = Some(
                        self.state
                            .locale
                            .format("RLM variable `{0}` saved", std::slice::from_ref(&(name))),
                    );
                    self.state.add_system_message(
                        self.state
                            .locale
                            .format("Saved RLM variable `{0}`.", std::slice::from_ref(&(name))),
                    );
                }
                Err(error) => {
                    self.state.error = Some(
                        self.state
                            .locale
                            .format("RLM set failed: {0}", &[format!("{:#}", error)]),
                    );
                }
            },
            RlmAction::Append { name, value } => match self.rlm_store.append(name.clone(), value) {
                Ok(()) => {
                    self.update_agent_system_prompt();
                    self.state.status = Some(
                        self.state
                            .locale
                            .format("RLM variable `{0}` appended", std::slice::from_ref(&(name))),
                    );
                    self.state.add_system_message(self.state.locale.format(
                        "Appended to RLM variable `{0}`.",
                        std::slice::from_ref(&(name)),
                    ));
                }
                Err(error) => {
                    self.state.error = Some(
                        self.state
                            .locale
                            .format("RLM append failed: {0}", &[format!("{:#}", error)]),
                    );
                }
            },
            RlmAction::Render(template) => match self.rlm_store.render_template(&template) {
                Ok(rendered) => self.state.add_system_message(self.state.locale.format(
                    "## RLM rendered context\n\n{0}",
                    std::slice::from_ref(&(rendered)),
                )),
                Err(error) => {
                    self.state.error = Some(
                        self.state
                            .locale
                            .format("RLM render failed: {0}", &[format!("{:#}", error)]),
                    );
                }
            },
            RlmAction::Clear(name) => match self.rlm_store.clear(&name) {
                Ok(true) => {
                    self.update_agent_system_prompt();
                    self.state.status = Some(
                        self.state
                            .locale
                            .format("RLM variable `{0}` cleared", std::slice::from_ref(&(name))),
                    );
                    self.state.add_system_message(
                        self.state
                            .locale
                            .format("Cleared RLM variable `{0}`.", std::slice::from_ref(&(name))),
                    );
                }
                Ok(false) => {
                    self.state.status = Some(
                        self.state
                            .locale
                            .format("No RLM variable named `{0}`", std::slice::from_ref(&(name))),
                    );
                }
                Err(error) => {
                    self.state.error = Some(
                        self.state
                            .locale
                            .format("RLM clear failed: {0}", &[format!("{:#}", error)]),
                    );
                }
            },
        }
    }

    /// Handle `/mailbox` durable parent/subagent messages.
    pub(super) fn handle_mailbox_action(&mut self, action: crate::commands::MailboxAction) {
        match action {
            crate::commands::MailboxAction::List => {
                let mut recipients = self.tool_executor.subagent_mailbox_recipients();
                recipients.push(crate::mailbox::local_identity());
                self.state
                    .add_system_message(crate::localization::with_locale(
                        self.state.locale,
                        || self.mailbox_store.report_for_recipients(&recipients),
                    ));
            }
            crate::commands::MailboxAction::Send { recipient, body } => {
                let sender = crate::mailbox::local_identity();
                match self.mailbox_store.send(sender, recipient.clone(), body) {
                    Ok(id) => {
                        self.update_agent_system_prompt();
                        self.state.status = Some(
                            self.state
                                .locale
                                .format("Mailbox message {0} sent", std::slice::from_ref(&(id))),
                        );
                        self.state.add_system_message(self.state.locale.format(
                            "Sent mailbox message `{0}` to `{1}`.",
                            &[(id).clone(), (recipient).clone()],
                        ));
                    }
                    Err(error) => {
                        self.state.error = Some(
                            self.state
                                .locale
                                .format("Mailbox send failed: {0}", &[format!("{:#}", error)]),
                        );
                    }
                }
            }
            crate::commands::MailboxAction::Read(id) => match self
                .mailbox_store
                .read_for(&id, Some(crate::mailbox::local_identity().as_str()))
            {
                Ok(message) => {
                    self.update_agent_system_prompt();
                    self.state.add_system_message(self.state.locale.format(
                        "## Mailbox message `{0}`\n\nFrom: `{1}`\nTo: `{2}`\n\n{3}",
                        &[
                            (message.id).clone(),
                            (message.sender).clone(),
                            (message.recipient).clone(),
                            (message.body).clone(),
                        ],
                    ));
                }
                Err(error) => {
                    self.state.error = Some(
                        self.state
                            .locale
                            .format("Mailbox read failed: {0}", &[format!("{:#}", error)]),
                    );
                }
            },
            crate::commands::MailboxAction::Inspect(id) => {
                match self.tool_executor.inspect_subagent_control(&id) {
                    Ok(message) => {
                        let crate::mailbox::MailboxPayload::SubagentControl { mode } =
                            message.payload
                        else {
                            unreachable!("inspect_subagent_control validates the payload")
                        };
                        let next_action = if message.delivery_state
                            == crate::mailbox::MailboxDeliveryState::Held
                        {
                            self.state.locale.format(
                                "\n\nApprove: `/mailbox approve {0}`",
                                std::slice::from_ref(&(message.id)),
                            )
                        } else {
                            String::new()
                        };
                        self.state.add_system_message(self.state.locale.format("## Subagent mailbox control `{0}`\n\nFrom: `{1}`\nTo: `{2}`\nMode: `{3}`\nState: `{4}`\n\n{5}{6}", &[(message.id).clone(), (message.sender).clone(), (message.recipient).clone(), (mode.label()).to_string(), format!("{:?}", message.delivery_state), (message.body).clone(), (next_action).clone()]));
                    }
                    Err(error) => {
                        self.state.error = Some(self.state.locale.format(
                            "Mailbox inspection failed: {0}",
                            std::slice::from_ref(&(error)),
                        ));
                    }
                }
            }
            crate::commands::MailboxAction::Acknowledge(id) => {
                match self
                    .mailbox_store
                    .acknowledge_for(&id, Some(crate::mailbox::local_identity().as_str()))
                {
                    Ok(()) => {
                        self.update_agent_system_prompt();
                        self.state.status = Some(self.state.locale.format(
                            "Mailbox message {0} acknowledged",
                            std::slice::from_ref(&(id)),
                        ));
                        self.state.add_system_message(self.state.locale.format(
                            "Acknowledged mailbox message `{0}`.",
                            std::slice::from_ref(&(id)),
                        ));
                    }
                    Err(error) => {
                        self.state.error =
                            Some(self.state.locale.format(
                                "Mailbox acknowledge failed: {0}",
                                &[format!("{:#}", error)],
                            ));
                    }
                }
            }
            crate::commands::MailboxAction::Approve(id) => {
                match self.tool_executor.approve_held_subagent_control(&id) {
                    Ok(message) => {
                        let _ = self.mailbox_store.reload_from_disk();
                        self.state.status =
                            Some(self.state.locale.format(
                                "Mailbox message {0} approved",
                                std::slice::from_ref(&(id)),
                            ));
                        self.state.add_system_message(self.state.locale.format(
                            "Approved held mailbox message `{0}` for delivery to `{1}`.",
                            &[(id).clone(), (message.recipient).clone()],
                        ));
                    }
                    Err(error) => {
                        let _ = self.mailbox_store.reload_from_disk();
                        self.state.error = Some(
                            self.state
                                .locale
                                .format("Mailbox approval failed: {0}", &[format!("{:#}", error)]),
                        );
                    }
                }
            }
            crate::commands::MailboxAction::Compact => match self.mailbox_store.compact() {
                Ok(removed) => {
                    self.update_agent_system_prompt();
                    self.state.add_system_message(self.state.locale.format(
                        "Removed {0} acknowledged mailbox message(s).",
                        &[(removed).to_string()],
                    ));
                }
                Err(error) => {
                    self.state.error = Some(
                        self.state
                            .locale
                            .format("Mailbox compact failed: {0}", &[format!("{:#}", error)]),
                    );
                }
            },
        }
    }

    pub(super) fn handle_queue_action(&mut self, action: QueueAction) {
        match action {
            QueueAction::Show => {
                let total = self.state.queued_prompt_count;
                let steer_count = self.state.queued_steering_count;
                let follow_up_count = self.state.queued_follow_up_count;
                let mut msg = String::new();
                msg.push_str(self.state.locale.translate("## Queue\n\n"));
                msg.push_str(&self.state.locale.format(
                    "**Steering mode:** {0}\n",
                    &[(self.state.steering_mode.label()).to_string()],
                ));
                msg.push_str(&self.state.locale.format(
                    "**Follow-up mode:** {0}\n",
                    &[(self.state.follow_up_mode.label()).to_string()],
                ));
                msg.push_str(
                    &self
                        .state
                        .locale
                        .format("**Pending:** {0}\n", &[(total).to_string()]),
                );
                if total > 0 {
                    msg.push_str(&format!(
                        "- steer: {steer_count}, follow-up: {follow_up_count}\n"
                    ));
                    if let Some(summary) = Self::describe_next_queue_batch(
                        steer_count,
                        self.state.steering_mode,
                        self.state.locale.translate("at the next tool boundary"),
                    ) {
                        msg.push_str(&self.state.locale.format(
                            "**Next steering batch:** {0}\n",
                            std::slice::from_ref(&(summary)),
                        ));
                    }
                    if let Some(summary) = Self::describe_next_queue_batch(
                        follow_up_count,
                        self.state.follow_up_mode,
                        self.state.locale.translate("after turn end"),
                    ) {
                        msg.push_str(&self.state.locale.format(
                            "**Next follow-up batch:** {0}\n",
                            std::slice::from_ref(&(summary)),
                        ));
                    }
                }
                if let Some(active) = &self.queued_prompt_active {
                    msg.push_str(&format!(
                        "\n**Active:** #{} ({}) – {}\n",
                        active.id,
                        active.kind.label(),
                        Self::format_queue_snippet(&active.content, 80)
                    ));
                }
                if self.queued_prompts.is_empty() {
                    msg.push_str(self.state.locale.translate("\nNo queued prompts.\n"));
                } else {
                    msg.push_str(self.state.locale.translate("\n**Pending prompts:**\n"));
                    let inflight_id = self.queued_prompt_inflight.map(|cursor| cursor.id);
                    for (index, prompt) in self.queued_prompts.iter().enumerate() {
                        let marker = if inflight_id == Some(prompt.id) {
                            " (starting...)"
                        } else {
                            ""
                        };
                        msg.push_str(&format!(
                            "{}. #{} ({}){} – {}\n",
                            index + 1,
                            prompt.id,
                            prompt.kind.label(),
                            marker,
                            Self::format_queue_snippet(&prompt.content, 80)
                        ));
                    }
                }
                msg.push_str(
                    self.state.locale.translate("\nUse /queue cancel <id> to remove a prompt, /queue move <id> <up|down> to reorder, or /queue send <id> to send one next. Use /queue mode [steer|followup] <one|all> to change behavior."),
                );
                self.state.add_system_message(msg);
            }
            QueueAction::Cancel { id } => {
                if self
                    .queued_prompt_active
                    .as_ref()
                    .is_some_and(|prompt| prompt.id == id)
                {
                    self.state.status.replace(self.state.locale.format(
                        "Queued prompt #{0} is already processing.",
                        &[(id).to_string()],
                    ));
                    return;
                }
                if self
                    .queued_prompt_inflight
                    .is_some_and(|prompt| prompt.id == id)
                {
                    self.state.status.replace(self.state.locale.format(
                        "Queued prompt #{0} is starting; try again if it re-queues.",
                        &[(id).to_string()],
                    ));
                    return;
                }
                match self.remove_queued_prompt(id) {
                    Some(removed) => {
                        if let Some(agent) = &self.native_agent {
                            agent.cancel_queued(id);
                        }
                        self.state.status.replace(self.state.locale.format(
                            "Removed queued {0} #{1}.",
                            &[
                                (self.state.locale.translate(removed.kind.label())).to_string(),
                                (removed.id).to_string(),
                            ],
                        ));
                    }
                    None => {
                        self.state.status.replace(
                            self.state.locale.format(
                                "No queued prompt found with id #{0}.",
                                &[(id).to_string()],
                            ),
                        );
                    }
                }
            }
            QueueAction::Move { id, direction } => {
                if !self.queued_prompts.iter().any(|prompt| prompt.id == id) {
                    self.state.status.replace(
                        self.state
                            .locale
                            .format("No queued prompt found with id #{0}.", &[(id).to_string()]),
                    );
                    return;
                }
                if self
                    .queued_prompt_active
                    .as_ref()
                    .is_some_and(|prompt| prompt.id == id)
                    || self
                        .queued_prompt_inflight
                        .is_some_and(|prompt| prompt.id == id)
                {
                    self.state.status.replace(self.state.locale.format(
                        "Queued prompt #{0} is already processing.",
                        &[(id).to_string()],
                    ));
                    return;
                }
                if let Some(placement) = self.move_queued_prompt(id, direction) {
                    if let Some(agent) = &self.native_agent {
                        agent.reorder_queued(id, placement);
                    }
                    let action = match direction {
                        QueueMoveDirection::Up => self.state.locale.translate("moved up"),
                        QueueMoveDirection::Down => self.state.locale.translate("moved down"),
                        QueueMoveDirection::Now => self.state.locale.translate("will send next"),
                    };
                    self.state.status.replace(self.state.locale.format(
                        "Queued prompt #{0} {1}.",
                        &[(id).to_string(), (action).to_string()],
                    ));
                } else {
                    self.state.status.replace(self.state.locale.format(
                        "Queued prompt #{0} is already in that position.",
                        &[(id).to_string()],
                    ));
                }
            }
            QueueAction::Mode { kind, mode } => {
                let label = match kind {
                    QueueModeKind::Steering => {
                        self.state.steering_mode = mode;
                        if let Some(agent) = &self.native_agent {
                            let _ = agent.set_steering_mode(mode);
                        }
                        self.state.locale.translate("Steering")
                    }
                    QueueModeKind::FollowUp => {
                        self.state.follow_up_mode = mode;
                        if let Some(agent) = &self.native_agent {
                            let _ = agent.set_follow_up_mode(mode);
                        }
                        "Follow-up"
                    }
                };
                let _ = crate::ui_state::save_queue_modes(
                    self.state.steering_mode,
                    self.state.follow_up_mode,
                );
                self.state.status.replace(self.state.locale.format(
                    "{0} mode: {1}",
                    &[(label).to_string(), (mode.label()).to_string()],
                ));
            }
        }
    }

    /// Execute a slash command
    pub(super) async fn execute_slash_command(&mut self) -> Result<()> {
        let input = self.state.take_input();
        // Normalize leading whitespace so " /goal status" still executes as a
        // command instead of failing registry parse or falling through to the
        // agent (bugbash: slash while busy looked like steering without `/`).
        let input = input.trim().to_string();

        // Expand an unambiguous partial command ("/qui" -> "/quit") or rescue
        // a one-character typo ("/quti" -> "/quit") so that pressing Enter runs
        // the intended command instead of erroring — or worse, forwarding the
        // partial text to the agent as a prompt.
        let without_slash = input.trim_start_matches('/');
        let mut parts = without_slash.splitn(2, char::is_whitespace);
        let typed_word = parts.next().unwrap_or("");
        let typed_args = parts.next().unwrap_or("").trim();
        let word = typed_word.to_lowercase();

        let expand = |name: &str| {
            if typed_args.is_empty() {
                format!("/{name}")
            } else {
                format!("/{name} {typed_args}")
            }
        };

        let input = if !word.is_empty() && self.command_registry.get(&word).is_none() {
            let prefix = self.command_registry.resolve_unique_prefix(&word);
            match prefix {
                Ok(Some(cmd)) => {
                    let expanded = expand(&cmd.name.clone());
                    self.state.status.replace(self.state.locale.format(
                        "Expanded /{0} → /{1}",
                        &[(typed_word).to_string(), (cmd.name).clone()],
                    ));
                    expanded
                }
                Ok(None) => match self.command_registry.resolve_typo(&word) {
                    Ok(Some(cmd)) => {
                        let expanded = expand(&cmd.name.clone());
                        self.state.status.replace(self.state.locale.format(
                            "Interpreted /{0} as /{1}",
                            &[(typed_word).to_string(), (cmd.name).clone()],
                        ));
                        expanded
                    }
                    Ok(None) => input,
                    Err(candidates) => {
                        // Rescuable typo with more than one candidate: restore
                        // the input, open the completion dropdown, and explain.
                        let list = candidates
                            .iter()
                            .map(|name| format!("/{name}"))
                            .collect::<Vec<_>>()
                            .join(", ");
                        self.state.set_input(&input);
                        self.update_slash_state();
                        self.state.error = Some(self.state.locale.format(
                            "Unknown command: /{0} — did you mean {1}?",
                            &[(typed_word).to_string(), (list).clone()],
                        ));
                        return Ok(());
                    }
                },
                Err(candidates) => {
                    // Ambiguous prefix: restore the input, open the completion
                    // dropdown with the candidates, and explain.
                    let list = candidates
                        .iter()
                        .map(|name| format!("/{name}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    self.state.set_input(&input);
                    self.update_slash_state();
                    self.state.error = Some(self.state.locale.format(
                        "Ambiguous command: /{0} (could be {1})",
                        &[(typed_word).to_string(), (list).clone()],
                    ));
                    return Ok(());
                }
            }
        } else {
            input
        };

        // Try executing through the registry first
        let cwd = self.state.cwd.clone().unwrap_or_else(|| ".".to_string());
        let session_id = self.state.session_id.clone();
        let model = self.state.model.clone();

        let result = crate::localization::with_locale(self.state.locale, || {
            self.command_registry
                .execute(&input, &cwd, session_id.as_deref(), model.as_deref())
        });
        match result {
            Ok(output) => {
                self.handle_command_output(output).await;
                self.slash_state.reset();
                return Ok(());
            }
            Err(e) => {
                if e.kind == crate::commands::CommandErrorKind::UnknownCommand {
                    if self.state.unknown_slash_command_fallback {
                        if let Some(agent) = &self.native_agent {
                            let _ = agent.prompt(input.clone(), vec![]).await;
                            self.state.busy = true;
                        } else {
                            self.state.error = Some(
                                self.state
                                    .locale
                                    .format("Unknown command: {0}", std::slice::from_ref(&(input))),
                            );
                        }
                    } else {
                        // Fallback disabled via tui.slash_command_fallback = false:
                        // never turn an unknown slash command into an agent prompt.
                        self.state.error = Some(self.state.locale.format(
                            "Unknown command: {0} (Type /help to see available commands)",
                            std::slice::from_ref(&(input)),
                        ));
                    }
                } else {
                    // Other errors (like missing args) should be shown to user
                    self.state.error = Some(e.to_string());
                }
            }
        }

        self.slash_state.reset();
        Ok(())
    }

    async fn handle_init_command(&mut self, force: bool) {
        let cwd = self
            .state
            .cwd
            .as_deref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()));
        let result = tokio::task::spawn_blocking({
            let cwd = cwd.clone();
            move || crate::agents_cli::init_workspace(&cwd, None, force)
        })
        .await;
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                self.state.error.replace(self.state.locale.format(
                    "Failed to scaffold AGENTS.md: {0}",
                    &[(format!("{error:#}")).to_string()],
                ));
                return;
            }
        };
        match result {
            Ok(crate::agents_cli::InitWorkspaceResult::Created { path, prompt }) => {
                let display = path
                    .strip_prefix(&cwd)
                    .map(|relative| format!("./{}", relative.display()))
                    .unwrap_or_else(|_| path.display().to_string());
                self.state.add_system_message(self.state.locale.format(
                    "Drafting AGENTS.md at {0}...",
                    std::slice::from_ref(&(display)),
                ));
                if let Err(error) = self.submit_prompt(prompt).await {
                    self.state.error = Some(self.state.locale.format(
                        "Failed to start AGENTS.md draft: {0}",
                        &[(error).to_string()],
                    ));
                }
            }
            Ok(crate::agents_cli::InitWorkspaceResult::Updated { path }) => {
                self.state.add_system_message(self.state.locale.format(
                    "Updated AGENTS instructions at {0}.",
                    &[(path.display()).to_string()],
                ));
            }
            Ok(crate::agents_cli::InitWorkspaceResult::Exists {
                path,
                preview,
                rerun,
            }) => {
                let hint = if rerun == "maestro agents init --force" {
                    "/init --force"
                } else {
                    rerun.as_str()
                };
                self.state.add_system_message(self.state.locale.format("AGENTS instructions already exist at {0}.\nPreview the proposed update below, then re-run `{1}` to apply it.\n\n{2}", &[(path.display()).to_string(), (hint).to_string(), (preview).clone()]));
            }
            Err(error) => {
                self.state.error.replace(self.state.locale.format(
                    "Failed to scaffold AGENTS.md: {0}",
                    &[(format!("{error:#}")).to_string()],
                ));
            }
        }
    }
}

/// Contracts for tool calls in a resumed transcript that never received a
/// result.
///
/// Those are the pending calls: the replayed history still shows them to the
/// model, so the model re-issues them under fresh call ids. Pinning them by
/// tool name is what lets dispatch refuse a call whose tool identity changed
/// while the session was saved.
pub(crate) fn unanswered_tool_call_contracts(
    session: &crate::session::ParsedSession,
) -> Vec<crate::tools::tool_call_contract::ToolCallContract> {
    use crate::session::{AppMessage, ContentBlock};

    let answered: std::collections::HashSet<&str> = session
        .messages
        .iter()
        .filter_map(|message| match message {
            AppMessage::ToolResult { tool_call_id, .. } => Some(tool_call_id.as_str()),
            _ => None,
        })
        .collect();

    let mut contracts = Vec::new();
    for message in &session.messages {
        let AppMessage::Assistant { content, .. } = message else {
            continue;
        };
        for block in content {
            let ContentBlock::ToolCall {
                id,
                contract: Some(contract),
                ..
            } = block
            else {
                continue;
            };
            if answered.contains(id.as_str()) {
                continue;
            }
            contracts.push(contract.clone());
        }
    }
    contracts
}

impl App {
    fn copy_transcript(&mut self, target: &crate::transcript_copy::CopyTarget) {
        match crate::transcript_copy::copy_text(
            target,
            &self.state.messages,
            self.session_manager.current_session_id(),
        ) {
            Ok(text) => match self.clipboard.copy(&text) {
                Ok(()) => {
                    self.state.status = Some(
                        self.state
                            .locale
                            .text(crate::localization::TextKey::Copied)
                            .into(),
                    );
                    self.state.error = None;
                }
                Err(error) => {
                    self.state.error = Some(format!(
                        "{}: {error}",
                        self.state
                            .locale
                            .text(crate::localization::TextKey::CopyFailed)
                    ));
                }
            },
            Err(error) => self.state.error = Some(error),
        }
    }
}
