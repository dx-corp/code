//! OAuth 2.1 + PKCE for user-configured HTTP MCP servers.
//!
//! Provider tokens live in the operating-system credential store and are
//! bound to the exact configured server URL. MCP config carries only the
//! non-secret `authPreset: "oauth"` marker.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use reqwest::header::WWW_AUTHENTICATE;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::Url;
use zeroize::Zeroizing;

use super::{McpError, McpServerConfig, McpTransport};

const KEYRING_SERVICE: &str = "maestro-mcp-oauth";

#[derive(Debug, Deserialize)]
struct ProtectedResourceMetadata {
    #[serde(default)]
    authorization_servers: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct AuthorizationServerMetadata {
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ClientRegistration {
    client_id: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    scope: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredCredential {
    server_url: String,
    access_token: String,
    refresh_token: Option<String>,
    expires_at: Option<u64>,
    token_endpoint: String,
    client_id: String,
    scope: Option<String>,
}

pub async fn login(
    config: &McpServerConfig,
    explicit_client_id: Option<&str>,
    requested_scopes: &[String],
) -> Result<()> {
    login_with_output(config, explicit_client_id, requested_scopes, true).await
}

pub async fn login_quiet(
    config: &McpServerConfig,
    explicit_client_id: Option<&str>,
    requested_scopes: &[String],
) -> Result<()> {
    login_with_output(config, explicit_client_id, requested_scopes, false).await
}

async fn login_with_output(
    config: &McpServerConfig,
    explicit_client_id: Option<&str>,
    requested_scopes: &[String],
    announce_url: bool,
) -> Result<()> {
    if !matches!(config.transport, McpTransport::Http | McpTransport::Sse) {
        bail!("OAuth is available only for HTTP and SSE MCP servers");
    }
    let server_url = config
        .url
        .as_deref()
        .context("HTTP MCP server is missing its URL")?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .context("build OAuth client")?;
    let resource_metadata_url = discover_resource_metadata_url(&client, server_url).await?;
    require_secure_endpoint(&resource_metadata_url, "protected-resource metadata")?;
    let resource: ProtectedResourceMetadata = client
        .get(resource_metadata_url.clone())
        .send()
        .await
        .context("load MCP protected-resource metadata")?
        .error_for_status()
        .context("MCP protected-resource metadata request failed")?
        .json()
        .await
        .context("parse MCP protected-resource metadata")?;
    let issuer = resource
        .authorization_servers
        .first()
        .context("MCP resource metadata did not advertise an authorization server")?;
    let metadata_url = authorization_metadata_url(issuer)?;
    require_secure_endpoint(&metadata_url, "authorization-server metadata")?;
    let metadata: AuthorizationServerMetadata = client
        .get(metadata_url)
        .send()
        .await
        .context("load OAuth authorization-server metadata")?
        .error_for_status()
        .context("OAuth authorization-server metadata request failed")?
        .json()
        .await
        .context("parse OAuth authorization-server metadata")?;
    let authorization_endpoint = Url::parse(&metadata.authorization_endpoint)
        .context("invalid OAuth authorization endpoint")?;
    require_secure_endpoint(&authorization_endpoint, "authorization")?;
    let token_endpoint =
        Url::parse(&metadata.token_endpoint).context("invalid OAuth token endpoint")?;
    require_secure_endpoint(&token_endpoint, "token")?;
    if let Some(endpoint) = metadata.registration_endpoint.as_deref() {
        let endpoint = Url::parse(endpoint).context("invalid OAuth registration endpoint")?;
        require_secure_endpoint(&endpoint, "registration")?;
    }

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("bind local OAuth callback")?;
    let callback = format!(
        "http://127.0.0.1:{}/callback",
        listener.local_addr()?.port()
    );
    let client_id = match explicit_client_id {
        Some(client_id) => client_id.to_string(),
        None => {
            let endpoint = metadata.registration_endpoint.as_deref().context(
                "authorization server does not support dynamic registration; pass --client-id",
            )?;
            client
                .post(endpoint)
                .json(&serde_json::json!({
                    "client_name": "Maestro",
                    "redirect_uris": [callback],
                    "grant_types": ["authorization_code", "refresh_token"],
                    "response_types": ["code"],
                    "token_endpoint_auth_method": "none"
                }))
                .send()
                .await
                .context("register MCP OAuth client")?
                .error_for_status()
                .context("MCP OAuth client registration failed")?
                .json::<ClientRegistration>()
                .await
                .context("parse MCP OAuth client registration")?
                .client_id
        }
    };

    let verifier = random_url_token(48);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    let state = random_url_token(24);
    let scopes = if requested_scopes.is_empty() {
        metadata.scopes_supported.clone()
    } else {
        requested_scopes.to_vec()
    };
    let mut authorization = authorization_endpoint;
    authorization
        .query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &client_id)
        .append_pair("redirect_uri", &callback)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("resource", server_url);
    if !scopes.is_empty() {
        authorization
            .query_pairs_mut()
            .append_pair("scope", &scopes.join(" "));
    }
    if announce_url {
        println!("Open this URL in your browser to authenticate:\n{authorization}");
    }
    if !open_browser(authorization.as_str()) && !announce_url {
        bail!(
            "could not open a browser; run `maestro mcp auth {}` from a terminal",
            config.name
        );
    }
    let code = await_callback(listener, &state).await?;
    let token = client
        .post(&metadata.token_endpoint)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", callback.as_str()),
            ("client_id", client_id.as_str()),
            ("code_verifier", verifier.as_str()),
            ("resource", server_url),
        ])
        .send()
        .await
        .context("exchange MCP OAuth authorization code")?
        .error_for_status()
        .context("MCP OAuth token exchange failed")?
        .json::<TokenResponse>()
        .await
        .context("parse MCP OAuth token response")?;
    save_credential(
        &config.name,
        &StoredCredential {
            server_url: server_url.to_string(),
            access_token: token.access_token,
            refresh_token: token.refresh_token,
            expires_at: token
                .expires_in
                .map(|ttl| now_seconds().saturating_add(ttl)),
            token_endpoint: metadata.token_endpoint,
            client_id,
            scope: token
                .scope
                .or_else(|| (!scopes.is_empty()).then(|| scopes.join(" "))),
        },
    )
}

