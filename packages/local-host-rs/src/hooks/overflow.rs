//! Overflow detection and handling
//!
//! Monitors token usage and detects context overflow conditions.
//! When overflow is detected, triggers compaction and continuation.
//!
//! # Detection Strategy
//!
//! Overflow is detected when:
//! 1. Response ends with `stop_reason` = "length" / "`max_tokens`"
//! 2. Token count approaches the model's context limit
//! 3. API returns a context length error
//!
//! # Handling Strategy
//!
//! On overflow:
//! 1. Emit overflow event to hooks
//! 2. Trigger automatic compaction
//! 3. Resume conversation with compacted context

use super::types::{HookResult, OverflowInput};
use std::time::Instant;

/// Model context limits (tokens)
#[derive(Debug, Clone, Copy)]
pub struct ModelLimits {
    /// Maximum context window size
    pub max_context: u64,
    /// Maximum output tokens
    pub max_output: u64,
    /// Warning threshold (percentage of `max_context`)
    pub warning_threshold: f64,
    /// Critical threshold (percentage of `max_context`)
    pub critical_threshold: f64,
}

impl Default for ModelLimits {
    fn default() -> Self {
        Self {
            max_context: 200_000, // Claude default
            max_output: 8_192,
            warning_threshold: 0.75,
            critical_threshold: 0.90,
        }
    }
}

/// The bare model name: the segment after the last `/`.
///
/// Routes arrive as `provider/model` and sometimes
/// `maestro-managed/provider/model`. Matching the bare name keeps the family
/// arms anchored instead of matching a substring anywhere in the route.
fn model_name(model_id: &str) -> &str {
    let trimmed = model_id.trim();
    trimmed.rsplit('/').next().unwrap_or(trimmed)
}

/// Whether `model` belongs to `family`: the same name, or the family followed
/// by a `-` separator.
///
/// This is deliberately not a substring test. `"gpt-4.1".contains("gpt-4")` is
/// true, which put a model with a 1,047,576-token window in the 8,192-token
/// GPT-4 arm and made the detector compact at 6,144 tokens.
///
/// A Claude id is also tried with its dotted release suffix respelled with a
/// dash, so an OpenRouter `claude-opus-4.6` matches the `claude-opus-4-6`
/// family. That retry is restricted to Claude ids on purpose: applied to every
/// id it would recreate the GPT-4 bug above.
fn is_model_family(model: &str, family: &str) -> bool {
    fn matches(model: &str, family: &str) -> bool {
        model == family
            || model
                .strip_prefix(family)
                .is_some_and(|suffix| suffix.starts_with('-'))
    }

    if matches(model, family) {
        return true;
    }
    // Only Claude ids get the dotted retry. Applied to every id it would
    // recreate the bug being fixed here: "gpt-4.1" respells to "gpt-4-1",
    // which does match the family "gpt-4".
    model.starts_with("claude-") && model.contains('.') && matches(&model.replace('.', "-"), family)
}

