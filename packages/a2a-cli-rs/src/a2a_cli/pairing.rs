//! A2A peer pairing code encode/decode (TS-compatible `maestro-pair-v1`).

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const PAIRING_CODE_PREFIX: &str = "maestro-pair-v1";
const PAIRING_CODE_VERSION: u32 = 1;
const PAIRING_CODE_CHECKSUM_LENGTH: usize = 16;
const MAX_PAIRING_CODE_LENGTH: usize = 8192;
const MAX_SKILLS_IN_PAIRING_CODE: usize = 8;
const MAX_SKILL_TAGS_IN_PAIRING_CODE: usize = 8;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingPayload {
    pub version: u32,
    pub display_name: String,
    pub agent_card_url: String,
    pub transport_url: String,
    pub protocol_binding: String,
    pub protocol_version: String,
    pub issued_at: String,
    pub expires_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relay_hints: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct PeerConnection {
    pub peer_id: String,
    pub display_name: String,
    pub base_url: String,
    pub agent_card_url: String,
    pub protocol_binding: String,
    pub protocol_version: String,
    pub capabilities: Option<Value>,
    pub skills: Option<Vec<Value>>,
    pub key_fingerprint: Option<String>,
    pub metadata: Option<Value>,
}

pub fn create_pairing_payload(
    display_name: &str,
    agent_card_url: &str,
    transport_url: &str,
    peer_id: Option<&str>,
    ttl_ms: u64,
) -> Result<PairingPayload> {
    let now = chrono::Utc::now();
    let expires_at = now + chrono::Duration::milliseconds(ttl_ms.max(1) as i64);
    let payload = PairingPayload {
        version: PAIRING_CODE_VERSION,
        display_name: require_non_empty(display_name, "displayName")?,
        agent_card_url: normalize_pairing_url(agent_card_url, "agentCardUrl")?,
        transport_url: normalize_pairing_url(transport_url, "transportUrl")?,
        protocol_binding: "HTTP+JSON".into(),
        protocol_version: "1.0".into(),
        issued_at: now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        expires_at: expires_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        peer_id: peer_id
            .map(|v| require_non_empty(v, "peerId"))
            .transpose()?,
        provider: None,
        capabilities: None,
        skills: None,
        key_fingerprint: None,
        relay_hints: None,
        metadata: None,
    };
    validate_payload(&payload, false)?;
    Ok(payload)
}

pub fn create_pairing_payload_from_agent_card(
    agent_card: &Value,
    agent_card_url: &str,
    display_name: Option<&str>,
    peer_id: Option<&str>,
    ttl_ms: u64,
) -> Result<PairingPayload> {
    let selected = select_agent_interface(agent_card)?;
    let name = display_name
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            agent_card
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "Deixic Code A2A Peer".into());

    let now = chrono::Utc::now();
    let expires_at = now + chrono::Duration::milliseconds(ttl_ms.max(1) as i64);
    let skills = agent_card
        .get("skills")
        .and_then(|v| v.as_array())
        .map(|skills| {
            skills
                .iter()
                .take(MAX_SKILLS_IN_PAIRING_CODE)
                .cloned()
                .collect::<Vec<_>>()
        });
    let payload = PairingPayload {
        version: PAIRING_CODE_VERSION,
        display_name: name,
        agent_card_url: normalize_pairing_url(agent_card_url, "agentCardUrl")?,
        transport_url: selected.url,
        protocol_binding: selected.protocol_binding,
        protocol_version: selected.protocol_version,
        issued_at: now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        expires_at: expires_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        peer_id: peer_id
            .map(|v| require_non_empty(v, "peerId"))
            .transpose()?,
        provider: agent_card.get("provider").cloned(),
        capabilities: agent_card.get("capabilities").cloned(),
        skills,
        key_fingerprint: None,
        relay_hints: None,
        metadata: None,
    };
    validate_payload(&payload, false)?;
    Ok(payload)
}

pub fn encode_pairing_code(payload: &PairingPayload) -> Result<String> {
    let normalized = normalize_decoded_payload(
        &serde_json::to_value(payload).context("serialize pairing payload")?,
        true,
    )?;
    reject_secret_bearing_fields(&serde_json::to_value(&normalized)?, "$")?;
    for candidate in pairing_code_size_candidates(normalized) {
        let code = encode_normalized(&candidate)?;
        if code.len() <= MAX_PAIRING_CODE_LENGTH {
            return Ok(code);
        }
    }
    bail!("A2A pairing payload is too large");
}

