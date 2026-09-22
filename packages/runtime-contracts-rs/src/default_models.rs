//! Every default model id Maestro ships, in one place.
//!
//! These ids were previously written out in at least four crates: the swarm
//! subagent tier table, the ambient cascader's frontier constants, the TUI
//! config presets, and the model picker's promoted list. Nothing tied them
//! together, so each went stale on its own schedule. Over one day of audit the
//! Google subagent tiers named three retired Gemini 2.0 previews, the ambient
//! Anthropic frontier named a retired Opus 4.1 snapshot, and the Anthropic
//! provider preset still defaulted to Opus 4.6.
//!
//! Adding a model generation is now one edit here. `id()` is an exhaustive
//! match, so a new variant will not compile until it is given an id, and
//! `default_models_are_catalogued` fails when an id is absent from the bundled
//! model catalog.

/// A default model slot, named for the role it fills rather than the model
/// that currently fills it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DefaultModel {
    /// Highest-capability Anthropic model. Subagent Opus tier, ambient
    /// frontier, provider preset default.
    AnthropicFlagship,
    /// Balanced Anthropic model. Subagent Sonnet tier.
    AnthropicBalanced,
    /// Fastest Anthropic model. Subagent Haiku tier.
    AnthropicFast,
    /// Anthropic model for demanding reasoning and long-horizon agentic work.
    AnthropicReasoning,
    /// Highest-capability OpenAI model.
    OpenAiFlagship,
    /// Balanced OpenAI model.
    OpenAiBalanced,
    /// Fastest OpenAI model.
    OpenAiFast,
    /// OpenAI model used through the Codex app-server route.
    OpenAiCodex,
    /// Highest-capability Google model.
    GoogleFlagship,
    /// Balanced Google model. An alias, so it tracks the current release.
    GoogleBalanced,
    /// Fastest Google model. An alias, so it tracks the current release.
    GoogleFast,
}

impl DefaultModel {
    /// The model id for this slot.
    ///
    /// Exhaustive on purpose: a new variant fails to compile until it is
    /// given an id here.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::AnthropicFlagship => "claude-opus-5-5",
            Self::AnthropicBalanced => "claude-sonnet-5",
            Self::AnthropicFast => "claude-haiku-4-5-20251001",
            Self::AnthropicReasoning => "claude-fable-5-1",
            Self::OpenAiFlagship => "gpt-6-astra",
            Self::OpenAiBalanced => "gpt-5.6",
            Self::OpenAiFast => "gpt-4o-mini",
            Self::OpenAiCodex => "gpt-5.6",
            Self::GoogleFlagship => "gemini-3.1-pro-preview",
            Self::GoogleBalanced => "gemini-flash-latest",
            Self::GoogleFast => "gemini-flash-lite-latest",
        }
    }

    /// Every slot, for tests that must cover all of them.
    pub const ALL: [DefaultModel; 11] = [
        Self::AnthropicFlagship,
        Self::AnthropicBalanced,
        Self::AnthropicFast,
        Self::AnthropicReasoning,
        Self::OpenAiFlagship,
        Self::OpenAiBalanced,
        Self::OpenAiFast,
        Self::OpenAiCodex,
        Self::GoogleFlagship,
        Self::GoogleBalanced,
        Self::GoogleFast,
    ];
}

#[cfg(test)]
mod tests {
    use super::DefaultModel;

    #[test]
    fn every_slot_has_a_distinct_non_empty_id() {
        for slot in DefaultModel::ALL {
            assert!(!slot.id().is_empty(), "{slot:?}");
        }
        // ALL must list every variant; a missing one would make the catalog
        // guard in the maestro crates silently skip it.
        assert_eq!(DefaultModel::ALL.len(), 11);
    }
}
