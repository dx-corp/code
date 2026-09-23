//! Native-agent tool execution lifecycle helpers.
//!
//! This module owns the policy, hook, approval, deferred-execution, and
//! cancellation transitions around a tool call. The runner owns stream and
//! provider orchestration; keeping this state machine here makes that boundary
//! explicit while preserving the runner's behavior.

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::Result;
use serde_json::Value;
use tokio::sync::mpsc;

use super::super::native_host::{
    ApprovalMode, NativeExecutionHostHandle, NativeFirewallVerdict, NativeHookEvent,
    NativeHookResult,
};
use super::super::protocol::InlineToolApprovalContext;
use super::super::safety::DenialMemory;
use super::super::{
    CredentialVault, DenialReason, ExecutionPhase, ExecutionSource, FromAgent,
    ManagedPolicyMetadata, ToolExecution, ToolResult,
};
use super::{AgentCommand, prompt_kind_starts_main_request};
use crate::ai::ContentBlock;

pub(super) fn normalize_post_hook_tool_args(
    tool_name: &str,
    mut args: serde_json::Value,
) -> (serde_json::Value, bool) {
    if !tool_name.eq_ignore_ascii_case("bash") {
        return (args, false);
    }

    let Some(object) = args.as_object_mut() else {
        return (args, false);
    };
    let command_is_empty = object
        .get("command")
        .and_then(serde_json::Value::as_str)
        .is_none_or(|command| command.trim().is_empty());
    if !command_is_empty {
        return (args, false);
    }

    object.insert("command".to_string(), serde_json::json!("pwd"));
    (args, true)
}

/// What the approval gate decided for one tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ApprovalDecision {
    /// Execute without asking.
    NotRequired,
    /// Ask the user.
    Required,
    /// The user already refused this exact call in this turn. Refuse it again
    /// without asking.
    RefusedEarlierThisTurn,
}

impl ApprovalDecision {
    /// Whether the call must not execute without a fresh human decision.
    ///
    /// A repeat of a refused call answers `true` as well: it does not run, and
    /// the caller reports the earlier refusal instead of prompting.
    pub(super) fn requires_approval(self) -> bool {
        !matches!(self, Self::NotRequired)
    }

    /// Whether the caller should refuse without prompting.
    pub(super) fn is_repeat_refusal(self) -> bool {
        matches!(self, Self::RefusedEarlierThisTurn)
    }
}

/// The message returned to the model for a call refused earlier this turn.
pub(super) fn repeat_refusal_message(tool_name: &str) -> String {
    format!(
        "Tool denied by user: `{tool_name}` was already refused with these exact arguments \
         earlier in this turn. Do not retry it; change the approach or ask the user."
    )
}

