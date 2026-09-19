//! Offline inventory of the administrator-managed disconnected profile.

use serde::Serialize;

use super::policy::{ManagedPolicyMetadata, disconnected_policy_verified};

/// Read-only projection of verified policy. This is configuration evidence, not
/// a reachability probe, tool grant, network sandbox or deployment qualification.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DisconnectedDeploymentContract {
    pub schema_version: u32,
    pub policy: ManagedPolicyMetadata,
    pub model_routes: Vec<DisconnectedModelDestination>,
    pub customer_authority: Option<DisconnectedAuthorityDestination>,
    /// Declared profile behavior, not an observation of traffic or worker state.
    pub declared_disabled_first_party_services: Vec<&'static str>,
    pub local_sessions: &'static str,
    pub enrolled_tools: &'static str,
    pub external_process_network: &'static str,
    pub customer_telemetry: &'static str,
    pub deployment_qualification: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DisconnectedModelDestination {
    pub model: String,
    pub endpoint: String,
    pub credential_required: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DisconnectedAuthorityDestination {
    pub endpoint: String,
    pub custom_ca_configured: bool,
}

/// Never reads credentials, resolves DNS, probes a server or loads repository
/// configuration. All destinations come from the same verified signed envelope.
pub fn disconnected_deployment_contract() -> Result<DisconnectedDeploymentContract, String> {
    let (profile, metadata) = disconnected_policy_verified()?
        .ok_or("No administrator-managed disconnected profile is installed")?;
    Ok(DisconnectedDeploymentContract {
        schema_version: 1,
        policy: metadata,
        model_routes: profile
            .routes
            .into_iter()
            .map(|route| DisconnectedModelDestination {
                model: route.model,
                endpoint: route.endpoint,
                credential_required: route.credential_env.is_some(),
            })
            .collect(),
        customer_authority: profile
            .authority
            .map(|authority| DisconnectedAuthorityDestination {
                endpoint: authority.endpoint,
                custom_ca_configured: authority.ca_pem.is_some(),
            }),
        declared_disabled_first_party_services: vec![
            "identity",
            "session_history_capture",
            "telemetry_outbox",
            "updates",
            "remote_catalog_refresh",
            "hosted_commands",
        ],
        local_sessions: "existing_local_store",
        enrolled_tools: "customer_authority_required",
        external_process_network: "operator_enforced",
        customer_telemetry: "separately_configured",
        deployment_qualification: "not_assessed",
    })
}
