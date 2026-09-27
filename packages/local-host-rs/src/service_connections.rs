//! Managed service connections and scoped credential leases.
//!
//! Connection metadata is safe to persist. Secret values are either held by
//! the operating-system credential store or resolved from an operator-owned
//! environment, file, or 1Password reference at the moment a client is built.
//! Plugins may describe connection types, but they never receive secret
//! values and they cannot implement a secret backend.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use wait_timeout::ChildExt;
use zeroize::Zeroizing;

const KEYRING_SERVICE: &str = "maestro-connections";
const MAX_LEASE_TTL_MS: i64 = 60 * 60 * 1_000;
const MAX_ACTIVE_LEASES: usize = 1_024;

fn run_subscription_probe(
    command: &mut Command,
    provider: &str,
) -> Result<(std::process::ExitStatus, Vec<u8>)> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("{provider} CLI is not installed"))?;
    let status = match child.wait_timeout(Duration::from_secs(10))? {
        Some(status) => status,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{provider} CLI check timed out after 10 seconds");
        }
    };
    let mut stdout = Vec::new();
    child
        .stdout
        .take()
        .context("subscription CLI stdout unavailable")?
        .take(64 * 1024)
        .read_to_end(&mut stdout)?;
    Ok((status, stdout))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionAuthKind {
    ApiKey,
    Subscription,
    #[serde(rename = "oauth")]
    OAuth,
    WorkloadIdentity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionPlacement {
    Local,
    Platform,
    Either,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    Active,
    Revoked,
}

/// A non-secret pointer to credential material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConnectionSecretRef {
    Keyring {
        service: String,
        account: String,
    },
    Environment {
        name: String,
    },
    File {
        path: PathBuf,
    },
    OnePassword {
        reference: String,
    },
    /// The named provider transport owns authentication. Maestro never
    /// materializes the vendor subscription token as a generic bearer.
    Delegated {
        provider: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
    },
}

/// Secret-free metadata for a remote MCP server owned by an external
/// connection authority. The credential reference is an opaque pointer; the
/// credential broker that owns it is responsible for resolving authentication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConnectionMcpBinding {
    pub server_name: String,
    pub endpoint: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_preset: Option<String>,
    pub credential_ref: String,
    pub provenance: ConnectionMcpProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConnectionMcpProvenance {
    pub authority: String,
    pub reference: String,
}

/// Local attribution to a verified Deixic user. This is not proof of an
/// external vendor identity and carries no provider credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConnectionOwner {
    pub organization_id: String,
    pub user_id: String,
}

impl ConnectionOwner {
    fn matches(&self, organization_id: &str, user_id: Option<&str>) -> bool {
        self.organization_id == organization_id && user_id == Some(&self.user_id)
    }

    pub fn require_current_user(&self) -> Result<()> {
        let current = crate::credential_mode::verified_current_identity_session()?;
        self.require_session(&current)
    }

    pub fn require_session(&self, current: &crate::credential_mode::PlatformSession) -> Result<()> {
        if !self.matches(&current.organization_id, current.user_id.as_deref()) {
            bail!("subscription connection belongs to another Deixic user or organization");
        }
        Ok(())
    }
}

impl ConnectionMcpBinding {
    pub fn validate(&self) -> Result<()> {
        validate_mcp_server_name(&self.server_name)?;
        let endpoint =
            url::Url::parse(&self.endpoint).context("invalid managed MCP connection endpoint")?;
        if endpoint.scheme() != "https"
            || endpoint.host_str().is_none()
            || endpoint.username() != ""
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            bail!(
                "managed MCP connection endpoint must be HTTPS without URL credentials, query, or fragment"
            );
        }
        if self.scopes.is_empty() {
            bail!("managed MCP connections must declare at least one capability scope");
        }
        validate_capabilities(&self.scopes)?;
        validate_opaque_reference("managed MCP credential_ref", &self.credential_ref)?;
        self.provenance.validate()
    }
}

