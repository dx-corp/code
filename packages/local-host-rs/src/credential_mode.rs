//! Product credential mode: managed inference through EvalOps Identity, or
//! local BYOK through a direct provider credential.
//!
//! A managed / `evalops` / `maestro-managed` route requires a live EvalOps
//! Identity session (workspace-scoped for llm-gateway, or the hosted Runner
//! Host service credential). A direct provider explicitly selected with its
//! own API key (OpenAI, Anthropic, Google, xAI, OpenRouter, and other local
//! BYOK sources) constructs a native agent without Identity. Explicit local
//! runtime routes never contact Identity, even when a stored session exists,
//! so an unavailable refresh or introspection endpoint cannot block offline
//! inference. Other present Identity sessions are still verified rather than
//! ignored. Hosted residents use the established tenant-bound Runner Host
//! service credential exclusively for managed inference.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ai::{
    ProviderRegistry, canonical_managed_credential_name, canonical_managed_environment,
};
use crate::init_cli::EvalOpsCredentialSnapshot;
use crate::service_connections::ConnectionStore;

/// Shipped model route. Credentials remain in the gateway, never in the client.
pub const DEFAULT_MANAGED_MODEL: &str = "evalops/accounts/fireworks/models/glm-5p3";
pub const DEFAULT_MANAGED_CREDENTIAL_NAME: &str = "deixic-llm-gateway-glm53";

pub const ACCESS_TOKEN_ENV: &str = "MAESTRO_EVALOPS_ACCESS_TOKEN";
pub const ACCESS_TOKEN_FILE_ENV: &str = "MAESTRO_EVALOPS_ACCESS_TOKEN_FILE";
pub const BASE_URL_ENV: &str = "MAESTRO_EVALOPS_BASE_URL";
pub const ORG_ID_ENV: &str = "MAESTRO_EVALOPS_ORG_ID";
pub const WORKSPACE_ID_ENV: &str = "MAESTRO_EVALOPS_WORKSPACE_ID";
pub const PROVIDER_ENV: &str = "MAESTRO_EVALOPS_PROVIDER";
pub const ENVIRONMENT_ENV: &str = "MAESTRO_EVALOPS_ENVIRONMENT";
pub const CREDENTIAL_NAME_ENV: &str = "MAESTRO_EVALOPS_CREDENTIAL_NAME";
pub const TEAM_ID_ENV: &str = "MAESTRO_EVALOPS_TEAM_ID";

pub const IDENTITY_REQUIRED_MESSAGE: &str = "An EvalOps Identity account is required. Run `deixic-code evalops login` to sign in or create one.";
pub const NOT_READY_MESSAGE: &str =
    "No usable local provider credential. Run `deixic-code setup --byok`.";

const IDENTITY_REQUIRED_SCOPE: &str = "llm_gateway:invoke";
const IDENTITY_INTROSPECTION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// The deliberately small projection used to validate a local Identity
/// session. Tenant scope is derived only from this signed Identity response;
/// caller-provided `MAESTRO_EVALOPS_ORG_ID` / workspace values are never
/// trusted for a user-facing model turn.
#[derive(Debug, Deserialize)]
pub(crate) struct IdentityIntrospection {
    #[serde(default)]
    active: bool,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    token_type: String,
    #[serde(default)]
    organization_id: Option<String>,
    #[serde(default)]
    workspace_id: Option<String>,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default)]
    scope: String,
}

/// Admission purpose stays attached until provider selection. A hosted service
/// credential must never become a human login or unlock local credentials.
enum VerifiedIdentity {
    Human(PlatformSession),
    HostedManaged(PlatformSession),
}

impl VerifiedIdentity {
    fn into_telemetry_session(self) -> PlatformSession {
        match self {
            Self::Human(session) | Self::HostedManaged(session) => session,
        }
    }

    fn require_human(self) -> Result<PlatformSession> {
        match self {
            Self::Human(session) => Ok(session),
            Self::HostedManaged(_) => bail!("{IDENTITY_REQUIRED_MESSAGE}"),
        }
    }
}

/// A minimal live Identity endpoint shared by process-isolated Maestro tests.
///
/// Child fixtures must use the real verification path rather than implicitly
/// skipping Identity admission just because they are test binaries. The parent
/// test process keeps this loopback endpoint alive and passes its URL into the
/// child explicitly.
#[cfg(any(test, feature = "test-support"))]
pub fn test_identity_base_url() -> &'static str {
    use std::io::{Read as _, Write as _};
    use std::sync::OnceLock;

    static IDENTITY_BASE_URL: OnceLock<String> = OnceLock::new();
    IDENTITY_BASE_URL
        .get_or_init(|| {
            let listener = std::net::TcpListener::bind("127.0.0.1:0")
                .expect("bind test Identity server");
            let address = listener.local_addr().expect("test Identity address");
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else {
                        continue;
                    };
                    let mut request = [0_u8; 4 * 1024];
                    let n = stream.read(&mut request).unwrap_or(0);
                    let request = String::from_utf8_lossy(&request[..n]);
                    let (status, body) = test_identity_introspect_response(&request);
                    let reason = match status {
                        200 => "OK",
                        401 => "Unauthorized",
                        404 => "Not Found",
                        _ => "Error",
                    };
                    let response = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
                }
            });
            format!("http://{address}")
        })
        .as_str()
}

#[cfg(any(test, feature = "test-support"))]
const TEST_IDENTITY_ACTIVE_BODY: &str = r#"{"active":true,"subject":"user-test","token_type":"access","organization_id":"org-test","workspace_id":"workspace-test","scopes":["llm_gateway:invoke"]}"#;

#[cfg(any(test, feature = "test-support"))]
fn test_identity_introspect_response(request: &str) -> (u16, &'static str) {
    let first_line = request.lines().next().unwrap_or_default();
    let method_path = first_line.to_ascii_lowercase();
    if !method_path.starts_with("post ") || !method_path.contains("/v1/tokens/introspect") {
        return (404, r#"{"active":false}"#);
    }
    let token = request.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if !name.eq_ignore_ascii_case("authorization") {
            return None;
        }
        let value = value.trim();
        let (scheme, remainder) = value.split_once(' ')?;
        if !scheme.eq_ignore_ascii_case("bearer") {
            return None;
        }
        let token = remainder.trim();
        (!token.is_empty()).then_some(token)
    });
    match token {
        Some("desktop-hosted-token") => (
            200,
            r#"{"active":true,"subject":"user-test","token_type":"access","organization_id":"org-test","workspace_id":"workspace-test","scopes":["llm_gateway:invoke","console:read","console:write"]}"#,
        ),
        Some("desktop-readonly-token") => (
            200,
            r#"{"active":true,"subject":"user-test","token_type":"access","organization_id":"org-test","workspace_id":"workspace-test","scopes":["llm_gateway:invoke","console:read"]}"#,
        ),
        Some("desktop-writeonly-token") => (
            200,
            r#"{"active":true,"subject":"user-test","token_type":"access","organization_id":"org-test","workspace_id":"workspace-test","scopes":["llm_gateway:invoke","console:write"]}"#,
        ),
        Some("inactive-token") => (
            200,
            r#"{"active":false,"subject":"user-test","token_type":"access","organization_id":"org-test","workspace_id":"workspace-test","scopes":["llm_gateway:invoke"]}"#,
        ),
        Some("missing-workspace-token") => (
            200,
            r#"{"active":true,"subject":"user-test","token_type":"access","organization_id":"org-test","scopes":["llm_gateway:invoke"]}"#,
        ),
        Some("unscoped-token") => (
            200,
            r#"{"active":true,"subject":"user-test","token_type":"access","organization_id":"org-test","workspace_id":"workspace-test","scopes":[]}"#,
        ),
        Some(_) => (200, TEST_IDENTITY_ACTIVE_BODY),
        None => (401, r#"{"active":false}"#),
    }
}

/// Process-global Identity env used by Maestro tests that construct a live
/// native agent. Callers must hold [`crate::config::test_process_env_lock`]
/// and restore the previous values on drop.
#[cfg(any(test, feature = "test-support"))]
pub fn install_test_identity_env() {
    std::env::set_var(ACCESS_TOKEN_ENV, "fixture-evalops-access-token");
    std::env::set_var(ORG_ID_ENV, "fixture-evalops-org");
    std::env::set_var("MAESTRO_IDENTITY_URL", test_identity_base_url());
    std::env::set_var(crate::init_cli::TEST_IDENTITY_AUTHORITY_ENV, "1");
}

