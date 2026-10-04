//! The writing policy one turn runs under: the workspace Content & AI policy
//! with the sender's voice choice already applied by the authenticated host.
//!
//! This is prompt data, never authority. It grants no tool, connector, or
//! model access, and the loop never reads it: the model port renders it into
//! the turn's stored context. Logged on the turn's `UserMessage`, so every
//! step, resume, and replica of the turn writes under the same policy even if
//! the workspace edits its style guide mid-turn.

use serde::{Deserialize, Serialize};

/// One turn's resolved voice. Empty (no policy, no tone) renders nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnVoice {
    /// The workspace Content & AI policy, present only when the workspace
    /// has it enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<TurnContentPolicy>,
    /// Short adjustments the sender asked for on top of the voice.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tone: Vec<ToneAdjustment>,
}

impl TurnVoice {
    /// True when there is nothing to render for the turn.
    pub fn is_empty(&self) -> bool {
        self.policy.is_none() && self.tone.is_empty()
    }
}

/// The workspace Content & AI policy snapshot admitted for one turn.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnContentPolicy {
    pub guide_version: u64,
    #[serde(default)]
    pub response_guidance: String,
    #[serde(default)]
    pub document_guidance: String,
    #[serde(default)]
    pub presentation_guidance: String,
    #[serde(default)]
    pub required_terms: Vec<String>,
    #[serde(default)]
    pub forbidden_terms: Vec<String>,
    #[serde(default)]
    pub require_citations: bool,
    #[serde(default)]
    pub allowed_citation_domains: Vec<String>,
    #[serde(default)]
    pub max_response_words: u32,
    #[serde(default)]
    pub voice: TurnVoiceChoice,
}

/// Which brand voice the turn writes in.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnVoiceChoice {
    /// The workspace has no brand voice assigned.
    #[default]
    Unassigned,
    /// The sender asked for no brand voice on this turn. The policy's
    /// content rules still apply.
    Neutral,
    /// A workspace brand voice: the workspace default, or one the sender
    /// picked.
    Brand(TurnBrandVoice),
}

/// One workspace brand voice, copied at the version admitted for the turn.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnBrandVoice {
    pub voice_id: String,
    pub name: String,
    pub guidance: String,
    pub version: u64,
    /// True when the sender picked this voice rather than inheriting the
    /// workspace default.
    #[serde(default)]
    pub explicit: bool,
}

/// A short tone adjustment the sender can add to one turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToneAdjustment {
    Concise,
    Formal,
    Warmer,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_voice_serializes_to_an_empty_object_and_round_trips() {
        let empty = TurnVoice::default();
        assert!(empty.is_empty());
        assert_eq!(serde_json::to_value(&empty).unwrap(), serde_json::json!({}));
        let voice = TurnVoice {
            policy: Some(TurnContentPolicy {
                guide_version: 4,
                voice: TurnVoiceChoice::Brand(TurnBrandVoice {
                    voice_id: "voice_exec".into(),
                    name: "Executive".into(),
                    guidance: "Lead with the decision.".into(),
                    version: 2,
                    explicit: true,
                }),
                ..TurnContentPolicy::default()
            }),
            tone: vec![ToneAdjustment::Concise, ToneAdjustment::Warmer],
        };
        let json = serde_json::to_value(&voice).unwrap();
        assert_eq!(json["policy"]["voice"]["kind"], "brand");
        assert_eq!(json["tone"], serde_json::json!(["concise", "warmer"]));
        assert_eq!(serde_json::from_value::<TurnVoice>(json).unwrap(), voice);
    }

    #[test]
    fn neutral_choice_has_no_voice_fields() {
        let json = serde_json::to_value(TurnVoiceChoice::Neutral).unwrap();
        assert_eq!(json, serde_json::json!({"kind": "neutral"}));
    }
}
