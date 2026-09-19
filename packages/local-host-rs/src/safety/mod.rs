//! Safety and Security Module
//!
//! This module implements security controls for the Maestro agent:
//!
//! - **Action Firewall**: Blocks or requires approval for dangerous operations
//! - **Dangerous Patterns**: Regex-based detection of malicious commands
//! - **Bash Analysis**: Parse and analyze shell commands for safety
//! - **Path Containment**: Ensure operations stay within safe directories
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────┐
//! │   Tool Call     │
//! └────────┬────────┘
//!          │
//!          ▼
//! ┌─────────────────┐
//! │ Action Firewall │──► Block / Require Approval / Allow
//! │                 │
//! │ ┌─────────────┐ │
//! │ │  Dangerous  │ │ Regex patterns for malicious commands
//! │ │  Patterns   │ │
//! │ └─────────────┘ │
//! │                 │
//! │ ┌─────────────┐ │
//! │ │    Bash     │ │ Command parsing and analysis
//! │ │  Analyzer   │ │
//! │ └─────────────┘ │
//! │                 │
//! │ ┌─────────────┐ │
//! │ │    Path     │ │ Workspace containment checks
//! │ │ Containment │ │
//! │ └─────────────┘ │
//! └─────────────────┘
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use maestro_local_host::safety::{ActionFirewall, FirewallVerdict};
//!
//! let firewall = ActionFirewall::new("/workspace");
//!
//! // Check a bash command
//! let verdict = firewall.check_bash("rm -rf /");
//! assert!(matches!(verdict, FirewallVerdict::Block { .. }));
//!
//! // Check a file write
//! let verdict = firewall.check_file_write("/etc/passwd", "content");
//! assert!(matches!(verdict, FirewallVerdict::Block { .. }));
//! ```

mod bash_analyzer;
mod dangerous_patterns;
mod firewall;
pub mod guardian;
mod path_containment;
mod policy;
mod safe_mode;

#[cfg(test)]
mod integration_tests;

pub use bash_analyzer::{
    BashAnalysis, CommandRisk, RunAttendance, UNPARSEABLE_INTERACTIVE_REASON,
    UNPARSEABLE_UNATTENDED_REASON, analyze_bash_command, analyze_bash_command_with_attendance,
    canonicalize_for_matching, is_dangerous, is_likely_safe,
};
pub(crate) use bash_analyzer::{find_has_dangerous_predicate, git_args_are_mutating, tokenize};
pub use dangerous_patterns::{
    DangerousPattern, PatternMatch, Severity, check_dangerous_patterns, has_high_severity_pattern,
    most_severe_match,
};
pub use firewall::{ActionFirewall, FirewallContext, FirewallVerdict};
pub use maestro_runtime::agent::denial_memory::{DenialMemory, MAX_DENIAL_TARGET_CHARS};
pub use maestro_runtime::agent::workflow_state::{
    ToolEgress, ToolTag, WorkflowStateSnapshot, WorkflowStateTracker, apply_workflow_state_hooks,
    has_tool_tags, is_human_facing_tool, is_workflow_tracked_tool, looks_like_egress,
};
pub use path_containment::expand_tilde;
pub(crate) use path_containment::is_tilde_path;
pub use path_containment::{
    PathContainment, has_path_traversal, is_path_contained, is_system_path,
};
pub use policy::{
    DisconnectedAuthority, DisconnectedPolicy, DisconnectedRoute, ManagedPolicyAuditEvent,
    ManagedPolicyEnvelope, ManagedPolicyMetadata, ManagedPolicyPublishResult, ManagedPolicyStatus,
    check_model_allowed, check_path_allowed, check_session_limits, disconnected_authority,
    disconnected_policy, disconnected_route, get_policy_limits, managed_policy_audit,
    managed_policy_metadata, managed_policy_status, publish_managed_policy,
    record_managed_policy_audit, refresh_managed_policy, require_vendor_network,
    vendor_network_disabled,
};
#[cfg(any(test, feature = "test-support"))]
pub use safe_mode::PlanModeOverride;
pub use safe_mode::{
    ValidatorResult, is_plan_mode, is_safe_mode_enabled, require_plan, run_validators,
    run_validators_with_diagnostics, set_plan_mode, set_plan_satisfied,
};
