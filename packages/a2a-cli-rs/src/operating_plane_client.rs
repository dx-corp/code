//! Platform agent operating-plane lookup client.
//!
//! Ports `src/platform/operating-plane-client.ts` for the native CLI. Resolves
//! EvalOps/Platform service configuration, builds filter query URLs, and fetches
//! the content-bearing inspection payload (summary redaction happens downstream).

use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::{Client, Response, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

pub const OPERATING_PLANE_RUNS_PATH: &str = "/v1/agent-operating-plane/runs";

const DEFAULT_TIMEOUT_MS: u64 = 2_000;
const DEFAULT_MAX_ATTEMPTS: usize = 2;
const INITIAL_RETRY_DELAY: Duration = Duration::from_millis(100);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(1);

const BASE_URL_ENV_VARS: &[&str] = &[
    "MAESTRO_AGENT_OPERATING_PLANE_URL",
    "MAESTRO_EVALOPS_BASE_URL",
];

const TOKEN_ENV_VARS: &[&str] = &[
    "MAESTRO_AGENT_OPERATING_PLANE_TOKEN",
    "MAESTRO_EVALOPS_ACCESS_TOKEN",
];

const ORGANIZATION_ENV_VARS: &[&str] = &[
    "MAESTRO_AGENT_OPERATING_PLANE_ORG_ID",
    "MAESTRO_EVALOPS_ORG_ID",
];

const WORKSPACE_ENV_VARS: &[&str] = &[
    "MAESTRO_AGENT_OPERATING_PLANE_WORKSPACE_ID",
    "MAESTRO_EVALOPS_WORKSPACE_ID",
];

const TIMEOUT_ENV_VARS: &[&str] = &[
    "MAESTRO_AGENT_OPERATING_PLANE_TIMEOUT_MS",
    "AGENT_OPERATING_PLANE_TIMEOUT_MS",
    "MAESTRO_AGENT_RUNTIME_TIMEOUT_MS",
    "AGENT_RUNTIME_SERVICE_TIMEOUT_MS",
];

const MAX_ATTEMPTS_ENV_VARS: &[&str] = &[
    "MAESTRO_AGENT_OPERATING_PLANE_MAX_ATTEMPTS",
    "AGENT_OPERATING_PLANE_MAX_ATTEMPTS",
    "MAESTRO_AGENT_RUNTIME_MAX_ATTEMPTS",
    "AGENT_RUNTIME_SERVICE_MAX_ATTEMPTS",
];

const BASE_URL_SUFFIXES: &[&str] = &[OPERATING_PLANE_RUNS_PATH, "/v1/agent-operating-plane"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformServiceConfig {
    pub base_url: String,
    pub token: Option<String>,
    pub organization_id: Option<String>,
    pub workspace_id: Option<String>,
    pub timeout_ms: u64,
    pub max_attempts: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OperatingPlaneRunQuery {
    pub workspace_id: Option<String>,
    pub run_id: Option<String>,
    pub work_envelope_id: Option<String>,
    pub autonomy_session_id: Option<String>,
    pub agent_id: Option<String>,
    pub thread_id: Option<String>,
    pub channel_thread_id: Option<String>,
    pub trace_id: Option<String>,
    pub session_id: Option<String>,
    pub evidence_id: Option<String>,
    pub gateway_authenticated_subject: Option<String>,
    pub auth_subject: Option<String>,
    pub audience: Option<String>,
    pub include_gates: Option<bool>,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OperatingPlaneInspection {
    pub contract_version: String,
    pub generated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_sources: Option<Vec<String>>,
    #[serde(default)]
    pub runs: Vec<OperatingPlaneRun>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OperatingPlaneRun {
    pub agent_run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_run_step_id: Option<String>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub surface: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<OperatingPlaneIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redaction_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub withholding_reasons: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_sources: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_refs: Option<Vec<OperatingPlaneEvidence>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_items: Option<Vec<OperatingPlaneWorkItem>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<OperatingPlaneUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_signals: Option<OperatingPlaneRuntimeSignals>,
    /// Retained only for deserialization parity; never surfaced by the summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_attributes: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OperatingPlaneIdentity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_authenticated_subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_authenticated_user_subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_authenticated_service: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OperatingPlaneEvidence {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    #[serde(default)]
    pub available: bool,
    /// Content-bearing; never forwarded by the content-free summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OperatingPlaneWorkItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocker: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OperatingPlaneUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_micros: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OperatingPlaneRuntimeSignals {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_bound: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_observed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_observed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_observed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_linked: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_linked: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_attributed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing_signals: Option<Vec<String>>,
}

pub fn resolve_operating_plane_service_config() -> Option<PlatformServiceConfig> {
    let base_url = get_env_value(BASE_URL_ENV_VARS)?;
    let organization_id = get_env_value(ORGANIZATION_ENV_VARS);
    let token = get_env_value(TOKEN_ENV_VARS);
    let stored = if organization_id.is_none() || token.is_none() {
        maestro_local_host::init_cli::load_evalops_snapshot()
            .ok()
            .flatten()
    } else {
        None
    };
    let organization_id = organization_id.or_else(|| {
        stored
            .as_ref()
            .and_then(|snapshot| snapshot.organization_id.clone())
    });
    organization_id.as_ref()?;
    let token = token.or_else(|| stored.as_ref().map(|snapshot| snapshot.access.clone()));
    token.as_ref()?;
    let workspace_id = require_workspace_scope(get_env_value(WORKSPACE_ENV_VARS));
    workspace_id.as_ref()?;
    Some(PlatformServiceConfig {
        base_url: normalize_base_url(&base_url, BASE_URL_SUFFIXES),
        token,
        organization_id,
        workspace_id,
        timeout_ms: parse_positive_int(
            get_env_value(TIMEOUT_ENV_VARS).as_deref(),
            DEFAULT_TIMEOUT_MS,
        ),
        max_attempts: parse_positive_int(
            get_env_value(MAX_ATTEMPTS_ENV_VARS).as_deref(),
            DEFAULT_MAX_ATTEMPTS as u64,
        ) as usize,
    })
}

pub fn build_operating_plane_runs_url(
    config: &PlatformServiceConfig,
    query: &OperatingPlaneRunQuery,
) -> Result<String> {
    let base = config.base_url.trim_end_matches('/');
    let mut url = Url::parse(&format!("{base}{OPERATING_PLANE_RUNS_PATH}"))
        .with_context(|| format!("invalid operating-plane base URL: {}", config.base_url))?;
    {
        let mut params = url.query_pairs_mut();
        add_string_param(
            &mut params,
            "workspace_id",
            query
                .workspace_id
                .as_deref()
                .or(config.workspace_id.as_deref()),
        );
        add_string_param(&mut params, "run_id", query.run_id.as_deref());
        add_string_param(
            &mut params,
            "work_envelope_id",
            query.work_envelope_id.as_deref(),
        );
        add_string_param(
            &mut params,
            "autonomy_session_id",
            query.autonomy_session_id.as_deref(),
        );
        add_string_param(&mut params, "agent_id", query.agent_id.as_deref());
        add_string_param(&mut params, "thread_id", query.thread_id.as_deref());
        add_string_param(
            &mut params,
            "channel_thread_id",
            query.channel_thread_id.as_deref(),
        );
        add_string_param(&mut params, "trace_id", query.trace_id.as_deref());
        add_string_param(&mut params, "session_id", query.session_id.as_deref());
        add_string_param(&mut params, "evidence_id", query.evidence_id.as_deref());
        add_string_param(
            &mut params,
            "gateway_authenticated_subject",
            query.gateway_authenticated_subject.as_deref(),
        );
        add_string_param(&mut params, "auth_subject", query.auth_subject.as_deref());
        add_string_param(&mut params, "audience", query.audience.as_deref());
        if let Some(include_gates) = query.include_gates {
            params.append_pair(
                "include_gates",
                if include_gates { "true" } else { "false" },
            );
        }
        if let Some(limit) = query.limit {
            params.append_pair("limit", &limit.to_string());
        }
    }
    Ok(url.to_string())
}

pub async fn inspect_operating_plane_runs(
    query: &OperatingPlaneRunQuery,
    config: Option<PlatformServiceConfig>,
) -> Result<OperatingPlaneInspection> {
    let config = match config.or_else(resolve_operating_plane_service_config) {
        Some(config) => config,
        None => bail!("agent operating plane service is not configured"),
    };
    let url = build_operating_plane_runs_url(&config, query)?;
    let client = Client::builder()
        .timeout(Duration::from_millis(config.timeout_ms.max(1)))
        .build()
        .context("failed to create operating-plane HTTP client")?;
    let response = fetch_with_retries(&client, &config, &url).await?;
    parse_operating_plane_response(response).await
}

async fn fetch_with_retries(
    client: &Client,
    config: &PlatformServiceConfig,
    url: &str,
) -> Result<Response> {
    let max_attempts = config.max_attempts.max(1);
    for attempt in 0..max_attempts {
        let mut request = client.get(url).header("Content-Type", "application/json");
        if let Some(token) = &config.token {
            request = request.bearer_auth(token);
        }
        if let Some(org) = &config.organization_id {
            request = request.header("X-Organization-ID", org);
        }
        match request.send().await {
            Ok(response) if !retryable_status(response.status()) || attempt + 1 == max_attempts => {
                return Ok(response);
            }
            Ok(response) => {
                tokio::time::sleep(retry_delay(&response, attempt)).await;
            }
            Err(error) if attempt + 1 == max_attempts => {
                return Err(error).context("agent operating plane service request failed");
            }
            Err(_) => tokio::time::sleep(exponential_delay(attempt)).await,
        }
    }
    unreachable!("operating-plane retry loop always returns")
}

async fn parse_operating_plane_response(response: Response) -> Result<OperatingPlaneInspection> {
    let status = response.status();
    let status_text = status.canonical_reason().unwrap_or("Unknown").to_owned();
    let text = response
        .text()
        .await
        .context("failed to read operating-plane response")?;
    if !status.is_success() {
        bail!(
            "agent operating plane service returned {}: {}",
            status.as_u16(),
            if text.is_empty() { &status_text } else { &text }
        );
    }
    if text.trim().is_empty() {
        bail!("agent operating plane service returned empty response");
    }
    serde_json::from_str(&text).context("agent operating plane service returned invalid JSON")
}

fn retryable_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 429) || status.is_server_error()
}

fn exponential_delay(attempt: usize) -> Duration {
    let multiplier = 1_u32.checked_shl(attempt as u32).unwrap_or(u32::MAX);
    INITIAL_RETRY_DELAY
        .saturating_mul(multiplier)
        .min(MAX_RETRY_DELAY)
}

fn retry_delay(response: &Response, attempt: usize) -> Duration {
    response
        .headers()
        .get("retry-after-ms")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or_else(|| exponential_delay(attempt))
        .min(MAX_RETRY_DELAY)
}

fn add_string_param(
    params: &mut url::form_urlencoded::Serializer<'_, url::UrlQuery<'_>>,
    name: &str,
    value: Option<&str>,
) {
    if let Some(normalized) = trim_string(value) {
        params.append_pair(name, normalized);
    }
}

pub fn normalize_base_url(base_url: &str, suffixes: &[&str]) -> String {
    let mut normalized = base_url.trim().trim_end_matches('/').to_owned();
    for suffix in suffixes {
        if let Some(stripped) = normalized.strip_suffix(suffix) {
            normalized = stripped.trim_end_matches('/').to_owned();
        }
    }
    normalized
}

fn get_env_value(names: &[&str]) -> Option<String> {
    for name in names {
        if let Ok(value) = std::env::var(name) {
            if let Some(trimmed) = trim_string(Some(value.as_str())) {
                return Some(trimmed.to_owned());
            }
        }
    }
    None
}

fn parse_positive_int(value: Option<&str>, fallback: u64) -> u64 {
    value
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|parsed| *parsed > 0)
        .unwrap_or(fallback)
}

pub fn trim_string(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn require_workspace_scope(workspace_id: Option<String>) -> Option<String> {
    workspace_id.filter(|value| !value.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_base_url_strips_operating_plane_suffixes() {
        assert_eq!(
            normalize_base_url(
                "https://platform.test/v1/agent-operating-plane/runs",
                BASE_URL_SUFFIXES
            ),
            "https://platform.test"
        );
        assert_eq!(
            normalize_base_url(
                "https://platform.test/v1/agent-operating-plane/",
                BASE_URL_SUFFIXES
            ),
            "https://platform.test"
        );
    }

    #[test]
    fn builds_lookup_urls_for_operator_filters() {
        let url = build_operating_plane_runs_url(
            &PlatformServiceConfig {
                base_url: "https://platform.test".to_owned(),
                token: Some("token".to_owned()),
                organization_id: Some("org".to_owned()),
                workspace_id: Some("ws_evalops".to_owned()),
                timeout_ms: 2000,
                max_attempts: 2,
            },
            &OperatingPlaneRunQuery {
                thread_id: Some("C123:1740000000.000100".to_owned()),
                trace_id: Some("trace-1".to_owned()),
                session_id: Some("maestro-session-1".to_owned()),
                evidence_id: Some("gateway:req_123".to_owned()),
                gateway_authenticated_subject: Some("user:alice".to_owned()),
                audience: Some("audit".to_owned()),
                include_gates: Some(false),
                limit: Some(25),
                ..OperatingPlaneRunQuery::default()
            },
        )
        .unwrap();
        let parsed = Url::parse(&url).unwrap();
        assert_eq!(parsed.path(), OPERATING_PLANE_RUNS_PATH);
        let pairs: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(
            pairs.get("workspace_id").map(String::as_str),
            Some("ws_evalops")
        );
        assert_eq!(
            pairs.get("thread_id").map(String::as_str),
            Some("C123:1740000000.000100")
        );
        assert_eq!(pairs.get("trace_id").map(String::as_str), Some("trace-1"));
        assert_eq!(
            pairs.get("session_id").map(String::as_str),
            Some("maestro-session-1")
        );
        assert_eq!(
            pairs.get("evidence_id").map(String::as_str),
            Some("gateway:req_123")
        );
        assert_eq!(
            pairs
                .get("gateway_authenticated_subject")
                .map(String::as_str),
            Some("user:alice")
        );
        assert_eq!(pairs.get("audience").map(String::as_str), Some("audit"));
        assert_eq!(
            pairs.get("include_gates").map(String::as_str),
            Some("false")
        );
        assert_eq!(pairs.get("limit").map(String::as_str), Some("25"));
    }

    #[test]
    fn operating_plane_scope_does_not_derive_workspace_from_organization() {
        assert_eq!(require_workspace_scope(None), None);
        assert_eq!(
            require_workspace_scope(Some("ws_explicit".to_owned())),
            Some("ws_explicit".to_owned())
        );
    }
}
