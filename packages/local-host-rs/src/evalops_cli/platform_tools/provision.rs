use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use url::Url;
use uuid::Uuid;

use crate::init_cli::load_evalops_snapshot;

const CALLBACK_PORT: u16 = 1461;
const CALLBACK_PATH: &str = "/auth/callback/evalops-platform-tools";
const REQUIRED_SCOPES: &[&str] = &["tool-execution:read", "tool-execution:write"];
const CREDENTIAL_SCHEMA: &str = "evalops.maestro.platform-tools-credential.v1";
const CREDENTIAL_FILE: &str = "platform-tools.json";
const DEFAULT_IDENTITY_URL: &str = "https://identity.deixic.com";
const MAX_CALLBACK_HEADER_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ProvisionedCredential {
    schema: String,
    pub(super) api_key: String,
    key_id: String,
    identity_base_url: String,
    organization_id: String,
    created_at: String,
    scopes: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct OAuthClientRegistration {
    client_id: String,
}

#[derive(Debug, Deserialize)]
struct OAuthTokenExchange {
    access_token: String,
    organization_id: String,
}

struct CallbackResult {
    code: String,
}

pub(super) async fn ensure_provisioned_credential(force_rotate: bool) -> Result<Option<PathBuf>> {
    if explicit_service_token_present() {
        return Ok(None);
    }
    if !force_rotate {
        if let Ok(Some(_)) = load_provisioned_credential() {
            return Ok(Some(credential_path()?));
        }
    } else if credential_path()?.exists() {
        revoke_provisioned_credential().await?;
    }

    let credential = provision_credential().await?;
    let path = credential_path()?;
    crate::path_utils::atomic_private_write(&path, &serde_json::to_vec_pretty(&credential)?)?;
    Ok(Some(path))
}

pub(super) fn load_provisioned_credential() -> Result<Option<ProvisionedCredential>> {
    let path = credential_path()?;
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let credential: ProvisionedCredential =
                serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
            validate_credential(&credential)?;
            Ok(Some(credential))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(super) async fn revoke_provisioned_credential() -> Result<()> {
    let path = credential_path()?;
    let credential = match load_provisioned_credential() {
        Ok(credential) => credential,
        Err(error) => {
            eprintln!(
                "Ignoring unusable Platform tools credential at {} during unconfigure: {error}",
                path.display()
            );
            None
        }
    };
    let Some(credential) = credential else {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        };
    };
    let snapshot = load_evalops_snapshot()?
        .context("stored EvalOps login is required to revoke the Platform tools key")?;
    let url = api_key_url(&credential.identity_base_url, &credential.key_id)?;
    let response = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?
        .delete(url)
        .bearer_auth(snapshot.access)
        .send()
        .await
        .context("revoke Platform tools API key")?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!(
            "Platform tools API key revocation failed (HTTP {}): {}. Run `deixic-code evalops login` and retry so the remote key is not orphaned.",
            status.as_u16(),
            response_detail(&body)
        );
    }
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn explicit_service_token_present() -> bool {
    [
        "TOOL_EXECUTION_SERVICE_TOKEN",
        "MAESTRO_TOOL_EXECUTION_SERVICE_TOKEN",
        "MAESTRO_PLATFORM_ACCESS_TOKEN",
    ]
    .iter()
    .any(|name| {
        std::env::var(name)
            .ok()
            .is_some_and(|value| !value.trim().is_empty())
    })
}

async fn provision_credential() -> Result<ProvisionedCredential> {
    let snapshot = load_evalops_snapshot()?
        .context("run `deixic-code evalops login` before provisioning Platform tools")?;
    let identity = configured_identity(snapshot.identity_base_url.as_deref())?;
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("create Platform tools provisioning client")?;
    let listener = TcpListener::bind(("127.0.0.1", CALLBACK_PORT))
        .await
        .with_context(|| format!("Port {CALLBACK_PORT} is already in use"))?;
    let callback_uri = format!("http://127.0.0.1:{CALLBACK_PORT}{CALLBACK_PATH}");
    let registration_response = client
        .post(format!("{identity}/register"))
        .json(&json!({
            "client_name": "Deixic Code Platform Tools",
            "redirect_uris": [&callback_uri],
            "grant_types": ["authorization_code"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none"
        }))
        .send()
        .await
        .context("register Platform tools OAuth client")?;
    let registration_status = registration_response.status();
    let registration_body = registration_response.text().await.unwrap_or_default();
    if !registration_status.is_success() {
        bail!(
            "Platform tools OAuth client registration failed (HTTP {}): {}",
            registration_status.as_u16(),
            response_detail(&registration_body)
        );
    }
    let registration: OAuthClientRegistration = serde_json::from_str(&registration_body)
        .context("parse Platform tools OAuth client registration")?;

    let verifier = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = Uuid::new_v4().simple().to_string();
    let authorization_url = authorization_url(
        &identity,
        &registration.client_id,
        &callback_uri,
        &challenge,
        &state,
        snapshot.organization_id.as_deref(),
    )?;
    println!("Open this URL to authorize Platform-governed tools:");
    println!("{authorization_url}");
    open_browser(authorization_url.as_str());

    let callback = tokio::time::timeout(Duration::from_mins(5), accept_callback(listener, &state))
        .await
        .context("Platform tools authorization timed out after 5 minutes")??;
    let token_body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "authorization_code")
        .append_pair("code", &callback.code)
        .append_pair("client_id", &registration.client_id)
        .append_pair("redirect_uri", &callback_uri)
        .append_pair("code_verifier", &verifier)
        .finish();
    let token_response = client
        .post(format!("{identity}/token"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(token_body)
        .send()
        .await
        .context("exchange Platform tools authorization code")?;
    let token_status = token_response.status();
    let token_body = token_response.text().await.unwrap_or_default();
    if !token_status.is_success() {
        bail!(
            "Platform tools authorization-code exchange failed (HTTP {}): {}",
            token_status.as_u16(),
            response_detail(&token_body)
        );
    }
    let token: OAuthTokenExchange =
        serde_json::from_str(&token_body).context("parse Platform tools token exchange")?;

    let key_response = client
        .post(format!("{identity}/v1/api-keys"))
        .bearer_auth(&token.access_token)
        .json(&json!({
            "name": format!("maestro-platform-tools-{}", Utc::now().format("%Y-%m-%d")),
            "scopes": REQUIRED_SCOPES,
            "rate_limit_per_minute": 120
        }))
        .send()
        .await
        .context("create least-privilege Platform tools API key")?;
    let key_status = key_response.status();
    let key_body = key_response.text().await.unwrap_or_default();
    if !key_status.is_success() {
        bail!(
            "Platform tools API key creation failed (HTTP {}): {}",
            key_status.as_u16(),
            response_detail(&key_body)
        );
    }
    let value: Value = serde_json::from_str(&key_body).context("parse Platform tools API key")?;
    let nested = value.get("key").unwrap_or(&Value::Null);
    let api_key =
        string_at(&value, "api_key").context("Platform tools API key response omitted api_key")?;
    let key_id = string_at(&value, "key_id")
        .or_else(|| string_at(nested, "id"))
        .context("Platform tools API key response omitted key_id")?;
    let scopes = string_array(value.get("scopes"))
        .or_else(|| string_array(value.get("scopes_granted")))
        .or_else(|| string_array(nested.get("scopes")))
        .unwrap_or_default();
    for required in REQUIRED_SCOPES {
        if !scopes.iter().any(|scope| scope == required) {
            bail!(
                "Identity did not grant required Platform tools scope {required}; granted scopes: {}",
                scopes.join(", ")
            );
        }
    }
    Ok(ProvisionedCredential {
        schema: CREDENTIAL_SCHEMA.to_string(),
        api_key,
        key_id,
        identity_base_url: identity,
        organization_id: token.organization_id,
        created_at: Utc::now().to_rfc3339(),
        scopes,
    })
}

fn authorization_url(
    identity: &str,
    client_id: &str,
    callback_uri: &str,
    challenge: &str,
    state: &str,
    organization_id: Option<&str>,
) -> Result<Url> {
    let mut url = Url::parse(&format!("{identity}/authorize"))?;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", client_id)
            .append_pair("redirect_uri", callback_uri)
            .append_pair("scope", &REQUIRED_SCOPES.join(" "))
            .append_pair("state", state)
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256");
        if let Some(organization_id) = organization_id {
            query.append_pair("organization_id", organization_id);
        }
    }
    Ok(url)
}

fn configured_identity(stored: Option<&str>) -> Result<String> {
    let identity = [
        "MAESTRO_IDENTITY_URL",
        "EVALOPS_IDENTITY_URL",
        "TOOL_EXECUTION_IDENTITY_URL",
    ]
    .iter()
    .find_map(|name| {
        std::env::var(name)
            .ok()
            .map(|value| value.trim().trim_end_matches('/').to_string())
            .filter(|value| !value.is_empty())
    })
    .or_else(|| stored.map(|value| value.trim().trim_end_matches('/').to_string()))
    .unwrap_or_else(|| DEFAULT_IDENTITY_URL.to_string());
    validate_identity_url(&identity)?;
    Ok(identity)
}

fn validate_identity_url(identity: &str) -> Result<Url> {
    let url = Url::parse(identity).context("invalid EvalOps Identity URL")?;
    if !url.username().is_empty() || url.password().is_some() {
        bail!("EvalOps Identity URL must not contain credentials");
    }
    if url.query().is_some() || url.fragment().is_some() {
        bail!("EvalOps Identity URL must not contain query or fragment components");
    }
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        bail!("EvalOps Identity URL must use HTTPS (loopback HTTP is allowed)");
    }
    Ok(url)
}

fn api_key_url(identity: &str, key_id: &str) -> Result<Url> {
    let mut url = validate_identity_url(identity)?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|()| anyhow::anyhow!("EvalOps Identity URL cannot be a base URL"))?;
        segments.pop_if_empty();
        segments.push("v1").push("api-keys").push(key_id);
    }
    Ok(url)
}

