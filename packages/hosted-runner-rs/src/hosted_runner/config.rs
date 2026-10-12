use std::collections::HashMap;
use std::fs;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};

use maestro_runtime::{
    HostedLaunchRendezvousMode, HostedLaunchSpec, HostedRuntimeAuthMode, HostedRuntimeBoundary,
    HostedRuntimeBoundaryInput, MAX_RUNTIME_RECEIPT_STRING_BYTES,
};

use super::env_aliases::{DeprecatedEnvAlias, deprecated_env_aliases};
use super::manifests::SnapshotManifest;
use super::rendezvous_protocol::{RendezvousMode, RendezvousNonce};

const DEFAULT_ENV_LISTEN_HOST: &str = "0.0.0.0";
const DEFAULT_PROGRAMMATIC_LISTEN_HOST: &str = "127.0.0.1";
const DEFAULT_LISTEN_PORT: u16 = 8080;
const MAX_CAUSAL_RECEIPT_ID_BYTES: usize = 128;
const MAX_HOSTED_SECRET_FILE_BYTES: usize = 16 * 1024;
const MAX_HOSTED_CA_FILE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct HostedRunnerConfig {
    pub runner_session_id: String,
    pub workspace_root: PathBuf,
    pub bind_addr: SocketAddr,
    pub runtime_generation: u64,
    pub owner_instance_id: Option<String>,
    pub snapshot_root: Option<PathBuf>,
    pub restore_manifest_path: Option<PathBuf>,
    pub workspace_id: Option<String>,
    pub agent_run_id: Option<String>,
    pub maestro_session_id: Option<String>,
    pub causal_receipt_id: Option<String>,
    pub attach_audience: Option<String>,
    pub auth_token: Option<String>,
    pub workload_identity: Option<HostedRunnerWorkloadIdentityConfig>,
    pub rendezvous: Option<HostedRunnerRendezvousConfig>,
    /// Legacy environment keys that supplied a value for this config.
    pub deprecated_env_aliases: Vec<DeprecatedEnvAlias>,
}

#[derive(Debug, Clone)]
pub struct HostedRunnerWorkloadIdentityConfig {
    pub kubernetes_token_file: PathBuf,
    pub identity_tls_ca_file: PathBuf,
    pub identity_exchange_url: url::Url,
    pub organization_id: String,
    pub workspace_id: String,
    pub sandbox_id: uuid::Uuid,
    pub placement_generation: u64,
}

#[derive(Debug, Clone)]
pub struct HostedRunnerRendezvousConfig {
    pub mode: RendezvousMode,
    pub endpoint: String,
    pub server_name: String,
    pub identity_exchange_url: url::Url,
    pub activation_id: uuid::Uuid,
    pub nonce: RendezvousNonce,
}

impl HostedRunnerConfig {
    pub(super) fn validate_live_runtime_receipt_binding(
        model: &str,
        provider: &str,
    ) -> Result<(), HostedRunnerConfigError> {
        for (field, value) in [("model", model), ("provider", provider)] {
            validate_runtime_receipt_string(field, value)?;
        }
        Ok(())
    }

    pub(super) fn validate_runtime_receipt_identity(
        &self,
        restore_manifest: Option<&SnapshotManifest>,
    ) -> Result<(), HostedRunnerConfigError> {
        let maestro_session_id = self
            .maestro_session_id
            .as_deref()
            .or_else(|| restore_manifest.map(|manifest| manifest.maestro_session_id.as_str()))
            .unwrap_or(self.runner_session_id.as_str());
        let workspace_id = self
            .workload_identity
            .as_ref()
            .map(|identity| identity.workspace_id.as_str())
            .or(self.workspace_id.as_deref());
        for (field, value) in [
            ("runner_session_id", self.runner_session_id.as_str()),
            ("maestro_session_id", maestro_session_id),
        ]
        .into_iter()
        .chain(workspace_id.map(|value| ("workspace_id", value)))
        .chain(
            self.agent_run_id
                .as_deref()
                .map(|value| ("agent_run_id", value)),
        ) {
            validate_runtime_receipt_string(field, value)?;
        }
        if let Some(manifest) = restore_manifest {
            for (field, value) in [
                ("model", manifest.snapshot.state.model.as_deref()),
                ("provider", manifest.snapshot.state.provider.as_deref()),
                (
                    "last_error_type",
                    manifest.snapshot.state.last_error_type.as_deref(),
                ),
            ] {
                if let Some(value) = value {
                    validate_runtime_receipt_string(field, value)?;
                }
            }
        }
        Ok(())
    }

    pub fn from_env() -> Result<Self, HostedRunnerConfigError> {
        let env = std::env::vars().collect::<HashMap<_, _>>();
        Self::from_env_map(&env)
    }

