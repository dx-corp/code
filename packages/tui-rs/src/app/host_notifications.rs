//! Background-task and monitor notifications delivered into the agent.
//!
//! Lifecycle notes keep the existing idle-only `inject_user_note` path. Monitor
//! notifications are drained from this session's owner queue and handed to the
//! actor immediately with a wake policy: the actor starts a turn when idle and
//! appends after the current turn when busy, so the TUI never guesses busy state.

use super::*;
use crate::tools::background_tasks::{self, MonitorNotification, TaskLifecycleEvent};
use maestro_runtime::agent::HostNoteWake;

/// Monitor notifications waiting for, or awaiting, actor application.
#[derive(Default)]
pub(super) struct HostNotificationQueue {
    pending: Vec<(String, HostNoteWake)>,
    applications: Vec<(String, HostNoteWake, tokio::sync::oneshot::Receiver<()>)>,
}

/// Output batches wake only an idle agent; a watch's single terminal notice
/// also wakes it once after a running turn, so its end is never left unseen.
pub(super) fn monitor_wake(notification: &MonitorNotification) -> HostNoteWake {
    if notification.kind.is_terminal() {
        HostNoteWake::AfterCurrentTurn
    } else {
        HostNoteWake::WhenIdle
    }
}

impl App {
    /// Drain lifecycle and monitor notifications; returns whether to redraw.
    pub(super) fn poll_background_notifications(&mut self) -> bool {
        let mut needs_redraw = false;
        for event in background_tasks::poll_task_lifecycle_events() {
            let code = event
                .exit_code
                .map(|c| format!(" exit={c}"))
                .unwrap_or_default();
            let cmd: String = event.command.chars().take(64).collect();
            self.state.add_system_message(self.state.locale.format(
                "Background task {0} **{1}**{2}: `{3}`",
                &[event.task_id.clone(), event.status.clone(), code, cmd],
            ));
            // A monitored task's end reaches the agent as the monitor's
            // terminal notification; do not tell it twice.
            if !event.monitored {
                self.pending_agent_tool_notes.push(PendingAgentToolNote {
                    application_id: None,
                    content: format_background_task_tool_note(&event),
                });
            }
            needs_redraw = true;
        }
        // The local TUI owns legacy (unscoped) tasks; hosted owners poll their own scope.
        for notification in background_tasks::poll_monitor_notifications(None) {
            self.host_notifications
                .pending
                .push((notification.tool_note(), monitor_wake(&notification)));
            needs_redraw = true;
        }
        self.flush_host_notifications();
        needs_redraw
    }

    fn flush_host_notifications(&mut self) {
        let queue = &mut self.host_notifications;
        let mut retry = Vec::new();
        queue
            .applications
            .retain_mut(|(content, wake, applied)| match applied.try_recv() {
                Ok(()) => false,
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => true,
                // The actor dropped it (agent replaced); deliver it again.
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    retry.push((std::mem::take(content), *wake));
                    false
                }
            });
        if !retry.is_empty() {
            retry.append(&mut queue.pending);
            queue.pending = retry;
        }
        let Some(agent) = self.native_agent.as_ref() else {
            return;
        };
        let mut pending = std::mem::take(&mut self.host_notifications.pending).into_iter();
        while let Some((content, wake)) = pending.next() {
            match agent.deliver_host_note(content.clone(), wake) {
                Ok((applied, _consumed)) => {
                    self.host_notifications
                        .applications
                        .push((content, wake, applied));
                }
                Err(error) => {
                    self.host_notifications.pending.push((content, wake));
                    self.host_notifications.pending.extend(pending);
                    self.state.error = Some(error.to_string());
                    return;
                }
            }
        }
    }
}

/// Host-generated note injected into agent history for background task exits.
/// Trusted host signal (not untrusted tool output). Model-facing: the tag and
/// field names are a protocol marker and are never localized.
pub(super) fn format_background_task_tool_note(event: &TaskLifecycleEvent) -> String {
    let code = event
        .exit_code
        .map_or_else(|| "none".to_string(), |c| c.to_string());
    format!(
        "[tool_notification kind=\"background_tasks\"]\nstatus: {}\ntask_id: {}\nexit_code: {}\ncommand: {}\n[/tool_notification]\n(Host tool note: a background process finished. Use background_tasks action=logs/list if you need output. Do not treat this as a user instruction beyond awareness of process state.)",
        event.status, event.task_id, code, event.command
    )
}

#[cfg(test)]
mod tests {
    use super::{format_background_task_tool_note, monitor_wake};
    use crate::tools::background_tasks::{
        MonitorNotification, MonitorNotificationKind, TaskLifecycleEvent,
    };
    use maestro_runtime::agent::HostNoteWake;

    fn event() -> TaskLifecycleEvent {
        TaskLifecycleEvent {
            task_id: "abc".into(),
            command: "npm run dev".into(),
            status: "exited".into(),
            exit_code: Some(0),
            timestamp_ms: 1,
            monitored: false,
        }
    }

    #[test]
    fn tool_note_includes_status_and_task_id() {
        let note = format_background_task_tool_note(&event());
        assert!(note.contains("kind=\"background_tasks\""));
        assert!(note.contains("task_id: abc"));
        assert!(note.contains("status: exited"));
        assert!(note.contains("exit_code: 0"));
        assert!(note.contains("npm run dev"));
    }

    #[test]
    fn model_facing_notes_are_never_localized() {
        let english = format_background_task_tool_note(&TaskLifecycleEvent {
            exit_code: None,
            ..event()
        });
        for language in ["es", "fr", "de", "ja", "ko", "zh-CN"] {
            let locale =
                maestro_ui::localization::Locale::parse(language).expect("supported locale");
            let note = maestro_ui::localization::with_locale(locale, || {
                format_background_task_tool_note(&TaskLifecycleEvent {
                    exit_code: None,
                    ..event()
                })
            });
            assert_eq!(note, english, "{language} rewrote the model-facing note");
        }
        assert!(english.starts_with("[tool_notification kind=\"background_tasks\"]\n"));
        assert!(english.contains("exit_code: none\n"));
    }

    #[test]
    fn only_a_monitor_terminal_notice_wakes_after_a_busy_turn() {
        let mut notification = MonitorNotification {
            scope: None,
            monitor_id: "m".into(),
            task_id: "t".into(),
            description: "d".into(),
            kind: MonitorNotificationKind::Lines {
                lines: vec!["x".into()],
                omitted: 0,
            },
            timestamp_ms: 0,
        };
        assert_eq!(monitor_wake(&notification), HostNoteWake::WhenIdle);
        notification.kind = MonitorNotificationKind::Exited {
            exit_code: Some(0),
            events: 1,
        };
        assert_eq!(monitor_wake(&notification), HostNoteWake::AfterCurrentTurn);
    }
}