fn credential_path() -> Result<PathBuf> {
    let home = crate::path_utils::maestro_home_dir().context("resolve Maestro home")?;
    Ok(home.join(CREDENTIAL_FILE))
}

fn validate_credential(credential: &ProvisionedCredential) -> Result<()> {
    if credential.schema != CREDENTIAL_SCHEMA {
        bail!("unsupported Platform tools credential schema");
    }
    if credential.api_key.trim().is_empty()
        || credential.key_id.trim().is_empty()
        || credential.organization_id.trim().is_empty()
    {
        bail!("Platform tools credential is incomplete");
    }
    validate_identity_url(&credential.identity_base_url)?;
    for required in REQUIRED_SCOPES {
        if !credential.scopes.iter().any(|scope| scope == required) {
            bail!("Platform tools credential is missing required scope {required}");
        }
    }
    Ok(())
}

async fn accept_callback(listener: TcpListener, expected_state: &str) -> Result<CallbackResult> {
    loop {
        let (mut stream, _) = listener.accept().await?;
        match tokio::time::timeout(
            Duration::from_secs(5),
            read_callback(&mut stream, expected_state),
        )
        .await
        {
            Ok(Ok(Some(result))) => return Ok(result),
            Ok(Ok(None)) => continue,
            Ok(Err(error)) => return Err(error),
            Err(_) => {
                eprintln!(
                    "Ignoring Platform tools callback that did not complete within 5 seconds"
                );
                continue;
            }
        }
    }
}