/// Decide whether a tool call must wait for caller approval before the
/// runner executes it, given the active `ApprovalMode`.
///
/// This is the single decision point behind `requires_approval` in
/// `run_loop`, and it is also the value reported on the `FromAgent::ToolCall`
/// event. Callers with their own approval UI (the interactive TUI, the
/// headless server) trust that field instead of recomputing their own
/// verdict; keeping this logic in one pure function (rather than duplicated
/// per-caller, as it used to be split between here and `app.rs`) is what
/// prevents the two from disagreeing again -- see issues #3149 and #3156.
///
/// - `is_external_tool`: the caller owns execution and its own approval
///   policy for this tool (see [`NativeAgentRunner`]'s external-tools doc).
///   It requires approval regardless of mode unless the exact local MCP
///   transport and schema identity has a remembered grant. Managed and
///   enterprise MCP tools cannot receive such a grant.
/// - [`ApprovalMode::Yolo`]: never require approval for ordinary calls
///   (including firewall soft holds; a hard [`FirewallVerdict::Block`] is
///   handled separately and always denies regardless of mode). The one
///   exception is a `bypass_sandbox` request: waiving the native sandbox is
///   the per-command escape hatch and must always be a decision a human
///   explicitly makes, so it requires approval even in Yolo.
/// - [`ApprovalMode::Safe`]: always require approval.
/// - [`ApprovalMode::Selective`]: defer to the tool executor's static/dynamic
///   per-tool heuristic (e.g. `bash` inspects the command).
pub(super) fn tool_requires_approval(
    approval_mode: ApprovalMode,
    is_external_tool: bool,
    firewall_verdict: &NativeFirewallVerdict,
    tool_executor: &NativeExecutionHostHandle,
    tool_name: &str,
    args: &serde_json::Value,
    denials: &DenialMemory,
) -> ApprovalDecision {
    if tool_executor.has_code_authority()
        && approval_mode != ApprovalMode::Safe
        && !is_external_tool
        && !tool_executor.requires_sandbox_bypass_approval(tool_name, args)
        && !matches!(firewall_verdict, NativeFirewallVerdict::Block { .. })
    {
        return if denials.was_refused(tool_name, args) {
            ApprovalDecision::RefusedEarlierThisTurn
        } else {
            // The executor obtains a fresh, exact-bound Identity decision
            // before dispatch; this only suppresses the redundant UI prompt.
            ApprovalDecision::NotRequired
        };
    }
    if is_external_tool && tool_executor.mcp_permission_allows(tool_name) {
        return ApprovalDecision::NotRequired;
    }
    let mode_requires_approval = match approval_mode {
        ApprovalMode::Yolo => tool_executor.requires_sandbox_bypass_approval(tool_name, args),
        ApprovalMode::Safe => true,
        ApprovalMode::Selective => tool_executor.requires_approval(tool_name, args),
    };
    let firewall_requires_approval = matches!(
        firewall_verdict,
        NativeFirewallVerdict::RequireApproval { .. }
    ) && approval_mode != ApprovalMode::Yolo;
    if !(is_external_tool || mode_requires_approval || firewall_requires_approval) {
        return ApprovalDecision::NotRequired;
    }
    // Only calls that would prompt are checked. A call that needs no approval
    // this turn was never refused through this gate.
    if denials.was_refused(tool_name, args) {
        return ApprovalDecision::RefusedEarlierThisTurn;
    }
    ApprovalDecision::Required
}

pub(super) fn parse_tool_input(tool_name: &str, json: &str) -> Result<serde_json::Value, String> {
    if json.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(json)
        .map_err(|err| format!("Failed to parse tool input JSON for '{tool_name}': {err}"))
}

pub(super) fn abort_pending_tools_after_stream_error(
    assistant_content: &mut Vec<ContentBlock>,
    pending_tool_calls: &mut Vec<(String, String, Value, Option<String>)>,
) {
    pending_tool_calls.clear();
    assistant_content.retain(|block| !matches!(block, ContentBlock::ToolUse { .. }));
}

/// Lifecycle managers must receive opaque credential references so child
/// prompts and durable records never receive the parent's resolved secrets.
/// The child receives the shared vault separately and resolves only at the
/// admitted tool execution boundary.
pub(super) fn tool_args_for_execution(
    tool_name: &str,
    safe_args: &serde_json::Value,
    credential_vault: &CredentialVault,
) -> serde_json::Value {
    if tool_name.eq_ignore_ascii_case("spawn_subagent")
        || tool_name.eq_ignore_ascii_case("resume_subagent")
        || tool_name.eq_ignore_ascii_case("control_subagent")
    {
        safe_args.clone()
    } else {
        credential_vault.resolve_in_json(safe_args)
    }
}

/// Everything needed to finish a tool call once its execution decision is
/// known. Approval-needing calls capture their hook-adjusted arguments when
/// their batched `ToolCall` event is emitted. Auto-approved calls behind that
/// boundary also retain the original model input so PreToolUse and all
/// argument derivations can be refreshed immediately before their event and
/// execution.
pub(super) struct ToolCallContext {
    pub(super) call_id: String,
    pub(super) tool_name: String,
    pub(super) args: serde_json::Value,
    pub(super) safe_args: serde_json::Value,
    pub(super) extra_context: Option<String>,
    pub(super) pre_hook_args: serde_json::Value,
    pub(super) initial_firewall_verdict: NativeFirewallVerdict,
    /// Exact inline command and execution context captured before the approval
    /// event was emitted. Approved calls must still match its environment at
    /// execution time.
    pub(super) approval_inline_env: Option<InlineToolApprovalContext>,
}

