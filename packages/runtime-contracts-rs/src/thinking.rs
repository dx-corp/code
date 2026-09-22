//! Shared model effort configuration stored in sessions and used by runtimes.

use serde::{Deserialize, Serialize};

/// Extended thinking budget configuration.
///
/// Controls how much the AI model can use its internal reasoning feature (extended
/// thinking) before responding. Higher levels allow more thorough reasoning but
/// consume more tokens and take longer.
///
/// # Token Budgets
///
/// - **Off**: 0 tokens (thinking disabled)
/// - **Minimal**: 1,024 tokens
/// - **Low**: 4,096 tokens
/// - **Medium**: 10,000 tokens (default)
/// - **High**: 20,000 tokens
/// - **XHigh**: 32,000 tokens
/// - **Max**: 50,000 tokens
///
/// # Serialization
///
/// Serializes to lowercase strings: "off", "minimal", "low", "medium", "high",
/// "xhigh", "max"
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    /// Thinking disabled (0 tokens).
    Off,

    /// Minimal thinking budget (1,024 tokens).
    Minimal,

    /// Low thinking budget (4,096 tokens).
    Low,

    /// Medium thinking budget (10,000 tokens) - default level.
    #[default]
    Medium,

    /// High thinking budget (20,000 tokens).
    High,

    /// Extended thinking budget for long-horizon work (32,000 tokens).
    ///
    /// Anthropic exposes this as the `xhigh` effort level and recommends it as
    /// the starting point for coding and agentic work on the models that
    /// support it. Models that do not support `xhigh` resolve this level back
    /// to the nearest level they do support; see `normalize_thinking`.
    XHigh,

    /// Maximum thinking budget (50,000 tokens).
    Max,
}

impl ThinkingLevel {
    /// Every level, in ascending ladder order.
    ///
    /// Cycling, boosting, and the effort picker all walk this list. Keeping one
    /// copy matters: when `XHigh` was inserted, three separate hand-written
    /// copies of the ladder went stale in three different crates and each one
    /// surfaced a merge cycle after the last.
    pub const ALL: [ThinkingLevel; 7] = [
        ThinkingLevel::Off,
        ThinkingLevel::Minimal,
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
        ThinkingLevel::XHigh,
        ThinkingLevel::Max,
    ];

    /// This level's index in [`ThinkingLevel::ALL`].
    ///
    /// The match is exhaustive and the array length is fixed, so adding a
    /// variant without placing it in the ladder fails to compile.
    #[must_use]
    pub fn ladder_position(self) -> usize {
        match self {
            ThinkingLevel::Off => 0,
            ThinkingLevel::Minimal => 1,
            ThinkingLevel::Low => 2,
            ThinkingLevel::Medium => 3,
            ThinkingLevel::High => 4,
            ThinkingLevel::XHigh => 5,
            ThinkingLevel::Max => 6,
        }
    }

    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            ThinkingLevel::Off => "Off",
            ThinkingLevel::Minimal => "Minimal",
            ThinkingLevel::Low => "Low",
            ThinkingLevel::Medium => "Medium",
            ThinkingLevel::High => "High",
            ThinkingLevel::XHigh => "XHigh",
            ThinkingLevel::Max => "Max",
        }
    }

    /// Convert to (enabled, budget) configuration
    #[must_use]
    pub fn to_config(&self) -> (bool, u32) {
        match self {
            ThinkingLevel::Off => (false, 0),
            ThinkingLevel::Minimal => (true, 1024),
            ThinkingLevel::Low => (true, 4096),
            ThinkingLevel::Medium => (true, 10000),
            ThinkingLevel::High => (true, 20000),
            ThinkingLevel::XHigh => (true, 32000),
            ThinkingLevel::Max => (true, 50000),
        }
    }

    /// Parse from string
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "off" | "none" | "disabled" => Some(ThinkingLevel::Off),
            "minimal" | "min" => Some(ThinkingLevel::Minimal),
            "low" => Some(ThinkingLevel::Low),
            "medium" | "med" | "default" => Some(ThinkingLevel::Medium),
            "high" => Some(ThinkingLevel::High),
            "xhigh" | "x-high" | "extra-high" => Some(ThinkingLevel::XHigh),
            "max" | "maximum" => Some(ThinkingLevel::Max),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ThinkingLevel;

    #[test]
    fn all_is_the_ladder_in_ascending_budget_order() {
        assert_eq!(ThinkingLevel::ALL.len(), 7);

        for (index, level) in ThinkingLevel::ALL.into_iter().enumerate() {
            assert_eq!(level.ladder_position(), index, "{level:?}");
        }

        let budgets: Vec<u32> = ThinkingLevel::ALL
            .into_iter()
            .map(|level| level.to_config().1)
            .collect();
        assert_eq!(
            budgets,
            vec![0, 1_024, 4_096, 10_000, 20_000, 32_000, 50_000]
        );
        assert!(
            budgets.windows(2).all(|pair| pair[0] < pair[1]),
            "ALL must be strictly ascending by budget: {budgets:?}"
        );

        // Every label and every serialized name must round-trip through parse,
        // so a new level cannot be added to ALL without a parse arm.
        for level in ThinkingLevel::ALL {
            assert_eq!(
                ThinkingLevel::parse(level.label()),
                Some(level),
                "{level:?}"
            );
            let wire = serde_json::to_value(level).unwrap();
            assert_eq!(
                ThinkingLevel::parse(wire.as_str().unwrap()),
                Some(level),
                "{level:?}"
            );
        }
    }

    #[test]
    fn xhigh_sits_between_high_and_max() {
        assert_eq!(ThinkingLevel::XHigh.to_config(), (true, 32_000));
        assert!(ThinkingLevel::High.to_config().1 < ThinkingLevel::XHigh.to_config().1);
        assert!(ThinkingLevel::XHigh.to_config().1 < ThinkingLevel::Max.to_config().1);
        assert_eq!(ThinkingLevel::XHigh.label(), "XHigh");
    }

    #[test]
    fn xhigh_parses_and_serializes_as_the_anthropic_effort_name() {
        for value in ["xhigh", "XHigh", "x-high", "extra-high"] {
            assert_eq!(
                ThinkingLevel::parse(value),
                Some(ThinkingLevel::XHigh),
                "{value}"
            );
        }
        assert_eq!(
            serde_json::to_string(&ThinkingLevel::XHigh).unwrap(),
            "\"xhigh\""
        );
        assert_eq!(
            serde_json::from_str::<ThinkingLevel>("\"xhigh\"").unwrap(),
            ThinkingLevel::XHigh
        );
    }
}
