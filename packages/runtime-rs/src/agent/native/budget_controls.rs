//! Host controls for cumulative output and Process budgets.
use super::*;

impl NativeAgent {
    /// Retire Process authority at an inactive, verified grant boundary.
    /// Acknowledgement covers both budget removal and the ordinary system prompt.
    pub async fn clear_process_budget(&self, system_prompt: String) -> Result<()> {
        let (applied, receiver) = oneshot::channel();
        self.command_tx
            .send(AgentCommand::ClearProcessBudget {
                system_prompt,
                applied,
            })
            .map_err(|_| anyhow::anyhow!("process budget runner unavailable"))?;
        receiver
            .await
            .context("process budget retirement was not acknowledged")?
    }

    /// Cap cumulative output tokens before the prompt they apply to.
    pub fn set_output_token_budget(&self, max_total_output_tokens: u32) -> Result<()> {
        self.command_tx
            .send(AgentCommand::SetOutputTokenBudget {
                max_total_output_tokens,
            })
            .map_err(|e| anyhow::anyhow!("Failed to set output token budget: {e}"))?;
        Ok(())
    }
}
