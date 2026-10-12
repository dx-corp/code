//! Native `maestro memory` account-memory and shared-memory commands.

use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::{Client, Response, StatusCode};
use serde::Deserialize;
use serde_json::Value;

use crate::init_cli::{AgentMcpClient, EvalOpsCredentialSnapshot, load_evalops_snapshot};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_ATTEMPTS: usize = 2;
const INITIAL_RETRY_DELAY: Duration = Duration::from_millis(100);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Eq)]
struct SharedMemoryConfig {
    base_url: String,
    api_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AccountMemoryConfig {
    api_key: String,
    endpoint: String,
    integration_profile: String,
    memory_mode: String,
    organization_id: String,
    runtime_owner: String,
    shim_type: String,
    trace_mode: String,
    workspace_id: String,
}

impl AccountMemoryConfig {
    fn from_snapshot(snapshot: &EvalOpsCredentialSnapshot) -> Result<Self> {
        let organization_id = snapshot
            .organization_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .context(
                "stored EvalOps registration has no explicit organization id; run `deixic-code init`",
            )?
            .to_owned();
        let workspace_id = snapshot
            .agent_mcp
            .as_ref()
            .and_then(|metadata| metadata.workspace_id.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .context("stored EvalOps registration has no explicit workspace id; run `deixic-code init --workspace-id <id>`")?
            .to_owned();
        let metadata = snapshot.agent_mcp.as_ref().context(
            "stored EvalOps credentials have no agent registration; run `deixic-code init`",
        )?;
        let required = |value: Option<&str>, field: &str| {
            value
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .with_context(|| {
                    format!("stored EvalOps registration has no {field}; run `deixic-code init`")
                })
        };
        let memory_mode = required(metadata.memory_mode.as_deref(), "memory mode")?;
        if memory_mode != "durable" {
            bail!("{}", crate::localization::cli_locale().format("stored EvalOps registration memory mode is not durable; run `deixic-code init --memory-mode durable`", &[]));
        }
        Ok(Self {
            api_key: required(metadata.api_key.as_deref(), "API key")?,
            endpoint: required(metadata.endpoint.as_deref(), "MCP endpoint")?,
            integration_profile: required(
                metadata.integration_profile.as_deref(),
                "integration profile",
            )?,
            memory_mode,
            organization_id,
            runtime_owner: required(metadata.runtime_owner.as_deref(), "runtime owner")?,
            shim_type: required(metadata.shim_type.as_deref(), "shim type")?,
            trace_mode: required(metadata.trace_mode.as_deref(), "trace mode")?,
            workspace_id,
        })
    }
}

fn account_registration_arguments(config: &AccountMemoryConfig) -> Value {
    serde_json::json!({
        "agent_type": "maestro",
        "capabilities": ["maestro:memory"],
        "integration_profile": config.integration_profile,
        "memory_mode": config.memory_mode,
        "organization_id": config.organization_id,
        "runtime_owner": config.runtime_owner,
        "scopes": ["memories:read", "memories:write"],
        "shim_type": config.shim_type,
        "surface": "cli",
        "trace_mode": config.trace_mode,
        "workspace_id": config.workspace_id,
    })
}

fn memory_write_governance_arguments(config: &AccountMemoryConfig) -> Value {
    serde_json::json!({
        "action_type": "evalops_store_memory",
        "action_payload": format!(
            "explicit account memory write; organization_id={}; workspace_id={}; scope=team",
            config.organization_id, config.workspace_id
        ),
        "declared_risk_level": "MEDIUM",
    })
}

fn account_store_arguments(config: &AccountMemoryConfig, fact: &str) -> Value {
    serde_json::json!({
        "content": fact,
        "scope": "team",
        "team_id": config.workspace_id,
        "type": "reference",
        "source": "maestro-cli",
        "confidence": 1.0,
    })
}

fn account_recall_arguments(config: &AccountMemoryConfig, query: &str) -> Value {
    serde_json::json!({
        "query": query,
        "scope": "team",
        "team_id": config.workspace_id,
        "top_k": 3,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AccountMemoryStatus {
    working: bool,
    reason: String,
}

fn account_memory_status(config: &AccountMemoryConfig, summary: &Value) -> AccountMemoryStatus {
    let not_working = |reason: &str| AccountMemoryStatus {
        working: false,
        reason: reason.to_owned(),
    };
    let Some(session) = summary.get("session") else {
        return not_working("live control-plane status has no session");
    };
    if session.get("registered").and_then(Value::as_bool) != Some(true)
        || session
            .pointer("/control_claims/registered")
            .and_then(Value::as_bool)
            != Some(true)
    {
        return not_working("live control-plane session is not registered");
    }
    if session
        .pointer("/control_claims/authenticated")
        .and_then(Value::as_bool)
        != Some(true)
    {
        return not_working("live control-plane session is not authenticated");
    }
    if session
        .pointer("/control_claims/governed")
        .and_then(Value::as_bool)
        != Some(true)
    {
        return not_working("live control-plane session is not governed");
    }
    if session.get("organization_id").and_then(Value::as_str)
        != Some(config.organization_id.as_str())
    {
        return not_working("live organization does not match the stored registration");
    }
    if session.get("workspace_id").and_then(Value::as_str) != Some(config.workspace_id.as_str()) {
        return not_working("live workspace does not match the stored registration");
    }
    if session.get("memory_mode").and_then(Value::as_str) != Some("durable") {
        return not_working("live memory mode is not durable");
    }
    if session
        .pointer("/control_claims/memory_writable")
        .and_then(Value::as_bool)
        != Some(true)
    {
        return not_working("live memory write capability is unavailable");
    }
    let scopes = session
        .get("scopes_granted")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    if !scopes.contains(&"memories:read") {
        return not_working("live memory read scope is unavailable");
    }
    if !scopes.contains(&"memories:write") {
        return not_working("live memory write scope is unavailable");
    }
    AccountMemoryStatus {
        working: true,
        reason: "live exact-scope recall and governed writes are available".to_owned(),
    }
}

fn validate_memory_write_gate(check: &Value, approval: Option<&Value>) -> Result<()> {
    let decision = check
        .get("decision")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    match decision.as_str() {
        "allow" | "auto_approved" => Ok(()),
        "require_approval" => {
            let approval_id = check
                .get("approval_id")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .context("memory write requires approval but returned no approval id")?;
            let approval = approval.context("memory write approval was not checked")?;
            if approval.get("approval_id").and_then(Value::as_str) != Some(approval_id) {
                bail!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "memory write approval id did not match the governance decision",
                        &[]
                    )
                );
            }
            let state = approval
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if state.eq_ignore_ascii_case("approved") {
                Ok(())
            } else {
                bail!(
                    "{}",
                    crate::localization::cli_locale()
                        .format("memory write approval is {0}", &[(state).to_string()])
                )
            }
        }
        "deny" => bail!(
            "{}",
            crate::localization::cli_locale().format(
                "memory write denied: {0}",
                &[(check
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("governance denied the write"))
                .to_string()]
            )
        ),
        _ => bail!(
            "{}",
            crate::localization::cli_locale()
                .format("memory write governance returned an unknown decision", &[])
        ),
    }
}

async fn register_account_memory_session(
    client: &mut AgentMcpClient,
    config: &AccountMemoryConfig,
) -> Result<Value> {
    let registration: Value = client
        .call_tool("evalops_register", account_registration_arguments(config))
        .await
        .context("register account-memory session")?;
    if registration.get("registered").and_then(Value::as_bool) != Some(true)
        || registration
            .get("agent_id")
            .and_then(Value::as_str)
            .is_none_or(|value| value.trim().is_empty())
    {
        bail!(
            "{}",
            crate::localization::cli_locale()
                .format("EvalOps did not register the account-memory session", &[])
        );
    }
    let summary: Value = client
        .call_tool("evalops_control_plane_summary", serde_json::json!({}))
        .await
        .context("load live account-memory status")?;
    let status = account_memory_status(config, &summary);
    if !status.working {
        bail!(
            "{}",
            crate::localization::cli_locale().format(
                "account memory is not working: {0}",
                std::slice::from_ref(&(status.reason))
            )
        );
    }
    Ok(summary)
}

async fn remember_account_memory(config: &AccountMemoryConfig, fact: &str) -> Result<Value> {
    let mut client = AgentMcpClient::connect(&config.endpoint, &config.api_key)
        .await
        .context("connect to EvalOps account memory")?;
    let result = async {
        register_account_memory_session(&mut client, config).await?;
        let check: Value = client
            .call_tool(
                "evalops_check_action",
                memory_write_governance_arguments(config),
            )
            .await
            .context("govern account-memory write")?;
        let approval = if check
            .get("decision")
            .and_then(Value::as_str)
            .is_some_and(|decision| decision.eq_ignore_ascii_case("require_approval"))
        {
            let approval_id = check
                .get("approval_id")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .context("memory write requires approval but returned no approval id")?;
            Some(
                client
                    .call_tool::<Value>(
                        "evalops_check_approval",
                        serde_json::json!({"approval_id": approval_id, "wait": true}),
                    )
                    .await
                    .context("wait for account-memory approval")?,
            )
        } else {
            None
        };
        validate_memory_write_gate(&check, approval.as_ref())?;
        let stored: Value = client
            .call_tool(
                "evalops_store_memory",
                account_store_arguments(config, fact),
            )
            .await
            .context("store explicit account memory")?;
        if stored.get("available").and_then(Value::as_bool) != Some(true)
            || stored.get("stored").and_then(Value::as_bool) != Some(true)
        {
            bail!(
                "{}",
                crate::localization::cli_locale().format(
                    "account memory did not confirm the write: {0}",
                    &[(stored
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("write unavailable"))
                    .to_string()]
                )
            );
        }
        Ok(stored)
    }
    .await;
    client.close().await;
    result
}

async fn recall_account_memory(config: &AccountMemoryConfig, query: &str) -> Result<Value> {
    let mut client = AgentMcpClient::connect(&config.endpoint, &config.api_key)
        .await
        .context("connect to EvalOps account memory")?;
    let result = async {
        register_account_memory_session(&mut client, config).await?;
        let recalled: Value = client
            .call_tool("evalops_recall", account_recall_arguments(config, query))
            .await
            .context("recall account memory")?;
        if recalled.get("available").and_then(Value::as_bool) != Some(true) {
            bail!(
                "{}",
                crate::localization::cli_locale().format(
                    "account memory recall is unavailable: {0}",
                    &[(recalled
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("control plane returned unavailable"))
                    .to_string()]
                )
            );
        }
        Ok(recalled)
    }
    .await;
    client.close().await;
    result
}

async fn live_account_memory_status(config: &AccountMemoryConfig) -> Result<AccountMemoryStatus> {
    let mut client = AgentMcpClient::connect(&config.endpoint, &config.api_key)
        .await
        .context("connect to EvalOps account memory")?;
    let result = async {
        let summary = register_account_memory_session(&mut client, config).await?;
        Ok(account_memory_status(config, &summary))
    }
    .await;
    client.close().await;
    result
}

fn load_account_memory_config() -> Result<AccountMemoryConfig> {
    let snapshot = load_evalops_snapshot()?
        .context("no stored EvalOps credentials; run `deixic-code init --workspace-id <id>`")?;
    AccountMemoryConfig::from_snapshot(&snapshot)
}

fn format_account_memory_status(status: &AccountMemoryStatus) -> String {
    let output = if status.working {
        format!("Account memory: working\n{}", status.reason)
    } else {
        format!("Account memory: not working\n{}", status.reason)
    };
    crate::output_sanitize::sanitize_control_chars(&output)
}

fn format_local_account_memory_status(snapshot: Option<&EvalOpsCredentialSnapshot>) -> String {
    match snapshot.map(AccountMemoryConfig::from_snapshot) {
        Some(Ok(_)) => "Account memory: configured for an exact organization/workspace. Run `deixic-code memory status` for live working/not working status.".to_owned(),
        Some(Err(error)) => format!("Account memory: not working. {error}"),
        None => "Account memory: not working. Run `deixic-code init --workspace-id <id>`.".to_owned(),
    }
}

pub fn local_account_memory_status() -> String {
    match load_evalops_snapshot() {
        Ok(snapshot) => format_local_account_memory_status(snapshot.as_ref()),
        Err(error) => format!("Account memory: not working. {error}"),
    }
}

fn format_account_recall(response: &Value) -> Result<String> {
    if response.get("available").and_then(Value::as_bool) != Some(true) {
        bail!(
            "{}",
            crate::localization::cli_locale().format(
                "account memory recall is unavailable: {0}",
                &[(response
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("control plane returned unavailable"))
                .to_string()]
            )
        );
    }
    let results = response
        .get("results")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if results.is_empty() {
        return Ok("Account memory: working\nNo matching memories found.".to_owned());
    }
    let mut lines = vec!["Account memory: working".to_owned()];
    for (index, result) in results.iter().take(3).enumerate() {
        let content = result
            .get("content")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("Memory content unavailable");
        lines.push(format!("{}. {content}", index + 1));
    }
    Ok(crate::output_sanitize::sanitize_control_chars(
        &lines.join("\n"),
    ))
}

#[derive(Debug, Default, Deserialize)]
struct CapabilitiesResponse {
    supports_sync: Option<bool>,
    supports_gzip: Option<bool>,
    max_body_bytes: Option<u64>,
    max_events_batch: Option<u64>,
    max_events: Option<u64>,
    max_event_payload_bytes: Option<u64>,
    max_event_type_length: Option<u64>,
    max_event_id_length: Option<u64>,
    max_session_id_length: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct ServiceMetricsResponse {
    status: Option<String>,
    now: Option<String>,
    capabilities: Option<CapabilitiesResponse>,
}

#[derive(Debug, Default, Deserialize)]
struct SessionMeta {
    last_seq: Option<u64>,
    min_seq: Option<u64>,
    updated_at: Option<String>,
    event_count: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct SessionMetricsResponse {
    meta: Option<SessionMeta>,
    metrics: Option<BTreeMap<String, Value>>,
}

#[derive(Debug, Default, Deserialize)]
struct AuditResponse {
    items: Option<Vec<BTreeMap<String, Value>>>,
}

pub async fn run_memory(args: &[String]) -> Result<i32> {
    let subcommand = args.first().map(String::as_str).unwrap_or("status");
    match subcommand {
        "help" | "--help" | "-h" => {
            println!("{}", memory_help());
            Ok(0)
        }
        "status" => {
            let shared_memory_configured = std::env::var("MAESTRO_SHARED_MEMORY_BASE").is_ok();
            let mut exit = 0;
            match load_evalops_snapshot() {
                Ok(Some(snapshot)) => match AccountMemoryConfig::from_snapshot(&snapshot) {
                    Ok(config) => match live_account_memory_status(&config).await {
                        Ok(status) => {
                            println!("{}", format_account_memory_status(&status));
                            exit = i32::from(!status.working);
                        }
                        Err(error) => {
                            eprintln!(
                                "{}",
                                crate::localization::cli_locale().format(
                                    "Account memory: not working\n{0}",
                                    &[format!("{:#}", error)]
                                )
                            );
                            exit = 1;
                        }
                    },
                    Err(error) => {
                        eprintln!(
                            "{}",
                            crate::localization::cli_locale().format(
                                "Account memory: not working\n{0}",
                                &[format!("{:#}", error)]
                            )
                        );
                        exit = 1;
                    }
                },
                Ok(None) if shared_memory_configured => {
                    println!(
                        "{}",
                        crate::localization::cli_locale()
                            .format("Account memory: not configured", &[])
                    );
                }
                Ok(None) => {
                    eprintln!("{}", crate::localization::cli_locale().format("Account memory: not working\nno stored EvalOps credentials; run `deixic-code init --workspace-id <id>`", &[]));
                    exit = 1;
                }
                Err(error) => {
                    eprintln!(
                        "{}",
                        crate::localization::cli_locale().format(
                            "Account memory: not working\n{0}",
                            &[format!("{:#}", error)]
                        )
                    );
                    exit = 1;
                }
            }
            if shared_memory_configured {
                match config_from_env() {
                    Ok(config) => match status_output(&http_client()?, &config).await {
                        Ok(output) => println!("{output}"),
                        Err(error) => {
                            eprintln!(
                                "{}",
                                crate::localization::cli_locale().format(
                                    "Failed to fetch shared memory status: {0}",
                                    &[format!("{:#}", error)]
                                )
                            );
                            exit = 1;
                        }
                    },
                    Err(error) => {
                        eprintln!(
                            "{}",
                            crate::localization::cli_locale().format(
                                "Failed to read shared memory configuration: {0}",
                                &[format!("{:#}", error)]
                            )
                        );
                        exit = 1;
                    }
                }
            }
            Ok(exit)
        }
        "remember" => {
            let fact = args[1..].join(" ");
            if fact.trim().is_empty() {
                eprintln!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "Fact required. Usage: deixic-code memory remember <fact>",
                        &[]
                    )
                );
                return Ok(1);
            }
            let result = match load_account_memory_config() {
                Ok(config) => remember_account_memory(&config, fact.trim()).await,
                Err(error) => Err(error),
            };
            match result {
                Ok(_) => {
                    println!(
                        "{}",
                        crate::localization::cli_locale().format(
                            "Account memory: working\nRemembered for this workspace.",
                            &[]
                        )
                    );
                    Ok(0)
                }
                Err(error) => {
                    eprintln!(
                        "{}",
                        crate::localization::cli_locale().format(
                            "Account memory: not working\n{0}",
                            &[format!("{:#}", error)]
                        )
                    );
                    Ok(1)
                }
            }
        }
        "recall" => {
            let query = args[1..].join(" ");
            if query.trim().is_empty() {
                eprintln!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "Query required. Usage: deixic-code memory recall <query>",
                        &[]
                    )
                );
                return Ok(1);
            }
            let result = match load_account_memory_config() {
                Ok(config) => recall_account_memory(&config, query.trim()).await,
                Err(error) => Err(error),
            };
            match result.and_then(|response| format_account_recall(&response)) {
                Ok(output) => {
                    println!("{output}");
                    Ok(0)
                }
                Err(error) => {
                    eprintln!(
                        "{}",
                        crate::localization::cli_locale().format(
                            "Account memory: not working\n{0}",
                            &[format!("{:#}", error)]
                        )
                    );
                    Ok(1)
                }
            }
        }
        "capabilities" => {
            let config = config_from_env()?;
            let client = http_client()?;
            let caps: CapabilitiesResponse = fetch_json(&client, &config, "/capabilities").await?;
            println!(
                "{}",
                crate::localization::cli_locale().format("\nShared Memory Capabilities\n", &[])
            );
            println!("{}", capabilities_line(Some(&caps)));
            Ok(0)
        }
        "session" => {
            let Some(session_id) = args.get(1).filter(|value| !value.is_empty()) else {
                eprintln!(
                    "{}",
                    crate::localization::cli_locale().format("Session id required.", &[])
                );
                return Ok(1);
            };
            let config = config_from_env()?;
            let client = http_client()?;
            println!("{}", session_output(&client, &config, session_id).await?);
            Ok(0)
        }
        "audit" => {
            let Some(session_id) = args.get(1).filter(|value| !value.is_empty()) else {
                eprintln!(
                    "{}",
                    crate::localization::cli_locale().format("Session id required.", &[])
                );
                return Ok(1);
            };
            let limit = args
                .get(2)
                .and_then(|value| value.parse::<usize>().ok())
                .filter(|value| *value > 0);
            let config = config_from_env()?;
            let client = http_client()?;
            println!(
                "{}",
                audit_output(&client, &config, session_id, limit).await?
            );
            Ok(0)
        }
        "export" => {
            let Some(session_id) = args.get(1).filter(|value| !value.is_empty()) else {
                eprintln!(
                    "{}",
                    crate::localization::cli_locale().format("Session id required.", &[])
                );
                return Ok(1);
            };
            let config = config_from_env()?;
            let client = http_client()?;
            let text = fetch_text(
                &client,
                &config,
                &format!(
                    "/sessions/{}/metrics.jsonl",
                    urlencoding::encode(session_id)
                ),
            )
            .await?;
            print!(
                "{}",
                export_text_for_stdout(&text, std::io::stdout().is_terminal())
            );
            if !text.ends_with('\n') {
                println!();
            }
            Ok(0)
        }
        "watch" => watch(args, config_from_env()?).await,
        other => {
            eprintln!(
                "{}",
                crate::localization::cli_locale()
                    .format("Unknown memory subcommand: {0}", &[(other).to_string()])
            );
            println!(
                "{}",
                crate::localization::cli_locale().format("\nAvailable commands:", &[])
            );
            println!("{}", memory_help());
            Ok(1)
        }
    }
}

fn config_from_env() -> Result<SharedMemoryConfig> {
    let base_url = std::env::var("MAESTRO_SHARED_MEMORY_BASE")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_owned())
        .filter(|value| !value.is_empty())
        .context(
            "MAESTRO_SHARED_MEMORY_BASE is not set. Configure shared memory to use this command.",
        )?;
    let api_key = std::env::var("MAESTRO_SHARED_MEMORY_API_KEY")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    Ok(SharedMemoryConfig { base_url, api_key })
}