    pub fn from_env_map(env: &HashMap<String, String>) -> Result<Self, HostedRunnerConfigError> {
        let deprecated_env_aliases = deprecated_env_aliases(env);
        let runner_session_id = first_env(
            env,
            &["MAESTRO_RUNNER_SESSION_ID", "REMOTE_RUNNER_SESSION_ID"],
        )
        .ok_or_else(|| {
            HostedRunnerConfigError::new(
                "deixic-code hosted-runner requires MAESTRO_RUNNER_SESSION_ID",
            )
        })?;
        let workspace_root = resolve_config_workspace_root(
            first_env(env, &["MAESTRO_WORKSPACE_ROOT", "WORKSPACE_ROOT"]).as_deref(),
        )?;
        let listen = parse_listen(env_value(env, "MAESTRO_HOSTED_RUNNER_LISTEN").as_deref())?;
        let hosted_runner_port =
            parse_optional_port(env_value(env, "MAESTRO_HOSTED_RUNNER_PORT").as_deref())
                .transpose()?;
        let port_env = parse_optional_port(env_value(env, "PORT").as_deref()).transpose()?;
        let port = listen
            .port
            .or(hosted_runner_port)
            .or(port_env)
            .unwrap_or(DEFAULT_LISTEN_PORT);
        let host = listen
            .host
            .or_else(|| env_value(env, "MAESTRO_HOSTED_RUNNER_HOST"))
            .unwrap_or_else(|| DEFAULT_ENV_LISTEN_HOST.to_string());
        let bind_addr = resolve_bind_addr(&host, port)?;
        let inline_auth_token = first_env(
            env,
            &["MAESTRO_HOSTED_RUNNER_AUTH_TOKEN", "MAESTRO_WEB_API_KEY"],
        );
        let auth_token_file = first_env(
            env,
            &[
                "MAESTRO_HOSTED_RUNNER_AUTH_TOKEN_FILE",
                "MAESTRO_WEB_API_KEY_FILE",
            ],
        );
        if inline_auth_token.is_some() && auth_token_file.is_some() {
            return Err(HostedRunnerConfigError::new(
                "hosted runner static bearer authentication must use either an inline value or a secret file reference, not both",
            ));
        }
        let auth_token = auth_token_file
            .as_deref()
            .map(|path| read_secret_file(path, "hosted runner auth token"))
            .transpose()?
            .or(inline_auth_token);
        let causal_receipt_id = parse_causal_receipt_id(env)?;
        let workload_identity = parse_workload_identity(env)?;
        let rendezvous = parse_rendezvous(env, workload_identity.is_some())?;
        if workload_identity.is_some() && auth_token.is_some() {
            return Err(HostedRunnerConfigError::new(
                "projected workload identity mode forbids static bearer authentication",
            ));
        }
        if !bind_addr.ip().is_loopback() && auth_token.is_none() && workload_identity.is_none() {
            return Err(HostedRunnerConfigError::new(
                "deixic-code hosted-runner requires MAESTRO_HOSTED_RUNNER_AUTH_TOKEN or MAESTRO_WEB_API_KEY when binding to non-loopback interfaces",
            ));
        }
        let snapshot_root = resolve_snapshot_root(
            first_env(
                env,
                &[
                    "MAESTRO_REMOTE_RUNNER_SNAPSHOT_ROOT",
                    "REMOTE_RUNNER_SNAPSHOT_ROOT",
                ],
            )
            .as_deref(),
            &workspace_root,
        );
        let restore_manifest_path = resolve_optional_config_path(
            first_env(
                env,
                &[
                    "MAESTRO_REMOTE_RUNNER_RESTORE_MANIFEST",
                    "REMOTE_RUNNER_RESTORE_MANIFEST",
                ],
            )
            .as_deref(),
            &workspace_root,
        );
        let runtime_generation = first_env(
            env,
            &[
                "MAESTRO_PLACEMENT_GENERATION",
                "MAESTRO_SANDBOXWICH_PLACEMENT_GENERATION",
                "MAESTRO_REMOTE_RUNNER_GENERATION",
            ],
        )
        .map(|value| {
            value.parse::<u64>().map_err(|_| {
                HostedRunnerConfigError::new(
                    "hosted runner runtime generation must be an unsigned integer",
                )
            })
        })
        .transpose()?
        .unwrap_or(0);

        let config = Self {
            runner_session_id: non_empty(runner_session_id, "runner_session_id")?,
            workspace_root,
            bind_addr,
            runtime_generation,
            owner_instance_id: first_env(
                env,
                &[
                    "MAESTRO_REMOTE_RUNNER_OWNER_INSTANCE_ID",
                    "REMOTE_RUNNER_OWNER_INSTANCE_ID",
                ],
            ),
            snapshot_root: Some(snapshot_root),
            restore_manifest_path,
            workspace_id: first_env(
                env,
                &["MAESTRO_REMOTE_RUNNER_WORKSPACE_ID", "MAESTRO_WORKSPACE_ID"],
            ),
            agent_run_id: env_value(env, "MAESTRO_AGENT_RUN_ID"),
            maestro_session_id: env_value(env, "MAESTRO_SESSION_ID"),
            causal_receipt_id,
            attach_audience: env_value(env, "MAESTRO_ATTACH_AUDIENCE"),
            auth_token,
            workload_identity,
            rendezvous,
            deprecated_env_aliases,
        };
        config.validate_runtime_receipt_identity(None)?;
        Ok(config)
    }

