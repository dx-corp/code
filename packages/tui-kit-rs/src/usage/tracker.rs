//! Usage tracking and statistics
//!
//! Tracks token usage and costs across turns and sessions.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use super::pricing::{DEFAULT_PRICING, ModelPricing};
use crate::headless::TokenUsage;

/// Usage data for a single turn (request/response pair)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnUsage {
    /// Timestamp of the turn
    pub timestamp: SystemTime,
    /// Model used for this turn
    pub model: String,
    /// Input tokens consumed
    pub input_tokens: u64,
    /// Output tokens generated
    pub output_tokens: u64,
    /// Cached input tokens (read from cache)
    pub cache_read_tokens: u64,
    /// Cached tokens written
    pub cache_write_tokens: u64,
    /// Calculated cost in USD
    pub cost: f64,
    /// Duration of the turn (time to first token to completion)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<Duration>,
}

impl TurnUsage {
    /// Create a new turn usage record
    pub fn new(model: impl Into<String>, usage: &TokenUsage) -> Self {
        let model = model.into();
        let cost = usage.cost.unwrap_or_else(|| {
            DEFAULT_PRICING.calculate_cost(
                &model,
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_read_tokens,
                usage.cache_write_tokens,
            )
        });

        Self {
            timestamp: SystemTime::now(),
            model,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            cache_write_tokens: usage.cache_write_tokens,
            cost,
            duration: None,
        }
    }

    /// Set the duration for this turn
    #[must_use]
    pub fn with_duration(mut self, duration: Duration) -> Self {
        self.duration = Some(duration);
        self
    }

    /// Total tokens (input + output)
    #[must_use]
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    /// Tokens per second (if duration is set)
    #[must_use]
    pub fn tokens_per_second(&self) -> Option<f64> {
        self.duration.map(|d| {
            let secs = d.as_secs_f64();
            if secs > 0.0 {
                self.output_tokens as f64 / secs
            } else {
                0.0
            }
        })
    }
}

/// Aggregated usage for a session
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionUsage {
    /// Session identifier
    pub session_id: Option<String>,
    /// When the session started
    pub started_at: Option<SystemTime>,
    /// Individual turn records
    pub turns: Vec<TurnUsage>,
    /// Usage breakdown by model
    #[serde(default)]
    pub by_model: HashMap<String, UsageStats>,
}

impl SessionUsage {
    /// Create a new session usage tracker
    #[must_use]
    pub fn new(session_id: Option<String>) -> Self {
        Self {
            session_id,
            started_at: Some(SystemTime::now()),
            turns: Vec::new(),
            by_model: HashMap::new(),
        }
    }

    /// Add a turn to this session
    pub fn add_turn(&mut self, turn: TurnUsage) {
        // Update per-model stats
        let stats = self.by_model.entry(turn.model.clone()).or_default();
        stats.add_turn(&turn);

        self.turns.push(turn);
    }

    /// Get total stats across all turns
    #[must_use]
    pub fn totals(&self) -> UsageStats {
        let mut stats = UsageStats::default();
        for turn in &self.turns {
            stats.add_turn(turn);
        }
        stats
    }

    /// Get stats for a specific model
    #[must_use]
    pub fn stats_for_model(&self, model: &str) -> Option<&UsageStats> {
        self.by_model.get(model)
    }

    /// Number of turns in this session
    #[must_use]
    pub fn turn_count(&self) -> usize {
        self.turns.len()
    }

    /// Duration since session start
    #[must_use]
    pub fn duration(&self) -> Option<Duration> {
        self.started_at
            .and_then(|start| SystemTime::now().duration_since(start).ok())
    }
}

/// Aggregated usage statistics
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageStats {
    /// Number of turns
    pub turns: u64,
    /// Total input tokens
    pub input_tokens: u64,
    /// Total output tokens
    pub output_tokens: u64,
    /// Total cache read tokens
    pub cache_read_tokens: u64,
    /// Total cache write tokens
    pub cache_write_tokens: u64,
    /// Total cost in USD
    pub cost: f64,
    /// Total duration (sum of all turn durations)
    #[serde(default)]
    pub total_duration: Duration,
}

impl UsageStats {
    /// Add a turn's usage to these stats
    pub fn add_turn(&mut self, turn: &TurnUsage) {
        self.turns += 1;
        self.input_tokens += turn.input_tokens;
        self.output_tokens += turn.output_tokens;
        self.cache_read_tokens += turn.cache_read_tokens;
        self.cache_write_tokens += turn.cache_write_tokens;
        self.cost += turn.cost;
        if let Some(d) = turn.duration {
            self.total_duration += d;
        }
    }

