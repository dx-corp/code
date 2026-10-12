//! Tests for the on-disk A2A peer registry: load/save error paths and the
//! upsert state-transition logic that decides whether a previously stored
//! bearer token survives a re-pair.

use super::*;
use tempfile::tempdir;

fn registry_path(dir: &tempfile::TempDir) -> String {
    dir.path().join("peers.json").to_string_lossy().into_owned()
}

fn sample_payload(transport_url: &str) -> PairingPayload {
    crate::a2a_cli::pairing::create_pairing_payload(
        "Peer One",
        "https://peer.example.com/.well-known/agent-card.json",
        transport_url,
        Some("peer-one"),
        60_000,
    )
    .expect("payload")
}

fn upsert_options(path: &str, token_env: Option<&str>, make_default: bool) -> UpsertPeerOptions {
    UpsertPeerOptions {
        name: Some("peer".into()),
        make_default,
        token_env: token_env.map(str::to_string),
        token_file: None,
        session_id: None,
        workspace_id: None,
        organization_id: None,
        registry_path: Some(path.to_string()),
    }
}

#[test]
fn load_peer_registry_missing_file_returns_empty_default() {
    let dir = tempdir().expect("tempdir");
    let path = registry_path(&dir);
    let (resolved, registry) = load_peer_registry(Some(path.as_str())).expect("load");
    assert_eq!(resolved, PathBuf::from(&path));
    assert!(registry.peers.is_empty());
    assert!(registry.default_peer.is_none());
}

#[test]
fn load_peer_registry_rejects_non_object_json() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("peers.json");
    fs::write(&path, "[1,2,3]").expect("write fixture");
    let err = load_peer_registry(Some(path.to_string_lossy().as_ref()))
        .expect_err("array-shaped registry file must be rejected");
    assert!(err.to_string().contains("must be a JSON object"), "{err}");
}

#[test]
fn load_peer_registry_rejects_malformed_json() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("peers.json");
    fs::write(&path, "{not valid json").expect("write fixture");
    let err = load_peer_registry(Some(path.to_string_lossy().as_ref()))
        .expect_err("malformed JSON must be rejected");
    assert!(err.to_string().contains("parse A2A peer registry"), "{err}");
}

#[test]
fn load_peer_registry_rejects_entry_missing_url() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("peers.json");
    fs::write(&path, r#"{"peers":{"bad":{"displayName":"Bad"}}}"#).expect("write fixture");
    let err = load_peer_registry(Some(path.to_string_lossy().as_ref()))
        .expect_err("entry without a url must be rejected");
    assert!(err.to_string().contains("url is required"), "{err}");
}

#[test]
fn save_then_load_round_trips_a_peer() {
    let dir = tempdir().expect("tempdir");
    let path = registry_path(&dir);
    let payload = sample_payload("https://peer.example.com/a2a");
    let result = upsert_peer_from_pairing_payload(&payload, upsert_options(&path, None, false))
        .expect("upsert");
    assert_eq!(result.name, "peer");

    let (_, registry) = load_peer_registry(Some(path.as_str())).expect("load");
    // The first peer registered always becomes the default, even without
    // `make_default`.
    assert_eq!(registry.default_peer.as_deref(), Some("peer"));
    let entry = registry.peers.get("peer").expect("entry present");
    assert_eq!(entry.url, "https://peer.example.com/a2a");
    assert_eq!(entry.display_name.as_deref(), Some("Peer One"));
}

#[test]
fn upsert_configures_remote_session_id_for_named_peer() {
    let dir = tempdir().expect("tempdir");
    let path = registry_path(&dir);
    let payload = sample_payload("https://peer.example.com/a2a");
    let mut options = upsert_options(&path, None, false);
    options.name = Some("chief".into());
    options.session_id = Some(" remote-session-1 ".into());

    upsert_peer_from_pairing_payload(&payload, options).expect("upsert");

    let (_, registry) = load_peer_registry(Some(path.as_str())).expect("load");
    assert_eq!(
        registry
            .peers
            .get("chief")
            .and_then(|entry| entry.session_id.as_deref()),
        Some("remote-session-1")
    );
}

