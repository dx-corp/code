//! Ordered cancellation and native actor cleanup barriers.
use super::*;

impl NativeAgent {
    /// Cancel the current operation
    pub fn cancel(&self) {
        self.cancel_with_options(true);
    }

    /// Cancel all queued and active work, close the command channel, and wait
    /// for the background runner to exit.
    ///
    /// This is a lifecycle barrier:
    /// buffered work must be preempted, active tool cleanup must finish, and
    /// the runner task must return before this future completes. The external
    /// repeat-signal monitor remains the hard escape hatch if platform cleanup
    /// itself wedges.
    pub async fn shutdown(mut self) {
        self.shutdown_token.cancel();
        self.cancel_with_options(true);
        let runner_handle = self.runner_handle.take();
        drop(self.command_tx);
        if let Some(runner_handle) = runner_handle {
            let _ = runner_handle.await;
        }
    }

    /// Cancel active and queued work and return a receiver for the cleanup barrier.
    /// A dropped receiver or closed command channel never authorizes a transition.
    pub fn cancel_for_session_transition(&self) -> Result<oneshot::Receiver<()>> {
        let (reply, settled) = oneshot::channel();
        self.command_tx
            .send(AgentCommand::Cancel {
                clear_pending: true,
            })
            .map_err(|error| anyhow::anyhow!("Failed to cancel session work: {error}"))?;
        self.command_tx
            .send(AgentCommand::AwaitIdle { reply })
            .map_err(|error| anyhow::anyhow!("Failed to await session cleanup: {error}"))?;
        cancel_active_operation(&self.active_cancellation);
        Ok(settled)
    }

    /// Cancel the current operation but keep any queued prompts.
    pub fn cancel_keep_queue(&self) {
        self.cancel_with_options(false);
    }

    pub fn cancel_queued(&self, id: u64) {
        let _ = self.command_tx.send(AgentCommand::CancelQueued { id });
    }

    pub fn reorder_queued(&self, id: u64, placement: QueuePlacement) {
        let _ = self
            .command_tx
            .send(AgentCommand::ReorderQueued { id, placement });
    }

    fn cancel_with_options(&self, clear_pending: bool) {
        // Preserve channel order before synchronously waking the runner. The
        // runner can then drain every prompt queued before this cancellation.
        let _ = self.command_tx.send(AgentCommand::Cancel { clear_pending });
        cancel_active_operation(&self.active_cancellation);
    }
}
