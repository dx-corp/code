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
use crate::event::{Output, PrincipalId};

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
        let foreign_summary = self.summarised_principals.iter().any(|p| p != current);
        if !foreign_history && !foreign_evidence && !foreign_inputs && !foreign_summary {
            return Cow::Borrowed(self);
        }
        let mut view = self.clone();
        view.history = scoped_history(&self.history, current);
        if foreign_summary {
            // The summary text was written over another principal's tool
            // activity; it cannot be split by author, so it is not shown.
            view.history
                .retain(|entry| !matches!(entry.message, Message::Summary { .. }));
        }
        view.tool_evidence
            .retain(|evidence| &evidence.call.principal == current);
        view.attachment_inputs
            .retain(|input| &input.principal == current);
        Cow::Owned(view)
    }
}

impl Context {
    /// True when `reference` is a stored output only other principals' calls
    /// produced in this thread's retained records. A reference this context
    /// has no record of is not hidden: it stays bound to the thread by the
    /// store, which pins refs to immutable versions.
    pub fn output_ref_hidden_from(&self, reference: &str, principal: &PrincipalId) -> bool {
        let reference = reference.trim();
        let is_ref = |output: &Output| matches!(output, Output::Ref(r) if r.as_str() == reference);
        let mut foreign = false;
        let mut own = false;
        let mut note = |owner: &PrincipalId| {
            if owner == principal {
                own = true;
            } else {
                foreign = true;
            }
        };
        for evidence in &self.tool_evidence {
            if is_ref(&evidence.result.output) {
                note(&evidence.call.principal);
            }
        }
        for entry in &self.history {
            if let Message::Tool { call, output, .. } = &entry.message
                && is_ref(output)
                && let Some(proposed) = self.proposed_call(call)
            {
                note(&proposed.principal);
            }
        }
        foreign && !own
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
