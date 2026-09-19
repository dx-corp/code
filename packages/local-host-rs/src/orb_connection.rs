//! Validation and binding helpers for externally managed hosted Orb MCP
//! connections.
//!
//! Maestro does not own the Orb catalog or credential broker. It persists only
//! an opaque connection/credential reference and the least-privilege binding
//! metadata supplied by that authority. The generated MCP configuration never
//! contains a bearer token or a raw authentication header.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::mcp::{McpConfigScope, McpServerConfig, McpTransport};
use crate::plugins::{ConnectionMcpBindingDefinition, ConnectionTypeDefinition};
use crate::service_connections::{
    ConnectionAuthKind, ConnectionMcpBinding, ConnectionSecretRef, ConnectionState,
    ConnectionStore, ServiceConnection, validate_opaque_reference,
};

pub const HOSTED_ORB_PROVIDER_ID: &str = "orb";
pub const HOSTED_ORB_MCP_SERVER_NAME: &str = "orb";
pub const HOSTED_ORB_MCP_ENDPOINT: &str = "https://orb.evalops.dev/mcp";
pub const HOSTED_ORB_PROVENANCE_AUTHORITY: &str = "https://orb.evalops.dev";
/// Optional explicit selector for an externally managed MCP connection. This
/// is intentionally separate from MAESTRO_CONNECTION, which selects a direct
/// model credential.
pub const MANAGED_MCP_CONNECTION_ENV: &str = "MAESTRO_MCP_CONNECTION";
/// Scopes advertised by the hosted Orb MCP protected-resource contract. A
/// connection may request a strict subset appropriate to its tool set.
pub const HOSTED_ORB_SUPPORTED_SCOPES: &[&str] = &[
    "orb:approvals:write",
    "orb:executor:write",
    "orb:tasks:read",
    "orb:tasks:write",
    "orb:threads:read",
    "orb:threads:write",
];

/// The non-secret identity of the managed hosted Orb connection currently
/// attached to Maestro. This is deliberately separate from the credential
/// reference: it binds a durable local task to the owner context without ever
/// materializing authentication material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostedOrbOwnerBinding {
    pub(crate) organization_id: String,
    pub(crate) workspace_id: String,
    pub(crate) connection_ref: String,
    pub(crate) managed_generation: u64,
}

const ORB_OWNER_ORGANIZATION_ENV_VARS: &[&str] = &[
    "MAESTRO_AGENT_OPERATING_PLANE_ORG_ID",
    "MAESTRO_EVALOPS_ORG_ID",
    "EVALOPS_ORGANIZATION_ID",
    "MAESTRO_ORGANIZATION_ID",
];
const ORB_OWNER_WORKSPACE_ENV_VARS: &[&str] = &[
    "MAESTRO_AGENT_OPERATING_PLANE_WORKSPACE_ID",
    "MAESTRO_EVALOPS_WORKSPACE_ID",
    "MAESTRO_REMOTE_RUNNER_WORKSPACE_ID",
    "EVALOPS_WORKSPACE_ID",
    "MAESTRO_WORKSPACE_ID",
];

fn first_non_empty_env<F>(names: &[&str], read_env: F) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    names.iter().find_map(|name| {
        read_env(name)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    })
}

/// Resolve the current managed connection's owner binding without reading any
/// credential value. A caller must have an explicit organization, workspace,
/// connection reference, and generation before it can operate on a durable
/// hosted Computer task; silently borrowing a retry-time default would rebind an old
/// task to a different remote owner.
pub(crate) fn hosted_orb_owner_binding(config: &McpServerConfig) -> Result<HostedOrbOwnerBinding> {
    hosted_orb_owner_binding_with_env(config, |name| std::env::var(name).ok())
}

