//! Typed native `maestro doctor` report.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[path = "managed_inference_readiness.rs"]
mod managed_inference_readiness;

use crate::ai::{ProviderProtocol, ProviderRegistry, ResolvedProvider, op_secret};
use crate::model_catalog::{
    ModelInfo, find_model, has_provider_mismatch, protocol_name, verify_model_offline,
};

pub const REPORT_SCHEMA_VERSION: u32 = 1;
#[cfg(not(test))]
const LIVE_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(test)]
const LIVE_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Pass,
    Warning,
    Fail,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorCheck {
    pub id: String,
    pub status: CheckStatus,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub live: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectedModelReport {
    pub requested: String,
    pub provider: String,
    pub protocol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catalog: Option<ModelInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorReport {
    pub schema_version: u32,
    pub ok: bool,
    pub live_requested: bool,
    pub selected_model: SelectedModelReport,
    pub checks: Vec<DoctorCheck>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DoctorOptions {
    json: bool,
    live: bool,
    verbose: bool,
    timeout_ms: u64,
    model: Option<String>,
    categories: HashSet<String>,
}

fn parse_options(args: &[String]) -> Result<DoctorOptions> {
    let mut options = DoctorOptions {
        json: false,
        live: false,
        verbose: false,
        timeout_ms: 10_000,
        model: None,
        categories: HashSet::new(),
    };
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--json" => options.json = true,
            "--live" => options.live = true,
            "--verbose" => options.verbose = true,
            "--timeout" => {
                index += 1;
                options.timeout_ms = args
                    .get(index)
                    .context("--timeout requires milliseconds")?
                    .parse::<u64>()
                    .context("--timeout must be a positive integer")?;
                if options.timeout_ms == 0 {
                    bail!("--timeout must be a positive integer");
                }
            }
            value if value.starts_with("--timeout=") => {
                options.timeout_ms = value[10..]
                    .parse::<u64>()
                    .context("--timeout must be a positive integer")?;
                if options.timeout_ms == 0 {
                    bail!("--timeout must be a positive integer");
                }
            }
            "--model" => {
                index += 1;
                options.model = Some(args.get(index).context("--model requires a value")?.clone());
            }
            value if value.starts_with("--model=") => {
                options.model = Some(value[8..].to_owned());
            }
            "--connectivity" | "--network" => {
                options.categories.insert("network".to_owned());
            }
            "--config" | "--certs" | "--proxy" | "--auth" | "--daemon" | "--runner" | "--mcp"
            | "--hooks" | "--plugins" => {
                options.categories.insert(args[index][2..].to_owned());
            }
            "--help" | "-h" | "help" => bail!("help"),
            other => bail!("unknown doctor option: {other}"),
        }
        index += 1;
    }
    Ok(options)
}

fn check(
    id: &str,
    status: CheckStatus,
    summary: impl Into<String>,
    detail: Option<String>,
    live: bool,
) -> DoctorCheck {
    DoctorCheck {
        id: id.to_owned(),
        status,
        summary: summary.into(),
        detail,
        live,
        duration_ms: None,
        remediation: None,
    }
}

fn normalize_config_path(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| {
        let mut clean = PathBuf::new();
        for component in path.components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    clean.pop();
                }
                other => clean.push(other.as_os_str()),
            }
        }
        clean
    })
}

fn config_paths(cwd: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(home) = crate::path_utils::maestro_home_dir() {
        paths.push(home.join("config.toml"));
    }
    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".composer/config.toml"));
    }
    paths.push(cwd.join(".maestro/config.toml"));
    paths.push(cwd.join(".composer/config.toml"));
    let mut seen = std::collections::HashSet::new();
    let mut unique = Vec::new();
    for path in paths {
        if seen.insert(normalize_config_path(&path)) {
            unique.push(path);
        }
    }
    unique
}

fn config_checks(cwd: &Path) -> Vec<DoctorCheck> {
    config_paths(cwd)
        .into_iter()
        .map(|path| {
            if !path.exists() {
                return check(
                    "config",
                    CheckStatus::Skipped,
                    format!("{} not present", path.display()),
                    None,
                    false,
                );
            }
            match std::fs::read_to_string(&path) {
                Ok(contents) => match toml::from_str::<crate::config::ComposerConfig>(&contents) {
                    Ok(_) => check(
                        "config",
                        CheckStatus::Pass,
                        format!("{} is valid TOML", path.display()),
                        None,
                        false,
                    ),
                    Err(error) => check(
                        "config",
                        CheckStatus::Fail,
                        format!("{} is invalid", path.display()),
                        Some(error.to_string()),
                        false,
                    ),
                },
                Err(error) => check(
                    "config",
                    CheckStatus::Fail,
                    format!("{} is unreadable", path.display()),
                    Some(error.to_string()),
                    false,
                ),
            }
        })
        .collect()
}

fn metadata_url(base_url: &str, suffix: &str) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(base_url).context("provider metadata URL is invalid")?;
    let path = format!(
        "{}/{}",
        url.path().trim_end_matches('/'),
        suffix.trim_start_matches('/')
    );
    url.set_path(&path);
    Ok(url)
}

fn redacted_url(url: &reqwest::Url) -> String {
    let mut redacted = url.clone();
    let _ = redacted.set_username("");
    let _ = redacted.set_password(None);
    redacted.set_query(None);
    redacted.set_fragment(None);
    redacted.to_string()
}

fn metadata_request(
    client: &reqwest::Client,
    resolved: &ResolvedProvider,
    token: &str,
) -> Result<Option<(reqwest::RequestBuilder, ProviderProtocol)>> {
    let Some(base_url) = resolved.base_url.as_deref() else {
        return Ok(None);
    };
    let protocol = resolved.provider.protocol;
    let request = match protocol {
        ProviderProtocol::OpenAi | ProviderProtocol::OpenAiCompatible => {
            let request = client.get(metadata_url(base_url, "models")?);
            if token.is_empty() {
                request
            } else {
                request.bearer_auth(token)
            }
        }
        ProviderProtocol::Anthropic
        | ProviderProtocol::Google
        | ProviderProtocol::VertexAi
        | ProviderProtocol::Codex
        | ProviderProtocol::AzureOpenAi
        | ProviderProtocol::Bedrock
        | ProviderProtocol::Managed => return Ok(None),
    };
    Ok(Some((request, protocol)))
}

fn metadata_contains_model(
    payload: &serde_json::Value,
    protocol: ProviderProtocol,
    model: &str,
) -> bool {
    let entries = match protocol {
        ProviderProtocol::Google | ProviderProtocol::VertexAi => payload.get("models"),
        _ => payload.get("data"),
    }
    .and_then(serde_json::Value::as_array);
    entries.is_some_and(|entries| {
        entries.iter().any(|entry| {
            let id = match protocol {
                ProviderProtocol::Google | ProviderProtocol::VertexAi => entry.get("name"),
                _ => entry.get("id"),
            }
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
            let id = id.strip_prefix("models/").unwrap_or(id);
            id == model
        })
    })
}

fn request_error_detail(error: &reqwest::Error, endpoint: &str) -> String {
    if error.is_timeout() {
        format!("request to {endpoint} timed out")
    } else if error.is_connect() {
        format!("connection to {endpoint} failed")
    } else {
        format!("request to {endpoint} failed")
    }
}

