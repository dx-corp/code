//! Capability negotiation for attached and transport-only hosted executors.

use super::*;

impl AgentSupervisorHostedRunnerMessageExecutor {
    pub(super) fn negotiated_server_capabilities(&self) -> Option<ServerCapabilities> {
        let Ok(supervisor) = self.supervisor.lock() else {
            return Some(native_server_capabilities_without_governed_grants());
        };
        supervisor.state().server_capabilities.clone().or_else(|| {
            (supervisor.transport_generation() == 0)
                .then(native_server_capabilities_without_governed_grants)
        })
    }

    pub(super) fn hosted_hello_ok_for_context(
        &self,
        context: &HostedRunnerHeadlessMessageContext,
    ) -> FromAgentMessage {
        FromAgentMessage::HelloOk {
            protocol_version: HEADLESS_PROTOCOL_VERSION.to_string(),
            controller_binding_version: None,
            controller_binding_sha256: None,
            connection_id: Some(context.connection_id.clone()),
            client_protocol_version: context.client_protocol_version.clone(),
            client_info: context.client_info.clone(),
            capabilities: context.capabilities.clone(),
            server_capabilities: self.negotiated_server_capabilities(),
            opt_out_notifications: context.opt_out_notifications.clone(),
            role: Some(context.role),
            controller_connection_id: context.controller_connection_id.clone(),
            lease_expires_at: Some(context.lease_expires_at.clone()),
        }
    }
}

pub(super) fn server_capabilities_for_executor(
    message_executor: &dyn HostedRunnerHeadlessMessageExecutor,
) -> Option<ServerCapabilities> {
    match message_executor.state() {
        Ok(Some(agent_state)) => agent_state.server_capabilities,
        Ok(None) | Err(_) => Some(native_server_capabilities_without_governed_grants()),
    }
}

pub(super) fn native_server_capabilities_without_governed_grants() -> ServerCapabilities {
    server_capabilities_without_governed_grants(native_server_capabilities(
        &maestro_local_host::agent::tool_host::BuiltInToolHost,
    ))
}

pub(super) fn server_capabilities_without_governed_grants(
    mut capabilities: ServerCapabilities,
) -> ServerCapabilities {
    capabilities.governed_tool_grant_algorithms.clear();
    capabilities
}

pub(super) fn governed_grant_algorithms_for_executor(
    message_executor: &dyn HostedRunnerHeadlessMessageExecutor,
) -> Vec<String> {
    server_capabilities_for_executor(message_executor)
        .map(|capabilities| capabilities.governed_tool_grant_algorithms)
        .unwrap_or_default()
}
