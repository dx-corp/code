use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Duration, Utc};
use futures::StreamExt as _;
use rand::RngExt as _;
use rcgen::{CertificateParams, KeyPair};
use rustls::{
    DigitallySignedStruct, DistinguishedName, Error as TlsError, SignatureScheme,
    client::danger::HandshakeSignatureValid,
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, UnixTime, pem::PemObject},
    server::danger::{ClientCertVerified, ClientCertVerifier},
};
use serde::{Deserialize, Serialize};
use std::{fmt, fs, path::Path, sync::Arc, time::Duration as StdDuration};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use x509_parser::{extensions::GeneralName, parse_x509_certificate};

const MAX_PROJECTED_TOKEN_BYTES: usize = 16 * 1024;
const MAX_CA_BUNDLE_BYTES: usize = 64 * 1024;
const MAX_EXCHANGE_RESPONSE_BYTES: usize = 128 * 1024;
const MAX_CERTIFICATE_TTL_SECONDS: i64 = 300;
// Identity holds the Sandboxwich placement fence for up to ~30s while the
// attestation becomes live. The resident must outwait that window or the first
// provision after a cold start fails closed with identity_exchange_failed.
const INITIAL_EXCHANGE_TIMEOUT: StdDuration = StdDuration::from_secs(12);
const INITIAL_EXCHANGE_MIN_BACKOFF: StdDuration = StdDuration::from_millis(25);
const INITIAL_EXCHANGE_MAX_BACKOFF: StdDuration = StdDuration::from_secs(2);
const INITIAL_EXCHANGE_JITTER_FACTOR: f64 = 0.2;
pub(super) const RUNNER_HOST_CLIENT_URI: &str = "spiffe://identity.evalops.dev/service/runner-host";

#[derive(Clone)]
pub(super) struct IdentityBinding {
    pub organization_id: String,
    pub workspace_id: String,
    pub sandbox_id: Uuid,
    pub placement_generation: u64,
    pub runner_session_id: String,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct ServiceIdentity {
    pub service_name: String,
    pub service_port: u16,
    pub image_digest: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct IdentityExchangeResponse {
    certificate_pem: String,
    ca_certificate_pem: String,
    serial_number: String,
    expires_at: String,
    uri_san: String,
}

pub(super) struct IssuedServerIdentity {
    pub tls_config: Arc<rustls::ServerConfig>,
    pub expires_at: DateTime<Utc>,
    #[cfg(test)]
    pub uri_san: String,
    #[cfg(test)]
    pub service: ServiceIdentity,
}

pub(super) struct IssuedClientIdentity {
    pub tls_config: Arc<rustls::ClientConfig>,
    pub expires_at: DateTime<Utc>,
    #[cfg(test)]
    pub uri_san: String,
}

#[derive(Clone)]
pub(super) struct ReloadableClientIdentity {
    active: Arc<RwLock<Option<ActiveClientIdentity>>>,
}

struct ActiveClientIdentity {
    tls_config: Arc<rustls::ClientConfig>,
    expires_at: DateTime<Utc>,
    connections: CancellationToken,
}

impl ReloadableClientIdentity {
    pub(super) fn new(identity: IssuedClientIdentity) -> Self {
        Self {
            active: Arc::new(RwLock::new(Some(ActiveClientIdentity::from(identity)))),
        }
    }

    pub(super) async fn snapshot(
        &self,
        now: DateTime<Utc>,
    ) -> Option<(Arc<rustls::ClientConfig>, CancellationToken, DateTime<Utc>)> {
        let active = self.active.read().await;
        active.as_ref().and_then(|active| {
            (active.expires_at > now).then(|| {
                (
                    active.tls_config.clone(),
                    active.connections.clone(),
                    active.expires_at,
                )
            })
        })
    }

    async fn install(&self, identity: IssuedClientIdentity) {
        let previous = self
            .active
            .write()
            .await
            .replace(ActiveClientIdentity::from(identity));
        if let Some(previous) = previous {
            previous.connections.cancel();
        }
    }

    async fn expires_at(&self) -> Option<DateTime<Utc>> {
        self.active
            .read()
            .await
            .as_ref()
            .map(|value| value.expires_at)
    }

    async fn expire_if_due(&self, now: DateTime<Utc>) {
        let mut active = self.active.write().await;
        if active
            .as_ref()
            .is_some_and(|identity| identity.expires_at <= now)
        {
            if let Some(expired) = active.take() {
                expired.connections.cancel();
            }
        }
    }

    async fn clear(&self) {
        if let Some(active) = self.active.write().await.take() {
            active.connections.cancel();
        }
    }
}

impl From<IssuedClientIdentity> for ActiveClientIdentity {
    fn from(identity: IssuedClientIdentity) -> Self {
        Self {
            tls_config: identity.tls_config,
            expires_at: identity.expires_at,
            connections: CancellationToken::new(),
        }
    }
}

#[derive(Clone)]
pub(super) struct ReloadableServerIdentity {
    active: Arc<RwLock<Option<ActiveServerIdentity>>>,
}

struct ActiveServerIdentity {
    tls_config: Arc<rustls::ServerConfig>,
    expires_at: DateTime<Utc>,
    connections: CancellationToken,
}

impl ReloadableServerIdentity {
    pub(super) fn new(identity: IssuedServerIdentity) -> Self {
        Self {
            active: Arc::new(RwLock::new(Some(ActiveServerIdentity::from(identity)))),
        }
    }

    pub(super) async fn snapshot(
        &self,
        now: DateTime<Utc>,
    ) -> Option<(Arc<rustls::ServerConfig>, CancellationToken)> {
        let active = self.active.read().await;
        active.as_ref().and_then(|active| {
            (active.expires_at > now)
                .then(|| (active.tls_config.clone(), active.connections.clone()))
        })
    }

    pub(super) async fn install(&self, identity: IssuedServerIdentity) {
        let previous = self
            .active
            .write()
            .await
            .replace(ActiveServerIdentity::from(identity));
        if let Some(previous) = previous {
            previous.connections.cancel();
        }
    }

    pub(super) async fn expire_if_due(&self, now: DateTime<Utc>) {
        let mut active = self.active.write().await;
        if active
            .as_ref()
            .is_some_and(|identity| identity.expires_at <= now)
        {
            if let Some(expired) = active.take() {
                expired.connections.cancel();
            }
        }
    }

    async fn expires_at(&self) -> Option<DateTime<Utc>> {
        self.active
            .read()
            .await
            .as_ref()
            .map(|identity| identity.expires_at)
    }

    pub(super) async fn clear(&self) {
        if let Some(active) = self.active.write().await.take() {
            active.connections.cancel();
        }
    }
}

impl From<IssuedServerIdentity> for ActiveServerIdentity {
    fn from(identity: IssuedServerIdentity) -> Self {
        Self {
            tls_config: identity.tls_config,
            expires_at: identity.expires_at,
            connections: CancellationToken::new(),
        }
    }
}

pub(super) async fn rotate_server_identity(
    exchanger: Arc<WorkloadIdentityExchanger>,
    state: ReloadableServerIdentity,
    shutdown: CancellationToken,
) {
    const RENEW_BEFORE_EXPIRY_SECONDS: i64 = 60;
    const MAX_RETRY_SECONDS: u64 = 15;

    loop {
        let Some(expires_at) = state.expires_at().await else {
            break;
        };
        let renew_at = expires_at - Duration::seconds(RENEW_BEFORE_EXPIRY_SECONDS);
        let delay = (renew_at - Utc::now())
            .to_std()
            .unwrap_or(StdDuration::ZERO);
        tokio::select! {
            () = shutdown.cancelled() => break,
            () = tokio::time::sleep(delay) => {}
        }

        let mut retry_seconds = 1_u64;
        let mut active_expiry = Some(expires_at);
        loop {
            let now = Utc::now();
            state.expire_if_due(now).await;
            if active_expiry.is_some_and(|expiry| expiry <= now) {
                active_expiry = None;
            }

            let exchange = exchanger.exchange_once(now);
            let result = if let Some(expiry) = active_expiry {
                let until_expiry = (expiry - Utc::now()).to_std().unwrap_or(StdDuration::ZERO);
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    () = tokio::time::sleep(until_expiry) => {
                        state.expire_if_due(Utc::now()).await;
                        active_expiry = None;
                        continue;
                    }
                    result = exchange => result,
                }
            } else {
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    result = exchange => result,
                }
            };

            match result {
                Ok(identity) => {
                    state.install(identity).await;
                    break;
                }
                Err(_) => {
                    let delay = StdDuration::from_secs(retry_seconds);
                    retry_seconds = (retry_seconds * 2).min(MAX_RETRY_SECONDS);
                    if let Some(expiry) = active_expiry {
                        let until_expiry =
                            (expiry - Utc::now()).to_std().unwrap_or(StdDuration::ZERO);
                        tokio::select! {
                            () = shutdown.cancelled() => return,
                            () = tokio::time::sleep(delay.min(until_expiry)) => {}
                        }
                    } else {
                        tokio::select! {
                            () = shutdown.cancelled() => return,
                            () = tokio::time::sleep(delay) => {}
                        }
                    }
                }
            }
        }
    }
    state.clear().await;
}

pub(super) struct WorkloadIdentityExchanger {
    config: super::config::HostedRunnerWorkloadIdentityConfig,
    binding: IdentityBinding,
    client: reqwest::Client,
}