fn hosted_orb_owner_binding_with_env<F>(
    config: &McpServerConfig,
    read_env: F,
) -> Result<HostedOrbOwnerBinding>
where
    F: Fn(&str) -> Option<String>,
{
    if config.scope != McpConfigScope::Managed
        || config.name != HOSTED_ORB_MCP_SERVER_NAME
        || config.transport != McpTransport::Http
    {
        bail!("hosted Computer owner binding requires the active managed HTTP connection");
    }
    config
        .validate()
        .map_err(anyhow::Error::msg)
        .context("invalid hosted Computer MCP runtime binding")?;
    let endpoint = config
        .url
        .as_deref()
        .context("hosted Computer owner binding requires an endpoint")?;
    validate_hosted_orb_endpoint(endpoint)?;
    let organization_id = first_non_empty_env(ORB_OWNER_ORGANIZATION_ENV_VARS, &read_env)
        .context("hosted Computer owner binding requires an organization id")?;
    let workspace_id = first_non_empty_env(ORB_OWNER_WORKSPACE_ENV_VARS, &read_env)
        .context("hosted Computer owner binding requires a workspace id")?;
    let connection_ref = config
        .connection_ref
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .context("hosted Computer owner binding requires connection_ref")?
        .to_owned();
    let managed_generation = config
        .managed_generation
        .filter(|generation| *generation > 0)
        .context("hosted Computer owner binding requires managed connection generation")?;
    Ok(HostedOrbOwnerBinding {
        organization_id,
        workspace_id,
        connection_ref,
        managed_generation,
    })
}

/// Validate the endpoint before a hosted Orb binding can be used.
pub fn validate_hosted_orb_endpoint(endpoint: &str) -> Result<()> {
    if endpoint != HOSTED_ORB_MCP_ENDPOINT {
        bail!(
            "hosted Computer MCP endpoint must be the canonical managed service {HOSTED_ORB_MCP_ENDPOINT}"
        );
    }
    let parsed = url::Url::parse(endpoint).context("invalid hosted Computer MCP endpoint")?;
    if parsed.scheme() != "https"
        || parsed.host_str() != Some("orb.evalops.dev")
        || parsed.path() != "/mcp"
        || parsed.username() != ""
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        bail!(
            "hosted Computer MCP endpoint must be HTTPS orb.evalops.dev/mcp without URL credentials"
        );
    }
    Ok(())
}

/// Validate a declarative type supplied by the external catalog or a trusted
/// plugin. This checks the hosted Orb contract without registering a Maestro
/// built-in connection type or exposing an onboarding dashboard.
pub fn validate_hosted_orb_definition(definition: &ConnectionTypeDefinition) -> Result<()> {
    definition.validate()?;
    if definition.provider_id != HOSTED_ORB_PROVIDER_ID
        || !matches!(
            definition.auth_kind,
            ConnectionAuthKind::OAuth
                | ConnectionAuthKind::Subscription
                | ConnectionAuthKind::WorkloadIdentity
        )
        || definition.env_var.is_some()
    {
        bail!("hosted Computer definitions require delegated authentication");
    }
    let binding = definition
        .mcp_binding
        .as_ref()
        .context("hosted Computer definitions require an MCP binding")?;
    validate_hosted_orb_binding_definition(binding, &definition.capabilities)
}

fn validate_hosted_orb_binding_definition(
    binding: &ConnectionMcpBindingDefinition,
    capabilities: &[String],
) -> Result<()> {
    validate_hosted_orb_endpoint(&binding.endpoint)?;
    validate_hosted_orb_scopes(&binding.scopes, capabilities)?;
    if binding.server_name != HOSTED_ORB_MCP_SERVER_NAME {
        bail!("hosted Computer bindings must use the canonical MCP server name");
    }
    Ok(())
}

/// Validate an externally managed connection record for hosted Orb use.
pub fn validate_hosted_orb_connection(connection: &ServiceConnection) -> Result<()> {
    connection.validate()?;
    if connection.provider_id != HOSTED_ORB_PROVIDER_ID
        || !matches!(
            connection.auth_kind,
            ConnectionAuthKind::OAuth
                | ConnectionAuthKind::Subscription
                | ConnectionAuthKind::WorkloadIdentity
        )
    {
        bail!("hosted Computer connections require delegated authentication");
    }
    if !matches!(
        &connection.secret_ref,
        ConnectionSecretRef::Delegated { provider, .. } if provider == HOSTED_ORB_PROVIDER_ID
    ) {
        bail!(
            "hosted Computer connections require a Computer-owned delegated credential reference"
        );
    }
    let binding = connection
        .mcp_binding
        .as_ref()
        .context("hosted Computer connections require an externally managed MCP binding")?;
    validate_hosted_orb_binding(binding, &connection.capabilities)
}