async fn live_metadata_check_with_env(
    model: &SelectedModelReport,
    env: &HashMap<String, String>,
) -> DoctorCheck {
    let Ok(resolved) = ProviderRegistry::resolve(&model.requested, env) else {
        return check(
            "live_metadata",
            CheckStatus::Fail,
            "provider could not be resolved",
            None,
            true,
        );
    };
    let token = resolved.credential.as_deref().unwrap_or_default();
    if resolved.provider.requires_auth() && token.is_empty() {
        return check(
            "live_metadata",
            CheckStatus::Warning,
            "live metadata skipped: credentials unavailable",
            None,
            true,
        );
    }
    let client = match reqwest::Client::builder().timeout(LIVE_TIMEOUT).build() {
        Ok(client) => client,
        Err(_) => {
            return check(
                "live_metadata",
                CheckStatus::Fail,
                "HTTP client setup failed",
                None,
                true,
            );
        }
    };
    let (request, protocol) = match metadata_request(&client, &resolved, token) {
        Ok(Some(request)) => request,
        Ok(None) => {
            return check(
                "live_metadata",
                CheckStatus::Skipped,
                format!(
                    "{} does not have a safe native metadata probe",
                    resolved.provider.id
                ),
                None,
                true,
            );
        }
        Err(_) => {
            return check(
                "live_metadata",
                CheckStatus::Fail,
                "provider metadata endpoint is invalid",
                None,
                true,
            );
        }
    };
    let endpoint = request
        .try_clone()
        .and_then(|request| request.build().ok())
        .map_or_else(
            || "provider endpoint".to_owned(),
            |request| redacted_url(request.url()),
        );
    let started = Instant::now();
    match request.send().await {
        Ok(response) if response.status().is_success() => {
            let requested_model = model
                .requested
                .split_once('/')
                .map_or(model.requested.as_str(), |(_, model)| model);
            match response.json::<serde_json::Value>().await {
                Ok(payload) if metadata_contains_model(&payload, protocol, requested_model) => {
                    check(
                        "live_metadata",
                        CheckStatus::Pass,
                        format!(
                            "provider metadata confirmed {requested_model} in {} ms",
                            started.elapsed().as_millis()
                        ),
                        Some(endpoint),
                        true,
                    )
                }
                Ok(_) => check(
                    "live_metadata",
                    CheckStatus::Fail,
                    format!("provider metadata does not include {requested_model}"),
                    Some(endpoint),
                    true,
                ),
                Err(_) => check(
                    "live_metadata",
                    CheckStatus::Fail,
                    "provider metadata response is invalid",
                    Some(endpoint),
                    true,
                ),
            }
        }
        Ok(response) => {
            let status = response.status();
            let failed = matches!(status.as_u16(), 401 | 403) || status.is_server_error();
            check(
                "live_metadata",
                if failed {
                    CheckStatus::Fail
                } else {
                    CheckStatus::Warning
                },
                format!("provider metadata returned HTTP {status}"),
                Some(endpoint),
                true,
            )
        }
        Err(error) => check(
            "live_metadata",
            CheckStatus::Fail,
            "provider metadata request failed",
            Some(request_error_detail(&error, &endpoint)),
            true,
        ),
    }
}

/// Providers covered by the doctor auth health section.
const AUTH_HEALTH_PROVIDERS: &[&str] = &[
    "openai",
    "openai-codex",
    "anthropic",
    "google",
    "vertex-ai",
    "xai",
];

/// Report credential availability for each well-known provider without ever
/// printing secret values. `op://` references are actually resolved through
/// the 1Password CLI so broken vault references surface here.
fn credential_mode_check(readiness: &Result<crate::credential_mode::DetectedMode>) -> DoctorCheck {
    match readiness {
        Ok(crate::credential_mode::DetectedMode::Platform(session))
            if session
                .workspace_id
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty()) =>
        {
            check(
                "credential_mode",
                CheckStatus::Pass,
                format!(
                    "platform: org {} via EvalOps identity",
                    session.organization_id
                ),
                session.email.clone(),
                false,
            )
        }
        Ok(crate::credential_mode::DetectedMode::Platform(_)) => check(
            "credential_mode",
            CheckStatus::Fail,
            "platform session is missing workspace scope",
            Some("Run `deixic-code doctor --live` to verify your account scope. If verification fails, run `deixic-code evalops login`.".to_owned()),
            false,
        ),
        Ok(crate::credential_mode::DetectedMode::Byok) => check(
            "credential_mode",
            CheckStatus::Pass,
            "byok: a local provider credential is available",
            None,
            false,
        ),
        Err(error)
            if error
                .to_string()
                .contains(crate::credential_mode::IDENTITY_REQUIRED_MESSAGE) =>
        {
            check(
                "credential_mode",
                CheckStatus::Fail,
                "EvalOps Identity is required",
                Some(error.to_string()),
                false,
            )
        }
        Err(error) => check(
            "credential_mode",
            CheckStatus::Fail,
            "no usable managed or local provider credential",
            Some(error.to_string()),
            false,
        ),
    }
}

fn auth_health_checks(env: &HashMap<String, String>) -> Vec<DoctorCheck> {
    AUTH_HEALTH_PROVIDERS
        .iter()
        .map(|provider_id| auth_health_check(provider_id, env))
        .collect()
}

fn auth_health_check(provider_id: &str, env: &HashMap<String, String>) -> DoctorCheck {
    let Some(descriptor) = ProviderRegistry::descriptor(provider_id) else {
        return check(
            "auth_health",
            CheckStatus::Skipped,
            format!("{provider_id}: unknown provider"),
            None,
            false,
        );
    };
    let configured = descriptor.auth_env.iter().find_map(|name| {
        env.get(*name)
            .map(String::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| (*name, value))
    });
    if let Some((name, value)) = configured {
        if op_secret::is_op_reference(value) {
            return match op_secret::resolve_credential(name, value) {
                Ok(_) => check(
                    "auth_health",
                    CheckStatus::Pass,
                    format!("{provider_id}: {name} op:// reference resolves via 1Password CLI"),
                    None,
                    false,
                ),
                Err(error) => check(
                    "auth_health",
                    CheckStatus::Fail,
                    format!("{provider_id}: {name} op:// reference could not be resolved"),
                    Some(format!("{error:#}")),
                    false,
                ),
            };
        }
        return check(
            "auth_health",
            CheckStatus::Pass,
            format!("{provider_id}: {name} is set"),
            None,
            false,
        );
    }
    if provider_id == "openai" && crate::openai_cli::has_stored_oauth_credential() {
        return check(
            "auth_health",
            CheckStatus::Pass,
            format!("{provider_id}: stored OAuth credential present"),
            None,
            false,
        );
    }
    if matches!(provider_id, "openai-codex" | "codex")
        && crate::codex_auth::read_codex_auth().is_some_and(|s| s.has_usable_credential())
    {
        return check(
            "auth_health",
            CheckStatus::Pass,
            format!("{provider_id}: CODEX_HOME/auth.json credential present"),
            crate::codex_auth::codex_auth_path().map(|p| p.display().to_string()),
            false,
        );
    }
    check(
        "auth_health",
        CheckStatus::Warning,
        format!("{provider_id}: no credential found"),
        Some(descriptor.auth_env.join(", ")),
        false,
    )
}

