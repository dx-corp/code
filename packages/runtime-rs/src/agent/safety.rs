//! Safety controls for agent tool execution
//!
//! This module implements safety mechanisms to prevent runaway agent behavior:
//!
//! - **Doom Loop Detection**: Blocks repeated identical tool calls
//! - **Rate Limiting**: Prevents excessive tool invocations
//! - **Retryable Error Detection**: Identifies transient errors for auto-retry
//!
//! # Doom Loop Detection
//!
//! A "doom loop" occurs when the agent gets stuck calling the same tool with
//! the same arguments repeatedly, often due to:
//! - Reading a file that doesn't exist and retrying
//! - Editing text that isn't found
//! - Running a command that fails with the same error
//!
//! Detection works by maintaining a sliding window of recent tool calls as
//! `(name, signature)` pairs. If the last N calls all have identical signatures,
//! the tool is blocked.
//!
//! # Rate Limiting
//!
//! Per-tool rate limits prevent runaway execution that could:
//! - Waste API tokens on repetitive calls
//! - Hit external API rate limits
//! - Consume excessive system resources
//!
//! The rate limiter tracks timestamps per tool and blocks calls that exceed
//! the configured limit within a time window.
//!
//! # Example
//!
//! ```rust,ignore
//! use maestro_runtime::agent::safety::{SafetyController, SafetyVerdict};
//!
//! let mut safety = SafetyController::new();
//!
//! // Check if tool call is safe
//! let args = serde_json::json!({"command": "ls -la"});
//! match safety.check_tool_call("bash", &args) {
//!     SafetyVerdict::Allow => { /* execute tool */ }
//!     SafetyVerdict::BlockDoomLoop { reason } => { /* return error */ }
//!     SafetyVerdict::BlockRateLimit { reason } => { /* return error */ }
//! }
//!
//! // Record the call after execution
//! safety.record_tool_call("bash", &args);
//! ```

use std::collections::HashMap;
use std::time::{Duration, Instant};

pub use super::denial_memory::DenialMemory;
pub use super::workflow_state::{
    WorkflowStateSnapshot, WorkflowStateTracker, apply_workflow_state_hooks,
};

/// Safety verdict for a tool call
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SafetyVerdict {
    /// Tool call is allowed to proceed
    Allow,
    /// Blocked due to doom loop detection
    BlockDoomLoop {
        /// Human-readable reason for blocking
        reason: String,
    },
    /// Blocked due to rate limiting
    BlockRateLimit {
        /// Human-readable reason for blocking
        reason: String,
    },
}

/// Signature of a tool call for doom loop detection
#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolCallSignature {
    /// Tool name
    name: String,
    /// Stable JSON representation of arguments
    args_hash: String,
}

/// Safety controller for agent tool execution
///
/// Implements doom loop detection and rate limiting to prevent runaway
/// agent behavior. Should be created once per agent session.
#[derive(Debug)]
pub struct SafetyController {
    /// Recent tool calls for doom loop detection
    recent_calls: Vec<ToolCallSignature>,

    /// Per-tool timestamps for rate limiting
    tool_timestamps: HashMap<String, Vec<Instant>>,

    /// Configuration
    config: SafetyConfig,
}

/// Configuration for safety controls
#[derive(Debug, Clone)]
pub struct SafetyConfig {
    /// Number of identical consecutive calls before blocking (doom loop threshold)
    pub doom_loop_threshold: usize,

    /// Maximum calls per tool within the rate window
    pub rate_limit: usize,

    /// Time window for rate limiting
    pub rate_window: Duration,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            doom_loop_threshold: 3,
            rate_limit: 5,
            rate_window: Duration::from_secs(10),
        }
    }
}

