//! A2A peer registry on disk (`~/.maestro/a2a/peers.json`), TS-compatible.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::client::A2AServiceConfig;
use super::pairing::{PairingPayload, normalize_a2a_base_url, peer_connection_from_payload};
use maestro_local_host::path_utils::{env_path, maestro_home_dir, resolve_env_path};
use maestro_local_host::skill_cli::write_atomic;

const DEFAULT_TIMEOUT_MS: u64 = 600_000;
const DEFAULT_MAX_ATTEMPTS: u64 = 1;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerRegistryFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_peer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u64>,
    #[serde(default)]
    pub peers: BTreeMap<String, PeerRegistryEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerRegistryEntry {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_card_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol_binding: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_env: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organization_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UpsertPeerOptions {
    pub name: Option<String>,
    pub make_default: bool,
    pub token_env: Option<String>,
    pub token_file: Option<String>,
    pub session_id: Option<String>,
    pub workspace_id: Option<String>,
    pub organization_id: Option<String>,
    pub registry_path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvePeerOptions {
    pub registry_path: Option<String>,
    pub timeout_ms: Option<u64>,
    pub token: Option<String>,
    pub max_attempts: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ResolvedPeer {
    pub name: String,
    pub entry: PeerRegistryEntry,
    pub config: A2AServiceConfig,
}

#[derive(Debug, Clone)]
pub struct UpsertResult {
    pub name: String,
    pub entry: PeerRegistryEntry,
    pub path: PathBuf,
}

pub fn get_peer_registry_path(path: Option<&str>) -> Result<PathBuf> {
    if let Some(configured) = path
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| env_path("MAESTRO_A2A_PEERS_FILE"))
        .or_else(|| env_path("CODEX_A2A_PEERS_FILE"))
    {
        return Ok(resolve_env_path(&configured.to_string_lossy()).unwrap_or(configured));
    }
    default_registry_path()
}

fn default_registry_path() -> Result<PathBuf> {
    Ok(maestro_home_dir()
        .context("Maestro home is unavailable")?
        .join("a2a")
        .join("peers.json"))
}

pub fn load_peer_registry(path: Option<&str>) -> Result<(PathBuf, PeerRegistryFile)> {
    let path = get_peer_registry_path(path)?;
    if !path.exists() {
        return Ok((path, PeerRegistryFile::default()));
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("read A2A peer registry {}", path.display()))?;
    let parsed: Value = serde_json::from_str(&raw)
        .with_context(|| format!("parse A2A peer registry {}", path.display()))?;
    let obj = parsed.as_object().with_context(|| {
        format!(
            "A2A peer registry at {} must be a JSON object",
            path.display()
        )
    })?;
    let mut registry = PeerRegistryFile::default();
    if let Some(default_peer) = obj.get("defaultPeer").and_then(|v| v.as_str()) {
        let trimmed = default_peer.trim();
        if !trimmed.is_empty() {
            registry.default_peer = Some(trimmed.to_string());
        }
    }
    if let Some(timeout) = obj.get("timeoutMs").and_then(|v| v.as_u64()) {
        if timeout > 0 {
            registry.timeout_ms = Some(timeout);
        }
    }
    if let Some(max_attempts) = obj.get("maxAttempts").and_then(|v| v.as_u64()) {
        if max_attempts > 0 {
            registry.max_attempts = Some(max_attempts);
        }
    }
    if let Some(peers) = obj.get("peers").and_then(|v| v.as_object()) {
        for (name, value) in peers {
            registry
                .peers
                .insert(name.clone(), normalize_registry_entry(value, name)?);
        }
    }
    Ok((path, registry))
}

pub fn save_peer_registry(registry: &PeerRegistryFile, path: Option<&str>) -> Result<PathBuf> {
    let path = get_peer_registry_path(path)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create registry directory {}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
    }
    let content = format!("{}\n", serde_json::to_string_pretty(registry)?);
    write_atomic(&path, &content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
    Ok(path)
}

pub fn list_peers(path: Option<&str>) -> Result<(PathBuf, PeerRegistryFile)> {
    load_peer_registry(path)
}

pub fn upsert_peer_from_pairing_payload(
    payload: &PairingPayload,
    options: UpsertPeerOptions,
) -> Result<UpsertResult> {
    let (path, mut registry) = load_peer_registry(options.registry_path.as_deref())?;
    let connection = peer_connection_from_payload(payload)?;
    let name = normalize_peer_name(
        options
            .name
            .as_deref()
            .unwrap_or(connection.peer_id.as_str()),
    )?;
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let previous = registry.peers.get(&name).cloned();
    let next_base_url = normalize_a2a_base_url(&connection.base_url)?;
    let identity_changed = previous.as_ref().is_some_and(|prev| {
        let prev_url = normalize_a2a_base_url(&prev.url).ok();
        prev_url.as_deref() != Some(next_base_url.as_str())
            || field_changed(
                prev.agent_card_url.as_deref(),
                Some(&connection.agent_card_url),
            )
            || field_changed(
                prev.protocol_binding.as_deref(),
                Some(&connection.protocol_binding),
            )
            || field_changed(
                prev.protocol_version.as_deref(),
                Some(&connection.protocol_version),
            )
            || optional_field_changed(
                prev.key_fingerprint.as_deref(),
                connection.key_fingerprint.as_deref(),
            )
    });

    let (token_env, token_file) = resolve_upsert_token_fields(
        previous.as_ref().and_then(|p| p.token_env.clone()),
        previous.as_ref().and_then(|p| p.token_file.clone()),
        options.token_env,
        options.token_file,
        !identity_changed,
    )?;

    let entry = PeerRegistryEntry {
        url: connection.base_url,
        display_name: Some(connection.display_name),
        agent_card_url: Some(connection.agent_card_url),
        protocol_binding: Some(connection.protocol_binding),
        protocol_version: Some(connection.protocol_version),
        token_env,
        token_file,
        organization_id: options
            .organization_id
            .or_else(|| previous.as_ref().and_then(|p| p.organization_id.clone())),
        workspace_id: options
            .workspace_id
            .or_else(|| previous.as_ref().and_then(|p| p.workspace_id.clone())),
        agent_id: previous.as_ref().and_then(|p| p.agent_id.clone()),
        session_id: options
            .session_id
            .map(|session_id| session_id.trim().to_owned())
            .filter(|session_id| !session_id.is_empty())
            .or_else(|| previous.as_ref().and_then(|p| p.session_id.clone())),
        actor_id: previous.as_ref().and_then(|p| p.actor_id.clone()),
        timeout_ms: previous.as_ref().and_then(|p| p.timeout_ms),
        max_attempts: previous.as_ref().and_then(|p| p.max_attempts),
        capabilities: connection
            .capabilities
            .or_else(|| previous.as_ref().and_then(|p| p.capabilities.clone())),
        skills: connection
            .skills
            .map(Value::Array)
            .or_else(|| previous.as_ref().and_then(|p| p.skills.clone())),
        key_fingerprint: connection
            .key_fingerprint
            .or_else(|| previous.as_ref().and_then(|p| p.key_fingerprint.clone())),
        metadata: connection
            .metadata
            .or_else(|| previous.as_ref().and_then(|p| p.metadata.clone())),
        created_at: previous
            .as_ref()
            .and_then(|p| p.created_at.clone())
            .or(Some(now.clone())),
        updated_at: Some(now),
    };
    registry.peers.insert(name.clone(), entry.clone());
    if options.make_default || registry.default_peer.is_none() {
        registry.default_peer = Some(name.clone());
    }
    let path_str = path.display().to_string();
    let saved = save_peer_registry(&registry, Some(path_str.as_str()))?;
    Ok(UpsertResult {
        name,
        entry,
        path: saved,
    })
}

pub fn resolve_peer(name: Option<&str>, options: ResolvePeerOptions) -> Result<ResolvedPeer> {
    let (_path, registry) = load_peer_registry(options.registry_path.as_deref())?;
    let resolved_name = normalize_peer_name(
        name.or(registry.default_peer.as_deref())
            .context("A2A peer name is required")?,
    )?;
    let entry = registry.peers.get(&resolved_name).cloned().with_context(|| {
        format!(
            "Unknown A2A peer \"{resolved_name}\". Run \"deixic-code a2a peers\" to list registered peers."
        )
    })?;
    let token = options
        .token
        .or_else(|| resolve_peer_token(&entry).ok().flatten());
    let config = A2AServiceConfig {
        base_url: normalize_a2a_base_url(&entry.url)?,
        token,
        organization_id: entry.organization_id.clone(),
        workspace_id: entry.workspace_id.clone(),
        agent_id: entry.agent_id.clone().or_else(|| Some("maestro".into())),
        session_id: entry.session_id.clone(),
        actor_id: entry.actor_id.clone(),
        timeout_ms: options
            .timeout_ms
            .or(entry.timeout_ms)
            .or(registry.timeout_ms)
            .unwrap_or(DEFAULT_TIMEOUT_MS),
        max_attempts: options
            .max_attempts
            .or(entry.max_attempts)
            .or(registry.max_attempts)
            .unwrap_or(DEFAULT_MAX_ATTEMPTS),
    };
    Ok(ResolvedPeer {
        name: resolved_name,
        entry,
        config,
    })
}

fn resolve_peer_token(entry: &PeerRegistryEntry) -> Result<Option<String>> {
    if let Some(token_env) = &entry.token_env {
        if let Ok(value) = std::env::var(token_env) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Ok(Some(trimmed.to_string()));
            }
        }
    }
    if let Some(token_file) = &entry.token_file {
        let path = resolve_env_path(token_file).unwrap_or_else(|| PathBuf::from(token_file));
        let content = fs::read_to_string(&path)
            .with_context(|| format!("read A2A peer token file {}", path.display()))?;
        let trimmed = content.trim();
        if !trimmed.is_empty() {
            return Ok(Some(trimmed.to_string()));
        }
    }
    Ok(None)
}

