//! Wide Events Telemetry - Canonical Turn Events
//!
//! Implements the "wide events" pattern from loggingsucks.com:
//! Instead of scattered log statements, emit ONE rich event per agent turn
//! with comprehensive context for analytics-style querying.
//!
//! # Key Principles
//!
//! - One event per turn, not N log lines
//! - High-cardinality fields for queryability
//! - Tail sampling: always keep errors/slow, sample successes
//! - Optimized for querying, not writing
//!
//! # Example
//!
//! ```rust,ignore
//! use maestro_local_host::telemetry::{TurnCollector, TailSamplingConfig};
//!
//! let mut turn = TurnCollector::new("session-123", 1, TailSamplingConfig::default());
//! turn.set_model(ModelInfo {
//!     id: "claude-opus-4-5-20251101".to_string(),
//!     provider: "anthropic".to_string(),
//!     thinking_level: ThinkingLevel::Medium,
//! });
//!
//! // During tool execution
//! turn.record_tool_start("bash", "call-123", None);
//! // ... execute tool ...
//! turn.record_tool_end("call-123", true, None, None);
//!
//! // At turn end
//! let event = turn.complete(
//!     TurnStatus::Success,
//!     tokens,
//!     0.05,
//!     None,
//!     None,
//! );
//! ```

mod customer_export;
mod onboarding;
mod operation;
mod staged_rollout;
mod tracker;
pub(crate) use staged_rollout::TurnJournal;
mod visibility;
mod wide_events;
pub use visibility::*;

pub use onboarding::*;
pub use staged_rollout::{
    experiments_telemetry_disabled, flush_first_party_telemetry, onboarding_identity_scope,
    purge_revoked_experiment_events, queue_experiment_enrollment, record_canonical_turn_event,
    record_first_party_visibility_event, record_onboarding_event,
    record_staged_rollout_surface_usage,
};
pub use tracker::{TurnTracker, TurnTrackerConfig, TurnTrackerContext};
pub use wide_events::{
    AbortReason, ApprovalMode, CanonicalTurnEvent, ErrorDetails, ExternalTurnEvent, FeatureFlags,
    ModelInfo, SampleReason, SandboxMode, TailSamplingConfig, TelemetryIdentityScope,
    ThinkingLevel, TokenUsage, ToolExecution, TurnCollector, TurnStatus,
};

#[cfg(any(test, feature = "test-support"))]
pub use staged_rollout::test_record_visibility_with_session;
