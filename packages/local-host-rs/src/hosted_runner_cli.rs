use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use clap::Parser;
use serde_json::json;

use crate::headless::{
    AgentEvent, AgentSupervisor, SessionRecorder, SupervisorConfig, SupervisorEvent,
};
use crate::hosted_runner::rendezvous_protocol::RendezvousMode;
use crate::hosted_runner::{
    AgentSupervisorHostedRunnerMessageExecutor, HostedRunnerConfig, HostedRunnerConfigError,
    HostedRunnerHandle, load_hosted_runner_session_replay, prepare_hosted_runner,
    start_prepared_hosted_runner, validate_startup_runtime_receipt_binding,
};
use maestro_runtime::{TelemetryConfig, TelemetryGuard};

const RESIDENT_MODEL_READY_CONTRACT_REVISION: &str = "maestro-resident-model-ready-v3";
const HEADLESS_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const HOSTED_LAUNCH_SPEC_FILE_ENV: &str = "MAESTRO_HOSTED_LAUNCH_SPEC_FILE";
const MAX_HOSTED_LAUNCH_SPEC_FILE_BYTES: usize = 256 * 1024;

#[derive(Debug, Parser)]
#[command(
    name = "deixic-code hosted-runner",
    about = "Run the Deixic Code hosted remote-runner runtime"
)]
pub struct HostedRunnerCliArgs {
    /// Versioned JSON launch descriptor. Legacy CLI/env coordinates cannot be
    /// combined with this input.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Platform remote-runner session id.
    #[arg(long)]
    runner_session_id: Option<String>,

    /// Platform runtime owner generation for attach fencing.
    #[arg(long)]
    owner_instance_id: Option<String>,

    /// Workspace root mounted into the runtime pod.
    #[arg(long)]
    workspace_root: Option<PathBuf>,

    /// Directory for drain snapshot manifests.
    #[arg(long)]
    snapshot_root: Option<PathBuf>,

    /// Snapshot manifest restored into this runner.
    #[arg(long)]
    restore_manifest: Option<PathBuf>,

    /// Address to bind, for example 0.0.0.0:8080.
    #[arg(long)]
    listen: Option<String>,

    /// Bind host when --listen is not used.
    #[arg(long)]
    host: Option<String>,

    /// Bind port when --listen is not used.
    #[arg(long)]
    port: Option<u16>,

    /// EvalOps workspace id for metadata.
    #[arg(long)]
    workspace_id: Option<String>,

    /// Platform agent-registry agent id forwarded to the headless runtime.
    #[arg(long)]
    agent_id: Option<String>,

    /// Platform AgentRun id for metadata.
    #[arg(long)]
    agent_run_id: Option<String>,

    /// Existing Maestro session id for metadata.
    #[arg(long)]
    maestro_session_id: Option<String>,

    /// Expected attach audience metadata.
    #[arg(long)]
    attach_audience: Option<String>,

    /// Local Maestro headless executable spawned behind the hosted runner.
    #[arg(long)]
    agent_cli_path: Option<PathBuf>,

    /// Compatibility flag used by Platform generated runtime args.
    #[arg(long)]
    from_config: bool,
}

/// Resolved hosted-runner startup configuration.
///
/// The runtime boundary is derived from [`Self::runner`] through
/// [`Self::runtime_boundary`] so adding the native boundary remains
/// source-compatible for downstream struct literals.
#[derive(Debug)]
pub struct HostedRunnerLaunchConfig {
    pub runner: HostedRunnerConfig,
    pub supervisor: SupervisorConfig,
    pub agent_id: Option<String>,
}

impl HostedRunnerLaunchConfig {
    /// Derives the transport-neutral, pre-start runtime identity snapshot.
    ///
    /// This accessor does not observe post-bind or post-session state. A
    /// requested port `0` and a restored or fallback session identity remain
    /// configuration inputs rather than authoritative runtime observations;
    /// listener and child-process ownership stays with the hosted runner.
    ///
    /// # Errors
    ///
    /// Returns the same validation errors as [`HostedRunnerConfig::runtime_boundary`].
    pub fn runtime_boundary(
        &self,
    ) -> Result<maestro_runtime::HostedRuntimeBoundary, HostedRunnerConfigError> {
        self.runner.runtime_boundary()
    }

    /// Compiles the resolved compatibility-edge inputs into one typed launch
    /// snapshot.
    ///
    /// The supplied map must be the merged map after CLI flags have been
    /// translated to canonical environment keys. The returned document is a
    /// pre-start identity snapshot: it does not observe a bound port, restored
    /// session, listener, or child process, and it does not move ownership of
    /// either listener or child lifecycle out of the hosted runner.
    ///
    /// # Errors
    ///
    /// Returns validation errors for an incomplete identity, invalid workspace,
    /// or inconsistent workload-identity launch.
    pub fn launch_spec(
        &self,
        resolved_env: &HashMap<String, String>,
    ) -> Result<maestro_runtime::HostedLaunchSpec> {
        let boundary = self.runtime_boundary()?;
        let workload_identity = self.runner.workload_identity.as_ref().map(|identity| {
            maestro_runtime::HostedLaunchWorkloadIdentity {
                kubernetes_token_file: identity
                    .kubernetes_token_file
                    .to_string_lossy()
                    .into_owned(),
                identity_tls_ca_file: identity.identity_tls_ca_file.to_string_lossy().into_owned(),
                identity_exchange_url: identity.identity_exchange_url.to_string(),
                organization_id: identity.organization_id.clone(),
                workspace_id: identity.workspace_id.clone(),
                sandbox_id: identity.sandbox_id.to_string(),
                placement_generation: identity.placement_generation,
            }
        });
        let rendezvous = self.runner.rendezvous.as_ref().map(|rendezvous| {
            let mode = match rendezvous.mode {
                RendezvousMode::Inbound => maestro_runtime::HostedLaunchRendezvousMode::Inbound,
                RendezvousMode::OutboundShadow => {
                    maestro_runtime::HostedLaunchRendezvousMode::OutboundShadow
                }
                RendezvousMode::Outbound => maestro_runtime::HostedLaunchRendezvousMode::Outbound,
            };
            maestro_runtime::HostedLaunchRendezvous {
                mode,
                endpoint: rendezvous.endpoint.clone(),
                server_name: rendezvous.server_name.clone(),
                identity_exchange_url: rendezvous.identity_exchange_url.to_string(),
                activation_id: rendezvous.activation_id.to_string(),
                nonce_file: first_env(resolved_env, &["MAESTRO_RENDEZVOUS_NONCE_FILE"]),
                nonce_present: true,
            }
        });
        let model = crate::headless_server::resolve_headless_model(None, resolved_env);
        let secret_files = maestro_runtime::HostedLaunchSecretFileRefs {
            static_bearer: first_env(
                resolved_env,
                &[
                    "MAESTRO_HOSTED_RUNNER_AUTH_TOKEN_FILE",
                    "MAESTRO_WEB_API_KEY_FILE",
                ],
            ),
            managed_gateway_access_token: first_env(
                resolved_env,
                &["MAESTRO_EVALOPS_ACCESS_TOKEN_FILE"],
            ),
            projected_workload_token: workload_identity
                .as_ref()
                .map(|identity| identity.kubernetes_token_file.clone()),
            identity_tls_ca: workload_identity
                .as_ref()
                .map(|identity| identity.identity_tls_ca_file.clone()),
        };

        Ok(maestro_runtime::HostedLaunchSpec::new(
            maestro_runtime::HostedLaunchSpecInput {
                runtime: maestro_runtime::HostedLaunchRuntime {
                    runner_session_id: boundary.runner_session_id,
                    bind_address: boundary.bind_address,
                    runtime_generation: boundary.runtime_generation,
                    owner_instance_id: boundary.owner_instance_id,
                    attach_audience: boundary.attach_audience,
                    causal_receipt_id: boundary.causal_receipt_id,
                },
                workspace: maestro_runtime::HostedLaunchWorkspace {
                    root: boundary.workspace_root,
                    workspace_id: boundary.workspace_id,
                    agent_run_id: boundary.agent_run_id,
                    maestro_session_id: boundary.maestro_session_id,
                },
                identity: maestro_runtime::HostedLaunchIdentity {
                    auth_mode: boundary.auth_mode,
                    workload_identity,
                },
                model: maestro_runtime::HostedLaunchModelContract {
                    model,
                    base_url: first_env(resolved_env, &["MAESTRO_EVALOPS_BASE_URL"]),
                    organization_id: first_env(resolved_env, &["MAESTRO_EVALOPS_ORG_ID"]),
                    workspace_id: first_env(resolved_env, &["MAESTRO_EVALOPS_WORKSPACE_ID"]),
                    provider: first_env(resolved_env, &["MAESTRO_EVALOPS_PROVIDER"]),
                    environment: first_env(resolved_env, &["MAESTRO_EVALOPS_ENVIRONMENT"]),
                    credential_name: first_env(resolved_env, &["MAESTRO_EVALOPS_CREDENTIAL_NAME"]),
                    team_id: first_env(resolved_env, &["MAESTRO_EVALOPS_TEAM_ID"]),
                    resident_contract_revision: first_env(
                        resolved_env,
                        &["MAESTRO_RESIDENT_CONTRACT_REVISION"],
                    ),
                },
                restore: maestro_runtime::HostedLaunchRestoreIntent {
                    snapshot_root: self
                        .runner
                        .snapshot_root
                        .as_ref()
                        .map(|path| path.to_string_lossy().into_owned()),
                    restore_manifest_path: self
                        .runner
                        .restore_manifest_path
                        .as_ref()
                        .map(|path| path.to_string_lossy().into_owned()),
                },
                rendezvous,
                secret_files,
                headless_cli_path: first_env(
                    resolved_env,
                    &[
                        "MAESTRO_HEADLESS_CLI_PATH",
                        "MAESTRO_AGENT_SCRIPT",
                        "MAESTRO_CLI_PATH",
                    ],
                ),
                profile: first_env(resolved_env, &["MAESTRO_PROFILE"]),
                agent_dir: first_env(resolved_env, &["MAESTRO_AGENT_DIR"]),
                agent_id: self.agent_id.clone(),
            },
        )?)
    }
}

pub struct HostedRunnerCliRuntime {
    handle: HostedRunnerHandle,
}

/// Install the structured logger for both the dedicated hosted-runner binary
/// and the `maestro-tui hosted-runner` compatibility entrypoint. The latter is
/// the immutable binary used by Platform's E2E harness, so keeping setup here
/// prevents the two supported launch paths from producing different traces.
pub fn init_hosted_runner_tracing() -> TelemetryGuard {
    TelemetryGuard::init(TelemetryConfig::new(
        "maestro-hosted-runner",
        env!("CARGO_PKG_VERSION"),
        "info",
        "local",
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostedRunnerShutdownSignal {
    Hangup,
    Interrupt,
    Quit,
    Terminate,
}

impl HostedRunnerShutdownSignal {
    fn reason(self) -> &'static str {
        "process_shutdown"
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Hangup => "sighup",
            Self::Interrupt => "sigint",
            Self::Quit => "sigquit",
            Self::Terminate => "sigterm",
        }
    }
}

impl HostedRunnerCliRuntime {
    #[must_use]
    pub fn base_url(&self) -> String {
        self.handle.base_url()
    }

    pub async fn drain_for_shutdown(
        &self,
        signal: HostedRunnerShutdownSignal,
    ) -> Result<serde_json::Value> {
        self.handle
            .drain_for_shutdown(signal.reason(), "maestro-hosted-runner")
            .await
            .context("drain Rust hosted runner before shutdown")
    }

    pub async fn shutdown(self) {
        self.handle.shutdown().await;
    }
}

pub fn resolve_hosted_runner_launch_config<I, T>(
    args: I,
    env: &HashMap<String, String>,
) -> Result<HostedRunnerLaunchConfig>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    resolve_hosted_runner_launch_config_with_env(args, env).map(|(config, _)| config)
}