impl WorkloadIdentityExchanger {
    pub(super) fn try_new(
        config: super::config::HostedRunnerWorkloadIdentityConfig,
        runner_session_id: String,
    ) -> Result<Self, WorkloadIdentityError> {
        let identity_ca_pem =
            read_bounded_utf8_file(&config.identity_tls_ca_file, MAX_CA_BUNDLE_BYTES)?;
        let identity_ca = reqwest::Certificate::from_pem(identity_ca_pem.as_bytes())
            .map_err(|_| WorkloadIdentityError::Unavailable)?;
        let client = reqwest::Client::builder()
            .connect_timeout(StdDuration::from_secs(2))
            .timeout(StdDuration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .https_only(true)
            .tls_built_in_root_certs(false)
            .add_root_certificate(identity_ca)
            .build()
            .map_err(|_| WorkloadIdentityError::Unavailable)?;
        let binding = IdentityBinding {
            organization_id: config.organization_id.clone(),
            workspace_id: config.workspace_id.clone(),
            sandbox_id: config.sandbox_id,
            placement_generation: config.placement_generation,
            runner_session_id,
        };
        Ok(Self {
            config,
            binding,
            client,
        })
    }

    pub(super) async fn exchange_once(
        &self,
        now: DateTime<Utc>,
    ) -> Result<IssuedServerIdentity, WorkloadIdentityError> {
        let projected_token = read_bounded_utf8_file(
            &self.config.kubernetes_token_file,
            MAX_PROJECTED_TOKEN_BYTES,
        )?;
        let projected_token = projected_token.trim();
        let pod_uid = projected_pod_uid(projected_token)?;
        let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
            .map_err(|_| WorkloadIdentityError::Unavailable)?;
        let csr_pem = CertificateParams::default()
            .serialize_request(&key)
            .and_then(|csr| csr.pem())
            .map_err(|_| WorkloadIdentityError::Unavailable)?;
        let request = build_exchange_request(projected_token, &csr_pem, &self.binding, pod_uid);
        let response = self
            .client
            .post(self.config.identity_exchange_url.clone())
            .json(&request)
            .send()
            .await
            .map_err(|_| WorkloadIdentityError::Unavailable)?;
        let status = response.status();
        if status != reqwest::StatusCode::CREATED && status != reqwest::StatusCode::CONFLICT {
            require_exchange_created(status, None)?;
        }
        let body = match read_bounded_response_body(response).await {
            Ok(body) => body,
            Err(_) if status == reqwest::StatusCode::CONFLICT => {
                return Err(WorkloadIdentityError::Rejected);
            }
            Err(error) => return Err(error),
        };
        require_exchange_created(status, Some(&body))?;
        let response: IdentityExchangeResponse =
            serde_json::from_slice(&body).map_err(|_| WorkloadIdentityError::Unavailable)?;
        build_server_identity(response, key, &self.binding, pod_uid, now)
    }

    pub(super) async fn exchange_client_once(
        &self,
        exchange_url: &url::Url,
        now: DateTime<Utc>,
    ) -> Result<IssuedClientIdentity, WorkloadIdentityError> {
        let projected_token = read_bounded_utf8_file(
            &self.config.kubernetes_token_file,
            MAX_PROJECTED_TOKEN_BYTES,
        )?;
        let projected_token = projected_token.trim();
        let pod_uid = projected_pod_uid(projected_token)?;
        let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
            .map_err(|_| WorkloadIdentityError::Unavailable)?;
        let csr_pem = CertificateParams::default()
            .serialize_request(&key)
            .and_then(|csr| csr.pem())
            .map_err(|_| WorkloadIdentityError::Unavailable)?;
        let request = build_exchange_request(projected_token, &csr_pem, &self.binding, pod_uid);
        let response = self
            .client
            .post(exchange_url.clone())
            .json(&request)
            .send()
            .await
            .map_err(|_| WorkloadIdentityError::Unavailable)?;
        let status = response.status();
        if status != reqwest::StatusCode::CREATED && status != reqwest::StatusCode::CONFLICT {
            require_exchange_created(status, None)?;
        }
        let body = match read_bounded_response_body(response).await {
            Ok(body) => body,
            Err(_) if status == reqwest::StatusCode::CONFLICT => {
                return Err(WorkloadIdentityError::Rejected);
            }
            Err(error) => return Err(error),
        };
        require_exchange_created(status, Some(&body))?;
        let response: IdentityExchangeResponse =
            serde_json::from_slice(&body).map_err(|_| WorkloadIdentityError::Unavailable)?;
        build_client_identity(response, key, &self.binding, pod_uid, now)
    }

    pub(super) async fn exchange_client_initial(
        &self,
        exchange_url: &url::Url,
    ) -> Result<IssuedClientIdentity, WorkloadIdentityError> {
        let deadline = tokio::time::Instant::now() + INITIAL_EXCHANGE_TIMEOUT;
        let mut backoff = INITIAL_EXCHANGE_MIN_BACKOFF;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(WorkloadIdentityError::Unavailable);
            }
            let exchange = tokio::time::timeout(
                remaining,
                self.exchange_client_once(exchange_url, Utc::now()),
            )
            .await
            .unwrap_or(Err(WorkloadIdentityError::Unavailable));
            match exchange {
                Ok(identity) => return Ok(identity),
                Err(WorkloadIdentityError::Unavailable) => {
                    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                    if remaining.is_zero() {
                        return Err(WorkloadIdentityError::Unavailable);
                    }
                    tokio::time::sleep(jittered_initial_exchange_delay(backoff).min(remaining))
                        .await;
                    backoff = backoff.saturating_mul(2).min(INITIAL_EXCHANGE_MAX_BACKOFF);
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub(super) async fn exchange_initial(
        &self,
    ) -> Result<IssuedServerIdentity, WorkloadIdentityError> {
        let deadline = tokio::time::Instant::now() + INITIAL_EXCHANGE_TIMEOUT;
        let mut backoff = INITIAL_EXCHANGE_MIN_BACKOFF;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(WorkloadIdentityError::Unavailable);
            }
            let exchange = tokio::time::timeout(remaining, self.exchange_once(Utc::now()))
                .await
                .unwrap_or(Err(WorkloadIdentityError::Unavailable));
            match exchange {
                Ok(identity) => return Ok(identity),
                Err(WorkloadIdentityError::Unavailable) => {
                    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                    if remaining.is_zero() {
                        return Err(WorkloadIdentityError::Unavailable);
                    }
                    tokio::time::sleep(jittered_initial_exchange_delay(backoff).min(remaining))
                        .await;
                    backoff = backoff.saturating_mul(2).min(INITIAL_EXCHANGE_MAX_BACKOFF);
                }
                Err(error) => return Err(error),
            }
        }
    }
}

pub(super) async fn rotate_client_identity(
    exchanger: Arc<WorkloadIdentityExchanger>,
    exchange_url: url::Url,
    state: ReloadableClientIdentity,
    shutdown: CancellationToken,
) {
    const RENEW_BEFORE_EXPIRY_SECONDS: i64 = 60;
    const MAX_RETRY_SECONDS: u64 = 15;
    loop {
        let Some(expires_at) = state.expires_at().await else {
            break;
        };
        let renew_at = expires_at - Duration::seconds(RENEW_BEFORE_EXPIRY_SECONDS);
        let delay = (renew_at - Utc::now())
            .to_std()
            .unwrap_or(StdDuration::ZERO);
        tokio::select! {
            () = shutdown.cancelled() => break,
            () = tokio::time::sleep(delay) => {}
        }
        let mut retry_seconds = 1_u64;
        let mut active_expiry = Some(expires_at);
        loop {
            let now = Utc::now();
            state.expire_if_due(now).await;
            if active_expiry.is_some_and(|expiry| expiry <= now) {
                active_expiry = None;
            }
            let exchange = exchanger.exchange_client_once(&exchange_url, now);
            let result = if let Some(expiry) = active_expiry {
                let until_expiry = (expiry - Utc::now()).to_std().unwrap_or(StdDuration::ZERO);
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    () = tokio::time::sleep(until_expiry) => {
                        state.expire_if_due(Utc::now()).await;
                        active_expiry = None;
                        continue;
                    }
                    result = exchange => result,
                }
            } else {
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    result = exchange => result,
                }
            };
            match result {
                Ok(identity) => {
                    state.install(identity).await;
                    break;
                }
                Err(_) => {
                    let delay = StdDuration::from_secs(retry_seconds);
                    retry_seconds = (retry_seconds * 2).min(MAX_RETRY_SECONDS);
                    if let Some(expiry) = active_expiry {
                        let until_expiry =
                            (expiry - Utc::now()).to_std().unwrap_or(StdDuration::ZERO);
                        tokio::select! {
                            () = shutdown.cancelled() => return,
                            () = tokio::time::sleep(delay.min(until_expiry)) => {}
                        }
                    } else {
                        tokio::select! {
                            () = shutdown.cancelled() => return,
                            () = tokio::time::sleep(delay) => {}
                        }
                    }
                }
            }
        }
    }
    state.clear().await;
}

#[derive(Deserialize)]
struct IdentityExchangeErrorEnvelope<'a> {
    ok: bool,
    #[serde(borrow)]
    code: &'a str,
}