impl ModelLimits {
    /// Create limits for a specific model
    ///
    /// The bundled model catalog is consulted first, so a model whose limits
    /// upstream already publishes never needs an arm here. The table below
    /// covers ids the catalog does not carry: retired snapshots, local
    /// runtimes, and fixtures.
    #[must_use]
    pub fn for_model(model_id: &str) -> Self {
        if let Some((context, output)) = crate::model_catalog::bundled_limits(model_id) {
            if context > 0 {
                let default = Self::default();
                return Self {
                    max_context: u64::from(context),
                    max_output: output.map_or(default.max_output, u64::from),
                    ..default
                };
            }
        }
        match model_name(model_id) {
            // Claude models. Context and output limits track the published
            // per-model values in platform.claude.com/docs/en/models/overview
            // and the bundled models.dev snapshot in model_catalog_data.json.
            s if is_model_family(s, "claude-3-5-sonnet") => Self {
                max_context: 200_000,
                max_output: 8_192,
                ..Default::default()
            },
            // Matches claude-opus-5 and claude-opus-5-5.
            s if is_model_family(s, "claude-opus-5") => Self {
                max_context: 1_000_000,
                max_output: 128_000,
                ..Default::default()
            },
            s if is_model_family(s, "claude-sonnet-5") => Self {
                max_context: 1_000_000,
                max_output: 128_000,
                ..Default::default()
            },
            // Matches claude-fable-5 and claude-fable-5-1.
            s if is_model_family(s, "claude-fable-5") => Self {
                max_context: 1_000_000,
                max_output: 128_000,
                ..Default::default()
            },
            s if is_model_family(s, "claude-opus-4-8")
                || is_model_family(s, "claude-opus-4-7")
                || is_model_family(s, "claude-opus-4-6") =>
            {
                Self {
                    max_context: 1_000_000,
                    max_output: 128_000,
                    ..Default::default()
                }
            }
            s if is_model_family(s, "claude-sonnet-4-6") => Self {
                max_context: 1_000_000,
                max_output: 128_000,
                ..Default::default()
            },
            // 200k, not 1M. See CONTEXT_WINDOW_OVERRIDES in
            // scripts/fetch-model-catalog.mjs: models.dev is wrong here and
            // Anthropic's context-windows doc names Sonnet 4.5 as a 200k model.
            s if is_model_family(s, "claude-sonnet-4-5") => Self {
                max_context: 200_000,
                max_output: 64_000,
                ..Default::default()
            },
            s if is_model_family(s, "claude-opus-4-5") => Self {
                max_context: 200_000,
                max_output: 64_000,
                ..Default::default()
            },
            s if is_model_family(s, "claude-haiku-4-5") => Self {
                max_context: 200_000,
                max_output: 64_000,
                ..Default::default()
            },
            // Opus 4.0 and 4.1 cap output at 32k.
            s if is_model_family(s, "claude-opus-4") => Self {
                max_context: 200_000,
                max_output: 32_000,
                ..Default::default()
            },
            s if is_model_family(s, "claude-3-haiku") => Self {
                max_context: 200_000,
                max_output: 4_096,
                ..Default::default()
            },
            // GPT models
            s if is_model_family(s, "gpt-4-turbo") => Self {
                max_context: 128_000,
                max_output: 4_096,
                warning_threshold: 0.75,
                critical_threshold: 0.90,
            },
            s if is_model_family(s, "gpt-4o") => Self {
                max_context: 128_000,
                max_output: 16_384,
                warning_threshold: 0.75,
                critical_threshold: 0.90,
            },
            s if is_model_family(s, "gpt-4") => Self {
                max_context: 8_192,
                max_output: 4_096,
                warning_threshold: 0.70,
                critical_threshold: 0.85,
            },
            // Default
            _ => Self::default(),
        }
    }

    /// Check if token count is at warning level
    #[must_use]
    pub fn is_warning(&self, tokens: u64) -> bool {
        tokens as f64 >= self.max_context as f64 * self.warning_threshold
    }

    /// Check if token count is at critical level
    #[must_use]
    pub fn is_critical(&self, tokens: u64) -> bool {
        tokens as f64 >= self.max_context as f64 * self.critical_threshold
    }

    /// Check if token count exceeds max
    #[must_use]
    pub fn is_overflow(&self, tokens: u64) -> bool {
        tokens >= self.max_context
    }
}

/// Stop reasons that indicate overflow
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// Normal end of response
    EndTurn,
    /// Tool use requested
    ToolUse,
    /// Max tokens reached (overflow)
    MaxTokens,
    /// Length limit reached (overflow)
    Length,
    /// Stop sequence matched
    StopSequence,
    /// Unknown/other
    Unknown,
}

impl StopReason {
    /// Parse from string (handles different API formats)
    #[allow(clippy::should_implement_trait)]
    #[must_use]
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "end_turn" | "stop" => StopReason::EndTurn,
            "tool_use" | "tool_calls" => StopReason::ToolUse,
            "max_tokens" => StopReason::MaxTokens,
            "length" => StopReason::Length,
            "stop_sequence" => StopReason::StopSequence,
            _ => StopReason::Unknown,
        }
    }

    /// Check if this stop reason indicates overflow
    #[must_use]
    pub fn is_overflow(&self) -> bool {
        matches!(self, StopReason::MaxTokens | StopReason::Length)
    }
}