#[cfg(any(test, feature = "test-support"))]
pub const TEST_IDENTITY_ENV_VARS: &[&str] = &[
    ACCESS_TOKEN_ENV,
    ACCESS_TOKEN_FILE_ENV,
    ORG_ID_ENV,
    WORKSPACE_ID_ENV,
    "MAESTRO_IDENTITY_URL",
    crate::init_cli::TEST_IDENTITY_AUTHORITY_ENV,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialModeKind {
    Platform,
    Byok,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlatformSession {
    pub access_token: String,
    pub organization_id: String,
    pub workspace_id: Option<String>,
    pub provider_ref: Value,
    pub email: Option<String>,
    pub user_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DetectedMode {
    Platform(PlatformSession),
    Byok,
}

impl DetectedMode {
    #[must_use]
    pub fn kind(&self) -> CredentialModeKind {
        match self {
            Self::Platform(_) => CredentialModeKind::Platform,
            Self::Byok => CredentialModeKind::Byok,
        }
    }

    #[must_use]
    pub fn is_platform(&self) -> bool {
        matches!(self, Self::Platform(_))
    }
}

/// Detect the active product mode from a snapshot and environment map.
///
/// Canonical identity env vars are `MAESTRO_EVALOPS_ACCESS_TOKEN` and
/// `MAESTRO_EVALOPS_ORG_ID`. Legacy aliases are ignored.
pub fn detect_from(
    snapshot: Option<&EvalOpsCredentialSnapshot>,
    env: &HashMap<String, String>,
) -> Option<DetectedMode> {
    if let Some(session) = platform_session_from(snapshot, env) {
        return Some(DetectedMode::Platform(session));
    }
    Some(DetectedMode::Byok)
}

/// Detect mode from stored EvalOps credentials and the process environment.
pub fn detect() -> Result<DetectedMode> {
    let snapshot = crate::init_cli::load_evalops_snapshot().ok().flatten();
    let env = std::env::vars().collect();
    Ok(detect_from(snapshot.as_ref(), &env).unwrap_or(DetectedMode::Byok))
}

/// Evaluate whether a supplied Identity session or local credential can serve
/// `model`.
///
/// A direct provider with a usable local key is BYOK without Identity. An
/// explicit `openrouter/...` route stays BYOK even after Identity is
/// established. Other local provider credentials also select BYOK after
/// Identity, while a scoped session without a local credential uses
/// llm-gateway. Managed / `evalops` routes, Codex app-server transports, and
/// any model with no local credential still require Identity. Production
/// model construction calls [`require_ready`]; this deterministic helper is
/// retained for diagnostics and unit tests.
pub fn require_ready_from(
    snapshot: Option<&EvalOpsCredentialSnapshot>,
    env: &HashMap<String, String>,
    model: &str,
) -> Result<DetectedMode> {
    if let Some(session) = platform_session_from(snapshot, env) {
        return ready_mode_from_session(session, env, model);
    }
    if admits_direct_provider_without_identity(model, env) {
        return Ok(DetectedMode::Byok);
    }
    bail!("{IDENTITY_REQUIRED_MESSAGE}");
}

fn ready_mode_from_session(
    session: PlatformSession,
    env: &HashMap<String, String>,
    model: &str,
) -> Result<DetectedMode> {
    if prefers_local_byok(model) {
        if byok_ready(model, env) {
            return Ok(DetectedMode::Byok);
        }
        bail!("{NOT_READY_MESSAGE}");
    }
    if byok_ready(model, env) || is_delegated_byok_transport(model) {
        return Ok(DetectedMode::Byok);
    }

    if session
        .workspace_id
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
    {
        Ok(DetectedMode::Platform(session))
    } else {
        bail!("EvalOps managed provider requires {WORKSPACE_ID_ENV}")
    }
}

pub fn require_ready(model: &str) -> Result<DetectedMode> {
    require_ready_with_identity(model).map(|(mode, _identity)| mode)
}

/// Construct only the signed customer route; shared by native turns and auxiliary model calls.
pub fn disconnected_client(model: &str) -> Result<Option<crate::ai::UnifiedClient>> {
    let Some(route) = crate::safety::disconnected_route(model).map_err(anyhow::Error::msg)? else {
        return Ok(None);
    };
    if let Some(reason) = crate::safety::check_model_allowed(model) {
        anyhow::bail!(reason);
    }
    let credential = match route.credential_env {
        Some(name) => std::env::var(&name)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("Disconnected route credential {name} is missing"))?,
        None => String::new(),
    };
    anyhow::ensure!(
        !credential.starts_with("op://"),
        "Disconnected routes do not execute credential helpers"
    );
    let client =
        crate::ai::UnifiedClient::OpenAI(crate::ai::OpenAiClient::with_disconnected_endpoint(
            credential,
            route.endpoint,
            model.split_once('/').expect("validated model route").0,
        )?);
    Ok(Some(client))
}

/// Resolve a ready provider mode together with the live Identity session that
/// authorized it, when one exists.
///
/// Direct-provider BYOK remains `DetectedMode::Byok` and does not contact
/// Identity when no session is present. Explicit local runtime routes also
/// stay unbound when a stored session exists: they neither need network
/// authority nor receive cached tenant scope. Other stored or env Identity
/// sessions are still verified before they authorize managed inference or
/// bind telemetry. Native telemetry uses that live session to pin a completed
/// turn to its originating tenant rather than rediscovering whatever account
/// happens to be active during a later retry.
pub fn require_ready_with_identity(model: &str) -> Result<(DetectedMode, Option<PlatformSession>)> {
    if crate::safety::disconnected_route(model)
        .map_err(anyhow::Error::msg)?
        .is_some()
    {
        return Ok((DetectedMode::Byok, None));
    }
    let env = std::env::vars().collect::<HashMap<String, String>>();
    if !hosted_runner_mode(&env)
        && crate::local_models::is_local_model_route(model)
        && admits_direct_provider_without_identity(model, &env)
    {
        return Ok((DetectedMode::Byok, None));
    }
    if !hosted_runner_mode(&env)
        && platform_session_from(None, &env).is_none()
        && crate::init_cli::load_evalops_snapshot()
            .ok()
            .flatten()
            .is_none()
        && admits_direct_provider_without_identity(model, &env)
    {
        return Ok((DetectedMode::Byok, None));
    }
    let (identity, env) = current_verified_identity_session_with_env()?;
    ready_mode_from_verified_identity(identity, env, model)
        .map(|(mode, session)| (mode, Some(session)))
}

fn ready_mode_from_verified_identity(
    identity: VerifiedIdentity,
    mut env: HashMap<String, String>,
    model: &str,
) -> Result<(DetectedMode, PlatformSession)> {
    match identity {
        VerifiedIdentity::Human(session) => {
            crate::service_connections::ConnectionBroker::merge_default_for_model(model, &mut env)?;
            let mode = ready_mode_from_session(session.clone(), &env, model)?;
            Ok((mode, session))
        }
        VerifiedIdentity::HostedManaged(session) => {
            if !ProviderRegistry::resolve_descriptor(model)
                .is_ok_and(|descriptor| descriptor.id == "evalops")
            {
                bail!("hosted Runner Host credentials require a managed model route");
            }
            Ok((DetectedMode::Platform(session.clone()), session))
        }
    }
}

/// Return live verified authority for the content-free first-party telemetry
/// outbox. That endpoint accepts the same invocation scope for human and hosted
/// service credentials and independently reauthorizes the exact tenant.
/// Human-only capture and provider admission use their separate typed guards.
pub fn current_verified_identity_session() -> Result<PlatformSession> {
    crate::safety::require_vendor_network()?;
    current_verified_identity_session_with_env()
        .map(|(identity, _env)| identity.into_telemetry_session())
}

fn current_verified_identity_session_with_env()
-> Result<(VerifiedIdentity, HashMap<String, String>)> {
    let mut env = std::env::vars().collect::<HashMap<String, String>>();
    let hosted = identity_verification_endpoint(None, &env)?.1.is_some();
    // An explicit Identity session is sufficient and avoids touching the
    // platform credential store (which may be an interactive OS keychain).
    // Fall back to the stored OAuth snapshot only when the process did not
    // provide both canonical Identity values.
    let snapshot = if platform_session_from(None, &env).is_some() {
        None
    } else if hosted {
        bail!("hosted Runner Host admission requires its explicit tenant credential");
    } else {
        crate::init_cli::load_current_evalops_snapshot()?
    };
    if !hosted {
        let _ = crate::codex_auth::merge_codex_auth_snapshot_into_env(
            &mut env,
            crate::codex_auth::read_codex_auth(),
            false,
        );
    }
    let identity = verify_live_runtime_identity(snapshot.as_ref(), &env, &[])?;
    Ok((identity, env))
}

/// Load and verify the current human Identity session for a product-owned
/// child operation such as Session History ingestion.
pub fn verified_current_identity_session() -> Result<PlatformSession> {
    crate::safety::require_vendor_network()?;
    let env = std::env::vars().collect::<HashMap<String, String>>();
    let snapshot = if platform_session_from(None, &env).is_some() {
        None
    } else {
        crate::init_cli::load_current_evalops_snapshot()?
    };
    verify_live_identity_session(snapshot.as_ref(), &env)
}

/// Verify a desktop-held access token without reading or replacing the CLI
/// credential. The expected tenant is compared after live introspection.
pub fn verified_desktop_identity_session(
    access_token: &str,
    organization_id: &str,
    workspace_id: &str,
) -> Result<PlatformSession> {
    crate::safety::require_vendor_network()?;
    if access_token.trim().is_empty()
        || organization_id.trim().is_empty()
        || workspace_id.trim().is_empty()
    {
        bail!("desktop Identity credential is incomplete");
    }
    let mut env = std::env::vars()
        .filter(|(key, _)| {
            matches!(
                key.as_str(),
                "MAESTRO_IDENTITY_URL" | "EVALOPS_IDENTITY_URL"
            ) || key == crate::init_cli::TEST_IDENTITY_AUTHORITY_ENV
        })
        .collect::<HashMap<_, _>>();
    env.insert(ACCESS_TOKEN_ENV.to_owned(), access_token.to_owned());
    env.insert(ORG_ID_ENV.to_owned(), organization_id.to_owned());
    env.insert(WORKSPACE_ID_ENV.to_owned(), workspace_id.to_owned());
    let session = verify_live_runtime_identity(None, &env, &["console:read", "console:write"])?
        .require_human()?;
    if session.organization_id != organization_id
        || session.workspace_id.as_deref() != Some(workspace_id)
    {
        bail!("desktop Identity tenant differs from verified token");
    }
    Ok(session)
}

/// Return a replacement verified session only when the credential backing a
/// long-running capture hook changed or reached its refresh window.
///
/// The common event path performs no network request. Stored OAuth credentials
/// are refreshed and re-introspected only after expiry approaches, while an
/// explicit environment token remains the caller's lifecycle responsibility.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn refreshed_identity_session_for_capture(
    current_access_token: &str,
) -> Result<Option<PlatformSession>> {
    let env = std::env::vars().collect::<HashMap<String, String>>();
    if let Some(session) = platform_session_from(None, &env) {
        return if session.access_token == current_access_token {
            Ok(None)
        } else {
            verify_live_identity_session(None, &env).map(Some)
        };
    }

    let Some(snapshot) = crate::init_cli::load_evalops_snapshot()? else {
        bail!(IDENTITY_REQUIRED_MESSAGE);
    };
    if snapshot.access == current_access_token
        && snapshot.expires > chrono::Utc::now().timestamp_millis() + 60_000
    {
        return Ok(None);
    }

    let snapshot =
        crate::init_cli::load_current_evalops_snapshot()?.context(IDENTITY_REQUIRED_MESSAGE)?;
    verify_live_identity_session(Some(&snapshot), &env).map(Some)
}

// The hosted exchange endpoint is an existing operator-provided coordinate, not
// discovery authority. Only this established private Identity origin may receive
// admission tokens; arbitrary URLs and local trust overrides remain rejected.
const HOSTED_IDENTITY_ORIGIN: &str = "https://identity-service.evalops.svc.cluster.local:8080";
const HOSTED_IDENTITY_EXCHANGE: &str = "https://identity-service.evalops.svc.cluster.local:8080/internal/v1/kubernetes-workload-certificates/exchange";

fn hosted_runner_mode(env: &HashMap<String, String>) -> bool {
    env.get("MAESTRO_HOSTED_RUNNER_MODE").is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn identity_verification_endpoint(
    snapshot: Option<&EvalOpsCredentialSnapshot>,
    env: &HashMap<String, String>,
) -> Result<(String, Option<std::path::PathBuf>)> {
    let hosted = hosted_runner_mode(env);
    if let Some(exchange) = env.get("MAESTRO_IDENTITY_EXCHANGE_URL").filter(|_| hosted) {
        let origin = if exchange.trim() == HOSTED_IDENTITY_EXCHANGE {
            HOSTED_IDENTITY_ORIGIN.to_owned()
        } else {
            hosted_test_identity_origin(exchange.trim(), env)
                .context("untrusted hosted EvalOps Identity exchange endpoint")?
        };
        let ca = env
            .get("MAESTRO_IDENTITY_TLS_CA_FILE")
            .map(|value| std::path::PathBuf::from(value.trim()))
            .filter(|path| path.is_absolute())
            .context("hosted EvalOps Identity requires an absolute projected CA file")?;
        return Ok((origin, Some(ca)));
    }
    Ok((
        crate::init_cli::evalops_identity_base_url(snapshot, env)?,
        None,
    ))
}

/// Loopback HTTPS exchange origins are admitted only for debug/test binaries
/// that opt in through the same test-authority switch as the public Identity
/// URL; release builds keep the single pinned in-cluster origin.
fn hosted_test_identity_origin(exchange: &str, env: &HashMap<String, String>) -> Result<String> {
    if !crate::init_cli::test_identity_authority_enabled(env) {
        bail!("hosted Identity exchange must be the in-cluster Identity service");
    }
    let url = url::Url::parse(exchange).context("parse hosted Identity exchange URL")?;
    let loopback = match url.host() {
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        Some(url::Host::Domain(domain)) => domain == "localhost",
        None => false,
    };
    if url.scheme() != "https" || !loopback || url.query().is_some() || url.fragment().is_some() {
        bail!("hosted Identity exchange must be the in-cluster Identity service");
    }
    Ok(url.origin().ascii_serialization())
}

fn identity_verification_client(
    identity_base_url: &str,
    ca_file: Option<&std::path::Path>,
) -> Result<reqwest::blocking::Client> {
    let mut builder = reqwest::blocking::Client::builder()
        .timeout(IDENTITY_INTROSPECTION_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none());
    if identity_base_url.starts_with("https://") {
        builder = builder.https_only(true);
    }
    if let Some(path) = ca_file {
        use std::io::Read;
        const MAX_CA_BYTES: u64 = 64 * 1024;
        let file = std::fs::File::open(path).context("open hosted EvalOps Identity CA")?;
        if !file
            .metadata()
            .context("inspect hosted EvalOps Identity CA")?
            .is_file()
        {
            bail!("hosted EvalOps Identity CA must be a regular file");
        }
        let mut pem = Vec::new();
        file.take(MAX_CA_BYTES + 1)
            .read_to_end(&mut pem)
            .context("read hosted EvalOps Identity CA")?;
        if pem.len() as u64 > MAX_CA_BYTES {
            bail!("hosted EvalOps Identity CA exceeds size limit");
        }
        let certificates = reqwest::Certificate::from_pem_bundle(&pem)
            .context("parse hosted EvalOps Identity CA")?;
        if certificates.is_empty() {
            bail!("hosted EvalOps Identity CA contains no certificates");
        }
        builder = builder.use_rustls_tls().tls_built_in_root_certs(false);
        for certificate in certificates {
            builder = builder.add_root_certificate(certificate);
        }
    }
    builder
        .build()
        .context("build EvalOps Identity verification client")
}

/// Verify the access token with EvalOps Identity before it authorizes a local
/// provider or Codex transport. This is intentionally synchronous at the
/// native-agent construction boundary: it prevents a caller from starting an
/// upstream model turn until Identity has confirmed a live human session.
///
/// The request runs on a short-lived OS thread because the public constructor
/// can be called from both synchronous code and an active Tokio runtime.
fn verify_live_identity_session(
    snapshot: Option<&EvalOpsCredentialSnapshot>,
    env: &HashMap<String, String>,
) -> Result<PlatformSession> {
    verify_live_runtime_identity(snapshot, env, &[])?.require_human()
}

fn verify_live_runtime_identity(
    snapshot: Option<&EvalOpsCredentialSnapshot>,
    env: &HashMap<String, String>,
    additional_scopes: &[&str],
) -> Result<VerifiedIdentity> {
    let Some(unverified) = platform_session_from(snapshot, env) else {
        bail!("{IDENTITY_REQUIRED_MESSAGE}");
    };
    let (identity_base_url, ca_file) = identity_verification_endpoint(snapshot, env)
        .with_context(|| IDENTITY_REQUIRED_MESSAGE.to_owned())?;
    let hosted = ca_file.is_some();
    let token = unverified.access_token.clone();
    let introspection = std::thread::spawn(move || -> Result<IdentityIntrospection> {
        let client = identity_verification_client(&identity_base_url, ca_file.as_deref())?;
        // Introspection is read-only. Retry one transient exchange against the
        // same authority and credential; never cache admission or retry a
        // rejection, redirect, inactive response, or malformed identity.
        for attempt in 0..2 {
            let response = client
                .post(format!("{identity_base_url}/v1/tokens/introspect"))
                .bearer_auth(&token)
                .send();
            let retry = match &response {
                Err(error) => error.is_timeout() || error.is_connect(),
                Ok(response) => matches!(response.status().as_u16(), 502..=504),
            };
            if retry && attempt == 0 {
                std::thread::sleep(std::time::Duration::from_millis(150));
                continue;
            }
            let response = response.context("contact EvalOps Identity")?;

            if response.status().is_redirection() {
                bail!("EvalOps Identity returned a redirect; admission does not follow redirects")
            }
            if response.status().is_server_error() {
                bail!(
                    "EvalOps Identity is temporarily unavailable (HTTP {})",
                    response.status()
                );
            }
            if !response.status().is_success() {
                bail!("EvalOps Identity rejected this session")
            }

            // Parse/drop the blocking response on this worker thread; moving
            // it into Tokio would tear down reqwest's runtime in async code.
            return response
                .json()
                .context("decode EvalOps Identity verification response");
        }
        unreachable!("the final identity attempt always returns")
    })
    .join()
    .map_err(|_| anyhow::anyhow!("EvalOps Identity verification thread panicked"))?
    .with_context(|| IDENTITY_REQUIRED_MESSAGE.to_owned())?;
    if additional_scopes.iter().any(|required| {
        !introspection
            .scopes
            .iter()
            .map(String::as_str)
            .chain(introspection.scope.split_whitespace())
            .any(|present| present == *required)
    }) {
        bail!("desktop Identity token lacks required hosted thread scope");
    }
    verified_runtime_identity(unverified, introspection, hosted)
}

fn verified_runtime_identity(
    session: PlatformSession,
    introspection: IdentityIntrospection,
    hosted: bool,
) -> Result<VerifiedIdentity> {
    if !hosted {
        return verified_platform_session(session, introspection).map(VerifiedIdentity::Human);
    }
    let organization_id = introspection
        .organization_id
        .as_deref()
        .filter(|value| !value.trim().is_empty());
    let workspace_id = introspection
        .workspace_id
        .as_deref()
        .filter(|value| !value.trim().is_empty());
    let scopes = introspection
        .scopes
        .iter()
        .map(String::as_str)
        .chain(introspection.scope.split_whitespace())
        .collect::<Vec<_>>();
    if !introspection.active
        || introspection.token_type != "service"
        || introspection.subject != "runner-host"
        || organization_id != Some(session.organization_id.as_str())
        || workspace_id.is_none()
        || workspace_id != session.workspace_id.as_deref()
        || scopes.as_slice() != [IDENTITY_REQUIRED_SCOPE]
    {
        bail!("Identity could not verify the hosted Runner Host tenant credential");
    }
    Ok(VerifiedIdentity::HostedManaged(session))
}

fn verified_platform_session(
    session: PlatformSession,
    introspection: IdentityIntrospection,
) -> Result<PlatformSession> {
    verified_platform_session_for_scope(session, introspection, IDENTITY_REQUIRED_SCOPE)
}

pub(crate) fn verified_platform_session_for_scope(
    mut session: PlatformSession,
    introspection: IdentityIntrospection,
    required_scope: &str,
) -> Result<PlatformSession> {
    let organization_id = introspection
        .organization_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let workspace_id = introspection
        .workspace_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let has_required_scope = introspection
        .scopes
        .iter()
        .map(String::as_str)
        .chain(introspection.scope.split_whitespace())
        .any(|scope| scope == required_scope);
    if !introspection.active
        || introspection.subject.trim().is_empty()
        || introspection.token_type != "access"
        || organization_id.is_none()
        || !has_required_scope
    {
        bail!("{IDENTITY_REQUIRED_MESSAGE} Identity could not verify this session.");
    }

    // Never let an environment variable or mutable local credential snapshot
    // select a tenant different from the signed Identity token.
    session.organization_id = organization_id.expect("checked above").to_owned();
    session.workspace_id = workspace_id.map(str::to_owned);
    session.user_id = Some(introspection.subject);
    Ok(session)
}

fn prefers_local_byok(model: &str) -> bool {
    ProviderRegistry::resolve_descriptor(model)
        .is_ok_and(|descriptor| descriptor.id == "openrouter")
}

fn is_managed_route(model: &str) -> bool {
    ProviderRegistry::resolve_descriptor(model)
        .is_ok_and(|descriptor| matches!(descriptor.id, "evalops" | "maestro-managed"))
}

/// Direct provider keys construct a native agent without Identity.
///
/// Managed / evalops routes, Codex app-server transports, and hosted Runner
/// Host admission still require a verified Identity session. A local key
/// must not unlock those paths.
fn admits_direct_provider_without_identity(model: &str, env: &HashMap<String, String>) -> bool {
    !is_managed_route(model) && !is_delegated_byok_transport(model) && byok_ready(model, env)
}

pub fn platform_session_from(
    snapshot: Option<&EvalOpsCredentialSnapshot>,
    env: &HashMap<String, String>,
) -> Option<PlatformSession> {
    let access_token = env_value(env, ACCESS_TOKEN_ENV)
        .or_else(|| file_env_value(env, ACCESS_TOKEN_ENV))
        .or_else(|| {
            snapshot
                .map(|value| value.access.trim().to_owned())
                .filter(|value| !value.is_empty())
        })?;
    let organization_id = env_value(env, ORG_ID_ENV).or_else(|| {
        snapshot
            .and_then(|value| value.organization_id.clone())
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    })?;
    let workspace_id = env_value(env, WORKSPACE_ID_ENV)
        .or_else(|| {
            snapshot
                .and_then(|value| value.agent_mcp.as_ref())
                .and_then(|meta| meta.workspace_id.clone())
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        })
        .or_else(|| {
            snapshot
                .and_then(|value| value.workspace_id.as_deref())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        });
    let provider_ref = snapshot
        .and_then(|value| value.provider_ref.clone())
        .unwrap_or_else(|| default_provider_ref(env, None));
    Some(PlatformSession {
        access_token,
        organization_id,
        workspace_id,
        provider_ref,
        email: snapshot.and_then(|value| value.email.clone()),
        user_id: snapshot.and_then(|value| value.user_id.clone()),
    })
}

/// True when BYOK can serve `model` from env, connections, or delegated login.
pub fn byok_ready(model: &str, env: &HashMap<String, String>) -> bool {
    byok_sources_ready(model, env) || connection_covers_provider(model, env).unwrap_or(false)
}

/// Env, file, and delegated-login sources only. Does not read connections.json.
pub fn byok_sources_ready(model: &str, env: &HashMap<String, String>) -> bool {
    let Ok(descriptor) = ProviderRegistry::resolve_descriptor(model) else {
        return false;
    };
    if !descriptor.requires_auth() {
        return true;
    }
    if descriptor.auth_env.iter().any(|name| {
        env.get(*name).is_some_and(|value| !value.trim().is_empty())
            || env
                .get(&format!("{name}_FILE"))
                .is_some_and(|value| !value.trim().is_empty())
    }) {
        return true;
    }
    if descriptor.id == "openai" && crate::openai_cli::has_stored_oauth_credential() {
        return true;
    }
    matches!(descriptor.id, "openai-codex" | "codex")
        && crate::codex_auth::read_codex_auth().is_some_and(|snap| snap.has_usable_credential())
}

fn is_delegated_byok_transport(model: &str) -> bool {
    ProviderRegistry::resolve_descriptor(model)
        .is_ok_and(|descriptor| matches!(descriptor.id, "openai-codex" | "codex"))
}

pub fn connection_covers_provider(model: &str, env: &HashMap<String, String>) -> Result<bool> {
    let provider_id = ProviderRegistry::resolve_descriptor(model)?.id;
    let store = match ConnectionStore::load(&ConnectionStore::default_path()?) {
        Ok(store) => store,
        Err(_) => return Ok(false),
    };
    let explicit = env
        .get("MAESTRO_CONNECTION")
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    Ok(store
        .selected(provider_id, explicit)?
        .is_some_and(|connection| {
            connection.state == crate::service_connections::ConnectionState::Active
        }))
}

impl PlatformSession {
    /// Environment for a managed `evalops/` client. Local provider keys are omitted.
    pub fn managed_env(
        &self,
        model: &str,
        source_env: &HashMap<String, String>,
    ) -> Result<HashMap<String, String>> {
        let workspace_id = self
            .workspace_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("EvalOps managed provider requires {WORKSPACE_ID_ENV}")
            })?;
        let mut env = HashMap::new();
        env.insert(ACCESS_TOKEN_ENV.to_owned(), self.access_token.clone());
        if let Some(base_url) = env_value(source_env, BASE_URL_ENV) {
            env.insert(BASE_URL_ENV.to_owned(), base_url);
        }
        env.insert(ORG_ID_ENV.to_owned(), self.organization_id.clone());
        env.insert(WORKSPACE_ID_ENV.to_owned(), workspace_id.to_owned());
        // Shipped models select a typed provider route. A provider switch uses
        // that route's credential reference, retaining authenticated tenant scope.
        let shipped = model
            .strip_prefix("evalops/")
            .or_else(|| model.strip_prefix("maestro-managed/"))
            .and_then(|model| {
                crate::model_catalog::MANAGED_MODELS
                    .iter()
                    .find(|entry| entry.id == model)
            });
        let switched = shipped.filter(|entry| {
            provider_ref_string(&self.provider_ref, "provider").as_deref() != Some(entry.provider)
        });
        let provider = if let Some(entry) = switched {
            entry.provider.to_owned()
        } else {
            vendor_provider_id(model, &self.provider_ref)?
        };
        env.insert(PROVIDER_ENV.to_owned(), provider);
        env.insert(
            ENVIRONMENT_ENV.to_owned(),
            provider_ref_string(&self.provider_ref, "environment")
                .unwrap_or_else(|| canonical_managed_environment(None)),
        );
        env.insert(
            CREDENTIAL_NAME_ENV.to_owned(),
            if let Some(entry) = switched {
                entry.credential_name.to_owned()
            } else {
                provider_ref_string(&self.provider_ref, "credential_name")
                    .unwrap_or_else(|| canonical_managed_credential_name(None))
            },
        );
        if let Some(team_id) = provider_ref_string(&self.provider_ref, "team_id") {
            env.insert(TEAM_ID_ENV.to_owned(), team_id);
        }
        Ok(env)
    }

    /// Route the selected model through the EvalOps managed provider.
    pub fn managed_model_route(&self, model: &str) -> String {
        let bare = model
            .split_once('/')
            .map(|(_, rest)| rest)
            .filter(|rest| !rest.is_empty())
            .unwrap_or(model);
        format!("evalops/{bare}")
    }
}

