//! Select, review, and durably branch a summary without editing its source.
use super::*;
use crate::agent::selective_summary::{
    RangeSelection, SelectiveSummaryPreview, SelectiveSummaryRequest, SelectiveSummaryResult,
    SummaryTurn,
};
use maestro_ui::{ActionPicker, KeyHint, Modal, PickerOptions};
use ratatui::{
    Frame,
    layout::Rect,
    widgets::{ListItem, Paragraph, Wrap},
};
use tokio::sync::oneshot::{self, error::TryRecvError};

enum Stage {
    Loading(oneshot::Receiver<Result<SelectiveSummaryPreview>>),
    Picking {
        preview: SelectiveSummaryPreview,
        picker: ActionPicker<SummaryTurn>,
        through: bool,
    },
    Running {
        request: SelectiveSummaryRequest,
        digest: String,
        cancelled: bool,
    },
    Review {
        result: SelectiveSummaryResult,
        digest: String,
        scroll: u16,
    },
}

pub(super) struct SummaryDialog {
    stage: Stage,
    instructions: Option<String>,
}

impl SummaryDialog {
    pub(super) fn render(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::themes::current_ui_theme();
        let title = match &self.stage {
            Stage::Picking { through: true, .. } => {
                maestro_ui::localization::tr("Summarize · start through selected turn")
            }
            Stage::Picking { .. } => {
                maestro_ui::localization::tr("Summarize · selected turn through end")
            }
            Stage::Review { .. } => {
                maestro_ui::localization::tr("Review summary · Enter saves a new conversation")
            }
            _ => maestro_ui::localization::tr("Summarize conversation"),
        };
        let inner = Modal::new(title, 88, area.height.saturating_sub(4).max(5))
            .theme(theme)
            .render(frame, area);
        match &mut self.stage {
            Stage::Picking { picker, .. } => picker.render(frame, inner, theme, PickerOptions {
                empty: maestro_ui::localization::tr("No complete turns to summarize"),
                hints: Some(&[KeyHint::new("↑↓", "turn"), KeyHint::new("f", maestro_ui::localization::tr("from here")), KeyHint::new("t", maestro_ui::localization::tr("up to here")), KeyHint::new("Enter", "generate"), KeyHint::new("Esc", maestro_ui::localization::tr("cancel"))]),
                ..PickerOptions::default()
            }, |turn| ListItem::new(format!("{}. {}", turn.number, turn.preview))),
            Stage::Loading(_) => frame.render_widget(Paragraph::new(maestro_ui::localization::tr("Reading current model context… Esc cancels")).style(theme.on_panel().text_style()), inner),
            Stage::Running { cancelled, .. } => frame.render_widget(Paragraph::new(if *cancelled { maestro_ui::localization::tr("Cancelling summary…") } else { maestro_ui::localization::tr("Generating summary… Esc cancels. Original conversation stays intact.") }).wrap(Wrap { trim: false }).style(theme.on_panel().text_style()), inner),
            Stage::Review { result, scroll, .. } => frame.render_widget(Paragraph::new(maestro_ui::localization::format("Turns {0}–{1} of {2} · original conversation stays available\nEnter: save and continue in child · Esc: discard · ↑↓: scroll\n\n{3}", &[(result.first_turn).to_string(), (result.last_turn).to_string(), (result.total_turns).to_string(), (result.summary).clone()])).wrap(Wrap { trim: false }).scroll((*scroll, 0)).style(theme.on_panel().text_style()), inner),
        }
    }
}

impl App {
    pub(super) fn open_selective_summary(&mut self) {
        self.open_selective_summary_with_instructions(None);
    }

    pub(super) fn open_selective_summary_with_instructions(
        &mut self,
        instructions: Option<String>,
    ) {
        if self.state.busy || !self.queued_prompts.is_empty() {
            self.state.status = Some(
                self.state
                    .locale
                    .translate("Finish the active response and queued prompts before summarizing.")
                    .into(),
            );
            return;
        }
        let result = self
            .native_agent
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Agent is not ready"))
            .and_then(|agent| agent.start_selective_summary_preview());
        match result {
            Ok(receiver) => {
                self.selective_summary = Some(SummaryDialog {
                    stage: Stage::Loading(receiver),
                    instructions,
                });
                self.active_modal = ActiveModal::SelectiveSummary;
            }
            Err(error) => {
                self.state.error = Some(
                    self.state
                        .locale
                        .format("Cannot summarize: {0}", &[(error).to_string()]),
                );
            }
        }
    }

    pub(super) fn selective_summary_in_flight(&self) -> bool {
        matches!(
            self.selective_summary.as_ref().map(|dialog| &dialog.stage),
            Some(Stage::Loading(_) | Stage::Running { .. })
        )
    }