/// Dedicated Codex login surface so doctor is not API-key-centric when the
/// user is signed in via `maestro codex login`.
fn codex_login_health_check() -> DoctorCheck {
    match crate::codex_auth::read_codex_auth() {
        Some(snap) if snap.has_usable_credential() => {
            let mode = snap.auth_mode.as_deref().unwrap_or("unknown");
            let via = if snap.access_token.is_some() {
                "ChatGPT access token"
            } else {
                "API key in auth.json"
            };
            check(
                "codex_login",
                CheckStatus::Pass,
                format!("Codex auth available ({mode}, {via})"),
                crate::codex_auth::codex_auth_path().map(|p| p.display().to_string()),
                false,
            )
        }
        _ => check(
            "codex_login",
            CheckStatus::Warning,
            "Codex auth not found — run `deixic-code codex login` for ChatGPT subscription models",
            crate::codex_auth::codex_auth_path().map(|p| p.display().to_string()),
            false,
        ),
    }
}

/// Documents that `openai-codex/*` is the Codex app-server transport (not
/// Platform API-key HTTP). Only when the selected model provider is
/// openai-codex/codex (not merely because auth.json exists).
fn codex_app_server_transport_check(selected_provider: &str) -> DoctorCheck {
    if !matches!(selected_provider, "openai-codex" | "codex") {
        return check(
            "codex_app_server",
            CheckStatus::Skipped,
            "Codex app-server transport not selected (model is not openai-codex)",
            None,
            false,
        );
    }

    let spawn = crate::codex_app_server::resolve_spawn_command(None, None);
    let spawn_detail = match spawn.source {
        crate::codex_app_server::SpawnSource::BundledPackage => {
            format!(
                "bundled {}",
                spawn.args.first().cloned().unwrap_or_default()
            )
        }
        crate::codex_app_server::SpawnSource::Path => {
            format!("PATH binary `{}`", spawn.command)
        }
        crate::codex_app_server::SpawnSource::Override => {
            format!("override `{}`", spawn.command)
        }
    };

    let auth_ok =
        crate::codex_auth::read_codex_auth().is_some_and(|snap| snap.has_usable_credential());
    if auth_ok {
        check(
            "codex_app_server",
            CheckStatus::Pass,
            format!(
                "openai-codex path: Codex app-server (`thread/start`, `turn/start`); ChatGPT auth owned by Codex ({spawn_detail})"
            ),
            Some(spawn_detail),
            false,
        )
    } else {
        check(
            "codex_app_server",
            CheckStatus::Warning,
            format!(
                "Codex app-server spawn resolved ({spawn_detail}) but ChatGPT auth missing — run `deixic-code codex login`"
            ),
            Some(spawn_detail),
            false,
        )
    }
}

async fn managed_setup_check(
    readiness: &Result<crate::credential_mode::DetectedMode>,
    live: bool,
) -> DoctorCheck {
    let Ok(crate::credential_mode::DetectedMode::Platform(session)) = readiness else {
        return check(
            "managed_setup",
            CheckStatus::Skipped,
            "Managed setup needs a verified managed login",
            Some("Run `deixic-code doctor --live` after signing in.".to_owned()),
            live,
        );
    };
    if crate::managed_setup::platform_base_url().is_none() {
        return check("managed_setup", CheckStatus::Warning,
            "MCP setup has no Deixic address",
            Some("Set MAESTRO_MANAGED_SETUP_URL or MAESTRO_PLATFORM_BASE_URL to your organization's Deixic address. Model requests can still work; MCP servers require a verified policy.".to_owned()), false);
    }
    if !live {
        return check(
            "managed_setup",
            CheckStatus::Skipped,
            "Deixic address is configured; connection not checked",
            Some("Run `deixic-code doctor --live` to check managed setup.".to_owned()),
            false,
        );
    }
    let session = session.clone();
    let result = tokio::task::spawn_blocking(move || {
        crate::managed_setup::ManagedSetupClient::resolve_with(
            Some(&session),
            None,
            0,
            Duration::ZERO,
            crate::managed_setup::fetch_managed_setup,
        )
    })
    .await;
    match result {
        Ok(client) if client.origin() == crate::managed_setup::ManagedSetupOrigin::Fetched =>
            check("managed_setup", CheckStatus::Pass, "Managed setup verified for your account", None, true),
        _ => check("managed_setup", CheckStatus::Warning, "Managed setup could not be verified",
            Some("Check MAESTRO_MANAGED_SETUP_URL or MAESTRO_PLATFORM_BASE_URL and your organization's access. MCP access remains governed by the last verified policy, or refused when none is available.".to_owned()), true),
    }
}

fn measured(mut item: DoctorCheck, started: Instant, verbose: bool) -> DoctorCheck {
    if verbose {
        item.duration_ms = Some(started.elapsed().as_millis().try_into().unwrap_or(u64::MAX));
    }
    item
}

fn mcp_config_check(cwd: &Path, verbose: bool) -> DoctorCheck {
    let started = Instant::now();
    let config = crate::mcp::load_mcp_config(Some(cwd));
    let enabled = config.enabled_servers().count();
    let item = if config.issues.is_empty() {
        check(
            "mcp",
            CheckStatus::Pass,
            format!("{enabled} enabled MCP server(s); configuration is valid"),
            None,
            false,
        )
    } else {
        let mut item = check(
            "mcp",
            CheckStatus::Fail,
            format!("{} MCP configuration issue(s)", config.issues.len()),
            Some(
                config
                    .issues
                    .iter()
                    .map(|issue| format!("{}: {}", issue.path.display(), issue.message))
                    .collect::<Vec<_>>()
                    .join("; "),
            ),
            false,
        );
        item.remediation = Some(
            "Run `deixic-code mcp status` and repair the reported configuration source.".to_owned(),
        );
        item
    };
    measured(item, started, verbose)
}

