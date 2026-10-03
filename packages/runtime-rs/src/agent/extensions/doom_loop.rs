//! Doom-loop and rate-limit enforcement, as an extension tenant.
//!
//! The detector itself stays in [`crate::agent::safety`]. This module is the
//! adapter that lets the agent loop reach it through the extension registry
//! instead of holding a [`SafetyController`] field of its own:
//!
//! - [`AgentExtension::on_tool_call_planned`] maps
//!   [`SafetyController::check_tool_call`] onto an [`ExtensionVerdict`];
//! - [`AgentExtension::on_tool_result`] records the executed call;
//! - [`AgentExtension::on_user_turn_start`] resets the sliding window.
//!
//! Both `SafetyVerdict::BlockDoomLoop` and `SafetyVerdict::BlockRateLimit`
//! become `ExtensionVerdict::Block` with the same reason string. The agent loop
//! handled both the same way before this tenant existed (same non-fatal error
//! event, same error tool result), so the collapse changes nothing the user or
//! the model sees.

use super::{
    AgentExtension, ExtensionVerdict, ToolCallContext, ToolResultContext, ToolResultPayload,
    TurnStartContext,
};
use crate::agent::safety::{SafetyConfig, SafetyController, SafetyVerdict};

/// Registry name of the doom-loop tenant.
pub const DOOM_LOOP_EXTENSION: &str = "doom-loop";

/// Extension tenant wrapping the [`SafetyController`] detector.
#[derive(Debug)]
pub struct DoomLoopExtension {
    safety: SafetyController,
}

impl DoomLoopExtension {
    /// A tenant with the default [`SafetyConfig`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            safety: SafetyController::new(),
        }
    }

    /// A tenant with a custom [`SafetyConfig`].
    #[must_use]
    pub fn with_config(config: SafetyConfig) -> Self {
        Self {
            safety: SafetyController::with_config(config),
        }
    }

    /// The detector this tenant wraps, for tests and diagnostics.
    #[must_use]
    pub fn controller(&self) -> &SafetyController {
        &self.safety
    }
}

impl Default for DoomLoopExtension {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentExtension for DoomLoopExtension {
    fn allows_independent_script_results(&self) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        DOOM_LOOP_EXTENSION
    }

    fn on_user_turn_start(&mut self, _cx: &TurnStartContext) {
        self.safety.reset();
    }

    fn on_tool_call_planned(&mut self, cx: &ToolCallContext) -> ExtensionVerdict {
        match self.safety.check_tool_call(&cx.tool_name, &cx.args) {
            SafetyVerdict::Allow => ExtensionVerdict::Proceed,
            SafetyVerdict::BlockDoomLoop { reason } | SafetyVerdict::BlockRateLimit { reason } => {
                ExtensionVerdict::Block { reason }
            }
        }
    }

    fn on_tool_result(&mut self, cx: &ToolResultContext, _result: &mut ToolResultPayload) {
        self.safety.record_tool_call(&cx.tool_name, &cx.args);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::extensions::ExtensionRegistry;
    use std::time::Duration;

    fn planned(tool_name: &str, args: &serde_json::Value, call_index: u64) -> ToolCallContext {
        ToolCallContext {
            turn_id: "turn-1".to_string(),
            call_id: format!("call-{call_index}"),
            tool_name: tool_name.to_string(),
            args_hash: crate::agent::safety::stable_stringify(args),
            args: args.clone(),
            call_index,
        }
    }

    fn executed(tool_name: &str, args: &serde_json::Value, call_index: u64) -> ToolResultContext {
        ToolResultContext {
            edit: None,
            turn_id: "turn-1".to_string(),
            call_id: format!("call-{call_index}"),
            tool_name: tool_name.to_string(),
            args_hash: crate::agent::safety::stable_stringify(args),
            args: args.clone(),
            is_error: false,
            duration_ms: 1,
        }
    }

    fn empty_payload() -> ToolResultPayload {
        ToolResultPayload::default()
    }

    #[test]
    fn doom_loop_blocks_through_the_registry_after_the_threshold() {
        let mut registry = ExtensionRegistry::with_default_tenants();
        let args = serde_json::json!({"command": "cat missing.txt"});

        // Default threshold is 3, so the third identical call is the one blocked.
        for index in 0..2 {
            assert_eq!(
                registry.on_tool_call_planned(&planned("bash", &args, index)),
                ExtensionVerdict::Proceed,
                "call {index} should be allowed"
            );
            registry.on_tool_result(&executed("bash", &args, index), &mut empty_payload());
        }

        let reason = match registry.on_tool_call_planned(&planned("bash", &args, 2)) {
            ExtensionVerdict::Block { reason } => reason,
            other => panic!("expected the third identical call to be blocked, got {other:?}"),
        };
        assert!(
            reason.contains("doom loop"),
            "block reason should name the doom loop: {reason}"
        );
    }

    #[test]
    fn a_new_user_turn_clears_the_doom_loop_window_through_the_registry() {
        let mut registry = ExtensionRegistry::with_default_tenants();
        let args = serde_json::json!({"command": "cat missing.txt"});

        for index in 0..2 {
            let _ = registry.on_tool_call_planned(&planned("bash", &args, index));
            registry.on_tool_result(&executed("bash", &args, index), &mut empty_payload());
        }

        registry.on_user_turn_start(&TurnStartContext {
            turn_id: "turn-2".to_string(),
            turn_index: 2,
        });

        assert_eq!(
            registry.on_tool_call_planned(&planned("bash", &args, 0)),
            ExtensionVerdict::Proceed
        );
    }

    #[test]
    fn different_arguments_do_not_trip_the_detector() {
        let mut registry = ExtensionRegistry::with_default_tenants();

        for index in 0..5 {
            let args = serde_json::json!({"command": format!("echo {index}")});
            assert_eq!(
                registry.on_tool_call_planned(&planned("bash", &args, index)),
                ExtensionVerdict::Proceed
            );
            registry.on_tool_result(&executed("bash", &args, index), &mut empty_payload());
        }
    }

    #[test]
    fn rate_limit_blocks_are_reported_as_the_same_verdict_variant() {
        let mut extension = DoomLoopExtension::with_config(SafetyConfig {
            // High enough that the doom-loop rule cannot fire first.
            doom_loop_threshold: 100,
            rate_limit: 2,
            rate_window: Duration::from_mins(1),
        });
        let mut registry = ExtensionRegistry::new();

        // Record two calls directly so the rate window is already full.
        for index in 0..2 {
            let args = serde_json::json!({"command": format!("echo {index}")});
            extension.on_tool_result(&executed("bash", &args, index), &mut empty_payload());
        }
        registry.register(Box::new(extension));

        let args = serde_json::json!({"command": "echo again"});
        let reason = match registry.on_tool_call_planned(&planned("bash", &args, 2)) {
            ExtensionVerdict::Block { reason } => reason,
            other => panic!("expected a rate-limit block, got {other:?}"),
        };
        assert!(
            reason.contains("rate limit"),
            "block reason should name the rate limit: {reason}"
        );
    }

    #[test]
    fn the_tenant_leaves_the_tool_result_payload_alone() {
        let mut extension = DoomLoopExtension::new();
        let args = serde_json::json!({"command": "ls"});
        let mut payload = ToolResultPayload {
            content: "listing".to_string(),
            is_error: false,
        };
        extension.on_tool_result(&executed("bash", &args, 0), &mut payload);
        assert_eq!(payload.content, "listing");
        assert!(!payload.is_error);
    }
}