pub(crate) async fn bearer_for(
    config: &McpServerConfig,
) -> Result<Option<Zeroizing<String>>, McpError> {
    if config.auth_preset.as_deref() != Some("oauth") {
        return Ok(None);
    }
    let expected_url = config.url.as_deref().ok_or_else(|| {
        McpError::ConnectionFailed("OAuth MCP server is missing its URL".to_string())
    })?;
    let mut credential = load_credential(&config.name)
        .map_err(|error| McpError::ConnectionFailed(error.to_string()))?
        .ok_or_else(|| {
            McpError::ConnectionFailed(format!(
                "OAuth authentication required; run maestro mcp auth {}",
                config.name
            ))
        })?;
    if credential.server_url != expected_url {
        return Err(McpError::ConnectionFailed(
            "stored OAuth credential does not match the configured MCP endpoint; authenticate again"
                .to_string(),
        ));
    }
    let expired = credential
        .expires_at
        .is_some_and(|expiry| expiry <= now_seconds().saturating_add(30));
    if expired {
        refresh_credential(config, &mut credential).await?;
    }
    Ok(Some(Zeroizing::new(credential.access_token)))
}

pub fn clear(name: &str) -> Result<bool> {
    let entry = crate::native_credentials::entry(KEYRING_SERVICE, name)
        .context("OS credential store is unavailable")?;
    match entry.delete_credential() {
        Ok(()) => Ok(true),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(error) => Err(error).context("delete MCP OAuth credential"),
    }
}

fn load_credential(name: &str) -> Result<Option<StoredCredential>> {
    let entry = crate::native_credentials::entry(KEYRING_SERVICE, name)
        .context("OS credential store is unavailable")?;
    match entry.get_password() {
        Ok(encoded) => serde_json::from_str(&encoded)
            .context("decode MCP OAuth credential")
            .map(Some),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(error).context("read MCP OAuth credential"),
    }
}

fn save_credential(name: &str, credential: &StoredCredential) -> Result<()> {
    let encoded = Zeroizing::new(serde_json::to_string(credential)?);
    crate::native_credentials::entry(KEYRING_SERVICE, name)
        .context("OS credential store is unavailable")?
        .set_password(encoded.as_str())
        .context("store MCP OAuth credential")
}

async fn refresh_credential(
    config: &McpServerConfig,
    credential: &mut StoredCredential,
) -> Result<(), McpError> {
    let refresh = credential.refresh_token.as_deref().ok_or_else(|| {
        McpError::ConnectionFailed(format!(
            "MCP OAuth session expired; run maestro mcp auth {}",
            config.name
        ))
    })?;
    let token = reqwest::Client::new()
        .post(&credential.token_endpoint)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("client_id", credential.client_id.as_str()),
            ("resource", credential.server_url.as_str()),
        ])
        .send()
        .await
        .map_err(|error| McpError::ConnectionFailed(format!("OAuth refresh failed: {error}")))?
        .error_for_status()
        .map_err(|error| McpError::ConnectionFailed(format!("OAuth refresh failed: {error}")))?
        .json::<TokenResponse>()
        .await
        .map_err(|error| McpError::ConnectionFailed(format!("invalid OAuth refresh: {error}")))?;
    credential.access_token = token.access_token;
    if token.refresh_token.is_some() {
        credential.refresh_token = token.refresh_token;
    }
    credential.expires_at = token
        .expires_in
        .map(|ttl| now_seconds().saturating_add(ttl));
    if token.scope.is_some() {
        credential.scope = token.scope;
    }
    save_credential(&config.name, credential)
        .map_err(|error| McpError::ConnectionFailed(error.to_string()))
}