fn hooks_config_check(cwd: &Path, verbose: bool) -> DoctorCheck {
    let started = Instant::now();
    let item = match crate::hooks::load_hook_config(cwd) {
        Ok(config) if !config.skipped_untrusted_paths.is_empty() => {
            let mut item = check(
                "hooks",
                CheckStatus::Warning,
                format!(
                    "{} effective hook(s); project hooks were skipped in an untrusted workspace",
                    config.hooks.len()
                ),
                Some(
                    config
                        .skipped_untrusted_paths
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
                false,
            );
            item.remediation = Some("Inspect with `deixic-code hooks list --json`; grant workspace trust only after reviewing the hook files.".to_owned());
            item
        }
        Ok(config) => check(
            "hooks",
            CheckStatus::Pass,
            format!(
                "{} effective hook(s) from {} configuration source(s)",
                config.hooks.len(),
                config.source_paths.len()
            ),
            None,
            false,
        ),
        Err(error) => {
            let mut item = check(
                "hooks",
                CheckStatus::Fail,
                "hook configuration failed to load",
                Some(error.to_string()),
                false,
            );
            item.remediation = Some(
                "Run `deixic-code hooks list --json` and repair the rejected source file."
                    .to_owned(),
            );
            item
        }
    };
    measured(item, started, verbose)
}

fn plugin_config_check(cwd: &Path, verbose: bool) -> DoctorCheck {
    let started = Instant::now();
    let registry = crate::plugins::PluginRegistry::discover_for_workspace(cwd);
    let item = if !registry.admission_errors().is_empty() {
        let mut item = check(
            "plugins",
            CheckStatus::Fail,
            format!(
                "{} plugin admission error(s)",
                registry.admission_errors().len()
            ),
            Some(registry.admission_errors().join("; ")),
            false,
        );
        item.remediation = Some(
            "Run `deixic-code plugins list --json` and repair or remove the rejected plugin."
                .to_owned(),
        );
        item
    } else if let Some(notice) = registry.untrusted_skip_notice() {
        check(
            "plugins",
            CheckStatus::Warning,
            format!("{} trusted plugin(s) loaded", registry.len()),
            Some(notice),
            false,
        )
    } else {
        check(
            "plugins",
            CheckStatus::Pass,
            format!("{} trusted plugin(s) loaded", registry.len()),
            None,
            false,
        )
    };
    measured(item, started, verbose)
}

fn proxy_config_check(verbose: bool) -> DoctorCheck {
    let started = Instant::now();
    let configured = ["HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY"]
        .into_iter()
        .filter_map(|name| std::env::var(name).ok().map(|value| (name, value)))
        .filter(|(_, value)| !value.trim().is_empty())
        .collect::<Vec<_>>();
    let invalid = configured
        .iter()
        .filter_map(|(name, value)| {
            reqwest::Url::parse(value)
                .err()
                .map(|error| format!("{name}: {error}"))
        })
        .collect::<Vec<_>>();
    let item = if !invalid.is_empty() {
        let mut item = check(
            "proxy",
            CheckStatus::Fail,
            "proxy environment contains an invalid URL",
            Some(invalid.join("; ")),
            false,
        );
        item.remediation = Some("Set HTTP_PROXY, HTTPS_PROXY, or ALL_PROXY to an absolute proxy URL, or remove the invalid value.".to_owned());
        item
    } else if configured.is_empty() {
        check(
            "proxy",
            CheckStatus::Pass,
            "direct network path; no proxy configured",
            None,
            false,
        )
    } else {
        check(
            "proxy",
            CheckStatus::Pass,
            format!("{} proxy variable(s) configured", configured.len()),
            Some(
                configured
                    .iter()
                    .map(|(name, _)| *name)
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            false,
        )
    };
    measured(item, started, verbose)
}

fn certificate_config_check(verbose: bool) -> DoctorCheck {
    let started = Instant::now();
    let configured = ["SSL_CERT_FILE", "REQUESTS_CA_BUNDLE", "CURL_CA_BUNDLE"]
        .into_iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .map(|value| (name, PathBuf::from(value)))
        })
        .collect::<Vec<_>>();
    let missing = configured
        .iter()
        .filter(|(_, path)| !path.is_file())
        .map(|(name, path)| format!("{name}={}", path.display()))
        .collect::<Vec<_>>();
    let item = if !missing.is_empty() {
        let mut item = check(
            "certs",
            CheckStatus::Fail,
            "custom certificate bundle is missing or not a file",
            Some(missing.join("; ")),
            false,
        );
        item.remediation = Some(
            "Install the certificate bundle or remove the stale environment override.".to_owned(),
        );
        item
    } else if configured.is_empty() {
        check(
            "certs",
            CheckStatus::Pass,
            "using the system certificate trust store",
            None,
            false,
        )
    } else {
        check(
            "certs",
            CheckStatus::Pass,
            format!(
                "{} custom certificate bundle override(s) are readable",
                configured.len()
            ),
            None,
            false,
        )
    };
    measured(item, started, verbose)
}

async fn http_health_check(
    id: &str,
    url: &str,
    timeout: Duration,
    verbose: bool,
    remediation: &str,
) -> DoctorCheck {
    let started = Instant::now();
    let parsed = match reqwest::Url::parse(url) {
        Ok(url) if matches!(url.scheme(), "http" | "https") => url,
        Ok(_) => {
            let mut item = check(
                id,
                CheckStatus::Fail,
                "health URL must use http or https",
                None,
                true,
            );
            item.remediation = Some(remediation.to_owned());
            return measured(item, started, verbose);
        }
        Err(error) => {
            let mut item = check(
                id,
                CheckStatus::Fail,
                "health URL is invalid",
                Some(error.to_string()),
                true,
            );
            item.remediation = Some(remediation.to_owned());
            return measured(item, started, verbose);
        }
    };
    let display = redacted_url(&parsed);
    let result = match reqwest::Client::builder().timeout(timeout).build() {
        Ok(client) => client.get(parsed).send().await,
        Err(error) => Err(error),
    };
    let item = match result {
        Ok(response) if response.status().is_success() => check(
            id,
            CheckStatus::Pass,
            format!("health endpoint responded at {display}"),
            Some(format!("HTTP {}", response.status())),
            true,
        ),
        Ok(response) => {
            let mut item = check(
                id,
                CheckStatus::Fail,
                format!("health endpoint rejected the probe at {display}"),
                Some(format!("HTTP {}", response.status())),
                true,
            );
            item.remediation = Some(remediation.to_owned());
            item
        }
        Err(error) => {
            let mut item = check(
                id,
                CheckStatus::Fail,
                format!("health endpoint is unreachable at {display}"),
                Some(if error.is_timeout() {
                    "request timed out".to_owned()
                } else if error.is_connect() {
                    "connection failed".to_owned()
                } else {
                    "request failed".to_owned()
                }),
                true,
            );
            item.remediation = Some(remediation.to_owned());
            item
        }
    };
    measured(item, started, verbose)
}

async fn daemon_check(force_live: bool, timeout: Duration, verbose: bool) -> DoctorCheck {
    if !force_live {
        return check(
            "daemon",
            CheckStatus::Skipped,
            "daemon probe not requested",
            Some("Run `deixic-code doctor --daemon` to probe the runtime gateway.".to_owned()),
            true,
        );
    }
    let url = std::env::var("MAESTRO_DAEMON_URL").unwrap_or_else(|_| {
        let host = std::env::var("MAESTRO_CONTROL_HOST").unwrap_or_else(|_| "127.0.0.1".to_owned());
        let port = std::env::var("PORT").unwrap_or_else(|_| "8080".to_owned());
        format!("http://{host}:{port}/healthz")
    });
    http_health_check(
        "daemon",
        &url,
        timeout,
        verbose,
        "Start `deixic-code serve`, verify its supervisor, and confirm MAESTRO_DAEMON_URL points at `/healthz`.",
    )
    .await
}