async fn read_callback(
    stream: &mut TcpStream,
    expected_state: &str,
) -> Result<Option<CallbackResult>> {
    let request = read_http_head(stream).await?;
    let first = request.lines().next().unwrap_or_default();
    let target = first.split_whitespace().nth(1).unwrap_or("/");
    let host = request
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("host").then(|| value.trim())
        })
        .unwrap_or_default();
    if !callback_host_allowed(host) {
        write_http(stream, 403, "Invalid callback host.").await?;
        return Ok(None);
    }
    let url = Url::parse(&format!("http://127.0.0.1:{CALLBACK_PORT}{target}"))?;
    if url.path() != CALLBACK_PATH {
        write_http(stream, 404, "Not found.").await?;
        return Ok(None);
    }
    let query = url.query_pairs().into_owned().collect::<BTreeMap<_, _>>();
    let Some(state) = query.get("state").filter(|value| !value.is_empty()) else {
        write_http(stream, 403, "Invalid OAuth state.").await?;
        return Ok(None);
    };
    if state != expected_state {
        write_http(stream, 403, "Invalid OAuth state.").await?;
        return Ok(None);
    }
    if let Some(error) = query.get("error") {
        write_http(stream, 400, "Platform tools authorization failed.").await?;
        bail!(
            "EvalOps Identity authorization failed: {}",
            safe_detail(error)
        );
    }
    let Some(code) = query.get("code").filter(|value| !value.is_empty()).cloned() else {
        write_http(stream, 400, "Missing authorization code.").await?;
        return Ok(None);
    };
    write_http(
        stream,
        200,
        "Platform tools authorized. You can close this window and return to Deixic Code.",
    )
    .await?;
    Ok(Some(CallbackResult { code }))
}

