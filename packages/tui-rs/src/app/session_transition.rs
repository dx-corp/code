//! Confirm session changes, then await the existing actor's cleanup barrier.
use super::*;
use maestro_ui::Modal;
use ratatui::{
    Frame,
    layout::Rect,
    widgets::{Paragraph, Wrap},
};
use tokio::sync::oneshot::{self, error::TryRecvError};

pub(super) enum TransitionAction {
    New(String),
    Rewind { turns: usize, files: bool },
}

pub(super) enum SessionCleanupState {
    Ready,
    // The screen's busy flag is not an actor cleanup receipt. A kept parent
    // retains its receiver; a closed receiver keeps confirmation required.
    Awaiting(Option<oneshot::Receiver<()>>),
}

enum Stage {
    Confirm,
    Waiting {
        settled: oneshot::Receiver<()>,
        started: Instant,
    },
}

pub(super) struct TransitionDialog {
    action: TransitionAction,
    stage: Stage,
    previous_modal: ActiveModal,
}

impl TransitionDialog {
    pub(super) fn render(&self, frame: &mut Frame, area: Rect) {
        let theme = crate::themes::current_ui_theme();
        let inner = Modal::new(
            maestro_ui::localization::tr("Change conversation"),
            80,
            area.height.saturating_sub(2).min(18),
        )
        .theme(theme)
        .render(frame, area);
        let text = match &self.stage {
            Stage::Waiting { .. } => maestro_ui::localization::tr(
                "Stopping the response and finishing tool cleanup… Esc keeps this conversation.",
            )
            .to_owned(),
            Stage::Confirm => {
                let question = match &self.action {
                    TransitionAction::New(_) => maestro_ui::localization::tr(
                        "Stop the response and start a new conversation? The saved conversation stays available.",
                    ).to_owned(),
                    TransitionAction::Rewind { turns, files } => maestro_ui::localization::format(
                        if *files {
                            "Stop the response and rewind the last {0} turn(s), including their file changes? The original conversation stays available."
                        } else {
                            "Stop the response and rewind the last {0} turn(s)? The original conversation stays available. Files stay unchanged."
                        }, &[turns.to_string()],
                    ),
                };
                maestro_ui::localization::format(
                    "{0}\n\nQueued prompts return to the composer.\nEnter: stop and continue · Esc: keep working",
                    &[question],
                )
            }
        };
        frame.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .style(theme.on_panel().text_style()),
            inner,
        );
    }
}

impl App {
    pub(super) fn session_cleanup_pending(&self) -> bool {
        matches!(self.session_cleanup, SessionCleanupState::Awaiting(_))
    }

    pub(super) fn session_change_requires_cleanup(&self) -> bool {
        self.state.busy || self.session_cleanup_pending()
    }

    async fn finish_session_cleanup_receipt(&mut self) -> Result<bool> {
        // Final snapshots and events belong to the parent even when the
        // transition dialog was dismissed while the actor was cleaning up.
        self.poll_agent().await?;
        self.goal_auto_continue_armed = false;
        if let Err(error) = self.session_manager.flush() {
            self.state.error = Some(
                self.state
                    .locale
                    .format("Failed to save session: {0}", &[error.to_string()]),
            );
            return Ok(false);
        }
        self.session_cleanup = SessionCleanupState::Ready;
        Ok(true)
    }

    async fn poll_kept_session_cleanup(&mut self) -> Result<bool> {
        let SessionCleanupState::Awaiting(Some(settled)) = &mut self.session_cleanup else {
            return Ok(false);
        };
        match settled.try_recv() {
            Ok(()) => {
                self.session_cleanup = SessionCleanupState::Awaiting(None);
                self.finish_session_cleanup_receipt().await?;
                Ok(true)
            }
            Err(TryRecvError::Empty) => Ok(false),
            Err(TryRecvError::Closed) => {
                self.session_cleanup = SessionCleanupState::Awaiting(None);
                Ok(true)
            }
        }
    }