pub fn vendor_provider_id(model: &str, provider_ref: &Value) -> Result<String> {
    let descriptor = ProviderRegistry::resolve_descriptor(model)?;
    if matches!(descriptor.id, "evalops" | "maestro-managed") {
        return Ok(provider_ref_string(provider_ref, "provider")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "openai".to_owned()));
    }
    Ok(descriptor.id.to_owned())
}

pub fn default_provider_ref(env: &HashMap<String, String>, provider: Option<&str>) -> Value {
    let mut value = serde_json::json!({
        "provider": provider
            .map(str::to_owned)
            .or_else(|| env_value(env, PROVIDER_ENV))
            .unwrap_or_else(|| "openai".to_owned()),
        "environment": canonical_managed_environment(env_value(env, ENVIRONMENT_ENV).as_deref()),
        "credential_name": canonical_managed_credential_name(
            env_value(env, CREDENTIAL_NAME_ENV).as_deref(),
        ),
    });
    if let Some(team_id) = env_value(env, TEAM_ID_ENV) {
        value["team_id"] = Value::String(team_id);
    }
    value
}

pub fn env_value(env: &HashMap<String, String>, name: &str) -> Option<String> {
    env.get(name)
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn file_env_value(env: &HashMap<String, String>, name: &str) -> Option<String> {
    let path = env_value(env, &format!("{name}_FILE"))?;
    std::fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn provider_ref_string(value: &Value, field: &str) -> Option<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

pub fn setup_next_commands(
    provider: &str,
    identity_required: bool,
    byok_required: bool,
) -> Vec<(&'static str, &'static str, String)> {
    let byok_command = if matches!(provider, "openai-codex" | "codex") {
        "deixic-code codex login".to_owned()
    } else {
        "deixic-code connections add".to_owned()
    };
    let mut commands = Vec::new();
    if identity_required {
        commands.push((
            "evalops-login",
            "Sign in to EvalOps Identity (required for every Deixic Code session).",
            "deixic-code evalops login".to_owned(),
        ));
    }
    if byok_required {
        commands.push((
            "byok",
            "Add a local API key or provider login after Identity sign-in.",
            byok_command,
        ));
    }
    commands
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_cli::{EvalOpsAgentMcpSnapshot, EvalOpsCredentialSnapshot};

    fn hosted_service_session() -> PlatformSession {
        platform_session_from(
            None,
            &HashMap::from([
                (ACCESS_TOKEN_ENV.into(), "scoped-runner-token".into()),
                (ORG_ID_ENV.into(), "org_expected".into()),
                (WORKSPACE_ID_ENV.into(), "workspace_expected".into()),
            ]),
        )
        .unwrap()
    }

    fn hosted_service_introspection() -> serde_json::Value {
        serde_json::json!({
            "active": true,
            "subject": "runner-host",
            "token_type": "service",
            "organization_id": "org_expected",
            "workspace_id": "workspace_expected",
            "scopes": ["llm_gateway:invoke"]
        })
    }

    #[test]
    fn hosted_service_admission_matches_existing_runner_host_mint() {
        let identity = verified_runtime_identity(
            hosted_service_session(),
            serde_json::from_value(hosted_service_introspection()).unwrap(),
            true,
        )
        .unwrap();
        let env = HashMap::from([("OPENAI_API_KEY".into(), "local-key".into())]);
        let (mode, session) =
            ready_mode_from_verified_identity(identity, env, DEFAULT_MANAGED_MODEL).unwrap();
        assert!(matches!(mode, DetectedMode::Platform(_)));
        assert_eq!(session.organization_id, "org_expected");
        assert_eq!(session.workspace_id.as_deref(), Some("workspace_expected"));
        assert_eq!(session.user_id, None);
    }

    #[test]
    fn hosted_service_admission_rejects_wrong_principal_scope_type_and_tenant() {
        for (field, value) in [
            ("active", serde_json::json!(false)),
            ("subject", serde_json::json!("platform-worker")),
            ("subject", serde_json::json!("")),
            ("token_type", serde_json::json!("access")),
            ("token_type", serde_json::json!("agent")),
            ("organization_id", serde_json::json!("other-org")),
            ("organization_id", serde_json::Value::Null),
            ("organization_id", serde_json::json!("")),
            ("workspace_id", serde_json::json!("other-workspace")),
            ("workspace_id", serde_json::Value::Null),
            ("workspace_id", serde_json::json!("")),
            ("scopes", serde_json::json!([])),
            ("scopes", serde_json::json!(["llm_gateway:admin"])),
            (
                "scopes",
                serde_json::json!(["llm_gateway:invoke", "llm_gateway:admin"]),
            ),
        ] {
            let mut projection = hosted_service_introspection();
            projection[field] = value;
            assert!(
                verified_runtime_identity(
                    hosted_service_session(),
                    serde_json::from_value(projection).unwrap(),
                    true
                )
                .is_err(),
                "must reject {field}"
            );
        }
        let mut expected = hosted_service_session();
        expected.workspace_id = None;
        assert!(
            verified_runtime_identity(
                expected,
                serde_json::from_value(hosted_service_introspection()).unwrap(),
                true
            )
            .is_err()
        );
    }

    #[test]
    fn hosted_service_telemetry_preserves_verified_tenant_without_human_admission() {
        let identity = verified_runtime_identity(
            hosted_service_session(),
            serde_json::from_value(hosted_service_introspection()).unwrap(),
            true,
        )
        .unwrap();
        let delivery = identity.into_telemetry_session();
        assert_eq!(delivery.organization_id, "org_expected");
        assert_eq!(delivery.workspace_id.as_deref(), Some("workspace_expected"));
        assert_eq!(delivery.access_token, "scoped-runner-token");
        assert_eq!(delivery.user_id, None);
        let identity = verified_runtime_identity(
            hosted_service_session(),
            serde_json::from_value(hosted_service_introspection()).unwrap(),
            true,
        )
        .unwrap();
        assert!(identity.require_human().is_err());
    }

    #[test]
    fn hosted_service_admission_never_unlocks_local_or_codex_credentials() {
        let identity = verified_runtime_identity(
            hosted_service_session(),
            serde_json::from_value(hosted_service_introspection()).unwrap(),
            true,
        )
        .unwrap();
        assert!(identity.require_human().is_err());
        for model in [
            "openai/gpt-5.5",
            "openrouter/openai/o4-mini",
            "openai-codex/gpt-5.5",
            "codex/gpt-5.5",
        ] {
            let identity = verified_runtime_identity(
                hosted_service_session(),
                serde_json::from_value(hosted_service_introspection()).unwrap(),
                true,
            )
            .unwrap();
            let env = HashMap::from([
                ("OPENAI_API_KEY".into(), "local-key".into()),
                ("OPENROUTER_API_KEY".into(), "local-key".into()),
                ("CODEX_ACCESS_TOKEN".into(), "local-key".into()),
            ]);
            assert!(
                ready_mode_from_verified_identity(identity, env, model).is_err(),
                "must reject {model}"
            );
        }
        assert!(
            verified_runtime_identity(
                hosted_service_session(),
                serde_json::from_value(hosted_service_introspection()).unwrap(),
                false
            )
            .is_err()
        );
    }

    #[test]
    fn hosted_service_admission_requires_trusted_hosted_transport() {
        for (key, value) in [
            ("MAESTRO_HOSTED_RUNNER_MODE", "0"),
            (
                "MAESTRO_IDENTITY_EXCHANGE_URL",
                "https://identity.attacker.example/exchange",
            ),
            ("MAESTRO_IDENTITY_TLS_CA_FILE", "relative-ca.pem"),
        ] {
            let mut env = hosted_identity_env();
            env.insert(key.into(), value.into());
            if let Ok((_, ca)) = identity_verification_endpoint(None, &env) {
                assert!(ca.is_none());
                assert!(
                    verified_runtime_identity(
                        hosted_service_session(),
                        serde_json::from_value(hosted_service_introspection()).unwrap(),
                        ca.is_some()
                    )
                    .is_err()
                );
            }
        }
    }

    fn hosted_identity_env() -> HashMap<String, String> {
        HashMap::from([
            ("MAESTRO_HOSTED_RUNNER_MODE".into(), "1".into()),
            (
                "MAESTRO_IDENTITY_EXCHANGE_URL".into(),
                HOSTED_IDENTITY_EXCHANGE.into(),
            ),
            (
                "MAESTRO_IDENTITY_TLS_CA_FILE".into(),
                "/var/run/secrets/evalops.dev/identity/ca.crt".into(),
            ),
        ])
    }

    #[test]
    fn hosted_identity_selects_private_authority_and_projected_ca() {
        let env = hosted_identity_env();
        let (origin, ca) = identity_verification_endpoint(None, &env).unwrap();
        assert_eq!(origin, HOSTED_IDENTITY_ORIGIN);
        assert_eq!(
            ca.unwrap(),
            std::path::PathBuf::from(&env["MAESTRO_IDENTITY_TLS_CA_FILE"])
        );
    }

    #[test]
    fn hosted_identity_rejects_other_authorities_and_missing_trust() {
        for exchange in [
            "http://identity-service.evalops.svc.cluster.local:8080/internal/v1/kubernetes-workload-certificates/exchange",
            "https://identity.attacker.example/internal/v1/kubernetes-workload-certificates/exchange",
            "https://identity-service.evalops.svc.cluster.local:8080/internal/v1/kubernetes-workload-certificates/exchange?redirect=1",
        ] {
            let mut env = hosted_identity_env();
            env.insert("MAESTRO_IDENTITY_EXCHANGE_URL".into(), exchange.into());
            assert!(identity_verification_endpoint(None, &env).is_err());
        }
        for ca in [None, Some(""), Some("relative/ca.crt")] {
            let mut env = hosted_identity_env();
            env.remove("MAESTRO_IDENTITY_TLS_CA_FILE");
            if let Some(ca) = ca {
                env.insert("MAESTRO_IDENTITY_TLS_CA_FILE".into(), ca.into());
            }
            assert!(identity_verification_endpoint(None, &env).is_err());
        }
    }

    #[test]
    fn hosted_identity_admits_loopback_exchange_only_under_test_authority() {
        let mut env = hosted_identity_env();
        env.insert(
            "MAESTRO_IDENTITY_EXCHANGE_URL".into(),
            "https://127.0.0.1:45123/internal/v1/kubernetes-workload-certificates/exchange".into(),
        );
        let (origin, ca) = identity_verification_endpoint(None, &env).unwrap();
        assert_eq!(origin, "https://127.0.0.1:45123");
        assert!(ca.is_some());

        env.insert("MAESTRO_TEST_IDENTITY_AUTHORITY".into(), "0".into());
        assert!(identity_verification_endpoint(None, &env).is_err());

        env.remove("MAESTRO_TEST_IDENTITY_AUTHORITY");
        for exchange in [
            "http://127.0.0.1:45123/exchange",
            "https://127.0.0.1:45123/exchange?redirect=1",
            "https://10.0.0.7:45123/exchange",
        ] {
            env.insert("MAESTRO_IDENTITY_EXCHANGE_URL".into(), exchange.into());
            assert!(
                identity_verification_endpoint(None, &env).is_err(),
                "{exchange}"
            );
        }
    }

    #[test]
    fn hosted_identity_preserves_public_and_local_admission() {
        for mode in ["0", "false", "unknown"] {
            let mut env = hosted_identity_env();
            env.insert("MAESTRO_HOSTED_RUNNER_MODE".into(), mode.into());
            assert_eq!(
                identity_verification_endpoint(None, &env).unwrap(),
                ("https://identity.evalops.dev".into(), None)
            );
        }
        let mut env = hosted_identity_env();
        env.remove("MAESTRO_IDENTITY_EXCHANGE_URL");
        assert_eq!(
            identity_verification_endpoint(None, &env).unwrap(),
            ("https://identity.evalops.dev".into(), None)
        );
        env.insert(
            "MAESTRO_IDENTITY_URL".into(),
            "http://127.0.0.1:12345".into(),
        );
        env.insert(
            crate::init_cli::TEST_IDENTITY_AUTHORITY_ENV.into(),
            "1".into(),
        );
        assert_eq!(
            identity_verification_endpoint(None, &env).unwrap(),
            ("http://127.0.0.1:12345".into(), None)
        );
        env.insert(
            crate::init_cli::TEST_IDENTITY_AUTHORITY_ENV.into(),
            "0".into(),
        );
        assert!(identity_verification_endpoint(None, &env).is_err());
    }

    #[test]
    fn hosted_identity_client_fails_closed_on_unreadable_or_invalid_ca() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ca.pem");
        assert!(identity_verification_client(HOSTED_IDENTITY_ORIGIN, Some(&path)).is_err());
        std::fs::write(&path, "invalid certificate").unwrap();
        assert!(identity_verification_client(HOSTED_IDENTITY_ORIGIN, Some(&path)).is_err());
    }

    #[test]
    fn hosted_identity_client_validates_projected_tls_trust() {
        use std::io::{Read, Write};
        for (trusted, hostname) in [
            (true, "localhost"),
            (false, "localhost"),
            (true, "other.example"),
        ] {
            let cert = rcgen::generate_simple_self_signed(vec![hostname.into()]).unwrap();
            let other = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("ca.pem");
            std::fs::write(
                &path,
                if trusted {
                    cert.cert.pem()
                } else {
                    other.cert.pem()
                },
            )
            .unwrap();
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let config = rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.cert.der().clone()],
                rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der())
                    .into(),
            )
            .unwrap();
            let server = std::thread::spawn(move || {
                listener.set_nonblocking(true).unwrap();
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
                let stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "Identity TLS client did not connect"
                            );
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                        Err(error) => panic!("Identity TLS accept: {error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                    .unwrap();
                let connection =
                    rustls::ServerConnection::new(std::sync::Arc::new(config)).unwrap();
                let mut stream = rustls::StreamOwned::new(connection, stream);
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                loop {
                    match stream.read(&mut buffer) {
                        Ok(0) | Err(_) => return,
                        Ok(count) => request.extend_from_slice(&buffer[..count]),
                    }
                    if request.windows(4).any(|part| part == b"\r\n\r\n") {
                        break;
                    }
                }
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                    )
                    .unwrap();
                stream.flush().unwrap();
            });
            let origin = format!("https://localhost:{}", address.port());
            let response = identity_verification_client(&origin, Some(&path))
                .unwrap()
                .post(format!("{origin}/v1/tokens/introspect"))
                .send();
            assert_eq!(
                response.is_ok(),
                trusted && hostname == "localhost",
                "{response:?}"
            );
            server.join().unwrap();
        }
    }

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

    #[test]
    fn shipped_managed_routes_preserve_provider_and_verified_tenant_scope() {
        let session = PlatformSession {
            access_token: "access".to_owned(),
            organization_id: "org-1".to_owned(),
            workspace_id: Some("workspace-2".to_owned()),
            user_id: None,
            email: None,
            provider_ref: serde_json::json!({
                "provider": "openai",
                "environment": "production",
                "credential_name": "default"
            }),
        };
        for price in crate::model_catalog::MANAGED_MODELS {
            let model = format!("evalops/{}", price.id);
            let env = session.managed_env(&model, &HashMap::new()).unwrap();
            assert_eq!(env[PROVIDER_ENV], price.provider);
            assert_eq!(env[ORG_ID_ENV], "org-1");
            assert_eq!(env[WORKSPACE_ID_ENV], "workspace-2");
            assert_eq!(env[ENVIRONMENT_ENV], "production");
            assert_eq!(env[CREDENTIAL_NAME_ENV], price.credential_name);
            assert_eq!(session.managed_model_route(&model), model);
            assert!(!env.contains_key("FIREWORKS_API_KEY"));
        }
    }

    fn snapshot(org: &str, token: &str) -> EvalOpsCredentialSnapshot {
        EvalOpsCredentialSnapshot {
            access: token.to_owned(),
            refresh: "refresh".to_owned(),
            expires: 1,
            email: Some("user@evalops.dev".to_owned()),
            organization_id: Some(org.to_owned()),
            workspace_id: None,
            user_id: Some("user_1".to_owned()),
            identity_base_url: Some("https://identity.evalops.dev".to_owned()),
            provider_ref: Some(serde_json::json!({
                "provider": "anthropic",
                "environment": "production",
                "credential_name": "default"
            })),
            agent_mcp: Some(EvalOpsAgentMcpSnapshot {
                workspace_id: Some("workspace_1".to_owned()),
                ..EvalOpsAgentMcpSnapshot::default()
            }),
        }
    }

    fn snapshot_without_workspace(org: &str, token: &str) -> EvalOpsCredentialSnapshot {
        let mut snapshot = snapshot(org, token);
        snapshot.agent_mcp = None;
        snapshot
    }

    #[test]
    fn issuer_workspace_survives_fresh_oauth_without_agent_metadata() {
        let mut credential = snapshot_without_workspace("org_1", "tok");
        credential.workspace_id = Some(" issuer-workspace ".to_owned());
        let mut env = HashMap::new();
        assert_eq!(
            platform_session_from(Some(&credential), &env)
                .unwrap()
                .workspace_id
                .as_deref(),
            Some("issuer-workspace")
        );
        credential.agent_mcp = Some(EvalOpsAgentMcpSnapshot {
            workspace_id: Some("agent-workspace".to_owned()),
            ..EvalOpsAgentMcpSnapshot::default()
        });
        assert_eq!(
            platform_session_from(Some(&credential), &env)
                .unwrap()
                .workspace_id
                .as_deref(),
            Some("agent-workspace")
        );
        env.insert(WORKSPACE_ID_ENV.to_owned(), "env-workspace".to_owned());
        assert_eq!(
            platform_session_from(Some(&credential), &env)
                .unwrap()
                .workspace_id
                .as_deref(),
            Some("env-workspace")
        );
        env.clear();
        credential.agent_mcp = None;
        credential.workspace_id = Some("  ".to_owned());
        assert!(
            platform_session_from(Some(&credential), &env)
                .unwrap()
                .workspace_id
                .is_none()
        );
    }

    fn active_introspection(
        organization_id: &str,
        workspace_id: Option<&str>,
    ) -> IdentityIntrospection {
        IdentityIntrospection {
            active: true,
            subject: "user_1".to_owned(),
            token_type: "access".to_owned(),
            organization_id: Some(organization_id.to_owned()),
            workspace_id: workspace_id.map(str::to_owned),
            scopes: vec![IDENTITY_REQUIRED_SCOPE.to_owned()],
            scope: String::new(),
        }
    }

    #[test]
    fn verified_identity_scope_overrides_mutable_local_tenant_values() {
        let session = PlatformSession {
            access_token: "access".to_owned(),
            organization_id: "forged-org".to_owned(),
            workspace_id: Some("forged-workspace".to_owned()),
            provider_ref: serde_json::json!({}),
            email: None,
            user_id: None,
        };

        let verified =
            verified_platform_session(session, active_introspection("org_1", Some("workspace_1")))
                .expect("active human token with the Maestro scope");

        assert_eq!(verified.organization_id, "org_1");
        assert_eq!(verified.workspace_id.as_deref(), Some("workspace_1"));
    }

    #[test]
    fn live_introspection_retries_transient_failure_but_not_rejection() {
        use std::io::{Read as _, Write as _};
        use std::sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        };

        for (first, second, active, expected_calls, accepted) in [
            (503, 200, true, 2, true),
            (503, 503, true, 2, false),
            (401, 200, true, 1, false),
            (403, 200, true, 1, false),
            (200, 200, false, 1, false),
        ] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let address = listener.local_addr().unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let calls = Arc::new(AtomicUsize::new(0));
            let server_stop = Arc::clone(&stop);
            let server_calls = Arc::clone(&calls);
            let server = std::thread::spawn(move || {
                while !server_stop.load(Ordering::SeqCst) {
                    let Ok((mut socket, _)) = listener.accept() else {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        continue;
                    };
                    socket.set_nonblocking(false).unwrap();
                    socket
                        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                        .unwrap();
                    let mut bytes = [0_u8; 4096];
                    assert!(
                        socket.read(&mut bytes).unwrap() > 0,
                        "request must reach server"
                    );
                    let index = server_calls.fetch_add(1, Ordering::SeqCst);
                    let status = if index == 0 { first } else { second };
                    let body = serde_json::json!({
                        "active": active, "subject":"user-test", "token_type":"access",
                        "organization_id":"org-live", "workspace_id":"workspace-live",
                        "scopes":["llm_gateway:invoke"]
                    })
                    .to_string();
                    write!(socket, "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                }
            });
            let env = HashMap::from([
                (
                    ACCESS_TOKEN_ENV.to_owned(),
                    "fixture-access-token".to_owned(),
                ),
                (ORG_ID_ENV.to_owned(), "untrusted-org".to_owned()),
                (
                    "MAESTRO_IDENTITY_URL".to_owned(),
                    format!("http://{address}"),
                ),
                (
                    crate::init_cli::TEST_IDENTITY_AUTHORITY_ENV.to_owned(),
                    "1".to_owned(),
                ),
            ]);
            let result = verify_live_identity_session(None, &env);
            stop.store(true, Ordering::SeqCst);
            server.join().unwrap();
            assert_eq!(
                result.is_ok(),
                accepted,
                "first={first}, second={second}, active={active}: {result:?}"
            );
            assert_eq!(calls.load(Ordering::SeqCst), expected_calls);
            if let Ok(session) = result {
                assert_eq!(session.organization_id, "org-live");
                assert_eq!(session.workspace_id.as_deref(), Some("workspace-live"));
            }
        }
    }

    #[test]
    fn identity_introspection_does_not_follow_redirects() {
        use std::io::{Read as _, Write as _};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let _guard = crate::config::test_process_env_lock();
        let _restore = EnvRestore::capture(&[
            "MAESTRO_HOME",
            ACCESS_TOKEN_ENV,
            ACCESS_TOKEN_FILE_ENV,
            ORG_ID_ENV,
            WORKSPACE_ID_ENV,
            "MAESTRO_IDENTITY_URL",
            crate::init_cli::TEST_IDENTITY_AUTHORITY_ENV,
        ]);

        let destination_hits = Arc::new(AtomicUsize::new(0));
        let destination = std::net::TcpListener::bind("127.0.0.1:0").expect("redirect target");
        let destination_addr = destination.local_addr().expect("redirect target addr");
        let destination_hits_for_server = Arc::clone(&destination_hits);
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = destination.accept() {
                let mut buf = [0_u8; 1024];
                let _ = stream.read(&mut buf);
                destination_hits_for_server.fetch_add(1, Ordering::SeqCst);
                let body = r#"{"active":true,"subject":"user-test","token_type":"access","organization_id":"org-test","workspace_id":"workspace-test","scopes":["llm_gateway:invoke"]}"#;
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            }
        });

        let origin = std::net::TcpListener::bind("127.0.0.1:0").expect("redirect origin");
        let origin_addr = origin.local_addr().expect("redirect origin addr");
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = origin.accept() {
                let mut buf = [0_u8; 1024];
                let _ = stream.read(&mut buf);
                let location = format!("http://{destination_addr}/v1/tokens/introspect");
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                );
            }
        });

        let maestro_home = tempfile::tempdir().expect("maestro home");
        std::env::set_var("MAESTRO_HOME", maestro_home.path());
        std::env::set_var(ACCESS_TOKEN_ENV, "redirect-token");
        std::env::set_var(ORG_ID_ENV, "attacker-org");
        std::env::set_var("MAESTRO_IDENTITY_URL", format!("http://{origin_addr}"));
        std::env::set_var(crate::init_cli::TEST_IDENTITY_AUTHORITY_ENV, "1");
        std::env::remove_var(ACCESS_TOKEN_FILE_ENV);
        std::env::remove_var(WORKSPACE_ID_ENV);

        let error = require_ready_with_identity("openai/gpt-5.5")
            .expect_err("a redirected Identity hop must fail closed");
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("redirect") || rendered.contains("rejected this session"),
            "{rendered}"
        );
        assert_eq!(
            destination_hits.load(Ordering::SeqCst),
            0,
            "introspection must not follow a 302 to a second origin"
        );
    }

    #[test]
    fn test_identity_fixture_rejects_missing_bearer_and_wrong_method() {
        let base = test_identity_base_url();
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("identity fixture client");
        let missing = client
            .post(format!("{base}/v1/tokens/introspect"))
            .send()
            .expect("missing bearer");
        assert_eq!(missing.status().as_u16(), 401, "{}", missing.status());
        let wrong_method = client
            .get(format!("{base}/v1/tokens/introspect"))
            .bearer_auth("fixture-evalops-access-token")
            .send()
            .expect("wrong method");
        assert_eq!(
            wrong_method.status().as_u16(),
            404,
            "{}",
            wrong_method.status()
        );
    }

    #[test]
    fn desktop_credential_is_live_verified_and_cannot_select_another_tenant() {
        let _guard = crate::config::test_process_env_lock();
        let _restore = EnvRestore::capture(&[
            "MAESTRO_IDENTITY_URL",
            crate::init_cli::TEST_IDENTITY_AUTHORITY_ENV,
        ]);
        std::env::set_var("MAESTRO_IDENTITY_URL", test_identity_base_url());
        std::env::set_var(crate::init_cli::TEST_IDENTITY_AUTHORITY_ENV, "1");
        let session =
            verified_desktop_identity_session("desktop-hosted-token", "org-test", "workspace-test")
                .expect("matching desktop token");
        assert_eq!(session.user_id.as_deref(), Some("user-test"));
        assert!(
            verified_desktop_identity_session(
                "desktop-hosted-token",
                "other-org",
                "workspace-test"
            )
            .is_err()
        );
        assert!(
            verified_desktop_identity_session(
                "desktop-hosted-token",
                "org-test",
                "other-workspace"
            )
            .is_err()
        );
        assert!(
            verified_desktop_identity_session("valid-token", "org-test", "workspace-test").is_err()
        );
        for token in ["desktop-readonly-token", "desktop-writeonly-token"] {
            assert!(
                verified_desktop_identity_session(token, "org-test", "workspace-test").is_err()
            );
        }
        assert!(
            verified_desktop_identity_session("inactive-token", "org-test", "workspace-test")
                .is_err()
        );
        assert!(
            verified_desktop_identity_session("unscoped-token", "org-test", "workspace-test")
                .is_err()
        );
    }

    #[test]
    fn live_introspection_rejects_inactive_and_unscoped_tokens() {
        let _guard = crate::config::test_process_env_lock();
        let _restore = EnvRestore::capture(&[
            "MAESTRO_HOME",
            ACCESS_TOKEN_ENV,
            ACCESS_TOKEN_FILE_ENV,
            ORG_ID_ENV,
            WORKSPACE_ID_ENV,
            "MAESTRO_IDENTITY_URL",
            crate::init_cli::TEST_IDENTITY_AUTHORITY_ENV,
        ]);
        let maestro_home = tempfile::tempdir().expect("maestro home");
        std::env::set_var("MAESTRO_HOME", maestro_home.path());
        std::env::set_var(ORG_ID_ENV, "org-test");
        std::env::set_var("MAESTRO_IDENTITY_URL", test_identity_base_url());
        std::env::set_var(crate::init_cli::TEST_IDENTITY_AUTHORITY_ENV, "1");
        std::env::remove_var(ACCESS_TOKEN_FILE_ENV);
        std::env::remove_var(WORKSPACE_ID_ENV);

        std::env::set_var(ACCESS_TOKEN_ENV, "inactive-token");
        let inactive = require_ready_with_identity("openai/gpt-5.5")
            .expect_err("inactive live token must fail closed");
        assert!(
            format!("{inactive:#}").contains("Identity could not verify this session"),
            "{inactive:#}"
        );

        std::env::set_var(ACCESS_TOKEN_ENV, "unscoped-token");
        let unscoped = require_ready_with_identity("openai/gpt-5.5")
            .expect_err("unscoped live token must fail closed");
        assert!(
            format!("{unscoped:#}").contains("Identity could not verify this session"),
            "{unscoped:#}"
        );
    }

    #[test]
    fn verified_identity_rejects_inactive_nonhuman_and_unscoped_tokens() {
        let session = PlatformSession {
            access_token: "access".to_owned(),
            organization_id: "org_1".to_owned(),
            workspace_id: Some("workspace_1".to_owned()),
            provider_ref: serde_json::json!({}),
            email: None,
            user_id: None,
        };
        let mut inactive = active_introspection("org_1", Some("workspace_1"));
        inactive.active = false;
        assert!(verified_platform_session(session.clone(), inactive).is_err());

        let mut service = active_introspection("org_1", Some("workspace_1"));
        service.token_type = "service".to_owned();
        assert!(verified_platform_session(session.clone(), service).is_err());

        let mut unscoped = active_introspection("org_1", Some("workspace_1"));
        unscoped.scopes.clear();
        assert!(verified_platform_session(session, unscoped).is_err());
    }

    #[test]
    fn stored_evalops_session_is_platform_mode() {
        let mode = detect_from(Some(&snapshot("org_1", "tok")), &HashMap::new()).expect("mode");
        assert!(mode.is_platform());
        let DetectedMode::Platform(session) = mode else {
            panic!("expected platform");
        };
        assert_eq!(session.organization_id, "org_1");
        assert_eq!(session.access_token, "tok");
    }

    #[test]
    fn canonical_env_session_is_platform_mode() {
        let env = HashMap::from([
            (ACCESS_TOKEN_ENV.to_owned(), "env-tok".to_owned()),
            (ORG_ID_ENV.to_owned(), "org_env".to_owned()),
        ]);
        let mode = detect_from(None, &env).expect("mode");
        let DetectedMode::Platform(session) = mode else {
            panic!("expected platform");
        };
        assert_eq!(session.organization_id, "org_env");
        assert_eq!(session.access_token, "env-tok");
    }

    #[test]
    fn legacy_evalops_token_alias_is_ignored() {
        let env = HashMap::from([
            ("EVALOPS_TOKEN".to_owned(), "legacy".to_owned()),
            (
                "EVALOPS_ORGANIZATION_ID".to_owned(),
                "org_legacy".to_owned(),
            ),
        ]);
        let mode = detect_from(None, &env).expect("mode");
        assert_eq!(mode.kind(), CredentialModeKind::Byok);
    }

    #[test]
    fn platform_managed_env_preserves_gateway_url_and_omits_local_provider_keys() {
        let source_env = HashMap::from([
            (
                BASE_URL_ENV.to_owned(),
                "http://127.0.0.1:4321/v1".to_owned(),
            ),
            ("ANTHROPIC_API_KEY".to_owned(), "local-key".to_owned()),
        ]);
        let session =
            platform_session_from(Some(&snapshot("org_1", "tok")), &source_env).expect("session");
        let env = session
            .managed_env("anthropic/claude-opus-4-6", &source_env)
            .expect("env");
        assert_eq!(env.get(ACCESS_TOKEN_ENV).map(String::as_str), Some("tok"));
        assert_eq!(
            env.get(BASE_URL_ENV).map(String::as_str),
            Some("http://127.0.0.1:4321/v1")
        );
        assert_eq!(env.get(ORG_ID_ENV).map(String::as_str), Some("org_1"));
        assert_eq!(
            env.get(WORKSPACE_ID_ENV).map(String::as_str),
            Some("workspace_1")
        );
        assert_eq!(env.get(PROVIDER_ENV).map(String::as_str), Some("anthropic"));
        assert!(!env.contains_key("ANTHROPIC_API_KEY"));
        let resolved = ProviderRegistry::require(
            &session.managed_model_route("anthropic/claude-opus-4-6"),
            &env,
        )
        .expect("managed provider route");
        assert_eq!(
            resolved.base_url.as_deref(),
            Some("http://127.0.0.1:4321/v1")
        );
        assert_eq!(
            session.managed_model_route("anthropic/claude-opus-4-6"),
            "evalops/claude-opus-4-6"
        );
    }

    #[test]
    fn byok_ready_requires_a_usable_local_credential() {
        let empty = HashMap::new();
        assert!(!byok_sources_ready("anthropic/claude-opus-4-6", &empty));
        let env = HashMap::from([("ANTHROPIC_API_KEY".to_owned(), "sk-test".to_owned())]);
        assert!(byok_sources_ready("anthropic/claude-opus-4-6", &env));
    }

    #[test]
    fn require_ready_fails_when_neither_mode_can_serve() {
        let error = require_ready_from(None, &HashMap::new(), "anthropic/claude-opus-4-6")
            .expect_err("should fail closed");
        assert!(error.to_string().contains("deixic-code evalops login"));
    }

    #[test]
    fn require_ready_accepts_platform_without_local_keys() {
        let mode = require_ready_from(
            Some(&snapshot("org_1", "tok")),
            &HashMap::new(),
            "anthropic/claude-opus-4-6",
        )
        .expect("platform is ready");
        assert!(mode.is_platform());
    }

    #[test]
    fn require_ready_allows_byok_after_identity_without_workspace() {
        let snapshot = snapshot_without_workspace("org_1", "tok");
        let env = HashMap::from([("ANTHROPIC_API_KEY".to_owned(), "local-key".to_owned())]);
        let mode = require_ready_from(Some(&snapshot), &env, "anthropic/claude-opus-4-6")
            .expect("an Identity session must permit a local provider credential");
        assert_eq!(mode.kind(), CredentialModeKind::Byok);

        let session = platform_session_from(Some(&snapshot), &env).expect("platform identity");
        assert!(
            session
                .managed_env("anthropic/claude-opus-4-6", &env)
                .is_err()
        );
    }

    #[test]
    fn require_ready_keeps_explicit_openrouter_on_byok() {
        let env = HashMap::from([("OPENROUTER_API_KEY".to_owned(), "or-test".to_owned())]);
        let mode = require_ready_from(
            Some(&snapshot("org_1", "tok")),
            &env,
            "openrouter/openai/o4-mini",
        )
        .expect("openrouter stays local");
        assert!(!mode.is_platform());
    }

    #[test]
    fn require_ready_admits_direct_provider_keys_without_identity() {
        for (model, key, value) in [
            ("openai/gpt-5.4", "OPENAI_API_KEY", "sk-test"),
            ("anthropic/claude-opus-4-6", "ANTHROPIC_API_KEY", "sk-test"),
            ("google/gemini-2.5-pro", "GEMINI_API_KEY", "sk-test"),
            ("xai/grok-4", "XAI_API_KEY", "sk-test"),
            ("openrouter/openai/o4-mini", "OPENROUTER_API_KEY", "or-test"),
        ] {
            let env = HashMap::from([(key.to_owned(), value.to_owned())]);
            let mode = require_ready_from(None, &env, model)
                .unwrap_or_else(|error| panic!("{model} with {key} must admit BYOK: {error:#}"));
            assert_eq!(
                mode.kind(),
                CredentialModeKind::Byok,
                "{model} must stay on the local provider"
            );
        }
    }

    #[test]
    fn require_ready_still_requires_identity_for_managed_evalops_without_a_session() {
        let env = HashMap::from([("OPENAI_API_KEY".to_owned(), "sk-test".to_owned())]);
        for model in [
            DEFAULT_MANAGED_MODEL,
            "evalops/gpt-5.5",
            "maestro-managed/gpt-5.5",
        ] {
            let error = require_ready_from(None, &env, model).expect_err(&format!(
                "{model} must not treat a local OpenAI key as managed admission"
            ));
            assert!(
                error.to_string().contains("deixic-code evalops login"),
                "{model}: {error}"
            );
        }
    }

    #[test]
    fn platform_managed_env_builds_evalops_llm_gateway_client() {
        let source_env = HashMap::new();
        let session =
            platform_session_from(Some(&snapshot("org_1", "tok")), &source_env).expect("session");
        let env = session
            .managed_env("anthropic/claude-opus-4-6", &source_env)
            .expect("env");
        let client = crate::ai::UnifiedClient::from_model_with_env(
            &session.managed_model_route("anthropic/claude-opus-4-6"),
            &env,
        )
        .expect("managed client");
        assert!(
            matches!(client, crate::ai::UnifiedClient::OpenAI(_)),
            "platform mode must use the llm-gateway OpenAI-compatible transport"
        );
        assert_eq!(client.provider_name(), "anthropic");
    }
}