fn encode_normalized(payload: &PairingPayload) -> Result<String> {
    let json = serde_json::to_string(payload).context("serialize pairing payload")?;
    let encoded = URL_SAFE_NO_PAD.encode(json.as_bytes());
    let checksum = pairing_checksum(&encoded);
    Ok(format!("{PAIRING_CODE_PREFIX}.{encoded}.{checksum}"))
}

fn pairing_code_size_candidates(payload: PairingPayload) -> Vec<PairingPayload> {
    if payload
        .skills
        .as_ref()
        .map(|s| s.is_empty())
        .unwrap_or(true)
    {
        return vec![payload];
    }
    let compact = {
        let mut next = payload.clone();
        next.skills = payload.skills.as_ref().map(|skills| {
            skills
                .iter()
                .map(|skill| {
                    let mut compact = serde_json::Map::new();
                    if let Some(id) = skill.get("id") {
                        compact.insert("id".into(), id.clone());
                    }
                    if let Some(name) = skill.get("name") {
                        compact.insert("name".into(), name.clone());
                    }
                    if let Some(tags) = skill.get("tags").and_then(|v| v.as_array()) {
                        let tags: Vec<Value> = tags
                            .iter()
                            .take(MAX_SKILL_TAGS_IN_PAIRING_CODE)
                            .cloned()
                            .collect();
                        if !tags.is_empty() {
                            compact.insert("tags".into(), Value::Array(tags));
                        }
                    }
                    Value::Object(compact)
                })
                .collect()
        });
        next
    };
    let mut without = payload.clone();
    without.skills = None;
    vec![payload, compact, without]
}

pub fn decode_pairing_code(code: &str, allow_expired: bool) -> Result<PairingPayload> {
    let compact = code.trim();
    if compact.is_empty() {
        bail!("A2A pairing code is required");
    }
    if compact.len() > MAX_PAIRING_CODE_LENGTH {
        bail!("A2A pairing code is too large");
    }
    let parts: Vec<&str> = compact.split('.').collect();
    if parts.len() != 3
        || parts[0] != PAIRING_CODE_PREFIX
        || parts[1].is_empty()
        || parts[2].is_empty()
    {
        bail!("A2A pairing code must use the {PAIRING_CODE_PREFIX} format");
    }
    let encoded = parts[1];
    let checksum = parts[2];
    assert_pairing_checksum(encoded, checksum)?;
    let raw = URL_SAFE_NO_PAD
        .decode(encoded.as_bytes())
        .context("A2A pairing code payload is not valid base64url")?;
    let raw_payload: Value =
        serde_json::from_slice(&raw).context("A2A pairing code payload is not valid JSON")?;
    normalize_decoded_payload(&raw_payload, allow_expired)
}

pub fn peer_connection_from_payload(payload: &PairingPayload) -> Result<PeerConnection> {
    validate_payload(payload, true)?;
    Ok(PeerConnection {
        peer_id: payload
            .peer_id
            .clone()
            .unwrap_or_else(|| stable_peer_id(payload)),
        display_name: payload.display_name.clone(),
        base_url: payload.transport_url.trim_end_matches('/').to_string(),
        agent_card_url: payload.agent_card_url.clone(),
        protocol_binding: payload.protocol_binding.clone(),
        protocol_version: payload.protocol_version.clone(),
        capabilities: payload.capabilities.clone(),
        skills: payload.skills.clone(),
        key_fingerprint: payload.key_fingerprint.clone(),
        metadata: payload.metadata.clone(),
    })
}

struct SelectedInterface {
    url: String,
    protocol_binding: String,
    protocol_version: String,
}

fn select_agent_interface(agent_card: &Value) -> Result<SelectedInterface> {
    let interfaces = agent_card
        .get("supportedInterfaces")
        .and_then(|v| v.as_array())
        .context("A2A Agent Card must include supportedInterfaces")?;
    let preferred = interfaces
        .iter()
        .find(|candidate| {
            candidate
                .get("protocolBinding")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().eq_ignore_ascii_case("HTTP+JSON"))
                .unwrap_or(false)
        })
        .context("A2A Agent Card does not advertise a supported HTTP+JSON interface")?;
    let url = preferred
        .get("url")
        .and_then(|v| v.as_str())
        .context("supportedInterfaces[].url")?;
    let protocol_binding = preferred
        .get("protocolBinding")
        .and_then(|v| v.as_str())
        .unwrap_or("HTTP+JSON");
    let protocol_version = preferred
        .get("protocolVersion")
        .and_then(|v| v.as_str())
        .context("supportedInterfaces[].protocolVersion")?;
    Ok(SelectedInterface {
        url: normalize_pairing_url(url, "supportedInterfaces[].url")?,
        protocol_binding: require_non_empty(protocol_binding, "protocolBinding")?,
        protocol_version: require_non_empty(protocol_version, "protocolVersion")?,
    })
}