fn resolve_hosted_runner_launch_config_with_env<I, T>(
    args: I,
    env: &HashMap<String, String>,
) -> Result<(HostedRunnerLaunchConfig, HashMap<String, String>)>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = HostedRunnerCliArgs::try_parse_from(args)?;
    let config_path = match (
        cli.config.as_ref(),
        first_env(env, &[HOSTED_LAUNCH_SPEC_FILE_ENV]),
    ) {
        (Some(_), Some(_)) => {
            anyhow::bail!(
                "hosted launch spec must be supplied by either --config or {HOSTED_LAUNCH_SPEC_FILE_ENV}, not both"
            )
        }
        (Some(path), None) => Some(path.clone()),
        (None, Some(path)) => Some(PathBuf::from(path)),
        (None, None) => None,
    };
    let (runner, merged_env, descriptor_input) = if let Some(config_path) = config_path {
        reject_legacy_launch_sources(&cli, env)?;
        let spec = read_hosted_launch_spec_file(&config_path)?;
        let mut merged_env = env.clone();
        apply_launch_spec_env(&mut merged_env, &spec);
        let runner = HostedRunnerConfig::from_launch_spec(&spec)?;
        (runner, merged_env, true)
    } else {
        let mut merged_env = env.clone();
        apply_cli_env_overrides(&mut merged_env, &cli);
        let runner = HostedRunnerConfig::from_env_map(&merged_env)?;
        (runner, merged_env, false)
    };
    validate_resident_contract(&merged_env)?;

    let auth_required = !descriptor_input
        && first_env(&merged_env, &["MAESTRO_WEB_REQUIRE_KEY"]).as_deref() != Some("0");
    if auth_required && runner.auth_token.is_none() && runner.workload_identity.is_none() {
        anyhow::bail!(
            "deixic-code hosted-runner requires MAESTRO_HOSTED_RUNNER_AUTH_TOKEN or MAESTRO_WEB_API_KEY; set MAESTRO_WEB_REQUIRE_KEY=0 only for local testing"
        );
    }
    // The hosted resident owns one attested child generation. If that child
    // exits, the resident must revoke readiness and let the external runtime
    // owner replace the generation instead of reconnecting behind its lease.
    let mut supervisor = SupervisorConfig {
        auto_reconnect: false,
        ..SupervisorConfig::default()
    };
    supervisor.transport.cli_path = first_env(
        &merged_env,
        &[
            "MAESTRO_HEADLESS_CLI_PATH",
            "MAESTRO_AGENT_SCRIPT",
            "MAESTRO_CLI_PATH",
        ],
    )
    .unwrap_or_else(|| "maestro".to_string());
    supervisor.transport.cwd = Some(runner.workspace_root.to_string_lossy().to_string());
    supervisor.transport.env = hosted_agent_env(&runner, &merged_env)?;

    Ok((
        HostedRunnerLaunchConfig {
            runner,
            supervisor,
            agent_id: first_env(
                &merged_env,
                &["MAESTRO_REMOTE_RUNNER_AGENT_ID", "MAESTRO_AGENT_ID"],
            ),
        },
        merged_env,
    ))
}

async fn join_hosted_runner_startup<HF, PF, H, P, E>(
    headless: HF,
    preparation: PF,
) -> std::result::Result<(H, P), E>
where
    HF: Future<Output = std::result::Result<H, E>>,
    PF: Future<Output = std::result::Result<P, E>>,
{
    tokio::try_join!(headless, preparation)
}

async fn prepare_while_starting_headless<PF, P>(
    supervisor: &mut AgentSupervisor,
    expected_model: &str,
    expected_provider: Option<&str>,
    preparation: PF,
) -> Result<P>
where
    PF: Future<Output = Result<P>>,
{
    let (validation_tx, validation_rx) = tokio::sync::oneshot::channel();
    let headless = async {
        supervisor.connect().await?;
        await_headless_ready(supervisor, expected_model, expected_provider).await?;
        validate_startup_runtime_receipt_binding(supervisor.state())
            .map_err(anyhow::Error::from)
            .context("validate startup runtime receipt binding")?;
        validation_tx
            .send(())
            .map_err(|()| anyhow::anyhow!("startup preparation stopped before validation"))?;
        Ok(())
    };
    let gated_preparation = async {
        validation_rx
            .await
            .map_err(|_| anyhow::anyhow!("headless startup validation failed"))?;
        preparation.await
    };
    match join_hosted_runner_startup(headless, gated_preparation).await {
        Ok(((), prepared)) => Ok(prepared),
        Err(error) => {
            supervisor.shutdown_and_wait().await;
            Err(error)
        }
    }
}

async fn shutdown_shared_supervisor(supervisor: Arc<Mutex<AgentSupervisor>>) -> Result<()> {
    let mutex = Arc::try_unwrap(supervisor)
        .map_err(|_| anyhow::anyhow!("headless supervisor still has active owners"))?;
    let mut supervisor = mutex
        .into_inner()
        .unwrap_or_else(|error| error.into_inner());
    supervisor.shutdown_and_wait().await;
    Ok(())
}

pub async fn start_hosted_runner_cli_runtime<I, T>(
    args: I,
    env: &HashMap<String, String>,
) -> Result<HostedRunnerCliRuntime>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let (config, resolved_env) = resolve_hosted_runner_launch_config_with_env(args, env)?;
    start_resolved_hosted_runner_cli_runtime(config, resolved_env).await
}

async fn start_resolved_hosted_runner_cli_runtime(
    mut config: HostedRunnerLaunchConfig,
    resolved_env: HashMap<String, String>,
) -> Result<HostedRunnerCliRuntime> {
    let runtime_boundary = config.runtime_boundary()?;
    let launch_spec = config.launch_spec(&resolved_env)?;
    for alias in &config.runner.deprecated_env_aliases {
        tracing::warn!(
            deprecated_env_key = alias.alias,
            canonical_env_key = alias.canonical,
            "{}",
            alias.warning()
        );
    }
    tracing::info!(
        runtime_product = maestro_runtime::RUNTIME_PRODUCT_ID,
        runtime_boundary = %runtime_boundary.schema_version,
        runtime_topology = %runtime_boundary.topology,
        launch_spec_version = %launch_spec.schema_version,
        launch_spec_digest = %launch_spec.redacted_digest(),
        runner_session_id = %runtime_boundary.runner_session_id,
        runtime_generation = runtime_boundary.runtime_generation,
        "resolved native Maestro runtime launch spec"
    );
    let restore_replay = load_hosted_runner_session_replay(&config.runner).await?;
    let session_id = resolve_hosted_session_id(
        config.runner.maestro_session_id.as_deref(),
        restore_replay
            .as_ref()
            .and_then(|replay| replay.state.session_id.as_deref()),
        &config.runner.runner_session_id,
    );
    set_env_value(
        &mut config.supervisor.transport.env,
        "MAESTRO_SESSION_ID",
        &session_id,
    );
    let sessions_dir = config.runner.workspace_root.join(".maestro/sessions");
    let mut recorder = SessionRecorder::resume(sessions_dir, &session_recorder_id(&session_id))?;
    let recorded_replay = recorder.replay();
    if let Some(replay) = restore_replay.as_ref() {
        recorder.apply_snapshot(replay.state.clone(), replay.last_init.clone())?;
    }
    let child_env = config
        .supervisor
        .transport
        .env
        .iter()
        .cloned()
        .collect::<HashMap<_, _>>();
    let expected_model = crate::headless_server::resolve_headless_model(None, &child_env);
    let expected_provider = managed_model(&expected_model).then(|| {
        first_env(&child_env, &["MAESTRO_EVALOPS_PROVIDER"]).unwrap_or_else(|| "openai".to_string())
    });
    let causal_receipt_id = config.runner.causal_receipt_id.clone();
    let mut supervisor = AgentSupervisor::new(config.supervisor).with_session_recorder(recorder);
    if let Some(replay) = restore_replay {
        supervisor.restore_session_replay(crate::headless::SessionReplay {
            semantic_conversation: replay
                .semantic_conversation
                .or(recorded_replay.semantic_conversation),
            ..replay
        });
    } else if recorded_replay.semantic_conversation.is_some() {
        supervisor.restore_session_replay(recorded_replay);
    }
    let preparation = async move {
        prepare_hosted_runner(config.runner)
            .await
            .context("prepare Rust hosted runner")
    };
    let prepared = prepare_while_starting_headless(
        &mut supervisor,
        &expected_model,
        expected_provider.as_deref(),
        preparation,
    )
    .await?;
    let supervisor = Arc::new(Mutex::new(supervisor));
    let executor = Arc::new(
        AgentSupervisorHostedRunnerMessageExecutor::new_with_causal_receipt_id(
            supervisor.clone(),
            causal_receipt_id,
        ),
    );
    let handle = match start_prepared_hosted_runner(prepared, executor) {
        Ok(handle) => handle,
        Err(error) => {
            shutdown_shared_supervisor(supervisor).await?;
            return Err(error).context("start Rust hosted runner");
        }
    };
    Ok(HostedRunnerCliRuntime { handle })
}

pub async fn run_hosted_runner_cli_from_env<I, T>(args: I) -> Result<()>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let _telemetry = init_hosted_runner_tracing();
    let env = std::env::vars().collect::<HashMap<_, _>>();
    // Register before startup can expose readiness. Constructing an async
    // signal waiter alone would defer registration until its first poll.
    let shutdown_signal = register_shutdown_signal_listener()?;
    let runtime = match start_hosted_runner_cli_runtime(args, &env).await {
        Ok(runtime) => runtime,
        Err(error) => {
            if let Some(clap_error) = error.downcast_ref::<clap::Error>() {
                clap_error.exit();
            }
            return Err(error);
        }
    };
    println!(
        "{}",
        json!({
            "baseUrl": runtime.base_url(),
            "runtime": "rust-hosted-runner",
        })
    );
    let signal = shutdown_signal.await?;
    let drain = runtime.drain_for_shutdown(signal).await?;
    println!(
        "{}",
        json!({
            "drain": drain,
            "runtime": "rust-hosted-runner",
            "shutdownSignal": signal.as_str(),
        })
    );
    runtime.shutdown().await;
    Ok(())
}

fn register_shutdown_signal_listener()
-> Result<impl std::future::Future<Output = Result<HostedRunnerShutdownSignal>>> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut hangup = signal(SignalKind::hangup())?;
        let mut quit = signal(SignalKind::quit())?;
        let mut terminate = signal(SignalKind::terminate())?;
        Ok(async move {
            tokio::select! {
                _ = hangup.recv() => Ok(HostedRunnerShutdownSignal::Hangup),
                _ = interrupt.recv() => Ok(HostedRunnerShutdownSignal::Interrupt),
                _ = quit.recv() => Ok(HostedRunnerShutdownSignal::Quit),
                _ = terminate.recv() => Ok(HostedRunnerShutdownSignal::Terminate),
            }
        })
    }
    #[cfg(not(unix))]
    {
        Ok(async {
            tokio::signal::ctrl_c().await?;
            Ok(HostedRunnerShutdownSignal::Interrupt)
        })
    }
}

fn read_hosted_launch_spec_file(path: &PathBuf) -> Result<maestro_runtime::HostedLaunchSpec> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("hosted launch spec file is unavailable: {}", path.display()))?;
    if !metadata.is_file()
        || metadata.len() == 0
        || metadata.len() > MAX_HOSTED_LAUNCH_SPEC_FILE_BYTES as u64
    {
        anyhow::bail!("hosted launch spec file is invalid or exceeds the bounded size")
    }
    let document = fs::read(path)
        .with_context(|| format!("hosted launch spec file is unreadable: {}", path.display()))?;
    if document.is_empty() || document.len() > MAX_HOSTED_LAUNCH_SPEC_FILE_BYTES {
        anyhow::bail!("hosted launch spec file is invalid or exceeds the bounded size")
    }
    let document =
        String::from_utf8(document).context("hosted launch spec file must be valid UTF-8 JSON")?;
    maestro_runtime::HostedLaunchSpec::from_json_str(&document)
        .map_err(|error| anyhow::anyhow!("hosted launch spec validation failed: {error}"))
}