impl ConnectionMcpProvenance {
    fn validate(&self) -> Result<()> {
        if self.authority.trim().is_empty() {
            bail!("managed MCP connection provenance authority must not be empty");
        }
        if self.authority.starts_with("https://") {
            let authority = url::Url::parse(&self.authority)
                .context("invalid managed MCP provenance authority")?;
            if authority.host_str().is_none()
                || authority.username() != ""
                || authority.password().is_some()
                || authority.query().is_some()
                || authority.fragment().is_some()
            {
                bail!("managed MCP provenance authority must be a credential-free HTTPS origin");
            }
        } else {
            validate_identifier("managed MCP provenance authority", &self.authority)?;
        }
        validate_opaque_reference("managed MCP provenance reference", &self.reference)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServiceConnection {
    pub id: String,
    pub type_id: String,
    pub provider_id: String,
    pub label: String,
    pub auth_kind: ConnectionAuthKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_var: Option<String>,
    pub secret_ref: ConnectionSecretRef,
    pub placement: ConnectionPlacement,
    pub state: ConnectionState,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_binding: Option<ConnectionMcpBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<ConnectionOwner>,
    pub generation: u64,
    #[serde(default)]
    pub is_default: bool,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

impl ServiceConnection {
    pub fn validate(&self) -> Result<()> {
        for (field, value) in [
            ("id", self.id.as_str()),
            ("type_id", self.type_id.as_str()),
            ("provider_id", self.provider_id.as_str()),
            ("label", self.label.as_str()),
        ] {
            if value.trim().is_empty() {
                bail!("connection {field} must not be empty");
            }
        }
        if self.generation == 0 {
            bail!("connection generation must be positive");
        }
        if matches!(
            self.type_id.as_str(),
            "claude-subscription" | "copilot-subscription"
        ) && self.owner.is_none()
        {
            bail!("subscription connection requires a Deixic user owner");
        }
        if matches!(
            self.type_id.as_str(),
            "claude-subscription" | "copilot-subscription"
        ) {
            let expected = if self.type_id == "claude-subscription" {
                "claude-code"
            } else {
                "github-copilot"
            };
            if self.provider_id != expected
                || self.auth_kind != ConnectionAuthKind::Subscription
                || self.placement != ConnectionPlacement::Local
                || !matches!(&self.secret_ref, ConnectionSecretRef::Delegated { provider, profile: None } if provider == expected)
            {
                bail!("subscription connection has an invalid provider-owned transport");
            }
        }
        if let Some(owner) = &self.owner {
            if !matches!(
                self.type_id.as_str(),
                "codex-subscription" | "claude-subscription" | "copilot-subscription"
            ) || owner.organization_id.trim().is_empty()
                || owner.user_id.trim().is_empty()
            {
                bail!("subscription connection owner must name a user and organization");
            }
        }
        match self.auth_kind {
            ConnectionAuthKind::ApiKey => {
                let env_var = self
                    .env_var
                    .as_deref()
                    .filter(|value| !value.is_empty())
                    .context("API-key connections require an env_var injection target")?;
                if matches!(self.secret_ref, ConnectionSecretRef::Delegated { .. }) {
                    bail!("API-key connections cannot use delegated authentication");
                }
                validate_provider_env_target(&self.provider_id, env_var)?;
            }
            ConnectionAuthKind::Subscription
            | ConnectionAuthKind::OAuth
            | ConnectionAuthKind::WorkloadIdentity => {
                if self.env_var.is_some() {
                    bail!("delegated connections cannot declare an env_var injection target");
                }
                if !matches!(self.secret_ref, ConnectionSecretRef::Delegated { .. }) {
                    bail!("non-API-key connections require delegated authentication");
                }
            }
        }
        match &self.secret_ref {
            ConnectionSecretRef::Keyring { service, account }
                if service != KEYRING_SERVICE
                    || account != &keyring_account(&self.id, self.generation) =>
            {
                bail!("managed keyring references must be scoped to the connection generation");
            }
            ConnectionSecretRef::Environment { name } => validate_env_name(name)?,
            ConnectionSecretRef::File { path } if !path.is_absolute() => {
                bail!("connection credential file paths must be absolute");
            }
            ConnectionSecretRef::OnePassword { reference } => {
                crate::ai::op_secret::validate_reference(reference)?;
            }
            ConnectionSecretRef::Delegated { provider, .. }
                if provider.trim().is_empty() || provider != &self.provider_id =>
            {
                bail!("delegated connection provider must match provider_id")
            }
            _ => {}
        }
        validate_identifier("connection id", &self.id)?;
        validate_identifier("connection type id", &self.type_id)?;
        if self.capabilities.is_empty() {
            bail!("connections must declare at least one capability");
        }
        validate_capabilities(&self.capabilities)?;
        if let Some(binding) = &self.mcp_binding {
            binding.validate()?;
            let capabilities = self.capabilities.iter().collect::<BTreeSet<_>>();
            if binding
                .scopes
                .iter()
                .any(|scope| !capabilities.contains(scope))
            {
                bail!(
                    "managed MCP connection scopes must be included in the connection capabilities"
                );
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConnectionStore {
    #[serde(default = "connection_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub connections: Vec<ServiceConnection>,
}

impl Default for ConnectionStore {
    fn default() -> Self {
        Self {
            schema_version: connection_schema_version(),
            connections: Vec::new(),
        }
    }
}

const fn connection_schema_version() -> u32 {
    1
}

impl ConnectionStore {
    pub fn load(path: &Path) -> Result<Self> {
        let store = match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).context("invalid connections.json")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => return Err(error.into()),
        };
        store.validate()?;
        Ok(store)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        crate::path_utils::atomic_private_write(path, &serde_json::to_vec_pretty(self)?)
    }

    pub fn default_path() -> Result<PathBuf> {
        Ok(crate::path_utils::maestro_home_dir()
            .context("could not resolve Maestro home")?
            .join("connections.json"))
    }

    pub fn get(&self, id: &str) -> Option<&ServiceConnection> {
        self.connections
            .iter()
            .find(|connection| connection.id == id)
    }

    pub fn upsert(&mut self, connection: ServiceConnection) -> Result<()> {
        connection.validate()?;
        if connection.is_default {
            for existing in &mut self.connections {
                if existing.provider_id == connection.provider_id {
                    existing.is_default = false;
                }
            }
        }
        if let Some(existing) = self
            .connections
            .iter_mut()
            .find(|existing| existing.id == connection.id)
        {
            *existing = connection;
        } else {
            self.connections.push(connection);
        }
        self.connections
            .sort_by(|left, right| left.id.cmp(&right.id));
        self.validate()
    }

    pub fn remove(&mut self, id: &str) -> Option<ServiceConnection> {
        let index = self.connections.iter().position(|item| item.id == id)?;
        let removed = self.connections.remove(index);
        if removed.is_default {
            if let Some(next) = self.connections.iter_mut().find(|connection| {
                connection.provider_id == removed.provider_id
                    && connection.state == ConnectionState::Active
                    && !matches!(
                        connection.secret_ref,
                        ConnectionSecretRef::OnePassword { .. }
                    )
            }) {
                next.is_default = true;
            }
        }
        Some(removed)
    }

    pub fn set_default(&mut self, id: &str) -> Result<()> {
        let provider = self
            .get(id)
            .with_context(|| format!("connection not found: {id}"))?
            .to_owned();
        if provider.state != ConnectionState::Active {
            bail!("revoked connections cannot be selected as default");
        }
        let provider = provider.provider_id;
        for connection in &mut self.connections {
            if connection.provider_id == provider {
                connection.is_default = connection.id == id;
            }
        }
        Ok(())
    }

    pub fn selected(
        &self,
        provider: &str,
        explicit_id: Option<&str>,
    ) -> Result<Option<&ServiceConnection>> {
        if let Some(id) = explicit_id {
            let connection = self
                .get(id)
                .with_context(|| format!("managed connection not found: {id}"))?;
            if connection.provider_id != provider {
                bail!(
                    "connection {id} is for provider {}, not {provider}",
                    connection.provider_id
                );
            }
            return Ok(Some(connection));
        }
        if let Some(connection) = self.connections.iter().find(|connection| {
            connection.provider_id == provider
                && connection.is_default
                && connection.state == ConnectionState::Active
        }) {
            return Ok(Some(connection));
        }
        Ok(self
            .connections
            .iter()
            .filter(|connection| {
                connection.provider_id == provider
                    && connection.state == ConnectionState::Active
                    && !matches!(
                        connection.secret_ref,
                        ConnectionSecretRef::OnePassword { .. }
                    )
            })
            .max_by_key(|connection| connection.updated_at_ms))
    }

    pub fn delegated_profile(
        &self,
        provider_id: &str,
        explicit_connection: Option<&str>,
    ) -> Result<Option<String>> {
        let Some(connection) = self.selected(provider_id, explicit_connection)? else {
            return Ok(None);
        };
        if connection.state != ConnectionState::Active {
            bail!("connection {} is revoked", connection.id);
        }
        match &connection.secret_ref {
            ConnectionSecretRef::Delegated { profile, .. } => Ok(profile.clone()),
            _ => bail!(
                "connection {} does not use provider-owned delegated authentication",
                connection.id
            ),
        }
    }

    fn validate(&self) -> Result<()> {
        if self.schema_version != connection_schema_version() {
            bail!("unsupported connections.json schema version");
        }
        let mut ids = BTreeSet::new();
        let mut defaults = BTreeSet::new();
        for connection in &self.connections {
            connection.validate()?;
            if !ids.insert(&connection.id) {
                bail!("duplicate connection id: {}", connection.id);
            }
            if connection.is_default && !defaults.insert(&connection.provider_id) {
                bail!(
                    "provider {} has multiple default connections",
                    connection.provider_id
                );
            }
        }
        Ok(())
    }
}

pub trait SecretBackend: Send + Sync {
    fn get(&self, service: &str, account: &str) -> Result<Zeroizing<String>>;
    fn set(&self, service: &str, account: &str, value: &str) -> Result<()>;
    fn delete(&self, service: &str, account: &str) -> Result<()>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct KeyringSecretBackend;

impl SecretBackend for KeyringSecretBackend {
    fn get(&self, service: &str, account: &str) -> Result<Zeroizing<String>> {
        let value = crate::native_credentials::entry(service, account)
            .context("OS credential store is unavailable")?
            .get_password()
            .with_context(|| {
                format!("credential material is unavailable for connection {account}")
            })?;
        Ok(Zeroizing::new(value))
    }

    fn set(&self, service: &str, account: &str, value: &str) -> Result<()> {
        if value.trim().is_empty() {
            bail!("credential value must not be empty");
        }
        crate::native_credentials::entry(service, account)
            .context("OS credential store is unavailable")?
            .set_password(value)
            .with_context(|| format!("failed to store credential for connection {account}"))
    }

    fn delete(&self, service: &str, account: &str) -> Result<()> {
        match crate::native_credentials::entry(service, account)
            .context("OS credential store is unavailable")?
            .delete_credential()
        {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error)
                .with_context(|| format!("failed to delete credential for connection {account}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionLease {
    pub lease_id: String,
    pub connection_id: String,
    pub provider_id: String,
    pub generation: u64,
    pub audience: String,
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub resources: Vec<String>,
    pub issued_at_ms: i64,
    pub expires_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionUseReceipt {
    pub lease_id: String,
    pub connection_id: String,
    pub provider_id: String,
    pub generation: u64,
    pub audience: String,
    pub capabilities: Vec<String>,
    pub used_at_ms: i64,
    pub outcome: String,
}

/// The first supported 1Password capability is a native model invocation.
/// Do not turn arbitrary plugin capability labels into bearer-token access.
pub(crate) fn one_password_model_destination(
    connection: &ServiceConnection,
) -> Result<&'static str> {
    if connection.auth_kind != ConnectionAuthKind::ApiKey
        || connection.capabilities != ["models.invoke"]
        || connection.mcp_binding.is_some()
    {
        bail!("1Password currently supports only the native models.invoke capability");
    }
    let provider = crate::ai::ProviderRegistry::descriptor(&connection.provider_id)
        .context("1Password requires a supported native provider")?;
    provider
        .default_base_url
        .filter(|url| url.starts_with("https://"))
        .context("1Password requires a provider with a fixed HTTPS destination")
}

fn validate_one_password_client(
    connection: &ServiceConnection,
    env: &HashMap<String, String>,
) -> Result<()> {
    let destination = one_password_model_destination(connection)?;
    let provider = crate::ai::ProviderRegistry::descriptor(&connection.provider_id)
        .context("unsupported 1Password provider")?;
    for name in provider.base_url_env {
        if env.get(*name).is_some_and(|value| {
            !value.trim().is_empty()
                && value.trim().trim_end_matches('/') != destination.trim_end_matches('/')
        }) {
            bail!("1Password capability cannot be sent to a custom provider endpoint");
        }
    }
    Ok(())
}

pub struct ConnectionBroker<B = KeyringSecretBackend> {
    store: ConnectionStore,
    persisted_store_path: Option<PathBuf>,
    backend: B,
    active_leases: Mutex<HashMap<String, ConnectionLease>>,
}

impl ConnectionBroker<KeyringSecretBackend> {
    pub fn load_default() -> Result<Self> {
        let path = ConnectionStore::default_path()?;
        Ok(Self::new_persisted(
            ConnectionStore::load(&path)?,
            KeyringSecretBackend,
            path,
        ))
    }

    /// Merge the default managed connection only when the caller does not
    /// already have a usable provider credential. This keeps the optional
    /// managed-connection store outside the legacy credential startup path.
    pub fn merge_default_for_model(
        model: &str,
        env: &mut HashMap<String, String>,
    ) -> Result<Option<String>> {
        let path = ConnectionStore::default_path()?;
        Self::merge_persisted_for_model(&path, model, env)
    }

    fn merge_persisted_for_model(
        path: &Path,
        model: &str,
        env: &mut HashMap<String, String>,
    ) -> Result<Option<String>> {
        // Codex app-server owns this delegated subscription. The native host
        // selects its profile separately; there is no bearer to inject.
        if crate::codex_auth::resolve_model_route(model).uses_app_server()
            || crate::ai::ProviderRegistry::resolve_descriptor(model)
                .is_ok_and(|provider| matches!(provider.id, "claude-code" | "github-copilot"))
        {
            return Ok(None);
        }
        if managed_connection_store_can_be_skipped(model, env)? {
            return Ok(None);
        }
        Self::new_persisted(
            ConnectionStore::load(path)?,
            KeyringSecretBackend,
            path.to_owned(),
        )
        .merge_for_model(model, env)
    }
}

fn managed_connection_store_can_be_skipped(
    model: &str,
    env: &HashMap<String, String>,
) -> Result<bool> {
    let provider = crate::ai::ProviderRegistry::resolve_descriptor(model)?;
    let explicit = env
        .get("MAESTRO_CONNECTION")
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    Ok(explicit.is_none()
        && (!provider.requires_auth()
            || provider.auth_env.iter().any(|name| {
                env.get(*name).is_some_and(|value| !value.trim().is_empty())
                    || env
                        .get(&format!("{name}_FILE"))
                        .is_some_and(|value| !value.trim().is_empty())
            })))
}

impl<B: SecretBackend> ConnectionBroker<B> {
    pub fn new(store: ConnectionStore, backend: B) -> Self {
        Self {
            store,
            persisted_store_path: None,
            backend,
            active_leases: Mutex::new(HashMap::new()),
        }
    }

    fn new_persisted(store: ConnectionStore, backend: B, path: PathBuf) -> Self {
        Self {
            store,
            persisted_store_path: Some(path),
            backend,
            active_leases: Mutex::new(HashMap::new()),
        }
    }

    fn current_store(&self) -> Result<ConnectionStore> {
        self.persisted_store_path
            .as_deref()
            .map_or_else(|| Ok(self.store.clone()), ConnectionStore::load)
    }

    pub fn issue_lease(
        &self,
        connection_id: &str,
        audience: &str,
        capabilities: Vec<String>,
        resources: Vec<String>,
        ttl_ms: i64,
        now_ms: i64,
    ) -> Result<ConnectionLease> {
        let current_store = self.current_store()?;
        let connection = current_store
            .get(connection_id)
            .with_context(|| format!("connection not found: {connection_id}"))?;
        if connection.state != ConnectionState::Active {
            bail!("connection {connection_id} is revoked");
        }
        if audience.trim().is_empty() {
            bail!("connection lease audience must not be empty");
        }
        if !(1..=MAX_LEASE_TTL_MS).contains(&ttl_ms) {
            bail!("connection lease ttl must be between 1ms and {MAX_LEASE_TTL_MS}ms");
        }
        validate_capabilities(&capabilities)?;
        if capabilities.is_empty() {
            bail!("connection leases must request at least one capability");
        }
        validate_sorted_values("connection lease resources", &resources)?;
        let allowed = connection.capabilities.iter().collect::<BTreeSet<_>>();
        if capabilities
            .iter()
            .any(|capability| !allowed.contains(capability))
        {
            bail!("connection lease requests a capability the connection does not allow");
        }
        if matches!(
            connection.secret_ref,
            ConnectionSecretRef::OnePassword { .. }
        ) {
            let destination = one_password_model_destination(connection)?;
            if audience != "native-model-client"
                || capabilities != ["models.invoke"]
                || resources != [destination]
            {
                bail!(
                    "1Password capability is restricted to its native model client and destination"
                );
            }
        }
        let expires_at_ms = now_ms
            .checked_add(ttl_ms)
            .context("connection lease expiry overflows timestamp range")?;
        let lease = ConnectionLease {
            lease_id: uuid::Uuid::new_v4().to_string(),
            connection_id: connection.id.clone(),
            provider_id: connection.provider_id.clone(),
            generation: connection.generation,
            audience: audience.to_owned(),
            capabilities,
            resources,
            issued_at_ms: now_ms,
            expires_at_ms,
        };
        let mut active_leases = self
            .active_leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active_leases.retain(|_, active| active.expires_at_ms >= now_ms);
        if active_leases.len() >= MAX_ACTIVE_LEASES {
            bail!("active connection lease limit reached");
        }
        active_leases.insert(lease.lease_id.clone(), lease.clone());
        Ok(lease)
    }

    pub(crate) fn apply_lease_to_env(
        &self,
        lease: &ConnectionLease,
        expected_audience: &str,
        env: &mut HashMap<String, String>,
        now_ms: i64,
    ) -> Result<ConnectionUseReceipt> {
        let lease_is_minted = self
            .active_leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&lease.lease_id)
            .is_some_and(|minted| minted == lease);
        if !lease_is_minted {
            bail!("connection lease is unknown or has been modified");
        }
        if lease.audience != expected_audience {
            bail!("connection lease audience mismatch");
        }
        if now_ms < lease.issued_at_ms || now_ms > lease.expires_at_ms {
            bail!("connection lease is outside its validity window");
        }
        let current_store = self.current_store()?;
        let connection = current_store
            .get(&lease.connection_id)
            .with_context(|| format!("connection not found: {}", lease.connection_id))?;
        if connection.state != ConnectionState::Active
            || connection.generation != lease.generation
            || connection.provider_id != lease.provider_id
        {
            bail!("connection lease is stale or revoked");
        }
        let current_capabilities = connection.capabilities.iter().collect::<BTreeSet<_>>();
        if lease
            .capabilities
            .iter()
            .any(|capability| !current_capabilities.contains(capability))
        {
            bail!("connection lease authority is no longer allowed");
        }
        if connection.placement == ConnectionPlacement::Platform {
            bail!("platform-only connections cannot be used by a local client");
        }
        let outcome = if matches!(connection.secret_ref, ConnectionSecretRef::Delegated { .. }) {
            "delegated_to_provider_transport"
        } else {
            self.inject(connection, env)?;
            "injected_into_client_scope"
        };
        Ok(ConnectionUseReceipt {
            lease_id: lease.lease_id.clone(),
            connection_id: connection.id.clone(),
            provider_id: connection.provider_id.clone(),
            generation: connection.generation,
            audience: lease.audience.clone(),
            capabilities: lease.capabilities.clone(),
            used_at_ms: now_ms,
            outcome: outcome.to_owned(),
        })
    }

    /// Apply a selected managed connection to a per-client environment map.
    /// Existing provider credentials win unless `MAESTRO_CONNECTION` names an
    /// explicit connection, preserving the current env-based contract.
    pub fn merge_for_model(
        &self,
        model: &str,
        env: &mut HashMap<String, String>,
    ) -> Result<Option<String>> {
        // Resolve identity without touching existing credential sources. An
        // explicitly selected managed connection must be able to replace a
        // stale *_FILE or op:// source before the native client resolves it.
        let provider = crate::ai::ProviderRegistry::resolve_descriptor(model)?;
        let explicit = env
            .get("MAESTRO_CONNECTION")
            .map(String::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if managed_connection_store_can_be_skipped(model, env)? {
            return Ok(None);
        }
        let current_store = self.current_store()?;
        let Some(connection) = current_store.selected(provider.id, explicit)? else {
            return Ok(None);
        };
        if connection.state != ConnectionState::Active {
            bail!("connection {} is revoked", connection.id);
        }
        if connection.placement == ConnectionPlacement::Platform {
            bail!("platform-only connections cannot be used by a local client");
        }
        if matches!(
            connection.secret_ref,
            ConnectionSecretRef::OnePassword { .. }
        ) {
            validate_one_password_client(connection, env)?;
            let timestamp = now_ms();
            let lease = self.issue_lease(
                &connection.id,
                "native-model-client",
                vec!["models.invoke".to_owned()],
                vec![one_password_model_destination(connection)?.to_owned()],
                60_000,
                timestamp,
            )?;
            let result = self.apply_lease_to_env(&lease, "native-model-client", env, timestamp);
            self.active_leases
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&lease.lease_id);
            result?;
        } else {
            self.inject(connection, env)?;
        }
        Ok(Some(connection.id.clone()))
    }

    pub fn check(&self, connection_id: &str, env: &HashMap<String, String>) -> Result<()> {
        let current_store = self.current_store()?;
        let connection = current_store
            .get(connection_id)
            .with_context(|| format!("connection not found: {connection_id}"))?;
        if connection.state != ConnectionState::Active {
            bail!("connection is revoked");
        }
        if connection.placement == ConnectionPlacement::Platform {
            bail!("platform-only connections cannot be checked by a local client");
        }
        match &connection.secret_ref {
            ConnectionSecretRef::Delegated { provider, profile } if provider == "openai-codex" => {
                if let Some(owner) = &connection.owner {
                    owner.require_current_user()?;
                }
                let workspace = std::env::current_dir().context("could not resolve workspace")?;
                crate::codex_subscription::check_chatgpt_profile(profile.as_deref(), &workspace)
            }
            ConnectionSecretRef::Delegated { provider, .. }
                if matches!(provider.as_str(), "claude-code" | "github-copilot") =>
            {
                connection
                    .owner
                    .as_ref()
                    .context("subscription connection has no Deixic owner")?
                    .require_current_user()?;
                if provider == "claude-code" {
                    let mut command = std::process::Command::new("claude");
                    command.args(["auth", "status", "--json"]);
                    for key in [
                        "ANTHROPIC_API_KEY",
                        "ANTHROPIC_AUTH_TOKEN",
                        "ANTHROPIC_BASE_URL",
                        "CLAUDE_CODE_USE_BEDROCK",
                        "CLAUDE_CODE_USE_VERTEX",
                        "CLAUDE_CODE_USE_FOUNDRY",
                    ] {
                        command.env_remove(key);
                    }
                    let (exit_status, stdout) =
                        run_subscription_probe(&mut command, "Claude Code")?;
                    let status: serde_json::Value = serde_json::from_slice(&stdout)
                        .context("Claude Code returned an invalid auth status")?;
                    if !exit_status.success()
                        || status.get("loggedIn").and_then(serde_json::Value::as_bool) != Some(true)
                        || status.get("authMethod").and_then(serde_json::Value::as_str)
                            != Some("claude.ai")
                        || status
                            .get("apiProvider")
                            .and_then(serde_json::Value::as_str)
                            != Some("firstParty")
                    {
                        bail!("Claude Code requires a first-party claude.ai subscription login");
                    }
                } else {
                    let (exit_status, _) = run_subscription_probe(
                        std::process::Command::new("copilot").arg("--version"),
                        "Copilot",
                    )?;
                    if !exit_status.success() {
                        bail!("Copilot CLI is not available");
                    }
                }
                Ok(())
            }
            _ if connection.mcp_binding.is_some() => {
                crate::orb_connection::validate_managed_mcp_connection(connection)
            }
            ConnectionSecretRef::Delegated { provider, .. } => {
                bail!("delegated authentication transport is unsupported for provider {provider}")
            }
            _ => self.resolve_secret(connection, env).map(|_| ()),
        }
    }

    fn inject(
        &self,
        connection: &ServiceConnection,
        env: &mut HashMap<String, String>,
    ) -> Result<()> {
        if connection.placement == ConnectionPlacement::Platform {
            bail!("platform-only connections cannot be injected into a local client");
        }
        if matches!(connection.secret_ref, ConnectionSecretRef::Delegated { .. }) {
            bail!(
                "delegated connection {} must be used through its provider-owned transport",
                connection.id
            );
        }
        let target = connection
            .env_var
            .as_deref()
            .context("connection has no credential injection target")?;
        if matches!(
            connection.secret_ref,
            ConnectionSecretRef::OnePassword { .. }
        ) {
            validate_one_password_client(connection, env)?;
        }
        let secret = self.resolve_secret(connection, env)?;
        env.insert(target.to_owned(), secret.to_string());
        Ok(())
    }

    fn resolve_secret(
        &self,
        connection: &ServiceConnection,
        env: &HashMap<String, String>,
    ) -> Result<Zeroizing<String>> {
        let value = match &connection.secret_ref {
            ConnectionSecretRef::Keyring { service, account } => {
                return self.backend.get(service, account);
            }
            ConnectionSecretRef::Environment { name } => {
                env.get(name).cloned().with_context(|| {
                    format!("connection source environment variable {name} is not set")
                })?
            }
            ConnectionSecretRef::File { path } => fs::read_to_string(path).with_context(|| {
                format!("failed to read connection source file {}", path.display())
            })?,
            ConnectionSecretRef::OnePassword { reference } => {
                crate::ai::op_secret::resolve_credential("managed connection", reference)?
            }
            ConnectionSecretRef::Delegated { .. } => {
                bail!("delegated connections do not expose credential material")
            }
        };
        let value = value.trim().to_owned();
        if value.is_empty() {
            bail!("connection credential source resolved to an empty value");
        }
        Ok(Zeroizing::new(value))
    }
}

fn keyring_account(connection_id: &str, generation: u64) -> String {
    format!("{connection_id}:g{generation}")
}

pub fn keyring_secret_ref(connection_id: &str, generation: u64) -> ConnectionSecretRef {
    ConnectionSecretRef::Keyring {
        service: KEYRING_SERVICE.to_owned(),
        account: keyring_account(connection_id, generation),
    }
}

/// Resolve a provider-owned auth profile without materializing its token.
/// An explicit provider profile keeps precedence; otherwise an explicitly
/// selected or default managed connection may supply the profile name.
pub fn selected_delegated_profile_from_env(provider_id: &str) -> Result<Option<String>> {
    let explicit_profile = (provider_id == "openai-codex")
        .then(|| std::env::var("MAESTRO_CODEX_PROFILE").ok())
        .flatten()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let explicit_connection = std::env::var("MAESTRO_CONNECTION")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    if explicit_profile.is_some() && explicit_connection.is_some() {
        bail!("MAESTRO_CODEX_PROFILE and MAESTRO_CONNECTION cannot both select Codex auth");
    }
    if explicit_profile.is_some() {
        return Ok(explicit_profile);
    }
    selected_delegated_profile_from_store(
        provider_id,
        explicit_connection.as_deref(),
        &ConnectionStore::default_path()?,
    )
}

/// Subscription CLIs keep the vendor token, while this owned local record
/// binds the capability to the current Deixic Identity user.
pub fn require_owned_subscription_connection(
    provider_id: &str,
    identity: &crate::credential_mode::PlatformSession,
) -> Result<()> {
    if !matches!(provider_id, "claude-code" | "github-copilot") {
        bail!("unsupported subscription connection provider");
    }
    let path = ConnectionStore::default_path()?;
    let store = ConnectionStore::load(&path)?;
    let explicit = std::env::var("MAESTRO_CONNECTION")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let connection = store.selected(provider_id, explicit.as_deref())?.context(
        "Subscription is not linked to this Deixic user; connect it during managed onboarding",
    )?;
    let expected_type = if provider_id == "claude-code" {
        "claude-subscription"
    } else {
        "copilot-subscription"
    };
    if connection.type_id != expected_type {
        bail!("Selected subscription connection has the wrong type");
    }
    let owner = connection
        .owner
        .as_ref()
        .context("Subscription connection has no Deixic owner")?;
    owner.require_session(identity)?;
    if !matches!(&connection.secret_ref, ConnectionSecretRef::Delegated { provider, .. } if provider == provider_id)
    {
        bail!("Subscription connection has an invalid delegated provider");
    }
    Ok(())
}

fn selected_delegated_profile_from_store(
    provider_id: &str,
    explicit_connection: Option<&str>,
    path: &Path,
) -> Result<Option<String>> {
    match ConnectionStore::load(path) {
        Ok(store) => {
            if let Some(connection) = store.selected(provider_id, explicit_connection)? {
                if let Some(owner) = &connection.owner {
                    owner.require_current_user()?;
                }
            }
            store.delegated_profile(provider_id, explicit_connection)
        }
        // Managed metadata is optional when no connection was explicitly
        // selected. Fall through to the provider-owned default identity.
        Err(_) if explicit_connection.is_none() => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn validate_identifier(label: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || !value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
    {
        bail!("{label} contains unsupported characters");
    }
    Ok(())
}

fn validate_mcp_server_name(value: &str) -> Result<()> {
    if value.is_empty()
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        bail!("managed MCP server name contains unsupported characters");
    }
    Ok(())
}

pub(crate) fn validate_opaque_reference(label: &str, value: &str) -> Result<()> {
    let Some(reference) = value.strip_prefix("ref:") else {
        bail!("{label} must use the ref: opaque-reference format");
    };

    // A prefix alone does not make a value an opaque broker reference: a
    // caller could otherwise persist a literal token such as
    // `ref:sk-proj-secretvalue`. Require the broker's typed namespace and a
    // UUID-shaped opaque identifier so references cannot be mistaken for
    // credential material at this boundary.
    let mut segments = reference.split('/');
    let namespace = segments.next().unwrap_or_default();
    let kind = segments.next().unwrap_or_default();
    let identifier = segments.next().unwrap_or_default();
    if segments.next().is_some()
        || namespace.is_empty()
        || kind.is_empty()
        || identifier.is_empty()
        || reference.len() > 512
        || !namespace.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
        || !matches!(kind, "credential" | "connection" | "binding")
        || uuid::Uuid::parse_str(identifier).is_err()
    {
        bail!("{label} must use a broker-owned ref:<namespace>/<kind>/<uuid> format");
    }
    Ok(())
}

fn validate_env_name(value: &str) -> Result<()> {
    if value.is_empty()
        || !value.chars().all(|character| {
            character.is_ascii_uppercase() || character.is_ascii_digit() || character == '_'
        })
    {
        bail!("connection environment source must be an uppercase environment name");
    }
    Ok(())
}

pub(crate) fn validate_provider_env_target(provider_id: &str, env_var: &str) -> Result<()> {
    let Some(provider) = crate::ai::ProviderRegistry::descriptor(provider_id) else {
        return Ok(());
    };
    if provider.id != provider_id {
        bail!("connection provider_id must use the canonical provider id");
    }
    if !provider.auth_env.contains(&env_var) {
        bail!("connection env_var is not an authentication target for provider {provider_id}");
    }
    Ok(())
}

fn validate_capabilities(capabilities: &[String]) -> Result<()> {
    let mut unique = BTreeSet::new();
    let mut previous: Option<&str> = None;
    for capability in capabilities {
        if capability.is_empty()
            || !capability.chars().all(|character| {
                character.is_ascii_alphanumeric()
                    || matches!(character, '-' | '_' | '.' | ':' | '/')
            })
        {
            bail!("connection capability contains unsupported characters");
        }
        if previous.is_some_and(|value| value >= capability.as_str()) || !unique.insert(capability)
        {
            bail!("connection capabilities must be normalized, sorted, and unique");
        }
        previous = Some(capability);
    }
    Ok(())
}

fn validate_sorted_values(label: &str, values: &[String]) -> Result<()> {
    let mut previous: Option<&str> = None;
    for value in values {
        if value.trim().is_empty() || previous.is_some_and(|previous| previous >= value.as_str()) {
            bail!("{label} must be normalized, sorted, and unique");
        }
        previous = Some(value);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    #[derive(Default)]
    struct MemorySecrets(Mutex<HashMap<(String, String), String>>);

    impl SecretBackend for MemorySecrets {
        fn get(&self, service: &str, account: &str) -> Result<Zeroizing<String>> {
            Ok(Zeroizing::new(
                self.0
                    .lock()
                    .unwrap()
                    .get(&(service.to_owned(), account.to_owned()))
                    .cloned()
                    .context("missing test secret")?,
            ))
        }

        fn set(&self, service: &str, account: &str, value: &str) -> Result<()> {
            self.0
                .lock()
                .unwrap()
                .insert((service.to_owned(), account.to_owned()), value.to_owned());
            Ok(())
        }

        fn delete(&self, service: &str, account: &str) -> Result<()> {
            self.0
                .lock()
                .unwrap()
                .remove(&(service.to_owned(), account.to_owned()));
            Ok(())
        }
    }

    fn connection() -> ServiceConnection {
        ServiceConnection {
            id: "openai-personal".into(),
            type_id: "openai-api-key".into(),
            provider_id: "openai".into(),
            label: "Personal OpenAI".into(),
            auth_kind: ConnectionAuthKind::ApiKey,
            env_var: Some("OPENAI_API_KEY".into()),
            secret_ref: keyring_secret_ref("openai-personal", 1),
            placement: ConnectionPlacement::Local,
            state: ConnectionState::Active,
            capabilities: vec!["models.read".into(), "responses.create".into()],
            mcp_binding: None,
            owner: None,
            generation: 1,
            is_default: true,
            created_at_ms: 10,
            updated_at_ms: 10,
        }
    }

    #[test]
    fn persisted_metadata_never_contains_secret_value() {
        let connection = connection();
        let store = ConnectionStore {
            schema_version: 1,
            connections: vec![connection],
        };
        let encoded = serde_json::to_string(&store).unwrap();
        assert!(!encoded.contains("super-secret"));
        assert!(encoded.contains("maestro-connections"));
    }

    #[test]
    fn persisted_metadata_rejects_unknown_and_secret_bearing_fields() {
        let encoded = serde_json::to_value(ConnectionStore {
            schema_version: 1,
            connections: vec![connection()],
        })
        .unwrap();
        let mut unknown_root = encoded.clone();
        unknown_root
            .as_object_mut()
            .unwrap()
            .insert("unexpected".into(), serde_json::json!(true));
        assert!(serde_json::from_value::<ConnectionStore>(unknown_root).is_err());

        let mut secret_bearing = encoded;
        secret_bearing["connections"][0]
            .as_object_mut()
            .unwrap()
            .insert("secret".into(), serde_json::json!("must-not-be-accepted"));
        assert!(serde_json::from_value::<ConnectionStore>(secret_bearing).is_err());
    }

    #[test]
    fn persisted_metadata_rejects_auth_kind_and_provider_target_mismatches() {
        let mut wrong_target = connection();
        wrong_target.env_var = Some("ANTHROPIC_API_KEY".into());
        assert!(
            wrong_target
                .validate()
                .unwrap_err()
                .to_string()
                .contains("not an authentication target")
        );

        let mut delegated_with_materialized_secret = connection();
        delegated_with_materialized_secret.auth_kind = ConnectionAuthKind::Subscription;
        delegated_with_materialized_secret.env_var = None;
        assert!(
            delegated_with_materialized_secret
                .validate()
                .unwrap_err()
                .to_string()
                .contains("require delegated authentication")
        );

        let mut delegated_with_env_target = connection();
        delegated_with_env_target.auth_kind = ConnectionAuthKind::OAuth;
        delegated_with_env_target.env_var = Some("OPENAI_API_KEY".into());
        delegated_with_env_target.secret_ref = ConnectionSecretRef::Delegated {
            provider: "openai".into(),
            profile: None,
        };
        assert!(
            delegated_with_env_target
                .validate()
                .unwrap_err()
                .to_string()
                .contains("cannot declare an env_var")
        );
    }

    #[test]
    fn default_connection_is_injected_only_into_client_env() {
        let process_value_before = std::env::var_os("OPENAI_API_KEY");
        let backend = MemorySecrets::default();
        backend
            .set(
                KEYRING_SERVICE,
                &keyring_account("openai-personal", 1),
                "super-secret",
            )
            .unwrap();
        let broker = ConnectionBroker::new(
            ConnectionStore {
                schema_version: 1,
                connections: vec![connection()],
            },
            backend,
        );
        let mut env = HashMap::new();
        assert_eq!(
            broker.merge_for_model("openai/gpt-4o", &mut env).unwrap(),
            Some("openai-personal".into())
        );
        assert_eq!(
            env.get("OPENAI_API_KEY").map(String::as_str),
            Some("super-secret")
        );
        assert_eq!(std::env::var_os("OPENAI_API_KEY"), process_value_before);
    }

    #[test]
    fn existing_environment_credential_wins_without_explicit_selection() {
        let broker = ConnectionBroker::new(
            ConnectionStore {
                schema_version: 1,
                connections: vec![connection()],
            },
            MemorySecrets::default(),
        );
        let mut env = HashMap::from([("OPENAI_API_KEY".into(), "existing".into())]);
        assert_eq!(broker.merge_for_model("gpt-4o", &mut env).unwrap(), None);
        assert_eq!(env["OPENAI_API_KEY"], "existing");
    }

    #[test]
    fn selected_falls_back_to_the_sole_active_connection() {
        let mut only = connection();
        only.is_default = false;
        let store = ConnectionStore {
            schema_version: 1,
            connections: vec![only],
        };
        assert_eq!(
            store
                .selected("openai", None)
                .unwrap()
                .map(|connection| connection.id.as_str()),
            Some("openai-personal")
        );
    }

    #[test]
    fn selected_uses_the_newest_active_connection_when_no_default() {
        let mut older = connection();
        older.is_default = false;
        older.updated_at_ms = 10;
        let mut newer = connection();
        newer.id = "openai-work".into();
        newer.is_default = false;
        newer.updated_at_ms = 20;
        let store = ConnectionStore {
            schema_version: 1,
            connections: vec![older, newer],
        };
        assert_eq!(
            store
                .selected("openai", None)
                .unwrap()
                .map(|connection| connection.id.as_str()),
            Some("openai-work")
        );
    }

    #[test]
    fn existing_environment_credential_skips_a_malformed_optional_store() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("connections.json");
        fs::write(&path, "not-json").unwrap();
        let mut env = HashMap::from([("OPENAI_API_KEY".into(), "existing".into())]);

        assert_eq!(
            ConnectionBroker::merge_persisted_for_model(&path, "gpt-4o", &mut env).unwrap(),
            None
        );
        assert_eq!(env["OPENAI_API_KEY"], "existing");
    }

    #[test]
    fn authless_provider_skips_a_malformed_optional_store() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("connections.json");
        fs::write(&path, "not-json").unwrap();
        let mut env = HashMap::new();

        assert_eq!(
            ConnectionBroker::merge_persisted_for_model(&path, "ollama/llama3.2", &mut env,)
                .unwrap(),
            None
        );
    }

    #[test]
    fn explicit_selection_still_validates_the_optional_store() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("connections.json");
        fs::write(&path, "not-json").unwrap();
        let mut env = HashMap::from([
            ("OPENAI_API_KEY".into(), "existing".into()),
            ("MAESTRO_CONNECTION".into(), "openai-personal".into()),
        ]);

        let error =
            ConnectionBroker::merge_persisted_for_model(&path, "gpt-4o", &mut env).unwrap_err();
        assert!(error.to_string().contains("invalid connections.json"));
    }

    #[test]
    fn explicit_connection_replaces_an_unreadable_provider_file_source() {
        let backend = MemorySecrets::default();
        backend
            .set(
                KEYRING_SERVICE,
                &keyring_account("openai-personal", 1),
                "managed-secret",
            )
            .unwrap();
        let broker = ConnectionBroker::new(
            ConnectionStore {
                schema_version: 1,
                connections: vec![connection()],
            },
            backend,
        );
        let mut env = HashMap::from([
            ("MAESTRO_CONNECTION".into(), "openai-personal".into()),
            (
                "OPENAI_API_KEY_FILE".into(),
                "/definitely/missing/provider-key".into(),
            ),
        ]);

        assert_eq!(
            broker.merge_for_model("openai/gpt-4o", &mut env).unwrap(),
            Some("openai-personal".into())
        );
        assert_eq!(env["OPENAI_API_KEY"], "managed-secret");
    }

    #[test]
    fn platform_only_connection_cannot_be_injected_locally() {
        let mut platform_connection = connection();
        platform_connection.placement = ConnectionPlacement::Platform;
        let broker = ConnectionBroker::new(
            ConnectionStore {
                schema_version: 1,
                connections: vec![platform_connection],
            },
            MemorySecrets::default(),
        );
        let mut env = HashMap::from([("MAESTRO_CONNECTION".into(), "openai-personal".into())]);
        let error = broker
            .merge_for_model("openai/gpt-4o", &mut env)
            .unwrap_err();
        assert!(error.to_string().contains("platform-only"));
    }

    #[test]
    fn delegated_subscription_connection_selects_vendor_profile_without_a_token() {
        let store = ConnectionStore {
            schema_version: 1,
            connections: vec![ServiceConnection {
                id: "codex-work".into(),
                type_id: "codex-subscription".into(),
                provider_id: "openai-codex".into(),
                label: "Codex work".into(),
                auth_kind: ConnectionAuthKind::Subscription,
                env_var: None,
                secret_ref: ConnectionSecretRef::Delegated {
                    provider: "openai-codex".into(),
                    profile: Some("work".into()),
                },
                placement: ConnectionPlacement::Local,
                state: ConnectionState::Active,
                capabilities: vec!["models.invoke".into()],
                mcp_binding: None,
                owner: None,
                generation: 1,
                is_default: true,
                created_at_ms: 10,
                updated_at_ms: 10,
            }],
        };
        assert_eq!(
            store
                .delegated_profile("openai-codex", None)
                .unwrap()
                .as_deref(),
            Some("work")
        );
        let serialized = serde_json::to_string(&store).unwrap();
        assert!(!serialized.contains("access_token"));
    }

    #[test]
    fn codex_app_server_connection_never_enters_direct_client_environment() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("connections.json");
        let mut env = HashMap::from([("MAESTRO_CONNECTION".into(), "codex-personal".into())]);
        assert!(
            ConnectionBroker::<KeyringSecretBackend>::merge_persisted_for_model(
                &path,
                "openai-codex/gpt-5.6-sol",
                &mut env
            )
            .unwrap()
            .is_none()
        );
        assert_eq!(env.len(), 1);
    }

    #[test]
    fn codex_connection_owner_rejects_another_user_or_organization() {
        let owner = ConnectionOwner {
            organization_id: "org_a".into(),
            user_id: "user_a".into(),
        };
        assert!(owner.matches("org_a", Some("user_a")));
        assert!(!owner.matches("org_a", Some("user_b")));
        assert!(!owner.matches("org_b", Some("user_a")));
        assert!(!owner.matches("org_a", None));
    }

    #[test]
    fn cli_subscription_metadata_is_owned_and_contains_no_vendor_token() {
        for (provider, type_id) in [
            ("claude-code", "claude-subscription"),
            ("github-copilot", "copilot-subscription"),
        ] {
            let mut linked = connection();
            linked.type_id = type_id.into();
            linked.provider_id = provider.into();
            linked.auth_kind = ConnectionAuthKind::Subscription;
            linked.env_var = None;
            linked.secret_ref = ConnectionSecretRef::Delegated {
                provider: provider.into(),
                profile: None,
            };
            linked.owner = Some(ConnectionOwner {
                organization_id: "org-one".into(),
                user_id: "user-one".into(),
            });
            linked.capabilities = vec!["models.invoke".into()];
            linked.validate().unwrap();
            let encoded = serde_json::to_string(&linked).unwrap();
            assert!(encoded.contains("user-one"));
            assert!(!encoded.contains("token"));
            linked.owner = None;
            assert!(linked.validate().is_err());
        }
    }

    #[test]
    fn default_delegated_auth_skips_a_malformed_optional_store() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("connections.json");
        fs::write(&path, "not-json").unwrap();

        assert_eq!(
            selected_delegated_profile_from_store("openai-codex", None, &path).unwrap(),
            None
        );
    }

    #[test]
    fn explicit_delegated_connection_still_validates_the_optional_store() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("connections.json");
        fs::write(&path, "not-json").unwrap();

        let error =
            selected_delegated_profile_from_store("openai-codex", Some("codex-work"), &path)
                .unwrap_err();
        assert!(error.to_string().contains("invalid connections.json"));
    }

    #[test]
    fn delegated_connection_requires_a_supported_provider_transport() {
        let mut unsupported = connection();
        unsupported.provider_id = "vendor-plugin".into();
        unsupported.auth_kind = ConnectionAuthKind::OAuth;
        unsupported.env_var = None;
        unsupported.secret_ref = ConnectionSecretRef::Delegated {
            provider: "vendor-plugin".into(),
            profile: Some("work".into()),
        };
        let broker = ConnectionBroker::new(
            ConnectionStore {
                schema_version: 1,
                connections: vec![unsupported],
            },
            MemorySecrets::default(),
        );

        let error = broker
            .check("openai-personal", &HashMap::new())
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unsupported for provider vendor-plugin")
        );
    }

    fn one_password_connection() -> ServiceConnection {
        let mut value = connection();
        value.secret_ref = ConnectionSecretRef::OnePassword {
            reference: "op://vault/item/key".to_owned(),
        };
        value.capabilities = vec!["models.invoke".to_owned()];
        value
    }

    #[test]
    fn one_password_broker_resolves_only_at_use_and_honors_removal() {
        let _fake = crate::ai::op_secret::test_support::FakeOp::install();
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("connections.json");
        let value = one_password_connection();
        let mut store = ConnectionStore {
            schema_version: 1,
            connections: vec![value.clone()],
        };
        store.save(&path).unwrap();
        let broker =
            ConnectionBroker::new_persisted(store.clone(), MemorySecrets::default(), path.clone());
        let lease = broker
            .issue_lease(
                &value.id,
                "native-model-client",
                vec!["models.invoke".into()],
                vec![one_password_model_destination(&value).unwrap().into()],
                100,
                1000,
            )
            .unwrap();
        let encoded = serde_json::to_string(&lease).unwrap();
        assert!(!encoded.contains("op://"));
        assert!(!encoded.contains("resolved-secret-value"));
        let mut env = HashMap::new();
        let receipt = broker
            .apply_lease_to_env(&lease, "native-model-client", &mut env, 1050)
            .unwrap();
        assert_eq!(env["OPENAI_API_KEY"], "resolved-secret-value");
        assert!(
            !serde_json::to_string(&receipt)
                .unwrap()
                .contains("resolved-secret-value")
        );
        assert!(
            !fs::read_to_string(&path)
                .unwrap()
                .contains("resolved-secret-value")
        );
        store.remove(&value.id);
        store.save(&path).unwrap();
        let mut next_client = HashMap::new();
        assert!(
            broker
                .apply_lease_to_env(&lease, "native-model-client", &mut next_client, 1050)
                .is_err()
        );
        assert!(next_client.is_empty());
    }

    #[test]
    fn one_password_requires_explicit_selection_even_after_default_removal() {
        let mut selected = connection();
        selected.is_default = true;
        let mut opt_in = one_password_connection();
        opt_in.id = "one-password-work".into();
        opt_in.is_default = false;
        let mut store = ConnectionStore {
            schema_version: 1,
            connections: vec![selected.clone(), opt_in.clone()],
        };
        store.remove(&selected.id);
        assert!(!store.get(&opt_in.id).unwrap().is_default);
        assert!(store.selected("openai", None).unwrap().is_none());
        assert!(
            store
                .selected("openai", Some(&opt_in.id))
                .unwrap()
                .is_some()
        );
        store.set_default(&opt_in.id).unwrap();
        assert!(store.selected("openai", None).unwrap().is_some());
    }

    #[test]
    fn one_password_does_not_turn_arbitrary_scopes_into_password_access() {
        let value = one_password_connection();
        let broker = ConnectionBroker::new(
            ConnectionStore {
                schema_version: 1,
                connections: vec![value.clone()],
            },
            MemorySecrets::default(),
        );
        for (audience, capabilities, resources) in [
            (
                "shell",
                vec!["models.invoke".into()],
                vec!["https://api.openai.com/v1".into()],
            ),
            ("native-model-client", vec!["models.invoke".into()], vec![]),
            (
                "native-model-client",
                vec!["models.invoke".into()],
                vec!["https://attacker.invalid".into()],
            ),
        ] {
            assert!(
                broker
                    .issue_lease(&value.id, audience, capabilities, resources, 100, 1000)
                    .is_err()
            );
        }
        let mut invalid = value;
        invalid.capabilities.push("passwords.read".into());
        assert!(one_password_model_destination(&invalid).is_err());
    }

    #[test]
    fn one_password_rejects_endpoint_redirection_before_resolving() {
        let value = one_password_connection();
        let env = HashMap::from([(
            "OPENAI_BASE_URL".to_owned(),
            "https://attacker.invalid".to_owned(),
        )]);
        assert!(validate_one_password_client(&value, &env).is_err());
        assert!(validate_one_password_client(&value, &HashMap::new()).is_ok());
    }

    #[test]
    fn lease_enforces_capability_audience_expiry_and_generation() {
        let backend = MemorySecrets::default();
        backend
            .set(
                KEYRING_SERVICE,
                &keyring_account("openai-personal", 1),
                "super-secret",
            )
            .unwrap();
        let store = ConnectionStore {
            schema_version: 1,
            connections: vec![connection()],
        };
        let broker = ConnectionBroker::new(store, backend);
        assert!(
            broker
                .issue_lease(
                    "openai-personal",
                    "agent-1",
                    vec!["admin".into()],
                    vec![],
                    100,
                    1_000,
                )
                .is_err()
        );
        let lease = broker
            .issue_lease(
                "openai-personal",
                "agent-1",
                vec!["responses.create".into()],
                vec!["project:demo".into()],
                100,
                1_000,
            )
            .unwrap();
        let mut env = HashMap::new();
        assert!(
            broker
                .apply_lease_to_env(&lease, "agent-2", &mut env, 1_050)
                .is_err()
        );
        let mut modified = lease.clone();
        modified.capabilities = vec!["models.read".into()];
        assert!(
            broker
                .apply_lease_to_env(&modified, "agent-1", &mut env, 1_050)
                .unwrap_err()
                .to_string()
                .contains("unknown or has been modified")
        );
        assert!(
            broker
                .apply_lease_to_env(&lease, "agent-1", &mut env, 1_101)
                .is_err()
        );
        let receipt = broker
            .apply_lease_to_env(&lease, "agent-1", &mut env, 1_050)
            .unwrap();
        assert_eq!(receipt.outcome, "injected_into_client_scope");
        assert!(
            !serde_json::to_string(&receipt)
                .unwrap()
                .contains("super-secret")
        );
    }

    #[test]
    fn persisted_rotation_revokes_leases_in_a_long_lived_broker() {
        let home = TempDir::new().unwrap();
        let store_path = home.path().join("connections.json");
        let initial_store = ConnectionStore {
            schema_version: 1,
            connections: vec![connection()],
        };
        initial_store.save(&store_path).unwrap();
        let backend = MemorySecrets::default();
        backend
            .set(
                KEYRING_SERVICE,
                &keyring_account("openai-personal", 1),
                "generation-one",
            )
            .unwrap();
        let broker = ConnectionBroker::new_persisted(initial_store, backend, store_path.clone());
        let lease = broker
            .issue_lease(
                "openai-personal",
                "agent-1",
                vec!["responses.create".into()],
                vec![],
                100,
                1_000,
            )
            .unwrap();

        let mut rotated_store = ConnectionStore::load(&store_path).unwrap();
        rotated_store.connections[0].generation = 2;
        rotated_store.connections[0].secret_ref = keyring_secret_ref("openai-personal", 2);
        rotated_store.save(&store_path).unwrap();

        let error = broker
            .apply_lease_to_env(&lease, "agent-1", &mut HashMap::new(), 1_050)
            .unwrap_err();
        assert!(error.to_string().contains("stale or revoked"));
        assert_eq!(
            broker
                .issue_lease(
                    "openai-personal",
                    "agent-1",
                    vec!["responses.create".into()],
                    vec![],
                    100,
                    1_050,
                )
                .unwrap()
                .generation,
            2
        );
    }

    #[test]
    fn persisted_capability_reduction_revokes_an_existing_lease() {
        let home = TempDir::new().unwrap();
        let store_path = home.path().join("connections.json");
        let initial_store = ConnectionStore {
            schema_version: 1,
            connections: vec![connection()],
        };
        initial_store.save(&store_path).unwrap();
        let backend = MemorySecrets::default();
        backend
            .set(
                KEYRING_SERVICE,
                &keyring_account("openai-personal", 1),
                "generation-one",
            )
            .unwrap();
        let broker = ConnectionBroker::new_persisted(initial_store, backend, store_path.clone());
        let lease = broker
            .issue_lease(
                "openai-personal",
                "agent-1",
                vec!["responses.create".into()],
                vec![],
                100,
                1_000,
            )
            .unwrap();

        let mut reduced_store = ConnectionStore::load(&store_path).unwrap();
        reduced_store.connections[0].capabilities = vec!["models.read".into()];
        reduced_store.save(&store_path).unwrap();

        let error = broker
            .apply_lease_to_env(&lease, "agent-1", &mut HashMap::new(), 1_050)
            .unwrap_err();
        assert!(error.to_string().contains("authority is no longer allowed"));
    }
}