pub(super) enum DeferredToolCall {
    AwaitApproval(ToolCallContext),
    Execute(ToolCallContext),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DeferredToolCallDisposition {
    AwaitApproval,
    Execute,
}

pub(super) fn deferred_tool_call_disposition(
    requires_approval: bool,
    has_prior_deferred_call: bool,
) -> Option<DeferredToolCallDisposition> {
    if requires_approval {
        Some(DeferredToolCallDisposition::AwaitApproval)
    } else if has_prior_deferred_call {
        Some(DeferredToolCallDisposition::Execute)
    } else {
        None
    }
}

/// Run `PreToolUse` for one call and reduce the result to the arguments to
/// execute with plus any context the hook injected.
///
/// `Err` carries the block reason. Every path that decides whether a tool runs
/// uses this, so a policy hook cannot be enforced on one transport and skipped
/// on another.
pub(super) async fn run_pre_tool_use_hook(
    hooks: &NativeExecutionHostHandle,
    tool_name: &str,
    call_id: &str,
    args: &serde_json::Value,
) -> Result<(serde_json::Value, Option<String>), String> {
    match hooks.hook_pre_tool_use(tool_name, call_id, args).await {
        NativeHookResult::Block { reason } => Err(reason),
        NativeHookResult::ModifyInput { new_input } => Ok((new_input, None)),
        NativeHookResult::InjectContext { context } => Ok((args.clone(), Some(context))),
        NativeHookResult::Continue => Ok((args.clone(), None)),
    }
}

pub(super) async fn rerun_deferred_pre_tool_use(
    hooks: &NativeExecutionHostHandle,
    call: &ToolCallContext,
) -> Result<(serde_json::Value, Option<String>), String> {
    run_pre_tool_use_hook(hooks, &call.tool_name, &call.call_id, &call.pre_hook_args).await
}

/// The context a hook asked to add, if it asked for any.
///
/// `PostToolUse` hooks return `hookSpecificOutput.contextToAdd` as
/// `InjectContext`; the documented effect is that the text reaches the model
/// with the tool result.
pub(super) fn hook_injected_context(result: NativeHookResult) -> Option<String> {
    match result {
        NativeHookResult::InjectContext { context } => Some(context),
        _ => None,
    }
}

/// What the post-execution hooks asked for on one finished tool call.
#[derive(Default)]
pub(super) struct PostExecutionHooks {
    /// Context the hooks asked to add to the tool result.
    pub(super) context: Option<String>,
    /// Why an `EvalGate` hook rejected the result, if it did.
    pub(super) rejected: Option<String>,
}

/// Run `PostToolUse` and then `EvalGate` for one finished tool call.
///
/// `EvalGate` receives the same tool name, arguments, and raw output; its input
/// type is shaped for exactly this point and it had no dispatch site at all, so
/// a configured evaluation hook silently never ran. It runs after `PostToolUse`
/// so a gate scores the result a `PostToolUse` hook has already observed.
///
/// A gate's `block` cannot un-run the tool, so it is reported as a failed tool
/// result rather than pretending the call was prevented.
pub(super) async fn run_post_execution_hooks(
    hooks: &NativeExecutionHostHandle,
    tool_name: &str,
    call_id: &str,
    args: &serde_json::Value,
    raw_output: &str,
    is_error: bool,
    duration_ms: u64,
) -> PostExecutionHooks {
    let mut outcome = PostExecutionHooks {
        context: hook_injected_context(
            hooks
                .hook_post_tool_use(tool_name, call_id, args, raw_output, is_error, duration_ms)
                .await,
        ),
        rejected: None,
    };

    match hooks
        .hook_eval_gate(tool_name, call_id, args, raw_output)
        .await
    {
        NativeHookResult::Block { reason } => outcome.rejected = Some(reason),
        gate => {
            if let Some(gate_context) = hook_injected_context(gate) {
                outcome.context = Some(match outcome.context {
                    Some(existing) => format!("{existing}\n\n{gate_context}"),
                    None => gate_context,
                });
            }
        }
    }
    outcome
}

/// Append hook-injected context to a tool result body.
///
/// The context is bounded, escaped, and wrapped in the `<system_reminder>`
/// delimiter by the host hook renderer, so hook text is separated from tool
/// output and cannot forge the delimiter. Empty or whitespace-only context is
/// dropped rather than appended as blank lines the model has to read. The host
/// preserves its existing context length limit and error rendering.
pub(super) fn append_hook_context(
    host: &NativeExecutionHostHandle,
    content: String,
    event: NativeHookEvent,
    context: Option<&str>,
) -> String {
    let Some(context) = context else {
        return content;
    };
    let rendered = match host.render_hook_context(event, context) {
        Ok(rendered) if rendered.is_empty() => return content,
        Ok(rendered) => rendered,
        Err(error) => error,
    };
    format!("{content}\n\n{rendered}")
}

pub(super) fn deferred_tool_call_event(
    call: &ToolCallContext,
    requires_approval: bool,
) -> FromAgent {
    FromAgent::ToolCall {
        call_id: call.call_id.clone(),
        tool: call.tool_name.clone(),
        args: call.safe_args.clone(),
        requires_approval,
        approval_inline_env: if requires_approval {
            call.approval_inline_env.clone()
        } else {
            None
        },
    }
}

pub(super) fn deferred_approved_policy_rejection(
    initial_verdict: &NativeFirewallVerdict,
    current_verdict: NativeFirewallVerdict,
) -> Option<String> {
    match current_verdict {
        NativeFirewallVerdict::Block { reason } => Some(reason),
        NativeFirewallVerdict::RequireApproval { reason }
            if !matches!(
                initial_verdict,
                NativeFirewallVerdict::RequireApproval {
                    reason: initial_reason
                } if initial_reason == &reason
            ) =>
        {
            Some(format!(
                "Tool requires fresh approval after earlier tool execution: {reason}"
            ))
        }
        NativeFirewallVerdict::RequireApproval { .. } | NativeFirewallVerdict::Allow => None,
    }
}

pub(super) fn approved_input_change_rejection(
    approved_args: &serde_json::Value,
    refreshed_args: &serde_json::Value,
) -> Option<&'static str> {
    (approved_args != refreshed_args)
        .then_some("Tool input changed after approval; retry to review refreshed input")
}