async fn runner_check(force_live: bool, timeout: Duration, verbose: bool) -> DoctorCheck {
    if let Ok(url) = std::env::var("MAESTRO_RUNNER_HEALTH_URL") {
        return http_health_check(
            "runner",
            &url,
            timeout,
            verbose,
            "Restart the runner's external supervisor and verify its lease, credentials, and health URL.",
        )
        .await;
    }
    if let Ok(raw_pid) = std::env::var("MAESTRO_HOSTED_RUNNER_PID") {
        let started = Instant::now();
        let item = match raw_pid.parse::<u32>() {
            Ok(pid) if pid > 0 && process_exists(pid) => check(
                "runner",
                CheckStatus::Pass,
                format!("hosted runner process {pid} is alive"),
                None,
                true,
            ),
            Ok(pid) => {
                let mut item = check(
                    "runner",
                    CheckStatus::Fail,
                    format!("hosted runner process {pid} is not alive"),
                    None,
                    true,
                );
                item.remediation = Some("Restart the external runner supervisor; it owns replacement of the attested child generation.".to_owned());
                item
            }
            Err(error) => check(
                "runner",
                CheckStatus::Fail,
                "MAESTRO_HOSTED_RUNNER_PID is invalid",
                Some(error.to_string()),
                true,
            ),
        };
        return measured(item, started, verbose);
    }
    let mut item = check(
        "runner",
        if force_live { CheckStatus::Fail } else { CheckStatus::Skipped },
        "runner health target is not configured",
        Some("Set MAESTRO_RUNNER_HEALTH_URL or MAESTRO_HOSTED_RUNNER_PID in the runner service environment.".to_owned()),
        force_live,
    );
    item.remediation = Some("Expose the runner's authenticated health endpoint to its local supervisor and doctor process.".to_owned());
    item
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn process_exists(_pid: u32) -> bool {
    false
}

fn category_for_check(id: &str) -> &'static str {
    match id {
        "config" => "config",
        "credential_mode" | "managed_setup" | "managed_inference" | "provider" | "auth_health"
        | "codex_login" | "codex_app_server" => "auth",
        "mcp" => "mcp",
        "hooks" => "hooks",
        "plugins" => "plugins",
        "proxy" => "proxy",
        "certs" => "certs",
        "daemon" => "daemon",
        "runner" => "runner",
        "live_metadata" => "network",
        _ => "config",
    }
}

pub async fn build_report(model_override: Option<&str>, live: bool, cwd: &Path) -> DoctorReport {
    let requested = model_override
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_owned)
        .or_else(|| crate::config::configured_model_route(cwd, None))
        .unwrap_or_else(|| crate::credential_mode::DEFAULT_MANAGED_MODEL.to_owned());
    let snapshot = if live {
        None
    } else {
        crate::init_cli::load_evalops_snapshot().ok().flatten()
    };
    let process_env = std::env::vars().collect::<HashMap<String, String>>();
    let mut env = process_env.clone();
    let _ = crate::codex_auth::merge_codex_auth_snapshot_into_env(
        &mut env,
        crate::codex_auth::read_codex_auth(),
        false,
    );
    let readiness =
        crate::service_connections::ConnectionBroker::merge_default_for_model(&requested, &mut env)
            .and_then(|_| {
                if live {
                    crate::credential_mode::require_ready(&requested)
                } else {
                    crate::credential_mode::require_ready_from(snapshot.as_ref(), &env, &requested)
                }
            });
    let resolved = match readiness.as_ref() {
        Ok(crate::credential_mode::DetectedMode::Platform(session)) => {
            match session.managed_env(&requested, &process_env) {
                Ok(managed_env) => {
                    env = managed_env;
                    ProviderRegistry::resolve(&session.managed_model_route(&requested), &env)
                }
                Err(error) => Err(error),
            }
        }
        _ => ProviderRegistry::resolve(&requested, &env),
    };
    let (provider, protocol) = resolved.as_ref().map_or_else(
        |_| ("unknown".to_owned(), "unknown".to_owned()),
        |value| {
            (
                value.provider.id.to_owned(),
                protocol_name(value.provider.protocol).to_owned(),
            )
        },
    );
    let mut catalog = find_model(&requested);
    if let Some(model) = catalog.as_mut() {
        model.verification = verify_model_offline(&requested);
    }
    let selected_model = SelectedModelReport {
        requested: requested.clone(),
        provider,
        protocol,
        catalog,
    };
    let mut checks = config_checks(cwd);
    let mut identity_check = credential_mode_check(&readiness);
    identity_check.live = live;
    if !live && identity_check.status == CheckStatus::Pass {
        identity_check.detail = Some(
            "Saved credentials only. Run `deixic-code doctor --live` to verify your login."
                .to_owned(),
        );
    }
    checks.push(identity_check);
    checks.push(managed_setup_check(&readiness, live).await);
    checks
        .push(managed_inference_readiness::check(&readiness, &requested, &process_env, live).await);
    checks.push(match resolved {
        Ok(provider) if provider.credential.is_some() => check(
            "provider",
            CheckStatus::Pass,
            format!("{} resolved with credentials", provider.provider.id),
            provider.auth_source,
            false,
        ),
        Ok(provider) if !provider.provider.requires_auth() => check(
            "provider",
            CheckStatus::Pass,
            format!("{} resolved without credentials", provider.provider.id),
            None,
            false,
        ),
        Ok(provider) => check(
            "provider",
            CheckStatus::Warning,
            format!("{} resolved; credentials not found", provider.provider.id),
            Some(provider.provider.auth_env.join(", ")),
            false,
        ),
        Err(error) => check(
            "provider",
            CheckStatus::Fail,
            "provider resolution failed",
            Some(error.to_string()),
            false,
        ),
    });
    match readiness.as_ref() {
        Ok(crate::credential_mode::DetectedMode::Platform(_)) => checks.push(check(
            "auth_health",
            CheckStatus::Pass,
            "platform session supplies llm-gateway credentials; local provider keys are ignored",
            None,
            false,
        )),
        _ => checks.extend(auth_health_checks(&env)),
    }
    if !matches!(
        readiness.as_ref(),
        Ok(crate::credential_mode::DetectedMode::Platform(_))
    ) {
        checks.push(codex_login_health_check());
    }
    checks.push(codex_app_server_transport_check(&selected_model.provider));
    checks.push(if has_provider_mismatch(&requested) {
        check(
            "model_catalog",
            CheckStatus::Fail,
            "selected model does not belong to the requested provider",
            Some(requested),
            false,
        )
    } else if selected_model.catalog.is_some() {
        check(
            "model_catalog",
            CheckStatus::Pass,
            "selected model has typed capability metadata",
            None,
            false,
        )
    } else {
        check(
            "model_catalog",
            CheckStatus::Warning,
            "selected model is not in the built-in capability catalog",
            Some(requested),
            false,
        )
    });
    match crate::codex_cli::codex_schema_diagnostics() {
        Ok(diagnostics) if diagnostics.is_empty() => checks.push(check(
            "codex_tools",
            CheckStatus::Pass,
            "Codex dynamic-tool schemas are compatible",
            None,
            false,
        )),
        Ok(diagnostics) => checks.push(check(
            "codex_tools",
            CheckStatus::Fail,
            "Codex dynamic-tool schema diagnostics found",
            Some(diagnostics.join("; ")),
            false,
        )),
        Err(error) => checks.push(check(
            "codex_tools",
            CheckStatus::Fail,
            "Codex tool profile is invalid",
            Some(error.to_string()),
            false,
        )),
    }
    if live {
        checks.push(live_metadata_check_with_env(&selected_model, &env).await);
    } else {
        checks.push(check(
            "live_metadata",
            CheckStatus::Skipped,
            "live checks not requested",
            None,
            true,
        ));
    }
    let ok = !checks.iter().any(|item| item.status == CheckStatus::Fail);
    DoctorReport {
        schema_version: REPORT_SCHEMA_VERSION,
        ok,
        live_requested: live,
        selected_model,
        checks,
    }
}