async fn read_bounded_response_body(
    response: reqwest::Response,
) -> Result<Vec<u8>, WorkloadIdentityError> {
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| WorkloadIdentityError::Unavailable)?;
        if body.len().saturating_add(chunk.len()) > MAX_EXCHANGE_RESPONSE_BYTES {
            return Err(WorkloadIdentityError::Unavailable);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn require_exchange_created(
    status: reqwest::StatusCode,
    body: Option<&[u8]>,
) -> Result<(), WorkloadIdentityError> {
    // Sandboxwich uses this stable conflict code while placement moves toward
    // a live attestation. Only that typed state joins the bounded initial retry;
    // every other conflict remains a terminal rejection.
    let transient_attestation_conflict = status == reqwest::StatusCode::CONFLICT
        && body.is_some_and(|body| {
            serde_json::from_slice::<IdentityExchangeErrorEnvelope<'_>>(body)
                .is_ok_and(|error| !error.ok && error.code == "placement_attestation_not_live")
        });
    if status == reqwest::StatusCode::CREATED {
        Ok(())
    } else if transient_attestation_conflict
        || status.is_server_error()
        || status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
    {
        Err(WorkloadIdentityError::Unavailable)
    } else {
        Err(WorkloadIdentityError::Rejected)
    }
}

fn jittered_initial_exchange_delay_for_sample(base: StdDuration, sample: f64) -> StdDuration {
    let multiplier = 1.0 + INITIAL_EXCHANGE_JITTER_FACTOR * sample.clamp(-1.0, 1.0);
    StdDuration::from_secs_f64(base.as_secs_f64() * multiplier)
}

fn jittered_initial_exchange_delay(base: StdDuration) -> StdDuration {
    let mut rng = rand::rng();
    jittered_initial_exchange_delay_for_sample(base, rng.random_range(-1.0..=1.0))
}

fn read_bounded_utf8_file(path: &Path, max_bytes: usize) -> Result<String, WorkloadIdentityError> {
    let metadata = fs::metadata(path).map_err(|_| WorkloadIdentityError::Unavailable)?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > max_bytes as u64 {
        return Err(WorkloadIdentityError::Unavailable);
    }
    let bytes = fs::read(path).map_err(|_| WorkloadIdentityError::Unavailable)?;
    if bytes.is_empty() || bytes.len() > max_bytes {
        return Err(WorkloadIdentityError::Unavailable);
    }
    String::from_utf8(bytes).map_err(|_| WorkloadIdentityError::Unavailable)
}

#[derive(Serialize)]
pub(super) struct IdentityExchangeRequest {
    organization_id: String,
    workspace_id: String,
    projected_service_account_token: String,
    csr_pem: String,
    sandbox_id: String,
    pod_uid: String,
    generation: u64,
    runner_session_id: String,
}

impl std::fmt::Debug for IdentityExchangeRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IdentityExchangeRequest")
            .field("organization_id", &self.organization_id)
            .field("workspace_id", &self.workspace_id)
            .field("projected_service_account_token", &"<redacted>")
            .field("csr_pem", &"<redacted>")
            .field("sandbox_id", &self.sandbox_id)
            .field("pod_uid", &self.pod_uid)
            .field("generation", &self.generation)
            .field("runner_session_id", &self.runner_session_id)
            .finish()
    }
}

#[derive(Debug, Eq, PartialEq, thiserror::Error)]
pub(super) enum WorkloadIdentityError {
    #[error("projected Kubernetes workload identity is invalid")]
    InvalidProjectedIdentity,
    #[error("issued Maestro workload identity is invalid")]
    InvalidIssuedIdentity,
    #[error("workload identity service is unavailable")]
    Unavailable,
    #[error("workload identity exchange was rejected")]
    Rejected,
}

impl WorkloadIdentityError {
    pub(super) const fn as_str(&self) -> &'static str {
        match self {
            Self::InvalidProjectedIdentity => "invalid_projected_identity",
            Self::InvalidIssuedIdentity => "invalid_issued_identity",
            Self::Unavailable => "unavailable",
            Self::Rejected => "rejected",
        }
    }
}

#[derive(Deserialize)]
struct ProjectedClaims {
    #[serde(rename = "kubernetes.io")]
    kubernetes: KubernetesClaims,
}

#[derive(Deserialize)]
struct KubernetesClaims {
    pod: KubernetesObjectReference,
}

#[derive(Deserialize)]
struct KubernetesObjectReference {
    uid: String,
}

pub(super) fn projected_pod_uid(token: &str) -> Result<Uuid, WorkloadIdentityError> {
    if token.is_empty() || token.len() > MAX_PROJECTED_TOKEN_BYTES {
        return Err(WorkloadIdentityError::InvalidProjectedIdentity);
    }
    let mut segments = token.split('.');
    let _header = segments
        .next()
        .ok_or(WorkloadIdentityError::InvalidProjectedIdentity)?;
    let payload = segments
        .next()
        .ok_or(WorkloadIdentityError::InvalidProjectedIdentity)?;
    let _signature = segments
        .next()
        .ok_or(WorkloadIdentityError::InvalidProjectedIdentity)?;
    if segments.next().is_some() {
        return Err(WorkloadIdentityError::InvalidProjectedIdentity);
    }
    let payload = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| WorkloadIdentityError::InvalidProjectedIdentity)?;
    let claims: ProjectedClaims = serde_json::from_slice(&payload)
        .map_err(|_| WorkloadIdentityError::InvalidProjectedIdentity)?;
    claims
        .kubernetes
        .pod
        .uid
        .parse()
        .map_err(|_| WorkloadIdentityError::InvalidProjectedIdentity)
}

pub(super) fn build_exchange_request(
    projected_token: &str,
    csr_pem: &str,
    binding: &IdentityBinding,
    pod_uid: Uuid,
) -> IdentityExchangeRequest {
    IdentityExchangeRequest {
        organization_id: binding.organization_id.clone(),
        workspace_id: binding.workspace_id.clone(),
        projected_service_account_token: projected_token.to_string(),
        csr_pem: csr_pem.to_string(),
        sandbox_id: binding.sandbox_id.to_string(),
        pod_uid: pod_uid.to_string(),
        generation: binding.placement_generation,
        runner_session_id: binding.runner_session_id.clone(),
    }
}

pub(super) fn validate_identity_contract(
    uri_san: &str,
    binding: &IdentityBinding,
    pod_uid: Uuid,
    now: DateTime<Utc>,
    expires_at: DateTime<Utc>,
) -> Result<ServiceIdentity, WorkloadIdentityError> {
    if expires_at <= now || expires_at - now > Duration::seconds(MAX_CERTIFICATE_TTL_SECONDS) {
        return Err(WorkloadIdentityError::InvalidIssuedIdentity);
    }
    let uri = url::Url::parse(uri_san).map_err(|_| WorkloadIdentityError::InvalidIssuedIdentity)?;
    if uri.scheme() != "spiffe"
        || uri.host_str() != Some("identity.evalops.dev")
        || uri.port().is_some()
        || !uri.username().is_empty()
        || uri.password().is_some()
        || uri.query().is_some()
        || uri.fragment().is_some()
    {
        return Err(WorkloadIdentityError::InvalidIssuedIdentity);
    }
    let segments = uri
        .path_segments()
        .ok_or(WorkloadIdentityError::InvalidIssuedIdentity)?
        .map(|segment| {
            urlencoding::decode(segment)
                .map(|value| value.into_owned())
                .map_err(|_| WorkloadIdentityError::InvalidIssuedIdentity)
        })
        .collect::<Result<Vec<_>, _>>()?;
    if segments.len() != 28
        || segments[0] != "maestro"
        || segments[1] != "v1"
        || segments[2] != "organizations"
        || segments[3] != binding.organization_id
        || segments[4] != "workspaces"
        || segments[5] != binding.workspace_id
        || segments[6] != "sandboxes"
        || segments[7] != binding.sandbox_id.to_string()
        || segments[8] != "pods"
        || segments[9] != pod_uid.to_string()
        || segments[10] != "generations"
        || segments[11] != binding.placement_generation.to_string()
        || segments[12] != "sessions"
        || segments[13] != binding.runner_session_id
        || segments[14] != "images"
        || segments[16] != "services"
        || segments[18] != "ports"
        || segments[20] != "resident-process-generations"
        || segments[22] != "leases"
        || segments[24] != "attempts"
        || segments[26] != "workers"
    {
        return Err(WorkloadIdentityError::InvalidIssuedIdentity);
    }
    let image_digest = &segments[15];
    let service_name = &segments[17];
    let service_port = segments[19].parse::<u16>().ok().filter(|port| *port > 0);
    let resident_generation = segments[21]
        .parse::<u64>()
        .ok()
        .filter(|generation| *generation > 0);
    let lease_id = segments[23].parse::<Uuid>().ok();
    let lease_attempt = segments[25]
        .parse::<u64>()
        .ok()
        .filter(|attempt| *attempt > 0);
    let worker_id = segments[27].parse::<Uuid>().ok();
    if image_digest.len() != 64
        || !image_digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || service_name.is_empty()
        || service_name.len() > 253
        || !service_name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || service_port.is_none()
        || resident_generation.is_none()
        || lease_id.is_none()
        || lease_attempt.is_none()
        || worker_id.is_none()
    {
        return Err(WorkloadIdentityError::InvalidIssuedIdentity);
    }
    Ok(ServiceIdentity {
        service_name: service_name.clone(),
        service_port: service_port.expect("validated service port"),
        image_digest: image_digest.clone(),
    })
}

