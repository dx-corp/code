//! Native `maestro openai` OAuth credential management.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use url::Url;

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const ORIGINATOR: &str = "codex_cli_rs";
const ISSUER: &str = "https://auth.openai.com";
const CALLBACK_ORIGIN: &str = "http://127.0.0.1:1455";
const CALLBACK_ADDR: &str = "127.0.0.1:1455";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct OpenAiOAuthCredential {
    access_token: String,
    refresh_token: String,
    id_token: String,
    expires_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
    #[serde(default = "oauth_mode")]
    mode: String,
}

fn oauth_mode() -> String {
    "openai-oauth".to_owned()
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct ApiKeyExchangeResponse {
    access_token: String,
}

struct LoginRequest {
    url: Url,
    verifier: String,
    state: String,
}

pub async fn run_openai(args: &[String]) -> Result<i32> {
    match args.first().map(String::as_str) {
        Some("login") => login().await,
        Some("logout") => logout(),
        Some("status") => status().await,
        _ => {
            eprintln!("{}", crate::localization::cli_locale().format("Unknown openai subcommand. Try \"deixic-code openai login\", \"logout\", or \"status\".", &[]));
            Ok(1)
        }
    }
}

async fn login() -> Result<i32> {
    let listener = match TcpListener::bind(CALLBACK_ADDR).await {
        Ok(listener) => listener,
        Err(error) if error.kind() == ErrorKind::AddrInUse => {
            eprintln!(
                "{}",
                crate::localization::cli_locale().format(
                    "Port 1455 is already in use. Please close the other process and try again.",
                    &[]
                )
            );
            return Ok(1);
        }
        Err(error) => {
            eprintln!(
                "{}",
                crate::localization::cli_locale()
                    .format("Server error: {0}", &[(error).to_string()])
            );
            return Ok(1);
        }
    };
    let request = login_request()?;
    println!(
        "{}",
        crate::localization::cli_locale().format("Deixic Code OpenAI Login", &[])
    );
    println!(
        "{}",
        crate::localization::cli_locale().format(
            "Please open the following URL in your browser to authenticate:",
            &[]
        )
    );
    println!("{}", request.url);

    loop {
        let (mut stream, _) = listener.accept().await.context("OAuth callback failed")?;
        match read_callback(&mut stream).await {
            Ok(Callback::Other) => {
                write_response(&mut stream, 404, "text/plain", "Not Found").await?;
            }
            Ok(Callback::Auth { code, state }) => {
                if !constant_time_equal(&state, &request.state) {
                    write_response(
                        &mut stream,
                        400,
                        "text/plain",
                        "State mismatch. Possible CSRF attack.",
                    )
                    .await?;
                    return Ok(1);
                }
                let Some(code) = code else {
                    write_response(
                        &mut stream,
                        400,
                        "text/plain",
                        "Authorization code missing.",
                    )
                    .await?;
                    return Ok(1);
                };
                match complete_login(&code, &request.verifier).await {
                    Ok(()) => {
                        write_response(
                            &mut stream,
                            200,
                            "text/html",
                            "<html><body><h1>Login Successful</h1><p>You can close this tab and return to the terminal.</p></body></html>",
                        )
                        .await?;
                        println!(
                            "{}",
                            crate::localization::cli_locale()
                                .format("\nOpenAI credentials saved successfully.", &[])
                        );
                        println!("{}", crate::localization::cli_locale().format("Future runs can use --auth auto (default) or provide an OpenAI API key.", &[]));
                        return Ok(0);
                    }
                    Err(error) => {
                        eprintln!(
                            "{}",
                            crate::localization::cli_locale()
                                .format("\nLogin failed: {0}", &[format!("{:#}", error)])
                        );
                        write_response(
                            &mut stream,
                            500,
                            "text/plain",
                            "Login failed. Check terminal for details.",
                        )
                        .await?;
                        return Ok(1);
                    }
                }
            }
            Err(error) => {
                write_response(&mut stream, 400, "text/plain", "Invalid request").await?;
                eprintln!(
                    "{}",
                    crate::localization::cli_locale()
                        .format("Server error: {0}", &[format!("{:#}", error)])
                );
                return Ok(1);
            }
        }
    }
}

async fn complete_login(code: &str, verifier: &str) -> Result<()> {
    let client = Client::new();
    let Some(tokens) = exchange_authorization_code(&client, code, verifier).await? else {
        bail!(
            "{}",
            crate::localization::cli_locale().format("Failed to exchange code for tokens.", &[])
        );
    };
    let id_token = tokens
        .id_token
        .as_deref()
        .context("Failed to exchange code for tokens.")?;
    let api_key = exchange_id_token_for_api_key(&client, id_token)
        .await?
        .context("Failed to exchange ID token for API key.")?;
    let credential = OpenAiOAuthCredential {
        access_token: tokens
            .access_token
            .context("Failed to exchange code for tokens.")?,
        refresh_token: tokens
            .refresh_token
            .context("Failed to exchange code for tokens.")?,
        id_token: id_token.to_owned(),
        expires_at: now_ms() + tokens.expires_in.unwrap_or(3_600) * 1_000,
        api_key: Some(api_key),
        mode: "openai-oauth".to_owned(),
    };
    save_credential(&credential)
}

fn logout() -> Result<i32> {
    let path = auth_file();
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to remove {}", path.display()));
        }
    }
    println!(
        "{}",
        crate::localization::cli_locale().format("Removed stored OpenAI credentials.", &[])
    );
    Ok(0)
}