#[test]
fn normalize_peer_name_accepts_and_rejects_expected_inputs() {
    assert!(normalize_peer_name("valid-name_1.2").is_ok());
    assert!(normalize_peer_name("has space").is_err());
    assert!(normalize_peer_name("").is_err());
    assert!(normalize_peer_name("   ").is_err());
    assert!(normalize_peer_name(&"x".repeat(81)).is_err());
    assert!(normalize_peer_name(&"x".repeat(80)).is_ok());
}

#[test]
fn resolve_peer_errors_when_no_name_and_no_default() {
    let dir = tempdir().expect("tempdir");
    let path = registry_path(&dir);
    let err = resolve_peer(
        None,
        ResolvePeerOptions {
            registry_path: Some(path),
            timeout_ms: None,
            token: None,
            max_attempts: None,
        },
    )
    .expect_err("empty registry with no explicit name must fail");
    assert!(
        err.to_string().contains("A2A peer name is required"),
        "{err}"
    );
}

#[test]
fn resolve_peer_errors_for_unknown_peer_name() {
    let dir = tempdir().expect("tempdir");
    let path = registry_path(&dir);
    let payload = sample_payload("https://peer.example.com/a2a");
    upsert_peer_from_pairing_payload(&payload, upsert_options(&path, None, true)).expect("upsert");

    let err = resolve_peer(
        Some("missing"),
        ResolvePeerOptions {
            registry_path: Some(path),
            timeout_ms: None,
            token: None,
            max_attempts: None,
        },
    )
    .expect_err("unknown peer name must fail");
    assert!(err.to_string().contains("Unknown A2A peer"), "{err}");
}

#[test]
fn resolve_peer_falls_back_to_registry_default() {
    let dir = tempdir().expect("tempdir");
    let path = registry_path(&dir);
    let payload = sample_payload("https://peer.example.com/a2a");
    upsert_peer_from_pairing_payload(&payload, upsert_options(&path, None, true)).expect("upsert");

    let resolved = resolve_peer(
        None,
        ResolvePeerOptions {
            registry_path: Some(path),
            timeout_ms: None,
            token: None,
            max_attempts: None,
        },
    )
    .expect("must resolve the default peer");
    assert_eq!(resolved.name, "peer");
    assert_eq!(resolved.config.base_url, "https://peer.example.com/a2a");
}

/// Security-relevant state transition: a bearer token minted for one peer
/// identity must not silently carry over once the peer is re-paired against
/// a *different* transport host, but should be retained across a re-pair to
/// the same host so operators are not forced to re-supply `--token-env` on
/// every routine refresh.
#[test]
fn upsert_retains_token_when_peer_identity_is_unchanged() {
    let dir = tempdir().expect("tempdir");
    let path = registry_path(&dir);
    let payload = sample_payload("https://peer.example.com/a2a");
    upsert_peer_from_pairing_payload(&payload, upsert_options(&path, Some("PEER_TOKEN"), true))
        .expect("first upsert");

    // Re-pair against the same transport URL without supplying new token
    // options.
    let payload_again = sample_payload("https://peer.example.com/a2a");
    let result =
        upsert_peer_from_pairing_payload(&payload_again, upsert_options(&path, None, false))
            .expect("second upsert");
    assert_eq!(result.entry.token_env.as_deref(), Some("PEER_TOKEN"));
}

#[test]
fn upsert_clears_token_when_peer_identity_changes() {
    let dir = tempdir().expect("tempdir");
    let path = registry_path(&dir);
    let payload = sample_payload("https://peer-a.example.com/a2a");
    upsert_peer_from_pairing_payload(&payload, upsert_options(&path, Some("PEER_TOKEN"), true))
        .expect("first upsert");

    // Re-pair the same peer name to a *different* transport host.
    let payload_new_host = sample_payload("https://peer-b.example.com/a2a");
    let result =
        upsert_peer_from_pairing_payload(&payload_new_host, upsert_options(&path, None, false))
            .expect("second upsert");
    assert_eq!(
        result.entry.token_env, None,
        "a token minted for the old host must not carry over to a new host"
    );
    assert_eq!(result.entry.url, "https://peer-b.example.com/a2a");
}