pub(super) fn build_server_identity(
    response: IdentityExchangeResponse,
    key: KeyPair,
    binding: &IdentityBinding,
    pod_uid: Uuid,
    now: DateTime<Utc>,
) -> Result<IssuedServerIdentity, WorkloadIdentityError> {
    let expires_at = DateTime::parse_from_rfc3339(&response.expires_at)
        .map_err(|_| WorkloadIdentityError::InvalidIssuedIdentity)?
        .with_timezone(&Utc);
    let service = validate_identity_contract(&response.uri_san, binding, pod_uid, now, expires_at)?;
    #[cfg(not(test))]
    let _ = &service;
    if response.serial_number.is_empty() || response.serial_number.len() > 128 {
        return Err(WorkloadIdentityError::InvalidIssuedIdentity);
    }
    let mut certificates = CertificateDer::pem_slice_iter(response.certificate_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| WorkloadIdentityError::InvalidIssuedIdentity)?;
    if certificates.len() != 1 {
        return Err(WorkloadIdentityError::InvalidIssuedIdentity);
    }
    validate_leaf_certificate(&certificates[0], &response.uri_san, expires_at)?;
    let ca_certificates = CertificateDer::pem_slice_iter(response.ca_certificate_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| WorkloadIdentityError::InvalidIssuedIdentity)?;
    if ca_certificates.len() != 1 {
        return Err(WorkloadIdentityError::InvalidIssuedIdentity);
    }
    validate_leaf_issuer(&certificates[0], &ca_certificates[0], now, expires_at)?;
    let crypto_provider = Arc::new(rustls::crypto::ring::default_provider());
    let client_verifier = build_runner_client_verifier(&ca_certificates, crypto_provider.clone())?;
    certificates.extend(ca_certificates);
    let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
    let mut tls_config = rustls::ServerConfig::builder_with_provider(crypto_provider)
        .with_safe_default_protocol_versions()
        .map_err(|_| WorkloadIdentityError::InvalidIssuedIdentity)?
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(certificates, private_key)
        .map_err(|_| WorkloadIdentityError::InvalidIssuedIdentity)?;
    tls_config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(IssuedServerIdentity {
        tls_config: Arc::new(tls_config),
        expires_at,
        #[cfg(test)]
        uri_san: response.uri_san,
        #[cfg(test)]
        service,
    })
}

pub(super) fn build_client_identity(
    response: IdentityExchangeResponse,
    key: KeyPair,
    binding: &IdentityBinding,
    pod_uid: Uuid,
    now: DateTime<Utc>,
) -> Result<IssuedClientIdentity, WorkloadIdentityError> {
    let expires_at = DateTime::parse_from_rfc3339(&response.expires_at)
        .map_err(|_| WorkloadIdentityError::InvalidIssuedIdentity)?
        .with_timezone(&Utc);
    let _service =
        validate_identity_contract(&response.uri_san, binding, pod_uid, now, expires_at)?;
    if response.serial_number.is_empty() || response.serial_number.len() > 128 {
        return Err(WorkloadIdentityError::InvalidIssuedIdentity);
    }
    let mut certificates = CertificateDer::pem_slice_iter(response.certificate_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| WorkloadIdentityError::InvalidIssuedIdentity)?;
    if certificates.len() != 1 {
        return Err(WorkloadIdentityError::InvalidIssuedIdentity);
    }
    validate_client_leaf_certificate(&certificates[0], &response.uri_san, expires_at)?;
    let ca_certificates = CertificateDer::pem_slice_iter(response.ca_certificate_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| WorkloadIdentityError::InvalidIssuedIdentity)?;
    if ca_certificates.len() != 1 {
        return Err(WorkloadIdentityError::InvalidIssuedIdentity);
    }
    validate_leaf_issuer(&certificates[0], &ca_certificates[0], now, expires_at)?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(ca_certificates[0].clone())
        .map_err(|_| WorkloadIdentityError::InvalidIssuedIdentity)?;
    certificates.extend(ca_certificates);
    let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
    let tls_config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|_| WorkloadIdentityError::InvalidIssuedIdentity)?
        .with_root_certificates(roots)
        .with_client_auth_cert(certificates, private_key)
        .map_err(|_| WorkloadIdentityError::InvalidIssuedIdentity)?;
    Ok(IssuedClientIdentity {
        tls_config: Arc::new(tls_config),
        expires_at,
        #[cfg(test)]
        uri_san: response.uri_san,
    })
}

fn validate_leaf_issuer(
    leaf: &CertificateDer<'_>,
    ca: &CertificateDer<'_>,
    now: DateTime<Utc>,
    leaf_expiry: DateTime<Utc>,
) -> Result<(), WorkloadIdentityError> {
    let (_, leaf) = parse_x509_certificate(leaf.as_ref())
        .map_err(|_| WorkloadIdentityError::InvalidIssuedIdentity)?;
    let (_, ca) = parse_x509_certificate(ca.as_ref())
        .map_err(|_| WorkloadIdentityError::InvalidIssuedIdentity)?;
    let now = x509_parser::time::ASN1Time::from_timestamp(now.timestamp())
        .map_err(|_| WorkloadIdentityError::InvalidIssuedIdentity)?;
    let leaf_expiry = x509_parser::time::ASN1Time::from_timestamp(leaf_expiry.timestamp())
        .map_err(|_| WorkloadIdentityError::InvalidIssuedIdentity)?;
    let basic_constraints = ca
        .basic_constraints()
        .map_err(|_| WorkloadIdentityError::InvalidIssuedIdentity)?
        .ok_or(WorkloadIdentityError::InvalidIssuedIdentity)?;
    let key_usage = ca
        .key_usage()
        .map_err(|_| WorkloadIdentityError::InvalidIssuedIdentity)?;
    if leaf.issuer() != ca.subject()
        || !basic_constraints.value.ca
        || key_usage.is_some_and(|usage| !usage.value.key_cert_sign())
        || !ca.validity().is_valid_at(now)
        || !ca.validity().is_valid_at(leaf_expiry)
    {
        return Err(WorkloadIdentityError::InvalidIssuedIdentity);
    }
    leaf.verify_signature(Some(ca.public_key()))
        .map_err(|_| WorkloadIdentityError::InvalidIssuedIdentity)
}

fn build_runner_client_verifier(
    ca_certificates: &[CertificateDer<'static>],
    crypto_provider: Arc<rustls::crypto::CryptoProvider>,
) -> Result<Arc<dyn ClientCertVerifier>, WorkloadIdentityError> {
    let mut client_roots = rustls::RootCertStore::empty();
    for ca_certificate in ca_certificates {
        client_roots
            .add(ca_certificate.clone())
            .map_err(|_| WorkloadIdentityError::InvalidIssuedIdentity)?;
    }
    let chain_verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
        Arc::new(client_roots),
        crypto_provider,
    )
    .build()
    .map_err(|_| WorkloadIdentityError::InvalidIssuedIdentity)?;
    Ok(Arc::new(ExactClientUriVerifier {
        chain_verifier,
        expected_uri: RUNNER_HOST_CLIENT_URI,
    }))
}

struct ExactClientUriVerifier {
    chain_verifier: Arc<dyn ClientCertVerifier>,
    expected_uri: &'static str,
}

impl fmt::Debug for ExactClientUriVerifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExactClientUriVerifier")
            .field("expected_uri", &self.expected_uri)
            .finish_non_exhaustive()
    }
}