async fn status() -> Result<i32> {
    let Some(stored) = load_credential() else {
        println!(
            "{}",
            crate::localization::cli_locale().format("No stored OpenAI credentials.", &[])
        );
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "Run \"deixic-code openai login\" to authenticate with OpenAI.",
                &[]
            )
        );
        return Ok(0);
    };
    let remaining_ms = (stored.expires_at - now_ms()).max(0);
    let minutes = (remaining_ms as f64 / 60_000.0).round() as i64;
    println!(
        "{}",
        crate::localization::cli_locale().format("Stored OpenAI credentials detected.", &[])
    );
    println!(
        "{}",
        crate::localization::cli_locale().format(
            "Access token expires in ~{0} minute{1} (auto-refresh enabled).",
            &[
                (minutes).to_string(),
                (if minutes == 1 { "" } else { "s" }).to_string()
            ]
        )
    );
    if fresh_credential(stored).await?.is_some() {
        println!(
            "{}",
            crate::localization::cli_locale().format("Credentials refreshed.", &[])
        );
    }
    Ok(0)
}

async fn fresh_credential(stored: OpenAiOAuthCredential) -> Result<Option<OpenAiOAuthCredential>> {
    if stored.expires_at - now_ms() > 60_000 {
        return Ok(Some(stored));
    }
    let client = Client::new();
    let response = client
        .post(format!("{ISSUER}/oauth/token"))
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", stored.refresh_token.as_str()),
            ("client_id", CLIENT_ID),
            ("scope", "openid profile email offline_access"),
        ])
        .send()
        .await;
    let response = match response {
        Ok(response) => response,
        Err(_) => return Ok(None),
    };
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        if definitive_refresh_failure(status, &body) {
            let _ = fs::remove_file(auth_file());
        }
        return Ok(None);
    }
    let payload: TokenResponse = match response.json().await {
        Ok(payload) => payload,
        Err(_) => return Ok(None),
    };
    let Some(access_token) = payload.access_token else {
        let _ = fs::remove_file(auth_file());
        return Ok(None);
    };
    let mut next = OpenAiOAuthCredential {
        access_token,
        refresh_token: payload.refresh_token.unwrap_or(stored.refresh_token),
        id_token: payload.id_token.clone().unwrap_or(stored.id_token.clone()),
        expires_at: now_ms() + payload.expires_in.unwrap_or(3_600) * 1_000,
        api_key: stored.api_key,
        mode: "openai-oauth".to_owned(),
    };
    if let Some(id_token) = payload.id_token.filter(|token| token != &stored.id_token) {
        if let Some(api_key) = exchange_id_token_for_api_key(&client, &id_token).await? {
            next.api_key = Some(api_key);
        }
    }
    save_credential(&next)?;
    Ok(Some(next))
}

fn definitive_refresh_failure(status: StatusCode, body: &str) -> bool {
    status == StatusCode::BAD_REQUEST
        || status == StatusCode::UNAUTHORIZED
        || body.to_ascii_lowercase().contains("invalid_grant")
}

async fn exchange_authorization_code(
    client: &Client,
    code: &str,
    verifier: &str,
) -> Result<Option<TokenResponse>> {
    let response = client
        .post(format!("{ISSUER}/oauth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &format!("{CALLBACK_ORIGIN}/auth/callback")),
            ("client_id", CLIENT_ID),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .context("OpenAI token exchange failed")?;
    if !response.status().is_success() {
        return Ok(None);
    }
    let payload = response
        .json::<TokenResponse>()
        .await
        .context("OpenAI token response was invalid")?;
    if payload.access_token.is_none()
        || payload.refresh_token.is_none()
        || payload.id_token.is_none()
    {
        return Ok(None);
    }
    Ok(Some(payload))
}

async fn exchange_id_token_for_api_key(client: &Client, id_token: &str) -> Result<Option<String>> {
    let response = client
        .post(format!("{ISSUER}/oauth/token"))
        .form(&[
            (
                "grant_type",
                "urn:ietf:params:oauth:grant-type:token-exchange",
            ),
            ("client_id", CLIENT_ID),
            ("requested_token", "openai-api-key"),
            ("subject_token", id_token),
            (
                "subject_token_type",
                "urn:ietf:params:oauth:token-type:id_token",
            ),
        ])
        .send()
        .await
        .context("OpenAI API-key exchange failed")?;
    if !response.status().is_success() {
        return Ok(None);
    }
    Ok(response
        .json::<ApiKeyExchangeResponse>()
        .await
        .ok()
        .map(|payload| payload.access_token)
        .filter(|token| !token.is_empty()))
}

fn login_request() -> Result<LoginRequest> {
    let mut verifier_bytes = [0_u8; 32];
    let mut state_bytes = [0_u8; 32];
    rand::fill(&mut verifier_bytes);
    rand::fill(&mut state_bytes);
    let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = URL_SAFE_NO_PAD.encode(state_bytes);
    let mut url = Url::parse(&format!("{ISSUER}/oauth/authorize"))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", &format!("{CALLBACK_ORIGIN}/auth/callback"))
        .append_pair("scope", "openid profile email offline_access")
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("state", &state)
        .append_pair("originator", ORIGINATOR);
    Ok(LoginRequest {
        url,
        verifier,
        state,
    })
}

enum Callback {
    Auth { code: Option<String>, state: String },
    Other,
}

async fn read_callback(stream: &mut TcpStream) -> Result<Callback> {
    let mut buffer = vec![0_u8; 16 * 1024];
    let size = stream.read(&mut buffer).await.context("reading callback")?;
    let request = String::from_utf8_lossy(&buffer[..size]);
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .context("missing callback target")?;
    let url = Url::parse(&format!("{CALLBACK_ORIGIN}{target}"))?;
    if url.path() != "/auth/callback" {
        return Ok(Callback::Other);
    }
    let code = url
        .query_pairs()
        .find_map(|(key, value)| (key == "code").then(|| value.into_owned()));
    let state = url
        .query_pairs()
        .find_map(|(key, value)| (key == "state").then(|| value.into_owned()))
        .unwrap_or_default();
    Ok(Callback::Auth { code, state })
}

async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &str,
) -> Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Response",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .context("writing callback response")
}