fn reject_legacy_launch_sources(
    cli: &HostedRunnerCliArgs,
    env: &HashMap<String, String>,
) -> Result<()> {
    let cli_has_legacy_coordinates = [
        cli.runner_session_id.is_some(),
        cli.owner_instance_id.is_some(),
        cli.workspace_root.is_some(),
        cli.snapshot_root.is_some(),
        cli.restore_manifest.is_some(),
        cli.listen.is_some(),
        cli.host.is_some(),
        cli.port.is_some(),
        cli.workspace_id.is_some(),
        cli.agent_id.is_some(),
        cli.agent_run_id.is_some(),
        cli.maestro_session_id.is_some(),
        cli.attach_audience.is_some(),
        cli.agent_cli_path.is_some(),
    ]
    .into_iter()
    .any(|present| present);
    const LEGACY_LAUNCH_ENV_KEYS: &[&str] = &[
        "MAESTRO_RUNNER_SESSION_ID",
        "REMOTE_RUNNER_SESSION_ID",
        "MAESTRO_WORKSPACE_ROOT",
        "WORKSPACE_ROOT",
        "MAESTRO_REMOTE_RUNNER_OWNER_INSTANCE_ID",
        "REMOTE_RUNNER_OWNER_INSTANCE_ID",
        "MAESTRO_REMOTE_RUNNER_SNAPSHOT_ROOT",
        "REMOTE_RUNNER_SNAPSHOT_ROOT",
        "MAESTRO_REMOTE_RUNNER_RESTORE_MANIFEST",
        "REMOTE_RUNNER_RESTORE_MANIFEST",
        "MAESTRO_HOSTED_RUNNER_LISTEN",
        "MAESTRO_HOSTED_RUNNER_HOST",
        "MAESTRO_HOSTED_RUNNER_PORT",
        "MAESTRO_REMOTE_RUNNER_WORKSPACE_ID",
        "MAESTRO_WORKSPACE_ID",
        "MAESTRO_REMOTE_RUNNER_AGENT_ID",
        "MAESTRO_AGENT_ID",
        "MAESTRO_AGENT_RUN_ID",
        "MAESTRO_SESSION_ID",
        "MAESTRO_ATTACH_AUDIENCE",
        "MAESTRO_CAUSAL_RECEIPT_ID",
        "MAESTRO_PLACEMENT_GENERATION",
        "MAESTRO_SANDBOXWICH_PLACEMENT_GENERATION",
        "MAESTRO_REMOTE_RUNNER_GENERATION",
        "MAESTRO_HEADLESS_CLI_PATH",
        "MAESTRO_AGENT_SCRIPT",
        "MAESTRO_CLI_PATH",
        "MAESTRO_PROFILE",
        "MAESTRO_AGENT_DIR",
        "MAESTRO_MODEL",
        "MAESTRO_DEFAULT_MODEL",
        "MAESTRO_EVALOPS_BASE_URL",
        "MAESTRO_EVALOPS_ORG_ID",
        "MAESTRO_EVALOPS_WORKSPACE_ID",
        "MAESTRO_EVALOPS_PROVIDER",
        "MAESTRO_EVALOPS_ENVIRONMENT",
        "MAESTRO_EVALOPS_CREDENTIAL_NAME",
        "MAESTRO_EVALOPS_TEAM_ID",
        "MAESTRO_EVALOPS_ACCESS_TOKEN",
        "MAESTRO_EVALOPS_ACCESS_TOKEN_FILE",
        "MAESTRO_RESIDENT_CONTRACT_REVISION",
        "MAESTRO_HOSTED_RUNNER_AUTH_TOKEN",
        "MAESTRO_WEB_API_KEY",
        "MAESTRO_HOSTED_RUNNER_AUTH_TOKEN_FILE",
        "MAESTRO_WEB_API_KEY_FILE",
        "MAESTRO_RUNNER_CLIENT_CA_FILE",
        "MAESTRO_KUBERNETES_TOKEN_FILE",
        "MAESTRO_IDENTITY_TLS_CA_FILE",
        "MAESTRO_IDENTITY_EXCHANGE_URL",
        "MAESTRO_ORGANIZATION_ID",
        "MAESTRO_SANDBOX_ID",
        "MAESTRO_RENDEZVOUS_MODE",
        "MAESTRO_RENDEZVOUS_OUTBOUND_PREFER",
        "MAESTRO_RENDEZVOUS_ENDPOINT",
        "MAESTRO_RENDEZVOUS_SERVER_NAME",
        "MAESTRO_RENDEZVOUS_IDENTITY_EXCHANGE_URL",
        "MAESTRO_RENDEZVOUS_ACTIVATION_ID",
        "MAESTRO_RENDEZVOUS_NONCE",
        "MAESTRO_RENDEZVOUS_NONCE_FILE",
    ];
    let env_has_legacy_coordinates = LEGACY_LAUNCH_ENV_KEYS
        .iter()
        .any(|key| first_env(env, &[*key]).is_some());
    if cli_has_legacy_coordinates || env_has_legacy_coordinates {
        anyhow::bail!(
            "hosted launch spec config cannot be combined with legacy hosted-runner launch coordinates"
        );
    }
    Ok(())
}

fn apply_launch_spec_env(
    env: &mut HashMap<String, String>,
    spec: &maestro_runtime::HostedLaunchSpec,
) {
    env.insert(
        "MAESTRO_RUNNER_SESSION_ID".to_string(),
        spec.runtime.runner_session_id.clone(),
    );
    env.insert(
        "MAESTRO_WORKSPACE_ROOT".to_string(),
        spec.workspace.root.clone(),
    );
    env.insert(
        "MAESTRO_HOSTED_RUNNER_LISTEN".to_string(),
        spec.runtime.bind_address.clone(),
    );
    env.insert(
        "MAESTRO_REMOTE_RUNNER_GENERATION".to_string(),
        spec.runtime.runtime_generation.to_string(),
    );
    set_string(
        env,
        "MAESTRO_REMOTE_RUNNER_OWNER_INSTANCE_ID",
        spec.runtime.owner_instance_id.as_deref(),
    );
    set_string(
        env,
        "MAESTRO_REMOTE_RUNNER_WORKSPACE_ID",
        spec.workspace.workspace_id.as_deref(),
    );
    set_string(
        env,
        "MAESTRO_AGENT_RUN_ID",
        spec.workspace.agent_run_id.as_deref(),
    );
    set_string(
        env,
        "MAESTRO_SESSION_ID",
        spec.workspace.maestro_session_id.as_deref(),
    );
    set_string(
        env,
        "MAESTRO_CAUSAL_RECEIPT_ID",
        spec.runtime.causal_receipt_id.as_deref(),
    );
    set_string(
        env,
        "MAESTRO_ATTACH_AUDIENCE",
        spec.runtime.attach_audience.as_deref(),
    );
    set_string(env, "MAESTRO_MODEL", Some(spec.model.model.as_str()));
    set_string(
        env,
        "MAESTRO_EVALOPS_BASE_URL",
        spec.model.base_url.as_deref(),
    );
    set_string(
        env,
        "MAESTRO_EVALOPS_ORG_ID",
        spec.model.organization_id.as_deref(),
    );
    set_string(
        env,
        "MAESTRO_EVALOPS_WORKSPACE_ID",
        spec.model.workspace_id.as_deref(),
    );
    set_string(
        env,
        "MAESTRO_EVALOPS_PROVIDER",
        spec.model.provider.as_deref(),
    );
    set_string(
        env,
        "MAESTRO_EVALOPS_ENVIRONMENT",
        spec.model.environment.as_deref(),
    );
    set_string(
        env,
        "MAESTRO_EVALOPS_CREDENTIAL_NAME",
        spec.model.credential_name.as_deref(),
    );
    set_string(
        env,
        "MAESTRO_EVALOPS_TEAM_ID",
        spec.model.team_id.as_deref(),
    );
    set_string(
        env,
        "MAESTRO_RESIDENT_CONTRACT_REVISION",
        spec.model.resident_contract_revision.as_deref(),
    );
    set_string(
        env,
        "MAESTRO_HOSTED_RUNNER_AUTH_TOKEN_FILE",
        spec.secret_files.static_bearer.as_deref(),
    );
    set_string(
        env,
        "MAESTRO_EVALOPS_ACCESS_TOKEN_FILE",
        spec.secret_files.managed_gateway_access_token.as_deref(),
    );
    set_string(
        env,
        "MAESTRO_REMOTE_RUNNER_AGENT_ID",
        spec.agent_id.as_deref(),
    );
    set_string(env, "MAESTRO_AGENT_ID", spec.agent_id.as_deref());
    set_string(env, "MAESTRO_PROFILE", spec.profile.as_deref());
    set_string(env, "MAESTRO_AGENT_DIR", spec.agent_dir.as_deref());
    set_string(
        env,
        "MAESTRO_HEADLESS_CLI_PATH",
        spec.headless_cli_path.as_deref(),
    );
    if let Some(snapshot_root) = spec.restore.snapshot_root.as_deref() {
        env.insert(
            "MAESTRO_REMOTE_RUNNER_SNAPSHOT_ROOT".to_string(),
            snapshot_root.to_string(),
        );
    }
    if let Some(restore_manifest) = spec.restore.restore_manifest_path.as_deref() {
        env.insert(
            "MAESTRO_REMOTE_RUNNER_RESTORE_MANIFEST".to_string(),
            restore_manifest.to_string(),
        );
    }
    if let Some(workload) = spec.identity.workload_identity.as_ref() {
        env.insert(
            "MAESTRO_KUBERNETES_TOKEN_FILE".to_string(),
            workload.kubernetes_token_file.clone(),
        );
        env.insert(
            "MAESTRO_IDENTITY_TLS_CA_FILE".to_string(),
            workload.identity_tls_ca_file.clone(),
        );
        env.insert(
            "MAESTRO_IDENTITY_EXCHANGE_URL".to_string(),
            workload.identity_exchange_url.clone(),
        );
        env.insert(
            "MAESTRO_ORGANIZATION_ID".to_string(),
            workload.organization_id.clone(),
        );
        env.insert(
            "MAESTRO_WORKSPACE_ID".to_string(),
            workload.workspace_id.clone(),
        );
        env.insert(
            "MAESTRO_SANDBOX_ID".to_string(),
            workload.sandbox_id.clone(),
        );
        env.insert(
            "MAESTRO_PLACEMENT_GENERATION".to_string(),
            workload.placement_generation.to_string(),
        );
        set_string(
            env,
            "MAESTRO_KUBERNETES_TOKEN_FILE",
            spec.secret_files.projected_workload_token.as_deref(),
        );
        set_string(
            env,
            "MAESTRO_IDENTITY_TLS_CA_FILE",
            spec.secret_files.identity_tls_ca.as_deref(),
        );
    }
    if let Some(rendezvous) = spec.rendezvous.as_ref() {
        let mode = match rendezvous.mode {
            maestro_runtime::HostedLaunchRendezvousMode::Inbound => "inbound",
            maestro_runtime::HostedLaunchRendezvousMode::OutboundShadow => "outbound_shadow",
            maestro_runtime::HostedLaunchRendezvousMode::Outbound => "outbound",
        };
        env.insert("MAESTRO_RENDEZVOUS_MODE".to_string(), mode.to_string());
        env.insert(
            "MAESTRO_RENDEZVOUS_ENDPOINT".to_string(),
            rendezvous.endpoint.clone(),
        );
        env.insert(
            "MAESTRO_RENDEZVOUS_SERVER_NAME".to_string(),
            rendezvous.server_name.clone(),
        );
        env.insert(
            "MAESTRO_RENDEZVOUS_IDENTITY_EXCHANGE_URL".to_string(),
            rendezvous.identity_exchange_url.clone(),
        );
        env.insert(
            "MAESTRO_RENDEZVOUS_ACTIVATION_ID".to_string(),
            rendezvous.activation_id.clone(),
        );
        set_string(
            env,
            "MAESTRO_RENDEZVOUS_NONCE_FILE",
            rendezvous.nonce_file.as_deref(),
        );
        if matches!(
            rendezvous.mode,
            maestro_runtime::HostedLaunchRendezvousMode::Outbound
        ) {
            env.insert(
                "MAESTRO_RENDEZVOUS_OUTBOUND_PREFER".to_string(),
                "true".to_string(),
            );
        }
    }
}

fn apply_cli_env_overrides(env: &mut HashMap<String, String>, cli: &HostedRunnerCliArgs) {
    set_string(
        env,
        "MAESTRO_RUNNER_SESSION_ID",
        cli.runner_session_id.as_deref(),
    );
    set_string(
        env,
        "MAESTRO_REMOTE_RUNNER_OWNER_INSTANCE_ID",
        cli.owner_instance_id.as_deref(),
    );
    set_path(env, "MAESTRO_WORKSPACE_ROOT", cli.workspace_root.as_ref());
    set_path(
        env,
        "MAESTRO_REMOTE_RUNNER_SNAPSHOT_ROOT",
        cli.snapshot_root.as_ref(),
    );
    set_path(
        env,
        "MAESTRO_REMOTE_RUNNER_RESTORE_MANIFEST",
        cli.restore_manifest.as_ref(),
    );
    set_string(env, "MAESTRO_HOSTED_RUNNER_LISTEN", cli.listen.as_deref());
    set_string(env, "MAESTRO_HOSTED_RUNNER_HOST", cli.host.as_deref());
    if let Some(port) = cli.port {
        env.insert("MAESTRO_HOSTED_RUNNER_PORT".to_string(), port.to_string());
    }
    set_string(
        env,
        "MAESTRO_REMOTE_RUNNER_WORKSPACE_ID",
        cli.workspace_id.as_deref(),
    );
    set_string(
        env,
        "MAESTRO_REMOTE_RUNNER_AGENT_ID",
        cli.agent_id.as_deref(),
    );
    set_string(env, "MAESTRO_AGENT_ID", cli.agent_id.as_deref());
    set_string(env, "MAESTRO_AGENT_RUN_ID", cli.agent_run_id.as_deref());
    set_string(env, "MAESTRO_SESSION_ID", cli.maestro_session_id.as_deref());
    set_string(
        env,
        "MAESTRO_ATTACH_AUDIENCE",
        cli.attach_audience.as_deref(),
    );
    set_path(
        env,
        "MAESTRO_HEADLESS_CLI_PATH",
        cli.agent_cli_path.as_ref(),
    );
}