fn validate_hosted_orb_binding(
    binding: &ConnectionMcpBinding,
    capabilities: &[String],
) -> Result<()> {
    binding.validate()?;
    validate_hosted_orb_endpoint(&binding.endpoint)?;
    validate_hosted_orb_scopes(&binding.scopes, capabilities)?;
    validate_hosted_orb_reference(
        "hosted Computer credential_ref",
        &binding.credential_ref,
        "credential",
    )?;
    validate_hosted_orb_reference(
        "hosted Computer provenance reference",
        &binding.provenance.reference,
        "connection",
    )?;
    if binding.server_name != HOSTED_ORB_MCP_SERVER_NAME {
        bail!("hosted Computer bindings must use the canonical MCP server name");
    }
    if binding.provenance.authority != HOSTED_ORB_PROVENANCE_AUTHORITY {
        bail!("hosted Computer binding provenance must name the managed Computer authority");
    }
    Ok(())
}

fn validate_hosted_orb_reference(label: &str, reference: &str, kind: &str) -> Result<()> {
    validate_opaque_reference(label, reference)?;
    let expected_prefix = format!("ref:{HOSTED_ORB_PROVIDER_ID}/{kind}/");
    if !reference.starts_with(&expected_prefix) {
        bail!("{label} must use the Computer-owned reference namespace");
    }
    Ok(())
}

/// Validate a managed binding and build the public Maestro MCP configuration.
/// Authentication remains an opaque broker reference; no token is materialized
/// into `headers` or persisted in the MCP config.
pub fn mcp_server_config_for_connection(connection: &ServiceConnection) -> Result<McpServerConfig> {
    validate_managed_mcp_connection(connection)?;
    let binding = connection
        .mcp_binding
        .as_ref()
        .context("managed MCP connection is missing its binding")?;
    Ok(McpServerConfig {
        name: binding.server_name.clone(),
        transport: McpTransport::Http,
        command: None,
        args: Vec::new(),
        env: HashMap::new(),
        cwd: None,
        url: Some(binding.endpoint.clone()),
        headers: HashMap::new(),
        headers_helper: Some(format!(
            "externally managed by {}",
            binding.provenance.authority
        )),
        auth_preset: binding.auth_preset.clone(),
        connection_ref: Some(connection.id.clone()),
        credential_ref: Some(binding.credential_ref.clone()),
        managed_generation: Some(connection.generation),
        supports_parallel_tool_calls: None,
        requires_project_approval: Some(false),
        disabled_tools: Vec::new(),
        timeout: None,
        enabled: true,
        disabled: false,
        scope: McpConfigScope::Managed,
    })
}

pub fn validate_managed_mcp_connection(connection: &ServiceConnection) -> Result<()> {
    if connection.state != ConnectionState::Active {
        bail!("connection {} is revoked", connection.id);
    }
    let binding = connection
        .mcp_binding
        .as_ref()
        .context("managed MCP connection is missing its binding")?;
    binding.validate()?;
    if connection.provider_id != HOSTED_ORB_PROVIDER_ID
        && binding.server_name == HOSTED_ORB_MCP_SERVER_NAME
    {
        bail!("the orb MCP server name is reserved for the hosted Computer provider");
    }
    if connection.provider_id == HOSTED_ORB_PROVIDER_ID {
        validate_hosted_orb_connection(connection)
    } else {
        Ok(())
    }
}

/// Revalidate the persisted hosted Orb binding immediately before a network
/// request. The MCP config is a runtime snapshot, so a revoked/rotated
/// connection must not keep using the old endpoint or credential reference.
pub fn validate_hosted_orb_runtime_binding(config: &McpServerConfig) -> Result<()> {
    crate::safety::require_vendor_network()?;
    if config.scope != McpConfigScope::Managed
        || config.name != HOSTED_ORB_MCP_SERVER_NAME
        || config.transport != McpTransport::Http
    {
        bail!("hosted Computer runtime binding must use the managed HTTP server")
    }
    if !config.headers.is_empty() {
        bail!("hosted Computer runtime binding must not carry persisted HTTP headers")
    }
    let connection_id = config
        .connection_ref
        .as_deref()
        .context("hosted Computer runtime binding is missing connection_ref")?;
    let credential_ref = config
        .credential_ref
        .as_deref()
        .context("hosted Computer runtime binding is missing credential_ref")?;
    let generation = config
        .managed_generation
        .filter(|generation| *generation > 0)
        .context("hosted Computer runtime binding is missing its generation")?;
    let endpoint = config
        .url
        .as_deref()
        .context("hosted Computer runtime binding is missing its endpoint")?;
    validate_hosted_orb_endpoint(endpoint)?;

    let path = ConnectionStore::default_path()?;
    let store = ConnectionStore::load(&path)?;
    let connection = store
        .get(connection_id)
        .with_context(|| format!("managed connection {connection_id} was not found"))?;
    if connection.state != ConnectionState::Active {
        bail!("managed connection {connection_id} is revoked")
    }
    if connection.generation != generation {
        bail!(
            "managed connection {connection_id} generation changed from {generation} to {}",
            connection.generation
        )
    }
    let explicit = std::env::var(MANAGED_MCP_CONNECTION_ENV)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let selected = selected_managed_mcp_connections(&store, explicit.as_deref())
        .into_iter()
        .find(|candidate| candidate.provider_id == HOSTED_ORB_PROVIDER_ID);
    if selected.is_none_or(|candidate| candidate.id != connection_id) {
        bail!(
            "managed Computer connection {connection_id} is no longer the selected active connection"
        )
    }
    let binding = connection
        .mcp_binding
        .as_ref()
        .context("managed connection is missing its MCP binding")?;
    if binding.endpoint != endpoint {
        bail!("managed connection endpoint no longer matches the MCP runtime binding")
    }
    if binding.credential_ref != credential_ref {
        bail!("managed connection credential_ref no longer matches the MCP runtime binding")
    }
    validate_hosted_orb_connection(connection)
}