    /// Total tokens
    #[must_use]
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    /// Average tokens per turn
    #[must_use]
    pub fn avg_tokens_per_turn(&self) -> f64 {
        if self.turns > 0 {
            self.total_tokens() as f64 / self.turns as f64
        } else {
            0.0
        }
    }

    /// Average cost per turn
    #[must_use]
    pub fn avg_cost_per_turn(&self) -> f64 {
        if self.turns > 0 {
            self.cost / self.turns as f64
        } else {
            0.0
        }
    }

    /// Cache hit ratio (cached reads vs total input)
    #[must_use]
    pub fn cache_hit_ratio(&self) -> f64 {
        let total_input = self.input_tokens + self.cache_read_tokens;
        if total_input > 0 {
            self.cache_read_tokens as f64 / total_input as f64
        } else {
            0.0
        }
    }

    /// Output tokens per second (if duration available)
    #[must_use]
    pub fn tokens_per_second(&self) -> Option<f64> {
        let secs = self.total_duration.as_secs_f64();
        if secs > 0.0 {
            Some(self.output_tokens as f64 / secs)
        } else {
            None
        }
    }
}

/// Cost alert configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostAlert {
    /// Alert name
    pub name: String,
    /// Threshold in USD
    pub threshold: f64,
    /// Whether this alert has been triggered
    #[serde(default)]
    pub triggered: bool,
}

impl CostAlert {
    /// Create a new cost alert
    pub fn new(name: impl Into<String>, threshold: f64) -> Self {
        Self {
            name: name.into(),
            threshold,
            triggered: false,
        }
    }

    /// Check if alert should trigger
    pub fn check(&mut self, cost: f64) -> bool {
        if !self.triggered && cost >= self.threshold {
            self.triggered = true;
            true
        } else {
            false
        }
    }

    /// Reset the alert
    pub fn reset(&mut self) {
        self.triggered = false;
    }
}

/// Main usage tracker for a session
#[derive(Debug)]
pub struct UsageTracker {
    /// Current model being used
    current_model: String,
    /// Session usage data
    session: SessionUsage,
    /// Custom pricing (optional)
    pricing: Option<ModelPricing>,
    /// Cost alerts
    alerts: Vec<CostAlert>,
}

impl UsageTracker {
    /// Create a new usage tracker
    #[must_use]
    pub fn new() -> Self {
        Self {
            current_model: "unknown".to_string(),
            session: SessionUsage::new(None),
            pricing: None,
            alerts: Vec::new(),
        }
    }

    /// Create with a session ID
    pub fn with_session(session_id: impl Into<String>) -> Self {
        Self {
            current_model: "unknown".to_string(),
            session: SessionUsage::new(Some(session_id.into())),
            pricing: None,
            alerts: Vec::new(),
        }
    }

    /// Set the current model
    pub fn set_model(&mut self, model: impl Into<String>) {
        self.current_model = model.into();
    }

    /// Set custom pricing
    pub fn set_pricing(&mut self, pricing: ModelPricing) {
        self.pricing = Some(pricing);
    }

    /// Add a cost alert
    pub fn add_alert(&mut self, alert: CostAlert) {
        self.alerts.push(alert);
    }

    /// Add default alerts ($0.10, $1.00, $5.00)
    pub fn add_default_alerts(&mut self) {
        self.alerts.push(CostAlert::new("Low", 0.10));
        self.alerts.push(CostAlert::new("Medium", 1.00));
        self.alerts.push(CostAlert::new("High", 5.00));
    }

    /// Record usage for a turn
    pub fn add_turn(&mut self, usage: &TokenUsage) -> Vec<String> {
        self.add_turn_for_model(&self.current_model.clone(), usage)
    }

    /// Record usage for a turn with explicit model
    pub fn add_turn_for_model(&mut self, model: &str, usage: &TokenUsage) -> Vec<String> {
        let cost = usage.cost.unwrap_or_else(|| {
            self.pricing
                .as_ref()
                .unwrap_or(&DEFAULT_PRICING)
                .calculate_cost(
                    model,
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.cache_read_tokens,
                    usage.cache_write_tokens,
                )
        });

        let turn = TurnUsage {
            timestamp: SystemTime::now(),
            model: model.to_string(),
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            cache_write_tokens: usage.cache_write_tokens,
            cost,
            duration: None,
        };

        self.session.add_turn(turn);

        // Check alerts
        let total_cost = self.total_cost();
        let mut triggered = Vec::new();
        for alert in &mut self.alerts {
            if alert.check(total_cost) {
                triggered.push(format!(
                    "Cost alert '{}': ${:.2} threshold reached (current: ${:.4})",
                    alert.name, alert.threshold, total_cost
                ));
            }
        }
        triggered
    }