    pub(super) fn poll_selective_summary(&mut self) -> bool {
        let Some(mut dialog) = self.selective_summary.take() else {
            return false;
        };
        let mut changed = false;
        let mut close = false;
        match &mut dialog.stage {
            Stage::Loading(receiver) => match receiver.try_recv() {
                Ok(Ok(preview)) if !preview.turns.is_empty() => {
                    let mut picker = ActionPicker::new(preview.turns.clone());
                    picker.open();
                    dialog.stage = Stage::Picking {
                        preview,
                        picker,
                        through: false,
                    };
                    changed = true;
                }
                Ok(Ok(_)) => {
                    self.state.status = Some(
                        self.state
                            .locale
                            .translate("No conversation turns to summarize.")
                            .into(),
                    );
                    close = true;
                }
                Ok(Err(error)) => {
                    self.state.error = Some(
                        self.state
                            .locale
                            .format("Cannot summarize: {0}", &[(error).to_string()]),
                    );
                    close = true;
                }
                Err(TryRecvError::Closed) => {
                    self.state.error = Some(
                        self.state
                            .locale
                            .translate("Summary preview stopped.")
                            .into(),
                    );
                    close = true;
                }
                Err(TryRecvError::Empty) => {}
            },
            Stage::Running {
                request,
                digest,
                cancelled,
            } => match request.receiver.try_recv() {
                Ok(outcome) => {
                    let mut usage_recorded = true;
                    if let Some(usage) = outcome.usage {
                        for alert in self.usage_tracker.add_turn(&to_headless_usage(&usage)) {
                            self.state.add_system_message(alert);
                        }
                        match crate::session::selective_summary_usage_entry(
                            &self.current_model,
                            &usage,
                        ) {
                            Ok(entry) => {
                                usage_recorded =
                                    self.write_session_entry(entry) && self.flush_session();
                            }
                            Err(error) => {
                                usage_recorded = false;
                                self.state.error = Some(self.state.locale.format(
                                    "Failed to record summary usage: {0}",
                                    &[(error).to_string()],
                                ));
                            }
                        }
                    }
                    if !usage_recorded {
                        close = true;
                    } else if *cancelled {
                        self.state.status = Some(
                            self.state
                                .locale
                                .translate("Summary cancelled; original conversation preserved.")
                                .into(),
                        );
                        close = true;
                    } else {
                        match outcome.result {
                            Ok(result) => {
                                dialog.stage = Stage::Review {
                                    result,
                                    digest: digest.clone(),
                                    scroll: 0,
                                };
                                changed = true;
                            }
                            Err(error) => {
                                self.state.error = Some(
                                    self.state
                                        .locale
                                        .format("Summary failed: {0}", &[(error).to_string()]),
                                );
                                close = true;
                            }
                        }
                    }
                }
                Err(TryRecvError::Closed) => {
                    self.state.error = Some(
                        self.state
                            .locale
                            .translate("Summary request stopped; original conversation preserved.")
                            .into(),
                    );
                    close = true;
                }
                Err(TryRecvError::Empty) => {}
            },
            _ => {}
        }
        if close {
            self.active_modal = ActiveModal::None;
            true
        } else {
            self.selective_summary = Some(dialog);
            changed
        }
    }

    pub(super) async fn handle_selective_summary_key(&mut self, code: KeyCode) -> Result<()> {
        let Some(mut dialog) = self.selective_summary.take() else {
            self.active_modal = ActiveModal::None;
            return Ok(());
        };
        let mut close = false;
        match &mut dialog.stage {
            Stage::Running {
                request, cancelled, ..
            } => {
                if code == KeyCode::Esc {
                    request.cancellation.cancel();
                    *cancelled = true;
                }
            }
            Stage::Picking {
                preview,
                picker,
                through,
            } => match code {
                KeyCode::Esc => close = true,
                KeyCode::Char('f') => *through = false,
                KeyCode::Char('t') => *through = true,
                KeyCode::Enter => {
                    if let Some(turn) = picker.selected() {
                        let selection = if *through {
                            RangeSelection::ThroughTurn(turn.number)
                        } else {
                            RangeSelection::FromTurn(turn.number)
                        };
                        let started = self.ensure_session_started().and_then(|()| {
                            self.native_agent
                                .as_ref()
                                .ok_or_else(|| anyhow::anyhow!("Agent stopped"))?
                                .start_selective_summary_with_instructions(
                                    selection,
                                    preview.history_digest.clone(),
                                    dialog.instructions.clone(),
                                )
                        });
                        match started {
                            Ok(request) => {
                                dialog.stage = Stage::Running {
                                    request,
                                    digest: preview.history_digest.clone(),
                                    cancelled: false,
                                }
                            }
                            Err(error) => {
                                self.state.error = Some(
                                    self.state
                                        .locale
                                        .format("Cannot summarize: {0}", &[(error).to_string()]),
                                );
                                close = true;
                            }
                        }
                    }
                }
                KeyCode::Up
                | KeyCode::Down
                | KeyCode::PageUp
                | KeyCode::PageDown
                | KeyCode::Home
                | KeyCode::End => {
                    picker.handle_key(code, false);
                }
                _ => {}
            },
            Stage::Review {
                result,
                digest,
                scroll,
            } => match code {
                KeyCode::Esc => close = true,
                KeyCode::Up => *scroll = scroll.saturating_sub(1),
                KeyCode::Down => *scroll = scroll.saturating_add(1),
                KeyCode::Enter => {
                    if let Err(error) = self.save_selective_summary(result, digest.clone()).await {
                        self.state.error = Some(
                            self.state
                                .locale
                                .format("Could not apply summary: {0}", &[(error).to_string()]),
                        );
                    }
                    close = true;
                }
                _ => {}
            },
            Stage::Loading(_) => {
                if code == KeyCode::Esc {
                    close = true;
                }
            }
        }
        if close {
            self.active_modal = ActiveModal::None;
        } else {
            self.selective_summary = Some(dialog);
        }
        Ok(())
    }

