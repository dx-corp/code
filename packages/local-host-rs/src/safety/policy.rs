//! Enterprise policy enforcement (parity with TS).
//!
//! Reads enterprise policy from the Maestro home directory, with legacy `.composer`
//! fallback, and enforces tool, path, network, model, and session limits. Policy
//! load failures fail closed (block) to match CLI behavior.

use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD as BASE64_STANDARD, URL_SAFE_NO_PAD},
};
use ring::signature::{ED25519, UnparsedPublicKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::io::Write as _;
use std::net::{IpAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(test)]
use std::cell::Cell;
#[cfg(test)]
use std::sync::{LazyLock, Mutex, MutexGuard};

use regex::Regex;
use url::Url;

use crate::path_utils::{env_path, legacy_composer_home_dir, maestro_home_dir};

use super::dangerous_patterns::check_dangerous_patterns;
use super::path_containment::{expand_tilde, is_tilde_path};

pub use maestro_runtime::ManagedPolicyMetadata;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct PolicyList {
    pub allowed: Option<Vec<String>>,
    pub blocked: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct NetworkPolicy {
    pub allowed_hosts: Option<Vec<String>>,
    pub blocked_hosts: Option<Vec<String>>,
    pub block_localhost: Option<bool>,
    #[serde(alias = "blockPrivateIPs")]
    pub block_private_ips: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct LimitsPolicy {
    pub max_tokens_per_session: Option<u64>,
    pub max_session_duration_minutes: Option<u64>,
    pub max_concurrent_sessions: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EnterprisePolicy {
    #[allow(dead_code)]
    pub org_id: Option<String>,
    pub tools: Option<PolicyList>,
    pub dependencies: Option<PolicyList>,
    pub models: Option<PolicyList>,
    pub paths: Option<PolicyList>,
    pub network: Option<NetworkPolicy>,
    pub limits: Option<LimitsPolicy>,
}

/// A v1 organization-managed policy bundle.
///
/// The signature covers every field except policyHash and signature, in the
/// exact JSON order represented by ManagedPolicyPayload. The hash is the
/// SHA-256 digest of those signed bytes, encoded as lowercase hexadecimal.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ManagedPolicyEnvelope {
    pub schema_version: u32,
    pub org_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    pub policy_version: u64,
    pub issued_at: u64,
    pub expires_at: u64,
    pub key_id: String,
    pub policy: EnterprisePolicy,
    pub kill_switch: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kill_switch_reason: Option<String>,
    pub policy_hash: String,
    pub signature: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ManagedPolicyPayload<'a> {
    schema_version: u32,
    org_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace_id: &'a Option<String>,
    policy_version: u64,
    issued_at: u64,
    expires_at: u64,
    key_id: &'a str,
    policy: &'a EnterprisePolicy,
    kill_switch: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    kill_switch_reason: &'a Option<String>,
}

/// Current managed-policy health, safe to return to an authenticated operator.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ManagedPolicyStatus {
    pub configured: bool,
    pub valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<ManagedPolicyMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ManagedPolicyPublishResult {
    pub published: bool,
    pub status: ManagedPolicyStatus,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ManagedPolicyAuditEvent {
    pub event_id: String,
    pub action: String,
    pub actor: Option<String>,
    pub recorded_at: u64,
    pub outcome: String,
    pub metadata: Option<ManagedPolicyMetadata>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone)]
struct VerifiedManagedPolicy {
    policy: EnterprisePolicy,
    metadata: ManagedPolicyMetadata,
    kill_switch_reason: Option<String>,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ManagedPolicyWatermark {
    policy_version: u64,
    policy_hash: String,
}

#[derive(Default)]
struct ManagedPolicyCache {
    path: Option<PathBuf>,
    policy: Option<VerifiedManagedPolicy>,
    mtime: Option<SystemTime>,
    accepted_version: Option<u64>,
    accepted_hash: Option<String>,
    trust_fingerprint: Option<String>,
    content_hash: Option<String>,
}

static MANAGED_POLICY_CACHE: std::sync::LazyLock<RwLock<ManagedPolicyCache>> =
    std::sync::LazyLock::new(|| RwLock::new(ManagedPolicyCache::default()));

const MANAGED_POLICY_SCHEMA_VERSION: u32 = 1;
const MANAGED_POLICY_CLOCK_SKEW: Duration = Duration::from_mins(5);

#[derive(Default)]
struct PolicyCache {
    policy: Option<EnterprisePolicy>,
    mtime: Option<SystemTime>,
}

static POLICY_CACHE: std::sync::LazyLock<RwLock<PolicyCache>> =
    std::sync::LazyLock::new(|| RwLock::new(PolicyCache::default()));

// Policy tests temporarily configure process-wide environment variables. Keep
// those mutations isolated from concurrent action-firewall reads while still
// allowing the configuring test to call back into policy code.
#[cfg(test)]
static TEST_ENV_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[cfg(test)]
thread_local! {
    static TEST_ENV_LOCK_DEPTH: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
struct TestEnvGuard {
    guard: Option<MutexGuard<'static, ()>>,
}

#[cfg(test)]
impl Drop for TestEnvGuard {
    fn drop(&mut self) {
        let depth = TEST_ENV_LOCK_DEPTH.with(|depth| {
            let next = depth.get().saturating_sub(1);
            depth.set(next);
            next
        });
        if depth == 0 {
            self.guard.take();
        }
    }
}

#[cfg(test)]
fn test_env_guard() -> TestEnvGuard {
    let reentrant = TEST_ENV_LOCK_DEPTH.with(|depth| {
        let current = depth.get();
        depth.set(current + 1);
        current > 0
    });
    let guard = (!reentrant).then(|| {
        TEST_ENV_MUTEX
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    });
    TestEnvGuard { guard }
}

static FILE_COMMAND_PATTERN: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?i)(?:cd|cat|rm|mv|cp|mkdir|touch|nano|vim|vi|less|more|head|tail|chmod|chown|strings|hexdump|dd|tee|ln|readlink|stat|file|wc|grep|sed|awk|sort|uniq|diff|patch|tar|gzip|gunzip|zip|unzip|find|rsync|scp)\s+((?:[^\s;&|<>`$()]|\\.)+(?:\s+(?:[^\s;&|<>`$()]|\\.)+)*)")
        .expect("Invalid file command regex")
});

static REDIRECT_PATTERN: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"[<>]{1,2}\s*([^\s<>|&;]+)").expect("Invalid redirect regex")
});

static COMMAND_SUB_PATTERN: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?:\$\(|<\()([^)]+)\)|`([^`]+)`").expect("Invalid command substitution regex")
});

static URL_PATTERN: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r#"https?://[^\s"'<>]+"#).expect("Invalid URL regex"));

static CURL_WGET_PATTERN: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?i)(?:curl|wget)\s+((?:[^\s;&|<>`$()]|\\.)+(?:\s+(?:[^\s;&|<>`$()]|\\.)+)*)")
        .expect("Invalid curl/wget regex")
});

static SHELL_META_PATTERN: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"[;&|`$()<>]").expect("Invalid shell meta regex"));

static PACKAGE_INSTALL_PATTERN: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:npm|yarn|pnpm|bun|pip|pip3|gem|cargo|go\s+get|composer)\s+(?:install|add|i\b)",
    )
    .expect("Invalid package install regex")
});

static NPM_INSTALL_PATTERN: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:npm|pnpm|yarn)\s+(?:install|i|add)\s+(?:--?[a-zA-Z-]+(?:=\S+)?\s+)*([\w@\-/.:\s]+)")
        .expect("Invalid npm install regex")
});

static BUN_INSTALL_PATTERN: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?i)\bbun\s+(?:add|install)\s+(?:--?[a-zA-Z-]+(?:=\S+)?\s+)*([\w@\-/.:\s]+)")
        .expect("Invalid bun install regex")
});

static PIP_INSTALL_PATTERN: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?i)\bpip\d*\s+install\s+(?:-[a-zA-Z-]+\s+)*([\w@\-/.:\s=<>]+)")
        .expect("Invalid pip install regex")
});

fn managed_policy_path() -> Option<PathBuf> {
    env_path("MAESTRO_MANAGED_POLICY_PATH")
}
fn managed_policy_state_path(policy_path: &Path) -> PathBuf {
    if let Some(path) = env_path("MAESTRO_MANAGED_POLICY_STATE_PATH") {
        return path;
    }
    let mut state = policy_path.as_os_str().to_os_string();
    state.push(".state");
    PathBuf::from(state)
}

fn managed_policy_trust_fingerprint(policy_path: &Path) -> String {
    let fingerprint = [
        std::env::var("MAESTRO_MANAGED_POLICY_PUBLIC_KEY").unwrap_or_default(),
        std::env::var("MAESTRO_MANAGED_POLICY_KEY_ID").unwrap_or_default(),
        std::env::var("MAESTRO_ORG_ID").unwrap_or_default(),
        std::env::var("MAESTRO_WORKSPACE_ID").unwrap_or_default(),
        managed_policy_state_path(policy_path).display().to_string(),
    ]
    .join("\u{0}");
    sha256_hex(fingerprint.as_bytes())
}
fn unix_now() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| "System clock is before the Unix epoch".to_string())
}

fn canonical_managed_policy_payload(envelope: &ManagedPolicyEnvelope) -> Vec<u8> {
    serde_json::to_vec(&ManagedPolicyPayload {
        schema_version: envelope.schema_version,
        org_id: &envelope.org_id,
        workspace_id: &envelope.workspace_id,
        policy_version: envelope.policy_version,
        issued_at: envelope.issued_at,
        expires_at: envelope.expires_at,
        key_id: &envelope.key_id,
        policy: &envelope.policy,
        kill_switch: envelope.kill_switch,
        kill_switch_reason: &envelope.kill_switch_reason,
    })
    .expect("managed policy payload is serializable")
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if value.is_empty() || !value.len().is_multiple_of(2) {
        return None;
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16)?;
            let low = (pair[1] as char).to_digit(16)?;
            Some(((high << 4) | low) as u8)
        })
        .collect()
}

