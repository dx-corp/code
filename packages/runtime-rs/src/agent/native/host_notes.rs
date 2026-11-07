//! Host notes: trusted host-generated user notes (background-task lifecycle,
//! monitor output) appended to the conversation by the actor.
//!
//! A note is never inserted mid-turn: while a turn runs it is deferred and
//! appended when the turn ends, so provider message order stays valid. A note
//! with a [`HostNoteWake`] policy also starts a turn (the existing
//! [`AgentCommand::Continue`] path) when it is appended to an idle agent.

use super::*;

/// When a host note may start a model turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostNoteWake {
    /// Start a turn if the agent is idle when the note arrives. While a turn
    /// runs, the note is appended after it without starting another, so
    /// steady output cannot keep the agent busy.
    WhenIdle,
    /// Like `WhenIdle`, but a note that arrives during a turn still starts one
    /// follow-up turn after it. For bounded, must-see notices such as a
    /// monitor's single terminal notification.
    AfterCurrentTurn,
}

impl HostNoteWake {
    /// The policy a note keeps once it is deferred behind a running turn.
    pub(super) fn after_busy_turn(wake: Option<Self>) -> Option<Self> {
        wake.filter(|wake| *wake == Self::AfterCurrentTurn)
    }
}

impl NativeAgent {
    /// Append a host tool/user note to history without starting a turn.
    /// While a turn runs the actor defers it until the turn ends.
    pub fn inject_user_note(
        &self,
        content: impl Into<String>,
    ) -> Result<(
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Receiver<()>,
    )> {
        self.send_host_note(content.into(), None)
    }

    /// Deliver a host notification: append it and start a turn when the agent
    /// is idle; while busy, append it after the current turn per `wake`.
    /// `applied` resolves once it is in history; `consumed` once a successful
    /// request has carried it to the model.
    pub fn deliver_host_note(
        &self,
        content: impl Into<String>,
        wake: HostNoteWake,
    ) -> Result<(
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Receiver<()>,
    )> {
        self.send_host_note(content.into(), Some(wake))
    }

    fn send_host_note(
        &self,
        content: String,
        wake: Option<HostNoteWake>,
    ) -> Result<(
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Receiver<()>,
    )> {
        if content.trim().is_empty() {
            anyhow::bail!("Cannot inject an empty user note");
        }
        let (applied, applied_receiver) = tokio::sync::oneshot::channel();
        let (consumed, consumed_receiver) = tokio::sync::oneshot::channel();
        self.command_tx
            .send(AgentCommand::InjectUserNote {
                content,
                wake,
                applied,
                consumed,
            })
            .map_err(|error| anyhow::anyhow!("Failed to inject user note: {error}"))?;
        Ok((applied_receiver, consumed_receiver))
    }
}

impl NativeAgentRunner {
    /// A waking note was appended to an idle agent. Append the notes queued
    /// directly behind it too, so one turn sees them all, then start that turn
    /// unless a queued prompt is about to start one anyway.
    pub(super) fn wake_for_host_notes(&mut self) {
        while matches!(
            self.deferred_commands.front(),
            Some(AgentCommand::InjectUserNote { .. })
        ) {
            let Some(AgentCommand::InjectUserNote {
                content,
                applied,
                consumed,
                ..
            }) = self.deferred_commands.pop_front()
            else {
                break;
            };
            self.apply_user_note(content, consumed);
            let _ = applied.send(());
        }
        let turn_pending = !self.pending_messages.is_empty()
            || self.deferred_commands.iter().any(|command| {
                matches!(
                    command,
                    AgentCommand::Prompt { .. } | AgentCommand::Continue
                )
            });
        if !turn_pending {
            self.deferred_commands.push_front(AgentCommand::Continue);
        }
    }
}