fn callback_host_allowed(host: &str) -> bool {
    [
        format!("127.0.0.1:{CALLBACK_PORT}"),
        format!("localhost:{CALLBACK_PORT}"),
        format!("[::1]:{CALLBACK_PORT}"),
    ]
    .iter()
    .any(|allowed| host == allowed)
}

async fn read_http_head(stream: &mut TcpStream) -> Result<String> {
    let mut bytes = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];
    loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if bytes.len() > MAX_CALLBACK_HEADER_BYTES {
            bail!("Platform tools callback headers exceeded {MAX_CALLBACK_HEADER_BYTES} bytes");
        }
    }
    String::from_utf8(bytes).context("Platform tools callback was not valid UTF-8")
}

async fn write_http(stream: &mut TcpStream, status: u16, body: &str) -> Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Error",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let command = ("open", vec![url]);
    #[cfg(target_os = "linux")]
    let command = ("xdg-open", vec![url]);
    #[cfg(target_os = "windows")]
    let command = ("cmd", vec!["/C", "start", "", url]);
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let command: (&str, Vec<&str>) = ("", Vec::new());
    if command.0.is_empty() {
        return;
    }
    if let Err(error) = std::process::Command::new(command.0)
        .args(command.1)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        eprintln!("Could not open a browser automatically: {error}");
    }
}

fn response_detail(body: &str) -> String {
    let payload: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    ["error_description", "error", "message"]
        .iter()
        .find_map(|key| payload.get(key).and_then(Value::as_str))
        .map(safe_detail)
        .unwrap_or_else(|| "response body omitted".to_string())
}

fn safe_detail(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .take(300)
        .collect()
}

fn string_at(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn string_array(value: Option<&Value>) -> Option<Vec<String>> {
    let values = value?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    (!values.is_empty()).then_some(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorization_url_requests_only_tool_execution_scopes() {
        let url = authorization_url(
            "https://identity.deixic.com",
            "client-1",
            "http://127.0.0.1:1461/auth/callback/evalops-platform-tools",
            "challenge",
            "state",
            Some("org-1"),
        )
        .unwrap();
        let query = url.query_pairs().into_owned().collect::<BTreeMap<_, _>>();
        assert_eq!(
            query.get("scope").unwrap(),
            "tool-execution:read tool-execution:write"
        );
        assert_eq!(query.get("organization_id").unwrap(), "org-1");
    }

    #[test]
    fn provisioned_credential_requires_exact_scopes() {
        let mut credential = ProvisionedCredential {
            schema: CREDENTIAL_SCHEMA.to_string(),
            api_key: "pk_example".to_string(),
            key_id: "key-1".to_string(),
            identity_base_url: DEFAULT_IDENTITY_URL.to_string(),
            organization_id: "org-1".to_string(),
            created_at: Utc::now().to_rfc3339(),
            scopes: vec!["tool-execution:read".to_string()],
        };
        assert!(validate_credential(&credential).is_err());
        credential.scopes.push("tool-execution:write".to_string());
        assert!(validate_credential(&credential).is_ok());
    }

    #[test]
    fn non_loopback_identity_requires_https() {
        assert!(validate_identity_url("http://identity.example").is_err());
        assert!(validate_identity_url("http://127.0.0.1:8080").is_ok());
        assert!(validate_identity_url("https://identity.example").is_ok());
    }

    #[test]
    fn key_url_percent_encodes_the_key_identifier() {
        let url = api_key_url("https://identity.example", "key/with spaces").unwrap();
        assert_eq!(
            url.as_str(),
            "https://identity.example/v1/api-keys/key%2Fwith%20spaces"
        );
    }

    #[test]
    fn opaque_error_bodies_are_not_echoed() {
        assert_eq!(
            response_detail("secret token payload"),
            "response body omitted"
        );
        assert_eq!(
            response_detail(r#"{"error":"invalid_scope"}"#),
            "invalid_scope"
        );
    }
}