    pub(super) fn request_session_transition(&mut self, action: TransitionAction) {
        if let TransitionAction::Rewind { turns, files } = &action {
            let preview = (|| -> anyhow::Result<()> {
                self.session_manager.flush()?;
                let path = self
                    .session_manager
                    .current_session_path()
                    .ok_or_else(|| anyhow::anyhow!("No saved session to rewind."))?;
                let (_, total) = crate::session::rewind_boundary_with_turn_count(&path, *turns)?;
                if *files {
                    self.preview_rewind_files(total.saturating_sub(*turns))?;
                }
                Ok(())
            })();
            if let Err(error) = preview {
                self.state.error = Some(
                    self.state
                        .locale
                        .format("Rewind failed: {0}", &[error.to_string()]),
                );
                return;
            }
        }
        self.session_transition = Some(TransitionDialog {
            action,
            stage: Stage::Confirm,
            previous_modal: self.active_modal,
        });
        self.active_modal = ActiveModal::SessionTransition;
    }

    pub(super) fn session_transition_waiting(&self) -> bool {
        self.session_transition
            .as_ref()
            .is_some_and(|dialog| matches!(dialog.stage, Stage::Waiting { .. }))
    }

    fn keep_current_session_after_transition(&mut self) {
        let waiting = self.session_transition_waiting();
        let previous = if let Some(dialog) = self.session_transition.take() {
            if let Stage::Waiting { settled, .. } = dialog.stage {
                self.session_cleanup = SessionCleanupState::Awaiting(Some(settled));
            }
            dialog.previous_modal
        } else {
            ActiveModal::None
        };
        if waiting {
            let prompts = self.drain_queued_prompts_for_restore();
            self.restore_queued_prompts_to_input(prompts);
        }
        self.active_modal = if self.approval_controller.is_visible() && !waiting {
            ActiveModal::Approval
        } else if previous == ActiveModal::SessionTransition || waiting {
            ActiveModal::None
        } else {
            previous
        };
    }

    pub(super) async fn handle_session_transition_key(
        &mut self,
        code: KeyCode,
        ctrl: bool,
    ) -> Result<()> {
        if code == KeyCode::Esc || (ctrl && code == KeyCode::Char('c')) {
            self.keep_current_session_after_transition();
            return Ok(());
        }
        if code != KeyCode::Enter || self.session_transition_waiting() {
            return Ok(());
        }
        let Some(agent) = &self.native_agent else {
            self.keep_current_session_after_transition();
            self.state.error = Some(self.state.locale.translate(
                "Could not confirm that the response stopped. The current conversation was kept.",
            ).to_owned());
            return Ok(());
        };
        let settled = match agent.cancel_for_session_transition() {
            Ok(settled) => settled,
            Err(error) => {
                self.keep_current_session_after_transition();
                self.state.error = Some(error.to_string());
                return Ok(());
            }
        };
        self.session_cleanup = SessionCleanupState::Awaiting(None);
        self.cancel_pending_guardian_reviews();
        self.approval_controller.clear();
        self.submit_queued_steering_after_interrupt = false;
        self.restore_queued_prompts_after_interrupt = false;
        self.goal_auto_continue_armed = false;
        if let Some(dialog) = &mut self.session_transition {
            dialog.stage = Stage::Waiting {
                settled,
                started: Instant::now(),
            };
        }
        Ok(())
    }

    pub(super) async fn poll_session_transition(&mut self) -> Result<bool> {
        let cleanup_changed = self.poll_kept_session_cleanup().await?;
        let Some(dialog) = &mut self.session_transition else {
            return Ok(cleanup_changed);
        };
        self.active_modal = ActiveModal::SessionTransition;
        let Stage::Waiting { settled, started } = &mut dialog.stage else {
            return Ok(cleanup_changed);
        };
        // A completion racing cancellation can re-arm goal continuation.
        self.goal_auto_continue_armed = false;
        let confirmed = match settled.try_recv() {
            Ok(()) => true,
            Err(TryRecvError::Empty) if started.elapsed() < Duration::from_secs(30) => {
                return Ok(cleanup_changed);
            }
            Err(_) => false,
        };
        if !confirmed {
            self.keep_current_session_after_transition();
            self.state.error = Some(self.state.locale.translate(
                "Could not confirm that the response stopped. The current conversation was kept.",
            ).to_owned());
            return Ok(true);
        }
        if !self.finish_session_cleanup_receipt().await? {
            self.keep_current_session_after_transition();
            return Ok(true);
        }
        self.state.busy = false;
        self.cancel_pending_guardian_reviews();
        self.approval_controller.clear();
        let Some(dialog) = self.session_transition.take() else {
            return Ok(false);
        };
        self.active_modal = ActiveModal::None;
        let prompts = self.drain_queued_prompts_for_restore();
        match dialog.action {
            TransitionAction::New(status) => self.start_new_session(&status),
            TransitionAction::Rewind { turns, files } => {
                self.rewind_saved_turns(turns, false, files);
            }
        }
        self.restore_queued_prompts_to_input(prompts);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        let mut app = super::super::tests::new_test_app();
        // Start these conversation fixtures independently of host diagnostics.
        app.state.messages.clear();
        app
    }

