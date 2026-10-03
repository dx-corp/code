//! Native host configuration for the interactive CLI.
use super::*;

impl App {
    pub(super) fn native_agent_config(
        &self,
        model: &str,
        cwd: &str,
        thinking_enabled: bool,
        thinking_budget: u32,
    ) -> NativeAgentConfig {
        NativeAgentConfig {
            background_task_access:
                maestro_local_host::tools::background_tasks::BackgroundTaskAccess::Legacy,
            model_capabilities: None,
            model_dynamics: crate::config::model_dynamics_config(),
            model: model.to_owned(),
            max_tokens: crate::model_catalog::default_max_output_tokens(model),
            max_tokens_source: MaxTokensSource::Catalog,
            system_prompt: Some(self.build_system_prompt()),
            thinking_enabled,
            thinking_budget,
            cwd: cwd.to_owned(),
            approval_mode: self.state.approval_mode,
            context_window: self.state.context_window,
            // See the `sandbox_policy` field doc on `App`: without this,
            // only calls reaching the human approval modal via `self.tool_executor` were ever
            // sandboxed. Yolo mode and Selective mode's allowlisted calls
            // run through the native agent runner's own executor instead.
            sandbox_policy: self.sandbox_policy.clone(),
            managed_mcp_policy: self.managed_setup.is_managed().then(|| {
                crate::mcp::ManagedMcpPolicy {
                    version: self.managed_setup.version(),
                    policy: self.managed_setup.mcp_policy().clone(),
                }
            }),
            external_tool_schema_policy: ExternalToolSchemaPolicy::Eager,
            max_turn_steps: crate::agent::DEFAULT_MAX_TURN_STEPS,
            allow_unbounded_turn: false,
            retry_config: crate::agent::retry::RetryConfig::default(),
        }
    }
}