/// Report local metadata health honestly. Maestro does not probe the remote
/// service or impersonate the external authority's credential broker here.
pub fn managed_mcp_health_detail(connection: &ServiceConnection) -> Result<String> {
    validate_managed_mcp_connection(connection)?;
    let authority = connection
        .mcp_binding
        .as_ref()
        .context("managed MCP connection is missing its binding")?
        .provenance
        .authority
        .clone();
    Ok(format!(
        "externally managed by {authority}; Deixic Code validated metadata only and did not probe remote authentication or reachability"
    ))
}

/// Load the selected active managed connections as ephemeral MCP configs.
/// The external catalog/broker remains the source of truth; this function only
/// translates persisted, validated metadata into the public Maestro config.
pub fn managed_mcp_servers() -> Vec<McpServerConfig> {
    let Ok(path) = ConnectionStore::default_path() else {
        return Vec::new();
    };
    let Ok(store) = ConnectionStore::load(&path) else {
        return Vec::new();
    };
    let explicit = std::env::var(MANAGED_MCP_CONNECTION_ENV)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let selected = selected_managed_mcp_connections(&store, explicit.as_deref());

    let mut servers = BTreeMap::new();
    for connection in selected {
        let Ok(server) = mcp_server_config_for_connection(connection) else {
            continue;
        };
        // A server name is the public MCP identity. Keep the first selected
        // authority rather than allowing a later record to redirect it.
        servers.entry(server.name.clone()).or_insert(server);
    }
    servers.into_values().collect()
}

/// Return the MCP server names owned by persisted managed connections,
/// including revoked records. A revoked authority must reserve its server
/// identity so file-backed configuration cannot take over while the managed
/// binding is unavailable.
pub fn managed_mcp_server_reservations() -> BTreeSet<String> {
    let Ok(path) = ConnectionStore::default_path() else {
        return hosted_orb_server_reservation();
    };
    managed_mcp_server_reservations_from_path(&path)
}

fn managed_mcp_server_reservations_from_path(path: &Path) -> BTreeSet<String> {
    match fs::symlink_metadata(path) {
        Ok(_) => match fs::metadata(path) {
            Ok(_) => match ConnectionStore::load(path) {
                Ok(store) => managed_mcp_server_reservations_for_store(&store),
                Err(_) => hosted_orb_server_reservation(),
            },
            // An existing path that cannot be followed/read (including a
            // dangling symlink) must not be treated as an empty store.
            Err(_) => hosted_orb_server_reservation(),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            hosted_orb_server_reservation()
        }
        Err(_) => hosted_orb_server_reservation(),
    }
}

fn hosted_orb_server_reservation() -> BTreeSet<String> {
    BTreeSet::from([HOSTED_ORB_MCP_SERVER_NAME.to_owned()])
}

fn managed_mcp_server_reservations_for_store(store: &ConnectionStore) -> BTreeSet<String> {
    store
        .connections
        .iter()
        .filter_map(|connection| {
            if connection.provider_id == HOSTED_ORB_PROVIDER_ID {
                return Some(HOSTED_ORB_MCP_SERVER_NAME.to_owned());
            }
            connection.mcp_binding.as_ref().and_then(|binding| {
                (!binding.server_name.is_empty()
                    && binding.server_name.chars().all(|character| {
                        character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
                    }))
                .then(|| binding.server_name.clone())
            })
        })
        .collect()
}