pub(super) fn approved_inline_env_change_rejection(
    approved_env: Option<&HashMap<String, String>>,
    current_env: Option<&HashMap<String, String>>,
) -> Option<&'static str> {
    approved_env
        .is_some_and(|approved| current_env != Some(approved))
        .then_some(
            "Inline tool environment changed after approval; retry to review refreshed environment",
        )
}

pub(super) fn clear_stashed_prompts(deferred_commands: &mut VecDeque<AgentCommand>) -> usize {
    let original_len = deferred_commands.len();
    deferred_commands.retain(|command| {
        !matches!(
            command,
            AgentCommand::Prompt {
                kind,
                ..
            } if prompt_kind_starts_main_request(*kind)
        )
    });
    original_len - deferred_commands.len()
}

pub(super) fn deferred_hook_block(
    call: &ToolCallContext,
    reason: String,
    emit_tool_call: bool,
    receipt_policy: Option<ManagedPolicyMetadata>,
) -> (Vec<FromAgent>, ContentBlock) {
    let message = format!("Tool blocked by hook: {reason}");
    let mut events = Vec::with_capacity(4);
    if emit_tool_call {
        events.push(deferred_tool_call_event(call, false));
    }
    events.extend([
        FromAgent::HookBlocked {
            call_id: call.call_id.clone(),
            tool: call.tool_name.clone(),
            reason,
        },
        deferred_rejection_output_event(call, &message),
        deferred_safety_rejection_event(call, &message, receipt_policy),
    ]);
    (
        events,
        ContentBlock::ToolResult {
            tool_use_id: call.call_id.clone(),
            content: message,
            is_error: Some(true),
        },
    )
}

