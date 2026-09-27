//! A closed, caller-owned read without a provider round trip.
//!
//! This is an execution primitive, not a prompt router. The composing host
//! supplies the admitted proposal and owns authorization, lease fencing,
//! durable call identity, canonical receipts and final response guardrails.
//! In particular, an error after ToolCall must not cause implicit replay.

use super::*;
use maestro_runtime_contracts::{BuiltinWorkTypeNamesProposal, BuiltinWorkTypeNamesResult};

const TOOL_NAME: &str = "work_type.builtins";

fn require_unchanged_hook(result: NativeHookResult) -> Result<()> {
    match result {
        NativeHookResult::Continue => Ok(()),
        NativeHookResult::Block { reason } => bail!("Typed read blocked by hook: {reason}"),
        NativeHookResult::ModifyInput { .. } | NativeHookResult::InjectContext { .. } => {
            bail!("Typed read requires model reconsideration after hook modification")
        }
    }
}

impl NativeAgentRunner {
    pub(super) async fn run_builtin_work_type_names(
        &mut self,
        content: &str,
        call_id: &str,
        proposal: &BuiltinWorkTypeNamesProposal,
        cancellation: &CancellationToken,
    ) -> Result<BuiltinWorkTypeNamesResult> {
        // A fresh actor prevents this closed operation from silently ignoring
        // prior conversation, deferred steering or a prior execution attempt.
        if self.turn_index != 0 || !self.messages.is_empty() {
            bail!("Typed reads require a fresh actor");
        }
        if content.trim().is_empty() || content.len() > 65_536 {
            bail!("Typed read prompt is empty or too large");
        }
        if call_id.is_empty()
            || call_id.len() > 128
            || !call_id.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':' | b'.')
            })
        {
            bail!("Typed read requires a bounded caller-owned call identity");
        }
        proposal.validate()?;
        if !self.external_tools.contains(TOOL_NAME)
            || !self.tools.contains_key(TOOL_NAME)
            || !self.active_tool_names.contains(TOOL_NAME)
        {
            bail!("Typed read tool is not admitted as an active caller-owned tool");
        }
        self.current_turn_id = Uuid::new_v4().to_string();
        self.turn_index = 1;
        self.turn_tool_calls = 0;
        self.workflow_state.reset();
        self.tool_executor.reset_coding_turn();
        self.notify_extensions_user_turn_start();

        // Cancellation can stop waiting, but cannot erase an external effect.
        // The owner still fences and records any in-flight execution. Tombstone
        // the local response identity so late replies cannot satisfy another call.
        let outcome = tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(anyhow::anyhow!("Typed read cancelled")),
            _ = self.shutdown_token.clone().cancelled_owned() => Err(anyhow::anyhow!("Typed read actor shut down")),
            result = self.run_builtin_work_type_names_inner(content, call_id, proposal, cancellation) => result,
        };
        self.set_tool_batch_active(false);
        if outcome.is_err() {
            self.tool_response_coordinator
                .discard_cancelled(&HashSet::from([call_id.to_owned()]));
        }
        self.extensions.on_turn_end(&TurnEndContext {
            turn_id: self.current_turn_id.clone(),
            tool_calls: self.turn_tool_calls,
            interrupted: outcome.is_err(),
        });
        outcome
    }

    async fn run_builtin_work_type_names_inner(
        &mut self,
        content: &str,
        call_id: &str,
        proposal: &BuiltinWorkTypeNamesProposal,
        cancellation: &CancellationToken,
    ) -> Result<BuiltinWorkTypeNamesResult> {
        require_unchanged_hook(self.hooks.hook_user_prompt_submit(content, 0).await)?;
        require_unchanged_hook(
            self.hooks
                .hook_pre_message(content, &[], Some(&self.config.model))
                .await,
        )?;
        let args = proposal.tool_arguments()?;
        let mut budget = TurnStepBudget::new(self.config.resolved_max_turn_steps());
        budget
            .admit_tool(TOOL_NAME, &args)
            .map_err(anyhow::Error::msg)?;
        if let Some(state) = &self.process_budget {
            state
                .lock()
                .map_err(|_| anyhow::anyhow!("process budget poisoned"))?
                .admit_tools(1)
                .map_err(anyhow::Error::msg)?;
        }
        let (hook_args, context) = run_pre_tool_use_hook(&self.hooks, TOOL_NAME, call_id, &args)
            .await
            .map_err(anyhow::Error::msg)?;
        if hook_args != args || context.is_some() {
            bail!("Typed read requires model reconsideration after tool hook modification");
        }
        let safe_args = self.credential_vault.vault_in_json(&args);
        if safe_args != args {
            bail!("Typed read arguments require credential projection");
        }
        match self.plan_tool_call_through_extensions(call_id, TOOL_NAME, &safe_args) {
            ExtensionVerdict::Proceed => {}
            ExtensionVerdict::Block { reason } => bail!("Typed read blocked: {reason}"),
            ExtensionVerdict::Steer { message } => bail!("Typed read requires steering: {message}"),
        }
        // Exactly as in the ordinary external-tool path, the caller applies
        // its own firewall and approval policy. Never fall back to a local tool.
        let firewall = NativeFirewallVerdict::Allow;
        let approval = tool_requires_approval(
            self.config.approval_mode,
            true,
            &firewall,
            &self.tool_executor,
            TOOL_NAME,
            &safe_args,
            &self.denial_memory,
        );
        if approval != ApprovalDecision::Required {
            bail!("Typed read requires an explicit caller-owned execution response");
        }
        require_unchanged_hook(
            self.hooks
                .hook_permission_request(TOOL_NAME, call_id, &args, "tool requires approval")
                .await,
        )?;
        if cancellation.is_cancelled() {
            bail!("Typed read cancelled before dispatch");
        }
        let call = ToolCallContext {
            call_id: call_id.to_owned(),
            tool_name: TOOL_NAME.to_owned(),
            args: args.clone(),
            safe_args,
            extra_context: None,
            pre_hook_args: args,
            initial_firewall_verdict: firewall,
            approval_inline_env: None,
        };
        self.set_tool_batch_active(true);
        self.event_tx
            .send(deferred_tool_call_event(&call, true))
            .map_err(|_| anyhow::anyhow!("Typed read caller disconnected"))?;
        let (approved, result, source) = match self
            .tool_response_coordinator
            .wait_for_tool_response(call_id, cancellation)
            .await
        {
            ToolResponseWait::Response(response) => response,
            ToolResponseWait::Cancelled => bail!("Typed read cancelled while awaiting owner"),
            ToolResponseWait::Closed => bail!("Typed read owner response channel closed"),
        };
        let execution = if approved {
            result.map(|result| {
                ToolExecution::from_legacy(call_id, TOOL_NAME, source, result)
                    .with_managed_policy(self.tool_executor.managed_policy_metadata())
            })
        } else {
            Some(
                ToolExecution::denied(call_id, TOOL_NAME, DenialReason::User)
                    .with_managed_policy(self.tool_executor.managed_policy_metadata()),
            )
        };
        let original = execution.as_ref().and_then(|execution| {
            let raw = execution.raw_content();
            let vaulted = self.credential_vault.vault_in_text(&raw);
            (raw == vaulted).then(|| {
                (
                    self.credential_vault
                        .vault_in_text(&execution.model_content()),
                    vaulted,
                )
            })
        });
        // Shared finalization enforces vaulting, output bounds, PostToolUse,
        // EvalGate, workflow bookkeeping and result extensions before parsing.
        let mut results = [self
            .finalize_tool_call_result(call, approved, execution)
            .await];
        self.apply_tool_batch_end_extensions(&mut results);
        let ContentBlock::ToolResult {
            content,
            is_error: Some(false),
            ..
        } = &results[0]
        else {
            bail!("Typed read did not produce an accepted owner result");
        };
        let Some((expected, raw)) = original else {
            bail!("Typed read has no owner result");
        };
        if content != &expected {
            bail!("Typed read result requires model reconsideration after policy projection");
        }
        // Preserve the untrusted model envelope above, but parse the vaulted
        // raw value internally. Parsing the envelope would reject valid remote
        // owner results; stripping it by string matching would be unsafe.
        let owner: Value = serde_json::from_str(&raw)
            .context("Typed read owner result cannot be safely rendered")?;
        BuiltinWorkTypeNamesResult::from_owner_result(proposal, &owner).map_err(Into::into)
    }
}
