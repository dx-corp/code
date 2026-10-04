//! One-time-code sign-in (RFC 8628) for `maestro login`.
//!
//! The browser callback flow stays in the parent module. This one never needs
//! a redirect back to the machine that started it.

use std::io::{IsTerminal, Write};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::{Map, Value};

use super::{
    OAuthCredentials, OAuthTokenExchange, REQUIRED_LOGIN_SCOPES, identity_base_from_env,
    metadata_string, non_empty, open_browser, provider_ref, response_detail, save_credentials,
};

/// The reviewed first-party client for device sign-in. Identity refuses the
/// device grant for dynamically registered clients, so unlike the browser
/// login this one is enrolled (`sso-applications.json` in dx-corp/k8s).
const DEVICE_CLIENT_ID: &str = "deixic-code-cli";
const DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// RFC 8628 §3.5: a `slow_down` adds five seconds to the polling interval.
const DEVICE_SLOW_DOWN_SECONDS: u64 = 5;
/// The braille spinner Droid and most CLIs draw while waiting.
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Stored credentials for a token response, whichever grant produced it.
pub(super) fn credentials_from_token(
    identity: String,
    token: OAuthTokenExchange,
) -> OAuthCredentials {
    let mut metadata = Map::from_iter([
        ("identityBaseUrl".to_owned(), Value::String(identity)),
        (
            "organizationId".to_owned(),
            Value::String(token.organization_id),
        ),
        ("providerRef".to_owned(), provider_ref()),
        (
            "scopes".to_owned(),
            Value::Array(
                token
                    .scope
                    .split_whitespace()
                    .map(|scope| Value::String(scope.to_owned()))
                    .collect(),
            ),
        ),
    ]);
    if let Some(workspace_id) = non_empty(token.workspace_id.as_deref()) {
        metadata.insert("workspaceId".to_owned(), Value::String(workspace_id));
    }
    OAuthCredentials {
        credential_type: "oauth".to_owned(),
        refresh: token.refresh_token,
        access: token.access_token,
        expires: Utc::now().timestamp_millis() + (token.expires_in as i64 * 1_000),
        metadata,
    }
}

/// Identity does not offer device sign-in to this client here (the endpoint is
/// not routed, or the client is not enrolled for the grant). `maestro login`
/// falls back to the browser callback on this error.
#[derive(Debug)]
pub struct DeviceSignInUnavailable(pub String);

impl std::fmt::Display for DeviceSignInUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for DeviceSignInUnavailable {}

/// Sign-in with a one-time code approved in a browser on any device, the way
/// Droid and `codex login --device-auth` sign in (RFC 8628). Nothing has to
/// reach back to this machine, so it works over SSH and on headless hosts.
pub async fn perform_evalops_device_login() -> Result<()> {
    crate::safety::require_vendor_network()?;
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("build EvalOps HTTP client")?;
    let terminal = DeviceTerminal::detect();
    let credentials = device_login(
        &client,
        &identity_base_from_env(),
        &device_client_id(),
        REQUIRED_LOGIN_SCOPES,
        terminal,
    )
    .await?;
    save_credentials(&credentials)?;
    Ok(())
}

fn device_client_id() -> String {
    std::env::var("MAESTRO_OAUTH_DEVICE_CLIENT_ID")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEVICE_CLIENT_ID.to_owned())
}

/// What the terminal can do for the sign-in prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DeviceTerminal {
    /// Colors and the spinner.
    styled: bool,
    /// Enter opens the link: a person at this machine's own keyboard and
    /// screen. Over SSH a browser would open on the remote host instead.
    open_on_enter: bool,
}

impl DeviceTerminal {
    fn detect() -> Self {
        let styled = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
        let remote = ["SSH_CONNECTION", "SSH_TTY", "SSH_CLIENT"]
            .iter()
            .any(|name| std::env::var_os(name).is_some());
        Self {
            styled,
            open_on_enter: !remote && std::io::stdin().is_terminal(),
        }
    }