pub(super) fn emit_deferred_failure(
    event_tx: &mpsc::UnboundedSender<FromAgent>,
    call: &ToolCallContext,
    reason: &str,
    tool_results: &mut Vec<ContentBlock>,
    receipt_policy: Option<ManagedPolicyMetadata>,
) {
    let _ = event_tx.send(deferred_rejection_output_event(call, reason));
    let _ = event_tx.send(deferred_safety_rejection_event(
        call,
        reason,
        receipt_policy,
    ));
    tool_results.push(ContentBlock::ToolResult {
        tool_use_id: call.call_id.clone(),
        content: reason.to_string(),
        is_error: Some(true),
    });
}

pub(super) fn emit_deferred_policy_failure(
    event_tx: &mpsc::UnboundedSender<FromAgent>,
    call: &ToolCallContext,
    reason: &str,
    tool_results: &mut Vec<ContentBlock>,
    receipt_policy: Option<ManagedPolicyMetadata>,
) {
    let _ = event_tx.send(deferred_rejection_output_event(call, reason));
    let _ = event_tx.send(deferred_policy_rejection_event(
        call,
        reason,
        receipt_policy,
    ));
    tool_results.push(ContentBlock::ToolResult {
        tool_use_id: call.call_id.clone(),
        content: reason.to_string(),
        is_error: Some(true),
    });
}

pub(super) fn invalidate_cache_after_serial_tool(
    tool_executor: &NativeExecutionHostHandle,
    tool_name: &str,
    executed: bool,
) {
    if executed && tool_can_change_workspace(tool_name) {
        tool_executor.clear_cache();
    }
}

pub(super) fn tool_can_change_workspace(tool_name: &str) -> bool {
    !matches!(
        tool_name.to_ascii_lowercase().as_str(),
        "read"
            | "read_image"
            | "glob"
            | "grep"
            | "diff"
            | "list"
            | "find"
            | "search"
            | "parallel_ripgrep"
            | "explore"
            | "websearch"
            | "codesearch"
            | "status"
            | "ask_user"
            | "extract_document"
            | "web_fetch"
            | "webfetch"
            | "screenshot"
            | "mcp_list_resources"
            | "mcp_list_prompts"
            | "mcp_read_resource"
            | "mcp_get_prompt"
            | "get_goal"
            | "vscode_get_diagnostics"
            | "jetbrains_get_diagnostics"
            | "vscode_get_definition"
            | "jetbrains_get_definition"
            | "vscode_find_references"
            | "jetbrains_find_references"
            | "vscode_read_file_range"
            | "jetbrains_read_file_range"
    )
}

pub(super) fn tool_is_visible_to_model(
    tool_name: &str,
    goal_tools_visible: bool,
    include_ide_tools: bool,
) -> bool {
    let name = tool_name.to_ascii_lowercase();
    if matches!(name.as_str(), "get_goal" | "update_goal") {
        return goal_tools_visible;
    }
    include_ide_tools || !(name.starts_with("vscode_") || name.starts_with("jetbrains_"))
}

pub(super) fn deferred_firewall_verdict(
    host: &NativeExecutionHostHandle,
    tool_name: &str,
    args: &serde_json::Value,
    workflow_snapshot: &super::super::safety::WorkflowStateSnapshot,
    annotations: Option<&super::super::native_host::NativeToolAnnotations>,
    is_external_tool: bool,
) -> NativeFirewallVerdict {
    if is_external_tool {
        NativeFirewallVerdict::Allow
    } else {
        host.firewall_verdict(tool_name, args, workflow_snapshot, annotations, false)
    }
}