fn decode_encoded_bytes(value: &str, expected_len: usize, label: &str) -> Result<Vec<u8>, String> {
    let value = value.trim();
    if let Some(decoded) = decode_hex(value) {
        if decoded.len() == expected_len {
            return Ok(decoded);
        }
    }

    let decoded = URL_SAFE_NO_PAD
        .decode(value)
        .or_else(|_| BASE64_STANDARD.decode(value))
        .map_err(|_| format!("Managed policy {label} is not valid base64 or hex"))?;
    if decoded.len() != expected_len {
        return Err(format!(
            "Managed policy {label} must decode to {expected_len} bytes"
        ));
    }
    Ok(decoded)
}

fn configured_managed_public_key() -> Result<Vec<u8>, String> {
    let value = std::env::var("MAESTRO_MANAGED_POLICY_PUBLIC_KEY")
        .map_err(|_| "Managed policy public key is not configured".to_string())?;
    decode_encoded_bytes(&value, 32, "public key")
}

fn verify_managed_policy(envelope: ManagedPolicyEnvelope) -> Result<VerifiedManagedPolicy, String> {
    if envelope.schema_version != MANAGED_POLICY_SCHEMA_VERSION {
        return Err(format!(
            "Unsupported managed policy schema version {}",
            envelope.schema_version
        ));
    }
    if envelope.org_id.trim().is_empty() || envelope.key_id.trim().is_empty() {
        return Err("Managed policy organization and key id are required".to_string());
    }
    if envelope.policy_version == 0 {
        return Err("Managed policy version must be positive".to_string());
    }
    if envelope.expires_at <= envelope.issued_at {
        return Err("Managed policy expiry must be after issuance".to_string());
    }

    let now = unix_now()?;
    if envelope.expires_at <= now {
        return Err("Managed policy has expired".to_string());
    }
    if envelope.issued_at > now.saturating_add(MANAGED_POLICY_CLOCK_SKEW.as_secs()) {
        return Err("Managed policy was issued too far in the future".to_string());
    }

    if let Ok(expected_org) = std::env::var("MAESTRO_ORG_ID") {
        let expected_org = expected_org.trim();
        if !expected_org.is_empty() && envelope.org_id != expected_org {
            return Err("Managed policy organization scope does not match this runner".to_string());
        }
    }
    if let Ok(expected_workspace) = std::env::var("MAESTRO_WORKSPACE_ID") {
        let expected_workspace = expected_workspace.trim();
        if !expected_workspace.is_empty()
            && envelope.workspace_id.as_deref() != Some(expected_workspace)
        {
            return Err("Managed policy workspace scope does not match this runner".to_string());
        }
    }
    if let Ok(expected_key_id) = std::env::var("MAESTRO_MANAGED_POLICY_KEY_ID") {
        let expected_key_id = expected_key_id.trim();
        if !expected_key_id.is_empty() && envelope.key_id != expected_key_id {
            return Err("Managed policy key id does not match the configured key".to_string());
        }
    }

    if let Some(policy_org) = envelope.policy.org_id.as_deref() {
        if policy_org != envelope.org_id {
            return Err("Managed policy body organization does not match its envelope".to_string());
        }
    }

    if envelope.kill_switch
        && envelope
            .kill_switch_reason
            .as_deref()
            .unwrap_or("")
            .trim()
            .is_empty()
    {
        return Err("Managed policy kill switch requires a reason".to_string());
    }

    let payload = canonical_managed_policy_payload(&envelope);
    let computed_hash = sha256_hex(&payload);
    if envelope.policy_hash.to_ascii_lowercase() != computed_hash {
        return Err("Managed policy hash does not match its signed payload".to_string());
    }

    let public_key = configured_managed_public_key()?;
    let signature = decode_encoded_bytes(&envelope.signature, 64, "signature")?;
    UnparsedPublicKey::new(&ED25519, public_key)
        .verify(&payload, &signature)
        .map_err(|_| "Managed policy signature verification failed".to_string())?;

    let metadata = ManagedPolicyMetadata {
        org_id: envelope.org_id,
        workspace_id: envelope.workspace_id,
        policy_version: envelope.policy_version,
        issued_at: envelope.issued_at,
        expires_at: envelope.expires_at,
        key_id: envelope.key_id,
        policy_hash: computed_hash,
        kill_switch: envelope.kill_switch,
    };

    Ok(VerifiedManagedPolicy {
        policy: envelope.policy,
        metadata,
        kill_switch_reason: envelope.kill_switch_reason,
    })
}

fn load_managed_policy_watermark(
    policy_path: &Path,
) -> Result<Option<ManagedPolicyWatermark>, String> {
    let state_path = managed_policy_state_path(policy_path);
    if !state_path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&state_path)
        .map_err(|_| "Managed policy rollback state could not be read".to_string())?;
    let watermark = serde_json::from_str::<ManagedPolicyWatermark>(&content)
        .map_err(|_| "Managed policy rollback state is not valid JSON".to_string())?;
    if watermark.policy_version == 0 || watermark.policy_hash.trim().is_empty() {
        return Err("Managed policy rollback state is invalid".to_string());
    }
    Ok(Some(watermark))
}
fn persist_managed_policy_watermark(
    policy_path: &Path,
    policy_version: u64,
    policy_hash: &str,
) -> Result<(), String> {
    let state_path = managed_policy_state_path(policy_path);
    if let Some(parent) = state_path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            format!("Managed policy rollback state directory could not be created: {error}")
        })?;
    }
    let serialized = serde_json::to_vec(&ManagedPolicyWatermark {
        policy_version,
        policy_hash: policy_hash.to_string(),
    })
    .map_err(|_| "Managed policy rollback state could not be serialized".to_string())?;
    let temp_path = state_path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temp_path, serialized)
        .map_err(|error| format!("Managed policy rollback state could not be written: {error}"))?;
    if let Err(error) = std::fs::rename(&temp_path, &state_path) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(format!(
            "Managed policy rollback state could not be committed: {error}"
        ));
    }
    Ok(())
}

fn managed_policy_audit_path(policy_path: &Path) -> PathBuf {
    if let Some(path) = env_path("MAESTRO_MANAGED_POLICY_AUDIT_PATH") {
        return path;
    }
    let mut audit = managed_policy_state_path(policy_path)
        .as_os_str()
        .to_os_string();
    audit.push(".audit.jsonl");
    PathBuf::from(audit)
}

fn bounded_audit_value(value: Option<String>) -> Option<String> {
    value.map(|value| value.chars().take(512).collect())
}

/// Atomically publish an externally signed managed-policy envelope.
///
/// Maestro verifies the envelope with its configured public key and never
/// handles the private key used by the customer KMS or HSM.
pub fn publish_managed_policy(
    envelope: ManagedPolicyEnvelope,
) -> Result<ManagedPolicyPublishResult, String> {
    let Some(path) = managed_policy_path() else {
        return Err("Managed policy publish requires MAESTRO_MANAGED_POLICY_PATH".to_string());
    };
    if path.exists() && !path.is_file() {
        return Err("Managed policy publish path is not a regular file".to_string());
    }

    let verified = verify_managed_policy(envelope.clone())?;
    if let Some(watermark) = load_managed_policy_watermark(&path)? {
        if verified.metadata.policy_version < watermark.policy_version {
            return Err("Managed policy rollback was rejected".to_string());
        }
        if verified.metadata.policy_version == watermark.policy_version
            && verified.metadata.policy_hash != watermark.policy_hash
        {
            return Err("Managed policy changed without increasing its version".to_string());
        }
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            format!("Managed policy publish directory could not be created: {error}")
        })?;
    }
    let serialized = serde_json::to_vec(&envelope)
        .map_err(|_| "Managed policy envelope could not be serialized".to_string())?;
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let temp_path = path.with_extension(format!("publish-tmp-{}-{unique}", std::process::id()));
    if let Err(error) = std::fs::write(&temp_path, serialized) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(format!(
            "Managed policy envelope could not be written: {error}"
        ));
    }
    // Advance the watermark before activation. If the process dies between
    // these operations, the older active file fails closed instead of being
    // accepted as a rollback on the next start.
    if let Err(error) = persist_managed_policy_watermark(
        &path,
        verified.metadata.policy_version,
        &verified.metadata.policy_hash,
    ) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&temp_path, &path) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(format!(
            "Managed policy envelope could not be activated: {error}"
        ));
    }

    Ok(ManagedPolicyPublishResult {
        published: true,
        status: refresh_managed_policy(),
    })
}

/// Append a bounded, non-secret managed-policy audit event.
pub fn record_managed_policy_audit(mut event: ManagedPolicyAuditEvent) -> Result<(), String> {
    let Some(policy_path) = managed_policy_path() else {
        return Err("Managed policy audit requires MAESTRO_MANAGED_POLICY_PATH".to_string());
    };
    event.event_id = event.event_id.chars().take(128).collect();
    event.action = event.action.chars().take(128).collect();
    event.actor = bounded_audit_value(event.actor);
    event.outcome = event.outcome.chars().take(128).collect();
    event.reason = bounded_audit_value(event.reason);

    let audit_path = managed_policy_audit_path(&policy_path);
    if let Some(parent) = audit_path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            format!("Managed policy audit directory could not be created: {error}")
        })?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&audit_path)
        .map_err(|error| format!("Managed policy audit could not be opened: {error}"))?;
    let serialized = serde_json::to_vec(&event)
        .map_err(|_| "Managed policy audit event could not be serialized".to_string())?;
    file.write_all(&serialized)
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("Managed policy audit could not be committed: {error}"))
}