fn hosted_agent_env(
    runner: &HostedRunnerConfig,
    merged_env: &HashMap<String, String>,
) -> Result<Vec<(String, String)>> {
    let profile =
        first_env(merged_env, &["MAESTRO_PROFILE"]).unwrap_or_else(|| "hosted-runner".to_string());
    let mut env = vec![
        ("MAESTRO_HOSTED_RUNNER_MODE".to_string(), "1".to_string()),
        (
            "MAESTRO_RUNNER_SESSION_ID".to_string(),
            runner.runner_session_id.clone(),
        ),
        (
            "MAESTRO_WORKSPACE_ROOT".to_string(),
            runner.workspace_root.to_string_lossy().to_string(),
        ),
        ("MAESTRO_PROFILE".to_string(), profile),
    ];
    env.push((
        "MAESTRO_AGENT_DIR".to_string(),
        first_env(merged_env, &["MAESTRO_AGENT_DIR"]).unwrap_or_else(|| {
            runner
                .workspace_root
                .join(".maestro/agent")
                .to_string_lossy()
                .to_string()
        }),
    ));
    let model = crate::headless_server::resolve_headless_model(None, merged_env);
    env.push(("MAESTRO_MODEL".to_string(), model));
    for key in [
        "MAESTRO_EVALOPS_BASE_URL",
        "MAESTRO_EVALOPS_ORG_ID",
        "MAESTRO_EVALOPS_WORKSPACE_ID",
        "MAESTRO_EVALOPS_PROVIDER",
        "MAESTRO_EVALOPS_ENVIRONMENT",
        "MAESTRO_EVALOPS_CREDENTIAL_NAME",
        "MAESTRO_EVALOPS_TEAM_ID",
        "MAESTRO_RESIDENT_CONTRACT_REVISION",
    ] {
        if let Some(value) = first_env(merged_env, &[key]) {
            env.push((key.to_string(), value));
        }
    }
    if let Some(owner_instance_id) = runner.owner_instance_id.as_ref() {
        env.push((
            "MAESTRO_REMOTE_RUNNER_OWNER_INSTANCE_ID".to_string(),
            owner_instance_id.clone(),
        ));
        env.push((
            "REMOTE_RUNNER_OWNER_INSTANCE_ID".to_string(),
            owner_instance_id.clone(),
        ));
    }
    if let Some(snapshot_root) = runner.snapshot_root.as_ref() {
        env.push((
            "MAESTRO_REMOTE_RUNNER_SNAPSHOT_ROOT".to_string(),
            snapshot_root.to_string_lossy().to_string(),
        ));
    }
    if let Some(workspace_id) = runner.workspace_id.as_ref() {
        env.push((
            "MAESTRO_REMOTE_RUNNER_WORKSPACE_ID".to_string(),
            workspace_id.clone(),
        ));
    }
    if let Some(agent_run_id) = runner.agent_run_id.as_ref() {
        env.push(("MAESTRO_AGENT_RUN_ID".to_string(), agent_run_id.clone()));
    }
    if let Some(maestro_session_id) = runner.maestro_session_id.as_ref() {
        env.push(("MAESTRO_SESSION_ID".to_string(), maestro_session_id.clone()));
    }
    if let Some(causal_receipt_id) = runner.causal_receipt_id.as_ref() {
        env.push((
            "MAESTRO_CAUSAL_RECEIPT_ID".to_string(),
            causal_receipt_id.clone(),
        ));
    }
    if let Some(attach_audience) = runner.attach_audience.as_ref() {
        env.push((
            "MAESTRO_ATTACH_AUDIENCE".to_string(),
            attach_audience.clone(),
        ));
    }
    if let Some(agent_id) = first_env(
        merged_env,
        &["MAESTRO_REMOTE_RUNNER_AGENT_ID", "MAESTRO_AGENT_ID"],
    ) {
        env.push((
            "MAESTRO_REMOTE_RUNNER_AGENT_ID".to_string(),
            agent_id.clone(),
        ));
        env.push(("MAESTRO_AGENT_ID".to_string(), agent_id));
    }
    if let Some(path) = first_env(merged_env, &["MAESTRO_EVALOPS_ACCESS_TOKEN_FILE"]) {
        const MAX_MANAGED_TOKEN_BYTES: usize = 16 * 1024;
        let metadata = fs::metadata(&path)
            .with_context(|| "managed gateway credential file is unavailable")?;
        if !metadata.is_file()
            || metadata.len() == 0
            || metadata.len() > MAX_MANAGED_TOKEN_BYTES as u64
        {
            anyhow::bail!("managed gateway credential file is invalid");
        }
        let token =
            fs::read(&path).with_context(|| "managed gateway credential file is unreadable")?;
        if token.is_empty() || token.len() > MAX_MANAGED_TOKEN_BYTES {
            anyhow::bail!("managed gateway credential file is invalid");
        }
        let token = String::from_utf8(token)
            .with_context(|| "managed gateway credential file is not valid UTF-8")?;
        let token = token.trim();
        if token.is_empty() || token.chars().any(char::is_control) {
            anyhow::bail!("managed gateway credential file is invalid");
        }
        // The control-plane resident request carries only the fixed file path;
        // the child receives the credential in its private process environment
        // after the encrypted bootstrap has been materialized by Sandboxwich.
        env.push((
            "MAESTRO_EVALOPS_ACCESS_TOKEN".to_string(),
            token.to_string(),
        ));
    } else if let Some(token) = first_env(merged_env, &["MAESTRO_EVALOPS_ACCESS_TOKEN"]) {
        env.push(("MAESTRO_EVALOPS_ACCESS_TOKEN".to_string(), token));
    }
    if managed_model(
        env.iter()
            .find(|(key, _)| key == "MAESTRO_MODEL")
            .map(|(_, value)| value.as_str())
            .unwrap_or_default(),
    ) {
        for required in [
            "MAESTRO_EVALOPS_ACCESS_TOKEN",
            "MAESTRO_EVALOPS_BASE_URL",
            "MAESTRO_EVALOPS_ORG_ID",
            "MAESTRO_EVALOPS_WORKSPACE_ID",
            "MAESTRO_EVALOPS_PROVIDER",
        ] {
            if !env
                .iter()
                .any(|(key, value)| key == required && !value.trim().is_empty())
            {
                anyhow::bail!("managed headless model requires {required}");
            }
        }
    }
    Ok(env)
}

fn managed_model(model: &str) -> bool {
    ["evalops/", "maestro-managed/"].into_iter().any(|prefix| {
        model
            .trim()
            .get(..prefix.len())
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix))
    })
}

fn validate_resident_contract(env: &HashMap<String, String>) -> Result<()> {
    let model = crate::headless_server::resolve_headless_model(None, env);
    if !managed_model(&model) {
        return Ok(());
    }
    let revision = first_env(env, &["MAESTRO_RESIDENT_CONTRACT_REVISION"]);
    if revision.as_deref() != Some(RESIDENT_MODEL_READY_CONTRACT_REVISION) {
        anyhow::bail!(
            "managed hosted runner requires MAESTRO_RESIDENT_CONTRACT_REVISION={RESIDENT_MODEL_READY_CONTRACT_REVISION}"
        );
    }
    Ok(())
}

fn validate_ready_binding(
    expected_model: &str,
    reported_model: &str,
    expected_provider: Option<&str>,
    reported_provider: &str,
) -> Result<()> {
    if expected_model != reported_model {
        anyhow::bail!(
            "headless model binding mismatch: expected {expected_model}, reported {reported_model}"
        );
    }
    if let Some(expected_provider) = expected_provider {
        if !expected_provider.eq_ignore_ascii_case(reported_provider) {
            anyhow::bail!(
                "headless provider binding mismatch: expected {expected_provider}, reported {reported_provider}"
            );
        }
    }
    Ok(())
}

async fn await_headless_ready(
    supervisor: &mut AgentSupervisor,
    expected_model: &str,
    expected_provider: Option<&str>,
) -> Result<()> {
    tokio::time::timeout(HEADLESS_READY_TIMEOUT, async {
        loop {
            match supervisor.recv().await {
                Some(SupervisorEvent::Agent(event)) => match *event {
                    AgentEvent::Ready {
                        model, provider, ..
                    } => {
                        return validate_ready_binding(
                            expected_model,
                            &model,
                            expected_provider,
                            &provider,
                        );
                    }
                    AgentEvent::Error {
                        message,
                        fatal: true,
                        ..
                    } => anyhow::bail!("headless provider validation failed: {message}"),
                    _ => {}
                },
                Some(SupervisorEvent::Disconnected { error }) => {
                    anyhow::bail!("headless runtime exited before Ready: {error}")
                }
                Some(SupervisorEvent::ShuttingDown) | None => {
                    anyhow::bail!("headless runtime stopped before Ready")
                }
                _ => {}
            }
        }
    })
    .await
    .context("headless runtime did not become ready before startup timeout")?
}

fn set_env_value(env: &mut Vec<(String, String)>, key: &str, value: &str) {
    if let Some((_, existing_value)) = env.iter_mut().find(|(existing_key, _)| existing_key == key)
    {
        *existing_value = value.to_string();
    } else {
        env.push((key.to_string(), value.to_string()));
    }
}

fn session_recorder_id(session_id: &str) -> String {
    const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0100_0000_01b3;
    let hash = session_id
        .as_bytes()
        .iter()
        .fold(FNV_OFFSET_BASIS, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME)
        });
    let prefix = session_id
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        .take(48)
        .collect::<String>();
    let prefix = if prefix.is_empty() {
        "session"
    } else {
        &prefix
    };
    format!("{prefix}-{hash:016x}")
}

fn resolve_hosted_session_id(
    configured_session_id: Option<&str>,
    restored_session_id: Option<&str>,
    runner_session_id: &str,
) -> String {
    configured_session_id
        .or(restored_session_id)
        .unwrap_or(runner_session_id)
        .to_string()
}

fn first_env(env: &HashMap<String, String>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|key| env.get(*key))
        .map(|value| value.trim())
        .find(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn set_string(env: &mut HashMap<String, String>, key: &str, value: Option<&str>) {
    if let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) {
        env.insert(key.to_string(), value.to_string());
    }
}

