//! Cloud turns use the public application SDK wire contract. The Platform
//! projects these calls onto its internal operating-thread implementation.

use anyhow::{Context, Result, anyhow};
use maestro_local_host::credential_mode::PlatformSession;
use maestro_local_host::hosted_thread::{Event, ThreadClient};

const PLATFORM_BASE_URL_ENV: &[&str] = &[
    "MAESTRO_CLOUD_PLATFORM_URL",
    "DEIXIC_PLATFORM_URL",
    "MAESTRO_PLATFORM_BASE_URL",
    "MAESTRO_EVALOPS_BASE_URL",
    "EVALOPS_BASE_URL",
];

pub(crate) mod request_type {
    pub const APPROVAL: &str = "approval";
    pub const USER_INPUT: &str = "user_input";
}

pub mod event_kind {
    pub const ASSISTANT_TEXT_DELTA: &str = "assistant_text_delta";
    pub const TOOL_PROPOSED: &str = "tool_proposed";
    pub const MODEL_ATTEMPT_ABANDONED: &str = "model_attempt_abandoned";
    pub const APPROVAL_REQUIRED: &str = "approval_required";
    pub const INPUT_REQUIRED: &str = "input_required";
    pub const TURN_COMPLETED: &str = "turn_completed";
    pub const TURN_FAILED: &str = "turn_failed";
    pub const TURN_INTERRUPTED: &str = "turn_interrupted";
}

#[derive(Debug, Clone)]
pub struct DeixicOperatingConfig {
    pub base_url: String,
    pub token: String,
    pub organization_id: String,
    pub workspace_id: String,
}

