//! Session creation, saved branches, and actor context adoption.
use super::*;

impl App {
    /// Point the subagent scope at `session_id`'s conversation.
    ///
    /// Call this wherever the active conversation changes. The tool executor
    /// lives behind an `Arc` for the whole process, so without a rotation a
    /// child started by an earlier conversation reported its completion into
    /// whichever conversation happened to be active when it finished, and that
    /// summary went on to the next model request.
    ///
    /// The scope is derived from the session id rather than random, so resuming
    /// a session re-adopts the scope its own children were stamped with and
    /// their parked completions surface then. `None` means no session id exists
    /// yet; the placeholder is unique so it can never collide with another
    /// conversation, and it is replaced when the session file is created.
    /// Point both the subagent scope and the hook system at `session_id`.
    ///
    /// `reason` is published to `SessionStart` / `SessionEnd` hooks as their
    /// `source` / `reason` field. The hook system lives in the runner, so the
    /// session id and the lifecycle dispatch both travel as a command; the
    /// runner compares against the session it holds and fires the transition.
    pub(super) fn adopt_session_context(&mut self, session_id: Option<&str>, reason: &str) {
        self.adopt_session_context_inner(session_id, reason, false);
    }

    pub(super) fn adopt_compacted_session_context(&mut self, session_id: &str) {
        self.adopt_session_context_inner(Some(session_id), "summarize", true);
    }

    fn adopt_session_context_inner(
        &mut self,
        session_id: Option<&str>,
        reason: &str,
        preserve_compacted_checkpoint: bool,
    ) {
        let scope = subagent_scope_for_session(session_id);
        self.tool_executor.set_subagent_parent_scope(scope.clone());
        let Some(agent) = &self.native_agent else {
            return;
        };
        if let Err(e) = agent.set_subagent_parent_scope(scope) {
            self.state.error = Some(
                self.state
                    .locale
                    .format("Failed to rotate subagent scope: {0}", &[(e).to_string()]),
            );
        }
        let owns_persistent_tool_spills =
            session_id.is_some() && self.session_manager.writer().is_some();
        let transcript_path = session_id.and_then(|_| {
            self.session_manager
                .current_session_path()
                .map(|path| path.to_string_lossy().into_owned())
        });
        let transition =
            if let Some(session_id) = session_id.filter(|_| preserve_compacted_checkpoint) {
                agent.set_compacted_session_context_with_transcript(
                    session_id.to_owned(),
                    transcript_path,
                    owns_persistent_tool_spills,
                )
            } else {
                agent.set_session_context_with_transcript(
                    session_id.map(str::to_owned),
                    transcript_path,
                    reason,
                    owns_persistent_tool_spills,
                )
            };
        if let Err(e) = transition {
            self.state.error = Some(
                self.state
                    .locale
                    .format("Failed to update session context: {0}", &[e.to_string()]),
            );
        }
    }

    pub(super) fn start_new_session(&mut self, status: &str) {
        if self.session_change_requires_cleanup() {
            self.request_session_transition(session_transition::TransitionAction::New(
                status.to_owned(),
            ));
            return;
        }
        self.last_esc_at = None;
        self.credential_vault.clear();
        // A child still running under the previous conversation must not report
        // into this one. The scope rotates now; the session id does not exist
        // until the first message creates the session file, which rotates it
        // again to the id-derived scope.
        self.adopt_session_context(None, "new");
        self.dex_terminal = None;
        self.dex_delight = Default::default();
        self.state.messages.clear();
        self.state.clear_focus_turn_state();
        self.plan_review_comments.clear();
        self.state.scroll_offset = 0;
        self.state.alerts.clear();
        self.state.unseen_alerts = 0;
        // Drop any lingering error surface and force a full viewport repaint
        // so the previous session's frames cannot linger on screen.
        self.reset_rendered_viewport();
        self.session_manager.reset_session();
        self.state.session_id = None;
        self.pending_agent_tool_notes.clear();
        self.pending_agent_note_applications.clear();
        self.pending_agent_note_consumptions.clear();
        self.pending_consumed_agent_tool_notes.clear();
        self.ready_consumed_agent_tool_notes.clear();
        self.active_turn_assistant_messages_persisted = true;
        self.ephemeral_lifecycle_applications.clear();
        crate::plan_mode::set_active_session_id(None);
        crate::tools::tool_call_contract::clear_pending_contracts();
        self.session_started_at = SystemTime::now();
        self.session_resume_failed = false;
        self.usage_tracker = crate::usage::UsageTracker::new();
        if !self.current_model.is_empty() {
            self.usage_tracker.set_model(self.current_model.clone());
        }
        self.clear_active_skills();
        if let Some(agent) = &self.native_agent {
            agent.clear_history();
        }
        self.state.status = Some(status.to_string());
        self.state.add_system_message(status.to_string());
    }