pub fn resolve_agent_card_url(input: &str) -> Result<String> {
    let base_url = normalize_a2a_base_url(&normalize_pairing_url(input, "agentCardUrl")?)?;
    let mut parsed = url::Url::parse(&base_url).context("agentCardUrl must be an absolute URL")?;
    let path = parsed.path().trim_end_matches('/');
    parsed.set_path(&format!("{path}/.well-known/agent-card.json"));
    parsed.set_query(None);
    parsed.set_fragment(None);
    normalize_pairing_url(parsed.as_str(), "agentCardUrl")
}

pub fn base_url_from_agent_card_url(agent_card_url: &str) -> Result<String> {
    let mut parsed =
        url::Url::parse(agent_card_url).context("agentCardUrl must be an absolute URL")?;
    let path = {
        let current = parsed.path().trim_end_matches('/').to_string();
        current
            .strip_suffix("/.well-known/agent-card.json")
            .unwrap_or(current.as_str())
            .to_string()
    };
    parsed.set_path(&path);
    parsed.set_query(None);
    parsed.set_fragment(None);
    Ok(parsed.as_str().trim_end_matches('/').to_string())
}

pub fn normalize_a2a_base_url(base_url: &str) -> Result<String> {
    let mut normalized = base_url.trim().trim_end_matches('/').to_string();
    const SUFFIXES: &[&str] = &[
        "/.well-known/agent-card.json",
        "/message:send",
        "/message:stream",
        "/agentruntime.v1.AgentRuntimeService/HandleTrigger",
        "/agentruntime.v1.AgentRuntimeService",
    ];
    for suffix in SUFFIXES {
        if let Some(stripped) = normalized.strip_suffix(suffix) {
            normalized = stripped.trim_end_matches('/').to_string();
        }
    }
    Ok(normalized)
}

fn normalize_decoded_payload(input: &Value, allow_expired: bool) -> Result<PairingPayload> {
    let obj = input
        .as_object()
        .context("A2A pairing code payload must be an object")?;
    let version = obj
        .get("version")
        .and_then(|v| v.as_u64())
        .context("version is required")? as u32;
    if version != PAIRING_CODE_VERSION {
        bail!("Unsupported A2A pairing code version: {version}");
    }
    let payload = PairingPayload {
        version: PAIRING_CODE_VERSION,
        display_name: require_non_empty(
            obj.get("displayName")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            "displayName",
        )?,
        agent_card_url: normalize_pairing_url(
            obj.get("agentCardUrl")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            "agentCardUrl",
        )?,
        transport_url: normalize_pairing_url(
            obj.get("transportUrl")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            "transportUrl",
        )?,
        protocol_binding: require_non_empty(
            obj.get("protocolBinding")
                .and_then(|v| v.as_str())
                .unwrap_or("HTTP+JSON"),
            "protocolBinding",
        )?,
        protocol_version: require_non_empty(
            obj.get("protocolVersion")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            "protocolVersion",
        )?,
        issued_at: parse_iso_timestamp(
            obj.get("issuedAt").and_then(|v| v.as_str()).unwrap_or(""),
            "issuedAt",
        )?
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        expires_at: parse_iso_timestamp(
            obj.get("expiresAt").and_then(|v| v.as_str()).unwrap_or(""),
            "expiresAt",
        )?
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        peer_id: obj
            .get("peerId")
            .and_then(|v| v.as_str())
            .map(|s| require_non_empty(s, "peerId"))
            .transpose()?,
        provider: obj.get("provider").cloned().filter(|v| v.is_object()),
        capabilities: obj.get("capabilities").cloned().filter(|v| v.is_object()),
        skills: obj.get("skills").and_then(|v| v.as_array()).map(|skills| {
            skills
                .iter()
                .take(MAX_SKILLS_IN_PAIRING_CODE)
                .cloned()
                .collect()
        }),
        key_fingerprint: obj
            .get("keyFingerprint")
            .and_then(|v| v.as_str())
            .map(|s| require_non_empty(s, "keyFingerprint"))
            .transpose()?,
        relay_hints: obj.get("relayHints").cloned().filter(|v| v.is_object()),
        metadata: obj.get("metadata").cloned().filter(|v| v.is_object()),
    };
    validate_payload(&payload, allow_expired)?;
    Ok(payload)
}

fn validate_payload(payload: &PairingPayload, allow_expired: bool) -> Result<()> {
    reject_secret_bearing_fields(&serde_json::to_value(payload)?, "$")?;
    let issued_at = parse_iso_timestamp(&payload.issued_at, "issuedAt")?;
    let expires_at = parse_iso_timestamp(&payload.expires_at, "expiresAt")?;
    if expires_at <= issued_at {
        bail!("A2A pairing code expiresAt must be after issuedAt");
    }
    if !allow_expired {
        let now = chrono::Utc::now();
        if expires_at <= now {
            bail!("A2A pairing code has expired");
        }
    }
    Ok(())
}