    /// Converts an already decoded, executable launch descriptor into the
    /// existing hosted-runner configuration without changing listener or child
    /// ownership. All referenced secret files are bounded-read before the
    /// caller can bind the listener; their contents are retained only in the
    /// existing private config fields needed by the runner.
    pub fn from_launch_spec(spec: &HostedLaunchSpec) -> Result<Self, HostedRunnerConfigError> {
        spec.validate_for_launch().map_err(|error| {
            HostedRunnerConfigError::new(format!("invalid hosted launch spec: {error}"))
        })?;
        let bind_addr = spec
            .runtime
            .bind_address
            .parse::<SocketAddr>()
            .map_err(|_| {
                HostedRunnerConfigError::new(
                    "hosted launch spec runtime.bindAddress must be an IP socket address",
                )
            })?;
        let workspace_root = resolve_config_workspace_root(Some(&spec.workspace.root))?;
        let snapshot_root = spec.restore.snapshot_root.as_deref().map(PathBuf::from);
        let restore_manifest_path = spec
            .restore
            .restore_manifest_path
            .as_deref()
            .map(PathBuf::from);
        let maestro_session_id = spec
            .workspace
            .maestro_session_id
            .as_deref()
            .map(|value| non_empty(value.to_string(), "workspace.maestroSessionId"))
            .transpose()?;
        let workspace_id = spec
            .workspace
            .workspace_id
            .as_deref()
            .map(|value| non_empty(value.to_string(), "workspace.workspaceId"))
            .transpose()?;
        let owner_instance_id = spec
            .runtime
            .owner_instance_id
            .as_deref()
            .and_then(trimmed_optional);
        let agent_run_id = spec
            .workspace
            .agent_run_id
            .as_deref()
            .and_then(trimmed_optional);
        let attach_audience = spec
            .runtime
            .attach_audience
            .as_deref()
            .and_then(trimmed_optional);
        let causal_receipt_id = spec
            .runtime
            .causal_receipt_id
            .as_deref()
            .map(validate_causal_receipt_id)
            .transpose()?;

        let auth_token = match spec.identity.auth_mode {
            HostedRuntimeAuthMode::None | HostedRuntimeAuthMode::WorkloadIdentity => None,
            HostedRuntimeAuthMode::StaticBearer => {
                let path = spec.secret_files.static_bearer.as_deref().ok_or_else(|| {
                    HostedRunnerConfigError::new(
                        "static bearer launch spec requires secretFiles.staticBearer",
                    )
                })?;
                Some(read_secret_file(path, "hosted runner auth token")?)
            }
        };
        let workload_identity = match spec.identity.workload_identity.as_ref() {
            Some(workload) => {
                let token_path = PathBuf::from(&workload.kubernetes_token_file);
                let ca_path = PathBuf::from(&workload.identity_tls_ca_file);
                let projected_token = spec
                    .secret_files
                    .projected_workload_token
                    .as_deref()
                    .ok_or_else(|| {
                        HostedRunnerConfigError::new(
                            "workload launch spec requires secretFiles.projectedWorkloadToken",
                        )
                    })?;
                let identity_tls_ca =
                    spec.secret_files
                        .identity_tls_ca
                        .as_deref()
                        .ok_or_else(|| {
                            HostedRunnerConfigError::new(
                                "workload launch spec requires secretFiles.identityTlsCa",
                            )
                        })?;
                if projected_token != workload.kubernetes_token_file
                    || identity_tls_ca != workload.identity_tls_ca_file
                {
                    return Err(HostedRunnerConfigError::new(
                        "workload launch spec secret-file references do not match identity coordinates",
                    ));
                }
                let _ = read_secret_file(projected_token, "projected workload token")?;
                let _ = read_bounded_file(
                    identity_tls_ca,
                    "identity exchange CA",
                    MAX_HOSTED_CA_FILE_BYTES,
                    false,
                )?;
                Some(HostedRunnerWorkloadIdentityConfig {
                    kubernetes_token_file: token_path,
                    identity_tls_ca_file: ca_path,
                    identity_exchange_url: parse_https_url(
                        &workload.identity_exchange_url,
                        "identity.workloadIdentity.identityExchangeUrl",
                    )?,
                    organization_id: non_empty(
                        workload.organization_id.clone(),
                        "identity.organization_id",
                    )?,
                    workspace_id: non_empty(
                        workload.workspace_id.clone(),
                        "identity.workspace_id",
                    )?,
                    sandbox_id: workload.sandbox_id.parse().map_err(|_| {
                        HostedRunnerConfigError::new(
                            "identity.workloadIdentity.sandboxId must be a UUID",
                        )
                    })?,
                    placement_generation: positive_generation(workload.placement_generation)?,
                })
            }
            None => None,
        };
        if auth_token.is_some() && workload_identity.is_some() {
            return Err(HostedRunnerConfigError::new(
                "hosted launch spec cannot combine static bearer and workload identity",
            ));
        }

        let rendezvous = match spec.rendezvous.as_ref() {
            Some(rendezvous) if matches!(rendezvous.mode, HostedLaunchRendezvousMode::Inbound) => {
                None
            }
            Some(rendezvous) => Some(rendezvous_from_launch_spec(rendezvous)?),
            None => None,
        };

        if let Some(path) = spec.secret_files.managed_gateway_access_token.as_deref() {
            let _ = read_secret_file(path, "managed gateway access token")?;
        }
        if !bind_addr.ip().is_loopback() && auth_token.is_none() && workload_identity.is_none() {
            return Err(HostedRunnerConfigError::new(
                "hosted launch spec requires static bearer or workload identity when binding to non-loopback interfaces",
            ));
        }

        let config = Self {
            runner_session_id: non_empty(
                spec.runtime.runner_session_id.clone(),
                "runner_session_id",
            )?,
            workspace_root,
            bind_addr,
            runtime_generation: spec.runtime.runtime_generation,
            owner_instance_id,
            snapshot_root,
            restore_manifest_path,
            workspace_id,
            agent_run_id,
            maestro_session_id,
            causal_receipt_id,
            attach_audience,
            auth_token,
            workload_identity,
            rendezvous,
            deprecated_env_aliases: Vec::new(),
        };
        config.validate_runtime_receipt_identity(None)?;
        Ok(config)
    }

