//! Construct the native actor and restore the current TUI session.

use super::*;

impl App {
    /// Spawn the native Rust agent
    pub(super) async fn spawn_agent(&mut self) -> Result<()> {
        let cwd_path = std::env::current_dir().unwrap_or_default();
        let cwd = cwd_path.to_string_lossy().to_string();

        // Detect git branch
        let git_branch = git::current_branch(&cwd_path);
        self.current_git_branch = git_branch.clone();

        // Prefer a model the user just picked in `/setup` or `/model`.
        // Otherwise: MAESTRO_MODEL, then ~/.maestro (and project) model
        // settings, then Codex ChatGPT login, then the platform default.
        let model = if self.current_model_user_set && !self.current_model.trim().is_empty() {
            self.current_model.clone()
        } else {
            crate::codex_auth::resolve_default_model()
        };

        let (history, session_id, thinking_level) = self.agent_context_for_spawn()?;
        let thinking_level = crate::model_dynamics::normalize_thinking(&model, thinking_level);
        self.current_thinking_level = thinking_level;
        self.state.thinking_level = thinking_level;
        let (thinking_enabled, thinking_budget) = thinking_level.to_config();
        let config = self.native_agent_config(&model, &cwd, thinking_enabled, thinking_budget);

        let policy_model = policy_model_id(&model);
        if let Some(reason) = check_model_allowed(&policy_model) {
            self.state.error = Some(reason);
            return Ok(());
        }

        self.current_model = model.clone();
        self.state.thinking_level = thinking_level;
        self.usage_tracker.set_model(model.clone());

        self.state.status = Some(self.state.locale.format(
            "Initializing agent ({0})...",
            std::slice::from_ref(&(model)),
        ));

        let subagent_parent_scope_id = self.tool_executor.subagent_parent_scope_id();
        match NativeAgent::new_with_credential_vault_and_subagent_scope(
            std::sync::Arc::new(maestro_local_host::agent::tool_host::BuiltInToolHost),
            config,
            self.credential_vault.clone(),
            subagent_parent_scope_id,
        ) {
            Ok((agent, event_rx)) => {
                self.restore_request_cache(&agent);
                let tool_tx = agent.tool_response_sender();
                self.native_agent = Some(agent);
                self.native_event_rx = Some(crate::loop_wake::forward_unbounded(
                    event_rx,
                    self.loop_wake.clone(),
                ));
                self.tool_response_tx = Some(tool_tx);

                // A session restored at startup exists before the runner does,
                // so it never passed through any of the transitions that adopt
                // a session context. Capture it here and adopt it below.
                let restored_session_id = session_id.clone();

                // Send ready event
                if let Some(agent) = &self.native_agent {
                    agent.set_goal_tools_visible(self.goal_store.tools_visible());
                    // Startup session restore runs before the agent exists. Queue
                    // the copied conversation before any initial prompt so a
                    // fork continues with the same model context.
                    let continuation = match self.session_manager.current_session_path() {
                        Some(path) => crate::session::SessionReader::read_file(&path)?
                            .compactions
                            .last()
                            .and_then(|entry| entry.continuation.clone()),
                        None => None,
                    };
                    agent.replace_history_with_continuation(history, continuation);
                    agent.send_ready();
                    // Send session info with git branch
                    agent.send_session_info(&cwd, session_id, git_branch);
                    let _ = agent.set_steering_mode(self.state.steering_mode);
                    let _ = agent.set_follow_up_mode(self.state.follow_up_mode);
                }

                // `send_session_info` is an outbound event to the UI and sets
                // nothing on the runner. Without this the runner kept the tool
                // executor's initial random subagent scope and a hook
                // `session_id` of `None`, so completions parked for this
                // session's own children were never drained and every hook
                // payload published a null session.
                if let Some(restored_session_id) = restored_session_id {
                    self.adopt_session_context(Some(&restored_session_id), "restore");
                }

                // Ensure busy is false so user can type
                self.state.busy = false;
                self.state.model = Some(model.clone());
                self.state.status = Some(
                    self.state
                        .locale
                        .format("Ready: {0}", std::slice::from_ref(&(model))),
                );
                self.flush_pending_agent_tool_notes();
            }
            Err(e) => {
                if should_open_first_run_setup(false, default_model_credentials_ready()) {
                    self.state.status = Some(
                        self.state
                            .locale
                            .translate("Complete setup to start.")
                            .to_string(),
                    );
                } else {
                    let mut message = self
                        .state
                        .locale
                        .format("Failed to create agent: {0}", &[format!("{e:#}")]);
                    if crate::codex_auth::read_codex_auth().is_none()
                        && std::env::var_os("OPENAI_API_KEY").is_none()
                        && std::env::var_os("OPENAI_CODEX_TOKEN").is_none()
                    {
                        message.push_str(
                            self.state.locale.translate(" — run `deixic-code evalops login` (required), then configure a provider if needed."),
                        );
                    }
                    self.state.error = Some(message);
                }
            }
        }

        Ok(())
    }
}