/// Read the newest bounded managed-policy audit events.
pub fn managed_policy_audit(limit: usize) -> Result<Vec<ManagedPolicyAuditEvent>, String> {
    let Some(policy_path) = managed_policy_path() else {
        return Ok(Vec::new());
    };
    let audit_path = managed_policy_audit_path(&policy_path);
    if !audit_path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(&audit_path)
        .map_err(|error| format!("Managed policy audit could not be read: {error}"))?;
    let mut events = content
        .lines()
        .map(|line| {
            serde_json::from_str::<ManagedPolicyAuditEvent>(line)
                .map_err(|_| "Managed policy audit contains invalid JSON".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    events.reverse();
    events.truncate(limit.min(100));
    Ok(events)
}

fn load_managed_policy(force: bool) -> Result<Option<VerifiedManagedPolicy>, String> {
    #[cfg(test)]
    let _test_env = test_env_guard();

    let Some(path) = managed_policy_path() else {
        return Ok(None);
    };
    if !path.is_file() {
        return Err("Managed policy file is missing or is not a regular file".to_string());
    }

    let mtime = std::fs::metadata(&path)
        .map_err(|_| "Managed policy file metadata is unavailable".to_string())?
        .modified()
        .ok();
    let trust_fingerprint = managed_policy_trust_fingerprint(&path);
    let watermark = load_managed_policy_watermark(&path)?;
    let content = std::fs::read_to_string(&path)
        .map_err(|_| "Managed policy file could not be read".to_string())?;
    let content_hash = sha256_hex(content.as_bytes());

    if let Ok(cache) = MANAGED_POLICY_CACHE.read() {
        let same_path = cache.path.as_ref() == Some(&path);
        let same_trust = cache.trust_fingerprint.as_deref() == Some(&trust_fingerprint);
        let same_content = cache.content_hash.as_deref() == Some(&content_hash);
        let cache_not_expired = cache.policy.as_ref().is_some_and(|policy| {
            unix_now()
                .map(|now| policy.metadata.expires_at > now)
                .unwrap_or(false)
        });
        let cache_watermark_matches = watermark.as_ref().is_some_and(|watermark| {
            cache.policy.as_ref().is_some_and(|policy| {
                policy.metadata.policy_version == watermark.policy_version
                    && policy.metadata.policy_hash == watermark.policy_hash
            })
        });
        if !force
            && same_path
            && same_trust
            && same_content
            && cache.mtime == mtime
            && cache_not_expired
            && cache_watermark_matches
        {
            return cache
                .policy
                .clone()
                .ok_or_else(|| "Managed policy cache is empty".to_string())
                .map(Some);
        }
    }

    let envelope = serde_json::from_str::<ManagedPolicyEnvelope>(&content)
        .map_err(|_| "Managed policy file is not valid JSON".to_string())?;
    let verified = verify_managed_policy(envelope)?;

    if let Ok(cache) = MANAGED_POLICY_CACHE.read() {
        if cache.path.as_ref() == Some(&path) {
            if let Some(previous_version) = cache.accepted_version {
                if verified.metadata.policy_version < previous_version {
                    return Err("Managed policy rollback was rejected".to_string());
                }
                if verified.metadata.policy_version == previous_version
                    && cache.accepted_hash.as_deref() != Some(&verified.metadata.policy_hash)
                {
                    return Err("Managed policy changed without increasing its version".to_string());
                }
            }
        }
    }

    if let Some(watermark) = &watermark {
        if verified.metadata.policy_version < watermark.policy_version {
            return Err("Managed policy rollback was rejected".to_string());
        }
        if verified.metadata.policy_version == watermark.policy_version
            && verified.metadata.policy_hash != watermark.policy_hash
        {
            return Err("Managed policy changed without increasing its version".to_string());
        }
    }

    let should_persist = watermark
        .as_ref()
        .map(|watermark| verified.metadata.policy_version > watermark.policy_version)
        .unwrap_or(true);
    if should_persist {
        persist_managed_policy_watermark(
            &path,
            verified.metadata.policy_version,
            &verified.metadata.policy_hash,
        )?;
    }

    if verified.metadata.kill_switch {
        return Err(format!(
            "Managed policy kill switch is active: {}",
            verified
                .kill_switch_reason
                .as_deref()
                .unwrap_or("emergency revocation")
        ));
    }

    if let Ok(mut cache) = MANAGED_POLICY_CACHE.write() {
        cache.path = Some(path);
        cache.mtime = mtime;
        cache.accepted_version = Some(verified.metadata.policy_version);
        cache.accepted_hash = Some(verified.metadata.policy_hash.clone());
        cache.trust_fingerprint = Some(trust_fingerprint);
        cache.content_hash = Some(content_hash);
        cache.policy = Some(verified.clone());
    }

    Ok(Some(verified))
}

fn managed_policy_status_with_force(force: bool) -> ManagedPolicyStatus {
    #[cfg(test)]
    let _test_env = test_env_guard();

    if managed_policy_path().is_none() {
        return ManagedPolicyStatus {
            configured: false,
            valid: false,
            metadata: None,
            error: None,
        };
    }

    match load_managed_policy(force) {
        Ok(Some(policy)) => ManagedPolicyStatus {
            configured: true,
            valid: true,
            metadata: Some(policy.metadata),
            error: None,
        },
        Ok(None) => ManagedPolicyStatus {
            configured: true,
            valid: false,
            metadata: None,
            error: Some("Managed policy is not configured".to_string()),
        },
        Err(error) => ManagedPolicyStatus {
            configured: true,
            valid: false,
            metadata: None,
            error: Some(error),
        },
    }
}

/// Return safe status for the active managed-policy bundle.
pub fn managed_policy_status() -> ManagedPolicyStatus {
    managed_policy_status_with_force(false)
}

/// Re-read and verify the managed-policy bundle.
pub fn refresh_managed_policy() -> ManagedPolicyStatus {
    managed_policy_status_with_force(true)
}

/// Return the verified policy identity for audit receipts.
pub fn managed_policy_metadata() -> Option<ManagedPolicyMetadata> {
    load_managed_policy(false)
        .ok()
        .flatten()
        .map(|policy| policy.metadata)
}

/// Return a fail-closed error when managed mode is configured but invalid.
pub fn managed_policy_gate_error() -> Option<String> {
    load_managed_policy(false)
        .err()
        .map(|error| format!("Managed policy error: {error}. Access blocked."))
}

fn policy_path() -> Option<PathBuf> {
    select_policy_path(policy_path_candidates())
}

#[derive(Clone)]
struct PolicyPathCandidate {
    path: PathBuf,
    explicit: bool,
}

fn select_policy_path(candidates: Vec<PolicyPathCandidate>) -> Option<PathBuf> {
    if let Some(path) = candidates
        .iter()
        .find(|candidate| candidate.path.is_file())
        .map(|candidate| candidate.path.clone())
    {
        return Some(path);
    }
    if let Some(path) = candidates
        .iter()
        .find(|candidate| candidate.explicit && candidate.path.exists())
        .map(|candidate| candidate.path.clone())
    {
        return Some(path);
    }
    candidates
        .into_iter()
        .find(|candidate| !candidate.path.exists())
        .map(|candidate| candidate.path)
}

fn push_policy_path_candidate(
    candidates: &mut Vec<PolicyPathCandidate>,
    path: PathBuf,
    explicit: bool,
) {
    if let Some(candidate) = candidates
        .iter_mut()
        .find(|candidate| candidate.path == path)
    {
        candidate.explicit = candidate.explicit || explicit;
    } else {
        candidates.push(PolicyPathCandidate { path, explicit });
    }
}

fn policy_path_candidates() -> Vec<PolicyPathCandidate> {
    let mut candidates = Vec::new();
    if let Some(path) = env_path("MAESTRO_ENTERPRISE_POLICY_PATH") {
        push_policy_path_candidate(&mut candidates, path, true);
    }
    if let Some(path) = env_path("MAESTRO_POLICY_PATH") {
        push_policy_path_candidate(&mut candidates, path, true);
    }
    if let Some(maestro_home) = maestro_home_dir() {
        push_policy_path_candidate(&mut candidates, maestro_home.join("policy.json"), false);
    }
    if let Some(composer_home) = legacy_composer_home_dir() {
        push_policy_path_candidate(&mut candidates, composer_home.join("policy.json"), false);
    }
    candidates
}

fn load_local_policy(force: bool) -> Result<Option<EnterprisePolicy>, String> {
    let Some(path) = policy_path() else {
        return Ok(None);
    };

    if !path.exists() {
        if let Ok(mut cache) = POLICY_CACHE.write() {
            cache.policy = None;
            cache.mtime = None;
        }
        return Ok(None);
    }

    let metadata = std::fs::metadata(&path)
        .map_err(|e| format!("Failed to stat policy file {}: {e}", path.display()))?;
    let mtime = metadata.modified().ok();

    if let Ok(cache) = POLICY_CACHE.read() {
        if !force && cache.mtime.is_some() && cache.mtime == mtime {
            return Ok(cache.policy.clone());
        }
    }

    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read policy file {}: {e}", path.display()))?;

    let policy = serde_json::from_str::<EnterprisePolicy>(&content)
        .map_err(|e| format!("Failed to parse enterprise policy: {e}"))?;

    if let Ok(mut cache) = POLICY_CACHE.write() {
        cache.policy = Some(policy.clone());
        cache.mtime = mtime;
    }

    Ok(Some(policy))
}

fn narrow_allowed_values(
    managed: Option<Vec<String>>,
    local: Option<Vec<String>>,
) -> Option<Vec<String>> {
    match (managed, local) {
        (None, None) => None,
        (Some(values), None) | (None, Some(values)) => Some(values),
        (Some(managed), Some(local)) => Some(
            managed
                .into_iter()
                .filter(|value| local.iter().any(|candidate| candidate == value))
                .collect(),
        ),
    }
}

fn union_values(managed: Option<Vec<String>>, local: Option<Vec<String>>) -> Option<Vec<String>> {
    match (managed, local) {
        (None, None) => None,
        (Some(values), None) | (None, Some(values)) => Some(values),
        (Some(mut managed), Some(local)) => {
            for value in local {
                if !managed.iter().any(|candidate| candidate == &value) {
                    managed.push(value);
                }
            }
            Some(managed)
        }
    }
}

fn narrow_policy_list(
    managed: Option<PolicyList>,
    local: Option<PolicyList>,
) -> Option<PolicyList> {
    match (managed, local) {
        (None, None) => None,
        (Some(policy), None) | (None, Some(policy)) => Some(policy),
        (Some(managed), Some(local)) => Some(PolicyList {
            allowed: narrow_allowed_values(managed.allowed, local.allowed),
            blocked: union_values(managed.blocked, local.blocked),
        }),
    }
}

fn narrow_bool(managed: Option<bool>, local: Option<bool>) -> Option<bool> {
    match (managed, local) {
        (None, None) => None,
        (managed, local) => Some(managed.unwrap_or(false) || local.unwrap_or(false)),
    }
}

fn narrow_network(
    managed: Option<NetworkPolicy>,
    local: Option<NetworkPolicy>,
) -> Option<NetworkPolicy> {
    match (managed, local) {
        (None, None) => None,
        (Some(policy), None) | (None, Some(policy)) => Some(policy),
        (Some(managed), Some(local)) => Some(NetworkPolicy {
            allowed_hosts: narrow_allowed_values(managed.allowed_hosts, local.allowed_hosts),
            blocked_hosts: union_values(managed.blocked_hosts, local.blocked_hosts),
            block_localhost: narrow_bool(managed.block_localhost, local.block_localhost),
            block_private_ips: narrow_bool(managed.block_private_ips, local.block_private_ips),
        }),
    }
}

fn narrow_limit(managed: Option<u64>, local: Option<u64>) -> Option<u64> {
    match (managed, local) {
        (None, None) => None,
        (Some(value), None) | (None, Some(value)) => Some(value),
        (Some(0), Some(value)) | (Some(value), Some(0)) => Some(value),
        (Some(managed), Some(local)) => Some(managed.min(local)),
    }
}

fn narrow_limits(
    managed: Option<LimitsPolicy>,
    local: Option<LimitsPolicy>,
) -> Option<LimitsPolicy> {
    match (managed, local) {
        (None, None) => None,
        (Some(policy), None) | (None, Some(policy)) => Some(policy),
        (Some(managed), Some(local)) => Some(LimitsPolicy {
            max_tokens_per_session: narrow_limit(
                managed.max_tokens_per_session,
                local.max_tokens_per_session,
            ),
            max_session_duration_minutes: narrow_limit(
                managed.max_session_duration_minutes,
                local.max_session_duration_minutes,
            ),
            max_concurrent_sessions: narrow_limit(
                managed.max_concurrent_sessions,
                local.max_concurrent_sessions,
            ),
        }),
    }
}

fn narrow_policy(
    mut managed: EnterprisePolicy,
    local: Option<EnterprisePolicy>,
) -> EnterprisePolicy {
    let Some(local) = local else {
        return managed;
    };

    managed.tools = narrow_policy_list(managed.tools, local.tools);
    managed.dependencies = narrow_policy_list(managed.dependencies, local.dependencies);
    managed.models = narrow_policy_list(managed.models, local.models);
    managed.paths = narrow_policy_list(managed.paths, local.paths);
    managed.network = narrow_network(managed.network, local.network);
    managed.limits = narrow_limits(managed.limits, local.limits);
    managed
}

fn load_policy(force: bool) -> Result<Option<EnterprisePolicy>, String> {
    #[cfg(test)]
    let _test_env = test_env_guard();

    let managed = load_managed_policy(force)?;
    let local = load_local_policy(force)?;

    match managed {
        Some(managed) => Ok(Some(narrow_policy(managed.policy, local))),
        None => Ok(local),
    }
}

fn expand_home_dir(path: &str) -> String {
    let raw = Path::new(path);
    if is_tilde_path(raw) {
        if let Some(expanded) = expand_tilde(raw) {
            return expanded.to_string_lossy().to_string();
        }
    }
    path.to_string()
}

fn resolve_absolute_path(path: &str) -> PathBuf {
    let expanded = expand_home_dir(path);
    let path = Path::new(&expanded);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn resolve_real_path(path: &Path) -> PathBuf {
    if let Ok(real) = dunce::canonicalize(path) {
        return real;
    }
    if let Some(parent) = path.parent() {
        if let Ok(real_parent) = dunce::canonicalize(parent) {
            if let Some(name) = path.file_name() {
                return real_parent.join(name);
            }
        }
    }
    path.to_path_buf()
}

fn normalize_for_match(path: &Path) -> String {
    let value = path.to_string_lossy().replace('\\', "/");
    #[cfg(windows)]
    let value = value.to_lowercase();
    value
}

fn contains_glob(pattern: &str) -> bool {
    pattern.contains('*') || pattern.contains('?') || pattern.contains('[') || pattern.contains('{')
}

fn matches_path_pattern(path: &Path, patterns: &[String]) -> bool {
    let normalized_path = resolve_absolute_path(&path.to_string_lossy());
    let real_path = resolve_real_path(&normalized_path);

    for pattern in patterns {
        let expanded = expand_home_dir(pattern);
        let is_glob = contains_glob(&expanded);
        let resolved_pattern = if is_glob {
            PathBuf::from(expanded)
        } else {
            resolve_absolute_path(&expanded)
        };

        for candidate in [&normalized_path, &real_path] {
            let path_str = normalize_for_match(candidate);
            let pattern_str = normalize_for_match(&resolved_pattern);
            if let Ok(glob_pat) = glob::Pattern::new(&pattern_str) {
                if glob_pat.matches(&path_str) {
                    return true;
                }
            }

            if !is_glob
                && (path_str == pattern_str || path_str.starts_with(&format!("{pattern_str}/")))
            {
                return true;
            }
        }
    }

    false
}

fn matches_model_pattern(model_id: &str, patterns: &[String]) -> bool {
    let model = model_id.to_lowercase();
    for pattern in patterns {
        let pat = pattern.to_lowercase();
        if let Ok(glob_pat) = glob::Pattern::new(&pat) {
            if glob_pat.matches(&model) {
                return true;
            }
        }
    }
    false
}

fn host_matches(host: &str, pattern: &str) -> bool {
    let host = host.to_lowercase();
    let pattern = pattern.to_lowercase();
    host == pattern || host.ends_with(&format!(".{pattern}"))
}

fn is_localhost_alias(host: &str) -> bool {
    let host = host.to_lowercase();
    host == "localhost" || host == "127.0.0.1" || host == "::1" || host.ends_with(".localhost")
}

fn check_network_restrictions(url: &str, network: &NetworkPolicy) -> Option<String> {
    let parsed = match Url::parse(url.trim()) {
        Ok(parsed) => parsed,
        Err(_) => {
            return Some(
                "Invalid URL format - cannot validate against network policy.".to_string(),
            );
        }
    };
    let host = match parsed.host_str() {
        Some(host) => host,
        None => {
            return Some(
                "Invalid URL format - cannot validate against network policy.".to_string(),
            );
        }
    };
    let host = host.trim_matches(['[', ']']);

    if let Some(blocked) = &network.blocked_hosts {
        if blocked.iter().any(|pattern| host_matches(host, pattern)) {
            return Some(format!("Host \"{host}\" is blocked by enterprise policy."));
        }
    }

    if let Some(allowed) = &network.allowed_hosts {
        if allowed.is_empty() {
            return Some(format!("Host \"{host}\" is not in the allowed hosts list."));
        }
        let ok = allowed.iter().any(|pattern| host_matches(host, pattern));
        if !ok {
            return Some(format!("Host \"{host}\" is not in the allowed hosts list."));
        }
    }

    let mut resolved_ips: Vec<IpAddr> = Vec::new();
    let is_ip = host.parse::<IpAddr>().is_ok();

    if is_ip {
        if let Ok(ip) = host.parse::<IpAddr>() {
            resolved_ips.push(ip);
        }
    } else if network.block_private_ips.unwrap_or(false) || network.block_localhost.unwrap_or(false)
    {
        let port = parsed.port_or_known_default().unwrap_or(80);
        let host_port = format!("{host}:{port}");
        if let Ok(addrs) = host_port.to_socket_addrs() {
            for addr in addrs {
                resolved_ips.push(addr.ip());
            }
        }
        if resolved_ips.is_empty() {
            return Some(format!(
                "DNS resolution failed for \"{host}\" and network policy requires IP validation (blockPrivateIPs/blockLocalhost enabled). Access blocked."
            ));
        }
    }

    if network.block_localhost.unwrap_or(false)
        && (is_localhost_alias(host) || resolved_ips.iter().any(|ip| ip.is_loopback()))
    {
        return Some("Access to localhost is blocked by enterprise policy.".to_string());
    }

    if network.block_private_ips.unwrap_or(false) && resolved_ips.iter().any(is_private_ip) {
        return Some("Access to private IP addresses is blocked by enterprise policy.".to_string());
    }

    None
}

fn clean_package_spec(spec: &str) -> String {
    if spec.contains("://")
        || spec.starts_with("git@")
        || spec.starts_with("./")
        || spec.starts_with("../")
    {
        return spec.to_string();
    }

    if let Some(rest) = spec.strip_prefix('@') {
        if let Some(idx) = rest.find('@') {
            return format!("@{}", &rest[..idx]);
        }
        return format!("@{rest}");
    }

    spec.split(['@', '=', '<', '>'])
        .next()
        .unwrap_or(spec)
        .to_string()
}

fn extract_dependencies(command: &str) -> Vec<String> {
    let mut results = Vec::new();
    let patterns = [
        &*NPM_INSTALL_PATTERN,
        &*BUN_INSTALL_PATTERN,
        &*PIP_INSTALL_PATTERN,
    ];

    for pattern in patterns {
        for caps in pattern.captures_iter(command) {
            let captured = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            if captured.is_empty() {
                continue;
            }
            for part in captured.split_whitespace() {
                if part.starts_with('-') {
                    continue;
                }
                let cleaned = clean_package_spec(part);
                if !cleaned.is_empty() {
                    results.push(cleaned);
                }
            }
        }
    }

    results
}

fn has_package_install(command: &str) -> bool {
    PACKAGE_INSTALL_PATTERN.is_match(command)
}

fn extract_urls_from_value(value: &serde_json::Value, urls: &mut Vec<String>) {
    match value {
        serde_json::Value::String(text) => {
            for m in URL_PATTERN.find_iter(text) {
                let trimmed = m
                    .as_str()
                    .trim_end_matches(&[')', '}', ']', ',', '.', ';', ':'][..]);
                if !trimmed.is_empty() {
                    urls.push(trimmed.to_string());
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                extract_urls_from_value(item, urls);
            }
        }
        serde_json::Value::Object(map) => {
            for value in map.values() {
                extract_urls_from_value(value, urls);
            }
        }
        _ => {}
    }
}

fn extract_urls_from_shell_command(command: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let flags_with_values = [
        "-X",
        "--request",
        "-o",
        "-O",
        "--output",
        "-H",
        "--header",
        "-d",
        "--data",
        "--data-raw",
        "--data-binary",
        "--data-urlencode",
        "-F",
        "--form",
        "-A",
        "--user-agent",
        "-u",
        "--user",
        "-T",
        "--upload-file",
        "-e",
        "--referer",
        "-b",
        "--cookie",
        "-c",
        "--cookie-jar",
        "-K",
        "--config",
        "--resolve",
        "--connect-to",
        "--max-time",
        "-m",
        "--retry",
        "--retry-delay",
        "-w",
        "--write-out",
    ];

    for caps in CURL_WGET_PATTERN.captures_iter(command) {
        let args_str = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let parts = shlex::split(args_str)
            .unwrap_or_else(|| args_str.split_whitespace().map(|s| s.to_string()).collect());
        let mut skip_next = false;
        for part in parts {
            let stripped = part.trim_matches(['"', '\'']);
            if skip_next {
                skip_next = false;
                continue;
            }
            if stripped.starts_with('-') {
                if stripped.contains('=') {
                    continue;
                }
                if flags_with_values.contains(&stripped) {
                    skip_next = true;
                }
                continue;
            }
            let mut url = stripped.to_string();
            if !url.starts_with("http://") && !url.starts_with("https://") {
                url = format!("http://{url}");
            }
            let cleaned = url.trim_end_matches(&[')', '}', ']', ',', '.', ';', ':'][..]);
            if !cleaned.is_empty() {
                urls.push(cleaned.to_string());
            }
        }
    }

    urls
}

fn extract_file_paths(tool_name: &str, args: &serde_json::Value) -> Vec<String> {
    let mut paths = Vec::new();
    let Some(map) = args.as_object() else {
        return paths;
    };

    let path_keys = [
        "path",
        "file_path",
        "filePath",
        "file",
        "files",
        "directory",
        "dir",
        "target",
        "source",
        "destination",
        "cwd",
        "output",
        "input",
        "src",
        "dest",
        "config",
        "workspace",
        "folder",
        "target_file",
        "target_directory",
    ];

    for key in path_keys {
        if let Some(value) = map.get(key) {
            match value {
                serde_json::Value::String(text) if !text.is_empty() => {
                    paths.push(text.clone());
                }
                serde_json::Value::Array(items) => {
                    for item in items {
                        if let Some(text) = item.as_str() {
                            if !text.is_empty() {
                                paths.push(text.to_string());
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    if matches!(tool_name, "bash" | "background_tasks") {
        if let Some(command) = map.get("command").and_then(|v| v.as_str()) {
            extract_paths_from_command(command, &mut paths, 0);
        }
    }

    paths
}

fn extract_paths_from_command(command: &str, paths: &mut Vec<String>, depth: usize) {
    if depth > 1 {
        return;
    }

    for caps in FILE_COMMAND_PATTERN.captures_iter(command) {
        let args_str = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let parts = shlex::split(args_str)
            .unwrap_or_else(|| args_str.split_whitespace().map(|s| s.to_string()).collect());
        for part in parts {
            let cleaned = part.trim_matches(['"', '\'']);
            if cleaned.is_empty() || cleaned.starts_with('-') {
                continue;
            }
            if matches!(cleaned.chars().next(), Some('<' | '>' | '|' | '&' | ';')) {
                continue;
            }
            paths.push(cleaned.to_string());
        }
    }

    for caps in REDIRECT_PATTERN.captures_iter(command) {
        if let Some(raw) = caps.get(1).map(|m| m.as_str()) {
            let cleaned = raw.trim_matches(['"', '\'']);
            if !cleaned.is_empty() {
                paths.push(cleaned.to_string());
            }
        }
    }

    for caps in COMMAND_SUB_PATTERN.captures_iter(command) {
        let inner = caps.get(1).or_else(|| caps.get(2)).map(|m| m.as_str());
        if let Some(inner_cmd) = inner {
            extract_paths_from_command(inner_cmd, paths, depth + 1);
        }
    }
}

fn check_paths_against_policy(paths: &[String], policy: &EnterprisePolicy) -> Option<String> {
    let path_policy = policy.paths.as_ref()?;

    for path in paths {
        let path_buf = Path::new(path);
        if let Some(blocked) = &path_policy.blocked {
            if !blocked.is_empty() && matches_path_pattern(path_buf, blocked) {
                return Some(format!("Path \"{path}\" is blocked by enterprise policy."));
            }
        }
        if let Some(allowed) = &path_policy.allowed {
            if allowed.is_empty() || !matches_path_pattern(path_buf, allowed) {
                return Some(format!("Path \"{path}\" is not in the allowed paths list."));
            }
        }
    }

    None
}

fn check_dependencies_against_policy(command: &str, policy: &EnterprisePolicy) -> Option<String> {
    let dep_policy = policy.dependencies.as_ref()?;

    let deps = extract_dependencies(command);
    let is_install = has_package_install(command);

    if (is_install || !deps.is_empty()) && SHELL_META_PATTERN.is_match(command) {
        return Some("Command contains shell metacharacters which are not allowed by enterprise policy during package installation.".to_string());
    }

    if let Some(allowed) = &dep_policy.allowed {
        for dep in &deps {
            if allowed.is_empty() || !allowed.iter().any(|d| d == dep) {
                return Some(format!(
                    "Dependency \"{dep}\" is not in the approved dependencies list."
                ));
            }
        }
    }

    if let Some(blocked) = &dep_policy.blocked {
        for dep in &deps {
            if blocked.iter().any(|d| d == dep) {
                return Some(format!(
                    "Dependency \"{dep}\" is explicitly blocked by enterprise policy."
                ));
            }
        }
    }

    None
}

fn check_obfuscation_patterns(command: &str) -> Option<String> {
    let patterns = check_dangerous_patterns(command);
    let blocked_ids = [
        "base64_decode",
        "openssl_enc",
        "python_eval",
        "perl_eval",
        "node_eval",
        "php_eval",
        "ruby_eval",
        "eval_call",
        "exec_call",
    ];

    if patterns.iter().any(|p| blocked_ids.contains(&p.pattern_id)) {
        return Some("Command contains obfuscated or dangerous patterns (e.g. base64 decoding, inline code execution) which are blocked by enterprise policy.".to_string());
    }

    None
}

fn check_network_against_policy(
    args: &serde_json::Value,
    command: Option<&str>,
    policy: &EnterprisePolicy,
) -> Option<String> {
    let network = policy.network.as_ref()?;

    let mut urls = Vec::new();
    extract_urls_from_value(args, &mut urls);
    if let Some(cmd) = command {
        urls.extend(extract_urls_from_shell_command(cmd));
    }

    for url in urls {
        if let Some(reason) = check_network_restrictions(&url, network) {
            return Some(reason);
        }
    }

    None
}

pub fn check_tool_allowed(tool_name: &str) -> Option<String> {
    let policy = match load_policy(false) {
        Ok(policy) => policy,
        Err(err) => {
            return Some(format!("Enterprise policy error: {err}. Access blocked."));
        }
    }?;

    let tools = policy.tools.as_ref()?;

    if let Some(allowed) = &tools.allowed {
        if allowed.is_empty() || !allowed.iter().any(|t| t == tool_name) {
            return Some(format!(
                "Tool \"{tool_name}\" is not in the approved tools list."
            ));
        }
    }

    if let Some(blocked) = &tools.blocked {
        if blocked.iter().any(|t| t == tool_name) {
            return Some(format!(
                "Tool \"{tool_name}\" is explicitly blocked by enterprise policy."
            ));
        }
    }

    None
}

pub fn check_command_policy(tool_name: &str, args: &serde_json::Value) -> Option<String> {
    if !matches!(tool_name, "bash" | "background_tasks") {
        return None;
    }

    let policy = match load_policy(false) {
        Ok(policy) => policy,
        Err(err) => {
            return Some(format!("Enterprise policy error: {err}. Access blocked."));
        }
    }?;

    let command = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
    if command.is_empty() {
        return None;
    }

    if let Some(reason) = check_obfuscation_patterns(command) {
        return Some(reason);
    }

    if let Some(reason) = check_dependencies_against_policy(command, &policy) {
        return Some(reason);
    }

    let paths = extract_file_paths(tool_name, args);
    if let Some(reason) = check_paths_against_policy(&paths, &policy) {
        return Some(reason);
    }

    if let Some(reason) = check_network_against_policy(args, Some(command), &policy) {
        return Some(reason);
    }

    None
}

pub fn check_path_allowed(path: &Path) -> Option<String> {
    let policy = match load_policy(false) {
        Ok(policy) => policy,
        Err(err) => {
            return Some(format!("Enterprise policy error: {err}. Access blocked."));
        }
    }?;

    check_paths_against_policy(&[path.to_string_lossy().to_string()], &policy)
}

pub fn check_url_allowed(url: &str) -> Option<String> {
    let policy = match load_policy(false) {
        Ok(policy) => policy,
        Err(err) => {
            return Some(format!("Enterprise policy error: {err}. Access blocked."));
        }
    }?;

    let network = policy.network.as_ref()?;
    check_network_restrictions(url, network)
}

pub fn check_model_allowed(model_id: &str) -> Option<String> {
    if let Some(reason) = crate::stealth_models::check_model_allowed(model_id) {
        return Some(reason);
    }
    let policy = match load_policy(false) {
        Ok(policy) => policy,
        Err(err) => {
            return Some(format!("Enterprise policy error: {err}. Model blocked."));
        }
    }?;

    let models = policy.models.as_ref()?;

    if let Some(blocked) = &models.blocked {
        if !blocked.is_empty() && matches_model_pattern(model_id, blocked) {
            return Some(format!(
                "Model \"{model_id}\" is blocked by enterprise policy."
            ));
        }
    }

    if let Some(allowed) = &models.allowed {
        if allowed.is_empty() || !matches_model_pattern(model_id, allowed) {
            return Some(format!(
                "Model \"{model_id}\" is not in the approved models list."
            ));
        }
    }

    None
}

pub fn check_session_limits(
    started_at: SystemTime,
    token_count: Option<u64>,
    active_session_count: Option<usize>,
) -> Option<String> {
    let policy = match load_policy(false) {
        Ok(policy) => policy,
        Err(err) => {
            return Some(format!("Enterprise policy error: {err}. Access blocked."));
        }
    }?;

    let limits = policy.limits.as_ref()?;

    if let Some(max_minutes) = limits.max_session_duration_minutes {
        if max_minutes > 0 {
            let duration = SystemTime::now()
                .duration_since(started_at)
                .unwrap_or_default();
            let elapsed = duration.as_secs_f64() / 60.0;
            if elapsed > max_minutes as f64 {
                return Some(format!(
                    "Session duration limit exceeded ({} / {} minutes). Please start a new session.",
                    elapsed.floor(),
                    max_minutes
                ));
            }
        }
    }

    if let Some(max_tokens) = limits.max_tokens_per_session {
        if max_tokens > 0 {
            if let Some(tokens) = token_count {
                if tokens > max_tokens {
                    return Some(format!(
                        "Session token limit exceeded ({tokens}/{max_tokens} tokens). Please start a new session."
                    ));
                }
            } else {
                return Some(format!(
                    "Session token limit is active ({max_tokens}) but token usage data is unavailable. Access blocked for safety."
                ));
            }
        }
    }

    if let Some(max_sessions) = limits.max_concurrent_sessions {
        if max_sessions > 0 {
            if let Some(active) = active_session_count {
                if active as u64 > max_sessions {
                    return Some(format!(
                        "Concurrent session limit exceeded ({active}/{max_sessions}). Please close existing sessions before starting a new one."
                    ));
                }
            } else {
                return Some(format!(
                    "Concurrent session limit is active ({max_sessions}) but session count data is unavailable. Access blocked for safety."
                ));
            }
        }
    }

    None
}

pub fn get_policy_limits() -> Option<LimitsPolicy> {
    load_policy(false).ok().flatten().and_then(|p| p.limits)
}

#[allow(dead_code)]
pub fn policy_file_path() -> Option<PathBuf> {
    #[cfg(test)]
    let _test_env = test_env_guard();

    policy_path()
}

fn is_private_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            octets[0] == 10
                || (octets[0] == 172 && (16..=31).contains(&octets[1]))
                || (octets[0] == 192 && octets[1] == 168)
                || (octets[0] == 127)
                || (octets[0] == 169 && octets[1] == 254)
        }
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unique_local() || v6.is_unicast_link_local(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::KeyPair;

    fn restore_env_var(name: &str, previous: Option<String>) {
        if let Some(value) = previous {
            std::env::set_var(name, value);
        } else {
            std::env::remove_var(name);
        }
    }

    // ========================================================================
    // Private IP Detection Tests
    // ========================================================================

    #[test]
    fn test_is_private_ip_class_a() {
        // 10.0.0.0/8 range
        assert!(is_private_ip(&"10.0.0.1".parse().unwrap()));
        assert!(is_private_ip(&"10.255.255.255".parse().unwrap()));
    }

    #[test]
    fn test_is_private_ip_class_b() {
        // 172.16.0.0/12 range
        assert!(is_private_ip(&"172.16.0.1".parse().unwrap()));
        assert!(is_private_ip(&"172.31.255.255".parse().unwrap()));
        assert!(!is_private_ip(&"172.15.0.1".parse().unwrap()));
        assert!(!is_private_ip(&"172.32.0.1".parse().unwrap()));
    }

    #[test]
    fn test_is_private_ip_class_c() {
        // 192.168.0.0/16 range
        assert!(is_private_ip(&"192.168.0.1".parse().unwrap()));
        assert!(is_private_ip(&"192.168.255.255".parse().unwrap()));
        assert!(!is_private_ip(&"192.167.0.1".parse().unwrap()));
    }

    #[test]
    fn test_is_private_ip_loopback() {
        // 127.0.0.0/8 range
        assert!(is_private_ip(&"127.0.0.1".parse().unwrap()));
        assert!(is_private_ip(&"127.255.255.255".parse().unwrap()));
    }

    #[test]
    fn test_is_private_ip_link_local() {
        // 169.254.0.0/16 range
        assert!(is_private_ip(&"169.254.0.1".parse().unwrap()));
        assert!(is_private_ip(&"169.254.255.255".parse().unwrap()));
    }

    #[test]
    fn test_is_private_ip_public() {
        // Public IPs should not be private
        assert!(!is_private_ip(&"8.8.8.8".parse().unwrap()));
        assert!(!is_private_ip(&"1.1.1.1".parse().unwrap()));
        assert!(!is_private_ip(&"142.250.80.110".parse().unwrap())); // google.com
    }

    #[test]
    fn test_is_private_ip_ipv6_loopback() {
        assert!(is_private_ip(&"::1".parse().unwrap()));
    }

    // ========================================================================
    // PolicyList Deserialization Tests
    // ========================================================================

    #[test]
    fn test_policy_list_deserialization() {
        let json = r#"{"allowed": ["bash", "read"], "blocked": ["rm"]}"#;
        let policy: PolicyList = serde_json::from_str(json).unwrap();
        assert_eq!(policy.allowed.as_ref().unwrap().len(), 2);
        assert_eq!(policy.blocked.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn test_policy_list_partial() {
        let json = r#"{"allowed": ["bash"]}"#;
        let policy: PolicyList = serde_json::from_str(json).unwrap();
        assert!(policy.allowed.is_some());
        assert!(policy.blocked.is_none());
    }

    #[test]
    fn test_network_policy_deserialization() {
        let json = r#"{
            "allowedHosts": ["example.com"],
            "blockedHosts": ["evil.com"],
            "blockLocalhost": true,
            "blockPrivateIPs": false
        }"#;
        let policy: NetworkPolicy = serde_json::from_str(json).unwrap();
        assert_eq!(policy.allowed_hosts.as_ref().unwrap().len(), 1);
        assert_eq!(policy.blocked_hosts.as_ref().unwrap().len(), 1);
        assert!(policy.block_localhost.unwrap());
        assert!(!policy.block_private_ips.unwrap());
    }

    #[test]
    fn test_network_policy_blocks_hosts_before_dns_ip_validation() {
        let policy = NetworkPolicy {
            allowed_hosts: None,
            blocked_hosts: Some(vec!["blocked.invalid".to_string()]),
            block_localhost: Some(true),
            block_private_ips: Some(true),
        };

        let reason = check_network_restrictions("https://blocked.invalid/api", &policy).unwrap();

        assert_eq!(
            reason,
            "Host \"blocked.invalid\" is blocked by enterprise policy."
        );
    }

    #[test]
    fn test_network_policy_allowlist_denies_before_dns_ip_validation() {
        let policy = NetworkPolicy {
            allowed_hosts: Some(vec!["api.example.com".to_string()]),
            blocked_hosts: None,
            block_localhost: Some(true),
            block_private_ips: Some(true),
        };

        let reason = check_network_restrictions("https://outside.invalid/api", &policy).unwrap();

        assert_eq!(
            reason,
            "Host \"outside.invalid\" is not in the allowed hosts list."
        );
    }

    #[test]
    fn test_enterprise_policy_deserialization() {
        let json = r#"{
            "tools": {"allowed": ["bash", "read"]},
            "paths": {"blocked": ["/etc/*"]},
            "network": {"blockLocalhost": true},
            "models": {"allowed": ["anthropic/*"]},
            "limits": {"maxTokensPerSession": 1000}
        }"#;
        let policy: EnterprisePolicy = serde_json::from_str(json).unwrap();
        assert!(policy.tools.is_some());
        assert!(policy.paths.is_some());
        assert!(policy.network.is_some());
        assert!(policy.models.is_some());
        assert!(policy.limits.is_some());
    }

    #[test]
    fn test_policy_file_path() {
        let path = policy_file_path();
        if dirs::home_dir().is_some() {
            assert!(path.is_some());
            let p = path.unwrap();
            assert!(p.ends_with("policy.json"));
        }
    }

    #[test]
    fn test_policy_path_candidates_use_custom_maestro_home_without_default_maestro_fallback() {
        let _env_guard = crate::config::test_process_env_lock();
        let _lock = test_env_guard();
        let previous_enterprise = std::env::var("MAESTRO_ENTERPRISE_POLICY_PATH").ok();
        let previous_policy = std::env::var("MAESTRO_POLICY_PATH").ok();
        let previous_home = std::env::var("MAESTRO_HOME").ok();
        let home = dirs::home_dir().expect("home dir");

        std::env::remove_var("MAESTRO_ENTERPRISE_POLICY_PATH");
        std::env::remove_var("MAESTRO_POLICY_PATH");
        std::env::set_var("MAESTRO_HOME", "/tmp/custom-maestro-home");

        let paths: Vec<PathBuf> = policy_path_candidates()
            .into_iter()
            .map(|candidate| candidate.path)
            .collect();

        assert!(paths.contains(&PathBuf::from("/tmp/custom-maestro-home/policy.json")));
        assert!(paths.contains(&home.join(".composer").join("policy.json")));
        assert!(!paths.contains(&home.join(".maestro").join("policy.json")));

        restore_env_var("MAESTRO_ENTERPRISE_POLICY_PATH", previous_enterprise);
        restore_env_var("MAESTRO_POLICY_PATH", previous_policy);
        restore_env_var("MAESTRO_HOME", previous_home);
    }

    #[test]
    fn test_select_policy_path_ignores_existing_directories() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let directory_candidate = temp.path().join("policy-dir");
        std::fs::create_dir(&directory_candidate).expect("create policy dir");

        assert_eq!(
            select_policy_path(vec![PolicyPathCandidate {
                path: directory_candidate,
                explicit: false,
            }]),
            None
        );
    }

    #[test]
    fn test_select_policy_path_prefers_regular_file_over_directory() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let directory_candidate = temp.path().join("policy-dir");
        let file_candidate = temp.path().join("policy.json");
        std::fs::create_dir(&directory_candidate).expect("create policy dir");
        std::fs::write(&file_candidate, "{}").expect("write policy file");

        assert_eq!(
            select_policy_path(vec![
                PolicyPathCandidate {
                    path: directory_candidate,
                    explicit: false,
                },
                PolicyPathCandidate {
                    path: file_candidate.clone(),
                    explicit: false,
                },
            ]),
            Some(file_candidate)
        );
    }

    #[test]
    fn test_select_policy_path_preserves_explicit_invalid_policy_path() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let directory_candidate = temp.path().join("policy-dir");
        std::fs::create_dir(&directory_candidate).expect("create policy dir");

        assert_eq!(
            select_policy_path(vec![PolicyPathCandidate {
                path: directory_candidate.clone(),
                explicit: true,
            }]),
            Some(directory_candidate)
        );
    }
    fn managed_test_policy() -> EnterprisePolicy {
        EnterprisePolicy {
            org_id: Some("org-1".to_string()),
            tools: Some(PolicyList {
                allowed: Some(vec!["bash".to_string(), "read".to_string()]),
                blocked: None,
            }),
            dependencies: None,
            models: None,
            paths: None,
            network: None,
            limits: None,
        }
    }

    fn signed_test_envelope(
        key_pair: &ring::signature::Ed25519KeyPair,
        version: u64,
        kill_switch: bool,
    ) -> ManagedPolicyEnvelope {
        let issued_at = unix_now().expect("test clock") - 1;
        let mut envelope = ManagedPolicyEnvelope {
            schema_version: MANAGED_POLICY_SCHEMA_VERSION,
            org_id: "org-1".to_string(),
            workspace_id: Some("workspace-1".to_string()),
            policy_version: version,
            issued_at,
            expires_at: issued_at + 3600,
            key_id: "test-key".to_string(),
            policy: managed_test_policy(),
            kill_switch,
            kill_switch_reason: kill_switch.then(|| "operator revoked access".to_string()),
            policy_hash: String::new(),
            signature: String::new(),
        };
        let payload = canonical_managed_policy_payload(&envelope);
        envelope.policy_hash = sha256_hex(&payload);
        envelope.signature = URL_SAFE_NO_PAD.encode(key_pair.sign(&payload).as_ref());
        envelope
    }

    fn restore_managed_test_env(previous: Vec<(&str, Option<String>)>) {
        for (name, value) in previous {
            restore_env_var(name, value);
        }
    }

    #[test]
    fn managed_policy_accepts_signature_and_only_narrows_local_policy() {
        let _lock = test_env_guard();
        let previous = vec![
            (
                "MAESTRO_MANAGED_POLICY_PATH",
                std::env::var("MAESTRO_MANAGED_POLICY_PATH").ok(),
            ),
            (
                "MAESTRO_MANAGED_POLICY_PUBLIC_KEY",
                std::env::var("MAESTRO_MANAGED_POLICY_PUBLIC_KEY").ok(),
            ),
            (
                "MAESTRO_MANAGED_POLICY_KEY_ID",
                std::env::var("MAESTRO_MANAGED_POLICY_KEY_ID").ok(),
            ),
            ("MAESTRO_ORG_ID", std::env::var("MAESTRO_ORG_ID").ok()),
            (
                "MAESTRO_WORKSPACE_ID",
                std::env::var("MAESTRO_WORKSPACE_ID").ok(),
            ),
            (
                "MAESTRO_POLICY_PATH",
                std::env::var("MAESTRO_POLICY_PATH").ok(),
            ),
        ];
        let temp = tempfile::tempdir().expect("create managed policy dir");
        let pkcs8 =
            ring::signature::Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new())
                .expect("generate test key");
        let key_pair =
            ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).expect("parse test key");
        let managed_path = temp.path().join("managed-policy.json");
        let local_path = temp.path().join("local-policy.json");
        std::fs::write(
            &managed_path,
            serde_json::to_vec(&signed_test_envelope(&key_pair, 1, false)).unwrap(),
        )
        .expect("write managed policy");
        std::fs::write(&local_path, r#"{"tools":{"allowed":["read"]}}"#)
            .expect("write local policy");

        std::env::set_var("MAESTRO_MANAGED_POLICY_PATH", &managed_path);
        std::env::set_var(
            "MAESTRO_MANAGED_POLICY_PUBLIC_KEY",
            URL_SAFE_NO_PAD.encode(key_pair.public_key().as_ref()),
        );
        std::env::set_var("MAESTRO_MANAGED_POLICY_KEY_ID", "test-key");
        std::env::set_var("MAESTRO_ORG_ID", "org-1");
        std::env::set_var("MAESTRO_WORKSPACE_ID", "workspace-1");
        std::env::set_var("MAESTRO_POLICY_PATH", &local_path);

        let status = refresh_managed_policy();
        assert!(status.valid, "managed status: {status:?}");
        assert_eq!(status.metadata.as_ref().unwrap().policy_version, 1);

        let effective = load_policy(false)
            .expect("effective policy")
            .expect("policy present");
        assert_eq!(
            effective.tools.unwrap().allowed.unwrap(),
            vec!["read".to_string()]
        );
        assert!(check_tool_allowed("read").is_none());
        assert!(check_tool_allowed("bash").is_some());
        assert_eq!(
            managed_policy_metadata().unwrap().policy_hash,
            status.metadata.unwrap().policy_hash
        );
        let denied = crate::agent::ToolExecution::denied(
            "receipt-call",
            "bash",
            crate::agent::DenialReason::ActionFirewall {
                message: "test".to_string(),
            },
        )
        .with_managed_policy(managed_policy_metadata());
        assert_eq!(denied.receipt.policy.unwrap().policy_version, 1);
        let failed = crate::agent::ToolExecution::from_legacy(
            "receipt-failed",
            "bash",
            crate::agent::ExecutionSource::Native,
            crate::agent::ToolResult::failure("test"),
        )
        .with_managed_policy(managed_policy_metadata());
        assert_eq!(
            failed
                .receipt
                .policy
                .as_deref()
                .map(|policy| policy.policy_version),
            Some(1)
        );
        let cancelled = crate::agent::ToolExecution::cancelled(
            "receipt-cancelled",
            "bash",
            crate::agent::ExecutionSource::Native,
            crate::agent::ExecutionPhase::Running,
        )
        .with_managed_policy(managed_policy_metadata());
        assert_eq!(
            cancelled
                .receipt
                .policy
                .as_deref()
                .map(|policy| policy.policy_version),
            Some(1)
        );

        // Exercise the actual composing host and executor, rather than only
        // attaching metadata manually to a protocol value. The caller and the
        // streamed ToolEnd must observe the same verified policy identity.
        let expected_policy = managed_policy_metadata().expect("verified policy identity");
        let cwd = temp.path().to_string_lossy().into_owned();
        let host = crate::agent::TestLocalNativeExecutionHost::compose(
            std::sync::Arc::new(crate::tools::ToolExecutor::new(cwd.clone())),
            crate::hooks::IntegratedHookSystem::new(&cwd),
            |_, _| Err("policy fixture has no model resolver".to_owned()),
            |_| maestro_runtime::agent::NativeModelRoute::DirectProvider,
            None,
        );
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("policy fixture runtime");
        let execution = runtime.block_on(host.execute_tool(
            "bash",
            &serde_json::json!({"command": "true"}),
            Some(&event_tx),
            "composed-policy-call",
            maestro_runtime::agent::NativeToolExecutionOptions {
                cancel: tokio_util::sync::CancellationToken::new(),
                approved_inline_env: None,
            },
        ));
        let mut emitted_receipts = Vec::new();
        while let Ok(event) = event_rx.try_recv() {
            if let crate::agent::FromAgent::ToolEnd {
                call_id, receipt, ..
            } = event
            {
                emitted_receipts.push((call_id, receipt));
            }
        }
        restore_managed_test_env(previous);
        assert!(matches!(
            execution.outcome,
            crate::agent::ToolOutcome::Denied { .. }
        ));
        assert_eq!(execution.receipt.call_id, "composed-policy-call");
        assert_eq!(execution.receipt.policy.as_deref(), Some(&expected_policy));
        assert_eq!(
            emitted_receipts.len(),
            1,
            "one terminal receipt per refusal"
        );
        let (call_id, receipt) = &emitted_receipts[0];
        assert_eq!(call_id, "composed-policy-call");
        let receipt = receipt
            .as_ref()
            .expect("ToolEnd carries the verified receipt");
        assert_eq!(receipt.call_id, execution.receipt.call_id);
        assert_eq!(receipt.status, execution.receipt.status);
        assert_eq!(receipt.policy.as_deref(), Some(&expected_policy));
    }

    #[test]
    fn managed_policy_tamper_and_kill_switch_fail_closed() {
        let _lock = test_env_guard();
        let previous = vec![
            (
                "MAESTRO_MANAGED_POLICY_PATH",
                std::env::var("MAESTRO_MANAGED_POLICY_PATH").ok(),
            ),
            (
                "MAESTRO_MANAGED_POLICY_PUBLIC_KEY",
                std::env::var("MAESTRO_MANAGED_POLICY_PUBLIC_KEY").ok(),
            ),
            (
                "MAESTRO_MANAGED_POLICY_KEY_ID",
                std::env::var("MAESTRO_MANAGED_POLICY_KEY_ID").ok(),
            ),
            ("MAESTRO_ORG_ID", std::env::var("MAESTRO_ORG_ID").ok()),
            (
                "MAESTRO_WORKSPACE_ID",
                std::env::var("MAESTRO_WORKSPACE_ID").ok(),
            ),
        ];
        let temp = tempfile::tempdir().expect("create managed policy dir");
        let pkcs8 =
            ring::signature::Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new())
                .expect("generate test key");
        let key_pair =
            ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).expect("parse test key");
        let managed_path = temp.path().join("managed-policy.json");
        let mut envelope = signed_test_envelope(&key_pair, 1, false);
        std::fs::write(&managed_path, serde_json::to_vec(&envelope).unwrap())
            .expect("write managed policy");

        std::env::set_var("MAESTRO_MANAGED_POLICY_PATH", &managed_path);
        std::env::set_var(
            "MAESTRO_MANAGED_POLICY_PUBLIC_KEY",
            URL_SAFE_NO_PAD.encode(key_pair.public_key().as_ref()),
        );
        std::env::set_var("MAESTRO_MANAGED_POLICY_KEY_ID", "test-key");
        std::env::set_var("MAESTRO_ORG_ID", "org-1");
        std::env::set_var("MAESTRO_WORKSPACE_ID", "workspace-1");
        assert!(refresh_managed_policy().valid);
        assert!(
            managed_path
                .with_file_name("managed-policy.json.state")
                .is_file()
        );

        let mut expired = signed_test_envelope(&key_pair, 1, false);
        let now = unix_now().unwrap();
        expired.issued_at = now.saturating_sub(100);
        expired.expires_at = now.saturating_sub(1);
        std::fs::write(&managed_path, serde_json::to_vec(&expired).unwrap())
            .expect("write expired policy");
        let expired_status = refresh_managed_policy();
        assert!(!expired_status.valid);
        assert!(expired_status.error.unwrap().contains("expired"));

        let mut future = signed_test_envelope(&key_pair, 1, false);
        future.issued_at = unix_now().unwrap() + 600;
        future.expires_at = future.issued_at + 3600;
        std::fs::write(&managed_path, serde_json::to_vec(&future).unwrap())
            .expect("write future policy");
        let future_status = refresh_managed_policy();
        assert!(!future_status.valid);
        assert!(future_status.error.unwrap().contains("future"));

        let key_id_check = signed_test_envelope(&key_pair, 1, false);
        std::fs::write(&managed_path, serde_json::to_vec(&key_id_check).unwrap())
            .expect("write key-id policy");
        std::env::set_var("MAESTRO_MANAGED_POLICY_KEY_ID", "wrong-key");
        let key_id_status = refresh_managed_policy();
        assert!(!key_id_status.valid);
        assert!(key_id_status.error.unwrap().contains("key id"));
        std::env::set_var("MAESTRO_MANAGED_POLICY_KEY_ID", "test-key");

        std::fs::write(&managed_path, b"not-json").expect("write malformed policy");
        let malformed = refresh_managed_policy();
        assert!(!malformed.valid);
        assert!(malformed.error.unwrap().contains("valid JSON"));

        std::fs::write(&managed_path, serde_json::to_vec(&envelope).unwrap())
            .expect("restore valid policy");
        assert!(refresh_managed_policy().valid);

        envelope.policy.tools.as_mut().unwrap().allowed = Some(vec!["read".to_string()]);
        std::fs::write(&managed_path, serde_json::to_vec(&envelope).unwrap())
            .expect("tamper managed policy");
        let tampered = refresh_managed_policy();
        assert!(!tampered.valid);
        assert!(check_tool_allowed("bash").is_some());

        let revoked = signed_test_envelope(&key_pair, 2, true);
        std::fs::write(&managed_path, serde_json::to_vec(&revoked).unwrap())
            .expect("write revoked policy");
        let kill_switch = refresh_managed_policy();
        assert!(!kill_switch.valid);
        assert!(kill_switch.error.unwrap().contains("kill switch"));

        restore_managed_test_env(previous);
    }

    #[test]
    fn managed_policy_rejects_scope_and_rollbacks() {
        let _lock = test_env_guard();
        let previous = vec![
            (
                "MAESTRO_MANAGED_POLICY_PATH",
                std::env::var("MAESTRO_MANAGED_POLICY_PATH").ok(),
            ),
            (
                "MAESTRO_MANAGED_POLICY_PUBLIC_KEY",
                std::env::var("MAESTRO_MANAGED_POLICY_PUBLIC_KEY").ok(),
            ),
            (
                "MAESTRO_MANAGED_POLICY_KEY_ID",
                std::env::var("MAESTRO_MANAGED_POLICY_KEY_ID").ok(),
            ),
            ("MAESTRO_ORG_ID", std::env::var("MAESTRO_ORG_ID").ok()),
            (
                "MAESTRO_WORKSPACE_ID",
                std::env::var("MAESTRO_WORKSPACE_ID").ok(),
            ),
        ];
        let temp = tempfile::tempdir().expect("create managed policy dir");
        let pkcs8 =
            ring::signature::Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new())
                .expect("generate test key");
        let key_pair =
            ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).expect("parse test key");
        let managed_path = temp.path().join("managed-policy.json");
        std::env::set_var("MAESTRO_MANAGED_POLICY_PATH", &managed_path);
        std::env::set_var(
            "MAESTRO_MANAGED_POLICY_PUBLIC_KEY",
            URL_SAFE_NO_PAD.encode(key_pair.public_key().as_ref()),
        );
        std::env::set_var("MAESTRO_MANAGED_POLICY_KEY_ID", "test-key");
        std::env::set_var("MAESTRO_ORG_ID", "org-1");
        std::env::set_var("MAESTRO_WORKSPACE_ID", "workspace-1");

        let mut scope = signed_test_envelope(&key_pair, 2, false);
        scope.org_id = "other-org".to_string();
        scope.policy.org_id = Some("other-org".to_string());
        let payload = canonical_managed_policy_payload(&scope);
        scope.policy_hash = sha256_hex(&payload);
        scope.signature = URL_SAFE_NO_PAD.encode(key_pair.sign(&payload).as_ref());
        std::fs::write(&managed_path, serde_json::to_vec(&scope).unwrap())
            .expect("write out-of-scope policy");
        let scope_status = refresh_managed_policy();
        assert!(!scope_status.valid);
        assert!(scope_status.error.unwrap().contains("organization scope"));

        std::env::set_var("MAESTRO_ORG_ID", "org-1");
        let version_two = signed_test_envelope(&key_pair, 2, false);
        std::fs::write(&managed_path, serde_json::to_vec(&version_two).unwrap())
            .expect("write version two");
        assert!(refresh_managed_policy().valid);
        let version_one = signed_test_envelope(&key_pair, 1, false);
        std::fs::write(&managed_path, serde_json::to_vec(&version_one).unwrap())
            .expect("write rollback");
        let rollback = refresh_managed_policy();
        assert!(!rollback.valid);
        assert!(rollback.error.unwrap().contains("rollback"));

        restore_managed_test_env(previous);
    }

    #[test]
    fn managed_policy_publish_persists_and_audits() {
        let _lock = test_env_guard();
        let previous = vec![
            (
                "MAESTRO_MANAGED_POLICY_PATH",
                std::env::var("MAESTRO_MANAGED_POLICY_PATH").ok(),
            ),
            (
                "MAESTRO_MANAGED_POLICY_STATE_PATH",
                std::env::var("MAESTRO_MANAGED_POLICY_STATE_PATH").ok(),
            ),
            (
                "MAESTRO_MANAGED_POLICY_AUDIT_PATH",
                std::env::var("MAESTRO_MANAGED_POLICY_AUDIT_PATH").ok(),
            ),
            (
                "MAESTRO_MANAGED_POLICY_PUBLIC_KEY",
                std::env::var("MAESTRO_MANAGED_POLICY_PUBLIC_KEY").ok(),
            ),
            (
                "MAESTRO_MANAGED_POLICY_KEY_ID",
                std::env::var("MAESTRO_MANAGED_POLICY_KEY_ID").ok(),
            ),
            ("MAESTRO_ORG_ID", std::env::var("MAESTRO_ORG_ID").ok()),
            (
                "MAESTRO_WORKSPACE_ID",
                std::env::var("MAESTRO_WORKSPACE_ID").ok(),
            ),
        ];
        let temp = tempfile::tempdir().expect("create managed policy dir");
        let pkcs8 =
            ring::signature::Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new())
                .expect("generate test key");
        let key_pair =
            ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).expect("parse test key");
        let managed_path = temp.path().join("managed-policy.json");
        let state_path = temp.path().join("managed-policy.state");
        let audit_path = temp.path().join("managed-policy.audit.jsonl");

        std::env::set_var("MAESTRO_MANAGED_POLICY_PATH", &managed_path);
        std::env::set_var("MAESTRO_MANAGED_POLICY_STATE_PATH", &state_path);
        std::env::set_var("MAESTRO_MANAGED_POLICY_AUDIT_PATH", &audit_path);
        std::env::set_var(
            "MAESTRO_MANAGED_POLICY_PUBLIC_KEY",
            URL_SAFE_NO_PAD.encode(key_pair.public_key().as_ref()),
        );
        std::env::set_var("MAESTRO_MANAGED_POLICY_KEY_ID", "test-key");
        std::env::set_var("MAESTRO_ORG_ID", "org-1");
        std::env::set_var("MAESTRO_WORKSPACE_ID", "workspace-1");

        let envelope = signed_test_envelope(&key_pair, 1, false);
        let published = publish_managed_policy(envelope.clone()).expect("publish policy");
        assert!(published.published);
        assert!(published.status.valid);
        assert_eq!(
            published.status.metadata.as_ref().unwrap().policy_version,
            1
        );
        assert!(managed_path.is_file());

        record_managed_policy_audit(ManagedPolicyAuditEvent {
            event_id: "event-1".to_string(),
            action: "publish".to_string(),
            actor: Some("admin@example.com".to_string()),
            recorded_at: unix_now().expect("test clock"),
            outcome: "accepted".to_string(),
            metadata: published.status.metadata.clone(),
            reason: None,
        })
        .expect("record audit event");
        let audit = managed_policy_audit(10).expect("read audit events");
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].event_id, "event-1");
        assert_eq!(audit[0].actor.as_deref(), Some("admin@example.com"));
        assert_eq!(audit[0].metadata.as_ref().unwrap().policy_version, 1);

        let original = std::fs::read(&managed_path).expect("read active policy");
        let mut tampered = envelope;
        tampered.policy.tools.as_mut().unwrap().allowed = Some(vec!["read".to_string()]);
        let tamper_error = publish_managed_policy(tampered).expect_err("reject tamper");
        assert!(tamper_error.contains("hash") || tamper_error.contains("signature"));
        assert_eq!(std::fs::read(&managed_path).unwrap(), original);

        assert!(
            publish_managed_policy(signed_test_envelope(&key_pair, 2, false))
                .expect("publish version two")
                .status
                .valid
        );
        let rollback = publish_managed_policy(signed_test_envelope(&key_pair, 1, false))
            .expect_err("reject rollback");
        assert!(rollback.contains("rollback"));

        restore_managed_test_env(previous);
    }
}