    pub fn new(
        runner_session_id: impl Into<String>,
        workspace_root: impl AsRef<Path>,
    ) -> Result<Self, HostedRunnerConfigError> {
        let config = Self {
            runner_session_id: non_empty(runner_session_id.into(), "runner_session_id")?,
            workspace_root: resolve_config_workspace_root(Some(path_to_str(
                workspace_root.as_ref(),
            )?))?,
            bind_addr: format!("{DEFAULT_PROGRAMMATIC_LISTEN_HOST}:{DEFAULT_LISTEN_PORT}")
                .parse()
                .expect("default hosted runner bind address is valid"),
            runtime_generation: 0,
            owner_instance_id: None,
            snapshot_root: None,
            restore_manifest_path: None,
            workspace_id: None,
            agent_run_id: None,
            maestro_session_id: None,
            causal_receipt_id: None,
            attach_audience: None,
            auth_token: None,
            workload_identity: None,
            rendezvous: None,
            deprecated_env_aliases: Vec::new(),
        };
        config.validate_runtime_receipt_identity(None)?;
        Ok(config)
    }

    #[must_use]
    pub fn with_bind_addr(mut self, bind_addr: SocketAddr) -> Self {
        self.bind_addr = bind_addr;
        self
    }

    #[must_use]
    pub fn with_runtime_generation(mut self, runtime_generation: u64) -> Self {
        self.runtime_generation = runtime_generation;
        self
    }

    #[must_use]
    pub fn with_auth_token(mut self, auth_token: impl Into<String>) -> Self {
        self.auth_token = Some(auth_token.into());
        self
    }

    #[must_use]
    pub fn with_owner_instance_id(mut self, owner_instance_id: impl Into<String>) -> Self {
        self.owner_instance_id = Some(owner_instance_id.into());
        self
    }

    #[must_use]
    pub fn with_snapshot_root(mut self, snapshot_root: impl Into<PathBuf>) -> Self {
        self.snapshot_root = Some(snapshot_root.into());
        self
    }

    #[must_use]
    pub fn with_restore_manifest_path(mut self, restore_manifest_path: impl Into<PathBuf>) -> Self {
        self.restore_manifest_path = Some(restore_manifest_path.into());
        self
    }

    #[must_use]
    pub fn with_workspace_id(mut self, workspace_id: impl Into<String>) -> Self {
        self.workspace_id = Some(workspace_id.into());
        self
    }

    #[must_use]
    pub fn with_agent_run_id(mut self, agent_run_id: impl Into<String>) -> Self {
        self.agent_run_id = Some(agent_run_id.into());
        self
    }

    #[must_use]
    pub fn with_maestro_session_id(mut self, maestro_session_id: impl Into<String>) -> Self {
        self.maestro_session_id = Some(maestro_session_id.into());
        self
    }