    fn paint(self, text: &str, sgr: &str) -> String {
        if self.styled {
            format!("\x1b[{sgr}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }
}

#[derive(Debug, Deserialize)]
struct DeviceAuthorization {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    expires_in: u64,
    #[serde(default)]
    interval: Option<u64>,
}

impl DeviceAuthorization {
    fn link(&self) -> &str {
        self.verification_uri_complete
            .as_deref()
            .unwrap_or(&self.verification_uri)
    }
}

/// What one poll of the token endpoint said.
#[derive(Debug, PartialEq)]
enum DevicePoll {
    Pending,
    SlowDown,
    Approved(String),
}

fn oauth_error(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default()
}

/// Whether a failed start means "not offered here" rather than a fault.
fn device_start_unavailable(status: StatusCode, body: &str) -> bool {
    status == StatusCode::NOT_FOUND
        || status == StatusCode::METHOD_NOT_ALLOWED
        || matches!(
            oauth_error(body).as_str(),
            "unauthorized_client" | "invalid_client" | "unsupported_grant_type"
        )
}

fn device_poll_outcome(status: StatusCode, body: &str) -> Result<DevicePoll> {
    if status.is_success() {
        return Ok(DevicePoll::Approved(body.to_owned()));
    }
    let locale = crate::localization::cli_locale();
    match oauth_error(body).as_str() {
        "authorization_pending" => Ok(DevicePoll::Pending),
        "slow_down" => Ok(DevicePoll::SlowDown),
        "access_denied" => bail!("{}", locale.format("The device sign-in was denied.", &[])),
        "expired_token" => bail!(
            "{}",
            locale.format(
                "The sign-in code expired before it was approved. Run `maestro login` again.",
                &[]
            )
        ),
        _ => bail!(
            "{}",
            locale.format("Device sign-in failed: {0}", &[response_detail(body)])
        ),
    }
}

/// The prompt, laid out as Droid lays it out: where to go, then the code to
/// enter if the link does not open by itself.
fn device_prompt(authorization: &DeviceAuthorization, terminal: DeviceTerminal) -> String {
    let locale = crate::localization::cli_locale();
    let heading = if terminal.open_on_enter {
        locale
            .format(
                "Authenticate your account at (press ENTER to open in browser):",
                &[],
            )
            .replacen("ENTER", &terminal.paint("ENTER", "1"), 1)
    } else {
        locale.format("Authenticate your account at:", &[])
    };
    let fallback = locale.format(
        "If the link does not open, visit {0} and enter code {1} to complete authentication.",
        &[
            terminal.paint(&authorization.verification_uri, "36"),
            terminal.paint(&authorization.user_code, "1"),
        ],
    );
    format!(
        "{heading}\n\n  {}\n\n{}",
        terminal.paint(authorization.link(), "4;36"),
        fallback,
    )
}

/// The account a token was issued to, read from the access token's claims
/// (no verification needed: Identity just issued it to this process).
fn signed_in_account(credentials: &OAuthCredentials) -> Option<String> {
    let claims = credentials
        .access
        .split('.')
        .nth(1)
        .and_then(|claims| URL_SAFE_NO_PAD.decode(claims).ok())
        .and_then(|claims| serde_json::from_slice::<Value>(&claims).ok());
    claims
        .as_ref()
        .and_then(|claims| {
            ["email", "preferred_username", "name"]
                .iter()
                .find_map(|key| claims.get(*key).and_then(Value::as_str))
        })
        .map(str::to_owned)
        .or_else(|| metadata_string(&credentials.metadata, "organizationId"))
}

/// Draws the wait line until `done` fires, then clears it.
fn spawn_spinner(
    terminal: DeviceTerminal,
    done: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let label =
        crate::localization::cli_locale().format("Waiting for authentication to complete...", &[]);
    tokio::spawn(async move {
        if !terminal.styled {
            eprintln!("{label}");
            return;
        }
        let mut frame = 0_usize;
        loop {
            eprint!(
                "\r{} {label}",
                terminal.paint(SPINNER_FRAMES[frame % SPINNER_FRAMES.len()], "36")
            );
            let _ = std::io::stderr().flush();
            frame += 1;
            tokio::select! {
                () = done.cancelled() => break,
                () = tokio::time::sleep(Duration::from_millis(80)) => {}
            }
        }
        eprint!("\r\x1b[2K");
        let _ = std::io::stderr().flush();
    })
}

async fn device_login(
    client: &Client,
    identity: &str,
    client_id: &str,
    requested_scopes: &str,
    terminal: DeviceTerminal,
) -> Result<OAuthCredentials> {
    let locale = crate::localization::cli_locale();
    println!("{}", locale.format("Initiating authentication...", &[]));
    let response = client
        .post(format!("{identity}/device_authorization"))
        .form(&[("client_id", client_id), ("scope", requested_scopes)])
        .send()
        .await
        .context("start EvalOps device sign-in")?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        let detail = format!("HTTP {}", status.as_u16());
        if device_start_unavailable(status, &body) {
            return Err(DeviceSignInUnavailable(detail).into());
        }
        bail!(
            "{}",
            locale.format(
                "Device sign-in failed: {0}",
                &[format!("{detail}: {}", response_detail(&body))]
            )
        );
    }
    let authorization: DeviceAuthorization =
        serde_json::from_str(&body).context("parse EvalOps device authorization")?;
    // A code nobody can approve is worse than no code: Identity can start a
    // request before its approval page is deployed.
    if !verification_page_live(client, &authorization.verification_uri).await {
        return Err(
            DeviceSignInUnavailable("the approval page is not available".to_owned()).into(),
        );
    }
    println!();
    println!("{}", device_prompt(&authorization, terminal));
    println!();
    if terminal.open_on_enter {
        // The login command exits after sign-in, so this reader never takes
        // a line meant for a later prompt.
        let link = authorization.link().to_owned();
        let mut lines = terminal_lines();
        tokio::spawn(async move {
            if lines.recv().await.is_some() {
                open_browser(&link);
            }
        });
    }

    let done = tokio_util::sync::CancellationToken::new();
    let spinner = spawn_spinner(terminal, done.clone());
    let polled = poll_device_token(client, identity, client_id, &authorization).await;
    done.cancel();
    let _ = spinner.await;
    let token: OAuthTokenExchange =
        serde_json::from_str(&polled?).context("parse EvalOps device sign-in tokens")?;
    let credentials = credentials_from_token(identity.to_owned(), token);
    println!(
        "{}",
        terminal.paint(
            &format!("✓ {}", locale.format("Successfully authenticated", &[])),
            "32"
        )
    );
    if let Some(account) = signed_in_account(&credentials) {
        println!(
            "{}",
            locale.format("Authenticated as {0}", &[terminal.paint(&account, "1")])
        );
    }
    Ok(credentials)
}

/// Whether the page the person approves on answers at all.
async fn verification_page_live(client: &Client, verification_uri: &str) -> bool {
    client
        .get(verification_uri)
        .send()
        .await
        .is_ok_and(|response| response.status().is_success())
}

/// Polls `/token` with the device code until the person decides or the code
/// expires, honouring `authorization_pending` and `slow_down` (RFC 8628 §3.5).
async fn poll_device_token(
    client: &Client,
    identity: &str,
    client_id: &str,
    authorization: &DeviceAuthorization,
) -> Result<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(authorization.expires_in);
    let mut interval = authorization.interval.unwrap_or(5).max(1);
    loop {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "{}",
                crate::localization::cli_locale().format(
                    "The sign-in code expired before it was approved. Run `maestro login` again.",
                    &[]
                )
            );
        }
        let response = client
            .post(format!("{identity}/token"))
            .form(&[
                ("grant_type", DEVICE_CODE_GRANT_TYPE),
                ("device_code", authorization.device_code.as_str()),
                ("client_id", client_id),
            ])
            .send()
            .await
            .context("poll EvalOps device sign-in")?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        match device_poll_outcome(status, &body)? {
            DevicePoll::Pending => {}
            DevicePoll::SlowDown => interval += DEVICE_SLOW_DOWN_SECONDS,
            DevicePoll::Approved(body) => return Ok(body),
        }
    }
}