pub(super) fn deferred_rejection_output_event(call: &ToolCallContext, reason: &str) -> FromAgent {
    FromAgent::ToolOutput {
        call_id: call.call_id.clone(),
        content: reason.to_string(),
    }
}

pub(super) fn deferred_safety_rejection_event(
    call: &ToolCallContext,
    reason: &str,
    receipt_policy: Option<ManagedPolicyMetadata>,
) -> FromAgent {
    let result = ToolResult::failure(reason);
    let receipt = ToolExecution::from_legacy(
        &call.call_id,
        &call.tool_name,
        ExecutionSource::Native,
        result.clone(),
    )
    .with_managed_policy(receipt_policy)
    .receipt;
    FromAgent::ToolEnd {
        call_id: call.call_id.clone(),
        success: false,
        result: Some(result),
        receipt: Some(receipt),
    }
}

pub(super) fn deferred_policy_rejection_event(
    call: &ToolCallContext,
    reason: &str,
    receipt_policy: Option<ManagedPolicyMetadata>,
) -> FromAgent {
    let execution = ToolExecution::denied(
        &call.call_id,
        &call.tool_name,
        DenialReason::ActionFirewall {
            message: reason.to_string(),
        },
    )
    .with_managed_policy(receipt_policy);
    FromAgent::ToolEnd {
        call_id: call.call_id.clone(),
        success: false,
        result: Some(execution.to_legacy()),
        receipt: Some(execution.receipt),
    }
}

pub(super) fn cancelled_deferred_tool(
    call: &ToolCallContext,
    reason: &str,
    receipt_policy: Option<ManagedPolicyMetadata>,
) -> (FromAgent, ContentBlock) {
    let execution = ToolExecution::cancelled(
        &call.call_id,
        &call.tool_name,
        ExecutionSource::Native,
        ExecutionPhase::Queued,
    )
    .with_managed_policy(receipt_policy);
    let result = ToolResult::failure(reason);
    (
        FromAgent::ToolEnd {
            call_id: call.call_id.clone(),
            success: false,
            result: Some(result),
            receipt: Some(execution.receipt),
        },
        ContentBlock::ToolResult {
            tool_use_id: call.call_id.clone(),
            content: reason.to_string(),
            is_error: Some(true),
        },
    )
}

pub(super) fn cancel_deferred_suffix(
    event_tx: &mpsc::UnboundedSender<FromAgent>,
    deferred_calls: impl IntoIterator<Item = DeferredToolCall>,
    tool_results: &mut Vec<ContentBlock>,
    receipt_policy: Option<ManagedPolicyMetadata>,
) -> HashSet<String> {
    let skipped_message = "Skipped after request cancellation.";
    let mut cancelled_ids = HashSet::new();
    for skipped_call in deferred_calls {
        let (call, needs_tool_call) = match skipped_call {
            DeferredToolCall::AwaitApproval(call) => (call, false),
            DeferredToolCall::Execute(call) => (call, true),
        };
        cancelled_ids.insert(call.call_id.clone());
        if needs_tool_call {
            let _ = event_tx.send(deferred_tool_call_event(&call, false));
        }
        let _ = event_tx.send(FromAgent::ToolOutput {
            call_id: call.call_id.clone(),
            content: skipped_message.to_string(),
        });
        let (event, result_block) =
            cancelled_deferred_tool(&call, skipped_message, receipt_policy.clone());
        let _ = event_tx.send(event);
        tool_results.push(result_block);
    }
    cancelled_ids
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_execution_parses_empty_input_as_an_object() {
        assert_eq!(
            parse_tool_input("bash", " \n\t ").expect("empty input is valid"),
            serde_json::json!({})
        );
    }

    #[test]
    fn tool_execution_defers_auto_approved_calls_behind_an_approval() {
        assert_eq!(
            deferred_tool_call_disposition(false, true),
            Some(DeferredToolCallDisposition::Execute)
        );
        assert_eq!(deferred_tool_call_disposition(false, false), None);
    }
}