    /// Derives the transport-neutral, pre-start identity snapshot for this
    /// hosted-runner configuration.
    ///
    /// The snapshot is configuration identity only: it does not observe a
    /// bound listener or active session. A requested port `0` and a restored or
    /// fallback session identity are not authoritative runtime observations,
    /// and listener/child-process ownership remains with the hosted runner.
    /// Authentication is represented by mode, never by secret material or
    /// projected-token paths.
    ///
    /// # Errors
    ///
    /// Returns an error when static bearer and workload-identity
    /// authentication are both configured, or when the runtime boundary
    /// contains invalid required identity fields.
    pub fn runtime_boundary(&self) -> Result<HostedRuntimeBoundary, HostedRunnerConfigError> {
        let auth_mode = match (&self.auth_token, &self.workload_identity) {
            (Some(_), None) => HostedRuntimeAuthMode::StaticBearer,
            (None, Some(_)) => HostedRuntimeAuthMode::WorkloadIdentity,
            (None, None) => HostedRuntimeAuthMode::None,
            (Some(_), Some(_)) => {
                return Err(HostedRunnerConfigError::new(
                    "hosted runner cannot use static bearer authentication with workload identity",
                ));
            }
        };
        HostedRuntimeBoundary::new(HostedRuntimeBoundaryInput {
            runner_session_id: self.runner_session_id.clone(),
            workspace_root: self.workspace_root.to_string_lossy().into_owned(),
            bind_address: self.bind_addr.to_string(),
            runtime_generation: self.runtime_generation,
            owner_instance_id: self.owner_instance_id.clone(),
            workspace_id: self.workspace_id.clone(),
            agent_run_id: self.agent_run_id.clone(),
            maestro_session_id: self.maestro_session_id.clone(),
            causal_receipt_id: self.causal_receipt_id.clone(),
            attach_audience: self.attach_audience.clone(),
            auth_mode,
        })
        .map_err(|error| HostedRunnerConfigError::new(format!("invalid runtime boundary: {error}")))
    }
}

fn parse_rendezvous(
    env: &HashMap<String, String>,
    has_workload_identity: bool,
) -> Result<Option<HostedRunnerRendezvousConfig>, HostedRunnerConfigError> {
    let Some(mode) = env_value(env, "MAESTRO_RENDEZVOUS_MODE") else {
        return Ok(None);
    };
    let mode = match mode.as_str() {
        "inbound" => return Ok(None),
        "outbound_shadow" => RendezvousMode::OutboundShadow,
        "outbound" => RendezvousMode::Outbound,
        _ => {
            return Err(HostedRunnerConfigError::new(
                "MAESTRO_RENDEZVOUS_MODE must be inbound, outbound_shadow, or outbound",
            ));
        }
    };
    if !has_workload_identity {
        return Err(HostedRunnerConfigError::new(
            "outbound rendezvous requires projected workload identity",
        ));
    }
    if mode == RendezvousMode::Outbound
        && env_value(env, "MAESTRO_RENDEZVOUS_OUTBOUND_PREFER").as_deref() != Some("true")
    {
        return Err(HostedRunnerConfigError::new(
            "outbound rendezvous authority requires MAESTRO_RENDEZVOUS_OUTBOUND_PREFER=true",
        ));
    }
    let required = |key: &'static str| {
        env_value(env, key)
            .ok_or_else(|| HostedRunnerConfigError::new(format!("rendezvous requires {key}")))
    };
    let endpoint = required("MAESTRO_RENDEZVOUS_ENDPOINT")?;
    if endpoint.len() > 512 || endpoint.rsplit_once(':').is_none() {
        return Err(HostedRunnerConfigError::new(
            "MAESTRO_RENDEZVOUS_ENDPOINT must be a bounded host:port",
        ));
    }
    let server_name = required("MAESTRO_RENDEZVOUS_SERVER_NAME")?;
    rustls::pki_types::ServerName::try_from(server_name.clone()).map_err(|_| {
        HostedRunnerConfigError::new("MAESTRO_RENDEZVOUS_SERVER_NAME must be a valid DNS name")
    })?;
    let identity_exchange_url = required("MAESTRO_RENDEZVOUS_IDENTITY_EXCHANGE_URL")?
        .parse::<url::Url>()
        .map_err(|_| {
            HostedRunnerConfigError::new(
                "MAESTRO_RENDEZVOUS_IDENTITY_EXCHANGE_URL must be a valid HTTPS URL",
            )
        })?;
    if identity_exchange_url.scheme() != "https"
        || identity_exchange_url.host_str().is_none()
        || identity_exchange_url.username() != ""
        || identity_exchange_url.password().is_some()
    {
        return Err(HostedRunnerConfigError::new(
            "MAESTRO_RENDEZVOUS_IDENTITY_EXCHANGE_URL must be HTTPS without userinfo",
        ));
    }
    let activation_id = required("MAESTRO_RENDEZVOUS_ACTIVATION_ID")?
        .parse()
        .map_err(|_| {
            HostedRunnerConfigError::new("MAESTRO_RENDEZVOUS_ACTIVATION_ID must be a UUID")
        })?;
    let nonce_file = env_value(env, "MAESTRO_RENDEZVOUS_NONCE_FILE");
    let inline_nonce = env_value(env, "MAESTRO_RENDEZVOUS_NONCE");
    if nonce_file.is_some() && inline_nonce.is_some() {
        return Err(HostedRunnerConfigError::new(
            "rendezvous bootstrap nonce must use either an inline value or a secret file reference, not both",
        ));
    }
    let nonce = if let Some(path) = nonce_file.as_deref() {
        RendezvousNonce::parse(read_secret_file(path, "rendezvous bootstrap nonce")?)
            .map_err(|error| HostedRunnerConfigError::new(error.to_string()))?
    } else {
        RendezvousNonce::parse(required("MAESTRO_RENDEZVOUS_NONCE")?)
            .map_err(|error| HostedRunnerConfigError::new(error.to_string()))?
    };
    Ok(Some(HostedRunnerRendezvousConfig {
        mode,
        endpoint,
        server_name,
        identity_exchange_url,
        activation_id,
        nonce,
    }))
}