impl ClientCertVerifier for ExactClientUriVerifier {
    fn offer_client_auth(&self) -> bool {
        self.chain_verifier.offer_client_auth()
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        self.chain_verifier.root_hint_subjects()
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, TlsError> {
        let verified = self
            .chain_verifier
            .verify_client_cert(end_entity, intermediates, now)?;
        validate_runner_client_certificate(end_entity, self.expected_uri).map_err(|_| {
            TlsError::InvalidCertificate(rustls::CertificateError::ApplicationVerificationFailure)
        })?;
        Ok(verified)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.chain_verifier
            .verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.chain_verifier
            .verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.chain_verifier.supported_verify_schemes()
    }
}

fn validate_runner_client_certificate(
    certificate: &CertificateDer<'_>,
    expected_uri: &str,
) -> Result<(), WorkloadIdentityError> {
    let (_, certificate) = parse_x509_certificate(certificate.as_ref())
        .map_err(|_| WorkloadIdentityError::InvalidIssuedIdentity)?;
    let subject_alt_name = certificate
        .subject_alternative_name()
        .map_err(|_| WorkloadIdentityError::InvalidIssuedIdentity)?
        .ok_or(WorkloadIdentityError::InvalidIssuedIdentity)?;
    let uri_names = subject_alt_name
        .value
        .general_names
        .iter()
        .filter_map(|name| match name {
            GeneralName::URI(uri) => Some(*uri),
            _ => None,
        })
        .collect::<Vec<_>>();
    let extended_key_usage = certificate
        .extended_key_usage()
        .map_err(|_| WorkloadIdentityError::InvalidIssuedIdentity)?
        .ok_or(WorkloadIdentityError::InvalidIssuedIdentity)?;
    let is_ca = certificate
        .basic_constraints()
        .map_err(|_| WorkloadIdentityError::InvalidIssuedIdentity)?
        .is_some_and(|constraints| constraints.value.ca);
    if uri_names != [expected_uri]
        || !extended_key_usage.value.client_auth
        || extended_key_usage.value.server_auth
        || is_ca
    {
        return Err(WorkloadIdentityError::InvalidIssuedIdentity);
    }
    Ok(())
}

fn validate_leaf_certificate(
    certificate: &CertificateDer<'_>,
    expected_uri_san: &str,
    expected_expiry: DateTime<Utc>,
) -> Result<(), WorkloadIdentityError> {
    let (_, certificate) = parse_x509_certificate(certificate.as_ref())
        .map_err(|_| WorkloadIdentityError::InvalidIssuedIdentity)?;
    let subject_alt_name = certificate
        .subject_alternative_name()
        .map_err(|_| WorkloadIdentityError::InvalidIssuedIdentity)?
        .ok_or(WorkloadIdentityError::InvalidIssuedIdentity)?;
    let uri_names = subject_alt_name
        .value
        .general_names
        .iter()
        .filter_map(|name| match name {
            GeneralName::URI(uri) => Some(*uri),
            _ => None,
        })
        .collect::<Vec<_>>();
    let extended_key_usage = certificate
        .extended_key_usage()
        .map_err(|_| WorkloadIdentityError::InvalidIssuedIdentity)?
        .ok_or(WorkloadIdentityError::InvalidIssuedIdentity)?;
    let is_ca = certificate
        .basic_constraints()
        .map_err(|_| WorkloadIdentityError::InvalidIssuedIdentity)?
        .is_some_and(|constraints| constraints.value.ca);
    if uri_names != [expected_uri_san]
        || !extended_key_usage.value.server_auth
        || extended_key_usage.value.client_auth
        || is_ca
        || certificate.validity().not_after.timestamp() != expected_expiry.timestamp()
    {
        return Err(WorkloadIdentityError::InvalidIssuedIdentity);
    }
    Ok(())
}

fn validate_client_leaf_certificate(
    certificate: &CertificateDer<'_>,
    expected_uri_san: &str,
    expected_expiry: DateTime<Utc>,
) -> Result<(), WorkloadIdentityError> {
    const EXPECTED_CN: &str = "maestro-hosted-runner-rendezvous-client";
    let (_, certificate) = parse_x509_certificate(certificate.as_ref())
        .map_err(|_| WorkloadIdentityError::InvalidIssuedIdentity)?;
    let subject_alt_name = certificate
        .subject_alternative_name()
        .map_err(|_| WorkloadIdentityError::InvalidIssuedIdentity)?
        .ok_or(WorkloadIdentityError::InvalidIssuedIdentity)?;
    let uri_names = subject_alt_name
        .value
        .general_names
        .iter()
        .filter_map(|name| match name {
            GeneralName::URI(uri) => Some(*uri),
            _ => None,
        })
        .collect::<Vec<_>>();
    let common_names = certificate
        .subject()
        .iter_common_name()
        .map(|name| name.as_str())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| WorkloadIdentityError::InvalidIssuedIdentity)?;
    let extended_key_usage = certificate
        .extended_key_usage()
        .map_err(|_| WorkloadIdentityError::InvalidIssuedIdentity)?
        .ok_or(WorkloadIdentityError::InvalidIssuedIdentity)?;
    let is_ca = certificate
        .basic_constraints()
        .map_err(|_| WorkloadIdentityError::InvalidIssuedIdentity)?
        .is_some_and(|constraints| constraints.value.ca);
    if uri_names != [expected_uri_san]
        || common_names != [EXPECTED_CN]
        || !extended_key_usage.value.client_auth
        || extended_key_usage.value.server_auth
        || is_ca
        || certificate.validity().not_after.timestamp() != expected_expiry.timestamp()
    {
        return Err(WorkloadIdentityError::InvalidIssuedIdentity);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use chrono::{Duration, Utc};
    use rcgen::{
        BasicConstraints, CertificateParams, CertificateSigningRequestParams, DistinguishedName,
        DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, SanType,
    };
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, UnixTime};
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use uuid::Uuid;

    use super::{
        INITIAL_EXCHANGE_JITTER_FACTOR, IdentityBinding, IdentityExchangeResponse,
        RUNNER_HOST_CLIENT_URI, ReloadableServerIdentity, WorkloadIdentityError,
        WorkloadIdentityExchanger, build_client_identity, build_exchange_request,
        build_runner_client_verifier, build_server_identity,
        jittered_initial_exchange_delay_for_sample, projected_pod_uid, require_exchange_created,
        validate_identity_contract,
    };
    use crate::hosted_runner::config::HostedRunnerWorkloadIdentityConfig;