async fn discover_resource_metadata_url(client: &reqwest::Client, server_url: &str) -> Result<Url> {
    let response = client
        .get(server_url)
        .send()
        .await
        .context("probe MCP server for OAuth metadata")?;
    if let Some(value) = response.headers().get(WWW_AUTHENTICATE) {
        if let Ok(value) = value.to_str() {
            if let Some(url) = quoted_parameter(value, "resource_metadata") {
                return Url::parse(url).context("invalid OAuth resource_metadata URL");
            }
        }
    }
    let mut url = Url::parse(server_url).context("invalid MCP server URL")?;
    url.set_path("/.well-known/oauth-protected-resource");
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

fn authorization_metadata_url(issuer: &str) -> Result<Url> {
    let mut url = Url::parse(issuer).context("invalid OAuth authorization server URL")?;
    let issuer_path = url.path().trim_matches('/');
    let metadata_path = if issuer_path.is_empty() {
        "/.well-known/oauth-authorization-server".to_string()
    } else {
        format!("/.well-known/oauth-authorization-server/{issuer_path}")
    };
    url.set_path(&metadata_path);
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

fn require_secure_endpoint(url: &Url, kind: &str) -> Result<()> {
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
    if url.scheme() == "https" || (url.scheme() == "http" && loopback) {
        return Ok(());
    }
    bail!("OAuth {kind} endpoint must use HTTPS (HTTP is allowed only for loopback hosts)")
}

fn quoted_parameter<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    let value = header.split_once(&format!("{name}=\""))?.1;
    value.split_once('"').map(|(value, _)| value)
}

async fn await_callback(listener: TcpListener, expected_state: &str) -> Result<String> {
    let (mut stream, _) = tokio::time::timeout(Duration::from_mins(5), listener.accept())
        .await
        .context("timed out waiting for OAuth callback")??;
    let mut bytes = vec![0_u8; 8192];
    let count = stream.read(&mut bytes).await?;
    let request = String::from_utf8_lossy(&bytes[..count]);
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .context("invalid OAuth callback")?;
    let url = Url::parse(&format!("http://127.0.0.1{target}"))?;
    let state = url
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .context("OAuth callback omitted state")?;
    if state != expected_state {
        bail!("OAuth callback state did not match");
    }
    let code = url
        .query_pairs()
        .find(|(key, _)| key == "code")
        .map(|(_, value)| value.into_owned())
        .context("OAuth callback omitted authorization code")?;
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 44\r\nConnection: close\r\n\r\nAuthentication complete. Return to Maestro.\n",
        )
        .await?;
    Ok(code)
}

fn random_url_token(bytes: usize) -> String {
    let mut random = vec![0_u8; bytes];
    rand::fill(&mut random);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random)
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn open_browser(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    let command = ("open", vec![url]);
    #[cfg(target_os = "linux")]
    let command = ("xdg-open", vec![url]);
    #[cfg(target_os = "windows")]
    let command = ("cmd", vec!["/C", "start", "", url]);
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let command: (&str, Vec<&str>) = ("", Vec::new());
    !command.0.is_empty()
        && std::process::Command::new(command.0)
            .args(command.1)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_resource_metadata_challenge() {
        assert_eq!(
            quoted_parameter(
                "Bearer realm=\"mcp\", resource_metadata=\"https://example.test/resource\"",
                "resource_metadata"
            ),
            Some("https://example.test/resource")
        );
    }

    #[test]
    fn authorization_metadata_preserves_issuer_path() {
        assert_eq!(
            authorization_metadata_url("https://example.test/tenant")
                .unwrap()
                .as_str(),
            "https://example.test/.well-known/oauth-authorization-server/tenant"
        );
    }

    #[test]
    fn oauth_endpoints_require_https_except_loopback() {
        assert!(
            require_secure_endpoint(&Url::parse("https://example.test/token").unwrap(), "token")
                .is_ok()
        );
        assert!(
            require_secure_endpoint(&Url::parse("http://127.0.0.1/token").unwrap(), "token")
                .is_ok()
        );
        assert!(
            require_secure_endpoint(&Url::parse("http://example.test/token").unwrap(), "token")
                .is_err()
        );
    }
}