fn constant_time_equal(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.as_bytes()
        .iter()
        .zip(right.as_bytes())
        .fold(0_u8, |diff, (left, right)| diff | (left ^ right))
        == 0
}

fn auth_file() -> PathBuf {
    if let Some(path) = env_path("OPENAI_OAUTH_FILE") {
        return path;
    }
    for name in [
        "MAESTRO_AGENT_DIR",
        "PLAYWRIGHT_AGENT_DIR",
        "CODING_AGENT_DIR",
    ] {
        if let Some(path) = env_path(name) {
            return path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("openai-oauth.json");
        }
    }
    let home = env_path("MAESTRO_HOME").unwrap_or_else(|| {
        dirs::home_dir().map_or_else(|| PathBuf::from(".maestro"), |path| path.join(".maestro"))
    });
    home.join("openai-oauth.json")
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Whether a complete stored OpenAI OAuth credential is present on disk.
#[must_use]
pub fn has_stored_oauth_credential() -> bool {
    load_credential().is_some()
}

fn load_credential() -> Option<OpenAiOAuthCredential> {
    let contents = fs::read_to_string(auth_file()).ok()?;
    let credential = serde_json::from_str::<OpenAiOAuthCredential>(&contents).ok()?;
    (!credential.access_token.is_empty()
        && !credential.refresh_token.is_empty()
        && !credential.id_token.is_empty())
    .then_some(credential)
}

fn save_credential(credential: &OpenAiOAuthCredential) -> Result<()> {
    let path = auth_file();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("Failed to create {}", parent.display()))?;
    let temp = parent.join(format!(
        ".openai-oauth.{}.{:016x}.tmp",
        std::process::id(),
        rand::random::<u64>()
    ));
    let contents = serde_json::to_string_pretty(credential)?;
    write_private(&temp, format!("{contents}\n").as_bytes())?;
    fs::rename(&temp, &path).with_context(|| format!("Failed to save {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    fs::write(path, contents).map_err(Into::into)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_url_matches_legacy_pkce_contract() {
        let request = login_request().expect("login request");
        let query = request
            .url
            .query_pairs()
            .into_owned()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(query.get("client_id").map(String::as_str), Some(CLIENT_ID));
        assert_eq!(
            query.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(
            query.get("originator").map(String::as_str),
            Some(ORIGINATOR)
        );
        assert_eq!(request.verifier.len(), 43);
        assert_eq!(request.state.len(), 43);
    }

    #[test]
    fn callback_state_comparison_rejects_mismatch_and_length_changes() {
        assert!(constant_time_equal("same", "same"));
        assert!(!constant_time_equal("same", "diff"));
        assert!(!constant_time_equal("same", "short"));
    }

    #[test]
    fn definitive_refresh_failures_match_legacy_contract() {
        assert!(definitive_refresh_failure(StatusCode::BAD_REQUEST, ""));
        assert!(definitive_refresh_failure(StatusCode::UNAUTHORIZED, ""));
        assert!(definitive_refresh_failure(
            StatusCode::FORBIDDEN,
            r#"{"error":"invalid_grant"}"#
        ));
        assert!(!definitive_refresh_failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            "temporary"
        ));
    }

    #[test]
    fn credential_reader_accepts_legacy_file_without_mode() {
        let credential: OpenAiOAuthCredential = serde_json::from_str(
            r#"{"accessToken":"a","refreshToken":"r","idToken":"i","expiresAt":1}"#,
        )
        .expect("legacy credential");
        assert_eq!(credential.mode, "openai-oauth");
        assert_eq!(credential.api_key, None);
    }
}