fn rendezvous_from_launch_spec(
    rendezvous: &maestro_runtime::HostedLaunchRendezvous,
) -> Result<HostedRunnerRendezvousConfig, HostedRunnerConfigError> {
    if rendezvous.endpoint.len() > 512 || rendezvous.endpoint.rsplit_once(':').is_none() {
        return Err(HostedRunnerConfigError::new(
            "rendezvous.endpoint must be a bounded host:port",
        ));
    }
    rustls::pki_types::ServerName::try_from(rendezvous.server_name.clone()).map_err(|_| {
        HostedRunnerConfigError::new("rendezvous.serverName must be a valid DNS name")
    })?;
    let identity_exchange_url = parse_https_url(
        &rendezvous.identity_exchange_url,
        "rendezvous.identityExchangeUrl",
    )?;
    let activation_id = rendezvous
        .activation_id
        .parse()
        .map_err(|_| HostedRunnerConfigError::new("rendezvous.activationId must be a UUID"))?;
    let mode = match rendezvous.mode {
        HostedLaunchRendezvousMode::Inbound => RendezvousMode::Inbound,
        HostedLaunchRendezvousMode::OutboundShadow => RendezvousMode::OutboundShadow,
        HostedLaunchRendezvousMode::Outbound => RendezvousMode::Outbound,
    };
    let nonce_path = rendezvous
        .nonce_file
        .as_deref()
        .ok_or_else(|| HostedRunnerConfigError::new("rendezvous requires nonceFile"))?;
    let nonce = RendezvousNonce::parse(read_secret_file(nonce_path, "rendezvous bootstrap nonce")?)
        .map_err(|error| HostedRunnerConfigError::new(error.to_string()))?;
    Ok(HostedRunnerRendezvousConfig {
        mode,
        endpoint: rendezvous.endpoint.clone(),
        server_name: rendezvous.server_name.clone(),
        identity_exchange_url,
        activation_id,
        nonce,
    })
}

fn parse_https_url(value: &str, field: &str) -> Result<url::Url, HostedRunnerConfigError> {
    let url = value
        .parse::<url::Url>()
        .map_err(|_| HostedRunnerConfigError::new(format!("{field} must be a valid HTTPS URL")))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(HostedRunnerConfigError::new(format!(
            "{field} must be HTTPS without userinfo"
        )));
    }
    Ok(url)
}

fn positive_generation(value: u64) -> Result<u64, HostedRunnerConfigError> {
    if value == 0 {
        return Err(HostedRunnerConfigError::new(
            "workload identity placement generation must be positive",
        ));
    }
    Ok(value)
}