impl DeixicOperatingConfig {
    pub fn resolve(
        base_url_override: Option<&str>,
        token: String,
        organization_id: String,
        workspace_id: String,
    ) -> Result<Self> {
        let base_url = base_url_override
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                PLATFORM_BASE_URL_ENV.iter().find_map(|name| {
                    std::env::var(name).ok().map(|value| value.trim().to_owned()).filter(|value| !value.is_empty())
                })
            })
            .ok_or_else(|| anyhow!("cloud mode needs a platform API URL. Set MAESTRO_CLOUD_PLATFORM_URL (or MAESTRO_EVALOPS_BASE_URL)."))?;
        Ok(Self {
            base_url,
            token,
            organization_id,
            workspace_id,
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct ThreadEvent {
    pub cursor: u64,
    pub turn_id: String,
    pub kind: String,
    pub text: String,
    pub terminal_code: String,
    pub terminal_message: String,
    pub request_id: Option<String>,
    pub request_type: Option<String>,
    pub tool_name: Option<String>,
    pub call_id: String,
}

fn event_kind(kind: i32) -> &'static str {
    match kind {
        12 => event_kind::ASSISTANT_TEXT_DELTA,
        13 => "assistant_text_observed",
        14 => event_kind::MODEL_ATTEMPT_ABANDONED,
        15 => event_kind::TOOL_PROPOSED,
        4 => event_kind::APPROVAL_REQUIRED,
        5 => event_kind::INPUT_REQUIRED,
        7 => event_kind::TURN_COMPLETED,
        8 => event_kind::TURN_FAILED,
        9 => event_kind::TURN_INTERRUPTED,
        10 => "client_tool_required",
        _ => "other",
    }
}

fn from_event(event: Event) -> ThreadEvent {
    ThreadEvent {
        cursor: event.cursor.max(0) as u64,
        turn_id: event.turn_id,
        kind: event_kind(event.kind).to_owned(),
        text: event.safe_text,
        terminal_code: event.terminal_code,
        terminal_message: event.terminal_message,
        request_id: (!event.request_id.is_empty()).then_some(event.request_id),
        request_type: match event.request_type {
            1 => Some(request_type::APPROVAL.to_owned()),
            2 => Some(request_type::USER_INPUT.to_owned()),
            _ => None,
        },
        tool_name: (!event.tool_name.is_empty()).then_some(event.tool_name),
        call_id: event.request_call_id,
    }
}

#[derive(Debug, Clone, Default)]
pub struct EventsPage {
    pub events: Vec<ThreadEvent>,
    pub next_cursor: u64,
    pub has_more: bool,
    pub reset_required: bool,
    pub active_turn_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SubmittedTurn {
    pub turn_id: String,
    pub replay_cursor: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnEnd {
    Completed,
    Failed(String),
    Interrupted(String),
}

pub fn classify_turn_end(event: &ThreadEvent) -> Option<TurnEnd> {
    match event.kind.as_str() {
        event_kind::TURN_COMPLETED => Some(TurnEnd::Completed),
        event_kind::TURN_FAILED => Some(TurnEnd::Failed(if event.terminal_code.is_empty() {
            event.terminal_message.clone()
        } else {
            format!("{}: {}", event.terminal_code, event.terminal_message)
        })),
        event_kind::TURN_INTERRUPTED => Some(TurnEnd::Interrupted(event.terminal_message.clone())),
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub struct DeixicOperatingClient {
    config: DeixicOperatingConfig,
}

impl DeixicOperatingClient {
    #[must_use]
    pub fn new(config: DeixicOperatingConfig) -> Self {
        Self { config }
    }

    fn client(&self, thread_id: &str) -> Result<ThreadClient> {
        ThreadClient::new(
            PlatformSession {
                access_token: self.config.token.clone(),
                organization_id: self.config.organization_id.clone(),
                workspace_id: Some(self.config.workspace_id.clone()),
                provider_ref: serde_json::Value::Null,
                email: None,
                user_id: None,
            },
            thread_id.to_owned(),
            &self.config.base_url,
        )
    }

    pub async fn submit_message(
        &self,
        thread_id: &str,
        body: &str,
        idempotency_key: &str,
    ) -> Result<SubmittedTurn> {
        let result = self
            .client(thread_id)?
            .submit_with_key(body.to_owned(), idempotency_key.to_owned())
            .await?;
        Ok(SubmittedTurn {
            turn_id: result
                .accepted_turn
                .context("Platform did not accept cloud turn")?
                .turn_id,
            replay_cursor: result.replay_cursor.max(0) as u64,
        })
    }

    pub async fn list_events(&self, thread_id: &str, after_cursor: u64) -> Result<EventsPage> {
        let cursor =
            i64::try_from(after_cursor).context("event cursor exceeds public protocol range")?;
        let result = self.client(thread_id)?.events(cursor).await?;
        Ok(EventsPage {
            events: result.events.into_iter().map(from_event).collect(),
            next_cursor: result.next_cursor.max(0) as u64,
            has_more: result.has_more,
            reset_required: result.reset_required,
            active_turn_id: result.active_turn_id,
        })
    }

    /// A published client may reach an older Platform projection that does
    /// not yet label assistant text deltas. The durable message still exists.
    pub async fn latest_assistant_message(&self, thread_id: &str) -> Result<Option<String>> {
        Ok(self
            .client(thread_id)?
            .get()
            .await?
            .messages
            .into_iter()
            .rev()
            .find(|message| message.role == "assistant")
            .map(|message| message.body))
    }

    async fn respond(
        &self,
        thread_id: &str,
        turn_id: &str,
        event: &ThreadEvent,
        action: i32,
        text: &str,
    ) -> Result<()> {
        let request_id = event
            .request_id
            .as_deref()
            .context("pending request has no id")?;
        let request_kind = match event.request_type.as_deref() {
            Some(request_type::APPROVAL) => 1,
            Some(request_type::USER_INPUT) => 2,
            _ => return Err(anyhow!("unsupported cloud request type")),
        };
        self.client(thread_id)?
            .respond_with_key(
                &Event {
                    turn_id: turn_id.to_owned(),
                    request_id: request_id.to_owned(),
                    request_call_id: event.call_id.clone(),
                    request_type: request_kind,
                    ..Event::default()
                },
                action,
                text.to_owned(),
                format!("cloud-response:{request_id}"),
            )
            .await?;
        Ok(())
    }

    pub async fn approve(&self, thread_id: &str, turn_id: &str, event: &ThreadEvent) -> Result<()> {
        self.respond(thread_id, turn_id, event, 1, "").await
    }

    pub async fn answer(
        &self,
        thread_id: &str,
        turn_id: &str,
        event: &ThreadEvent,
        text: &str,
    ) -> Result<()> {
        self.respond(thread_id, turn_id, event, 3, text).await
    }

    pub async fn interrupt(
        &self,
        thread_id: &str,
        turn_id: Option<&str>,
        reason: &str,
    ) -> Result<()> {
        self.client(thread_id)?.interrupt(turn_id, reason).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_event_projection_preserves_terminal_and_request_details() {
        let event = from_event(Event {
            cursor: 5,
            turn_id: "turn-1".into(),
            kind: 8,
            terminal_code: "provider_stream_timeout".into(),
            terminal_message: "gateway timed out".into(),
            ..Event::default()
        });
        assert_eq!(
            classify_turn_end(&event),
            Some(TurnEnd::Failed(
                "provider_stream_timeout: gateway timed out".into()
            ))
        );
        let request = from_event(Event {
            kind: 4,
            request_type: 1,
            request_id: "r1".into(),
            request_call_id: "call-1".into(),
            ..Event::default()
        });
        assert_eq!(request.kind, event_kind::APPROVAL_REQUIRED);
        assert_eq!(
            request.request_type.as_deref(),
            Some(request_type::APPROVAL)
        );
        assert_eq!(request.call_id, "call-1");
        assert_eq!(
            from_event(Event {
                kind: 12,
                safe_text: "hello".into(),
                ..Event::default()
            })
            .kind,
            event_kind::ASSISTANT_TEXT_DELTA
        );
        assert_eq!(
            from_event(Event {
                kind: 14,
                ..Event::default()
            })
            .kind,
            event_kind::MODEL_ATTEMPT_ABANDONED
        );
        assert_eq!(
            from_event(Event {
                kind: 15,
                tool_name: "bash".into(),
                ..Event::default()
            })
            .tool_name
            .as_deref(),
            Some("bash")
        );
    }
}
