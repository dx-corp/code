use super::*;

impl App {
    /// Resolve the signed-in account's live default before switching routes.
    /// The app-server keeps ChatGPT auth; no subscription token enters Maestro.
    pub(in crate::app) async fn switch_to_codex_subscription(&mut self, persist_default: bool) {
        if self.state.busy || self.pending_model_change.is_some() || self.pending_agent_spawn {
            self.state.status = Some(
                self.state
                    .locale
                    .translate("Wait for the current turn or model switch to finish.")
                    .into(),
            );
            return;
        }
        match maestro_local_host::codex_cli::preferred_chatgpt_model().await {
            Ok(model) => self.switch_model(&model, persist_default),
            Err(error) => {
                self.state.error = Some(self.state.locale.format(
                    "Could not use ChatGPT subscription: {0}",
                    &[(error).to_string()],
                ));
            }
        }
    }
}
