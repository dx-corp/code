//! Tests for the A2A pairing code wire format.
//!
//! `maestro-pair-v1` codes are `prefix.base64url(json).checksum` and are the
//! only artifact exchanged between peers to bootstrap a connection, so this
//! module leans on malformed/adversarial inputs: wrong version, tampered
//! checksum, non-JSON/non-object payloads, wrong field types, missing
//! required fields, secret-bearing fields, and oversized payloads.

use super::*;
use serde_json::{Map, json};

/// A minimal, valid `maestro-pair-v1` payload as raw wire JSON (camelCase),
/// so tests can remove/replace individual fields to simulate adversarial
/// input from a peer that does not honor the Rust type.
fn base_payload_map() -> Map<String, Value> {
    let now = chrono::Utc::now();
    let issued_at = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let expires_at =
        (now + chrono::Duration::hours(1)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let mut map = Map::new();
    map.insert("version".into(), json!(PAIRING_CODE_VERSION));
    map.insert("displayName".into(), json!("Peer One"));
    map.insert(
        "agentCardUrl".into(),
        json!("https://peer.example.com/.well-known/agent-card.json"),
    );
    map.insert("transportUrl".into(), json!("https://peer.example.com/a2a"));
    map.insert("protocolBinding".into(), json!("HTTP+JSON"));
    map.insert("protocolVersion".into(), json!("1.0"));
    map.insert("issuedAt".into(), json!(issued_at));
    map.insert("expiresAt".into(), json!(expires_at));
    map
}

/// Encode an arbitrary (possibly malformed) JSON value using the exact wire
/// framing `encode_normalized` uses for well-typed payloads, so tests can
/// exercise `decode_pairing_code`'s parsing robustness against input that
/// could never be produced by our own `PairingPayload` serializer.
fn encode_raw(value: &Value) -> String {
    let json = value.to_string();
    let encoded = URL_SAFE_NO_PAD.encode(json.as_bytes());
    let checksum = pairing_checksum(&encoded);
    format!("{PAIRING_CODE_PREFIX}.{encoded}.{checksum}")
}

#[test]
fn encode_then_decode_round_trips_fields() {
    let payload = create_pairing_payload(
        "Peer One",
        "https://peer.example.com/.well-known/agent-card.json",
        "https://peer.example.com/a2a",
        Some("peer-one"),
        60_000,
    )
    .expect("payload");
    let code = encode_pairing_code(&payload).expect("encode");
    assert!(code.starts_with(&format!("{PAIRING_CODE_PREFIX}.")));
    let decoded = decode_pairing_code(&code, false).expect("decode");
    assert_eq!(decoded.display_name, payload.display_name);
    assert_eq!(decoded.agent_card_url, payload.agent_card_url);
    assert_eq!(decoded.transport_url, payload.transport_url);
    assert_eq!(decoded.peer_id.as_deref(), Some("peer-one"));
}

#[test]
fn decode_rejects_empty_code() {
    let err = decode_pairing_code("   ", true).expect_err("empty code must fail");
    assert!(err.to_string().contains("is required"), "{err}");
}

#[test]
fn decode_rejects_oversized_code() {
    let huge = format!(
        "{PAIRING_CODE_PREFIX}.{}.{}",
        "A".repeat(MAX_PAIRING_CODE_LENGTH + 10),
        "0".repeat(16)
    );
    let err = decode_pairing_code(&huge, true).expect_err("oversized code must fail");
    assert!(err.to_string().contains("too large"), "{err}");
}

#[test]
fn decode_rejects_wrong_prefix() {
    let code = encode_raw(&Value::Object(base_payload_map()));
    let bad = code.replacen(PAIRING_CODE_PREFIX, "maestro-pair-v2", 1);
    let err = decode_pairing_code(&bad, true).expect_err("wrong prefix must fail");
    assert!(
        err.to_string()
            .contains(&format!("must use the {PAIRING_CODE_PREFIX} format")),
        "{err}"
    );
}

#[test]
fn decode_rejects_tampered_checksum() {
    let code = encode_raw(&Value::Object(base_payload_map()));
    let mut parts: Vec<String> = code.split('.').map(str::to_string).collect();
    let first = parts[2].chars().next().expect("checksum non-empty");
    let replacement = if first == '0' { '1' } else { '0' };
    parts[2].replace_range(0..1, &replacement.to_string());
    let tampered = parts.join(".");
    let err = decode_pairing_code(&tampered, true).expect_err("tampered checksum must fail");
    assert!(err.to_string().contains("checksum does not match"), "{err}");
}

#[test]
fn decode_rejects_invalid_base64_segment() {
    let encoded = "not-valid-base64!!!";
    let checksum = pairing_checksum(encoded);
    let code = format!("{PAIRING_CODE_PREFIX}.{encoded}.{checksum}");
    let err = decode_pairing_code(&code, true).expect_err("invalid base64 must fail");
    assert!(err.to_string().contains("base64"), "{err}");
}

#[test]
fn decode_rejects_non_json_payload() {
    let encoded = URL_SAFE_NO_PAD.encode(b"not json at all");
    let checksum = pairing_checksum(&encoded);
    let code = format!("{PAIRING_CODE_PREFIX}.{encoded}.{checksum}");
    let err = decode_pairing_code(&code, true).expect_err("non-JSON payload must fail");
    assert!(err.to_string().contains("not valid JSON"), "{err}");
}

#[test]
fn decode_rejects_non_object_payload() {
    let code = encode_raw(&json!([1, 2, 3]));
    let err = decode_pairing_code(&code, true).expect_err("array payload must fail");
    assert!(err.to_string().contains("must be an object"), "{err}");
}

#[test]
fn decode_rejects_unsupported_version() {
    let mut map = base_payload_map();
    map.insert("version".into(), json!(2));
    let code = encode_raw(&Value::Object(map));
    let err = decode_pairing_code(&code, true).expect_err("wrong version must fail");
    assert!(
        err.to_string()
            .contains("Unsupported A2A pairing code version: 2"),
        "{err}"
    );
}

#[test]
fn decode_rejects_non_numeric_version() {
    let mut map = base_payload_map();
    map.insert("version".into(), json!("1"));
    let code = encode_raw(&Value::Object(map));
    let err = decode_pairing_code(&code, true).expect_err("string version must fail");
    assert!(err.to_string().contains("version is required"), "{err}");
}

#[test]
fn decode_rejects_missing_required_field() {
    let mut map = base_payload_map();
    map.remove("displayName");
    let code = encode_raw(&Value::Object(map));
    let err = decode_pairing_code(&code, true).expect_err("missing displayName must fail");
    assert!(err.to_string().contains("displayName is required"), "{err}");
}

#[test]
fn decode_ignores_unknown_fields() {
    let mut map = base_payload_map();
    map.insert("totallyUnknownField".into(), json!("whatever"));
    let code = encode_raw(&Value::Object(map));
    let decoded = decode_pairing_code(&code, true).expect("unknown fields must be tolerated");
    assert_eq!(decoded.display_name, "Peer One");
}

#[test]
fn decode_rejects_secret_bearing_metadata() {
    let mut map = base_payload_map();
    map.insert("metadata".into(), json!({"apiToken": "super-secret"}));
    let code = encode_raw(&Value::Object(map));
    let err =
        decode_pairing_code(&code, true).expect_err("secret-bearing metadata must be rejected");
    assert!(
        err.to_string().contains("must not include secret field"),
        "{err}"
    );
}

#[test]
fn decode_rejects_expired_code_unless_allowed() {
    let now = chrono::Utc::now();
    let issued_at =
        (now - chrono::Duration::hours(2)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let expires_at =
        (now - chrono::Duration::hours(1)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let mut map = base_payload_map();
    map.insert("issuedAt".into(), json!(issued_at));
    map.insert("expiresAt".into(), json!(expires_at));
    let code = encode_raw(&Value::Object(map));

    let err = decode_pairing_code(&code, false).expect_err("expired code must fail by default");
    assert!(err.to_string().contains("expired"), "{err}");

    let allowed = decode_pairing_code(&code, true).expect("allow_expired must accept it");
    assert_eq!(allowed.display_name, "Peer One");
}

#[test]
fn rejects_http_transport_for_public_host() {
    let err = create_pairing_payload(
        "Peer",
        "https://peer.example.com/.well-known/agent-card.json",
        "http://peer.example.com/a2a",
        None,
        60_000,
    )
    .expect_err("plain http to a public host must be rejected");
    assert!(err.to_string().contains("must use https"), "{err}");
}

#[test]
fn allows_http_transport_for_localhost() {
    let payload = create_pairing_payload(
        "Peer",
        "https://peer.example.com/.well-known/agent-card.json",
        "http://localhost:4000/a2a",
        None,
        60_000,
    )
    .expect("http to localhost must be allowed");
    assert_eq!(payload.transport_url, "http://localhost:4000/a2a");
}

#[test]
fn create_from_agent_card_requires_http_json_interface() {
    let agent_card = json!({
        "name": "Peer",
        "supportedInterfaces": [
            {"protocolBinding": "GRPC", "url": "grpc://peer.example.com", "protocolVersion": "1.0"}
        ],
    });
    let err = create_pairing_payload_from_agent_card(
        &agent_card,
        "https://peer.example.com/.well-known/agent-card.json",
        None,
        None,
        60_000,
    )
    .expect_err("agent card without an HTTP+JSON interface must be rejected");
    assert!(err.to_string().contains("HTTP+JSON"), "{err}");
}

#[test]
fn create_from_agent_card_truncates_skills_to_max() {
    let skills: Vec<Value> = (0..12)
        .map(|i| json!({"id": format!("skill-{i}")}))
        .collect();
    let agent_card = json!({
        "name": "Peer",
        "supportedInterfaces": [
            {"protocolBinding": "HTTP+JSON", "url": "https://peer.example.com/a2a", "protocolVersion": "1.0"}
        ],
        "skills": skills,
    });
    let payload = create_pairing_payload_from_agent_card(
        &agent_card,
        "https://peer.example.com/.well-known/agent-card.json",
        None,
        None,
        60_000,
    )
    .expect("payload");
    assert_eq!(
        payload.skills.as_ref().map(Vec::len),
        Some(MAX_SKILLS_IN_PAIRING_CODE)
    );
}

#[test]
fn encode_falls_back_to_compact_skills_when_oversized() {
    let skills: Vec<Value> = (0..MAX_SKILLS_IN_PAIRING_CODE)
        .map(|i| {
            json!({
                "id": format!("skill-{i}"),
                "name": format!("Skill {i}"),
                "tags": ["a", "b", "c"],
                // Large filler the compact candidate must drop to fit under
                // the pairing code size ceiling.
                "description": "x".repeat(2_000),
            })
        })
        .collect();
    let agent_card = json!({
        "name": "Peer",
        "supportedInterfaces": [
            {"protocolBinding": "HTTP+JSON", "url": "https://peer.example.com/a2a", "protocolVersion": "1.0"}
        ],
        "skills": skills,
    });
    let payload = create_pairing_payload_from_agent_card(
        &agent_card,
        "https://peer.example.com/.well-known/agent-card.json",
        None,
        None,
        60_000,
    )
    .expect("payload");

    let code = encode_pairing_code(&payload).expect("encode must fall back to compact skills");
    assert!(code.len() <= MAX_PAIRING_CODE_LENGTH);

    let decoded = decode_pairing_code(&code, false).expect("decode");
    let decoded_skills = decoded.skills.expect("skills must survive compaction");
    assert_eq!(decoded_skills.len(), MAX_SKILLS_IN_PAIRING_CODE);
    assert!(decoded_skills[0].get("id").is_some());
    assert!(
        decoded_skills[0].get("description").is_none(),
        "compact skill entries must drop the oversized filler field"
    );
}

#[test]
fn encode_rejects_payload_too_large_even_without_skills() {
    let now = chrono::Utc::now();
    let payload = PairingPayload {
        version: PAIRING_CODE_VERSION,
        display_name: "x".repeat(20_000),
        agent_card_url: "https://peer.example.com/.well-known/agent-card.json".into(),
        transport_url: "https://peer.example.com/a2a".into(),
        protocol_binding: "HTTP+JSON".into(),
        protocol_version: "1.0".into(),
        issued_at: now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        expires_at: (now + chrono::Duration::hours(1))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        peer_id: None,
        provider: None,
        capabilities: None,
        skills: None,
        key_fingerprint: None,
        relay_hints: None,
        metadata: None,
    };
    let err = encode_pairing_code(&payload).expect_err("oversized payload must be rejected");
    assert!(err.to_string().contains("too large"), "{err}");
}
