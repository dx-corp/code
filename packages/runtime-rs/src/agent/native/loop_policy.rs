//! The native actor's call classification, exposed for hosts that run
//! Maestro tools on the dex-loop kernel (`maestro-dex-host`).
//!
//! Each function wraps the one the native actor itself calls, so a tool call
//! is gated, firewalled and classified as read-only identically on both
//! loops. Turn-local refusal memory is the calling loop's own concern: the
//! kernel refuses an exact repeat of an uncertain or refused call itself.

use serde_json::Value;

use super::read_only_tools::is_native_parallel_read_only_tool_call;
use super::tool_execution::{deferred_firewall_verdict, tool_requires_approval};
use crate::agent::denial_memory::DenialMemory;
use crate::agent::native_host::{
    ApprovalMode, NativeExecutionHostHandle, NativeFirewallVerdict, NativeToolAnnotations,
};
use crate::agent::safety::WorkflowStateSnapshot;

/// The action firewall's verdict for one call, as the native actor computes it.
#[must_use]
pub fn firewall_verdict(
    host: &NativeExecutionHostHandle,
    tool_name: &str,
    args: &Value,
    workflow: &WorkflowStateSnapshot,
    annotations: Option<&NativeToolAnnotations>,
    is_external_tool: bool,
) -> NativeFirewallVerdict {
    deferred_firewall_verdict(
        host,
        tool_name,
        args,
        workflow,
        annotations,
        is_external_tool,
    )
}

/// Whether the call needs a human decision before it runs under `mode`.
#[must_use]
pub fn approval_required(
    mode: ApprovalMode,
    is_external_tool: bool,
    firewall: &NativeFirewallVerdict,
    host: &NativeExecutionHostHandle,
    tool_name: &str,
    args: &Value,
) -> bool {
    tool_requires_approval(
        mode,
        is_external_tool,
        firewall,
        host,
        tool_name,
        args,
        &DenialMemory::new(),
    )
    .requires_approval()
}

/// Whether the call may run in a parallel read-only wave.
#[must_use]
pub fn parallel_read_only(
    tool_name: &str,
    requires_approval: bool,
    annotations: Option<&NativeToolAnnotations>,
    explicit_inline_read_only: bool,
) -> bool {
    is_native_parallel_read_only_tool_call(
        tool_name,
        requires_approval,
        annotations,
        explicit_inline_read_only,
    )
}
