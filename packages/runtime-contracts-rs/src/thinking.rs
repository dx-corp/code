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
