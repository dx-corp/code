use super::*;

pub(super) fn deliver(
    state: &HeadlessState,
    request_id: &str,
    authorization: ManagedInferenceAuthorization,
    gateway_credential: Option<maestro_runtime_contracts::ManagedGatewayCredential>,
) -> anyhow::Result<()> {
    let agent = state
        .agent
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no active native agent"))?;
    agent
        .managed_authorization_coordinator()
        .respond(request_id, authorization, gateway_credential)
}