pub async fn run_doctor(args: &[String]) -> Result<i32> {
    let options = match parse_options(args) {
        Ok(options) => options,
        Err(error) if error.to_string() == "help" => {
            println!(
                "Usage: deixic-code doctor [--json] [--live] [--verbose] [--timeout <ms>] [--model <provider/model>]\n\
                 \nProbe groups (combine as needed):\n  --connectivity  --config  --certs  --proxy  --auth  --daemon  --runner  --mcp  --hooks  --plugins"
            );
            return Ok(0);
        }
        Err(error) => return Err(error),
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let live = options.live || options.categories.contains("network");
    let mut report = build_report(options.model.as_deref(), live, &cwd).await;
    let timeout = Duration::from_millis(options.timeout_ms);
    report.checks.extend([
        mcp_config_check(&cwd, options.verbose),
        hooks_config_check(&cwd, options.verbose),
        plugin_config_check(&cwd, options.verbose),
        proxy_config_check(options.verbose),
        certificate_config_check(options.verbose),
        daemon_check(
            options.categories.contains("daemon")
                || std::env::var_os("MAESTRO_DAEMON_URL").is_some(),
            timeout,
            options.verbose,
        )
        .await,
        runner_check(
            options.categories.contains("runner")
                || std::env::var_os("MAESTRO_RUNNER_HEALTH_URL").is_some()
                || std::env::var_os("MAESTRO_HOSTED_RUNNER_PID").is_some(),
            timeout,
            options.verbose,
        )
        .await,
    ]);
    if !options.categories.is_empty() {
        report
            .checks
            .retain(|item| options.categories.contains(category_for_check(&item.id)));
    }
    report.ok = !report
        .checks
        .iter()
        .any(|item| item.status == CheckStatus::Fail);
    report.live_requested = report
        .checks
        .iter()
        .any(|item| item.live && item.status != CheckStatus::Skipped);
    if options.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("Deixic Code Doctor (schema v{})", report.schema_version);
        println!(
            "Model: {} ({}, {})",
            report.selected_model.requested,
            report.selected_model.provider,
            report.selected_model.protocol
        );
        for item in &report.checks {
            println!(
                "  {:<7} {:<16} {}",
                format!("{:?}", item.status).to_lowercase(),
                item.id,
                item.summary
            );
            if let Some(detail) = &item.detail {
                println!("          {detail}");
            }
            if let Some(duration_ms) = item.duration_ms {
                println!("          duration: {duration_ms}ms");
            }
            if let Some(remediation) = &item.remediation {
                println!("          fix: {remediation}");
            }
        }
    }
    Ok(report_exit_code(&report))
}