fn parse_workload_identity(
    env: &HashMap<String, String>,
) -> Result<Option<HostedRunnerWorkloadIdentityConfig>, HostedRunnerConfigError> {
    if env.contains_key("MAESTRO_RUNNER_CLIENT_CA_FILE") {
        return Err(HostedRunnerConfigError::new(
            "MAESTRO_RUNNER_CLIENT_CA_FILE is not supported; Runner Host client trust comes from the authenticated Identity exchange response",
        ));
    }
    const IDENTITY_TRIGGER_KEYS: [&str; 6] = [
        "MAESTRO_KUBERNETES_TOKEN_FILE",
        "MAESTRO_IDENTITY_TLS_CA_FILE",
        "MAESTRO_IDENTITY_EXCHANGE_URL",
        "MAESTRO_ORGANIZATION_ID",
        "MAESTRO_SANDBOX_ID",
        "MAESTRO_PLACEMENT_GENERATION",
    ];
    if !IDENTITY_TRIGGER_KEYS
        .iter()
        .any(|key| env.contains_key(*key))
    {
        return Ok(None);
    }
    let required = |key: &'static str| {
        env_value(env, key).ok_or_else(|| {
            HostedRunnerConfigError::new(format!("projected workload identity requires {key}"))
        })
    };
    let kubernetes_token_file = PathBuf::from(required("MAESTRO_KUBERNETES_TOKEN_FILE")?);
    if !kubernetes_token_file.is_absolute() {
        return Err(HostedRunnerConfigError::new(
            "MAESTRO_KUBERNETES_TOKEN_FILE must be an absolute read-only projected-token path",
        ));
    }
    let identity_tls_ca_file = PathBuf::from(required("MAESTRO_IDENTITY_TLS_CA_FILE")?);
    if !identity_tls_ca_file.is_absolute() {
        return Err(HostedRunnerConfigError::new(
            "MAESTRO_IDENTITY_TLS_CA_FILE must be an absolute read-only CA path",
        ));
    }
    let identity_exchange_url = required("MAESTRO_IDENTITY_EXCHANGE_URL")?
        .parse::<url::Url>()
        .map_err(|_| {
            HostedRunnerConfigError::new("MAESTRO_IDENTITY_EXCHANGE_URL must be a valid HTTPS URL")
        })?;
    if identity_exchange_url.scheme() != "https"
        || identity_exchange_url.host_str().is_none()
        || identity_exchange_url.username() != ""
        || identity_exchange_url.password().is_some()
    {
        return Err(HostedRunnerConfigError::new(
            "MAESTRO_IDENTITY_EXCHANGE_URL must be a valid HTTPS URL without userinfo",
        ));
    }
    let sandbox_id = required("MAESTRO_SANDBOX_ID")?
        .parse()
        .map_err(|_| HostedRunnerConfigError::new("MAESTRO_SANDBOX_ID must be a UUID"))?;
    let placement_generation = required("MAESTRO_PLACEMENT_GENERATION")?
        .parse()
        .map_err(|_| {
            HostedRunnerConfigError::new("MAESTRO_PLACEMENT_GENERATION must be a positive integer")
        })?;
    if placement_generation == 0 {
        return Err(HostedRunnerConfigError::new(
            "MAESTRO_PLACEMENT_GENERATION must be a positive integer",
        ));
    }
    let _ = required("MAESTRO_RUNNER_SESSION_ID")?;
    Ok(Some(HostedRunnerWorkloadIdentityConfig {
        kubernetes_token_file,
        identity_tls_ca_file,
        identity_exchange_url,
        organization_id: required("MAESTRO_ORGANIZATION_ID")?,
        workspace_id: required("MAESTRO_WORKSPACE_ID")?,
        sandbox_id,
        placement_generation,
    }))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostedRunnerConfigError {
    message: String,
}

impl HostedRunnerConfigError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for HostedRunnerConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for HostedRunnerConfigError {}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedListen {
    host: Option<String>,
    port: Option<u16>,
}

fn first_env(env: &HashMap<String, String>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| env_value(env, key))
}

pub(super) fn env_value(env: &HashMap<String, String>, key: &str) -> Option<String> {
    env.get(key).map(|value| value.trim()).and_then(|value| {
        if value.is_empty() {
            None
        } else {
            Some(value.to_string())
        }
    })
}

fn trimmed_optional(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn parse_causal_receipt_id(
    env: &HashMap<String, String>,
) -> Result<Option<String>, HostedRunnerConfigError> {
    let Some(value) = env.get("MAESTRO_CAUSAL_RECEIPT_ID") else {
        return Ok(None);
    };
    validate_causal_receipt_id(value).map(Some)
}

fn validate_causal_receipt_id(value: &str) -> Result<String, HostedRunnerConfigError> {
    if value.is_empty()
        || value.len() > MAX_CAUSAL_RECEIPT_ID_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b':' | b'-'))
    {
        return Err(HostedRunnerConfigError::new(
            "MAESTRO_CAUSAL_RECEIPT_ID must be 1-128 ASCII bytes matching [A-Za-z0-9_.:-]",
        ));
    }
    Ok(value.to_string())
}

fn parse_listen(value: Option<&str>) -> Result<ParsedListen, HostedRunnerConfigError> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(ParsedListen {
            host: None,
            port: None,
        });
    };
    if value.chars().all(|char| char.is_ascii_digit()) {
        return Ok(ParsedListen {
            host: None,
            port: Some(parse_port(value, "MAESTRO_HOSTED_RUNNER_LISTEN")?),
        });
    }
    let Some((host, port)) = value.rsplit_once(':') else {
        return Err(HostedRunnerConfigError::new(
            "MAESTRO_HOSTED_RUNNER_LISTEN must be <host:port> or <port>",
        ));
    };
    if host.trim().is_empty() || port.trim().is_empty() {
        return Err(HostedRunnerConfigError::new(
            "MAESTRO_HOSTED_RUNNER_LISTEN must be <host:port> or <port>",
        ));
    }
    Ok(ParsedListen {
        host: Some(host.trim().to_string()),
        port: Some(parse_port(port.trim(), "MAESTRO_HOSTED_RUNNER_LISTEN")?),
    })
}

fn parse_optional_port(value: Option<&str>) -> Option<Result<u16, HostedRunnerConfigError>> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| parse_port(value, "hosted runner port"))
}

fn parse_port(value: &str, label: &str) -> Result<u16, HostedRunnerConfigError> {
    if !value.chars().all(|char| char.is_ascii_digit()) {
        return Err(HostedRunnerConfigError::new(format!(
            "{label} must be a TCP port between 1 and 65535"
        )));
    }
    let port = value.parse::<u32>().map_err(|_| {
        HostedRunnerConfigError::new(format!("{label} must be a TCP port between 1 and 65535"))
    })?;
    if !(1..=65535).contains(&port) {
        return Err(HostedRunnerConfigError::new(format!(
            "{label} must be a TCP port between 1 and 65535"
        )));
    }
    Ok(port as u16)
}