    /// Get total cost
    #[must_use]
    pub fn total_cost(&self) -> f64 {
        self.session.totals().cost
    }

    /// Get total tokens
    #[must_use]
    pub fn total_tokens(&self) -> u64 {
        self.session.totals().total_tokens()
    }

    /// Get session stats
    #[must_use]
    pub fn stats(&self) -> UsageStats {
        self.session.totals()
    }

    /// Get stats for current model
    #[must_use]
    pub fn current_model_stats(&self) -> Option<&UsageStats> {
        self.session.stats_for_model(&self.current_model)
    }

    /// Get the session data
    #[must_use]
    pub fn session(&self) -> &SessionUsage {
        &self.session
    }

    /// Get number of turns
    #[must_use]
    pub fn turn_count(&self) -> usize {
        self.session.turn_count()
    }

    /// Generate a summary string
    #[must_use]
    pub fn summary(&self) -> String {
        let stats = self.stats();
        format!(
            "{} turns · {} in / {} out · ${:.4}",
            stats.turns,
            format_tokens(stats.input_tokens),
            format_tokens(stats.output_tokens),
            stats.cost
        )
    }

    /// Generate a detailed summary
    #[must_use]
    pub fn detailed_summary(&self) -> String {
        let stats = self.stats();
        let mut lines = vec![
            format!("Session Usage Summary"),
            format!("─────────────────────"),
            format!("Turns: {}", stats.turns),
            format!(""),
            format!("Tokens:"),
            format!("  Input:       {:>12}", format_tokens(stats.input_tokens)),
            format!("  Output:      {:>12}", format_tokens(stats.output_tokens)),
            format!(
                "  Cache Read:  {:>12}",
                format_tokens(stats.cache_read_tokens)
            ),
            format!(
                "  Cache Write: {:>12}",
                format_tokens(stats.cache_write_tokens)
            ),
            format!("  Total:       {:>12}", format_tokens(stats.total_tokens())),
            format!(""),
            format!("Cost: ${:.4}", stats.cost),
            format!("  Avg/turn: ${:.4}", stats.avg_cost_per_turn()),
        ];

        if stats.cache_read_tokens > 0 {
            lines.push(format!(
                "  Cache hit: {:.1}%",
                stats.cache_hit_ratio() * 100.0
            ));
        }

        if let Some(tps) = stats.tokens_per_second() {
            lines.push(String::new());
            lines.push(format!("Speed: {tps:.1} tokens/sec"));
        }

        // Per-model breakdown if multiple models used
        if self.session.by_model.len() > 1 {
            lines.push(String::new());
            lines.push("By Model:".to_string());
            for (model, model_stats) in &self.session.by_model {
                lines.push(format!(
                    "  {}: {} turns, ${:.4}",
                    model, model_stats.turns, model_stats.cost
                ));
            }
        }

        lines.join("\n")
    }

    /// Export usage data
    #[must_use]
    pub fn export(&self) -> UsageExport {
        UsageExport {
            session_id: self.session.session_id.clone(),
            started_at: self.session.started_at,
            current_model: self.current_model.clone(),
            totals: self.stats(),
            by_model: self.session.by_model.clone(),
            turns: self.session.turns.clone(),
        }
    }

    /// Reset the tracker (keeps alerts but clears data)
    pub fn reset(&mut self) {
        self.session = SessionUsage::new(self.session.session_id.clone());
        for alert in &mut self.alerts {
            alert.reset();
        }
    }
}

impl Default for UsageTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Exportable usage data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageExport {
    /// Session ID
    pub session_id: Option<String>,
    /// When tracking started
    pub started_at: Option<SystemTime>,
    /// Current model
    pub current_model: String,
    /// Total statistics
    pub totals: UsageStats,
    /// Per-model breakdown
    pub by_model: HashMap<String, UsageStats>,
    /// Individual turns
    pub turns: Vec<TurnUsage>,
}