fn http_client() -> Result<Client> {
    Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .context("failed to create shared memory HTTP client")
}

async fn fetch_response(
    client: &Client,
    config: &SharedMemoryConfig,
    path: &str,
) -> Result<Response> {
    let url = format!("{}{}", config.base_url, path);
    for attempt in 0..MAX_ATTEMPTS {
        let mut request = client.get(&url);
        if let Some(api_key) = &config.api_key {
            request = request.bearer_auth(api_key);
        }
        match request.send().await {
            Ok(response) if !retryable_status(response.status()) || attempt + 1 == MAX_ATTEMPTS => {
                return Ok(response);
            }
            Ok(response) => {
                tokio::time::sleep(retry_delay(&response, attempt)).await;
            }
            Err(error) if attempt + 1 == MAX_ATTEMPTS => {
                return Err(error).context("shared memory service request failed");
            }
            Err(_) => tokio::time::sleep(exponential_delay(attempt)).await,
        }
    }
    unreachable!("shared memory retry loop always returns")
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

async fn fetch_text(client: &Client, config: &SharedMemoryConfig, path: &str) -> Result<String> {
    let response = fetch_response(client, config, path).await?;
    let status = response.status();
    let status_text = status.canonical_reason().unwrap_or("Unknown").to_owned();
    let text = response
        .text()
        .await
        .context("failed to read shared memory response")?;
    if !status.is_success() {
        bail!(
            "{}",
            crate::localization::cli_locale().format(
                "Shared memory error {0}: {1}",
                &[
                    (status.as_u16()).to_string(),
                    (if text.is_empty() { &status_text } else { &text }).clone()
                ]
            )
        );
    }
    Ok(text)
}

async fn fetch_json<T: serde::de::DeserializeOwned>(
    client: &Client,
    config: &SharedMemoryConfig,
    path: &str,
) -> Result<T> {
    let text = fetch_text(client, config, path).await?;
    serde_json::from_str(if text.is_empty() { "{}" } else { &text })
        .context("shared memory service returned invalid JSON")
}

/// Sanitize the raw shared-memory HTTP response body (`memory export`'s
/// JSONL payload) before it reaches the real terminal, which has no
/// ratatui `Buffer` in this CLI path to filter it.
///
/// This happens at the print boundary, not in `fetch_text` (which
/// `fetch_json` also uses to feed `serde_json::from_str`): sanitizing
/// there could strip bytes that are meaningful to the JSON parser.
fn sanitize_export_text(text: &str) -> String {
    crate::output_sanitize::sanitize_control_chars(text)
}

/// Decide whether `memory export`'s stdout write should sanitize `text`.
///
/// `export`'s whole point is byte-exact JSONL: a valid JSON string can
/// legitimately contain C1 code points (e.g. `U+0085`), and
/// `sanitize_export_text` would silently corrupt those when the output is
/// redirected to a file or another process -- which also cannot execute a
/// terminal escape sequence in the first place, so there is nothing to
/// protect against there. Only sanitize when stdout is an actual terminal.
/// Takes `stdout_is_terminal` as a parameter (mirroring
/// `hyperlink::format_link_with_fallback`'s `is_tty` parameter) so this
/// stays unit-testable without a real pty.
fn export_text_for_stdout(text: &str, stdout_is_terminal: bool) -> String {
    if stdout_is_terminal {
        sanitize_export_text(text)
    } else {
        text.to_string()
    }
}

async fn status_output(client: &Client, config: &SharedMemoryConfig) -> Result<String> {
    let metrics: ServiceMetricsResponse = fetch_json(client, config, "/metrics").await?;
    let mut lines = vec![
        "\nShared Memory\n".to_owned(),
        format!("Base: {}", config.base_url),
        format!("Status: {}", metrics.status.as_deref().unwrap_or("unknown")),
    ];
    if let Some(now) = metrics.now {
        lines.push(format!("Time: {now}"));
    }
    lines.push(capabilities_line(metrics.capabilities.as_ref()));
    // `metrics.status` and `metrics.now` are server-controlled JSON string
    // fields; this function's sole consumer is `print!`/`println!` to the
    // real terminal (no ratatui `Buffer` in this CLI path), so sanitize the
    // fully assembled output here rather than at ingestion.
    Ok(crate::output_sanitize::sanitize_control_chars(
        &lines.join("\n"),
    ))
}

fn capabilities_line(capabilities: Option<&CapabilitiesResponse>) -> String {
    let Some(cap) = capabilities else {
        return "Capabilities unavailable".to_owned();
    };
    let value = |value: Option<u64>| value.map_or_else(|| "?".to_owned(), |v| v.to_string());
    [
        format!(
            "sync: {}",
            if cap.supports_sync != Some(false) {
                "on"
            } else {
                "off"
            }
        ),
        format!(
            "gzip: {}",
            if cap.supports_gzip != Some(false) {
                "on"
            } else {
                "off"
            }
        ),
        format!("max_body: {}", value(cap.max_body_bytes)),
        format!("max_batch: {}", value(cap.max_events_batch)),
        format!("max_events: {}", value(cap.max_events)),
        format!("event_payload: {}", value(cap.max_event_payload_bytes)),
        format!("event_type: {}", value(cap.max_event_type_length)),
        format!("event_id: {}", value(cap.max_event_id_length)),
        format!("session_id: {}", value(cap.max_session_id_length)),
    ]
    .join("  ·  ")
}

async fn session_output(
    client: &Client,
    config: &SharedMemoryConfig,
    session_id: &str,
) -> Result<String> {
    let response: SessionMetricsResponse = fetch_json(
        client,
        config,
        &format!("/sessions/{}/metrics", urlencoding::encode(session_id)),
    )
    .await?;
    let mut lines = vec![
        "\nShared Memory Session\n".to_owned(),
        format!("Session: {session_id}"),
    ];
    if let Some(meta) = response.meta {
        lines.push(format!(
            "last_seq: {}  ·  min_seq: {}  ·  events: {}",
            display_number(meta.last_seq),
            display_number(meta.min_seq),
            display_number(meta.event_count)
        ));
        if let Some(updated_at) = meta.updated_at {
            lines.push(format!("Updated: {updated_at}"));
        }
    }
    if let Some(metrics) = response.metrics {
        lines.push("\nShared Memory Sync Metrics\n".to_owned());
        for (key, value) in metrics {
            if !value.is_null() {
                lines.push(format!("  {key}: {value}"));
            }
        }
    }
    // `updated_at` and the sync-metrics keys/values are server-controlled;
    // see the comment in `status_output` for why sanitization happens here.
    Ok(crate::output_sanitize::sanitize_control_chars(
        &lines.join("\n"),
    ))
}

async fn audit_output(
    client: &Client,
    config: &SharedMemoryConfig,
    session_id: &str,
    limit: Option<usize>,
) -> Result<String> {
    let response: AuditResponse = fetch_json(
        client,
        config,
        &format!("/sessions/{}/audit", urlencoding::encode(session_id)),
    )
    .await?;
    let items = response.items.unwrap_or_default();
    let start = limit.map_or(0, |limit| items.len().saturating_sub(limit));
    let mut lines = vec!["\nShared Memory Audit\n".to_owned()];
    if items.is_empty() || start == items.len() {
        lines.push("No audit entries found.".to_owned());
        return Ok(lines.join("\n"));
    }
    for entry in &items[start..] {
        lines.push(format!(
            "{}  ·  mode: {}  ·  events: {}  ·  source: {}",
            display_value(entry.get("at"), "?"),
            display_value(entry.get("mode"), "?"),
            display_value(entry.get("event_count"), "0"),
            display_value(entry.get("source"), "?")
        ));
    }
    // Every `display_value` above can surface an arbitrary server-controlled
    // JSON string (`at`/`mode`/`source`); see the comment in `status_output`.
    Ok(crate::output_sanitize::sanitize_control_chars(
        &lines.join("\n"),
    ))
}

async fn watch(args: &[String], config: SharedMemoryConfig) -> Result<i32> {
    let session_id = args.get(1).filter(|value| !value.is_empty());
    let interval_ms = args
        .get(2)
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(2_000);
    println!(
        "{}",
        crate::localization::cli_locale().format(
            "Watching {0} every {1}ms. Ctrl+C to stop.",
            &[
                session_id
                    .map_or_else(|| "service".to_owned(), |id| format!("session {id}"))
                    .clone(),
                (interval_ms).to_string()
            ]
        )
    );
    let client = http_client()?;
    loop {
        let result = match session_id {
            Some(id) => session_output(&client, &config, id).await,
            None => status_output(&client, &config).await,
        };
        match result {
            Ok(output) => println!("{output}"),
            Err(error) => eprintln!(
                "{}",
                crate::localization::cli_locale()
                    .format("Shared memory watch error: {0}", &[format!("{:#}", error)])
            ),
        }
        tokio::time::sleep(Duration::from_millis(interval_ms)).await;
    }
}

fn display_number(value: Option<u64>) -> String {
    value.map_or_else(|| "?".to_owned(), |value| value.to_string())
}

fn display_value(value: Option<&Value>, fallback: &str) -> String {
    match value {
        Some(Value::String(value)) => value.clone(),
        Some(value) if !value.is_null() => value.to_string(),
        _ => fallback.to_owned(),
    }
}

fn memory_help() -> &'static str {
    crate::localization::cli_locale().translate("  deixic-code memory [status]\n  deixic-code memory remember <fact>\n  deixic-code memory recall <query>\n  deixic-code memory capabilities\n  deixic-code memory session <id>\n  deixic-code memory audit <id> [limit]\n  deixic-code memory export <id>\n  deixic-code memory watch [id] [intervalMs]\n\nAccount memory uses the exact stored organization/workspace registration. Remember is always explicit; recall returns at most three memories.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_cli::{EvalOpsAgentMcpSnapshot, EvalOpsCredentialSnapshot};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn evalops_snapshot(
        organization_id: Option<&str>,
        workspace_id: Option<&str>,
    ) -> EvalOpsCredentialSnapshot {
        EvalOpsCredentialSnapshot {
            access: "oauth-access".to_owned(),
            refresh: "oauth-refresh".to_owned(),
            expires: i64::MAX,
            email: Some("operator@example.com".to_owned()),
            organization_id: organization_id.map(str::to_owned),
            workspace_id: None,
            user_id: Some("user-1".to_owned()),
            identity_base_url: Some("https://identity.evalops.dev".to_owned()),
            provider_ref: None,
            agent_mcp: Some(EvalOpsAgentMcpSnapshot {
                agent_id: Some("agent-1".to_owned()),
                api_key: Some("agent-api-key".to_owned()),
                endpoint: Some("https://app.evalops.dev/mcp".to_owned()),
                integration_profile: Some("managed_runtime".to_owned()),
                key_prefix: Some("agent".to_owned()),
                memory_mode: Some("durable".to_owned()),
                run_id: Some("run-1".to_owned()),
                runtime_owner: Some("evalops".to_owned()),
                session_expires_at: Some("2026-08-22T00:00:00Z".to_owned()),
                shim_type: Some("sdk".to_owned()),
                trace_mode: Some("otlp".to_owned()),
                workspace_id: workspace_id.map(str::to_owned),
            }),
        }
    }

    #[test]
    fn account_memory_requires_explicit_organization_and_workspace_scope() {
        let missing_workspace = evalops_snapshot(Some("org-1"), None);
        let error = AccountMemoryConfig::from_snapshot(&missing_workspace).unwrap_err();
        assert!(error.to_string().contains("workspace"));

        let missing_organization = evalops_snapshot(None, Some("workspace-1"));
        let error = AccountMemoryConfig::from_snapshot(&missing_organization).unwrap_err();
        assert!(error.to_string().contains("organization"));

        let config = AccountMemoryConfig::from_snapshot(&evalops_snapshot(
            Some("org-1"),
            Some("workspace-1"),
        ))
        .unwrap();
        assert_eq!(config.organization_id, "org-1");
        assert_eq!(config.workspace_id, "workspace-1");
    }

    #[test]
    fn account_memory_registration_carries_exact_scope_and_memory_permissions() {
        let config = AccountMemoryConfig::from_snapshot(&evalops_snapshot(
            Some("org-1"),
            Some("workspace-1"),
        ))
        .unwrap();
        let arguments = account_registration_arguments(&config);
        assert_eq!(arguments["agent_type"], "maestro");
        assert_eq!(arguments["surface"], "cli");
        assert_eq!(arguments["organization_id"], "org-1");
        assert_eq!(arguments["workspace_id"], "workspace-1");
        assert_eq!(arguments["memory_mode"], "durable");
        assert_eq!(
            arguments["scopes"],
            serde_json::json!(["memories:read", "memories:write"])
        );
    }

    #[test]
    fn explicit_memory_write_is_governed_without_sending_the_fact_to_policy() {
        let config = AccountMemoryConfig::from_snapshot(&evalops_snapshot(
            Some("org-1"),
            Some("workspace-1"),
        ))
        .unwrap();
        let fact = "The launch codename is Blue Heron";
        let governance = memory_write_governance_arguments(&config);
        assert_eq!(governance["action_type"], "evalops_store_memory");
        assert_eq!(governance["declared_risk_level"], "MEDIUM");
        assert!(!governance.to_string().contains(fact));

        let store = account_store_arguments(&config, fact);
        assert_eq!(store["content"], fact);
        assert_eq!(store["scope"], "team");
        assert_eq!(store["team_id"], "workspace-1");
        assert!(store.get("organization_id").is_none());
    }

    #[test]
    fn account_recall_is_workspace_scoped_and_capped_at_three() {
        let config = AccountMemoryConfig::from_snapshot(&evalops_snapshot(
            Some("org-1"),
            Some("workspace-1"),
        ))
        .unwrap();
        let recall = account_recall_arguments(&config, "launch codename");
        assert_eq!(recall["query"], "launch codename");
        assert_eq!(recall["scope"], "team");
        assert_eq!(recall["team_id"], "workspace-1");
        assert_eq!(recall["top_k"], 3);
        assert!(recall.get("organization_id").is_none());
    }

    #[test]
    fn account_memory_status_is_working_only_for_live_matching_governed_scope() {
        let config = AccountMemoryConfig::from_snapshot(&evalops_snapshot(
            Some("org-1"),
            Some("workspace-1"),
        ))
        .unwrap();
        let working = serde_json::json!({
            "session": {
                "registered": true,
                "organization_id": "org-1",
                "workspace_id": "workspace-1",
                "memory_mode": "durable",
                "scopes_granted": ["memories:read", "memories:write"],
                "control_claims": {
                    "authenticated": true,
                    "governed": true,
                    "memory_writable": true,
                    "registered": true
                }
            }
        });
        assert!(account_memory_status(&config, &working).working);

        let wrong_workspace = serde_json::json!({
            "session": {
                "registered": true,
                "organization_id": "org-1",
                "workspace_id": "workspace-2",
                "memory_mode": "durable",
                "scopes_granted": ["memories:read", "memories:write"],
                "control_claims": {
                    "authenticated": true,
                    "governed": true,
                    "memory_writable": true,
                    "registered": true
                }
            }
        });
        let status = account_memory_status(&config, &wrong_workspace);
        assert!(!status.working);
        assert!(status.reason.contains("workspace"));

        let missing_write_scope = serde_json::json!({
            "session": {
                "registered": true,
                "organization_id": "org-1",
                "workspace_id": "workspace-1",
                "memory_mode": "durable",
                "scopes_granted": ["memories:read"],
                "control_claims": {
                    "authenticated": true,
                    "governed": true,
                    "memory_writable": false,
                    "registered": true
                }
            }
        });
        let status = account_memory_status(&config, &missing_write_scope);
        assert!(!status.working);
        assert!(status.reason.contains("write"));
    }

    #[test]
    fn memory_write_gate_accepts_allow_or_approved_and_rejects_everything_else() {
        let allowed = serde_json::json!({"decision": "allow", "risk_level": "medium"});
        assert!(validate_memory_write_gate(&allowed, None).is_ok());

        let denied = serde_json::json!({
            "decision": "deny",
            "risk_level": "medium",
            "message": "policy denied the write"
        });
        assert!(validate_memory_write_gate(&denied, None).is_err());

        let approval_required = serde_json::json!({
            "decision": "require_approval",
            "risk_level": "medium",
            "approval_id": "approval-1"
        });
        let pending = serde_json::json!({
            "approval_id": "approval-1",
            "state": "pending"
        });
        assert!(validate_memory_write_gate(&approval_required, Some(&pending)).is_err());
        let approved = serde_json::json!({
            "approval_id": "approval-1",
            "state": "approved"
        });
        assert!(validate_memory_write_gate(&approval_required, Some(&approved)).is_ok());
    }

    #[tokio::test]
    async fn account_memory_round_trip_registers_verifies_governs_stores_and_recalls() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let seen_tools = Arc::new(Mutex::new(Vec::new()));
        let server_tools = Arc::clone(&seen_tools);
        let server = tokio::spawn(async move {
            for _ in 0..13 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buffer = vec![0_u8; 65_536];
                let read = stream.read(&mut buffer).await.unwrap();
                let request = String::from_utf8_lossy(&buffer[..read]);
                if request.starts_with("DELETE ") {
                    stream
                        .write_all(
                            b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await
                        .unwrap();
                    continue;
                }
                let body = request.split("\r\n\r\n").nth(1).unwrap_or("{}");
                let payload: Value = serde_json::from_str(body).unwrap();
                let method = payload["method"].as_str().unwrap();
                let response = if method == "notifications/initialized" {
                    "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_owned()
                } else if method == "initialize" {
                    mcp_http_response(payload["id"].clone(), serde_json::json!({}))
                } else {
                    assert_eq!(method, "tools/call");
                    let tool = payload["params"]["name"].as_str().unwrap().to_owned();
                    server_tools.lock().unwrap().push(tool.clone());
                    let structured = match tool.as_str() {
                        "evalops_register" => serde_json::json!({
                            "registered": true,
                            "agent_id": "agent-1"
                        }),
                        "evalops_control_plane_summary" => serde_json::json!({
                            "session": {
                                "registered": true,
                                "organization_id": "org-1",
                                "workspace_id": "workspace-1",
                                "memory_mode": "durable",
                                "scopes_granted": ["memories:read", "memories:write"],
                                "control_claims": {
                                    "authenticated": true,
                                    "governed": true,
                                    "memory_writable": true,
                                    "registered": true
                                }
                            }
                        }),
                        "evalops_check_action" => serde_json::json!({
                            "decision": "allow",
                            "risk_level": "medium"
                        }),
                        "evalops_store_memory" => serde_json::json!({
                            "available": true,
                            "stored": true,
                            "memory": {"id": "memory-1", "content": "fact"}
                        }),
                        "evalops_recall" => serde_json::json!({
                            "available": true,
                            "count": 1,
                            "results": [{"id": "memory-1", "content": "fact"}]
                        }),
                        other => panic!("unexpected tool: {other}"),
                    };
                    mcp_http_response(
                        payload["id"].clone(),
                        serde_json::json!({"structuredContent": structured}),
                    )
                };
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });

        let mut config = AccountMemoryConfig::from_snapshot(&evalops_snapshot(
            Some("org-1"),
            Some("workspace-1"),
        ))
        .unwrap();
        config.endpoint = format!("http://{address}");
        let result = remember_account_memory(&config, "fact").await.unwrap();
        assert_eq!(result["stored"], true);
        let recalled = recall_account_memory(&config, "fact").await.unwrap();
        assert_eq!(recalled["results"][0]["content"], "fact");
        server.await.unwrap();
        assert_eq!(
            *seen_tools.lock().unwrap(),
            [
                "evalops_register",
                "evalops_control_plane_summary",
                "evalops_check_action",
                "evalops_store_memory",
                "evalops_register",
                "evalops_control_plane_summary",
                "evalops_recall"
            ]
        );
    }

    #[test]
    fn recall_output_never_prints_more_than_three_memories() {
        let response = serde_json::json!({
            "available": true,
            "count": 4,
            "results": [
                {"id": "1", "content": "first"},
                {"id": "2", "content": "second"},
                {"id": "3", "content": "third"},
                {"id": "4", "content": "must not print"}
            ]
        });
        let output = format_account_recall(&response).unwrap();
        assert!(output.contains("first"));
        assert!(output.contains("second"));
        assert!(output.contains("third"));
        assert!(!output.contains("must not print"));
    }

    #[test]
    fn account_status_uses_unambiguous_working_copy() {
        let working = AccountMemoryStatus {
            working: true,
            reason: "live exact-scope recall and governed writes are available".to_owned(),
        };
        assert!(format_account_memory_status(&working).starts_with("Account memory: working"));
        let unavailable = AccountMemoryStatus {
            working: false,
            reason: "live workspace does not match".to_owned(),
        };
        let output = format_account_memory_status(&unavailable);
        assert!(output.starts_with("Account memory: not working"));
        assert!(output.contains("live workspace does not match"));
    }

    #[test]
    fn memory_help_exposes_explicit_account_memory_commands() {
        let help = memory_help();
        assert!(help.contains("deixic-code memory remember <fact>"));
        assert!(help.contains("deixic-code memory recall <query>"));
        assert!(help.contains("Account memory uses the exact stored organization/workspace"));
    }

    #[test]
    fn local_tui_status_never_confuses_configuration_with_live_success() {
        let configured = format_local_account_memory_status(Some(&evalops_snapshot(
            Some("org-1"),
            Some("workspace-1"),
        )));
        assert!(configured.contains("configured"));
        assert!(!configured.contains("Account memory: working"));
        assert!(configured.contains("deixic-code memory status"));

        let unavailable = format_local_account_memory_status(None);
        assert!(unavailable.contains("Account memory: not working"));
    }

    fn mcp_http_response(id: Value, result: Value) -> String {
        let body = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: session-1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn capabilities_preserve_legacy_defaults_and_limits() {
        let line = capabilities_line(Some(&CapabilitiesResponse {
            supports_sync: None,
            supports_gzip: Some(false),
            max_body_bytes: Some(1024),
            max_events_batch: Some(10),
            ..CapabilitiesResponse::default()
        }));
        assert!(line.contains("sync: on"));
        assert!(line.contains("gzip: off"));
        assert!(line.contains("max_body: 1024"));
        assert!(line.contains("max_events: ?"));
    }

    #[test]
    fn retryable_status_matches_downstream_http_contract() {
        assert!(retryable_status(StatusCode::REQUEST_TIMEOUT));
        assert!(retryable_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(retryable_status(StatusCode::SERVICE_UNAVAILABLE));
        assert!(!retryable_status(StatusCode::UNAUTHORIZED));
    }

    #[tokio::test]
    async fn status_retries_transient_failure_and_sends_bearer_token() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buffer = vec![0_u8; 4096];
                let read = stream.read(&mut buffer).await.unwrap();
                let request = String::from_utf8_lossy(&buffer[..read]);
                assert!(request.starts_with("GET /metrics HTTP/1.1"));
                assert!(
                    request
                        .to_ascii_lowercase()
                        .contains("authorization: bearer memory-key")
                );
                let response = if attempt == 0 {
                    "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 11\r\nRetry-After-Ms: 1\r\nConnection: close\r\n\r\nunavailable"
                        .to_owned()
                } else {
                    let body = r#"{"status":"ok","now":"2026-04-20T00:00:00.000Z","capabilities":{"supports_sync":true,"supports_gzip":true,"max_body_bytes":1024,"max_events_batch":10}}"#;
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let config = SharedMemoryConfig {
            base_url: format!("http://{address}"),
            api_key: Some("memory-key".to_owned()),
        };
        let output = status_output(&http_client().unwrap(), &config)
            .await
            .unwrap();
        server.await.unwrap();
        assert!(output.contains("Shared Memory"));
        assert!(output.contains("Status: ok"));
        assert!(output.contains("max_body: 1024"));
    }

    #[tokio::test]
    async fn missing_session_id_fails_before_reading_config() {
        let result = run_memory(&["session".to_owned()]).await.unwrap();
        assert_eq!(result, 1);
    }

    #[tokio::test]
    async fn status_output_sanitizes_server_controlled_strings() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0_u8; 4096];
            let _ = stream.read(&mut buffer).await.unwrap();
            // `\u001b` / `\u0007` are valid JSON string escapes that decode
            // to a literal ESC/BEL byte in the parsed Rust `String` -- this
            // is how a malicious/compromised shared-memory service could
            // smuggle a minimal OSC-0 (set title) sequence through
            // `metrics.status`.
            let body = r#"{"status":"ok\u001b]0;evil\u0007","now":"safe","capabilities":null}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let config = SharedMemoryConfig {
            base_url: format!("http://{address}"),
            api_key: None,
        };
        let output = status_output(&http_client().unwrap(), &config)
            .await
            .unwrap();
        server.await.unwrap();
        assert!(!output.contains('\x1b'));
        assert!(!output.contains('\x07'));
        assert!(output.contains("Status: ok]0;evil"));
    }

    #[tokio::test]
    async fn session_output_sanitizes_server_controlled_strings() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0_u8; 4096];
            let _ = stream.read(&mut buffer).await.unwrap();
            let body = r#"{"meta":{"last_seq":1,"min_seq":0,"event_count":2,"updated_at":"bad\u001b]0;evil\u0007time"},"metrics":{"queue_depth":"12"}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let config = SharedMemoryConfig {
            base_url: format!("http://{address}"),
            api_key: None,
        };
        let output = session_output(&http_client().unwrap(), &config, "sess-1")
            .await
            .unwrap();
        server.await.unwrap();
        assert!(!output.contains('\x1b'));
        assert!(!output.contains('\x07'));
        assert!(output.contains("Updated: bad]0;eviltime"));
    }

    #[tokio::test]
    async fn audit_output_sanitizes_server_controlled_strings() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0_u8; 4096];
            let _ = stream.read(&mut buffer).await.unwrap();
            let body = r#"{"items":[{"at":"t0","mode":"m","event_count":1,"source":"peer\u001b]0;evil\u0007"}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let config = SharedMemoryConfig {
            base_url: format!("http://{address}"),
            api_key: None,
        };
        let output = audit_output(&http_client().unwrap(), &config, "sess-1", None)
            .await
            .unwrap();
        server.await.unwrap();
        assert!(!output.contains('\x1b'));
        assert!(!output.contains('\x07'));
        assert!(output.contains("source: peer]0;evil"));
    }

    #[test]
    fn sanitize_export_text_strips_osc_injection_preserves_visible_text() {
        let input = "line one\nbefore\x1b]0;evil\x07after";
        let out = sanitize_export_text(input);
        assert_eq!(out, "line one\nbefore]0;evilafter");
        assert!(!out.contains('\x1b'));
        assert!(!out.contains('\x07'));
    }

    #[test]
    fn sanitize_export_text_preserves_ordinary_jsonl_text() {
        let input = "{\"seq\":1}\n{\"seq\":2}\n";
        assert_eq!(sanitize_export_text(input), input);
    }

    #[test]
    fn export_text_for_stdout_sanitizes_only_when_a_terminal() {
        let input = "before\x1b]0;evil\x07after";
        assert_eq!(export_text_for_stdout(input, true), "before]0;evilafter");
        // Redirected output must be byte-exact, even for control bytes that
        // would be dangerous on a real terminal.
        assert_eq!(export_text_for_stdout(input, false), input);
    }

    #[test]
    fn export_text_for_stdout_preserves_c1_jsonl_when_redirected() {
        // A legitimate JSON string value containing U+0085 (NEL, a C1 code
        // point `sanitize_control_chars` would otherwise strip) must survive
        // byte-for-byte when stdout is not a terminal.
        let input = "{\"note\":\"line one\u{0085}line two\"}\n";
        assert_eq!(export_text_for_stdout(input, false), input);
        assert_ne!(export_text_for_stdout(input, true), input);
    }
}