impl SafetyController {
    /// Create a new safety controller with default configuration
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(SafetyConfig::default())
    }

    /// Create a new safety controller with custom configuration
    #[must_use]
    pub fn with_config(config: SafetyConfig) -> Self {
        Self {
            recent_calls: Vec::new(),
            tool_timestamps: HashMap::new(),
            config,
        }
    }

    /// Check if a tool call should be allowed
    ///
    /// Returns a verdict indicating whether the call is allowed or blocked.
    /// Does NOT record the call - call `record_tool_call` after execution.
    #[must_use]
    pub fn check_tool_call(&self, tool_name: &str, args: &serde_json::Value) -> SafetyVerdict {
        // Check doom loop first
        if let Some(reason) = self.check_doom_loop(tool_name, args) {
            return SafetyVerdict::BlockDoomLoop { reason };
        }

        // Check rate limit
        if let Some(reason) = self.check_rate_limit(tool_name) {
            return SafetyVerdict::BlockRateLimit { reason };
        }

        SafetyVerdict::Allow
    }

    /// Record a tool call (call after execution)
    ///
    /// Updates the doom loop tracker and rate limiter with the new call.
    pub fn record_tool_call(&mut self, tool_name: &str, args: &serde_json::Value) {
        // Record for doom loop detection
        let signature = ToolCallSignature {
            name: tool_name.to_string(),
            args_hash: stable_stringify(args),
        };
        self.recent_calls.push(signature);

        // Keep only recent calls (threshold + 2 as buffer)
        let max_history = self.config.doom_loop_threshold + 2;
        if self.recent_calls.len() > max_history {
            self.recent_calls.remove(0);
        }

        // Record timestamp for rate limiting
        let timestamps = self
            .tool_timestamps
            .entry(tool_name.to_string())
            .or_default();
        timestamps.push(Instant::now());

        // Clean old timestamps
        let window = self.config.rate_window;
        timestamps.retain(|&ts| ts.elapsed() < window);
    }

    /// Reset all safety state (call on new session)
    pub fn reset(&mut self) {
        self.recent_calls.clear();
        self.tool_timestamps.clear();
    }

    /// Check for doom loop condition
    fn check_doom_loop(&self, tool_name: &str, args: &serde_json::Value) -> Option<String> {
        let args_hash = stable_stringify(args);
        let threshold = self.config.doom_loop_threshold;

        // Build what the new tail would look like
        let new_signature = ToolCallSignature {
            name: tool_name.to_string(),
            args_hash: args_hash.clone(),
        };

        // Get the tail of recent calls plus this new one
        let start = if self.recent_calls.len() >= threshold - 1 {
            self.recent_calls.len() - (threshold - 1)
        } else {
            0
        };
        let tail: Vec<_> = self.recent_calls[start..]
            .iter()
            .chain(std::iter::once(&new_signature))
            .collect();

        // Check if we have enough calls and they're all identical
        if tail.len() >= threshold {
            let last_n: Vec<_> = tail.iter().rev().take(threshold).collect();
            let all_identical = last_n
                .iter()
                .all(|sig| sig.name == tool_name && sig.args_hash == args_hash);

            if all_identical {
                return Some(format!(
                    "Blocked \"{tool_name}\" to prevent a possible doom loop: same tool invoked {threshold} times with identical arguments."
                ));
            }
        }

        None
    }

    /// Check rate limit for a tool
    fn check_rate_limit(&self, tool_name: &str) -> Option<String> {
        let timestamps = self.tool_timestamps.get(tool_name)?;

        let window = self.config.rate_window;
        let recent_count = timestamps.iter().filter(|ts| ts.elapsed() < window).count();

        if recent_count >= self.config.rate_limit {
            return Some(format!(
                "Blocked \"{}\" due to rate limit: >{} calls in {}s window.",
                tool_name,
                self.config.rate_limit,
                window.as_secs()
            ));
        }

        None
    }
}

impl Default for SafetyController {
    fn default() -> Self {
        Self::new()
    }
}

/// Retryable error patterns for transient failures
const RETRYABLE_PATTERNS: &[&str] = &[
    "overloaded",
    "rate limit",
    "rate_limit",
    "too many requests",
    "429",
    "500",
    "502",
    "503",
    "504",
    "service unavailable",
    "server error",
    "internal error",
    "temporarily",
    "try again",
];

/// Check if an error message indicates a retryable (transient) error
///
/// Returns true if the error is likely transient and the operation could
/// succeed on retry (e.g., rate limits, server errors, overload).
#[must_use]
pub fn is_retryable_error(error_message: &str) -> bool {
    let lower = error_message.to_lowercase();
    RETRYABLE_PATTERNS
        .iter()
        .any(|pattern| lower.contains(pattern))
}

