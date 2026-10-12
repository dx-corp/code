//! Org-scoped provider refs in Platform mode.
//!
//! Secrets are uploaded to `keys` and are not stored locally. Maestro keeps
//! only the non-secret selection tuple so `connections list` and `connections
//! use` can choose which ref `llm-gateway` should resolve.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::credential_mode::{CREDENTIAL_NAME_ENV, ENVIRONMENT_ENV, PlatformSession};
use crate::init_cli::EvalOpsCredentialSnapshot;
use crate::path_utils;

const DEFAULT_KEYS_URL: &str = "https://keys.evalops.dev";
const STORE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StoredProviderRef {
    pub id: String,
    pub provider: String,
    pub environment: String,
    pub credential_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
    pub is_default: bool,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

impl StoredProviderRef {
    pub fn selection_value(&self) -> Value {
        let mut value = json!({
            "provider": self.provider,
            "environment": self.environment,
            "credential_name": self.credential_name,
        });
        if let Some(team_id) = &self.team_id {
            value["team_id"] = json!(team_id);
        }
        value
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderRefStore {
    #[serde(default = "store_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub refs: Vec<StoredProviderRef>,
}

impl Default for ProviderRefStore {
    fn default() -> Self {
        Self {
            schema_version: STORE_SCHEMA_VERSION,
            refs: Vec::new(),
        }
    }
}

const fn store_schema_version() -> u32 {
    STORE_SCHEMA_VERSION
}

impl ProviderRefStore {
    pub fn default_path() -> Result<PathBuf> {
        Ok(path_utils::maestro_home_dir()
            .context("could not resolve Maestro home")?
            .join("provider-refs.json"))
    }

    pub fn load(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).context("invalid provider-refs.json"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if self.schema_version != STORE_SCHEMA_VERSION {
            bail!("unsupported provider-refs.json schema version");
        }
        path_utils::atomic_private_write(path, &serde_json::to_vec_pretty(self)?)
    }

    pub fn get(&self, id: &str) -> Option<&StoredProviderRef> {
        self.refs.iter().find(|item| item.id == id)
    }

    pub fn upsert(&mut self, stored: StoredProviderRef) -> Result<()> {
        if stored.is_default {
            for existing in &mut self.refs {
                if existing.provider == stored.provider {
                    existing.is_default = false;
                }
            }
        }
        if let Some(existing) = self.refs.iter_mut().find(|item| item.id == stored.id) {
            *existing = stored;
        } else {
            self.refs.push(stored);
        }
        self.refs.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(())
    }

    pub fn set_default(&mut self, id: &str) -> Result<&StoredProviderRef> {
        let provider = self
            .get(id)
            .with_context(|| format!("provider ref not found: {id}"))?
            .provider
            .clone();
        for item in &mut self.refs {
            item.is_default = item.id == id && item.provider == provider;
        }
        self.get(id)
            .ok_or_else(|| anyhow::anyhow!("provider ref not found: {id}"))
    }

    pub fn remove(&mut self, id: &str) -> Option<StoredProviderRef> {
        let index = self.refs.iter().position(|item| item.id == id)?;
        let removed = self.refs.remove(index);
        if removed.is_default {
            if let Some(next) = self
                .refs
                .iter_mut()
                .find(|item| item.provider == removed.provider)
            {
                next.is_default = true;
            }
        }
        Some(removed)
    }
}

pub fn keys_base_url() -> String {
    std::env::var("MAESTRO_KEYS_URL")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_KEYS_URL.to_owned())
}

pub fn load_default_store() -> Result<ProviderRefStore> {
    ProviderRefStore::load(&ProviderRefStore::default_path()?)
}

#[derive(Debug, Clone)]
pub struct UpsertRequest {
    pub id: String,
    pub provider: String,
    pub environment: String,
    pub credential_name: String,
    pub team_id: Option<String>,
    pub api_key: String,
    pub make_default: bool,
}

/// Request body for `POST /v1/provider-refs` on Platform `keys`.
/// Field names match `keys.v1.UpsertProviderRefRequest` JSON tags.
pub fn upsert_provider_ref_body(request: &UpsertRequest) -> Value {
    json!({
        "provider": request.provider,
        "environment": request.environment,
        "credential_name": request.credential_name,
        "team_id": request.team_id,
        "credential_data": { "api_key": request.api_key },
    })
}

/// Request body for resolve/delete. Field names match
/// `keys.v1.ResolveProviderRefRequest` / `DeleteProviderRefRequest`.
pub fn resolve_provider_ref_body(stored: &StoredProviderRef) -> Value {
    json!({
        "provider": stored.provider,
        "environment": stored.environment,
        "credential_name": stored.credential_name,
        "team_id": stored.team_id,
    })
}

pub fn upsert_org_provider_ref(
    session: &PlatformSession,
    request: UpsertRequest,
) -> Result<StoredProviderRef> {
    let body = upsert_provider_ref_body(&request);
    let _response = keys_request(
        session,
        reqwest::Method::POST,
        "/v1/provider-refs",
        Some(body),
    )?;
    persist_selection(session, &request)
}

pub fn check_org_provider_ref(
    session: &PlatformSession,
    stored: &StoredProviderRef,
) -> Result<Value> {
    keys_request(
        session,
        reqwest::Method::POST,
        "/v1/provider-refs/resolve",
        Some(resolve_provider_ref_body(stored)),
    )
}

pub fn delete_org_provider_ref(
    session: &PlatformSession,
    stored: &StoredProviderRef,
) -> Result<()> {
    let _ = keys_request(
        session,
        reqwest::Method::DELETE,
        "/v1/provider-refs",
        Some(resolve_provider_ref_body(stored)),
    )?;
    let path = ProviderRefStore::default_path()?;
    let mut store = ProviderRefStore::load(&path)?;
    store.remove(&stored.id);
    store.save(&path)
}

fn persist_selection(
    session: &PlatformSession,
    request: &UpsertRequest,
) -> Result<StoredProviderRef> {
    let now = now_ms();
    let stored = StoredProviderRef {
        id: request.id.clone(),
        provider: request.provider.clone(),
        environment: request.environment.clone(),
        credential_name: request.credential_name.clone(),
        team_id: request.team_id.clone(),
        is_default: request.make_default,
        created_at_ms: now,
        updated_at_ms: now,
    };
    let path = ProviderRefStore::default_path()?;
    let mut store = ProviderRefStore::load(&path)?;
    store.upsert(stored.clone())?;
    store.save(&path)?;
    crate::init_cli::store_evalops_provider_ref(stored.selection_value())?;
    let _ = session;
    Ok(stored)
}

pub fn select_default(id: &str) -> Result<StoredProviderRef> {
    let path = ProviderRefStore::default_path()?;
    let mut store = ProviderRefStore::load(&path)?;
    let selected = store.set_default(id)?.clone();
    store.save(&path)?;
    crate::init_cli::store_evalops_provider_ref(selected.selection_value())?;
    Ok(selected)
}

pub fn keys_request(
    session: &PlatformSession,
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
) -> Result<Value> {
    let url = format!("{}{path}", keys_base_url());
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .context("create keys HTTP client")?;
    let mut request = client
        .request(method, url)
        .bearer_auth(&session.access_token)
        .header("X-Organization-ID", &session.organization_id)
        .header("accept", "application/json");
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().context("call keys provider-refs")?;
    let status = response.status();
    let text = response.text().unwrap_or_default();
    if !status.is_success() {
        bail!(
            "keys {path} failed (HTTP {}): {}",
            status.as_u16(),
            redact_keys_error(&text)
        );
    }
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    let mut value: Value =
        serde_json::from_str(&text).context("parse keys provider-ref response")?;
    redact_credential_data(&mut value);
    Ok(value)
}

fn redact_credential_data(value: &mut Value) {
    if let Some(object) = value.as_object_mut() {
        if let Some(data) = object.get_mut("credential_data") {
            *data = json!({ "redacted": true });
        }
        if let Some(data) = object.get_mut("credentialData") {
            *data = json!({ "redacted": true });
        }
        for child in object.values_mut() {
            redact_credential_data(child);
        }
    }
}

fn redact_keys_error(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.len() > 240 {
        format!("{}…", &trimmed[..240])
    } else if trimmed.is_empty() {
        "no response body".to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// Summaries for `maestro connections list` in Platform mode.
pub fn list_summaries(
    snapshot: Option<&EvalOpsCredentialSnapshot>,
    env: &BTreeMap<String, String>,
) -> Result<Vec<Value>> {
    let env_map = env
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let Some(_session) = crate::credential_mode::platform_session_from(snapshot, &env_map) else {
        return Ok(Vec::new());
    };
    let store = load_default_store().unwrap_or_default();
    Ok(store
        .refs
        .iter()
        .map(|item| {
            json!({
                "id": item.id,
                "provider": item.provider,
                "environment": item.environment,
                "credentialName": item.credential_name,
                "teamId": item.team_id,
                "default": item.is_default,
                "placement": "platform",
            })
        })
        .collect())
}

pub fn default_environment() -> String {
    std::env::var(ENVIRONMENT_ENV)
        .ok()
        .map(|value| crate::ai::canonical_managed_environment(Some(value.as_str())))
        .unwrap_or_else(|| crate::ai::canonical_managed_environment(None))
}

pub fn default_credential_name(id: &str) -> String {
    std::env::var(CREDENTIAL_NAME_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            if id.trim().is_empty() {
                crate::ai::canonical_managed_credential_name(None)
            } else {
                id.to_owned()
            }
        })
}

/// Canonical Maestro → Platform `keys` upsert JSON. Kept as a string so
/// `evalops/platform` can decode the same payload in
/// `internal/keys/http/maestro_upsert_contract_test.go`.
pub const MAESTRO_KEYS_UPSERT_CONTRACT_JSON: &str = r#"{
  "provider": "anthropic",
  "environment": "production",
  "credential_name": "default",
  "team_id": null,
  "credential_data": {
    "api_key": "sk-test"
  }
}"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential_mode::PlatformSession;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread;

    static KEYS_URL_LOCK: Mutex<()> = Mutex::new(());

    fn sample_request() -> UpsertRequest {
        UpsertRequest {
            id: "work".to_owned(),
            provider: "anthropic".to_owned(),
            environment: "production".to_owned(),
            credential_name: "default".to_owned(),
            team_id: None,
            api_key: "sk-test".to_owned(),
            make_default: true,
        }
    }

    fn sample_session() -> PlatformSession {
        PlatformSession {
            access_token: "identity-access".to_owned(),
            organization_id: "org_evalops".to_owned(),
            workspace_id: Some("ws_1".to_owned()),
            provider_ref: serde_json::json!({
                "provider": "anthropic",
                "environment": "production",
                "credential_name": "default"
            }),
            email: None,
            user_id: None,
        }
    }

    #[test]
    fn upsert_body_matches_keys_proto_json_tags() {
        let body = upsert_provider_ref_body(&sample_request());
        let contract: Value =
            serde_json::from_str(MAESTRO_KEYS_UPSERT_CONTRACT_JSON).expect("contract json");
        assert_eq!(body, contract);
        assert!(body.get("credentialName").is_none());
        assert!(body["credential_data"].get("apiKey").is_none());
    }

    #[test]
    fn resolve_body_matches_keys_proto_json_tags() {
        let stored = StoredProviderRef {
            id: "work".to_owned(),
            provider: "anthropic".to_owned(),
            environment: "production".to_owned(),
            credential_name: "default".to_owned(),
            team_id: None,
            is_default: true,
            created_at_ms: 1,
            updated_at_ms: 1,
        };
        let body = resolve_provider_ref_body(&stored);
        assert_eq!(body["provider"], "anthropic");
        assert_eq!(body["environment"], "production");
        assert_eq!(body["credential_name"], "default");
        assert!(body.get("credentialName").is_none());
    }

    #[test]
    fn keys_request_sends_identity_bearer_and_org_header() {
        let _guard = KEYS_URL_LOCK.lock().expect("lock keys url");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let seen = Arc::new(Mutex::new(String::new()));
        let seen_for_thread = Arc::clone(&seen);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0_u8; 4096];
            let n = stream.read(&mut buf).expect("read");
            *seen_for_thread.lock().expect("lock") =
                String::from_utf8_lossy(&buf[..n]).into_owned();
            stream
                .write_all(b"HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}")
                .expect("write");
        });

        let previous_url = std::env::var("MAESTRO_KEYS_URL").ok();
        std::env::set_var("MAESTRO_KEYS_URL", format!("http://{addr}"));
        let result = keys_request(
            &sample_session(),
            reqwest::Method::POST,
            "/v1/provider-refs",
            Some(upsert_provider_ref_body(&sample_request())),
        );
        if let Some(previous) = previous_url {
            std::env::set_var("MAESTRO_KEYS_URL", previous);
        } else {
            std::env::remove_var("MAESTRO_KEYS_URL");
        }
        result.expect("keys request");
        server.join().expect("server");
        let request = seen.lock().expect("lock").clone();
        let request_lower = request.to_ascii_lowercase();
        assert!(
            request.starts_with("POST /v1/provider-refs "),
            "path: {request}"
        );
        assert!(
            request_lower.contains("authorization: bearer identity-access"),
            "auth header: {request}"
        );
        assert!(
            request_lower.contains("x-organization-id: org_evalops"),
            "org header: {request}"
        );
        assert!(
            request.contains("\"credential_name\":\"default\""),
            "body: {request}"
        );
        assert!(
            request.contains("\"api_key\":\"sk-test\""),
            "credential: {request}"
        );
        assert!(
            !request.contains("ANTHROPIC_API_KEY"),
            "must not send local env names: {request}"
        );
    }

    #[test]
    fn keys_response_redacts_credential_data() {
        let _guard = KEYS_URL_LOCK.lock().expect("lock keys url");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0_u8; 2048];
            let _ = stream.read(&mut buf);
            let body = br#"{"provider":"anthropic","credential_data":{"api_key":"sk-live"}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                std::str::from_utf8(body).expect("utf8")
            );
            stream.write_all(response.as_bytes()).expect("write");
        });
        let previous_url = std::env::var("MAESTRO_KEYS_URL").ok();
        std::env::set_var("MAESTRO_KEYS_URL", format!("http://{addr}"));
        let value = keys_request(
            &sample_session(),
            reqwest::Method::POST,
            "/v1/provider-refs/resolve",
            Some(json!({})),
        );
        if let Some(previous) = previous_url {
            std::env::set_var("MAESTRO_KEYS_URL", previous);
        } else {
            std::env::remove_var("MAESTRO_KEYS_URL");
        }
        let value = value.expect("keys request");
        server.join().expect("server");
        assert_eq!(value["credential_data"], json!({ "redacted": true }));
        assert!(
            serde_json::to_string(&value)
                .unwrap()
                .find("sk-live")
                .is_none()
        );
    }
}
