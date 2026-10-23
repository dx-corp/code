//! Resume saved conversation state and restore its visible transcript.
use super::command_handlers::unanswered_tool_call_contracts;
use super::*;

impl App {
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
        if self.session_change_requires_cleanup() {
            self.request_session_transition(session_transition::TransitionAction::Resume {
                path: path.to_owned(),
                session_id: target_session_id.to_owned(),
            });
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
        let restored_model = self
            .startup_session_model_override
            .clone()
            .unwrap_or_else(|| session.header.model.clone());
        if let Some(reason) = check_model_allowed(&policy_model_id(&restored_model)) {
            self.report_session_resume_failure(target_session_id, reason);
            return;
        }
        let restored_thinking = crate::model_dynamics::normalize_thinking(
            &restored_model,
            session.header.thinking_level,
        );
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
        self.state.transcript_images.borrow_mut().restore(session);
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
            if let Err(e) = agent.set_model(&restored_model) {
                self.state.error = Some(
                    self.state
                        .locale
                        .format("Failed to set model: {0}", &[(e).to_string()]),
                );
                model_applied = false;
                thinking_applied = false;
            } else {
                let (enabled, budget) = restored_thinking.to_config();
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
            self.current_model = restored_model.clone();
            self.current_model_user_set = true;
            self.state.model = Some(restored_model.clone());
            self.usage_tracker.set_model(restored_model);
            if thinking_applied {
                self.current_thinking_level = restored_thinking;
                self.state.thinking_level = self.current_thinking_level;
            }
        } else if !self.current_model.is_empty() {
            self.usage_tracker.set_model(self.current_model.clone());
        }

        self.restore_session_preferences(session.settings.as_ref());
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
        self.startup_session_model_override = None;
        self.persisted_session_settings = None;
        if !self.record_session_settings() {
            self.session_resume_failed = true;
            return;
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
}

pub(super) fn restore_visible_session_messages(state: &mut AppState, session: &ParsedSession) {
    state.clear_conversation_messages();
    state.clear_focus_turn_state();

    let tool_results: std::collections::HashMap<_, _> = session
        .messages
        .iter()
        .filter_map(|message| {
            if let AppMessage::ToolResult {
                tool_call_id,
                content,
                is_error,
                receipt,
                ..
            } = message
            {
                Some((
                    tool_call_id.as_str(),
                    (content, *is_error, receipt.as_ref()),
                ))
            } else {
                None
            }
        })
        .collect();

    for app_msg in &session.messages {
        let role = match app_msg {
            AppMessage::User { .. } => MessageRole::User,
            AppMessage::Assistant { .. } => MessageRole::Assistant,
            AppMessage::ToolResult { .. } => continue,
        };

        let tool_calls = match app_msg {
            AppMessage::Assistant { content, .. } => content
                .iter()
                .filter_map(|block| {
                    let SessionContentBlock::ToolCall { id, name, args, .. } = block else {
                        return None;
                    };
                    let result = tool_results.get(id.as_str());
                    let receipt = result
                        .and_then(|(_, _, receipt)| *receipt)
                        .filter(|receipt| receipt.call_id == *id && receipt.tool_name == *name);
                    let status = if matches!(
                        receipt.map(|receipt| &receipt.status),
                        Some(maestro_runtime_contracts::ExecutionStatus::Cancelled { .. })
                    ) {
                        crate::state::ToolCallStatus::Cancelled
                    } else if matches!(
                        receipt.map(|receipt| &receipt.status),
                        Some(maestro_runtime_contracts::ExecutionStatus::Denied)
                    ) {
                        crate::state::ToolCallStatus::Blocked
                    } else if matches!(
                        receipt.map(|receipt| &receipt.status),
                        Some(
                            maestro_runtime_contracts::ExecutionStatus::Failed
                                | maestro_runtime_contracts::ExecutionStatus::Indeterminate
                        )
                    ) {
                        crate::state::ToolCallStatus::Failed
                    } else {
                        match result {
                            Some((_, true, _)) => crate::state::ToolCallStatus::Failed,
                            Some((_, false, _)) => crate::state::ToolCallStatus::Completed,
                            None => crate::state::ToolCallStatus::Pending,
                        }
                    };
                    Some(crate::state::ToolCallState {
                        call_id: id.clone(),
                        parent_call_id: None,
                        duration_ms: receipt.and_then(|receipt| receipt.duration_ms),
                        tool: name.clone(),
                        args: args.clone(),
                        status,
                        output: result
                            .map_or_else(String::new, |(content, _, _)| (*content).clone()),
                    })
                })
                .collect(),
            _ => Vec::new(),
        };
        state.messages.push(Message {
            id: uuid::Uuid::new_v4().to_string(),
            role,
            kind: MessageKind::Regular,
            content: app_msg.text_content(),
            thinking: String::new(),
            streaming: false,
            tool_calls,
            usage: None,
            timestamp: UNIX_EPOCH + Duration::from_millis(app_msg.timestamp()),
            thinking_expanded: false,
        });
    }

    for compaction in session.compactions.iter().skip(
        session
            .selective_summary_context
            .as_ref()
            .map_or(0, |(_, _, count)| *count),
    ) {
        if let Some(first_kept_entry_index) = compaction.first_kept_entry_index {
            state.apply_compaction(
                compaction.summary.clone(),
                first_kept_entry_index,
                parse_rfc3339_system_time(&compaction.timestamp)
                    .unwrap_or_else(|_| SystemTime::now()),
            );
        }
    }

    for notification in &session.lifecycle_notifications {
        let insert_at =
            lifecycle_notification_insert_position(&state.messages, notification.visible_index);
        state.messages.insert(
            insert_at,
            Message {
                id: notification.id.clone(),
                role: MessageRole::Assistant,
                kind: MessageKind::System,
                content: notification.content.clone(),
                thinking: String::new(),
                streaming: false,
                tool_calls: Vec::new(),
                usage: None,
                timestamp: parse_rfc3339_system_time(&notification.timestamp)
                    .unwrap_or_else(|_| SystemTime::now()),
                thinking_expanded: false,
            },
        );
    }

    let mut side_questions = session.side_questions.iter().collect::<Vec<_>>();
    side_questions
        .sort_by_key(|side| parse_rfc3339_system_time(&side.timestamp).unwrap_or(UNIX_EPOCH));
    for side in side_questions {
        let timestamp =
            parse_rfc3339_system_time(&side.timestamp).unwrap_or_else(|_| SystemTime::now());
        let question = Message {
            id: side.id.clone(),
            role: MessageRole::User,
            kind: MessageKind::SideQuestion,
            content: side.question.clone(),
            thinking: String::new(),
            streaming: false,
            tool_calls: Vec::new(),
            usage: None,
            timestamp,
            thinking_expanded: false,
        };
        let answer = match &side.error {
            Some(error) if side.answer.is_empty() => maestro_ui::localization::format(
                "Side question failed: {0}",
                std::slice::from_ref(error),
            ),
            Some(error) => maestro_ui::localization::format(
                "{0}\n\nSide question failed: {1}",
                &[(side.answer).clone(), (error).clone()],
            ),
            None => side.answer.clone(),
        };
        let response = Message {
            id: format!("{}-answer", side.id),
            role: MessageRole::Assistant,
            kind: MessageKind::SideAnswer,
            content: answer,
            thinking: String::new(),
            streaming: false,
            tool_calls: Vec::new(),
            usage: None,
            timestamp,
            thinking_expanded: false,
        };
        let insert_at = state
            .messages
            .iter()
            .position(|message| {
                message.kind != MessageKind::CompactionBoundary && message.timestamp > timestamp
            })
            .unwrap_or(state.messages.len());
        state.messages.insert(insert_at, question);
        state.messages.insert(insert_at + 1, response);
    }
}

fn lifecycle_notification_insert_position(messages: &[Message], visible_index: usize) -> usize {
    let mut visible_count = 0usize;
    for (index, message) in messages.iter().enumerate() {
        if message.kind != MessageKind::Regular {
            continue;
        }
        if visible_count == visible_index {
            return index;
        }
        visible_count += 1;
    }
    messages.len()
}