    pub(super) async fn save_selective_summary(
        &mut self,
        result: &SelectiveSummaryResult,
        digest: String,
    ) -> Result<()> {
        self.ensure_session_started()?;
        let (child_id, child_path) = self.session_manager.fork_session_snapshot()?;
        let prepared = async {
            crate::session::append_selective_summary_checkpoint(&child_path, &result.messages)?;
            // Keep the source writer locked until both persistence and agent adoption succeed.
            let prepared = self.session_manager.prepare_session_adoption(&child_path)?;
            let child = crate::session::SessionReader::read_file(&child_path)?;
            let receiver = self
                .native_agent
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Agent stopped"))?
                .apply_selective_summary(result.messages.clone(), digest)?;
            receiver
                .await
                .map_err(|_| anyhow::anyhow!("Agent stopped before applying summary"))??;
            Ok::<_, anyhow::Error>((prepared, child))
        }
        .await;
        let (prepared, child) = match prepared {
            Ok(child) => child,
            Err(error) => {
                std::fs::remove_file(&child_path).map_err(|cleanup| {
                    anyhow::anyhow!(
                        "{error}; failed to remove abandoned summary {}: {cleanup}",
                        child_path.display()
                    )
                })?;
                return Err(error);
            }
        };
        self.session_manager.adopt_prepared_session(prepared);
        self.reset_rendered_viewport();
        restore_visible_session_messages(&mut self.state, &child);
        self.state.session_id = Some(child_id.clone());
        self.adopt_compacted_session_context(&child_id);
        self.persist_request_cache();
        if !self.flush_session() {
            anyhow::bail!("Failed to persist the adopted checkpoint cache audit");
        }
        self.session_resume_failed = false;
        let notice = self.state.locale.format(
            "Summary saved in {0}. Original conversation remains available in /sessions.",
            std::slice::from_ref(&(child_id)),
        );
        self.state.status = Some(notice.clone());
        self.state.add_system_message(notice);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn render(dialog: &mut SummaryDialog, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| dialog.render(frame, frame.area()))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn summary_dialog_shows_range_direction_and_selected_turn() {
        let turns = vec![
            SummaryTurn {
                number: 1,
                preview: "First request".into(),
            },
            SummaryTurn {
                number: 2,
                preview: "Second request".into(),
            },
        ];
        let mut picker = ActionPicker::new(turns.clone());
        picker.open();
        picker.handle_key(KeyCode::Down, false);
        let mut dialog = SummaryDialog {
            instructions: None,
            stage: Stage::Picking {
                preview: SelectiveSummaryPreview {
                    turns,
                    history_digest: "test".into(),
                },
                picker,
                through: false,
            },
        };
        let text = render(&mut dialog, 100, 20);
        assert!(text.contains("selected turn through end"));
        assert!(text.contains("Second request"));
        assert!(text.contains("from here"));
        assert!(text.contains("up to here"));
        if let Stage::Picking {
            picker, through, ..
        } = &mut dialog.stage
        {
            assert_eq!(picker.selected().unwrap().number, 2);
            *through = true;
        }
        assert!(render(&mut dialog, 100, 20).contains("start through selected turn"));
        for (width, height) in [(32, 10), (4, 3), (1, 1)] {
            render(&mut dialog, width, height);
        }
    }

    #[test]
    fn summary_dialog_review_displays_result_and_discard_action() {
        let mut dialog = SummaryDialog {
            instructions: None,
            stage: Stage::Review {
                result: SelectiveSummaryResult {
                    messages: Vec::new(),
                    summary: "Retain the agreed constraints.".into(),
                    first_turn: 2,
                    last_turn: 4,
                    total_turns: 5,
                },
                digest: "test".into(),
                scroll: 0,
            },
        };
        let text = render(&mut dialog, 100, 20);
        assert!(text.contains("Turns 2–4 of 5"));
        assert!(text.contains("Esc: discard"));
        assert!(text.contains("Retain the agreed constraints."));
        assert!(text.contains("Enter saves a new conversation"));
        render(&mut dialog, 32, 10);
    }
}