    pub(super) fn fork_session(&mut self) {
        use crate::session::BranchPoint;

        let fork_index = self.state.messages.len().saturating_sub(1);
        let fork_id = self
            .state
            .messages
            .last()
            .map(|m| m.id.clone())
            .unwrap_or_else(|| "start".to_string());
        let branch = BranchPoint::new(fork_id, fork_index)
            .with_description(self.state.locale.translate("Forked via /fork"));
        if let Err(error) = self.ensure_session_started() {
            self.state.error = Some(self.state.locale.format(
                "Failed to start session before fork: {0}",
                &[(error).to_string()],
            ));
            return;
        }
        match self.session_manager.fork_session_snapshot() {
            Ok((fork_session_id, path)) => {
                let activity = if self.state.busy {
                    self.state
                        .locale
                        .translate(" The parent remains active and its current response continues.")
                } else {
                    self.state.locale.translate(" The parent remains selected.")
                };
                self.state.status = Some(self.state.locale.format(
                    "Fork {0} created without switching sessions.",
                    &[fork_session_id[..8.min(fork_session_id.len())].to_string()],
                ));
                self.state.add_system_message(self.state.locale.format(
                    "Forked at message {0} (branch {1}) into session {2} at {3}.{4}",
                    &[
                        (branch.fork_index + 1).to_string(),
                        branch.id[..8.min(branch.id.len())].to_string(),
                        (fork_session_id).clone(),
                        (path.display()).to_string(),
                        (activity).to_string(),
                    ],
                ));
                self.state.add_system_message(self.state.locale.format(
                    "Resume fork: deixic-code --resume-session {0}\nBrowse branches: /tree or /sessions, then Ctrl+F.",
                    &[fork_session_id],
                ));
            }
            Err(error) => {
                self.state.error = Some(
                    self.state
                        .locale
                        .format("Failed to fork session: {0}", &[(error).to_string()]),
                );
            }
        }
    }

    pub(super) fn rewind_turns(&mut self, turns: usize, dry_run: bool) {
        self.rewind_saved_turns(turns, dry_run, false);
    }

    pub(super) fn rewind_saved_turns(&mut self, turns: usize, dry_run: bool, files: bool) {
        if self.session_change_requires_cleanup() && !dry_run {
            self.request_session_transition(session_transition::TransitionAction::Rewind {
                turns,
                files,
            });
            return;
        }
        let result = (|| -> anyhow::Result<()> {
            self.session_manager.flush()?;
            let source = self
                .session_manager
                .current_session_path()
                .ok_or_else(|| anyhow::anyhow!("No saved session to rewind."))?;
            let (boundary, saved_turns) =
                crate::session::rewind_boundary_with_turn_count(&source, turns)?;
            let kept_turns = saved_turns.saturating_sub(turns);
            if dry_run {
                self.state.add_system_message(self.state.locale.format("Rewind before the last {0} user turn(s) into a new saved session. The original remains available.", &[(turns).to_string()]));
                if files {
                    self.preview_rewind_files(kept_turns)?;
                }
                return Ok(());
            }
            if files {
                self.preview_rewind_files(kept_turns)?;
            }
            // Publish the branch before changing active history or files.
            let fork = crate::session::fork_session_prefix(&source, Some(boundary))?;
            let source_id = self.state.session_id.clone();
            if let Some(source_id) = source_id.as_deref() {
                let sessions = self.session_manager.sessions_dir();
                crate::checkpoints::fork_before_turn(
                    &crate::checkpoints::CheckpointStore::new(sessions, source_id),
                    &crate::checkpoints::CheckpointStore::new(sessions, &fork.id),
                    kept_turns,
                )?;
            }

            self.resume_session_path(&fork.path, &fork.id);
            if self.session_manager.current_session_id() != Some(fork.id.as_str()) {
                anyhow::bail!("The saved branch could not be opened.");
            }
            self.state.status = Some(self.state.locale.format(
                "Rewound into saved session {0}.",
                std::slice::from_ref(&(fork.id)),
            ));
            if files {
                self.restore_rewind_files(source_id.as_deref(), kept_turns)?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            self.state.error = Some(
                self.state
                    .locale
                    .format("Rewind failed: {0}", &[(error).to_string()]),
            );
        }
    }
}