fn resolve_bind_addr(host: &str, port: u16) -> Result<SocketAddr, HostedRunnerConfigError> {
    format!("{host}:{port}")
        .to_socket_addrs()
        .map_err(|error| {
            HostedRunnerConfigError::new(format!(
                "hosted runner listen address is invalid: {error}"
            ))
        })?
        .next()
        .ok_or_else(|| HostedRunnerConfigError::new("hosted runner listen address is invalid"))
}

fn resolve_config_workspace_root(path: Option<&str>) -> Result<PathBuf, HostedRunnerConfigError> {
    let path = path
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .ok_or_else(|| {
            HostedRunnerConfigError::new(
                "deixic-code hosted-runner requires MAESTRO_WORKSPACE_ROOT",
            )
        })?;
    let workspace_root = dunce::canonicalize(Path::new(path)).map_err(|error| {
        HostedRunnerConfigError::new(format!(
            "hosted runner workspace root is unavailable: {error}"
        ))
    })?;
    let metadata = fs::metadata(&workspace_root).map_err(|error| {
        HostedRunnerConfigError::new(format!(
            "hosted runner workspace root is unavailable: {error}"
        ))
    })?;
    if !metadata.is_dir() {
        return Err(HostedRunnerConfigError::new(
            "hosted runner workspace root is not a directory",
        ));
    }
    Ok(workspace_root)
}

fn resolve_snapshot_root(path: Option<&str>, workspace_root: &Path) -> PathBuf {
    let Some(path) = path.map(str::trim).filter(|path| !path.is_empty()) else {
        return workspace_root.join(".maestro").join("runner-snapshots");
    };
    resolve_config_path(path, workspace_root)
}

fn resolve_optional_config_path(path: Option<&str>, workspace_root: &Path) -> Option<PathBuf> {
    path.map(str::trim)
        .filter(|path| !path.is_empty())
        .map(|path| resolve_config_path(path, workspace_root))
}

fn resolve_config_path(path: &str, workspace_root: &Path) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        workspace_root.join(path)
    }
}

fn path_to_str(path: &Path) -> Result<&str, HostedRunnerConfigError> {
    path.to_str()
        .ok_or_else(|| HostedRunnerConfigError::new("path must be valid UTF-8"))
}

fn non_empty(value: String, field: &str) -> Result<String, HostedRunnerConfigError> {
    let value = value.trim().to_string();
    if value.is_empty() {
        return Err(HostedRunnerConfigError::new(format!(
            "{field} must not be empty"
        )));
    }
    Ok(value)
}

fn validate_runtime_receipt_string(
    field: &str,
    value: &str,
) -> Result<(), HostedRunnerConfigError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(HostedRunnerConfigError::new(format!(
            "{field} must not be empty"
        )));
    }
    if trimmed != value {
        return Err(HostedRunnerConfigError::new(format!(
            "{field} must not contain surrounding whitespace"
        )));
    }
    if value.len() > MAX_RUNTIME_RECEIPT_STRING_BYTES {
        return Err(HostedRunnerConfigError::new(format!(
            "{field} exceeds the {MAX_RUNTIME_RECEIPT_STRING_BYTES}-byte runtime receipt limit"
        )));
    }
    Ok(())
}

fn read_secret_file(path: &str, label: &str) -> Result<String, HostedRunnerConfigError> {
    let value = read_bounded_file(path, label, MAX_HOSTED_SECRET_FILE_BYTES, true)?;
    let value = value.trim();
    if value.is_empty() {
        return Err(HostedRunnerConfigError::new(format!(
            "{label} file is invalid"
        )));
    }
    Ok(value.to_string())
}

fn read_bounded_file(
    path: &str,
    label: &str,
    max_bytes: usize,
    reject_controls: bool,
) -> Result<String, HostedRunnerConfigError> {
    let metadata = fs::metadata(path).map_err(|error| {
        HostedRunnerConfigError::new(format!("{label} file is unavailable: {error}"))
    })?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > max_bytes as u64 {
        return Err(HostedRunnerConfigError::new(format!(
            "{label} file is invalid"
        )));
    }
    let value = fs::read(path).map_err(|error| {
        HostedRunnerConfigError::new(format!("{label} file is unreadable: {error}"))
    })?;
    if value.is_empty() || value.len() > max_bytes {
        return Err(HostedRunnerConfigError::new(format!(
            "{label} file is invalid"
        )));
    }
    let value = String::from_utf8(value)
        .map_err(|_| HostedRunnerConfigError::new(format!("{label} file is not valid UTF-8")))?;
    if value.trim().is_empty() || (reject_controls && value.trim().chars().any(char::is_control)) {
        return Err(HostedRunnerConfigError::new(format!(
            "{label} file is invalid"
        )));
    }
    Ok(value)
}