pub fn normalize_peer_name(name: &str) -> Result<String> {
    let normalized = name.trim();
    if normalized.is_empty() {
        bail!("A2A peer name is required");
    }
    if normalized.len() > 80
        || !normalized
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
    {
        bail!("A2A peer names may only contain letters, numbers, dots, underscores, and dashes");
    }
    Ok(normalized.to_string())
}

fn resolve_upsert_token_fields(
    previous_token_env: Option<String>,
    previous_token_file: Option<String>,
    token_env: Option<String>,
    token_file: Option<String>,
    retain_existing: bool,
) -> Result<(Option<String>, Option<String>)> {
    let token_env = token_env
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let token_file = token_file
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(|s| {
            resolve_env_path(&s)
                .map(|p| p.display().to_string())
                .unwrap_or(s)
        });
    if token_env.is_some() || token_file.is_some() {
        return Ok((token_env, token_file));
    }
    if !retain_existing {
        return Ok((None, None));
    }
    Ok((previous_token_env, previous_token_file))
}

fn field_changed(previous: Option<&str>, next: Option<&str>) -> bool {
    let previous = previous.map(str::trim).filter(|s| !s.is_empty());
    let next = next.map(str::trim).filter(|s| !s.is_empty());
    matches!((previous, next), (Some(p), Some(n)) if p != n)
}