fn set_path(env: &mut HashMap<String, String>, key: &str, value: Option<&PathBuf>) {
    if let Some(value) = value {
        env.insert(key.to_string(), value.to_string_lossy().to_string());
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use std::fs;
    use std::io::Write;
    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn hosted_runner_startup_branches_are_polled_concurrently() {
        let headless_started = Arc::new(tokio::sync::Notify::new());
        let preparation_started = Arc::new(tokio::sync::Notify::new());
        let headless = {
            let headless_started = headless_started.clone();
            let preparation_started = preparation_started.clone();
            async move {
                headless_started.notify_one();
                preparation_started.notified().await;
                Ok::<_, &'static str>("headless")
            }
        };
        let preparation = {
            let headless_started = headless_started.clone();
            let preparation_started = preparation_started.clone();
            async move {
                preparation_started.notify_one();
                headless_started.notified().await;
                Ok::<_, &'static str>("prepared")
            }
        };

        let joined = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            join_hosted_runner_startup(headless, preparation),
        )
        .await
        .expect("both startup branches must be polled")
        .expect("both startup branches succeed");

        assert_eq!(joined, ("headless", "prepared"));
    }

    #[tokio::test]
    async fn hosted_runner_startup_failure_drops_the_sibling_future() {
        struct DropSignal(Arc<std::sync::atomic::AtomicBool>);

        impl Drop for DropSignal {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }

        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let drop_signal = DropSignal(dropped.clone());
        let sibling = {
            async move {
                let _signal = drop_signal;
                std::future::pending::<std::result::Result<&'static str, &'static str>>().await
            }
        };
        let failed = async { Err::<&'static str, _>("headless failed") };

        assert_eq!(
            join_hosted_runner_startup(failed, sibling).await,
            Err("headless failed")
        );
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn preparation_failure_reaps_the_started_headless_child() {
        use std::os::unix::fs::PermissionsExt;

        let workspace = tempdir().expect("workspace");
        let agent = workspace.path().join("stubborn-maestro-headless.sh");
        let pid_file = workspace.path().join("headless.pid");
        fs::write(
            &agent,
            format!(
                "#!/bin/sh\nprintf '%s' \"$$\" > '{}'\nprintf '%s\\n' '{{\"type\":\"ready\",\"model\":\"gpt-5.5\",\"provider\":\"test\"}}'\nwhile IFS= read -r line; do :; done\n",
                pid_file.display()
            ),
        )
        .expect("agent script");
        let mut permissions = fs::metadata(&agent).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&agent, permissions).expect("chmod");

        let listen = "127.0.0.1:43123".to_owned();
        let config = resolve_hosted_runner_launch_config(
            [
                "deixic-code hosted-runner",
                "--runner-session-id",
                "mrs_cleanup",
                "--workspace-root",
                workspace.path().to_str().expect("workspace path"),
                "--listen",
                listen.as_str(),
                "--agent-cli-path",
                agent.to_str().expect("agent path"),
            ],
            &unauthenticated_local_env(),
        )
        .expect("launch config");
        let mut supervisor = AgentSupervisor::new(config.supervisor);
        let preparation = async {
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while !pid_file.is_file() {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("headless child starts");
            Err::<(), _>(anyhow::anyhow!("injected preparation failure"))
        };

        let error =
            prepare_while_starting_headless(&mut supervisor, "gpt-5.5", Some("test"), preparation)
                .await
                .expect_err("preparation must fail");
        assert!(error.to_string().contains("injected preparation failure"));

        let pid = fs::read_to_string(&pid_file)
            .expect("pid file")
            .parse::<libc::pid_t>()
            .expect("pid");
        assert_ne!(
            unsafe { libc::kill(pid, 0) },
            0,
            "headless child must be reaped before cleanup returns"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn activation_failure_reaps_the_ready_headless_child() {
        use std::os::unix::fs::PermissionsExt;

        let workspace = tempdir().expect("workspace");
        let agent = workspace.path().join("ready-stubborn-maestro-headless.sh");
        let pid_file = workspace.path().join("ready-headless.pid");
        fs::write(
            &agent,
            format!(
                "#!/bin/sh\nprintf '%s' \"$$\" > '{}'\nprintf '%s\\n' '{{\"type\":\"ready\",\"model\":\"gpt-5.5\",\"provider\":\"test\"}}'\nwhile IFS= read -r line; do :; done\n",
                pid_file.display()
            ),
        )
        .expect("agent script");
        let mut permissions = fs::metadata(&agent).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&agent, permissions).expect("chmod");

        let journal_dir = workspace.path().join(".maestro/hosted-runner/threads");
        fs::create_dir_all(&journal_dir).expect("journal directory");
        fs::write(
            journal_dir.join(format!(
                "{}.json",
                session_recorder_id("mrs_activation_cleanup")
            )),
            "{",
        )
        .expect("invalid journal");
        let listen = "127.0.0.1:43123".to_owned();

        let error = match start_test_hosted_runner_cli_runtime(
            [
                "deixic-code hosted-runner",
                "--runner-session-id",
                "mrs_activation_cleanup",
                "--workspace-root",
                workspace.path().to_str().expect("workspace path"),
                "--listen",
                listen.as_str(),
                "--agent-cli-path",
                agent.to_str().expect("agent path"),
            ],
            &unauthenticated_local_env(),
        )
        .await
        {
            Ok(_) => panic!("invalid thread journal must reject activation"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("start Rust hosted runner"));

        let pid = fs::read_to_string(&pid_file)
            .expect("pid file")
            .parse::<libc::pid_t>()
            .expect("pid");
        assert_ne!(
            unsafe { libc::kill(pid, 0) },
            0,
            "ready headless child must be reaped before cleanup returns"
        );
    }

    async fn start_test_hosted_runner_cli_runtime<I, T>(
        args: I,
        env: &HashMap<String, String>,
    ) -> Result<HostedRunnerCliRuntime>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        let (mut config, resolved_env) = resolve_hosted_runner_launch_config_with_env(args, env)?;
        // CLI parsing still requires an explicit nonzero port. The resolved
        // runtime supports an OS-assigned port, keeping allocation and ownership
        // in the same bind instead of probing and releasing a socket first.
        config.runner.bind_addr.set_port(0);
        start_resolved_hosted_runner_cli_runtime(config, resolved_env).await
    }

    fn unauthenticated_local_env() -> HashMap<String, String> {
        HashMap::from([
            ("MAESTRO_WEB_REQUIRE_KEY".to_string(), "0".to_string()),
            ("MAESTRO_MODEL".to_string(), "gpt-5.5".to_string()),
        ])
    }

    #[test]
    fn resolves_cli_flags_into_runner_and_supervisor_config() {
        let workspace = tempdir().expect("workspace");
        let agent = workspace.path().join("fake-maestro");
        fs::write(&agent, "#!/bin/sh\n").expect("agent");
        let port = 43123;
        let listen = format!("127.0.0.1:{port}");
        let env = HashMap::from([
            ("MAESTRO_MODEL".to_string(), "gpt-5.5".to_string()),
            ("MAESTRO_PROFILE".to_string(), "sandbox".to_string()),
            ("MAESTRO_WEB_REQUIRE_KEY".to_string(), "0".to_string()),
            (
                "MAESTRO_CAUSAL_RECEIPT_ID".to_string(),
                "causal.receipt:platform-1".to_string(),
            ),
        ]);
        let config = resolve_hosted_runner_launch_config(
            [
                "deixic-code hosted-runner",
                "--runner-session-id",
                "mrs_cli",
                "--owner-instance-id",
                "pod_cli",
                "--workspace-root",
                workspace.path().to_str().expect("workspace path"),
                "--snapshot-root",
                ".snapshots",
                "--listen",
                listen.as_str(),
                "--workspace-id",
                "ws_cli",
                "--agent-id",
                "agent_cli",
                "--agent-run-id",
                "run_cli",
                "--maestro-session-id",
                "sess_cli",
                "--attach-audience",
                "aud_cli",
                "--agent-cli-path",
                agent.to_str().expect("agent path"),
                "--from-config",
            ],
            &env,
        )
        .expect("config");

        assert_eq!(config.runner.runner_session_id, "mrs_cli");
        assert_eq!(config.runner.owner_instance_id.as_deref(), Some("pod_cli"));
        assert_eq!(config.runner.bind_addr.port(), port);
        assert_eq!(config.runner.workspace_id.as_deref(), Some("ws_cli"));
        assert_eq!(config.runner.agent_run_id.as_deref(), Some("run_cli"));
        assert_eq!(
            config.runner.causal_receipt_id.as_deref(),
            Some("causal.receipt:platform-1")
        );
        assert_eq!(
            config.runner.maestro_session_id.as_deref(),
            Some("sess_cli")
        );
        assert_eq!(config.runner.attach_audience.as_deref(), Some("aud_cli"));
        let runtime_boundary = config
            .runtime_boundary()
            .expect("launch config should expose the runtime boundary");
        assert_eq!(runtime_boundary.runner_session_id, "mrs_cli");
        assert_eq!(runtime_boundary.runtime_generation, 0);
        assert_eq!(
            runtime_boundary.workspace_root,
            config.runner.workspace_root.to_string_lossy()
        );
        assert_eq!(
            runtime_boundary.topology,
            maestro_runtime::HOSTED_RUNTIME_TOPOLOGY
        );
        assert_eq!(
            config.supervisor.transport.cli_path,
            agent.to_string_lossy()
        );
        assert!(!config.supervisor.auto_reconnect);
        assert_eq!(config.agent_id.as_deref(), Some("agent_cli"));
        assert!(
            config
                .supervisor
                .transport
                .env
                .iter()
                .any(|(key, value)| { key == "MAESTRO_AGENT_ID" && value == "agent_cli" })
        );
        assert!(
            config
                .supervisor
                .transport
                .env
                .iter()
                .any(|(key, value)| { key == "MAESTRO_PROFILE" && value == "sandbox" })
        );
        assert!(config.supervisor.transport.env.iter().any(|(key, value)| {
            key == "MAESTRO_CAUSAL_RECEIPT_ID" && value == "causal.receipt:platform-1"
        }));
        assert!(config.supervisor.transport.env.iter().any(|(key, value)| {
            key == "MAESTRO_AGENT_DIR"
                && value
                    == &config
                        .runner
                        .workspace_root
                        .join(".maestro/agent")
                        .to_string_lossy()
        }));
    }

    #[test]
    fn config_file_compiles_to_existing_runner_inputs_without_secret_leakage() {
        let workspace = tempdir().expect("workspace");
        let secret_file = workspace.path().join("static-bearer");
        let sentinel = "descriptor-static-bearer-sentinel";
        fs::write(&secret_file, format!("{sentinel}\n")).expect("static bearer");
        let descriptor = workspace.path().join("hosted-launch-spec.json");
        let mut document = json!({
            "schemaVersion": maestro_runtime::HOSTED_LAUNCH_SPEC_VERSION,
            "runtime": {
                "runnerSessionId": "descriptor-runner",
                "bindAddress": "127.0.0.1:0",
                "runtimeGeneration": 11,
                "ownerInstanceId": "  descriptor-owner  ",
                "attachAudience": "  descriptor-audience  ",
                "causalReceiptId": "descriptor-receipt"
            },
            "workspace": {
                "root": workspace.path(),
                "workspaceId": "descriptor-workspace",
                "agentRunId": "  descriptor-run  ",
                "maestroSessionId": "descriptor-session"
            },
            "identity": {"authMode": "static_bearer", "workloadIdentity": null},
            "model": {"model": "gpt-5.5", "provider": "test", "environment": "test"},
            "restore": {"snapshotRoot": workspace.path().join("snapshots"), "restoreManifestPath": null},
            "rendezvous": null,
            "secretFiles": {
                "staticBearer": &secret_file,
                "managedGatewayAccessToken": null,
                "projectedWorkloadToken": null,
                "identityTlsCa": null
            },
            "headlessCliPath": "maestro",
            "profile": "descriptor-profile",
            "agentDir": workspace.path().join("agent"),
            "agentId": "descriptor-agent"
        });
        fs::write(
            &descriptor,
            serde_json::to_vec_pretty(&document).expect("descriptor JSON"),
        )
        .expect("descriptor file");

        let (config, resolved_env) = resolve_hosted_runner_launch_config_with_env(
            [
                "deixic-code hosted-runner",
                "--config",
                descriptor.to_str().expect("descriptor path"),
            ],
            &HashMap::new(),
        )
        .expect("descriptor should resolve before listener bind");

        assert_eq!(config.runner.runner_session_id, "descriptor-runner");
        assert_eq!(config.runner.bind_addr.port(), 0);
        assert_eq!(config.runner.runtime_generation, 11);
        assert_eq!(
            config.runner.owner_instance_id.as_deref(),
            Some("descriptor-owner")
        );
        assert_eq!(
            config.runner.agent_run_id.as_deref(),
            Some("descriptor-run")
        );
        assert_eq!(
            config.runner.attach_audience.as_deref(),
            Some("descriptor-audience")
        );
        assert_eq!(config.runner.auth_token.as_deref(), Some(sentinel));
        assert_eq!(config.agent_id.as_deref(), Some("descriptor-agent"));
        assert_eq!(config.supervisor.transport.cli_path, "maestro");
        assert_eq!(
            config.supervisor.transport.cwd,
            Some(config.runner.workspace_root.to_string_lossy().into_owned())
        );
        let launch_spec = config
            .launch_spec(&resolved_env)
            .expect("resolved config should produce typed launch spec");
        assert_eq!(
            launch_spec.schema_version,
            maestro_runtime::HOSTED_LAUNCH_SPEC_VERSION
        );
        assert_eq!(launch_spec.runtime.bind_address, "127.0.0.1:0");
        assert_eq!(launch_spec.profile.as_deref(), Some("descriptor-profile"));
        assert_eq!(launch_spec.agent_id.as_deref(), Some("descriptor-agent"));
        let encoded = serde_json::to_string(&launch_spec).expect("launch spec JSON");
        assert!(encoded.contains(secret_file.to_string_lossy().as_ref()));
        assert!(!encoded.contains(sentinel));
        assert!(
            !config
                .supervisor
                .transport
                .env
                .iter()
                .any(|(_, value)| value == sentinel)
        );

        document["secretFiles"]["managedGatewayAccessToken"] =
            json!(workspace.path().join("managed-gateway-token"));
        fs::write(
            &descriptor,
            serde_json::to_vec_pretty(&document).expect("orphaned managed secret JSON"),
        )
        .expect("orphaned managed secret descriptor file");
        let error = resolve_hosted_runner_launch_config(
            [
                "deixic-code hosted-runner",
                "--config",
                descriptor.to_str().expect("descriptor path"),
            ],
            &HashMap::new(),
        )
        .expect_err("orphaned managed gateway secret must fail closed");
        assert!(
            error
                .to_string()
                .contains("managedGatewayAccessToken requires a managed model launch contract")
        );
        assert!(!error.to_string().contains(sentinel));
    }

    #[test]
    fn legacy_partial_model_metadata_still_resolves_and_produces_snapshot() {
        let workspace = tempdir().expect("workspace");
        let listen = "127.0.0.1:43123".to_owned();
        let env = HashMap::from([
            ("MAESTRO_MODEL".to_string(), "gpt-5.5".to_string()),
            ("MAESTRO_WEB_REQUIRE_KEY".to_string(), "0".to_string()),
            (
                "MAESTRO_EVALOPS_BASE_URL".to_string(),
                "https://gateway.example/v1".to_string(),
            ),
        ]);
        let (config, resolved_env) = resolve_hosted_runner_launch_config_with_env(
            [
                "deixic-code hosted-runner",
                "--runner-session-id",
                "legacy-runner",
                "--workspace-root",
                workspace.path().to_str().expect("workspace path"),
                "--listen",
                listen.as_str(),
            ],
            &env,
        )
        .expect("legacy ordinary-model launch should remain compatible");

        let spec = config
            .launch_spec(&resolved_env)
            .expect("legacy telemetry snapshot must not enforce executable model tuple");
        assert_eq!(
            spec.model.base_url.as_deref(),
            Some("https://gateway.example/v1")
        );
        assert!(spec.model.organization_id.is_none());
        assert!(spec.model.workspace_id.is_none());
        assert!(spec.model.provider.is_none());
    }

    #[test]
    fn descriptor_env_path_is_supported_and_legacy_coordinates_are_rejected() {
        let workspace = tempdir().expect("workspace");
        let secret_file = workspace.path().join("static-bearer");
        fs::write(&secret_file, "descriptor-secret\n").expect("static bearer");
        let descriptor = workspace.path().join("hosted-launch-spec.json");
        let document = json!({
            "schemaVersion": maestro_runtime::HOSTED_LAUNCH_SPEC_VERSION,
            "runtime": {"runnerSessionId": "runner", "bindAddress": "127.0.0.1:0", "runtimeGeneration": 1},
            "workspace": {"root": workspace.path()},
            "identity": {"authMode": "static_bearer", "workloadIdentity": null},
            "model": {"model": "gpt-5.5"},
            "restore": {"snapshotRoot": null, "restoreManifestPath": null},
            "rendezvous": null,
            "secretFiles": {"staticBearer": &secret_file, "managedGatewayAccessToken": null, "projectedWorkloadToken": null, "identityTlsCa": null}
        });
        fs::write(
            &descriptor,
            serde_json::to_vec(&document).expect("descriptor JSON"),
        )
        .expect("descriptor file");

        let env = HashMap::from([
            (
                HOSTED_LAUNCH_SPEC_FILE_ENV.to_string(),
                descriptor.to_string_lossy().into_owned(),
            ),
            ("PORT".to_string(), "3000".to_string()),
        ]);
        let config = resolve_hosted_runner_launch_config(["deixic-code hosted-runner"], &env)
            .expect("generic PORT must not conflict with descriptor coordinates");
        assert_eq!(config.runner.runner_session_id, "runner");

        let conflicting_env = HashMap::from([
            (
                HOSTED_LAUNCH_SPEC_FILE_ENV.to_string(),
                descriptor.to_string_lossy().into_owned(),
            ),
            (
                "MAESTRO_RUNNER_SESSION_ID".to_string(),
                "legacy".to_string(),
            ),
        ]);
        let error =
            resolve_hosted_runner_launch_config(["deixic-code hosted-runner"], &conflicting_env)
                .expect_err("descriptor must not mix env coordinates");
        assert!(error.to_string().contains("cannot be combined"));

        let conflicting_cli = resolve_hosted_runner_launch_config(
            [
                "deixic-code hosted-runner",
                "--config",
                descriptor.to_str().expect("descriptor path"),
                "--runner-session-id",
                "legacy",
            ],
            &HashMap::new(),
        )
        .expect_err("descriptor must not mix CLI coordinates");
        assert!(conflicting_cli.to_string().contains("cannot be combined"));
    }

    #[test]
    fn descriptor_rejects_outbound_rendezvous_without_workload_identity() {
        let workspace = tempdir().expect("workspace");
        let secret_file = workspace.path().join("static-bearer");
        let nonce_file = workspace.path().join("rendezvous-nonce");
        fs::write(&secret_file, "descriptor-secret\n").expect("static bearer");
        fs::write(&nonce_file, "descriptor-nonce\n").expect("rendezvous nonce");
        let descriptor = workspace.path().join("hosted-launch-spec.json");
        let document = json!({
            "schemaVersion": maestro_runtime::HOSTED_LAUNCH_SPEC_VERSION,
            "runtime": {"runnerSessionId": "runner", "bindAddress": "127.0.0.1:0", "runtimeGeneration": 1},
            "workspace": {"root": workspace.path()},
            "identity": {"authMode": "static_bearer", "workloadIdentity": null},
            "model": {"model": "gpt-5.5"},
            "restore": {"snapshotRoot": null, "restoreManifestPath": null},
            "rendezvous": {
                "mode": "outbound_shadow",
                "endpoint": "rendezvous.example:443",
                "serverName": "rendezvous.example",
                "identityExchangeUrl": "https://identity.example/exchange",
                "activationId": "00000000-0000-0000-0000-000000000008",
                "nonceFile": &nonce_file,
                "noncePresent": true
            },
            "secretFiles": {
                "staticBearer": &secret_file,
                "managedGatewayAccessToken": null,
                "projectedWorkloadToken": null,
                "identityTlsCa": null
            }
        });
        fs::write(
            &descriptor,
            serde_json::to_vec(&document).expect("descriptor JSON"),
        )
        .expect("descriptor file");

        let error = resolve_hosted_runner_launch_config(
            [
                "deixic-code hosted-runner",
                "--config",
                descriptor.to_str().expect("descriptor path"),
            ],
            &HashMap::new(),
        )
        .expect_err("outbound rendezvous must require workload identity");
        assert!(
            error
                .to_string()
                .contains("outbound rendezvous requires projected workload identity")
        );
        assert!(!error.to_string().contains("descriptor-secret"));
    }

    #[test]
    fn descriptor_session_id_is_trimmed_and_blank_is_rejected() {
        let workspace = tempdir().expect("workspace");
        let descriptor = workspace.path().join("hosted-launch-spec.json");
        let mut document = json!({
            "schemaVersion": maestro_runtime::HOSTED_LAUNCH_SPEC_VERSION,
            "runtime": {"runnerSessionId": "runner", "bindAddress": "127.0.0.1:0", "runtimeGeneration": 1},
            "workspace": {"root": workspace.path(), "workspaceId": "  descriptor-workspace  ", "maestroSessionId": "  descriptor-session  "},
            "identity": {"authMode": "none", "workloadIdentity": null},
            "model": {"model": "gpt-5.5"},
            "restore": {"snapshotRoot": null, "restoreManifestPath": null},
            "rendezvous": null,
            "secretFiles": {
                "staticBearer": null,
                "managedGatewayAccessToken": null,
                "projectedWorkloadToken": null,
                "identityTlsCa": null
            }
        });
        fs::write(
            &descriptor,
            serde_json::to_vec(&document).expect("descriptor JSON"),
        )
        .expect("descriptor file");

        let config = resolve_hosted_runner_launch_config(
            [
                "deixic-code hosted-runner",
                "--config",
                descriptor.to_str().expect("descriptor path"),
            ],
            &HashMap::new(),
        )
        .expect("padded session identity should be normalized");
        assert_eq!(
            config.runner.maestro_session_id.as_deref(),
            Some("descriptor-session")
        );
        assert_eq!(
            config.runner.workspace_id.as_deref(),
            Some("descriptor-workspace")
        );
        document["workspace"]["maestroSessionId"] = json!(" \t");
        fs::write(
            &descriptor,
            serde_json::to_vec(&document).expect("blank descriptor JSON"),
        )
        .expect("blank descriptor file");
        let error = resolve_hosted_runner_launch_config(
            [
                "deixic-code hosted-runner",
                "--config",
                descriptor.to_str().expect("descriptor path"),
            ],
            &HashMap::new(),
        )
        .expect_err("blank session identity must fail closed");
        assert!(error.to_string().contains("workspace.maestroSessionId"));

        document["workspace"]["maestroSessionId"] = json!("descriptor-session");
        document["workspace"]["workspaceId"] = json!(" \t");
        fs::write(
            &descriptor,
            serde_json::to_vec(&document).expect("blank workspace descriptor JSON"),
        )
        .expect("blank workspace descriptor file");
        let error = resolve_hosted_runner_launch_config(
            [
                "deixic-code hosted-runner",
                "--config",
                descriptor.to_str().expect("descriptor path"),
            ],
            &HashMap::new(),
        )
        .expect_err("blank workspace identity must fail closed");
        assert!(error.to_string().contains("workspace.workspaceId"));
    }

    #[test]
    fn descriptor_optional_identity_fields_reject_blank_values() {
        let workspace = tempdir().expect("workspace");
        let descriptor = workspace.path().join("hosted-launch-spec.json");
        let mut document = json!({
            "schemaVersion": maestro_runtime::HOSTED_LAUNCH_SPEC_VERSION,
            "runtime": {
                "runnerSessionId": "runner",
                "bindAddress": "127.0.0.1:0",
                "runtimeGeneration": 1,
                "ownerInstanceId": " \t"
            },
            "workspace": {"root": workspace.path(), "agentRunId": "agent-run"},
            "identity": {"authMode": "none", "workloadIdentity": null},
            "model": {"model": "gpt-5.5"},
            "restore": {"snapshotRoot": null, "restoreManifestPath": null},
            "rendezvous": null,
            "secretFiles": {
                "staticBearer": null,
                "managedGatewayAccessToken": null,
                "projectedWorkloadToken": null,
                "identityTlsCa": null
            }
        });
        fs::write(
            &descriptor,
            serde_json::to_vec(&document).expect("descriptor JSON"),
        )
        .expect("descriptor file");

        let error = resolve_hosted_runner_launch_config(
            [
                "deixic-code hosted-runner",
                "--config",
                descriptor.to_str().expect("descriptor path"),
            ],
            &HashMap::new(),
        )
        .expect_err("blank owner identity must fail closed");
        assert!(error.to_string().contains("runtime.ownerInstanceId"));

        document["runtime"]["ownerInstanceId"] = json!("owner");
        document["agentId"] = json!(" \t");
        fs::write(
            &descriptor,
            serde_json::to_vec(&document).expect("blank agent descriptor JSON"),
        )
        .expect("blank agent descriptor file");
        let error = resolve_hosted_runner_launch_config(
            [
                "deixic-code hosted-runner",
                "--config",
                descriptor.to_str().expect("descriptor path"),
            ],
            &HashMap::new(),
        )
        .expect_err("blank agent identity must fail closed");
        assert!(error.to_string().contains("agentId"));

        document["agentId"] = json!("agent");
        document["rendezvous"] = json!({
            "mode": "inbound",
            "endpoint": "rendezvous.example:not-a-port",
            "serverName": "rendezvous.example",
            "identityExchangeUrl": "https://identity.example/exchange",
            "activationId": "00000000-0000-0000-0000-000000000008",
            "nonceFile": null,
            "noncePresent": false
        });
        fs::write(
            &descriptor,
            serde_json::to_vec(&document).expect("invalid endpoint descriptor JSON"),
        )
        .expect("invalid endpoint descriptor file");
        let error = resolve_hosted_runner_launch_config(
            [
                "deixic-code hosted-runner",
                "--config",
                descriptor.to_str().expect("descriptor path"),
            ],
            &HashMap::new(),
        )
        .expect_err("invalid rendezvous endpoint must fail closed");
        assert!(error.to_string().contains("rendezvous.endpoint"));
    }

    #[test]
    fn descriptor_causal_receipt_id_uses_legacy_bounded_validation() {
        let workspace = tempdir().expect("workspace");
        let descriptor = workspace.path().join("hosted-launch-spec.json");
        let document = json!({
            "schemaVersion": maestro_runtime::HOSTED_LAUNCH_SPEC_VERSION,
            "runtime": {
                "runnerSessionId": "runner",
                "bindAddress": "127.0.0.1:0",
                "runtimeGeneration": 1,
                "causalReceiptId": "contains whitespace"
            },
            "workspace": {"root": workspace.path()},
            "identity": {"authMode": "none", "workloadIdentity": null},
            "model": {"model": "gpt-5.5"},
            "restore": {"snapshotRoot": null, "restoreManifestPath": null},
            "rendezvous": null,
            "secretFiles": {
                "staticBearer": null,
                "managedGatewayAccessToken": null,
                "projectedWorkloadToken": null,
                "identityTlsCa": null
            }
        });
        fs::write(
            &descriptor,
            serde_json::to_vec(&document).expect("descriptor JSON"),
        )
        .expect("descriptor file");

        let error = resolve_hosted_runner_launch_config(
            [
                "deixic-code hosted-runner",
                "--config",
                descriptor.to_str().expect("descriptor path"),
            ],
            &HashMap::new(),
        )
        .expect_err("descriptor causal receipt must use legacy validation");
        assert!(error.to_string().contains("MAESTRO_CAUSAL_RECEIPT_ID"));
    }

    #[test]
    fn managed_gateway_bootstrap_file_is_loaded_only_for_the_headless_child() {
        let workspace = tempdir().expect("workspace");
        let credential = workspace.path().join("gateway-token");
        fs::write(&credential, "tenant-token\n").expect("credential");
        let runner = HostedRunnerConfig::new("runner", workspace.path()).expect("runner config");
        let env = HashMap::from([
            ("MAESTRO_MODEL".to_string(), "gpt-5.5".to_string()),
            (
                "MAESTRO_EVALOPS_ACCESS_TOKEN_FILE".to_string(),
                credential.to_string_lossy().into_owned(),
            ),
        ]);

        let child_env = hosted_agent_env(&runner, &env).expect("child environment");

        assert!(child_env.iter().any(|(key, value)| {
            key == "MAESTRO_EVALOPS_ACCESS_TOKEN" && value == "tenant-token"
        }));
        assert!(
            !child_env
                .iter()
                .any(|(key, _)| key == "MAESTRO_EVALOPS_ACCESS_TOKEN_FILE")
        );
    }

    #[test]
    fn managed_gateway_configuration_is_forwarded_as_one_headless_child_cohort() {
        let workspace = tempdir().expect("workspace");
        let credential = workspace.path().join("gateway-token");
        fs::write(&credential, "tenant-token\n").expect("credential");
        let runner = HostedRunnerConfig::new("runner", workspace.path()).expect("runner config");
        let env = HashMap::from([
            (
                "MAESTRO_DEFAULT_MODEL".to_string(),
                "evalops/gpt-5.5".to_string(),
            ),
            (
                "MAESTRO_EVALOPS_ACCESS_TOKEN_FILE".to_string(),
                credential.to_string_lossy().into_owned(),
            ),
            (
                "MAESTRO_EVALOPS_BASE_URL".to_string(),
                "https://gateway.example/v1".to_string(),
            ),
            ("MAESTRO_EVALOPS_ORG_ID".to_string(), "org_1".to_string()),
            (
                "MAESTRO_EVALOPS_WORKSPACE_ID".to_string(),
                "ws_1".to_string(),
            ),
            (
                "MAESTRO_EVALOPS_PROVIDER".to_string(),
                "openrouter".to_string(),
            ),
            (
                "MAESTRO_EVALOPS_ENVIRONMENT".to_string(),
                "production".to_string(),
            ),
            (
                "MAESTRO_EVALOPS_CREDENTIAL_NAME".to_string(),
                "platform-default".to_string(),
            ),
            ("MAESTRO_EVALOPS_TEAM_ID".to_string(), "team_1".to_string()),
            (
                "MAESTRO_RESIDENT_CONTRACT_REVISION".to_string(),
                RESIDENT_MODEL_READY_CONTRACT_REVISION.to_string(),
            ),
        ]);

        let child_env = hosted_agent_env(&runner, &env).expect("child environment");
        let child_env = child_env.into_iter().collect::<HashMap<_, _>>();

        assert_eq!(
            child_env.get("MAESTRO_MODEL"),
            Some(&"evalops/gpt-5.5".to_string())
        );
        assert_eq!(
            child_env.get("MAESTRO_EVALOPS_ACCESS_TOKEN"),
            Some(&"tenant-token".to_string())
        );
        for key in [
            "MAESTRO_EVALOPS_BASE_URL",
            "MAESTRO_EVALOPS_ORG_ID",
            "MAESTRO_EVALOPS_WORKSPACE_ID",
            "MAESTRO_EVALOPS_PROVIDER",
            "MAESTRO_EVALOPS_ENVIRONMENT",
            "MAESTRO_EVALOPS_CREDENTIAL_NAME",
            "MAESTRO_EVALOPS_TEAM_ID",
            "MAESTRO_RESIDENT_CONTRACT_REVISION",
        ] {
            assert_eq!(child_env.get(key), env.get(key), "missing child key {key}");
        }
        assert!(!child_env.contains_key("MAESTRO_EVALOPS_ACCESS_TOKEN_FILE"));
    }

    #[test]
    fn managed_resident_contract_revision_must_match_before_launch() {
        let env = HashMap::from([
            (
                "MAESTRO_DEFAULT_MODEL".to_string(),
                "evalops/gpt-5.5".to_string(),
            ),
            (
                "MAESTRO_RESIDENT_CONTRACT_REVISION".to_string(),
                "maestro-resident-model-ready-v1".to_string(),
            ),
        ]);

        let error = validate_resident_contract(&env).expect_err("stale revision must fail");
        assert!(
            error
                .to_string()
                .contains(RESIDENT_MODEL_READY_CONTRACT_REVISION)
        );
    }

    #[test]
    fn managed_child_environment_rejects_missing_materialized_credential() {
        let workspace = tempdir().expect("workspace");
        let runner = HostedRunnerConfig::new("runner", workspace.path()).expect("runner config");
        let env = HashMap::from([
            ("MAESTRO_MODEL".to_string(), "evalops/gpt-5.5".to_string()),
            (
                "MAESTRO_EVALOPS_BASE_URL".to_string(),
                "https://gateway.example/v1".to_string(),
            ),
            ("MAESTRO_EVALOPS_ORG_ID".to_string(), "org_1".to_string()),
        ]);

        let error = hosted_agent_env(&runner, &env).expect_err("missing token must fail");
        assert!(error.to_string().contains("MAESTRO_EVALOPS_ACCESS_TOKEN"));
    }

    #[test]
    fn ready_binding_must_match_the_model_forwarded_to_the_child() {
        validate_ready_binding(
            "evalops/gpt-5.5",
            "evalops/gpt-5.5",
            Some("openrouter"),
            "openrouter",
        )
        .expect("matching binding");
        let error = validate_ready_binding(
            "evalops/gpt-5.5",
            "gpt-5.5",
            Some("openrouter"),
            "openrouter",
        )
        .expect_err("rewritten binding must fail");
        assert!(error.to_string().contains("model binding mismatch"));
    }

    #[test]
    fn session_recorder_id_is_stable_and_path_safe() {
        let recorder_id = session_recorder_id("../../session/with spaces");

        assert_eq!(
            recorder_id,
            session_recorder_id("../../session/with spaces")
        );
        assert!(!recorder_id.contains('/'));
        assert!(!recorder_id.contains(".."));
        assert!(recorder_id.len() <= 65);
    }

    #[test]
    fn explicit_session_id_precedes_restored_and_runner_ids() {
        assert_eq!(
            resolve_hosted_session_id(Some("explicit"), Some("restored"), "runner"),
            "explicit"
        );
        assert_eq!(
            resolve_hosted_session_id(None, Some("restored"), "runner"),
            "restored"
        );
        assert_eq!(resolve_hosted_session_id(None, None, "runner"), "runner");
    }

    #[test]
    fn shipped_managed_default_requires_the_resident_model_contract() {
        let workspace = tempdir().expect("workspace");
        let error = resolve_hosted_runner_launch_config(
            [
                "deixic-code hosted-runner",
                "--runner-session-id",
                "managed-default",
                "--listen",
                "127.0.0.1:8080",
                "--workspace-root",
                workspace.path().to_str().expect("workspace path"),
            ],
            &HashMap::new(),
        )
        .expect_err("managed default cannot bypass the resident model contract");
        assert!(
            error
                .to_string()
                .contains("MAESTRO_RESIDENT_CONTRACT_REVISION"),
            "unexpected launch error: {error:#}"
        );
    }

    #[test]
    fn cli_requires_auth_by_default_even_on_loopback() {
        let workspace = tempdir().expect("workspace");
        let args = [
            "deixic-code hosted-runner",
            "--runner-session-id",
            "mrs_auth",
            "--workspace-root",
            workspace.path().to_str().expect("workspace path"),
            "--listen",
            "127.0.0.1:8080",
        ];

        let mut env = HashMap::from([("MAESTRO_MODEL".to_string(), "gpt-5.5".to_string())]);
        let error = match resolve_hosted_runner_launch_config(args.iter().copied(), &env) {
            Ok(_) => panic!("missing auth should fail"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("MAESTRO_WEB_API_KEY"),
            "unexpected launch error: {error:#}"
        );

        env.insert(
            "MAESTRO_WEB_API_KEY".to_string(),
            "legacy-secret".to_string(),
        );
        let config = resolve_hosted_runner_launch_config(args.iter().copied(), &env)
            .expect("legacy auth should remain supported");
        assert_eq!(config.runner.auth_token.as_deref(), Some("legacy-secret"));
    }

    #[tokio::test]
    async fn starts_real_hosted_runner_with_supervised_headless_child() {
        let workspace = tempdir().expect("workspace");
        let agent = workspace.path().join("fake-maestro-headless.sh");
        let listen = "127.0.0.1:43123".to_owned();
        let mut script = fs::File::create(&agent).expect("agent script");
        writeln!(script, "#!/bin/sh").expect("write shebang");
        writeln!(
            script,
            "printf '%s\\n' '{{\"type\":\"ready\",\"model\":\"gpt-5.5\",\"provider\":\"test\",\"session_id\":\"sess_fake\"}}'"
        )
        .expect("write ready");
        writeln!(script, "while IFS= read -r line; do").expect("write loop");
        writeln!(
            script,
            "  printf '%s\\n' '{{\"type\":\"status\",\"message\":\"echo\"}}'"
        )
        .expect("write status");
        writeln!(script, "done").expect("write done");
        drop(script);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&agent).expect("metadata").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&agent, permissions).expect("chmod");
        }

        let runtime = start_test_hosted_runner_cli_runtime(
            [
                "deixic-code hosted-runner",
                "--runner-session-id",
                "mrs_real",
                "--workspace-root",
                workspace.path().to_str().expect("workspace path"),
                "--listen",
                listen.as_str(),
                "--agent-cli-path",
                agent.to_str().expect("agent path"),
            ],
            &unauthenticated_local_env(),
        )
        .await
        .expect("runtime");

        let identity: serde_json::Value = reqwest::get(format!(
            "{}/.well-known/evalops/remote-runner/identity",
            runtime.base_url()
        ))
        .await
        .expect("identity response")
        .json()
        .await
        .expect("identity json");
        assert_eq!(identity["runner_session_id"], "mrs_real");
        assert_eq!(identity["ready"], true);

        let drain = runtime
            .drain_for_shutdown(HostedRunnerShutdownSignal::Terminate)
            .await
            .expect("shutdown drain");
        assert_eq!(drain["status"], "drained");
        assert_eq!(drain["reason"], "process_shutdown");
        assert_eq!(drain["requested_by"], "maestro-hosted-runner");
        assert!(
            drain["manifest_path"]
                .as_str()
                .map(|path| PathBuf::from(path).is_file())
                .unwrap_or(false)
        );

        let post_drain_identity: serde_json::Value = reqwest::get(format!(
            "{}/.well-known/evalops/remote-runner/identity",
            runtime.base_url()
        ))
        .await
        .expect("post-drain identity response")
        .json()
        .await
        .expect("post-drain identity json");
        assert_eq!(post_drain_identity["ready"], false);
        assert_eq!(post_drain_identity["draining"], true);

        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn restore_manifest_replays_semantic_conversation_after_init() {
        let workspace = tempdir().expect("workspace");
        let source_agent = workspace.path().join("source-agent.sh");
        let mut source_script = fs::File::create(&source_agent).expect("source agent script");
        writeln!(source_script, "#!/bin/sh").expect("write shebang");
        writeln!(
            source_script,
            "printf '%s\\n' '{{\"type\":\"ready\",\"model\":\"gpt-5.5\",\"provider\":\"test\",\"session_id\":\"sess_restore\"}}'"
        )
        .expect("write ready");
        writeln!(source_script, "while IFS= read -r line; do").expect("write loop");
        writeln!(source_script, "  case \"$line\" in").expect("write case");
        writeln!(source_script, "    *'\"type\":\"prompt\"'*)").expect("write prompt case");
        writeln!(
            source_script,
            "      printf '%s\\n' '{{\"type\":\"conversation_snapshot\",\"protocol_version\":\"evalops.maestro.semantic-conversation.v1\",\"messages\":[{{\"role\":\"user\",\"content\":\"first turn\"}},{{\"role\":\"assistant\",\"content\":[{{\"type\":\"tool_use\",\"id\":\"tool-call-1\",\"name\":\"bash\",\"input\":{{\"command\":\"pwd\"}}}}]}},{{\"role\":\"user\",\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"tool-call-1\",\"content\":\"unbounded source tool output\"}}]}}]}}'"
        )
        .expect("write runtime snapshot");
        writeln!(source_script, "      ;;").expect("write case end");
        writeln!(source_script, "  esac").expect("write case close");
        writeln!(source_script, "done").expect("write loop end");
        drop(source_script);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&source_agent).expect("metadata").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&source_agent, permissions).expect("chmod");
        }

        // Let the runtime retain the socket allocated by the OS; probing and
        // releasing an unused port races with concurrent tests.
        let source_listen = "127.0.0.1:43123".to_owned();
        let source = start_test_hosted_runner_cli_runtime(
            [
                "deixic-code hosted-runner",
                "--runner-session-id",
                "mrs_source",
                "--workspace-root",
                workspace.path().to_str().expect("workspace path"),
                "--listen",
                source_listen.as_str(),
                "--maestro-session-id",
                "sess_restore",
                "--agent-cli-path",
                source_agent.to_str().expect("source agent path"),
            ],
            &unauthenticated_local_env(),
        )
        .await
        .expect("source runtime");
        let client = reqwest::Client::new();
        let connection: serde_json::Value = client
            .post(format!("{}/api/headless/connections", source.base_url()))
            .json(&json!({
                "sessionId": "sess_restore",
                "connectionId": "conn_restore",
                "role": "controller"
            }))
            .send()
            .await
            .expect("source connection response")
            .json()
            .await
            .expect("source connection json");
        assert_eq!(connection["connection_id"], "conn_restore");
        let connection_capability = connection["connection_capability"]
            .as_str()
            .expect("source connection capability");
        let subscription: serde_json::Value = client
            .post(format!(
                "{}/api/headless/sessions/sess_restore/subscribe",
                source.base_url()
            ))
            .json(&json!({
                "connectionId": "conn_restore",
                "connectionCapability": connection_capability,
                "connectionCapabilityRequired": true,
                "role": "controller"
            }))
            .send()
            .await
            .expect("source subscription response")
            .json()
            .await
            .expect("source subscription json");
        let subscription_id = subscription["subscription_id"]
            .as_str()
            .expect("source subscription id");
        client
            .post(format!(
                "{}/api/headless/sessions/sess_restore/messages",
                source.base_url()
            ))
            .header("x-maestro-headless-connection-id", "conn_restore")
            .header("x-maestro-headless-subscriber-id", subscription_id)
            .header(
                "x-maestro-headless-connection-capability",
                connection_capability,
            )
            .json(&json!({
                "type": "hello",
                "protocol_version": crate::headless::HEADLESS_PROTOCOL_VERSION
            }))
            .send()
            .await
            .expect("source hello response")
            .error_for_status()
            .expect("source hello status");
        client
            .post(format!(
                "{}/api/headless/sessions/sess_restore/messages",
                source.base_url()
            ))
            .header("x-maestro-headless-connection-id", "conn_restore")
            .header("x-maestro-headless-subscriber-id", subscription_id)
            .header(
                "x-maestro-headless-connection-capability",
                connection_capability,
            )
            .json(&json!({
                "type": "init",
                "system_prompt": "restore system prompt",
                "append_system_prompt": "restore append prompt",
                "thinking_level": "high",
                "approval_mode": "prompt"
            }))
            .send()
            .await
            .expect("source init response")
            .error_for_status()
            .expect("source init status");
        let semantic_messages = vec![
            crate::ai::Message {
                role: crate::ai::Role::User,
                content: crate::ai::MessageContent::Text("first turn".to_string()),
            },
            crate::ai::Message {
                role: crate::ai::Role::Assistant,
                content: crate::ai::MessageContent::Blocks(vec![
                    crate::ai::ContentBlock::ToolUse {
                        id: "tool-call-1".to_string(),
                        name: "bash".to_string(),
                        input: json!({ "command": "pwd" }),
                        gemini_context: None,
                    },
                ]),
            },
            crate::ai::Message {
                role: crate::ai::Role::User,
                content: crate::ai::MessageContent::Blocks(vec![
                    crate::ai::ContentBlock::ToolResult {
                        tool_use_id: "tool-call-1".to_string(),
                        content: "/workspace".to_string(),
                        is_error: None,
                    },
                ]),
            },
        ];
        client
            .post(format!(
                "{}/api/headless/sessions/sess_restore/messages",
                source.base_url()
            ))
            .header("x-maestro-headless-connection-id", "conn_restore")
            .header("x-maestro-headless-subscriber-id", subscription_id)
            .header(
                "x-maestro-headless-connection-capability",
                connection_capability,
            )
            .json(&json!({ "type": "prompt", "content": "first turn" }))
            .send()
            .await
            .expect("source prompt response")
            .error_for_status()
            .expect("source prompt status");
        let session_path = workspace
            .path()
            .join(".maestro/sessions")
            .join(format!("{}.jsonl", session_recorder_id("sess_restore")));
        let replay_path = session_path.with_extension("replay.json");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let replay_ready = fs::read_to_string(&replay_path)
                    .ok()
                    .and_then(|_| {
                        SessionRecorder::resume(
                            workspace.path().join(".maestro/sessions"),
                            &session_recorder_id("sess_restore"),
                        )
                        .ok()
                    })
                    .and_then(|recorder| recorder.replay().semantic_conversation)
                    .is_some();
                if replay_ready {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("runtime semantic checkpoint timeout");
        let drain = source
            .drain_for_shutdown(HostedRunnerShutdownSignal::Terminate)
            .await
            .expect("source drain");
        assert_eq!(drain["manifest"]["runtime"]["flush_status"], "completed");
        let manifest_path = PathBuf::from(
            drain["manifest_path"]
                .as_str()
                .expect("source manifest path"),
        );
        let session_file = PathBuf::from(
            drain["manifest"]["runtime"]["session_file"]
                .as_str()
                .expect("source session file"),
        );
        assert!(session_file.is_file());
        let source_entry_count = fs::read_to_string(&session_file)
            .expect("source session log")
            .lines()
            .count();
        let metadata_file = session_file.with_extension("meta.json");
        let source_metadata: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(&metadata_file).expect("source session metadata"),
        )
        .expect("source session metadata json");
        source.shutdown().await;

        let restored_agent = workspace.path().join("restored-agent.sh");
        let restored_env = workspace.path().join("restored-session.txt");
        let restored_messages = workspace.path().join("restored-messages.jsonl");
        let mut restored_script = fs::File::create(&restored_agent).expect("restored agent script");
        writeln!(restored_script, "#!/bin/sh").expect("write shebang");
        writeln!(
            restored_script,
            "printf '%s' \"$MAESTRO_SESSION_ID\" > '{}'",
            restored_env.display()
        )
        .expect("write session capture");
        writeln!(
            restored_script,
            "printf '%s\\n' \"{{\\\"type\\\":\\\"ready\\\",\\\"model\\\":\\\"gpt-5.5\\\",\\\"provider\\\":\\\"test\\\",\\\"session_id\\\":\\\"$MAESTRO_SESSION_ID\\\"}}\""
        )
        .expect("write ready");
        writeln!(restored_script, "while IFS= read -r line; do").expect("write loop");
        writeln!(
            restored_script,
            "  printf '%s\\n' \"$line\" >> '{}'",
            restored_messages.display()
        )
        .expect("write message capture");
        writeln!(restored_script, "done").expect("write loop end");
        drop(restored_script);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&restored_agent)
                .expect("metadata")
                .permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&restored_agent, permissions).expect("chmod");
        }

        let restored_listen = "127.0.0.1:43123".to_owned();
        let restored = start_test_hosted_runner_cli_runtime(
            [
                "deixic-code hosted-runner",
                "--runner-session-id",
                "mrs_restored",
                "--workspace-root",
                workspace.path().to_str().expect("workspace path"),
                "--restore-manifest",
                manifest_path.to_str().expect("manifest path"),
                "--listen",
                restored_listen.as_str(),
                "--agent-cli-path",
                restored_agent.to_str().expect("restored agent path"),
            ],
            &unauthenticated_local_env(),
        )
        .await
        .expect("restored runtime");

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let messages = fs::read_to_string(&restored_messages).unwrap_or_default();
                if messages.contains("restore system prompt")
                    && messages.contains("\"type\":\"restore_conversation\"")
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("restored init replay timeout");
        assert_eq!(
            fs::read_to_string(&restored_env).expect("restored session capture"),
            "sess_restore"
        );
        let messages = fs::read_to_string(&restored_messages).expect("restored messages");
        let init: serde_json::Value = serde_json::from_str(
            messages
                .lines()
                .find(|line| line.contains("restore system prompt"))
                .expect("restored init line"),
        )
        .expect("restored init json");
        assert_eq!(init["type"], "init");
        assert_eq!(init["system_prompt"], "restore system prompt");
        assert_eq!(init["append_system_prompt"], "restore append prompt");
        assert_eq!(init["thinking_level"], "high");
        assert_eq!(init["approval_mode"], "prompt");
        let parsed_messages = messages
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("restored message"))
            .collect::<Vec<_>>();
        let init_index = parsed_messages
            .iter()
            .position(|message| message["type"] == "init")
            .expect("restored init");
        let restore_index = parsed_messages
            .iter()
            .position(|message| message["type"] == "restore_conversation")
            .expect("restored semantic conversation");
        assert!(init_index < restore_index, "restore must follow init");
        let mut expected_semantic_messages = semantic_messages.clone();
        if let crate::ai::MessageContent::Blocks(blocks) =
            &mut expected_semantic_messages[2].content
        {
            if let crate::ai::ContentBlock::ToolResult { content, .. } = &mut blocks[0] {
                *content = "[tool result omitted from checkpoint]".to_string();
            }
        }
        assert_eq!(
            parsed_messages[restore_index]["messages"],
            serde_json::to_value(expected_semantic_messages).expect("semantic messages json")
        );
        let restored_drain = restored
            .drain_for_shutdown(HostedRunnerShutdownSignal::Terminate)
            .await
            .expect("restored drain");
        assert_eq!(
            restored_drain["manifest"]["runtime"]["session_file"],
            session_file.to_string_lossy().as_ref()
        );
        let restored_entry_count = fs::read_to_string(&session_file)
            .expect("restored session log")
            .lines()
            .count();
        assert!(restored_entry_count > source_entry_count);
        let restored_metadata: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(metadata_file).expect("restored session metadata"),
        )
        .expect("restored session metadata json");
        assert_eq!(
            restored_metadata["created_at"],
            source_metadata["created_at"]
        );
        assert!(
            restored_metadata["message_count"].as_u64() > source_metadata["message_count"].as_u64()
        );
        restored.shutdown().await;
    }
}