    fn waiting(app: &mut App, receiver: oneshot::Receiver<()>, started: Instant) {
        app.session_cleanup = SessionCleanupState::Awaiting(None);
        app.session_transition = Some(TransitionDialog {
            action: TransitionAction::New("New session started.".into()),
            stage: Stage::Waiting {
                settled: receiver,
                started,
            },
            previous_modal: ActiveModal::None,
        });
        app.active_modal = ActiveModal::SessionTransition;
    }

    #[tokio::test]
    async fn busy_rewind_requires_confirmation_and_keeps_the_saved_parent() {
        let mut app = app();
        let saved = tempfile::tempdir().unwrap();
        app.session_manager =
            SessionManager::with_sessions_dir("rewind-confirm-test", saved.path());
        app.state.add_user_message("keep me".into());
        app.record_user_message("keep me");
        app.session_manager.flush().unwrap();
        let parent = app.session_manager.current_session_path().unwrap();
        let before = std::fs::read(&parent).unwrap();
        app.state.busy = true;
        app.rewind_turns(1, false);
        assert_eq!(app.active_modal, ActiveModal::SessionTransition);
        assert!(!app.session_transition_waiting());
        assert!(app.state.busy);
        assert_eq!(app.state.messages[0].content, "keep me");
        assert_eq!(
            app.session_manager.current_session_path(),
            Some(parent.clone())
        );
        assert_eq!(std::fs::read(&parent).unwrap(), before);
        app.handle_session_transition_key(KeyCode::Esc, false)
            .await
            .unwrap();
        assert_eq!(app.active_modal, ActiveModal::None);
        assert!(app.state.busy);
        assert_eq!(app.state.messages[0].content, "keep me");
        assert_eq!(
            app.session_manager.current_session_path(),
            Some(parent.clone())
        );
        assert_eq!(std::fs::read(&parent).unwrap(), before);
    }

    #[tokio::test]
    async fn dismissal_preserves_running_response_draft_and_queued_work() {
        let mut app = app();
        app.state.busy = true;
        app.state.add_user_message("parent prompt".into());
        app.state.set_input("draft é 日本語");
        app.queued_prompts.push_back(QueuedPrompt {
            id: 1,
            content: "queued work".into(),
            kind: PromptKind::FollowUp,
        });
        app.start_new_session("New session started.");
        assert_eq!(app.active_modal, ActiveModal::SessionTransition);
        app.handle_session_transition_key(KeyCode::Esc, false)
            .await
            .unwrap();
        assert!(app.state.busy);
        assert_eq!(app.state.input(), "draft é 日本語");
        assert_eq!(app.queued_prompts.len(), 1);
        assert_eq!(app.state.messages[0].content, "parent prompt");
    }

    #[tokio::test]
    async fn late_approval_cannot_take_confirmation_keys_or_start_during_cleanup() {
        let mut app = app();
        app.state.busy = true;
        app.state.set_input("retained draft");
        let (_reply, settled) = oneshot::channel();
        waiting(&mut app, settled, Instant::now());
        let (decisions, mut denied) = mpsc::unbounded_channel();
        app.tool_response_tx = Some(decisions);
        app.handle_agent_message(FromAgent::ToolCall {
            call_id: "late-tool".into(),
            tool: "bash".into(),
            args: serde_json::json!({"command":"touch late-approval-marker"}),
            requires_approval: true,
            approval_inline_env: None,
        })
        .await
        .unwrap();
        assert!(app.approval_controller.current().is_none());
        assert!(app.pending_guardian_reviews.is_empty());
        let (id, approved, _, _, _) = denied.try_recv().unwrap();
        assert_eq!(id, "late-tool");
        assert!(!approved);
        app.active_modal = ActiveModal::Approval;
        app.handle_key(KeyCode::Enter, CrosstermModifiers::NONE)
            .await
            .unwrap();
        assert_eq!(app.active_modal, ActiveModal::SessionTransition);
        assert!(app.session_transition_waiting());
        assert_eq!(app.state.input(), "retained draft");
        app.handle_session_transition_key(KeyCode::Esc, false)
            .await
            .unwrap();
        app.handle_agent_message(FromAgent::ToolCall {
            call_id: "late-tool-after-keep".into(),
            tool: "bash".into(),
            args: serde_json::json!({"command":"touch late-approval-marker"}),
            requires_approval: true,
            approval_inline_env: None,
        })
        .await
        .unwrap();
        let (id, approved, _, _, _) = denied.try_recv().unwrap();
        assert_eq!(id, "late-tool-after-keep");
        assert!(!approved);
        assert!(app.approval_controller.current().is_none());
        assert!(app.pending_guardian_reviews.is_empty());
        assert_eq!(app.active_modal, ActiveModal::None);
    }

