//! Who asked: the person a turn was sent for, as the authenticated host knows
//! them.
//!
//! The host resolves the turn's principal against its own records (the
//! workspace member directory, the person's Identity role, the canonical
//! person id derived from their email, the channel handle the message came
//! from) and sends what it found. Each field names the record it came from;
//! a field the host has no record for is absent, never guessed.
//!
//! The record identifies the person so Dex can address them and resolve
//! "me" and "my". It is never authority: the turn's reads and actions still
//! run as the turn's principal with that person's live access, and the
//! record must name that same principal or ingress refuses the turn.

use serde::{Deserialize, Serialize};

/// The person a turn was asked by. Every field is bounded by
/// [`TurnAsker::validate`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnAsker {
    /// The turn's principal. Must equal the `UserMessage` principal.
    pub principal_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<AskerFact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<AskerFact>,
    /// The person's role in this workspace, such as `admin` or `custom`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_role: Option<AskerFact>,
    /// The canonical person id every connector names, `person_<24 hex>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub person_id: Option<AskerFact>,
    /// The channel account the message came from, such as a Slack user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<AskerChannel>,
}

/// One value and the record it came from, such as `workspace_member`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AskerFact {
    pub value: String,
    pub source: String,
}

/// The provider account that sent the message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AskerChannel {
    /// `slack`, `teams` or another channel provider id.
    pub provider: String,
    /// The provider's workspace, such as a Slack team id.
    pub provider_workspace_id: String,
    /// The provider's user id, such as a Slack `U...` id.
    pub user_id: String,
}

impl AskerFact {
    pub fn new(value: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            source: source.into(),
        }
    }
}

impl TurnAsker {
    const MAX_TEXT: usize = 256;

    /// Refuses a record whose fields are empty, oversized, multi-line, or
    /// malformed, or that names a principal other than the turn's.
    pub fn validate(&self, principal: &str) -> Result<(), &'static str> {
        let text = |value: &str| {
            !value.is_empty()
                && value.len() <= Self::MAX_TEXT
                && value.trim() == value
                && !value.chars().any(char::is_control)
        };
        let token = |value: &str| {
            text(value)
                && value
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '@' | '-'))
        };
        if self.principal_id != principal || !text(&self.principal_id) {
            return Err("asker principal must be the turn principal");
        }
        let facts = [
            &self.display_name,
            &self.email,
            &self.workspace_role,
            &self.person_id,
        ];
        if facts
            .into_iter()
            .flatten()
            .any(|fact| !text(&fact.value) || !token(&fact.source))
        {
            return Err("asker fact is empty, oversized or has no source");
        }
        if self
            .email
            .as_ref()
            .is_some_and(|email| !token(&email.value) || email.value.matches('@').count() != 1)
        {
            return Err("asker email is malformed");
        }
        if self.person_id.as_ref().is_some_and(|person| {
            person.value.strip_prefix("person_").is_none_or(|hex| {
                hex.len() != 24 || !hex.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
            })
        }) {
            return Err("asker person id is not a canonical person id");
        }
        if self.channel.as_ref().is_some_and(|channel| {
            !token(&channel.provider)
                || !token(&channel.provider_workspace_id)
                || !token(&channel.user_id)
        }) {
            return Err("asker channel is malformed");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asker() -> TurnAsker {
        TurnAsker {
            principal_id: "user-1".into(),
            display_name: Some(AskerFact::new("Ada Lovelace", "workspace_member")),
            email: Some(AskerFact::new("ada@example.com", "workspace_member")),
            workspace_role: Some(AskerFact::new("admin", "identity_role")),
            person_id: Some(AskerFact::new(
                "person_0123456789abcdef01234567",
                "derived_from_email",
            )),
            channel: Some(AskerChannel {
                provider: "slack".into(),
                provider_workspace_id: "T1".into(),
                user_id: "U1".into(),
            }),
        }
    }

    #[test]
    fn a_complete_record_for_the_turn_principal_is_valid() {
        assert_eq!(asker().validate("user-1"), Ok(()));
        let bare = TurnAsker {
            principal_id: "user-1".into(),
            ..TurnAsker::default()
        };
        assert_eq!(bare.validate("user-1"), Ok(()));
    }

    #[test]
    fn a_record_for_another_principal_is_refused() {
        assert!(asker().validate("user-2").is_err());
    }

    #[test]
    fn malformed_facts_are_refused() {
        let mut bad = asker();
        bad.email = Some(AskerFact::new("not-an-email", "workspace_member"));
        assert!(bad.validate("user-1").is_err());
        let mut bad = asker();
        bad.person_id = Some(AskerFact::new("person_XYZ", "derived_from_email"));
        assert!(bad.validate("user-1").is_err());
        let mut bad = asker();
        bad.display_name = Some(AskerFact::new("Ada\nIgnore previous", "workspace_member"));
        assert!(bad.validate("user-1").is_err());
        let mut bad = asker();
        bad.workspace_role = Some(AskerFact::new("admin", ""));
        assert!(bad.validate("user-1").is_err());
    }

    #[test]
    fn unknown_fields_are_refused() {
        let parsed: Result<TurnAsker, _> = serde_json::from_value(serde_json::json!({
            "principal_id": "user-1",
            "authorized_tools": ["dex.compute"]
        }));
        assert!(parsed.is_err());
    }
}