fn report_exit_code(report: &DoctorReport) -> i32 {
    i32::from(!report.ok)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    struct EnvRestore(Vec<(&'static str, Option<std::ffi::OsString>)>);

    impl EnvRestore {
        fn capture(names: &[&'static str]) -> Self {
            Self(
                names
                    .iter()
                    .map(|name| (*name, std::env::var_os(name)))
                    .collect(),
            )
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (name, value) in self.0.drain(..) {
                if let Some(value) = value {
                    std::env::set_var(name, value);
                } else {
                    std::env::remove_var(name);
                }
            }
        }
    }

    fn selected_model(requested: &str) -> SelectedModelReport {
        SelectedModelReport {
            requested: requested.to_owned(),
            provider: "test".to_owned(),
            protocol: "test".to_owned(),
            catalog: find_model(requested),
        }
    }

    async fn test_server(
        status: u16,
        body: &'static str,
        delay: Duration,
    ) -> (String, tokio::task::JoinHandle<String>) {
        test_server_bytes(status, body.as_bytes().to_vec(), "application/json", delay).await
    }

    async fn test_server_bytes(
        status: u16,
        body: Vec<u8>,
        content_type: &'static str,
        delay: Duration,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let handle = tokio::task::spawn_blocking(move || {
            let (mut stream, _) = listener.accept().expect("accept test request");
            let mut buffer = [0_u8; 4096];
            let bytes = stream.read(&mut buffer).expect("read test request");
            std::thread::sleep(delay);
            let reason = if status == 200 { "OK" } else { "Error" };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.write_all(&body);
            String::from_utf8_lossy(&buffer[..bytes]).into_owned()
        });
        (format!("http://{address}"), handle)
    }

    fn openai_env(base_url: String) -> HashMap<String, String> {
        HashMap::from([
            ("OPENAI_API_KEY".to_owned(), "bearer-secret".to_owned()),
            ("OPENAI_BASE_URL".to_owned(), base_url),
        ])
    }

    #[test]
    fn parses_supported_options() {
        let args = vec![
            "--json".into(),
            "--live".into(),
            "--verbose".into(),
            "--timeout=2500".into(),
            "--network".into(),
            "--daemon".into(),
            "--hooks".into(),
            "--model=openai/gpt-4o".into(),
        ];
        let options = parse_options(&args).expect("options");
        assert!(options.json && options.live && options.verbose);
        assert_eq!(options.timeout_ms, 2500);
        assert_eq!(
            options.categories,
            ["network", "daemon", "hooks"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        );
        assert_eq!(options.model.as_deref(), Some("openai/gpt-4o"));
        assert!(parse_options(&["--timeout=0".into()]).is_err());
    }

    #[tokio::test]
    async fn offline_report_is_typed_and_skips_live_network() {
        let temp = tempfile::tempdir().expect("tempdir");
        let report = build_report(Some("openai/gpt-4o"), false, temp.path()).await;
        assert_eq!(report.schema_version, REPORT_SCHEMA_VERSION);
        assert!(report.selected_model.catalog.is_some());
        assert_eq!(
            report.checks.last().map(|check| check.status),
            Some(CheckStatus::Skipped)
        );
    }

    #[tokio::test]
    async fn operational_health_probe_reports_latency_and_redacts_credentials() {
        let (base_url, server) = test_server(200, "ok", Duration::ZERO).await;
        let url = format!(
            "http://user:password@{}/healthz?token=secret",
            base_url.trim_start_matches("http://")
        );
        let item = http_health_check(
            "daemon",
            &url,
            Duration::from_secs(1),
            true,
            "restart daemon",
        )
        .await;
        assert_eq!(item.status, CheckStatus::Pass);
        assert!(item.duration_ms.is_some());
        let serialized = serde_json::to_string(&item).expect("serialize check");
        assert!(!serialized.contains("password"));
        assert!(!serialized.contains("secret"));
        server.await.expect("test server");

        let failed = http_health_check(
            "daemon",
            "http://user:password@127.0.0.1:1/healthz?token=secret",
            Duration::from_millis(50),
            true,
            "restart daemon",
        )
        .await;
        assert_eq!(failed.status, CheckStatus::Fail);
        let serialized = serde_json::to_string(&failed).expect("serialize failed check");
        assert!(!serialized.contains("password"));
        assert!(!serialized.contains("secret"));
    }

    #[test]
    fn offline_report_reports_byok_ready_without_identity() {
        let _lock = crate::config::test_process_env_lock();
        let _restore = EnvRestore::capture(&[
            "MAESTRO_HOME",
            "MAESTRO_OAUTH_STORAGE_MODE",
            "MAESTRO_DISABLE_KEYCHAIN",
            crate::credential_mode::ACCESS_TOKEN_ENV,
            crate::credential_mode::ACCESS_TOKEN_FILE_ENV,
            crate::credential_mode::ORG_ID_ENV,
            crate::credential_mode::WORKSPACE_ID_ENV,
            "OPENROUTER_API_KEY",
            "MAESTRO_CONNECTION",
            "CODEX_HOME",
        ]);
        let home = tempfile::tempdir().expect("maestro home");
        let codex_home = tempfile::tempdir().expect("codex home");
        std::env::set_var("MAESTRO_HOME", home.path());
        std::env::set_var("MAESTRO_OAUTH_STORAGE_MODE", "file");
        std::env::set_var("MAESTRO_DISABLE_KEYCHAIN", "1");
        std::env::set_var("OPENROUTER_API_KEY", "or-test");
        std::env::set_var("CODEX_HOME", codex_home.path());
        for name in [
            crate::credential_mode::ACCESS_TOKEN_ENV,
            crate::credential_mode::ACCESS_TOKEN_FILE_ENV,
            crate::credential_mode::ORG_ID_ENV,
            crate::credential_mode::WORKSPACE_ID_ENV,
            "MAESTRO_CONNECTION",
        ] {
            std::env::remove_var(name);
        }

        let cwd = tempfile::tempdir().expect("cwd");
        let report = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("doctor runtime")
            .block_on(build_report(
                Some("openrouter/openai/o4-mini"),
                false,
                cwd.path(),
            ));
        let check = report
            .checks
            .iter()
            .find(|check| check.id == "credential_mode")
            .expect("credential mode check");
        assert_eq!(check.status, CheckStatus::Pass);
        assert_eq!(
            check.summary,
            "byok: a local provider credential is available"
        );
    }

    #[test]
    fn live_report_verifies_identity_and_rejects_revoked_sessions() {
        let _lock = crate::config::test_process_env_lock();
        let _restore = EnvRestore::capture(crate::credential_mode::TEST_IDENTITY_ENV_VARS);
        crate::credential_mode::install_test_identity_env();
        std::env::remove_var(crate::credential_mode::WORKSPACE_ID_ENV);
        std::env::set_var(
            crate::credential_mode::BASE_URL_ENV,
            crate::credential_mode::test_identity_base_url(),
        );
        let cwd = tempfile::tempdir().expect("cwd");
        let report = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(build_report(
                Some(crate::credential_mode::DEFAULT_MANAGED_MODEL),
                true,
                cwd.path(),
            ));
        let identity = report
            .checks
            .iter()
            .find(|check| check.id == "credential_mode")
            .expect("Identity check");
        assert_eq!(identity.status, CheckStatus::Pass, "{identity:?}");
        assert!(identity.live);
        std::env::set_var(crate::credential_mode::ACCESS_TOKEN_ENV, "inactive-token");
        let rejected = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(build_report(
                Some(crate::credential_mode::DEFAULT_MANAGED_MODEL),
                true,
                cwd.path(),
            ));
        let rejected_identity = rejected
            .checks
            .iter()
            .find(|check| check.id == "credential_mode")
            .expect("Identity check");
        assert_eq!(rejected_identity.status, CheckStatus::Fail);
        assert!(rejected_identity.live);
        assert!(
            report
                .checks
                .iter()
                .any(|check| check.id == "managed_setup")
        );
        let json = serde_json::to_string(&report).expect("report JSON");
        assert!(!json.contains("fixture-evalops-access-token"));
    }

    #[test]
    fn managed_setup_report_requires_the_matching_tenant() {
        let _lock = crate::config::test_process_env_lock();
        let _restore = EnvRestore::capture(&["MAESTRO_MANAGED_SETUP_URL"]);
        let session = crate::credential_mode::platform_session_from(
            None,
            &HashMap::from([
                (
                    crate::credential_mode::ACCESS_TOKEN_ENV.to_owned(),
                    "test-token".to_owned(),
                ),
                (
                    crate::credential_mode::ORG_ID_ENV.to_owned(),
                    "org-test".to_owned(),
                ),
                (
                    crate::credential_mode::WORKSPACE_ID_ENV.to_owned(),
                    "workspace-test".to_owned(),
                ),
            ]),
        )
        .expect("session");
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                // Independently encode the public response: scope=1, version=2, MCP=6.
                for (organization, expected) in [
                    ("org-test", CheckStatus::Pass),
                    ("other-org", CheckStatus::Warning),
                ] {
                    let mut scope = vec![0x0a, organization.len() as u8];
                    scope.extend_from_slice(organization.as_bytes());
                    scope.extend_from_slice(b"\x12\x0eworkspace-test");
                    let mut body = vec![0x0a, scope.len() as u8];
                    body.extend_from_slice(&scope);
                    body.extend_from_slice(&[0x10, 1, 0x32, 2, 0x08, 2]);
                    let (base, server) =
                        test_server_bytes(200, body, "application/proto", Duration::ZERO).await;
                    std::env::set_var("MAESTRO_MANAGED_SETUP_URL", base);
                    let report = managed_setup_check(
                        &Ok(crate::credential_mode::DetectedMode::Platform(
                            session.clone(),
                        )),
                        true,
                    )
                    .await;
                    assert_eq!(report.status, expected, "{report:?}");
                    assert!(report.live);
                    let request = server.await.expect("server");
                    assert!(
                        request.contains("/deixicpublic.v1.DeixicPublicService/GetClientSetup")
                    );
                    assert!(
                        !serde_json::to_string(&report)
                            .expect("report")
                            .contains("test-token")
                    );
                }
            });
    }

    #[test]
    fn config_check_uses_runtime_config_schema() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_dir = temp.path().join(".composer");
        std::fs::create_dir_all(&config_dir).expect("config directory");
        std::fs::write(config_dir.join("config.toml"), "model = 123\n")
            .expect("invalid runtime config");
        let checks = config_checks(temp.path());
        assert!(checks.iter().any(|check| {
            check
                .summary
                .contains(temp.path().to_string_lossy().as_ref())
                && check.status == CheckStatus::Fail
        }));
    }

    #[test]
    fn config_paths_include_project_files_and_dedupe() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = config_paths(temp.path());
        assert!(
            paths.contains(&temp.path().join(".maestro/config.toml")),
            "{paths:?}"
        );
        assert!(
            paths.contains(&temp.path().join(".composer/config.toml")),
            "{paths:?}"
        );
        let keys: Vec<_> = paths
            .iter()
            .map(|path| normalize_config_path(path))
            .collect();
        let mut unique = keys.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            keys.len(),
            unique.len(),
            "config_paths must not repeat: {paths:?}"
        );
    }

    #[test]
    fn normalize_config_path_collapses_dot_components() {
        assert_eq!(
            normalize_config_path(Path::new("/tmp/foo/./config.toml")),
            normalize_config_path(Path::new("/tmp/foo/config.toml"))
        );
    }

    #[test]
    fn metadata_requests_only_probe_documented_protocols() {
        let client = reqwest::Client::new();

        let openai = ProviderRegistry::resolve(
            "openai/gpt-4o",
            &openai_env("https://example.com/v1".to_owned()),
        )
        .expect("OpenAI provider");
        let openai_request = metadata_request(&client, &openai, "openai-secret")
            .expect("OpenAI request")
            .expect("supported OpenAI probe")
            .0
            .build()
            .expect("build OpenAI request");
        assert_eq!(openai_request.url().path(), "/v1/models");
        assert_eq!(
            openai_request.headers()[reqwest::header::AUTHORIZATION],
            "Bearer openai-secret"
        );

        let anthropic_env = HashMap::from([
            (
                "ANTHROPIC_API_KEY".to_owned(),
                "anthropic-secret".to_owned(),
            ),
            (
                "ANTHROPIC_BASE_URL".to_owned(),
                "https://example.com/v1".to_owned(),
            ),
        ]);
        let anthropic =
            ProviderRegistry::resolve("anthropic/claude-sonnet-4-5-20250514", &anthropic_env)
                .expect("Anthropic provider");
        assert!(
            metadata_request(&client, &anthropic, "anthropic-secret")
                .expect("Anthropic request")
                .is_none()
        );

        let google_env = HashMap::from([
            ("GEMINI_API_KEY".to_owned(), "google-secret".to_owned()),
            (
                "GEMINI_BASE_URL".to_owned(),
                "https://example.com".to_owned(),
            ),
        ]);
        let google = ProviderRegistry::resolve("google/gemini-2.5-pro", &google_env)
            .expect("Google provider");
        assert!(
            metadata_request(&client, &google, "google-secret")
                .expect("Google request")
                .is_none()
        );
    }

    #[tokio::test]
    async fn provider_model_mismatch_fails_report() {
        let temp = tempfile::tempdir().expect("tempdir");
        let report = build_report(Some("anthropic/gpt-4o"), false, temp.path()).await;
        assert!(!report.ok);
        assert_eq!(report_exit_code(&report), 1);
        assert!(
            report
                .checks
                .iter()
                .any(|check| { check.id == "model_catalog" && check.status == CheckStatus::Fail })
        );
    }

    #[tokio::test]
    async fn live_failures_are_fatal_and_redacted() {
        for status in [401, 403, 500] {
            let (base_url, server) = test_server(status, "{}", Duration::ZERO).await;
            let configured_url = format!(
                "http://user:password@{}?token=query-secret#fragment",
                base_url.trim_start_matches("http://")
            );
            let check = live_metadata_check_with_env(
                &selected_model("openai/gpt-4o"),
                &openai_env(configured_url),
            )
            .await;
            assert_eq!(check.status, CheckStatus::Fail);
            let serialized = serde_json::to_string(&check).expect("serialize check");
            for secret in [
                "user",
                "password",
                "query-secret",
                "fragment",
                "bearer-secret",
            ] {
                assert!(
                    !serialized.contains(secret),
                    "leaked {secret}: {serialized}"
                );
            }
            server.await.expect("test server");
        }
    }

    #[tokio::test]
    async fn live_timeout_and_missing_model_fail() {
        let (base_url, server) = test_server(
            200,
            r#"{"data":[{"id":"gpt-4o"}]}"#,
            Duration::from_millis(200),
        )
        .await;
        let timeout =
            live_metadata_check_with_env(&selected_model("openai/gpt-4o"), &openai_env(base_url))
                .await;
        assert_eq!(timeout.status, CheckStatus::Fail);
        assert!(
            timeout
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("timed out"))
        );
        server.await.expect("timeout server");

        let (base_url, server) = test_server(200, r#"{"data":[]}"#, Duration::ZERO).await;
        let missing =
            live_metadata_check_with_env(&selected_model("openai/gpt-4o"), &openai_env(base_url))
                .await;
        assert_eq!(missing.status, CheckStatus::Fail);
        let report = DoctorReport {
            schema_version: REPORT_SCHEMA_VERSION,
            ok: false,
            live_requested: true,
            selected_model: selected_model("openai/gpt-4o"),
            checks: vec![missing],
        };
        assert_eq!(report_exit_code(&report), 1);
        server.await.expect("missing model server");
    }

    #[tokio::test]
    async fn unsupported_protocol_is_skipped() {
        let env = HashMap::from([
            ("AZURE_OPENAI_API_KEY".to_owned(), "secret".to_owned()),
            (
                "AZURE_OPENAI_ENDPOINT".to_owned(),
                "https://example.com".to_owned(),
            ),
        ]);
        let check =
            live_metadata_check_with_env(&selected_model("azure-openai/gpt-4o"), &env).await;
        assert_eq!(check.status, CheckStatus::Skipped);
    }

    #[test]
    fn auth_health_reports_env_key_without_printing_secret() {
        let env = HashMap::from([(
            "OPENAI_API_KEY".to_owned(),
            "sk-plain-auth-health-secret".to_owned(),
        )]);
        let checks = auth_health_checks(&env);
        let openai = checks
            .iter()
            .find(|item| item.summary.starts_with("openai:"))
            .expect("openai auth health check");
        assert_eq!(openai.status, CheckStatus::Pass);
        assert!(openai.summary.contains("OPENAI_API_KEY is set"));
        let serialized = serde_json::to_string(&checks).expect("serialize checks");
        assert!(
            !serialized.contains("sk-plain-auth-health-secret"),
            "leaked secret: {serialized}"
        );
    }

    #[test]
    fn auth_health_resolves_op_reference() {
        let _fake = crate::ai::op_secret::test_support::FakeOp::install();
        let env = HashMap::from([(
            "OPENAI_API_KEY".to_owned(),
            "op://vault/item/doctor-health".to_owned(),
        )]);
        let checks = auth_health_checks(&env);
        let openai = checks
            .iter()
            .find(|item| item.summary.starts_with("openai:"))
            .expect("openai auth health check");
        assert_eq!(openai.status, CheckStatus::Pass);
        assert!(openai.summary.contains("op:// reference resolves"));
        let serialized = serde_json::to_string(&checks).expect("serialize checks");
        assert!(
            !serialized.contains("resolved-secret-value"),
            "leaked secret: {serialized}"
        );
    }

    #[test]
    fn auth_health_flags_unresolvable_op_reference() {
        let _fake = crate::ai::op_secret::test_support::FakeOp::install();
        let env = HashMap::from([(
            "ANTHROPIC_API_KEY".to_owned(),
            "op://vault/item/missing".to_owned(),
        )]);
        let checks = auth_health_checks(&env);
        let anthropic = checks
            .iter()
            .find(|item| item.summary.starts_with("anthropic:"))
            .expect("anthropic auth health check");
        assert_eq!(anthropic.status, CheckStatus::Fail);
        let detail = anthropic.detail.as_deref().expect("failure detail");
        assert!(
            detail.contains("1Password CLI"),
            "detail should mention the op CLI: {detail}"
        );
    }

    #[test]
    fn auth_health_warns_when_no_credential() {
        let checks = auth_health_checks(&HashMap::new());
        assert_eq!(checks.len(), AUTH_HEALTH_PROVIDERS.len());
        for provider in ["anthropic", "google", "xai"] {
            let item = checks
                .iter()
                .find(|check| check.summary.starts_with(&format!("{provider}:")))
                .expect("provider auth health check");
            assert_eq!(item.status, CheckStatus::Warning, "{}", item.summary);
            assert!(item.summary.contains("no credential found"));
        }
        // openai / openai-codex may additionally have stored credentials on
        // disk; only assert those checks exist and never print a secret.
        assert!(
            checks
                .iter()
                .any(|check| check.summary.starts_with("openai:"))
        );
        assert!(
            checks
                .iter()
                .any(|check| check.summary.starts_with("openai-codex:"))
        );
        for check in &checks {
            assert!(
                !check.summary.contains("sk-")
                    && check
                        .detail
                        .as_deref()
                        .is_none_or(|d| !d.contains("sk-") && !d.contains("eyJ")),
                "auth_health must not print secrets: {:?}",
                check
            );
        }
    }
}