fn selected_managed_mcp_connections<'a>(
    store: &'a ConnectionStore,
    explicit: Option<&str>,
) -> Vec<&'a ServiceConnection> {
    if let Some(id) = explicit {
        return store
            .get(id)
            .filter(|connection| {
                connection.state == ConnectionState::Active && connection.mcp_binding.is_some()
            })
            .into_iter()
            .collect();
    }

    let providers = store
        .connections
        .iter()
        .filter(|connection| {
            connection.state == ConnectionState::Active && connection.mcp_binding.is_some()
        })
        .map(|connection| connection.provider_id.clone())
        .collect::<BTreeSet<_>>();
    providers
        .iter()
        .filter_map(|provider| {
            store
                .connections
                .iter()
                .filter(|connection| {
                    connection.provider_id == *provider
                        && connection.state == ConnectionState::Active
                        && connection.mcp_binding.is_some()
                })
                .find(|connection| connection.is_default)
                .or_else(|| {
                    store
                        .connections
                        .iter()
                        .filter(|connection| {
                            connection.provider_id == *provider
                                && connection.state == ConnectionState::Active
                                && connection.mcp_binding.is_some()
                        })
                        .max_by_key(|connection| connection.updated_at_ms)
                })
        })
        .collect()
}

fn validate_hosted_orb_scopes(scopes: &[String], capabilities: &[String]) -> Result<()> {
    if scopes.is_empty()
        || scopes
            .iter()
            .any(|scope| !HOSTED_ORB_SUPPORTED_SCOPES.contains(&scope.as_str()))
        || scopes
            .iter()
            .any(|scope| !capabilities.iter().any(|capability| capability == scope))
    {
        bail!("hosted Computer bindings must use supported least-privilege scopes");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::McpTransport;
    use crate::plugins::ConnectionMcpBindingDefinition;
    use crate::service_connections::{
        ConnectionAuthKind, ConnectionMcpProvenance, ConnectionPlacement, ConnectionSecretRef,
        ConnectionState, ConnectionStore, ServiceConnection,
    };

    struct EnvRestore(Vec<(&'static str, Option<std::ffi::OsString>)>);

    impl EnvRestore {
        fn capture(names: &[&'static str]) -> Self {
            Self(
                names
                    .iter()
                    .copied()
                    .map(|name| (name, std::env::var_os(name)))
                    .collect(),
            )
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (name, value) in self.0.drain(..) {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    fn binding() -> ConnectionMcpBinding {
        ConnectionMcpBinding {
            server_name: HOSTED_ORB_MCP_SERVER_NAME.into(),
            endpoint: HOSTED_ORB_MCP_ENDPOINT.into(),
            scopes: vec!["orb:threads:read".into(), "orb:threads:write".into()],
            auth_preset: None,
            credential_ref: "ref:orb/credential/00000000-0000-4000-8000-000000000001".into(),
            provenance: ConnectionMcpProvenance {
                authority: HOSTED_ORB_PROVENANCE_AUTHORITY.into(),
                reference: "ref:orb/connection/00000000-0000-4000-8000-000000000002".into(),
            },
        }
    }

    fn connection() -> ServiceConnection {
        ServiceConnection {
            id: "orb-team".into(),
            type_id: "orb-remote-mcp".into(),
            provider_id: HOSTED_ORB_PROVIDER_ID.into(),
            label: "Hosted Computer".into(),
            auth_kind: ConnectionAuthKind::OAuth,
            env_var: None,
            secret_ref: ConnectionSecretRef::Delegated {
                provider: HOSTED_ORB_PROVIDER_ID.into(),
                profile: Some("managed".into()),
            },
            placement: ConnectionPlacement::Either,
            state: ConnectionState::Active,
            capabilities: vec!["orb:threads:read".into(), "orb:threads:write".into()],
            mcp_binding: Some(binding()),
            generation: 1,
            is_default: true,
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    fn definition() -> ConnectionTypeDefinition {
        ConnectionTypeDefinition {
            id: "orb-remote-mcp".into(),
            display_name: "Hosted Computer MCP".into(),
            provider_id: HOSTED_ORB_PROVIDER_ID.into(),
            auth_kind: ConnectionAuthKind::OAuth,
            placement: ConnectionPlacement::Either,
            env_var: None,
            capabilities: vec!["orb:threads:read".into(), "orb:threads:write".into()],
            documentation_url: None,
            mcp_binding: Some(ConnectionMcpBindingDefinition {
                server_name: HOSTED_ORB_MCP_SERVER_NAME.into(),
                endpoint: HOSTED_ORB_MCP_ENDPOINT.into(),
                scopes: vec!["orb:threads:read".into(), "orb:threads:write".into()],
                auth_preset: None,
            }),
        }
    }

    fn direct_model_connection() -> ServiceConnection {
        let mut model = connection();
        model.id = "openai-work".into();
        model.type_id = "openai-oauth".into();
        model.provider_id = "openai".into();
        model.label = "OpenAI work".into();
        model.auth_kind = ConnectionAuthKind::OAuth;
        model.secret_ref = ConnectionSecretRef::Delegated {
            provider: "openai".into(),
            profile: Some("work".into()),
        };
        model.capabilities = vec!["models.invoke".into()];
        model.mcp_binding = None;
        model.is_default = true;
        model
    }

    #[test]
    fn externally_catalogued_hosted_orb_type_validates_without_being_builtin() {
        let definition = definition();
        validate_hosted_orb_definition(&definition).unwrap();
        assert_eq!(definition.id, "orb-remote-mcp");
    }

    #[test]
    fn managed_binding_generates_http_config_with_opaque_auth_reference() {
        let config = mcp_server_config_for_connection(&connection()).unwrap();
        assert_eq!(config.name, HOSTED_ORB_MCP_SERVER_NAME);
        assert_eq!(config.transport, McpTransport::Http);
        assert_eq!(config.url.as_deref(), Some(HOSTED_ORB_MCP_ENDPOINT));
        assert!(config.headers.is_empty());
        assert_eq!(config.connection_ref.as_deref(), Some("orb-team"));
        assert_eq!(
            config.credential_ref.as_deref(),
            Some("ref:orb/credential/00000000-0000-4000-8000-000000000001")
        );
        assert_eq!(config.scope, McpConfigScope::Managed);
        let encoded = serde_json::to_string(&config).unwrap();
        assert!(encoded.contains("\"headers\":{}"));
        assert!(!encoded.contains("managedGeneration"));
    }

    #[test]
    fn owner_binding_fails_closed_without_ambient_tenant_identity() {
        let config = mcp_server_config_for_connection(&connection()).unwrap();
        let error = hosted_orb_owner_binding_with_env(&config, |_| None).unwrap_err();
        assert!(
            error.to_string().contains("organization id"),
            "unexpected owner binding error: {error:#}"
        );
    }

    #[test]
    fn persisted_metadata_redacts_literal_secrets_and_keeps_only_references() {
        let encoded = serde_json::to_string(&connection()).unwrap();
        assert!(encoded.contains("ref:orb/credential/00000000-0000-4000-8000-000000000001"));
        assert!(!encoded.contains("Authorization"));
        assert!(!encoded.contains("Bearer"));
        assert!(!encoded.contains("token-1"));
    }

    #[test]
    fn revoked_managed_connections_cannot_generate_a_binding() {
        let mut revoked = connection();
        revoked.state = ConnectionState::Revoked;
        let error = mcp_server_config_for_connection(&revoked).unwrap_err();
        assert!(error.to_string().contains("revoked"));
    }

    #[test]
    fn revoked_orb_connections_reserve_the_canonical_server_name() {
        let mut revoked = connection();
        revoked.state = ConnectionState::Revoked;
        let store = ConnectionStore {
            schema_version: 1,
            connections: vec![revoked],
        };

        assert!(
            managed_mcp_server_reservations_for_store(&store).contains(HOSTED_ORB_MCP_SERVER_NAME)
        );
    }

    #[test]
    fn invalid_connection_store_reserves_the_canonical_server_name() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("connections.json");
        std::fs::write(
            &path,
            r#"{"schemaVersion":1,"connections":[{"providerId":"orb"}]}"#,
        )
        .unwrap();

        assert!(
            managed_mcp_server_reservations_from_path(&path).contains(HOSTED_ORB_MCP_SERVER_NAME)
        );
    }

    #[test]
    fn missing_connection_store_reserves_the_canonical_server_name() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("connections.json");

        assert!(
            managed_mcp_server_reservations_from_path(&path).contains(HOSTED_ORB_MCP_SERVER_NAME)
        );
    }

    #[test]
    fn runtime_binding_revalidates_store_identity_on_every_request() {
        let _guard = crate::config::test_process_env_lock();
        let _restore = EnvRestore::capture(&["MAESTRO_HOME", MANAGED_MCP_CONNECTION_ENV]);
        let temp = tempfile::tempdir().unwrap();
        std::env::set_var("MAESTRO_HOME", temp.path());
        std::env::remove_var(MANAGED_MCP_CONNECTION_ENV);

        let original = connection();
        let config = mcp_server_config_for_connection(&original).unwrap();
        let path = ConnectionStore::default_path().unwrap();
        ConnectionStore {
            schema_version: 1,
            connections: vec![original.clone()],
        }
        .save(&path)
        .unwrap();
        validate_hosted_orb_runtime_binding(&config).unwrap();

        let mut rotated = original.clone();
        rotated.generation = 2;
        ConnectionStore {
            schema_version: 1,
            connections: vec![rotated.clone()],
        }
        .save(&path)
        .unwrap();
        let error = validate_hosted_orb_runtime_binding(&config).unwrap_err();
        assert!(
            format!("{error:#}").contains("generation changed"),
            "{error:#}"
        );

        let mut tampered_config = config.clone();
        tampered_config.url = Some("https://orb.evalops.dev/other".to_owned());
        ConnectionStore {
            schema_version: 1,
            connections: vec![original.clone()],
        }
        .save(&path)
        .unwrap();
        let error = validate_hosted_orb_runtime_binding(&tampered_config).unwrap_err();
        assert!(
            format!("{error:#}").contains("canonical managed service"),
            "{error:#}"
        );

        let mut credential_changed = original.clone();
        credential_changed
            .mcp_binding
            .as_mut()
            .expect("binding")
            .credential_ref = "ref:orb/credential/00000000-0000-4000-8000-000000000003".into();
        ConnectionStore {
            schema_version: 1,
            connections: vec![credential_changed],
        }
        .save(&path)
        .unwrap();
        let error = validate_hosted_orb_runtime_binding(&config).unwrap_err();
        assert!(format!("{error:#}").contains("credential_ref"), "{error:#}");

        let mut revoked = original;
        revoked.state = ConnectionState::Revoked;
        ConnectionStore {
            schema_version: 1,
            connections: vec![revoked],
        }
        .save(&path)
        .unwrap();
        let error = validate_hosted_orb_runtime_binding(&config).unwrap_err();
        assert!(format!("{error:#}").contains("revoked"), "{error:#}");
    }

    #[test]
    fn runtime_binding_rejects_a_connection_after_default_switch() {
        let _guard = crate::config::test_process_env_lock();
        let _restore = EnvRestore::capture(&["MAESTRO_HOME", MANAGED_MCP_CONNECTION_ENV]);
        let temp = tempfile::tempdir().unwrap();
        std::env::set_var("MAESTRO_HOME", temp.path());
        std::env::remove_var(MANAGED_MCP_CONNECTION_ENV);

        let original = connection();
        let config = mcp_server_config_for_connection(&original).unwrap();
        let mut replacement = original.clone();
        replacement.id = "orb-personal".into();
        replacement.is_default = false;
        replacement.generation = 2;
        replacement.updated_at_ms = 2;
        let replacement_binding = replacement.mcp_binding.as_mut().expect("binding");
        replacement_binding.credential_ref =
            "ref:orb/credential/00000000-0000-4000-8000-000000000003".into();
        replacement_binding.provenance.reference =
            "ref:orb/connection/00000000-0000-4000-8000-000000000004".into();

        let path = ConnectionStore::default_path().unwrap();
        let mut store = ConnectionStore {
            schema_version: 1,
            connections: vec![original, replacement],
        };
        store.save(&path).unwrap();
        validate_hosted_orb_runtime_binding(&config).unwrap();

        store.set_default("orb-personal").unwrap();
        store.save(&path).unwrap();
        let error = validate_hosted_orb_runtime_binding(&config).unwrap_err();
        assert!(
            format!("{error:#}").contains("no longer the selected active connection"),
            "{error:#}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn dangling_connection_store_symlink_reserves_the_canonical_server_name() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("connections.json");
        symlink(temp.path().join("missing.json"), &path).unwrap();

        assert!(
            managed_mcp_server_reservations_from_path(&path).contains(HOSTED_ORB_MCP_SERVER_NAME)
        );
    }

    #[test]
    fn direct_model_selection_does_not_disable_managed_mcp_defaults() {
        let store = ConnectionStore {
            schema_version: 1,
            connections: vec![connection(), direct_model_connection()],
        };

        let selected = selected_managed_mcp_connections(&store, None);
        assert_eq!(
            selected
                .iter()
                .map(|connection| connection.id.as_str())
                .collect::<Vec<_>>(),
            vec!["orb-team"]
        );
    }

    #[test]
    fn generic_managed_connections_cannot_claim_the_hosted_orb_server_name() {
        let mut generic = connection();
        generic.id = "aaa-remote".into();
        generic.type_id = "aaa-remote-mcp".into();
        generic.provider_id = "aaa".into();
        generic.label = "AAA remote MCP".into();
        generic.secret_ref = ConnectionSecretRef::Delegated {
            provider: "aaa".into(),
            profile: None,
        };
        generic.capabilities = vec!["tools.read".into()];
        generic.mcp_binding = Some(ConnectionMcpBinding {
            server_name: HOSTED_ORB_MCP_SERVER_NAME.into(),
            endpoint: "https://aaa.example.test/mcp".into(),
            scopes: vec!["tools.read".into()],
            auth_preset: None,
            credential_ref: "ref:aaa/credential/00000000-0000-4000-8000-000000000003".into(),
            provenance: ConnectionMcpProvenance {
                authority: "https://aaa.example.test".into(),
                reference: "ref:aaa/connection/00000000-0000-4000-8000-000000000004".into(),
            },
        });

        let error = mcp_server_config_for_connection(&generic).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("reserved for the hosted Computer provider")
        );

        let store = ConnectionStore {
            schema_version: 1,
            connections: vec![generic, connection()],
        };
        let configs = selected_managed_mcp_connections(&store, None)
            .into_iter()
            .filter_map(|connection| mcp_server_config_for_connection(connection).ok())
            .collect::<Vec<_>>();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].url.as_deref(), Some(HOSTED_ORB_MCP_ENDPOINT));
    }

    #[test]
    fn hosted_orb_endpoint_is_fixed_to_the_managed_service() {
        assert!(validate_hosted_orb_endpoint("http://127.0.0.1:8080/mcp").is_err());
        assert!(validate_hosted_orb_endpoint("https://orb.example.test/mcp").is_err());

        let mut tampered = connection();
        tampered.mcp_binding.as_mut().expect("binding").endpoint =
            "https://orb.example.test/mcp".into();
        assert!(validate_hosted_orb_connection(&tampered).is_err());
    }

    #[test]
    fn hosted_orb_scope_and_provenance_validation_is_least_privilege() {
        let mut missing_scope = connection();
        missing_scope.capabilities = vec![
            "orb:admin".into(),
            "orb:threads:read".into(),
            "orb:threads:write".into(),
        ];
        missing_scope.mcp_binding.as_mut().expect("binding").scopes = vec!["orb:admin".into()];
        assert!(
            validate_hosted_orb_connection(&missing_scope)
                .unwrap_err()
                .to_string()
                .contains("least-privilege")
        );

        let mut raw_secret = connection();
        raw_secret
            .mcp_binding
            .as_mut()
            .expect("binding")
            .credential_ref = "ref:sk-proj-secretvalue".into();
        assert!(
            raw_secret
                .validate()
                .unwrap_err()
                .to_string()
                .contains("ref:")
        );

        let mut false_provenance = connection();
        false_provenance
            .mcp_binding
            .as_mut()
            .expect("binding")
            .provenance
            .authority = "https://orb.example.test".into();
        assert!(
            validate_hosted_orb_connection(&false_provenance)
                .unwrap_err()
                .to_string()
                .contains("provenance")
        );

        let mut wrong_reference_namespace = connection();
        wrong_reference_namespace
            .mcp_binding
            .as_mut()
            .expect("binding")
            .credential_ref = "ref:other/credential/00000000-0000-4000-8000-000000000003".into();
        assert!(
            validate_hosted_orb_connection(&wrong_reference_namespace)
                .unwrap_err()
                .to_string()
                .contains("Computer-owned reference namespace")
        );
    }

    #[test]
    fn managed_health_does_not_claim_remote_reachability() {
        let detail = managed_mcp_health_detail(&connection()).unwrap();
        assert!(detail.contains("validated metadata only"));
        assert!(detail.contains("did not probe remote authentication or reachability"));
    }
}