fn normalize_pairing_url(input: &str, label: &str) -> Result<String> {
    let raw = require_non_empty(input, label)?;
    let mut parsed =
        url::Url::parse(&raw).with_context(|| format!("{label} must be an absolute URL"))?;
    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);
    parsed.set_query(None);
    parsed.set_fragment(None);
    match parsed.scheme() {
        "https" => Ok(parsed.to_string()),
        "http" => {
            if !is_local_pairing_host(parsed.host_str().unwrap_or("")) {
                bail!(
                    "{label} must use https unless it targets localhost, private LAN, or Tailscale"
                );
            }
            Ok(parsed.to_string())
        }
        _ => bail!("{label} must use http or https"),
    }
}

fn is_local_pairing_host(hostname: &str) -> bool {
    let host = hostname
        .to_ascii_lowercase()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    if host == "localhost"
        || host == "::1"
        || host.ends_with(".local")
        || host.ends_with(".ts.net")
        || !host.contains('.')
    {
        return true;
    }
    let ipv4_parts: Vec<i32> = host
        .split('.')
        .filter_map(|part| part.parse().ok())
        .collect();
    if ipv4_parts.len() == 4 {
        let first = ipv4_parts[0];
        let second = ipv4_parts[1];
        return first == 10
            || first == 127
            || (first == 192 && second == 168)
            || (first == 172 && (16..=31).contains(&second))
            || (first == 100 && (64..=127).contains(&second));
    }
    host.contains(':')
        && (host.starts_with("fc")
            || host.starts_with("fd")
            || host.starts_with("fe80:")
            || host.starts_with('f')
            || host.starts_with('F'))
}

fn reject_secret_bearing_fields(value: &Value, path: &str) -> Result<()> {
    match value {
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                reject_secret_bearing_fields(item, &format!("{path}[{index}]"))?;
            }
        }
        Value::Object(map) => {
            for (key, child) in map {
                if is_secret_like_key(key) {
                    bail!("A2A pairing codes must not include secret field {path}.{key}");
                }
                reject_secret_bearing_fields(child, &format!("{path}.{key}"))?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn is_secret_like_key(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .filter(|c| *c != '-' && *c != '_' && !c.is_whitespace())
        .collect();
    normalized == "authorization"
        || normalized == "token"
        || normalized.ends_with("token")
        || normalized == "secret"
        || normalized.ends_with("secret")
        || normalized == "password"
        || normalized.ends_with("password")
        || normalized == "apikey"
        || normalized.ends_with("apikey")
        || normalized == "credentials"
        || normalized.ends_with("credentials")
        || normalized == "bearer"
}

fn require_non_empty(input: &str, label: &str) -> Result<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("{label} is required");
    }
    Ok(trimmed.to_string())
}

fn parse_iso_timestamp(input: &str, label: &str) -> Result<chrono::DateTime<chrono::Utc>> {
    let trimmed = require_non_empty(input, label)?;
    chrono::DateTime::parse_from_rfc3339(&trimmed)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(&trimmed, "%Y-%m-%dT%H:%M:%S%.f")
                .or_else(|_| chrono::NaiveDateTime::parse_from_str(&trimmed, "%Y-%m-%dT%H:%M:%S"))
                .map(|naive| naive.and_utc())
                .map_err(|_| anyhow::anyhow!("{label} must be an ISO timestamp"))
        })
}

fn pairing_checksum(encoded: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("{PAIRING_CODE_PREFIX}.{encoded}").as_bytes());
    let digest = hasher.finalize();
    hex_encode(&digest)[..PAIRING_CODE_CHECKSUM_LENGTH].to_string()
}

fn assert_pairing_checksum(encoded: &str, checksum: &str) -> Result<()> {
    let expected = pairing_checksum(encoded);
    if checksum.len() != expected.len() {
        bail!("A2A pairing code checksum does not match");
    }
    if !constant_time_eq(checksum.as_bytes(), expected.as_bytes()) {
        bail!("A2A pairing code checksum does not match");
    }
    Ok(())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn stable_peer_id(payload: &PairingPayload) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!(
        "{}\n{}\n{}",
        payload.display_name, payload.agent_card_url, payload.transport_url
    ));
    let digest = hasher.finalize();
    let encoded = URL_SAFE_NO_PAD.encode(digest);
    format!("a2a-peer-{}", &encoded[..16.min(encoded.len())])
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests;