    #[tokio::test]
    async fn missing_actor_cannot_authorize_switching() {
        let mut app = app();
        app.state.busy = true;
        app.state.add_user_message("parent prompt".into());
        app.start_new_session("New session started.");
        app.handle_session_transition_key(KeyCode::Enter, false)
            .await
            .unwrap();
        assert!(app.state.busy);
        assert_eq!(app.state.messages[0].content, "parent prompt");
        assert!(
            app.state
                .error
                .as_deref()
                .unwrap()
                .contains("current conversation was kept")
        );
    }

    #[tokio::test]
    async fn closed_barrier_and_timeout_keep_parent_and_restore_queued_draft() {
        for closed in [true, false] {
            let mut app = app();
            app.state.busy = true;
            app.state.add_user_message("parent prompt".into());
            app.state.set_input("draft é");
            app.queued_prompts.push_back(QueuedPrompt {
                id: 1,
                content: "queued work".into(),
                kind: PromptKind::FollowUp,
            });
            let (reply, settled) = oneshot::channel();
            let started = Instant::now()
                .checked_sub(if closed {
                    Duration::ZERO
                } else {
                    Duration::from_secs(31)
                })
                .unwrap();
            waiting(&mut app, settled, started);
            let _held_reply = if closed {
                drop(reply);
                None
            } else {
                Some(reply)
            };
            assert!(app.poll_session_transition().await.unwrap());
            assert_eq!(app.state.messages[0].content, "parent prompt");
            assert!(app.state.input().contains("queued work"));
            assert!(app.state.input().contains("draft é"));
            assert!(
                app.state
                    .error
                    .as_deref()
                    .unwrap()
                    .contains("current conversation was kept")
            );
        }
    }

    #[tokio::test]
    async fn failed_cleanup_cannot_be_bypassed_by_an_idle_retry() {
        let mut app = app();
        app.state.busy = true;
        app.state.add_user_message("parent prompt".into());
        let (reply, settled) = oneshot::channel();
        waiting(&mut app, settled, Instant::now());
        app.handle_agent_message(FromAgent::TurnInterrupted {
            response_id: "old".into(),
            reason: "user".into(),
        })
        .await
        .unwrap();
        assert!(
            !app.state.busy,
            "terminal events can precede the cleanup receipt"
        );
        drop(reply);
        assert!(app.poll_session_transition().await.unwrap());
        let other = tempfile::tempdir().unwrap();
        app.resume_session_path(&other.path().join("unread-target.jsonl"), "other");
        assert!(
            app.state
                .error
                .as_deref()
                .unwrap()
                .contains("Could not confirm"),
            "unknown cleanup must reject adoption before reading another transcript"
        );
        app.start_new_session("New session started.");
        assert_eq!(app.active_modal, ActiveModal::SessionTransition);
        assert_eq!(app.state.messages[0].content, "parent prompt");
    }

