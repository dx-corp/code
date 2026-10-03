//! Construct the headless host config without inventing gateway task ownership.
use super::*;

impl HeadlessState {
    pub(super) fn native_agent_config(&self) -> NativeAgentConfig {
        NativeAgentConfig {
            background_task_access: headless_background_access(
                std::env::var("MAESTRO_GATEWAY_BACKGROUND_SCOPE_REQUIRED")
                    .as_deref()
                    .ok(),
            ),
            model_dynamics: headless_model_dynamics(crate::config::model_dynamics_config()),
            model: self.model.clone(),
            model_capabilities: Some(self.model_capabilities),
            max_tokens: crate::model_catalog::default_max_output_tokens(&self.model),
            max_tokens_source: MaxTokensSource::Catalog,
            system_prompt: Some(self.system_prompt.clone()),
            thinking_enabled: self.thinking_enabled,
            thinking_budget: self.thinking_budget,
            cwd: self.cwd.clone(),
            // The headless protocol's own `ApprovalMode` (Auto/Fail/Prompt,
            // imported above) only resolves calls the runner already
            // marked `requires_approval`; preserve the prior (mode-unaware)
            // per-tool heuristic here exactly so that decision is unchanged.
            approval_mode: crate::state::ApprovalMode::Selective,
            context_window: None,
            // Headless has no sandbox-policy resolution today (unlike the interactive TUI's
            // `config::resolve_interactive_sandbox_policy` or print
            // mode's `PrintModeOptions::sandbox_policy`); preserve that
            // status quo explicitly rather than silently expanding this
            // PR's scope to headless sandboxing.
            sandbox_policy: None,
            managed_mcp_policy: None,
            max_turn_steps: crate::agent::DEFAULT_MAX_TURN_STEPS,
            allow_unbounded_turn: false,
            retry_config: crate::agent::retry::RetryConfig::hosted_outage(),
            external_tool_schema_policy: crate::agent::ExternalToolSchemaPolicy::Eager,
        }
    }
}

fn headless_background_access(
    required: Option<&str>,
) -> crate::tools::background_tasks::BackgroundTaskAccess {
    use crate::tools::background_tasks::BackgroundTaskAccess;
    if required == Some("1") {
        BackgroundTaskAccess::Denied
    } else {
        BackgroundTaskAccess::Legacy
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::background_tasks::BackgroundTaskAccess;

    #[test]
    fn gateway_headless_requires_authorized_background_scope() {
        assert_eq!(
            headless_background_access(Some("1")),
            BackgroundTaskAccess::Denied
        );
        assert_eq!(
            headless_background_access(None),
            BackgroundTaskAccess::Legacy
        );
        assert_eq!(
            headless_background_access(Some("0")),
            BackgroundTaskAccess::Legacy
        );
    }
}