/// Type alias for overflow handler function
type OverflowHandler = Box<dyn Fn(&OverflowInput) -> HookResult + Send + Sync>;

/// Overflow detector
pub struct OverflowDetector {
    /// Model limits
    limits: ModelLimits,
    /// Current token count (estimated)
    current_tokens: u64,
    /// Last check time
    last_check: Option<Instant>,
    /// Overflow hook handler
    overflow_handler: Option<OverflowHandler>,
}

impl OverflowDetector {
    /// Create a new detector with default limits
    #[must_use]
    pub fn new() -> Self {
        Self {
            limits: ModelLimits::default(),
            current_tokens: 0,
            last_check: None,
            overflow_handler: None,
        }
    }

    /// Create a detector for a specific model
    #[must_use]
    pub fn for_model(model_id: &str) -> Self {
        Self {
            limits: ModelLimits::for_model(model_id),
            current_tokens: 0,
            last_check: None,
            overflow_handler: None,
        }
    }

    /// Set custom limits
    #[must_use]
    pub fn with_limits(mut self, limits: ModelLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Set overflow handler
    pub fn with_handler<F>(mut self, handler: F) -> Self
    where
        F: Fn(&OverflowInput) -> HookResult + Send + Sync + 'static,
    {
        self.overflow_handler = Some(Box::new(handler));
        self
    }

    /// Update token count from usage stats
    pub fn update_tokens(&mut self, input_tokens: u64, output_tokens: u64, cache_tokens: u64) {
        // Total context = input + output + any cached tokens being used
        self.current_tokens = input_tokens + output_tokens + cache_tokens;
        self.last_check = Some(Instant::now());
    }

    /// Check current status
    #[must_use]
    pub fn check_status(&self) -> OverflowStatus {
        if self.limits.is_overflow(self.current_tokens) {
            OverflowStatus::Overflow
        } else if self.limits.is_critical(self.current_tokens) {
            OverflowStatus::Critical
        } else if self.limits.is_warning(self.current_tokens) {
            OverflowStatus::Warning
        } else {
            OverflowStatus::Normal
        }
    }

    /// Check if a stop reason indicates overflow
    #[must_use]
    pub fn check_stop_reason(&self, stop_reason: &str) -> bool {
        StopReason::from_str(stop_reason).is_overflow()
    }

    /// Handle overflow condition
    #[must_use]
    pub fn handle_overflow(&self, cwd: &str, session_id: Option<&str>) -> HookResult {
        let input = OverflowInput {
            hook_event_name: "Overflow".to_string(),
            cwd: cwd.to_string(),
            session_id: session_id.map(std::string::ToString::to_string),
            timestamp: chrono::Utc::now().to_rfc3339(),
            token_count: self.current_tokens,
            max_tokens: self.limits.max_context,
        };

        if let Some(ref handler) = self.overflow_handler {
            handler(&input)
        } else {
            // Default: allow auto-compaction
            HookResult::Continue
        }
    }

    /// Get current token count
    #[must_use]
    pub fn current_tokens(&self) -> u64 {
        self.current_tokens
    }

    /// Get max context size
    #[must_use]
    pub fn max_tokens(&self) -> u64 {
        self.limits.max_context
    }

    /// Get utilization percentage
    #[must_use]
    pub fn utilization(&self) -> f64 {
        self.current_tokens as f64 / self.limits.max_context as f64 * 100.0
    }
}

impl Default for OverflowDetector {
    fn default() -> Self {
        Self::new()
    }
}

/// Overflow status levels
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverflowStatus {
    /// Normal operation
    Normal,
    /// Approaching limit (warning)
    Warning,
    /// Near limit (critical)
    Critical,
    /// At or over limit (overflow)
    Overflow,
}

impl OverflowStatus {
    /// Get a human-readable description
    #[must_use]
    pub fn description(&self) -> &'static str {
        match self {
            OverflowStatus::Normal => "Normal",
            OverflowStatus::Warning => "Warning: Approaching context limit",
            OverflowStatus::Critical => "Critical: Near context limit",
            OverflowStatus::Overflow => "Overflow: Context limit exceeded",
        }
    }

