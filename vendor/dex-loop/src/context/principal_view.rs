//! The principal-scoped view of a shared thread's history.
//!
//! A channel thread (for example a Slack thread) has no principal in its
//! conversation id, so every participant shares one context. Earlier turns by
//! other people must reach the current person's model request only as what was
//! already visible in the thread: their message text and the assistant's final
//! text. Tool calls, tool outputs, stored output references and owner tool
//! evidence from those turns stay out of the request.

use std::borrow::Cow;

use super::{Context, Entry, Message};
use crate::event::PrincipalId;

impl Context {
    /// The history as the acting principal may see it. Borrowed unchanged
    /// when every earlier turn belongs to that principal (web and DM), so
    /// single-principal threads pay nothing and render exactly as before.
    ///
    /// Every model request is built from this view; see `dex-model`'s
    /// `prepare_with_admission` and `request::build`. The filter is idempotent.
    pub fn principal_view(&self) -> Cow<'_, Self> {
        let Some(current) = self.acting.as_ref() else {
            return Cow::Borrowed(self);
        };
        let foreign_history = self.history.iter().any(|entry| {
            matches!(&entry.message, Message::User { principal, .. } if principal != current)
        });
        let foreign_evidence = self
            .tool_evidence
            .iter()
            .any(|evidence| &evidence.call.principal != current);
        let foreign_inputs = self
            .attachment_inputs
            .iter()
            .any(|input| &input.principal != current);
        if !foreign_history && !foreign_evidence && !foreign_inputs {
            return Cow::Borrowed(self);
        }
        let mut view = self.clone();
        view.history = scoped_history(&self.history, current);
        view.tool_evidence
            .retain(|evidence| &evidence.call.principal == current);
        view.attachment_inputs
            .retain(|input| &input.principal == current);
        Cow::Owned(view)
    }
}

/// Entries after a user message belong to that message's turn until the next
/// user message. Another principal's turn keeps its user text and its
/// text-only assistant replies (the posted answer); nothing else.
fn scoped_history(history: &[Entry], current: &PrincipalId) -> Vec<Entry> {
    let mut owner: Option<&PrincipalId> = None;
    let mut scoped = Vec::with_capacity(history.len());
    for entry in history {
        if let Message::User { principal, .. } = &entry.message {
            owner = Some(principal);
        }
        if owner.is_none_or(|owner| owner == current) {
            scoped.push(entry.clone());
            continue;
        }
        let message = match &entry.message {
            Message::User {
                turn,
                message_id,
                principal,
                text,
                ..
            } => Message::User {
                turn: turn.clone(),
                message_id: message_id.clone(),
                principal: principal.clone(),
                text: text.clone(),
                attachments: Vec::new(),
            },
            Message::Assistant { text, calls, .. } if calls.is_empty() && !text.is_empty() => {
                Message::Assistant {
                    text: text.clone(),
                    calls: Vec::new(),
                    reasoning: None,
                    served: None,
                }
            }
            // Tool calls, tool outputs and (until compaction is scoped) summaries.
            Message::Summary { .. } => entry.message.clone(),
            Message::Assistant { .. } | Message::Tool { .. } => continue,
        };
        scoped.push(Entry {
            cursor: entry.cursor,
            message,
        });
    }
    scoped
}