    fn projected_token(pod_uid: Uuid) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","kid":"test"}"#);
        let claims = URL_SAFE_NO_PAD.encode(
            serde_json::json!({
                "iss": "https://kubernetes.default.svc",
                "aud": ["https://identity.evalops.dev/v1/workload-certificates"],
                "sub": "system:serviceaccount:sandboxes:maestro-workload",
                "exp": 1_900_000_600_i64,
                "iat": 1_900_000_000_i64,
                "nbf": 1_900_000_000_i64,
                "kubernetes.io": {
                    "namespace": "sandboxes",
                    "pod": {"name": "maestro-pod", "uid": pod_uid.to_string()},
                    "serviceaccount": {
                        "name": "maestro-workload",
                        "uid": "21234567-89ab-cdef-0123-456789abcdef"
                    },
                    "warnafter": 1_900_000_480_i64
                }
            })
            .to_string(),
        );
        format!("{header}.{claims}.signature")
    }

    fn binding() -> IdentityBinding {
        IdentityBinding {
            organization_id: "org-123".into(),
            workspace_id: "workspace-123".into(),
            sandbox_id: Uuid::parse_str("01234567-89ab-cdef-0123-456789abcdef").unwrap(),
            placement_generation: 7,
            runner_session_id: "session/with spaces".into(),
        }
    }

    #[test]
    fn projected_pod_uid_comes_from_the_signed_kubernetes_claim_shape() {
        let pod_uid = Uuid::parse_str("11234567-89ab-cdef-0123-456789abcdef").unwrap();

        assert_eq!(
            projected_pod_uid(&projected_token(pod_uid)).expect("pod uid"),
            pod_uid
        );
        assert!(projected_pod_uid("not-a-jwt").is_err());
    }

    #[test]
    fn initial_exchange_retries_only_transient_statuses_with_bounded_jitter() {
        assert_eq!(
            require_exchange_created(reqwest::StatusCode::CREATED, None),
            Ok(())
        );
        assert_eq!(
            require_exchange_created(reqwest::StatusCode::SERVICE_UNAVAILABLE, None),
            Err(WorkloadIdentityError::Unavailable)
        );
        assert_eq!(
            require_exchange_created(reqwest::StatusCode::TOO_MANY_REQUESTS, None),
            Err(WorkloadIdentityError::Unavailable)
        );
        assert_eq!(
            require_exchange_created(reqwest::StatusCode::FORBIDDEN, None),
            Err(WorkloadIdentityError::Rejected)
        );
        assert_eq!(
            require_exchange_created(
                reqwest::StatusCode::CONFLICT,
                Some(&br#"{"ok":false,"code":"placement_attestation_not_live","message":"Maestro hosted runner is not running"}"#[..]),
            ),
            Err(WorkloadIdentityError::Unavailable)
        );
        assert_eq!(
            require_exchange_created(
                reqwest::StatusCode::CONFLICT,
                Some(&br#"{"ok":false,"code":"identity_binding_mismatch","message":"binding rejected"}"#[..]),
            ),
            Err(WorkloadIdentityError::Rejected)
        );

        let base = std::time::Duration::from_secs(10);
        assert_eq!(
            jittered_initial_exchange_delay_for_sample(base, -1.0),
            base.mul_f64(1.0 - INITIAL_EXCHANGE_JITTER_FACTOR)
        );
        assert_eq!(
            jittered_initial_exchange_delay_for_sample(base, 1.0),
            base.mul_f64(1.0 + INITIAL_EXCHANGE_JITTER_FACTOR)
        );
    }

    #[test]
    fn workload_identity_error_kinds_are_stable_safe_labels() {
        assert_eq!(
            WorkloadIdentityError::InvalidProjectedIdentity.as_str(),
            "invalid_projected_identity"
        );
        assert_eq!(
            WorkloadIdentityError::InvalidIssuedIdentity.as_str(),
            "invalid_issued_identity"
        );
        assert_eq!(WorkloadIdentityError::Unavailable.as_str(), "unavailable");
        assert_eq!(WorkloadIdentityError::Rejected.as_str(), "rejected");
    }

    #[test]
    fn identity_exchange_request_binds_proof_without_debugging_secrets() {
        let pod_uid = Uuid::parse_str("11234567-89ab-cdef-0123-456789abcdef").unwrap();

        let request =
            build_exchange_request("projected-secret", "private-csr", &binding(), pod_uid);
        let value = serde_json::to_value(&request).expect("request json");
        let debug = format!("{request:?}");

        assert_eq!(value["organization_id"], "org-123");
        assert_eq!(value["workspace_id"], "workspace-123");
        assert_eq!(value["sandbox_id"], "01234567-89ab-cdef-0123-456789abcdef");
        assert_eq!(value["pod_uid"], "11234567-89ab-cdef-0123-456789abcdef");
        assert_eq!(value["generation"], 7);
        assert_eq!(value["runner_session_id"], "session/with spaces");
        assert_eq!(value["projected_service_account_token"], "projected-secret");
        assert_eq!(value["csr_pem"], "private-csr");
        assert!(!debug.contains("projected-secret"));
        assert!(!debug.contains("private-csr"));
        assert_eq!(debug.matches("<redacted>").count(), 2);
    }

    #[test]
    fn issued_identity_requires_the_exact_platform_spiffe_binding() {
        let pod_uid = Uuid::parse_str("11234567-89ab-cdef-0123-456789abcdef").unwrap();
        let now = Utc::now();
        let uri = concat!(
            "spiffe://identity.evalops.dev/maestro/v1/",
            "organizations/org-123/workspaces/workspace-123/",
            "sandboxes/01234567-89ab-cdef-0123-456789abcdef/",
            "pods/11234567-89ab-cdef-0123-456789abcdef/",
            "generations/7/sessions/session%2Fwith%20spaces/",
            "images/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/",
            "services/sw-msvc-123/ports/8443/",
            "resident-process-generations/4/",
            "leases/31234567-89ab-cdef-0123-456789abcdef/",
            "attempts/2/workers/41234567-89ab-cdef-0123-456789abcdef"
        );

        let identity =
            validate_identity_contract(uri, &binding(), pod_uid, now, now + Duration::minutes(5))
                .expect("exact identity");

        assert_eq!(identity.service_name, "sw-msvc-123");
        assert_eq!(identity.service_port, 8443);
        assert_eq!(identity.image_digest, "a".repeat(64));
    }

    #[test]
    fn issued_identity_rejects_session_drift_and_excessive_lifetime() {
        let pod_uid = Uuid::parse_str("11234567-89ab-cdef-0123-456789abcdef").unwrap();
        let now = Utc::now();
        let wrong_session = concat!(
            "spiffe://identity.evalops.dev/maestro/v1/",
            "organizations/org-123/workspaces/workspace-123/",
            "sandboxes/01234567-89ab-cdef-0123-456789abcdef/",
            "pods/11234567-89ab-cdef-0123-456789abcdef/",
            "generations/7/sessions/other-session/",
            "images/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/",
            "services/sw-msvc-123/ports/8443/",
            "resident-process-generations/4/",
            "leases/31234567-89ab-cdef-0123-456789abcdef/",
            "attempts/2/workers/41234567-89ab-cdef-0123-456789abcdef"
        );

        assert!(
            validate_identity_contract(
                wrong_session,
                &binding(),
                pod_uid,
                now,
                now + Duration::minutes(5)
            )
            .is_err()
        );
        let exact = wrong_session.replace("other-session", "session%2Fwith%20spaces");
        assert!(
            validate_identity_contract(
                &exact,
                &binding(),
                pod_uid,
                now,
                now + Duration::seconds(301)
            )
            .is_err()
        );
    }

    fn signed_exchange_response(
        csr_pem: &str,
        uri_san: &str,
        expires_at: chrono::DateTime<Utc>,
    ) -> IdentityExchangeResponse {
        let mut ca_params = CertificateParams::default();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "test workload CA");
        let ca_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let ca_certificate = ca_params.self_signed(&ca_key).unwrap();
        let ca_pem = ca_certificate.pem();
        let issuer = Issuer::from_ca_cert_pem(&ca_pem, ca_key).unwrap();
        let mut csr = CertificateSigningRequestParams::from_pem(csr_pem).unwrap();
        csr.params.distinguished_name = DistinguishedName::new();
        csr.params.subject_alt_names = vec![SanType::URI(
            rcgen::string::Ia5String::try_from(uri_san).unwrap(),
        )];
        csr.params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        csr.params.not_after =
            time::OffsetDateTime::from_unix_timestamp(expires_at.timestamp()).unwrap();
        let certificate = csr.signed_by(&issuer).unwrap();
        IdentityExchangeResponse {
            certificate_pem: certificate.pem(),
            ca_certificate_pem: ca_pem,
            serial_number: "0123456789abcdef".into(),
            expires_at: expires_at.to_rfc3339(),
            uri_san: uri_san.into(),
        }
    }

    fn signed_client_exchange_response(
        csr_pem: &str,
        uri_san: &str,
        expires_at: chrono::DateTime<Utc>,
        eku: ExtendedKeyUsagePurpose,
    ) -> IdentityExchangeResponse {
        let mut ca_params = CertificateParams::default();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let ca_certificate = ca_params.self_signed(&ca_key).unwrap();
        let ca_pem = ca_certificate.pem();
        let issuer = Issuer::from_ca_cert_pem(&ca_pem, ca_key).unwrap();
        let mut csr = CertificateSigningRequestParams::from_pem(csr_pem).unwrap();
        csr.params.distinguished_name = DistinguishedName::new();
        csr.params.distinguished_name.push(
            DnType::CommonName,
            "maestro-hosted-runner-rendezvous-client",
        );
        csr.params.subject_alt_names = vec![SanType::URI(
            rcgen::string::Ia5String::try_from(uri_san).unwrap(),
        )];
        csr.params.extended_key_usages = vec![eku];
        csr.params.not_after =
            time::OffsetDateTime::from_unix_timestamp(expires_at.timestamp()).unwrap();
        let certificate = csr.signed_by(&issuer).unwrap();
        IdentityExchangeResponse {
            certificate_pem: certificate.pem(),
            ca_certificate_pem: ca_pem,
            serial_number: "0123456789abcdef".into(),
            expires_at: expires_at.to_rfc3339(),
            uri_san: uri_san.into(),
        }
    }

    #[test]
    fn rendezvous_client_identity_requires_client_auth_only_and_exact_cn() {
        let pod_uid = Uuid::parse_str("11234567-89ab-cdef-0123-456789abcdef").unwrap();
        let now = Utc::now();
        let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let csr = CertificateParams::default()
            .serialize_request(&key)
            .unwrap()
            .pem()
            .unwrap();
        let uri = concat!(
            "spiffe://identity.evalops.dev/maestro/v1/",
            "organizations/org-123/workspaces/workspace-123/",
            "sandboxes/01234567-89ab-cdef-0123-456789abcdef/",
            "pods/11234567-89ab-cdef-0123-456789abcdef/",
            "generations/7/sessions/session%2Fwith%20spaces/",
            "images/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/",
            "services/sw-msvc-123/ports/8443/",
            "resident-process-generations/4/",
            "leases/31234567-89ab-cdef-0123-456789abcdef/",
            "attempts/2/workers/41234567-89ab-cdef-0123-456789abcdef"
        );
        let expires_at = now + Duration::minutes(4);
        let response = signed_client_exchange_response(
            &csr,
            uri,
            expires_at,
            ExtendedKeyUsagePurpose::ClientAuth,
        );
        let identity = build_client_identity(response, key, &binding(), pod_uid, now)
            .expect("client identity");
        assert_eq!(identity.expires_at, expires_at);
        assert_eq!(identity.uri_san, uri);

        let wrong_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let wrong_csr = CertificateParams::default()
            .serialize_request(&wrong_key)
            .unwrap()
            .pem()
            .unwrap();
        let server_only = signed_client_exchange_response(
            &wrong_csr,
            uri,
            expires_at,
            ExtendedKeyUsagePurpose::ServerAuth,
        );
        assert!(build_client_identity(server_only, wrong_key, &binding(), pod_uid, now).is_err());
    }

    #[test]
    fn server_identity_uses_in_memory_csr_key_and_exact_certificate_san() {
        let pod_uid = Uuid::parse_str("11234567-89ab-cdef-0123-456789abcdef").unwrap();
        let now = Utc::now();
        let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let csr = CertificateParams::default()
            .serialize_request(&key)
            .unwrap()
            .pem()
            .unwrap();
        let uri = concat!(
            "spiffe://identity.evalops.dev/maestro/v1/",
            "organizations/org-123/workspaces/workspace-123/",
            "sandboxes/01234567-89ab-cdef-0123-456789abcdef/",
            "pods/11234567-89ab-cdef-0123-456789abcdef/",
            "generations/7/sessions/session%2Fwith%20spaces/",
            "images/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/",
            "services/sw-msvc-123/ports/8443/",
            "resident-process-generations/4/",
            "leases/31234567-89ab-cdef-0123-456789abcdef/",
            "attempts/2/workers/41234567-89ab-cdef-0123-456789abcdef"
        );
        let response = signed_exchange_response(&csr, uri, now + Duration::minutes(5));

        let identity = build_server_identity(response, key, &binding(), pod_uid, now)
            .expect("server identity");

        assert_eq!(identity.uri_san, uri);
        assert_eq!(identity.expires_at, now + Duration::minutes(5));
        assert_eq!(identity.service.service_port, 8443);
    }

    #[test]
    fn server_identity_rejects_response_metadata_that_differs_from_certificate_san() {
        let pod_uid = Uuid::parse_str("11234567-89ab-cdef-0123-456789abcdef").unwrap();
        let now = Utc::now();
        let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let csr = CertificateParams::default()
            .serialize_request(&key)
            .unwrap()
            .pem()
            .unwrap();
        let certificate_uri = concat!(
            "spiffe://identity.evalops.dev/maestro/v1/",
            "organizations/org-123/workspaces/workspace-123/",
            "sandboxes/01234567-89ab-cdef-0123-456789abcdef/",
            "pods/11234567-89ab-cdef-0123-456789abcdef/",
            "generations/7/sessions/session%2Fwith%20spaces/",
            "images/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/",
            "services/sw-msvc-123/ports/8443/",
            "resident-process-generations/4/",
            "leases/31234567-89ab-cdef-0123-456789abcdef/",
            "attempts/2/workers/41234567-89ab-cdef-0123-456789abcdef"
        );
        let mut response =
            signed_exchange_response(&csr, certificate_uri, now + Duration::minutes(5));
        response.uri_san = response.uri_san.replace("sw-msvc-123", "sw-msvc-attacker");

        assert!(build_server_identity(response, key, &binding(), pod_uid, now).is_err());
    }

    #[test]
    fn server_identity_rejects_a_response_ca_that_did_not_issue_the_leaf() {
        let pod_uid = Uuid::parse_str("11234567-89ab-cdef-0123-456789abcdef").unwrap();
        let now = Utc::now();
        let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let csr = CertificateParams::default()
            .serialize_request(&key)
            .unwrap()
            .pem()
            .unwrap();
        let uri = concat!(
            "spiffe://identity.evalops.dev/maestro/v1/",
            "organizations/org-123/workspaces/workspace-123/",
            "sandboxes/01234567-89ab-cdef-0123-456789abcdef/",
            "pods/11234567-89ab-cdef-0123-456789abcdef/",
            "generations/7/sessions/session%2Fwith%20spaces/",
            "images/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/",
            "services/sw-msvc-123/ports/8443/",
            "resident-process-generations/4/",
            "leases/31234567-89ab-cdef-0123-456789abcdef/",
            "attempts/2/workers/41234567-89ab-cdef-0123-456789abcdef"
        );
        let mut response = signed_exchange_response(&csr, uri, now + Duration::minutes(5));
        let mut different_ca_params = CertificateParams::default();
        different_ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let different_ca_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        response.ca_certificate_pem = different_ca_params
            .self_signed(&different_ca_key)
            .unwrap()
            .pem();

        assert!(build_server_identity(response, key, &binding(), pod_uid, now).is_err());
    }

    fn test_client_ca() -> (CertificateDer<'static>, Issuer<'static, KeyPair>) {
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let certificate = params.self_signed(&key).unwrap();
        (
            CertificateDer::from(certificate.der().to_vec()),
            Issuer::new(params, key),
        )
    }

    fn signed_client_identity(
        issuer: &Issuer<'static, KeyPair>,
        uri: &str,
    ) -> CertificateDer<'static> {
        let mut params = CertificateParams::default();
        params.subject_alt_names = vec![SanType::URI(
            rcgen::string::Ia5String::try_from(uri).unwrap(),
        )];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let certificate = params.signed_by(&key, issuer).unwrap();
        CertificateDer::from(certificate.der().to_vec())
    }

    #[test]
    fn runner_client_auth_uses_only_response_ca_and_exact_uri() {
        let (response_ca, response_issuer) = test_client_ca();
        let (_, other_response_issuer) = test_client_ca();
        let verifier = build_runner_client_verifier(
            &[response_ca],
            Arc::new(rustls::crypto::ring::default_provider()),
        )
        .unwrap();
        let exact = signed_client_identity(&response_issuer, RUNNER_HOST_CLIENT_URI);
        let wrong_ca = signed_client_identity(&other_response_issuer, RUNNER_HOST_CLIENT_URI);
        let wrong_uri = signed_client_identity(
            &response_issuer,
            "spiffe://identity.evalops.dev/service/other",
        );

        assert!(
            verifier
                .verify_client_cert(&exact, &[], UnixTime::now())
                .is_ok()
        );
        assert!(
            verifier
                .verify_client_cert(&wrong_ca, &[], UnixTime::now())
                .is_err()
        );
        assert!(
            verifier
                .verify_client_cert(&wrong_uri, &[], UnixTime::now())
                .is_err()
        );
    }

    struct IdentityHarness {
        _directory: TempDir,
        token_file: std::path::PathBuf,
        ca_file: std::path::PathBuf,
        exchange_url: url::Url,
        workload_ca_pem: String,
        exact_client_identity_pem: String,
        wrong_uri_client_identity_pem: String,
        requests: Arc<Mutex<Vec<serde_json::Value>>>,
        task: tokio::task::JoinHandle<()>,
    }

    async fn start_identity_harness(expected_exchanges: usize) -> IdentityHarness {
        start_identity_harness_with_unavailable(expected_exchanges, 0).await
    }

    async fn start_identity_harness_with_unavailable(
        expected_exchanges: usize,
        unavailable_exchanges: usize,
    ) -> IdentityHarness {
        start_identity_harness_with_failures(expected_exchanges, unavailable_exchanges, 0).await
    }

    async fn start_identity_harness_with_transient_conflicts(
        expected_exchanges: usize,
        transient_conflict_exchanges: usize,
    ) -> IdentityHarness {
        start_identity_harness_with_failures(expected_exchanges, 0, transient_conflict_exchanges)
            .await
    }

    async fn start_identity_harness_with_failures(
        expected_exchanges: usize,
        unavailable_exchanges: usize,
        transient_conflict_exchanges: usize,
    ) -> IdentityHarness {
        let directory = tempfile::tempdir().unwrap();
        let token_file = directory.path().join("token");
        let ca_file = directory.path().join("identity-ca.pem");
        let pod_uid = Uuid::parse_str("11234567-89ab-cdef-0123-456789abcdef").unwrap();
        std::fs::write(&token_file, projected_token(pod_uid)).unwrap();

        let mut ca_params = CertificateParams::default();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "test Identity HTTPS CA");
        let ca_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let ca_certificate = ca_params.self_signed(&ca_key).unwrap();
        let ca_pem = ca_certificate.pem();
        std::fs::write(&ca_file, &ca_pem).unwrap();
        let borrowed_issuer = Issuer::from_params(&ca_params, &ca_key);
        let mut server_params = CertificateParams::new(vec!["127.0.0.1".into()]).unwrap();
        server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let server_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let server_certificate = server_params
            .signed_by(&server_key, &borrowed_issuer)
            .unwrap();
        let client_identity = |uri: &str| {
            let mut params = CertificateParams::default();
            params.subject_alt_names = vec![SanType::URI(
                rcgen::string::Ia5String::try_from(uri).unwrap(),
            )];
            params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
            let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
            let certificate = params.signed_by(&key, &borrowed_issuer).unwrap();
            format!("{}{}", certificate.pem(), key.serialize_pem())
        };
        let exact_client_identity_pem = client_identity(RUNNER_HOST_CLIENT_URI);
        let wrong_uri_client_identity_pem =
            client_identity("spiffe://identity.evalops.dev/service/other");
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let server_tls = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![
                    CertificateDer::from(server_certificate.der().to_vec()),
                    CertificateDer::from(ca_certificate.der().to_vec()),
                ],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(server_key.serialize_der())),
            )
            .unwrap();
        let issuer = Issuer::from_ca_cert_pem(&ca_pem, ca_key).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let task_requests = requests.clone();
        let workload_ca_pem = ca_pem.clone();
        let task = tokio::spawn(async move {
            let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_tls));
            for exchange_index in 0..expected_exchanges {
                let (socket, _) = listener.accept().await.unwrap();
                let mut socket = acceptor.accept(socket).await.unwrap();
                let mut request = Vec::new();
                let body_offset = loop {
                    let mut chunk = [0_u8; 4096];
                    let read = socket.read(&mut chunk).await.unwrap();
                    assert!(read > 0);
                    request.extend_from_slice(&chunk[..read]);
                    if let Some(offset) =
                        request.windows(4).position(|window| window == b"\r\n\r\n")
                    {
                        break offset + 4;
                    }
                };
                let headers = std::str::from_utf8(&request[..body_offset]).unwrap();
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("content-length: ")
                            .or_else(|| line.strip_prefix("Content-Length: "))
                    })
                    .unwrap()
                    .trim()
                    .parse::<usize>()
                    .unwrap();
                while request.len() - body_offset < content_length {
                    let mut chunk = [0_u8; 4096];
                    let read = socket.read(&mut chunk).await.unwrap();
                    assert!(read > 0);
                    request.extend_from_slice(&chunk[..read]);
                }
                let request: serde_json::Value =
                    serde_json::from_slice(&request[body_offset..body_offset + content_length])
                        .unwrap();
                task_requests.lock().unwrap().push(request.clone());
                if exchange_index < unavailable_exchanges {
                    socket
                        .write_all(
                            b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                        )
                        .await
                        .unwrap();
                    continue;
                }
                if exchange_index < unavailable_exchanges + transient_conflict_exchanges {
                    let response = br#"{"ok":false,"code":"placement_attestation_not_live","message":"Maestro hosted runner is not running"}"#;
                    let head = format!(
                        "HTTP/1.1 409 Conflict\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        response.len()
                    );
                    socket.write_all(head.as_bytes()).await.unwrap();
                    socket.write_all(response).await.unwrap();
                    continue;
                }
                let csr = request["csr_pem"].as_str().unwrap();
                let uri = concat!(
                    "spiffe://identity.evalops.dev/maestro/v1/",
                    "organizations/org-123/workspaces/workspace-123/",
                    "sandboxes/01234567-89ab-cdef-0123-456789abcdef/",
                    "pods/11234567-89ab-cdef-0123-456789abcdef/",
                    "generations/7/sessions/session%2Fwith%20spaces/",
                    "images/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/",
                    "services/sw-msvc-123/ports/8443/",
                    "resident-process-generations/4/",
                    "leases/31234567-89ab-cdef-0123-456789abcdef/",
                    "attempts/2/workers/41234567-89ab-cdef-0123-456789abcdef"
                );
                let expires_at = Utc::now() + Duration::minutes(4);
                let mut parsed_csr = CertificateSigningRequestParams::from_pem(csr).unwrap();
                parsed_csr.params.subject_alt_names = vec![
                    SanType::URI(rcgen::string::Ia5String::try_from(uri).unwrap()),
                    SanType::IpAddress("127.0.0.1".parse().unwrap()),
                ];
                parsed_csr.params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
                parsed_csr.params.not_after =
                    time::OffsetDateTime::from_unix_timestamp(expires_at.timestamp()).unwrap();
                let certificate = parsed_csr.signed_by(&issuer).unwrap();
                let response = serde_json::json!({
                    "certificate_pem": certificate.pem(),
                    "ca_certificate_pem": ca_pem,
                    "serial_number": "0123456789abcdef",
                    "expires_at": expires_at.to_rfc3339(),
                    "uri_san": uri
                })
                .to_string();
                let head = format!(
                    "HTTP/1.1 201 Created\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    response.len()
                );
                socket.write_all(head.as_bytes()).await.unwrap();
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        IdentityHarness {
            _directory: directory,
            token_file,
            ca_file,
            exchange_url: format!(
                "https://{address}/internal/v1/kubernetes-workload-certificates/exchange"
            )
            .parse()
            .unwrap(),
            workload_ca_pem,
            exact_client_identity_pem,
            wrong_uri_client_identity_pem,
            requests,
            task,
        }
    }

    #[tokio::test]
    async fn exchange_rereads_rotated_projected_token_and_never_persists_key_material() {
        let harness = start_identity_harness(2).await;
        let config = HostedRunnerWorkloadIdentityConfig {
            kubernetes_token_file: harness.token_file.clone(),
            identity_tls_ca_file: harness.ca_file.clone(),
            identity_exchange_url: harness.exchange_url.clone(),
            organization_id: "org-123".into(),
            workspace_id: "workspace-123".into(),
            sandbox_id: Uuid::parse_str("01234567-89ab-cdef-0123-456789abcdef").unwrap(),
            placement_generation: 7,
        };
        let exchanger =
            WorkloadIdentityExchanger::try_new(config, "session/with spaces".into()).unwrap();

        let first = exchanger.exchange_once(Utc::now()).await.unwrap();
        let rotated =
            projected_token(Uuid::parse_str("11234567-89ab-cdef-0123-456789abcdef").unwrap())
                .replace("\"warnafter\":1900000480", "\"warnafter\":1900000540");
        std::fs::write(&harness.token_file, &rotated).unwrap();
        let second = exchanger.exchange_once(Utc::now()).await.unwrap();
        harness.task.await.unwrap();

        assert_eq!(first.uri_san, second.uri_san);
        let requests = harness.requests.lock().unwrap();
        assert_eq!(
            requests[0]["projected_service_account_token"],
            projected_token(Uuid::parse_str("11234567-89ab-cdef-0123-456789abcdef").unwrap())
        );
        assert_eq!(requests[1]["projected_service_account_token"], rotated);
        assert_ne!(requests[0]["csr_pem"], requests[1]["csr_pem"]);
        let files = std::fs::read_dir(harness.token_file.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(files.len(), 2, "only projected token and public CA exist");
    }

    fn hosted_runner_client(workload_ca_pem: &str, identity_pem: Option<&str>) -> reqwest::Client {
        let mut builder = reqwest::Client::builder().add_root_certificate(
            reqwest::Certificate::from_pem(workload_ca_pem.as_bytes()).unwrap(),
        );
        if let Some(identity_pem) = identity_pem {
            builder =
                builder.identity(reqwest::Identity::from_pem(identity_pem.as_bytes()).unwrap());
        }
        builder.build().unwrap()
    }

    #[tokio::test]
    async fn hosted_runner_listener_requires_response_ca_and_exact_runner_host_uri() {
        let harness = start_identity_harness(1).await;
        let workspace = tempfile::tempdir().unwrap();
        let mut config =
            crate::hosted_runner::HostedRunnerConfig::new("session/with spaces", workspace.path())
                .unwrap()
                .with_bind_addr("127.0.0.1:0".parse().unwrap());
        config.workload_identity = Some(HostedRunnerWorkloadIdentityConfig {
            kubernetes_token_file: harness.token_file.clone(),
            identity_tls_ca_file: harness.ca_file.clone(),
            identity_exchange_url: harness.exchange_url.clone(),
            organization_id: "org-123".into(),
            workspace_id: "workspace-123".into(),
            sandbox_id: Uuid::parse_str("01234567-89ab-cdef-0123-456789abcdef").unwrap(),
            placement_generation: 7,
        });

        let handle = crate::hosted_runner::start_hosted_runner(config)
            .await
            .unwrap();
        let health_url = format!("{}/healthz", handle.base_url());

        assert!(
            hosted_runner_client(&harness.workload_ca_pem, None)
                .get(&health_url)
                .send()
                .await
                .is_err(),
            "a client certificate is mandatory"
        );
        assert!(
            hosted_runner_client(
                &harness.workload_ca_pem,
                Some(&harness.wrong_uri_client_identity_pem),
            )
            .get(&health_url)
            .send()
            .await
            .is_err(),
            "a trusted certificate with the wrong URI must fail"
        );
        assert_eq!(
            hosted_runner_client(
                &harness.workload_ca_pem,
                Some(&harness.exact_client_identity_pem),
            )
            .get(&health_url)
            .send()
            .await
            .unwrap()
            .status(),
            reqwest::StatusCode::OK
        );

        handle.shutdown().await;
        harness.task.await.unwrap();
    }

    #[tokio::test]
    async fn hosted_runner_retries_transient_identity_unavailability() {
        let harness = start_identity_harness_with_unavailable(3, 2).await;
        let workspace = tempfile::tempdir().unwrap();
        let mut config =
            crate::hosted_runner::HostedRunnerConfig::new("session/with spaces", workspace.path())
                .unwrap()
                .with_bind_addr("127.0.0.1:0".parse().unwrap());
        config.workload_identity = Some(HostedRunnerWorkloadIdentityConfig {
            kubernetes_token_file: harness.token_file.clone(),
            identity_tls_ca_file: harness.ca_file.clone(),
            identity_exchange_url: harness.exchange_url.clone(),
            organization_id: "org-123".into(),
            workspace_id: "workspace-123".into(),
            sandbox_id: Uuid::parse_str("01234567-89ab-cdef-0123-456789abcdef").unwrap(),
            placement_generation: 7,
        });

        let handle = crate::hosted_runner::start_hosted_runner(config)
            .await
            .expect("transient identity unavailability must recover during startup");

        handle.shutdown().await;
        harness.task.await.unwrap();
        assert_eq!(harness.requests.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn hosted_runner_retries_typed_transient_identity_conflict() {
        let harness = start_identity_harness_with_transient_conflicts(2, 1).await;
        let workspace = tempfile::tempdir().unwrap();
        let mut config =
            crate::hosted_runner::HostedRunnerConfig::new("session/with spaces", workspace.path())
                .unwrap()
                .with_bind_addr("127.0.0.1:0".parse().unwrap());
        config.workload_identity = Some(HostedRunnerWorkloadIdentityConfig {
            kubernetes_token_file: harness.token_file.clone(),
            identity_tls_ca_file: harness.ca_file.clone(),
            identity_exchange_url: harness.exchange_url.clone(),
            organization_id: "org-123".into(),
            workspace_id: "workspace-123".into(),
            sandbox_id: Uuid::parse_str("01234567-89ab-cdef-0123-456789abcdef").unwrap(),
            placement_generation: 7,
        });

        let handle = crate::hosted_runner::start_hosted_runner(config)
            .await
            .expect("typed transient identity conflict must recover during startup");

        handle.shutdown().await;
        harness.task.await.unwrap();
        assert_eq!(harness.requests.lock().unwrap().len(), 2);
    }

    fn server_identity_expiring_at(
        expires_at: chrono::DateTime<Utc>,
    ) -> super::IssuedServerIdentity {
        let pod_uid = Uuid::parse_str("11234567-89ab-cdef-0123-456789abcdef").unwrap();
        let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let csr = CertificateParams::default()
            .serialize_request(&key)
            .unwrap()
            .pem()
            .unwrap();
        let uri = concat!(
            "spiffe://identity.evalops.dev/maestro/v1/",
            "organizations/org-123/workspaces/workspace-123/",
            "sandboxes/01234567-89ab-cdef-0123-456789abcdef/",
            "pods/11234567-89ab-cdef-0123-456789abcdef/",
            "generations/7/sessions/session%2Fwith%20spaces/",
            "images/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/",
            "services/sw-msvc-123/ports/8443/",
            "resident-process-generations/4/",
            "leases/31234567-89ab-cdef-0123-456789abcdef/",
            "attempts/2/workers/41234567-89ab-cdef-0123-456789abcdef"
        );
        let response = signed_exchange_response(&csr, uri, expires_at);
        build_server_identity(
            response,
            key,
            &binding(),
            pod_uid,
            expires_at - Duration::minutes(4),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn reloadable_identity_cancels_old_connections_and_fails_closed_at_expiry() {
        let now = Utc::now();
        let state =
            ReloadableServerIdentity::new(server_identity_expiring_at(now + Duration::minutes(4)));
        let (_, first_connections) = state.snapshot(now).await.expect("initial identity");

        state
            .install(server_identity_expiring_at(now + Duration::minutes(5)))
            .await;

        assert!(first_connections.is_cancelled());
        let (_, second_connections) = state.snapshot(now).await.expect("rotated identity");
        assert!(!second_connections.is_cancelled());

        state.expire_if_due(now + Duration::minutes(5)).await;

        assert!(second_connections.is_cancelled());
        assert!(state.snapshot(now + Duration::minutes(5)).await.is_none());
    }
}
