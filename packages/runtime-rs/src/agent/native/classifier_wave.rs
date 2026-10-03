//! Governed classifier waves: all admission and finite bounds precede dispatch.

use super::*;
use futures::StreamExt;

impl NativeAgentRunner {
    pub(super) async fn drain_classifier_wave(
        &mut self,
        pending: &mut Vec<QueuedReadOnlyToolExecution>,
        tool_results: &mut Vec<ContentBlock>,
    ) -> Result<()> {
        let calls = std::mem::take(pending);
        let started = Instant::now();
        let cancel = self
            .codemode_cancel
            .as_ref()
            .unwrap_or(&self.shutdown_token)
            .child_token();
        let mut operations = HashMap::new();
        for call in &calls {
            operations.insert(
                call.call_id.clone(),
                self.begin_tool_operation(&call.call_id, &call.tool_name, &call.execution_args)
                    .await
                    .map_err(anyhow::Error::msg)?,
            );
        }
        let cap = self.output_token_budget.map_or(512, |budget| {
            u64::from(budget)
                .saturating_sub(self.output_tokens_spent)
                .checked_div(calls.len() as u64)
                .unwrap_or(0)
                .min(512) as u32
        });
        let mut prepared = Vec::new();
        let mut refusal =
            (cap == 0 || self.classifier_budget_uncertain || self.codemode_indeterminate)
                .then(|| "Classification wave exceeds its finite output budget".to_owned());
        if refusal.is_none() {
            for call in &calls {
                match self.prepare_classifier(&call.execution_args, cap).await {
                    Ok(request) => prepared.push(request),
                    Err(error) => {
                        refusal = Some(error.to_string());
                        break;
                    }
                }
            }
        }
        if let Some(refusal) = refusal {
            // None of the prepared futures has been polled: no accepted inference.
            for call in calls {
                let result = self.finish_classifier_attempt(
                    classifier::ClassifierAttempt::refused(anyhow::anyhow!(refusal.clone())),
                    &call.call_id,
                    started,
                    &cancel,
                );
                let operation = operations.remove(&call.call_id);
                self.finish_read_only_tool_call(
                    call,
                    result,
                    operation,
                    started.elapsed().as_millis() as u64,
                    tool_results,
                )
                .await;
            }
            return Ok(());
        }
        let order = calls
            .iter()
            .enumerate()
            .map(|(index, call)| (call.call_id.clone(), index))
            .collect::<HashMap<_, _>>();
        let projection_start = tool_results.len();
        let shutdown = self.shutdown_token.clone();
        let events = self.event_tx.clone();
        let mut completions =
            futures::stream::iter(calls.into_iter().zip(prepared).map(|(call, prepared)| {
                let cancel = cancel.clone();
                let shutdown = shutdown.clone();
                let events = events.clone();
                async move {
                    let attempt = prepared.run(&cancel, &shutdown, &events).await;
                    (call, attempt)
                }
            }))
            .buffer_unordered(4);
        self.set_active_tool_cancel_token(Some(cancel.clone()), true);
        while let Some((call, attempt)) = completions.next().await {
            let result = self.finish_classifier_attempt(attempt, &call.call_id, started, &cancel);
            if matches!(result.outcome, ToolOutcome::Indeterminate { .. }) {
                cancel.cancel();
            }
            let operation = operations.remove(&call.call_id);
            self.finish_read_only_tool_call(
                call,
                result,
                operation,
                started.elapsed().as_millis() as u64,
                tool_results,
            )
            .await;
        }
        tool_results[projection_start..].sort_by_key(|block| match block {
            ContentBlock::ToolResult { tool_use_id, .. } => {
                order.get(tool_use_id).copied().unwrap_or(usize::MAX)
            }
            _ => usize::MAX,
        });
        self.set_active_tool_cancel_token(None, false);
        Ok(())
    }
}