impl UsageExport {
    /// Export as JSON
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Export as compact JSON
    pub fn to_json_compact(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

/// Format token count with K/M suffixes
fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}K", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_usage(input: u64, output: u64) -> TokenUsage {
        TokenUsage {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cost: None,
            total_tokens: None,
            model_id: None,
            provider: None,
        }
    }

    #[test]
    fn test_turn_usage_new() {
        let usage = sample_usage(1000, 500);
        let turn = TurnUsage::new("claude-sonnet-4", &usage);

        assert_eq!(turn.input_tokens, 1000);
        assert_eq!(turn.output_tokens, 500);
        assert!(turn.cost > 0.0);
    }

    #[test]
    fn test_usage_tracker_basic() {
        let mut tracker = UsageTracker::new();
        tracker.set_model("claude-sonnet-4");

        let usage = sample_usage(2000, 500);
        tracker.add_turn(&usage);

        assert_eq!(tracker.turn_count(), 1);
        assert!(tracker.total_cost() > 0.0);
        assert_eq!(tracker.total_tokens(), 2500);
        let summary = tracker.summary();
        assert!(!summary.contains('|'), "{summary}");
        assert!(summary.contains("1 turns"), "{summary}");
        assert!(summary.contains("in /"), "{summary}");
    }

    #[test]
    fn test_usage_tracker_alerts() {
        let mut tracker = UsageTracker::new();
        tracker.set_model("claude-sonnet-4");
        tracker.add_alert(CostAlert::new("test", 0.001));

        // Small usage that exceeds threshold
        let usage = sample_usage(1000, 500);
        let alerts = tracker.add_turn(&usage);

        assert!(!alerts.is_empty());
        assert!(alerts[0].contains("test"));
    }

    #[test]
    fn test_usage_tracker_multiple_models() {
        let mut tracker = UsageTracker::new();

        tracker.add_turn_for_model("claude-sonnet-4", &sample_usage(1000, 500));
        tracker.add_turn_for_model("gpt-4o", &sample_usage(1000, 500));

        assert_eq!(tracker.session.by_model.len(), 2);
        assert!(tracker.session.by_model.contains_key("claude-sonnet-4"));
        assert!(tracker.session.by_model.contains_key("gpt-4o"));
    }

    #[test]
    fn test_session_usage_totals() {
        let mut session = SessionUsage::new(Some("test-session".into()));

        session.add_turn(TurnUsage::new("claude-sonnet-4", &sample_usage(1000, 500)));
        session.add_turn(TurnUsage::new("claude-sonnet-4", &sample_usage(2000, 1000)));

        let totals = session.totals();
        assert_eq!(totals.turns, 2);
        assert_eq!(totals.input_tokens, 3000);
        assert_eq!(totals.output_tokens, 1500);
    }

    #[test]
    fn test_usage_stats_cache_ratio() {
        let stats = UsageStats {
            input_tokens: 2000,
            cache_read_tokens: 8000,
            ..UsageStats::default()
        };

        // 8000 cache hits out of 10000 total = 80%
        assert!((stats.cache_hit_ratio() - 0.8).abs() < 0.001);
    }

    #[test]
    fn test_format_tokens() {
        assert_eq!(format_tokens(500), "500");
        assert_eq!(format_tokens(1500), "1.5K");
        assert_eq!(format_tokens(1_500_000), "1.5M");
    }

    #[test]
    fn test_usage_export() {
        let mut tracker = UsageTracker::with_session("test-123");
        tracker.set_model("claude-sonnet-4");
        tracker.add_turn(&sample_usage(1000, 500));

        let export = tracker.export();
        assert_eq!(export.session_id, Some("test-123".to_string()));
        assert_eq!(export.turns.len(), 1);

        // Should be JSON serializable
        let json = export.to_json().unwrap();
        assert!(json.contains("test-123"));
    }

    #[test]
    fn test_cost_alert() {
        let mut alert = CostAlert::new("test", 1.0);

        assert!(!alert.check(0.5));
        assert!(!alert.triggered);

        assert!(alert.check(1.0));
        assert!(alert.triggered);

        // Shouldn't trigger again
        assert!(!alert.check(2.0));

        alert.reset();
        assert!(!alert.triggered);
    }

    #[test]
    fn test_detailed_summary() {
        let mut tracker = UsageTracker::new();
        tracker.set_model("claude-sonnet-4");
        tracker.add_turn(&sample_usage(10000, 5000));

        let summary = tracker.detailed_summary();
        assert!(summary.contains("Session Usage Summary"));
        assert!(summary.contains("Input:"));
        assert!(summary.contains("Output:"));
        assert!(summary.contains("Cost:"));
    }
}