    /// Check if compaction should be triggered
    #[must_use]
    pub fn should_compact(&self) -> bool {
        matches!(self, OverflowStatus::Critical | OverflowStatus::Overflow)
    }
}

/// Compaction request generated on overflow
#[derive(Debug, Clone)]
pub struct CompactionRequest {
    /// Current token count
    pub current_tokens: u64,
    /// Target token count after compaction
    pub target_tokens: u64,
    /// Whether this was triggered automatically
    pub auto_triggered: bool,
    /// Custom instructions for summarization
    pub custom_instructions: Option<String>,
}

impl OverflowDetector {
    /// Generate a compaction request
    #[must_use]
    pub fn create_compaction_request(&self) -> CompactionRequest {
        // Target: reduce to 50% of max context
        let target = (self.limits.max_context as f64 * 0.5) as u64;

        CompactionRequest {
            current_tokens: self.current_tokens,
            target_tokens: target,
            auto_triggered: true,
            custom_instructions: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_limits() {
        for model in [
            "claude-opus-5-5",
            "anthropic/claude-opus-5-5",
            "claude-opus-5",
            "anthropic/claude-opus-5",
            "claude-sonnet-5",
            "claude-fable-5-1",
            "claude-fable-5",
            "claude-opus-4-8",
            "claude-opus-4-7",
            "claude-opus-4-6",
            "claude-sonnet-4-6",
        ] {
            let limits = ModelLimits::for_model(model);
            assert_eq!(limits.max_context, 1_000_000, "{model}");
            assert_eq!(limits.max_output, 128_000, "{model}");
        }

        // Declaring 1M here let a Sonnet 4.5 session run to ~750k before
        // compacting, where the API rejects at 200k with "prompt is too long".
        let limits = ModelLimits::for_model("claude-sonnet-4-5-20250929");
        assert_eq!(limits.max_context, 200_000);
        assert_eq!(limits.max_output, 64_000);

        let limits = ModelLimits::for_model("claude-opus-4-5-20251101");
        assert_eq!(limits.max_context, 200_000);
        assert_eq!(limits.max_output, 64_000);

        let limits = ModelLimits::for_model("claude-haiku-4-5-20251001");
        assert_eq!(limits.max_context, 200_000);
        assert_eq!(limits.max_output, 64_000);

        // Opus 4.0 and 4.1 keep the older 32k output cap.
        let limits = ModelLimits::for_model("claude-opus-4-20250514");
        assert_eq!(limits.max_context, 200_000);
        assert_eq!(limits.max_output, 32_000);

        let limits = ModelLimits::for_model("gpt-4o");
        assert_eq!(limits.max_context, 128_000);
    }

    #[test]
    fn catalog_models_get_their_published_limits() {
        // Before the catalog lookup these fell through to the 200,000/8,192
        // default, or worse: gpt-4.1 matched the generic `gpt-4` arm and got
        // 8,192, so the detector compacted at 6,144 tokens on a model with a
        // 1,047,576-token window.
        for (model, context, output) in [
            ("gpt-6-astra", 1_050_000, 128_000),
            ("gpt-5.6", 1_050_000, 128_000),
            ("gemini-3.6-flash", 1_048_576, 65_536),
            ("gpt-4.1", 1_047_576, 32_768),
            ("gpt-4.1-mini", 1_047_576, 32_768),
        ] {
            let limits = ModelLimits::for_model(model);
            assert_eq!(limits.max_context, context, "{model} context");
            assert_eq!(limits.max_output, output, "{model} output");
        }

        // Claude ids keep the values the hand table already had, now sourced
        // from the catalog instead of duplicated.
        for (model, context, output) in [
            ("claude-opus-5-5", 1_000_000, 128_000),
            ("anthropic/claude-opus-5-5", 1_000_000, 128_000),
            ("anthropic/claude-opus-5.5", 1_000_000, 128_000),
            ("claude-sonnet-4-5-20250929", 200_000, 64_000),
            ("claude-opus-4-5-20251101", 200_000, 64_000),
            ("claude-haiku-4-5-20251001", 200_000, 64_000),
        ] {
            let limits = ModelLimits::for_model(model);
            assert_eq!(limits.max_context, context, "{model} context");
            assert_eq!(limits.max_output, output, "{model} output");
        }
    }

    #[test]
    fn uncatalogued_models_still_use_the_fallback_table() {
        // Neither id is in the bundled snapshot, so the match arms below the
        // catalog lookup are what answer for them.
        let limits = ModelLimits::for_model("claude-opus-4-20250514");
        assert_eq!(limits.max_context, 200_000);
        assert_eq!(limits.max_output, 32_000);

        let limits = ModelLimits::for_model("gpt-4-turbo-2024-04-09");
        assert_eq!(limits.max_context, 128_000);
        assert_eq!(limits.max_output, 4_096);

        // Nothing matches at all: the conservative default.
        let limits = ModelLimits::for_model("some-local-runtime-model");
        assert_eq!(limits.max_context, 200_000);
        assert_eq!(limits.max_output, 8_192);
    }

    #[test]
    fn family_matching_is_anchored_not_a_substring() {
        // The bug this replaces: "gpt-4.1".contains("gpt-4") is true, so a
        // 1,047,576-token model landed in the 8,192-token GPT-4 arm.
        assert!(!is_model_family("gpt-4.1", "gpt-4"));
        assert!(!is_model_family("gpt-4o", "gpt-4"));
        assert!(is_model_family("gpt-4", "gpt-4"));
        assert!(is_model_family("gpt-4-0613", "gpt-4"));
        assert!(is_model_family("gpt-4-turbo-2024-04-09", "gpt-4-turbo"));

        // A dotted release suffix still resolves to its dashed family.
        assert!(is_model_family("claude-opus-4.6", "claude-opus-4-6"));
        assert!(is_model_family("claude-opus-5-5-20260922", "claude-opus-5"));

        // Provider-qualified and managed routes match on the bare name.
        assert_eq!(model_name("anthropic/claude-opus-5-5"), "claude-opus-5-5");
        assert_eq!(
            model_name("maestro-managed/openai/gpt-6-astra"),
            "gpt-6-astra"
        );
        assert_eq!(model_name("claude-opus-5-5"), "claude-opus-5-5");
    }

    #[test]
    fn an_uncatalogued_gpt_4_point_release_is_no_longer_capped_at_8k() {
        // Belt and braces: even with the catalog lookup removed from the
        // picture, an id the GPT-4 arm used to swallow must not come back with
        // the 8,192-token window.
        assert_ne!(ModelLimits::for_model("gpt-4.9-preview").max_context, 8_192);
        assert_eq!(ModelLimits::for_model("gpt-4").max_context, 8_192);
    }

    #[test]
    fn test_stop_reason_parsing() {
        assert!(StopReason::from_str("max_tokens").is_overflow());
        assert!(StopReason::from_str("length").is_overflow());
        assert!(!StopReason::from_str("end_turn").is_overflow());
        assert!(!StopReason::from_str("tool_use").is_overflow());
    }

    #[test]
    fn test_overflow_detection() {
        let mut detector = OverflowDetector::new();
        detector.limits = ModelLimits {
            max_context: 100,
            warning_threshold: 0.75,
            critical_threshold: 0.90,
            ..Default::default()
        };

        detector.update_tokens(50, 0, 0);
        assert_eq!(detector.check_status(), OverflowStatus::Normal);

        detector.update_tokens(80, 0, 0);
        assert_eq!(detector.check_status(), OverflowStatus::Warning);

        detector.update_tokens(95, 0, 0);
        assert_eq!(detector.check_status(), OverflowStatus::Critical);

        detector.update_tokens(100, 0, 0);
        assert_eq!(detector.check_status(), OverflowStatus::Overflow);
    }

    #[test]
    fn test_utilization() {
        let mut detector = OverflowDetector::new();
        detector.limits.max_context = 100;
        detector.update_tokens(50, 0, 0);
        assert!((detector.utilization() - 50.0).abs() < 0.01);
    }
}