    #[tokio::test]
    async fn dismissed_cleanup_still_requires_the_retained_actor_receipt() {
        let mut app = app();
        let saved = tempfile::tempdir().unwrap();
        app.session_manager = SessionManager::with_sessions_dir("kept-cleanup-test", saved.path());
        app.state.add_user_message("parent prompt".into());
        app.handle_agent_message(FromAgent::ResponseStart {
            response_id: "old".into(),
        })
        .await
        .unwrap();
        app.ensure_session_started().unwrap();
        let parent_path = app.session_manager.current_session_path().unwrap();
        app.state.busy = false;
        let (events, rx) = mpsc::unbounded_channel();
        app.native_event_rx = Some(rx);
        let (reply, settled) = oneshot::channel();
        waiting(&mut app, settled, Instant::now());
        app.handle_session_transition_key(KeyCode::Esc, false)
            .await
            .unwrap();
        assert!(app.session_cleanup_pending());
        app.start_new_session("New session started.");
        assert_eq!(app.active_modal, ActiveModal::SessionTransition);
        assert_eq!(app.state.messages[0].content, "parent prompt");
        app.handle_session_transition_key(KeyCode::Esc, false)
            .await
            .unwrap();
        events
            .send(FromAgent::ResponseChunk {
                response_id: "old".into(),
                content: "late kept-parent chunk".into(),
                is_thinking: false,
            })
            .unwrap();
        events
            .send(FromAgent::ResponseEnd {
                response_id: "old".into(),
                usage: None,
            })
            .unwrap();
        events
            .send(FromAgent::TurnInterrupted {
                response_id: "old".into(),
                reason: "user".into(),
            })
            .unwrap();
        reply.send(()).unwrap();
        assert!(app.poll_session_transition().await.unwrap());
        assert!(!app.session_cleanup_pending());
        assert_eq!(app.state.messages[0].content, "parent prompt");
        app.start_new_session("New session started.");
        assert_eq!(app.active_modal, ActiveModal::None);
        assert_eq!(app.state.messages[0].content, "New session started.");
        assert!(
            std::fs::read_to_string(parent_path)
                .unwrap()
                .contains("late kept-parent chunk"),
            "the retained receipt must drain and save the parent's final events"
        );
    }

    #[tokio::test]
    async fn acknowledgement_drains_old_events_before_starting_new_session() {
        let mut app = app();
        app.goal_store = crate::goal::GoalStore::default();
        app.goal_store
            .create("parent goal", None, false, None, None)
            .unwrap();
        app.goal_store.set_auto_continue(true).unwrap();
        let saved = tempfile::tempdir().unwrap();
        app.session_manager =
            SessionManager::with_sessions_dir("transition-drain-test", saved.path());
        app.state.add_user_message("parent prompt".into());
        app.handle_agent_message(FromAgent::ResponseStart {
            response_id: "old".into(),
        })
        .await
        .unwrap();
        app.ensure_session_started().unwrap();
        let parent_path = app.session_manager.current_session_path().unwrap();
        app.state.busy = true;
        app.state.set_input("draft é");
        let (events, rx) = mpsc::unbounded_channel();
        app.native_event_rx = Some(rx);
        let (reply, settled) = oneshot::channel();
        waiting(&mut app, settled, Instant::now());
        app.goal_auto_continue_armed = true;
        assert!(!app.poll_session_transition().await.unwrap());
        assert!(!app.goal_auto_continue_armed);
        assert_eq!(app.state.messages[0].content, "parent prompt");
        events
            .send(FromAgent::ResponseChunk {
                response_id: "old".into(),
                content: "late parent chunk".into(),
                is_thinking: false,
            })
            .unwrap();
        events
            .send(FromAgent::ResponseEnd {
                response_id: "old".into(),
                usage: None,
            })
            .unwrap();
        events
            .send(FromAgent::TurnCompleted {
                response_id: "old".into(),
                coding_completion: None,
                coding_child_records: Vec::new(),
            })
            .unwrap();
        reply.send(()).unwrap();
        assert!(app.poll_session_transition().await.unwrap());
        assert!(!app.state.busy);
        assert!(
            !app.goal_auto_continue_armed,
            "late completion must not continue a parent goal in the new session"
        );
        assert!(
            app.state
                .messages
                .iter()
                .all(|message| !message.content.contains("late parent chunk"))
        );
        assert_eq!(app.state.input(), "draft é");
        assert!(app.session_transition.is_none());
        let parent = std::fs::read_to_string(parent_path).unwrap();
        assert!(
            parent.contains("late parent chunk"),
            "the old response must be saved in its parent: {parent}"
        );
    }

    #[tokio::test]
    async fn rewind_preview_refuses_missing_saved_session_before_cancellation() {
        let mut app = app();
        app.state.busy = true;
        app.rewind_saved_turns(1, false, false);
        assert!(app.session_transition.is_none());
        assert!(app.state.busy);
        assert!(
            app.state
                .error
                .as_deref()
                .unwrap()
                .contains("No saved session")
        );
    }
}