fn optional_field_changed(previous: Option<&str>, next: Option<&str>) -> bool {
    let previous = previous.map(str::trim).filter(|s| !s.is_empty());
    let next = next.map(str::trim).filter(|s| !s.is_empty());
    previous != next
}

fn normalize_registry_entry(input: &Value, label: &str) -> Result<PeerRegistryEntry> {
    let obj = input
        .as_object()
        .with_context(|| format!("{label} must be an object"))?;
    let url = obj
        .get("url")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .with_context(|| format!("{label}.url is required"))?
        .to_string();
    Ok(PeerRegistryEntry {
        url,
        display_name: string_field(obj, "displayName"),
        agent_card_url: string_field(obj, "agentCardUrl"),
        protocol_binding: string_field(obj, "protocolBinding"),
        protocol_version: string_field(obj, "protocolVersion"),
        token_env: string_field(obj, "tokenEnv"),
        token_file: string_field(obj, "tokenFile"),
        organization_id: string_field(obj, "organizationId"),
        workspace_id: string_field(obj, "workspaceId"),
        agent_id: string_field(obj, "agentId"),
        session_id: string_field(obj, "sessionId"),
        actor_id: string_field(obj, "actorId"),
        timeout_ms: obj
            .get("timeoutMs")
            .and_then(|v| v.as_u64())
            .filter(|v| *v > 0),
        max_attempts: obj
            .get("maxAttempts")
            .and_then(|v| v.as_u64())
            .filter(|v| *v > 0),
        capabilities: obj.get("capabilities").cloned().filter(|v| v.is_object()),
        skills: obj.get("skills").cloned(),
        key_fingerprint: string_field(obj, "keyFingerprint"),
        metadata: obj.get("metadata").cloned().filter(|v| v.is_object()),
        created_at: string_field(obj, "createdAt"),
        updated_at: string_field(obj, "updatedAt"),
    })
}

fn string_field(obj: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[allow(dead_code)]
pub fn registry_path_display(path: &Path) -> String {
    path.display().to_string()
}

#[cfg(test)]
mod tests;