/// Stable JSON stringify for signature comparison
///
/// Produces a canonical JSON string with sorted keys for deterministic
/// comparison of tool arguments.
/// Canonical, key-sorted JSON rendering of a value.
///
/// Two calls with the same arguments produce the same string regardless of map
/// iteration order, which is what makes a doom-loop signature comparable. The
/// agent extension registry uses it to fill `ToolCallContext::args_hash`.
#[must_use]
pub fn stable_stringify(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by_key(|(k, _)| *k);
            let pairs: Vec<String> = entries
                .iter()
                .map(|(k, v)| format!("\"{}\":{}", k, stable_stringify(v)))
                .collect();
            format!("{{{}}}", pairs.join(","))
        }
        serde_json::Value::Array(arr) => {
            let items: Vec<String> = arr.iter().map(stable_stringify).collect();
            format!("[{}]", items.join(","))
        }
        _ => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_safety_controller_allows_normal_calls() {
        let safety = SafetyController::new();
        let args = json!({"command": "ls -la"});

        let verdict = safety.check_tool_call("bash", &args);
        assert_eq!(verdict, SafetyVerdict::Allow);
    }

    #[test]
    fn test_doom_loop_detection() {
        let mut safety = SafetyController::new();
        let args = json!({"command": "ls"});

        // First two calls should be allowed
        assert_eq!(safety.check_tool_call("bash", &args), SafetyVerdict::Allow);
        safety.record_tool_call("bash", &args);

        assert_eq!(safety.check_tool_call("bash", &args), SafetyVerdict::Allow);
        safety.record_tool_call("bash", &args);

        // Third identical call triggers doom loop
        let verdict = safety.check_tool_call("bash", &args);
        assert!(matches!(verdict, SafetyVerdict::BlockDoomLoop { .. }));
    }

    #[test]
    fn test_doom_loop_different_args_allowed() {
        let mut safety = SafetyController::new();

        // Different args should not trigger doom loop
        safety.record_tool_call("bash", &json!({"command": "ls"}));
        safety.record_tool_call("bash", &json!({"command": "pwd"}));

        let verdict = safety.check_tool_call("bash", &json!({"command": "whoami"}));
        assert_eq!(verdict, SafetyVerdict::Allow);
    }

    #[test]
    fn test_doom_loop_different_tools_allowed() {
        let mut safety = SafetyController::new();
        let args = json!({"path": "/tmp"});

        // Same args but different tools should not trigger doom loop
        safety.record_tool_call("read", &args);
        safety.record_tool_call("read", &args);

        let verdict = safety.check_tool_call("glob", &args);
        assert_eq!(verdict, SafetyVerdict::Allow);
    }

    #[test]
    fn test_rate_limiting() {
        let config = SafetyConfig {
            doom_loop_threshold: 3,
            rate_limit: 3,
            rate_window: Duration::from_secs(10),
        };
        let mut safety = SafetyController::with_config(config);

        // First 3 calls allowed (different args to avoid doom loop)
        safety.record_tool_call("bash", &json!({"command": "ls"}));
        safety.record_tool_call("bash", &json!({"command": "pwd"}));
        safety.record_tool_call("bash", &json!({"command": "whoami"}));

        // 4th call should be rate limited
        let verdict = safety.check_tool_call("bash", &json!({"command": "date"}));
        assert!(matches!(verdict, SafetyVerdict::BlockRateLimit { .. }));
    }

    #[test]
    fn test_rate_limit_different_tools_independent() {
        let config = SafetyConfig {
            doom_loop_threshold: 5,
            rate_limit: 2,
            rate_window: Duration::from_secs(10),
        };
        let mut safety = SafetyController::with_config(config);

        // Fill up bash rate limit
        safety.record_tool_call("bash", &json!({"command": "ls"}));
        safety.record_tool_call("bash", &json!({"command": "pwd"}));

        // read should still be allowed (different tool)
        let verdict = safety.check_tool_call("read", &json!({"path": "/tmp"}));
        assert_eq!(verdict, SafetyVerdict::Allow);
    }

    #[test]
    fn test_reset_clears_state() {
        let mut safety = SafetyController::new();
        let args = json!({"command": "ls"});

        // Build up state
        safety.record_tool_call("bash", &args);
        safety.record_tool_call("bash", &args);

        // Reset
        safety.reset();

        // Should be allowed again (state cleared)
        let verdict = safety.check_tool_call("bash", &args);
        assert_eq!(verdict, SafetyVerdict::Allow);
    }

    #[test]
    fn test_is_retryable_error() {
        // Retryable errors
        assert!(is_retryable_error("Error: rate limit exceeded"));
        assert!(is_retryable_error("Server returned 429 Too Many Requests"));
        assert!(is_retryable_error("API is overloaded, please try again"));
        assert!(is_retryable_error("503 Service Unavailable"));
        assert!(is_retryable_error("Internal server error (500)"));
        assert!(is_retryable_error("Temporarily unavailable"));

        // Not retryable
        assert!(!is_retryable_error("File not found"));
        assert!(!is_retryable_error("Permission denied"));
        assert!(!is_retryable_error("Invalid syntax"));
    }

    #[test]
    fn test_stable_stringify_sorts_keys() {
        let obj1 = json!({"b": 2, "a": 1});
        let obj2 = json!({"a": 1, "b": 2});

        assert_eq!(stable_stringify(&obj1), stable_stringify(&obj2));
        assert_eq!(stable_stringify(&obj1), r#"{"a":1,"b":2}"#);
    }

    #[test]
    fn test_stable_stringify_nested() {
        let obj = json!({
            "z": {"b": 2, "a": 1},
            "y": [3, 2, 1]
        });

        let result = stable_stringify(&obj);
        assert!(result.contains(r#""y":[3,2,1]"#));
        assert!(result.contains(r#""z":{"a":1,"b":2}"#));
    }

    #[test]
    fn test_doom_loop_with_custom_threshold() {
        let config = SafetyConfig {
            doom_loop_threshold: 5,
            rate_limit: 10,
            rate_window: Duration::from_secs(10),
        };
        let mut safety = SafetyController::with_config(config);
        let args = json!({"command": "ls"});

        // 4 identical calls should still be allowed
        for _ in 0..4 {
            assert_eq!(safety.check_tool_call("bash", &args), SafetyVerdict::Allow);
            safety.record_tool_call("bash", &args);
        }

        // 5th call triggers doom loop
        let verdict = safety.check_tool_call("bash", &args);
        assert!(matches!(verdict, SafetyVerdict::BlockDoomLoop { .. }));
    }
}