/// Lines typed at the terminal, read on a detached thread so a sign-in that
/// completes elsewhere never waits on stdin.
fn terminal_lines() -> tokio::sync::mpsc::UnboundedReceiver<String> {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lines() {
            let Ok(line) = line else { break };
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    receiver
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn device_authorization() -> DeviceAuthorization {
        DeviceAuthorization {
            device_code: "device-secret".into(),
            user_code: "WDJB-MJHT".into(),
            verification_uri: "https://identity.evalops.dev/device".into(),
            verification_uri_complete: Some(
                "https://identity.evalops.dev/device?user_code=WDJB-MJHT".into(),
            ),
            expires_in: 600,
            interval: Some(5),
        }
    }

    const PLAIN_LOCAL: DeviceTerminal = DeviceTerminal {
        styled: false,
        open_on_enter: true,
    };
    const PLAIN_REMOTE: DeviceTerminal = DeviceTerminal {
        styled: false,
        open_on_enter: false,
    };

    #[test]
    fn the_device_prompt_shows_where_to_go_and_the_code() {
        let local = device_prompt(&device_authorization(), PLAIN_LOCAL);
        assert_eq!(
            local,
            "Authenticate your account at (press ENTER to open in browser):\n\n  \
             https://identity.evalops.dev/device?user_code=WDJB-MJHT\n\n\
             If the link does not open, visit https://identity.evalops.dev/device \
             and enter code WDJB-MJHT to complete authentication."
        );
        assert!(
            !local.contains("device-secret"),
            "the device code is never shown"
        );
    }

    #[test]
    fn over_ssh_the_prompt_does_not_offer_to_open_a_remote_browser() {
        let remote = device_prompt(&device_authorization(), PLAIN_REMOTE);
        assert!(remote.starts_with("Authenticate your account at:\n"));
        assert!(!remote.contains("ENTER"));
        assert!(remote.contains("WDJB-MJHT"));
    }

    #[test]
    fn a_styled_prompt_highlights_the_link_and_the_code() {
        let styled = device_prompt(
            &device_authorization(),
            DeviceTerminal {
                styled: true,
                open_on_enter: true,
            },
        );
        assert!(styled.contains("\x1b[1mENTER\x1b[0m"));
        assert!(
            styled.contains("\x1b[4;36mhttps://identity.evalops.dev/device?user_code=WDJB-MJHT")
        );
        assert!(styled.contains("\x1b[1mWDJB-MJHT\x1b[0m"));
    }

    #[test]
    fn the_link_falls_back_to_the_plain_verification_uri() {
        let mut authorization = device_authorization();
        authorization.verification_uri_complete = None;
        assert_eq!(authorization.link(), "https://identity.evalops.dev/device");
    }

    #[test]
    fn device_polls_follow_rfc_8628() {
        assert_eq!(
            device_poll_outcome(
                StatusCode::BAD_REQUEST,
                r#"{"error":"authorization_pending"}"#
            )
            .unwrap(),
            DevicePoll::Pending
        );
        assert_eq!(
            device_poll_outcome(StatusCode::BAD_REQUEST, r#"{"error":"slow_down"}"#).unwrap(),
            DevicePoll::SlowDown
        );
        assert_eq!(
            device_poll_outcome(StatusCode::OK, "{}").unwrap(),
            DevicePoll::Approved("{}".to_owned())
        );
        let denied = device_poll_outcome(StatusCode::BAD_REQUEST, r#"{"error":"access_denied"}"#)
            .unwrap_err()
            .to_string();
        assert!(denied.contains("denied"), "{denied}");
        let expired = device_poll_outcome(StatusCode::BAD_REQUEST, r#"{"error":"expired_token"}"#)
            .unwrap_err()
            .to_string();
        assert!(expired.contains("expired"), "{expired}");
        for other in [r#"{"error":"invalid_grant"}"#, "not json", ""] {
            assert!(
                device_poll_outcome(StatusCode::BAD_REQUEST, other).is_err(),
                "{other}"
            );
        }
    }

    #[test]
    fn only_an_unoffered_grant_falls_back_to_browser_sign_in() {
        for (status, body) in [
            (StatusCode::NOT_FOUND, "<html>404 Not Found</html>"),
            (StatusCode::METHOD_NOT_ALLOWED, ""),
            (
                StatusCode::BAD_REQUEST,
                r#"{"error":"unauthorized_client"}"#,
            ),
            (StatusCode::UNAUTHORIZED, r#"{"error":"invalid_client"}"#),
        ] {
            assert!(device_start_unavailable(status, body), "{status} {body}");
        }
        for (status, body) in [
            (StatusCode::BAD_REQUEST, r#"{"error":"invalid_scope"}"#),
            (StatusCode::INTERNAL_SERVER_ERROR, ""),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                r#"{"error":"temporarily_unavailable"}"#,
            ),
        ] {
            assert!(!device_start_unavailable(status, body), "{status} {body}");
        }
    }

    #[test]
    fn the_signed_in_account_comes_from_the_access_token() {
        let claims = URL_SAFE_NO_PAD.encode(br#"{"sub":"u1","email":"ada@example.com"}"#);
        let mut credentials = OAuthCredentials {
            credential_type: "oauth".into(),
            refresh: String::new(),
            access: format!("header.{claims}.signature"),
            expires: 0,
            metadata: Map::from_iter([(
                "organizationId".to_owned(),
                Value::String("org_1".into()),
            )]),
        };
        assert_eq!(
            signed_in_account(&credentials).as_deref(),
            Some("ada@example.com")
        );
        credentials.access = "opaque".into();
        assert_eq!(signed_in_account(&credentials).as_deref(), Some("org_1"));
    }

    /// A scripted Identity: `/device_authorization` answers `start`, and each
    /// `/token` poll takes the next of `polls`. Token requests are recorded.
    async fn spawn_device_identity(
        start: (u16, String),
        polls: Vec<(u16, String)>,
        page_live: bool,
    ) -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<BTreeMap<String, String>>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("identity bind");
        let addr = listener.local_addr().expect("identity addr");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = seen.clone();
        let polls = std::sync::Arc::new(std::sync::Mutex::new(
            polls.into_iter().collect::<std::collections::VecDeque<_>>(),
        ));
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut buffer = vec![0_u8; 16 * 1024];
                let Ok(size) = stream.read(&mut buffer).await else {
                    continue;
                };
                let request = String::from_utf8_lossy(&buffer[..size]).into_owned();
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_owned();
                let form = url::form_urlencoded::parse(
                    request
                        .split("\r\n\r\n")
                        .nth(1)
                        .unwrap_or_default()
                        .as_bytes(),
                )
                .into_owned()
                .collect::<BTreeMap<_, _>>();
                let (status, body) = match path.as_str() {
                    "/device_authorization" => {
                        recorded.lock().unwrap().push(form);
                        let (status, body) = start.clone();
                        (status, body.replace("IDENTITY", &format!("http://{addr}")))
                    }
                    "/device" if page_live => (200, "<html>approve</html>".to_owned()),
                    "/token" => {
                        recorded.lock().unwrap().push(form);
                        polls
                            .lock()
                            .unwrap()
                            .pop_front()
                            .unwrap_or((500, "{}".to_owned()))
                    }
                    _ => (404, String::new()),
                };
                let response = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        (format!("http://{addr}"), seen, handle)
    }

    /// `IDENTITY` becomes the stub's own address.
    fn device_start_body(interval: u64) -> String {
        serde_json::json!({
            "device_code": "device-secret",
            "user_code": "WDJB-MJHT",
            "verification_uri": "IDENTITY/device",
            "verification_uri_complete": "IDENTITY/device?user_code=WDJB-MJHT",
            "expires_in": 600,
            "interval": interval,
        })
        .to_string()
    }

    fn device_token_body() -> String {
        let claims = URL_SAFE_NO_PAD.encode(br#"{"email":"ada@example.com"}"#);
        serde_json::json!({
            "access_token": format!("h.{claims}.s"),
            "refresh_token": "refresh-1",
            "expires_in": 3600,
            "scope": "llm_gateway:invoke sessions:read",
            "organization_id": "org_1",
            "workspace_id": "ws_1",
        })
        .to_string()
    }

    #[tokio::test]
    async fn a_device_sign_in_waits_through_pending_and_slow_down_then_stores_the_tokens() {
        let (identity, seen, task) = spawn_device_identity(
            (200, device_start_body(1)),
            vec![
                (400, r#"{"error":"authorization_pending"}"#.into()),
                (400, r#"{"error":"slow_down"}"#.into()),
                (200, device_token_body()),
            ],
            true,
        )
        .await;
        let started = std::time::Instant::now();
        let credentials = device_login(
            &Client::new(),
            &identity,
            "deixic-code-cli",
            "llm_gateway:invoke sessions:read",
            PLAIN_REMOTE,
        )
        .await
        .expect("device sign-in completes");
        task.abort();
        assert_eq!(credentials.refresh, "refresh-1");
        assert_eq!(
            metadata_string(&credentials.metadata, "organizationId").as_deref(),
            Some("org_1")
        );
        assert_eq!(
            metadata_string(&credentials.metadata, "workspaceId").as_deref(),
            Some("ws_1")
        );
        assert_eq!(
            signed_in_account(&credentials).as_deref(),
            Some("ada@example.com")
        );
        // 1 s, 1 s, then 1 + 5 s after slow_down.
        assert!(
            started.elapsed() >= Duration::from_secs(7),
            "{:?}",
            started.elapsed()
        );
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen[0].get("client_id").map(String::as_str),
            Some("deixic-code-cli")
        );
        assert_eq!(
            seen[0].get("scope").map(String::as_str),
            Some("llm_gateway:invoke sessions:read")
        );
        assert_eq!(seen.len(), 4, "one start and three polls");
        for poll in &seen[1..] {
            assert_eq!(
                poll.get("grant_type").map(String::as_str),
                Some(DEVICE_CODE_GRANT_TYPE)
            );
            assert_eq!(
                poll.get("device_code").map(String::as_str),
                Some("device-secret")
            );
            assert_eq!(
                poll.get("client_id").map(String::as_str),
                Some("deixic-code-cli")
            );
        }
    }

    #[tokio::test]
    async fn a_denied_device_sign_in_stops_polling() {
        let (identity, seen, task) = spawn_device_identity(
            (200, device_start_body(1)),
            vec![
                (400, r#"{"error":"access_denied"}"#.into()),
                (200, device_token_body()),
            ],
            true,
        )
        .await;
        let error = device_login(
            &Client::new(),
            &identity,
            "deixic-code-cli",
            "s",
            PLAIN_REMOTE,
        )
        .await
        .expect_err("a denial ends the sign-in");
        task.abort();
        assert!(error.to_string().contains("denied"), "{error}");
        assert_eq!(seen.lock().unwrap().len(), 2, "no poll after the denial");
    }

    #[tokio::test]
    async fn an_unrouted_device_endpoint_is_reported_as_unavailable() {
        let (identity, _seen, task) =
            spawn_device_identity((404, "<html>404 Not Found</html>".into()), Vec::new(), true)
                .await;
        let error = device_login(
            &Client::new(),
            &identity,
            "deixic-code-cli",
            "s",
            PLAIN_REMOTE,
        )
        .await
        .expect_err("no device sign-in here");
        task.abort();
        assert!(error.is::<DeviceSignInUnavailable>(), "{error}");
    }

    #[tokio::test]
    async fn a_request_nobody_can_approve_is_never_shown() {
        // Identity starts the request, but its approval page is not served:
        // falling back beats showing a code that can never be approved.
        let (identity, seen, task) = spawn_device_identity(
            (200, device_start_body(1)),
            vec![(200, device_token_body())],
            false,
        )
        .await;
        let error = device_login(
            &Client::new(),
            &identity,
            "deixic-code-cli",
            "s",
            PLAIN_REMOTE,
        )
        .await
        .expect_err("no approval page, no device sign-in");
        task.abort();
        assert!(error.is::<DeviceSignInUnavailable>(), "{error}");
        assert_eq!(seen.lock().unwrap().len(), 1, "it never polls");
    }

    #[tokio::test]
    async fn a_server_fault_is_not_mistaken_for_unavailability() {
        let (identity, _seen, task) = spawn_device_identity(
            (500, r#"{"error":"server_error"}"#.into()),
            Vec::new(),
            true,
        )
        .await;
        let error = device_login(
            &Client::new(),
            &identity,
            "deixic-code-cli",
            "s",
            PLAIN_REMOTE,
        )
        .await
        .expect_err("a fault fails the sign-in");
        task.abort();
        assert!(!error.is::<DeviceSignInUnavailable>(), "{error}");
    }
}
