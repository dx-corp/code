//! Native `maestro remote` control-plane CLI.
//!
//! Ported from `src/cli/commands/remote.ts` and `src/remote-runner/client.ts`.
//! Interactive TTY attach lives in `remote_attach.rs` (from `attach-client.ts`).
//!
//! Residual:
//! - Richer multi-step headless verify loops

use std::collections::BTreeMap;
use std::io::{self, IsTerminal};
use std::time::{Duration, Instant};

use crate::headless::HEADLESS_PROTOCOL_VERSION;
use crate::remote_attach::{
    AttachRole, RemoteAttachInput, attach_to_remote_runner_session,
    should_use_interactive_remote_attach,
};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use uuid::Uuid;

pub(crate) const DEFAULT_BASE_URL: &str = "https://runner.evalops.dev";
const DEFAULT_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_MAX_ATTEMPTS: usize = 2;
const VERIFY_ERROR_BODY_MAX_CHARS: usize = 512;
const DEFAULT_WAIT_TIMEOUT_MS: u64 = 5 * 60 * 1000;
const DEFAULT_POLL_MS: u64 = 5_000;
const CONNECT_VERSION: &str = "1";
pub(crate) const SERVICE: &str = "remote runner service";
const SERVICE_PATH: &str = "/remoterunner.v1.RemoteRunnerService";

pub(crate) const CREATE_PATH: &str = "/remoterunner.v1.RemoteRunnerService/CreateRunnerSession";
pub(crate) const GET_PATH: &str = "/remoterunner.v1.RemoteRunnerService/GetRunnerSession";
pub(crate) const LIST_PATH: &str = "/remoterunner.v1.RemoteRunnerService/ListRunnerSessions";
pub(crate) const STOP_PATH: &str = "/remoterunner.v1.RemoteRunnerService/StopRunnerSession";
const EXTEND_PATH: &str = "/remoterunner.v1.RemoteRunnerService/ExtendRunnerSession";
pub(crate) const MINT_PATH: &str = "/remoterunner.v1.RemoteRunnerService/MintAttachToken";
const REVOKE_PATH: &str = "/remoterunner.v1.RemoteRunnerService/RevokeAttachToken";
pub(crate) const EVENTS_PATH: &str = "/remoterunner.v1.RemoteRunnerService/ListRunnerSessionEvents";
const STATUS_PATH: &str = "/remoterunner.v1.RemoteRunnerService/GetStatus";

const BASE_URL_ENV: &[&str] = &[
    "MAESTRO_REMOTE_RUNNER_URL",
    "REMOTE_RUNNER_SERVICE_URL",
    "EVALOPS_REMOTE_RUNNER_URL",
    "MAESTRO_PLATFORM_BASE_URL",
    "MAESTRO_EVALOPS_BASE_URL",
    "EVALOPS_BASE_URL",
];
const TOKEN_ENV: &[&str] = &[
    "MAESTRO_REMOTE_RUNNER_TOKEN",
    "REMOTE_RUNNER_SERVICE_TOKEN",
    "MAESTRO_EVALOPS_ACCESS_TOKEN",
    "EVALOPS_TOKEN",
];
const ORG_ENV: &[&str] = &[
    "MAESTRO_REMOTE_RUNNER_ORG_ID",
    "REMOTE_RUNNER_ORGANIZATION_ID",
    "MAESTRO_EVALOPS_ORG_ID",
    "EVALOPS_ORGANIZATION_ID",
    "EVALOPS_ORG_ID",
    "MAESTRO_ENTERPRISE_ORG_ID",
];
const WORKSPACE_ENV: &[&str] = &[
    "MAESTRO_REMOTE_RUNNER_WORKSPACE_ID",
    "REMOTE_RUNNER_WORKSPACE_ID",
    "MAESTRO_EVALOPS_WORKSPACE_ID",
    "EVALOPS_WORKSPACE_ID",
    "MAESTRO_WORKSPACE_ID",
];

const USAGE: &str = "\
maestro remote <command> [options]

Commands:
  start --workspace <id> [--ttl 90m] [--profile standard] [--wait] [--wait-timeout 5m] [--poll-interval 5s] [--verify]
  list --workspace <id> [--state running] [--limit 20]
  status --workspace <id>
  get <session-id>
  events <session-id> [--after <sequence>] [--limit 50]
  extend <session-id> --ttl 2h [--idle-ttl 30m]
  stop <session-id> [--reason <text>]
  attach <session-id> [--role controller] [--ttl 30m] [--verify] [--print-env]
  attach-token <session-id> [--role viewer] [--ttl 30m] [--json]
  revoke-token <session-id> <token-id>
  target <session-id>

Shared options:
  --base-url <url>       Remote runner URL (defaults to https://runner.evalops.dev)
  --org <id>            EvalOps organization id
  --workspace <id>      EvalOps workspace id
  --token <token>       EvalOps access token
  --json                Print machine-readable JSON
  --help                Show this help

Notes:
  On a TTY (without --json/--print-env), attach mints a token and opens an
  interactive REPL. Otherwise it prints transport env handoff instructions.";

pub(crate) const STATE_RUNNING: &str = "RUNNER_SESSION_STATE_RUNNING";
pub(crate) const STATE_IDLE: &str = "RUNNER_SESSION_STATE_IDLE";
pub(crate) const STATE_STOPPED: &str = "RUNNER_SESSION_STATE_STOPPED";
pub(crate) const STATE_EXPIRED: &str = "RUNNER_SESSION_STATE_EXPIRED";
pub(crate) const STATE_FAILED: &str = "RUNNER_SESSION_STATE_FAILED";
pub(crate) const STATE_LOST: &str = "RUNNER_SESSION_STATE_LOST";
pub(crate) const ROLE_VIEWER: &str = "RUNNER_ATTACH_ROLE_VIEWER";
pub(crate) const ROLE_CONTROLLER: &str = "RUNNER_ATTACH_ROLE_CONTROLLER";
const ROLE_ADMIN: &str = "RUNNER_ATTACH_ROLE_ADMIN";

#[derive(Debug, Clone)]
struct Opts {
    flags: BTreeMap<String, Vec<FlagVal>>,
    positionals: Vec<String>,
}

#[derive(Debug, Clone)]
enum FlagVal {
    Bool,
    Str(String),
}

#[derive(Debug, Clone)]
pub(crate) struct ClientOpts {
    pub(crate) base_url: Option<String>,
    pub(crate) token: Option<String>,
    pub(crate) organization_id: Option<String>,
    pub(crate) workspace_id: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub(crate) base_url: String,
    pub(crate) token: String,
    pub(crate) organization_id: String,
    pub(crate) workspace_id: Option<String>,
    pub(crate) timeout_ms: u64,
    pub(crate) max_attempts: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Session {
    pub(crate) id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) runner_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) maestro_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) repo_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) expires_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) idle_expires_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) stop_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Event {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) sequence: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) event_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) occurred_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AttachToken {
    pub(crate) id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) expires_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Minted {
    pub(crate) token: AttachToken,
    pub(crate) token_secret: String,
    pub(crate) gateway_base_url: String,
}

pub async fn run_remote(args: &[String]) -> Result<i32> {
    maestro_local_host::safety::require_vendor_network()?;
    let (cmd, rest) = match args.first().map(String::as_str) {
        None | Some("help" | "--help" | "-h") => {
            println!("{USAGE}");
            return Ok(0);
        }
        Some(c) => (c.to_owned(), &args[1..]),
    };
    let opts = parse_opts(rest);
    if has_flag(&opts, "help") {
        println!("{USAGE}");
        return Ok(0);
    }
    match dispatch(&cmd, &opts).await {
        Ok(()) => Ok(0),
        Err(e) => {
            eprintln!("{e:#}");
            Ok(1)
        }
    }
}

async fn dispatch(cmd: &str, o: &Opts) -> Result<()> {
    match cmd {
        "start" => cmd_start(o).await,
        "list" => cmd_list(o).await,
        "status" => cmd_status(o).await,
        "get" => cmd_get(o).await,
        "events" => cmd_events(o).await,
        "stop" => cmd_stop(o).await,
        "extend" => cmd_extend(o).await,
        "attach" => cmd_attach(o).await,
        "attach-token" => cmd_attach_token(o).await,
        "revoke-token" => cmd_revoke_token(o).await,
        "target" => cmd_target(o).await,
        other => bail!(
            "{}",
            crate::localization::cli_locale()
                .format("Unknown remote command: {0}", &[(other).to_string()])
        ),
    }
}

async fn cmd_start(o: &Opts) -> Result<()> {
    let co = client_opts(o);
    let config = require_config(&co)?;
    let body = start_request(o, &config)?;
    let payload = post(&config, CREATE_PATH, body).await?;
    let session = require_session(&payload)?;
    let created = json!({
        "session": session,
        "events": array_events(&payload),
        "replayed": payload.get("replayed").and_then(Value::as_bool) == Some(true),
    });

    let mut wait_json: Option<Value> = None;
    let mut final_session = session.clone();
    if has_flag(o, "wait") || has_flag(o, "verify") {
        let timeout = parse_wait_ms(flag(o, &["wait-timeout"]), DEFAULT_WAIT_TIMEOUT_MS)?;
        let poll = parse_wait_ms(flag(o, &["poll-interval"]), DEFAULT_POLL_MS)?;
        let wait = wait_ready(&session.id, &co, timeout, poll).await?;
        final_session = wait.0.clone();
        wait_json = Some(json!({"session": wait.0, "attempts": wait.1, "elapsedMs": wait.2}));
    }

    let mut attach_json: Option<Value> = None;
    if has_flag(o, "verify") {
        let minted = mint(
            &session.id,
            role_values(o)?,
            parse_minutes(flag(o, &["attach-ttl"]).as_deref(), 30)?,
            None,
            &co,
        )
        .await?;
        let verified = verify_attach(&minted, &session.id, has_flag(o, "take-control")).await?;
        attach_json = Some(json!({
            "gatewayBaseUrl": minted.gateway_base_url,
            "token": minted.token,
            "tokenSecret": minted.token_secret,
            "verified": verified,
        }));
        if !json_flag(o) {
            print_session(&final_session);
            if let Some(w) = &wait_json {
                println!(
                    "  ready:     {} ({} checks)",
                    format_elapsed(w["elapsedMs"].as_u64().unwrap_or(0)),
                    w["attempts"].as_u64().unwrap_or(0)
                );
            }
            if created["replayed"].as_bool() == Some(true) {
                println!(
                    "{}",
                    crate::localization::cli_locale()
                        .format("  replayed:  existing idempotent request", &[])
                );
            }
            println!();
            print_attach_instr(
                &session.id,
                &minted,
                false,
                has_flag(o, "show-secret"),
                Some(&verified),
            )?;
            return Ok(());
        }
    }

    if json_flag(o) {
        print_json(&json!({"created": created, "wait": wait_json, "attach": attach_json}))?;
        return Ok(());
    }
    print_session(&final_session);
    if let Some(w) = &wait_json {
        println!(
            "  ready:     {} ({} checks)",
            format_elapsed(w["elapsedMs"].as_u64().unwrap_or(0)),
            w["attempts"].as_u64().unwrap_or(0)
        );
    }
    if created["replayed"].as_bool() == Some(true) {
        println!(
            "{}",
            crate::localization::cli_locale()
                .format("  replayed:  existing idempotent request", &[])
        );
    }
    println!();
    println!(
        "{}",
        crate::localization::cli_locale().format(
            "Attach: deixic-code remote attach {0}",
            std::slice::from_ref(&(session.id))
        )
    );
    Ok(())
}

fn start_request(o: &Opts, config: &Config) -> Result<Value> {
    for name in ["workspace-source", "repo", "repo-url", "branch"] {
        if has_flag(o, name) {
            bail!("{}", crate::localization::cli_locale().format("--{0} cannot be used: mutable hydration locators require a signed immutable hydration envelope", &[(name).to_string()]));
        }
    }
    let ttl = parse_minutes(flag(o, &["ttl"]).as_deref(), 90)?;
    let idle = parse_minutes_opt(flag(o, &["idle-ttl", "idle"]))?;
    let workspace_id = trim(flag(o, &["workspace"]).or_else(|| config.workspace_id.clone()))
        .ok_or_else(|| {
            anyhow!("{}", crate::localization::cli_locale().format("Remote runner start requires a workspace id. Pass --workspace or set MAESTRO_REMOTE_RUNNER_WORKSPACE_ID.", &[]))
        })?;
    Ok(strip_null(json!({
        "organizationId": config.organization_id,
        "workspaceId": workspace_id,
        "userId": flag(o, &["user"]),
        "agentRunId": flag(o, &["agent-run"]),
        "maestroSessionId": flag(o, &["maestro-session", "session"]),
        "idempotencyKey": flag(o, &["idempotency-key"]).unwrap_or_else(|| Uuid::new_v4().to_string()),
        "runnerProfile": flag(o, &["profile"]).unwrap_or_else(|| "standard".into()),
        "runnerImage": flag(o, &["image"]),
        "model": flag(o, &["model"]),
        "ttlMinutes": ensure_pos(ttl, "ttlMinutes", 1440)?,
        "idleTtlMinutes": ensure_nonneg(idle, "idleTtlMinutes", 1440)?,
        "metadata": parse_metadata(&repeated(o, &["metadata"]))?,
    })))
}

async fn cmd_list(o: &Opts) -> Result<()> {
    let co = client_opts(o);
    let config = require_config(&co)?;
    let ws = workspace_required(flag(o, &["workspace"]), &co, "list")?;
    let state = flag(o, &["state"])
        .as_deref()
        .map(normalize_state)
        .transpose()?;
    let body = strip_null(json!({
        "organizationId": config.organization_id,
        "workspaceId": ws,
        "state": state,
        "limit": int_flag(o, "limit", Some(20))?,
        "offset": int_flag(o, "offset", Some(0))?,
    }));
    let payload = post(&config, LIST_PATH, body).await?;
    let sessions = array_sessions(&payload);
    if json_flag(o) {
        print_json(&json!({
            "sessions": sessions,
            "nextOffset": first_num(&payload, &["nextOffset", "next_offset"]),
        }))?;
        return Ok(());
    }
    print_table(&sessions);
    if let Some(n) = first_num(&payload, &["nextOffset", "next_offset"]) {
        if n > 0.0 {
            println!(
                "{}",
                crate::localization::cli_locale().format("next offset: {0}", &[(n).to_string()])
            );
        }
    }
    Ok(())
}

async fn cmd_status(o: &Opts) -> Result<()> {
    let co = client_opts(o);
    let config = require_config(&co)?;
    let ws = workspace_required(flag(o, &["workspace"]), &co, "status")?;
    let payload = post(&config, STATUS_PATH, json!({"workspaceId": ws})).await?;
    if json_flag(o) {
        print_json(&json!({
            "service": first_str(&payload, &["service"]),
            "workspaceId": first_str(&payload, &["workspaceId", "workspace_id"]),
            "downstreamPolicy": first_str(&payload, &["downstreamPolicy", "downstream_policy"]),
        }))?;
        return Ok(());
    }
    println!(
        "{}",
        first_str(&payload, &["service"]).unwrap_or_else(|| "remote-runner".into())
    );
    println!(
        "  workspace: {}",
        first_str(&payload, &["workspaceId", "workspace_id"]).unwrap_or_else(|| "-".into())
    );
    println!(
        "  policy:    {}",
        first_str(&payload, &["downstreamPolicy", "downstream_policy"])
            .unwrap_or_else(|| "-".into())
    );
    Ok(())
}

async fn cmd_get(o: &Opts) -> Result<()> {
    let id = o.positionals.first().cloned().ok_or_else(|| {
        anyhow!(
            "{}",
            crate::localization::cli_locale()
                .format("Usage: deixic-code remote get <session-id>", &[])
        )
    })?;
    let session = get_session(&id, &client_opts(o)).await?;
    if json_flag(o) {
        print_json(&session)?;
    } else {
        print_session(&session);
    }
    Ok(())
}

async fn cmd_events(o: &Opts) -> Result<()> {
    let id = o.positionals.first().cloned().ok_or_else(|| {
        anyhow!(
            "{}",
            crate::localization::cli_locale()
                .format("Usage: deixic-code remote events <session-id>", &[])
        )
    })?;
    let config = require_config(&client_opts(o))?;
    let body = strip_null(json!({
        "sessionId": id,
        "afterSequence": int_flag(o, "after", None)?,
        "limit": int_flag(o, "limit", Some(50))?,
    }));
    let payload = post(&config, EVENTS_PATH, body).await?;
    let events = array_events(&payload);
    if json_flag(o) {
        print_json(&json!({
            "events": events,
            "nextSequence": first_num(&payload, &["nextSequence", "next_sequence"]),
        }))?;
        return Ok(());
    }
    if events.is_empty() {
        println!(
            "{}",
            crate::localization::cli_locale().format("No remote runner events found.", &[])
        );
        return Ok(());
    }
    for e in events {
        let seq = e
            .sequence
            .map(|s| format!("{s:.0}"))
            .unwrap_or_else(|| "-".into());
        println!(
            "{:>4}  {}  {}",
            seq,
            e.occurred_at.as_deref().unwrap_or("-"),
            e.event_type.as_deref().unwrap_or("-")
        );
    }
    Ok(())
}

async fn cmd_stop(o: &Opts) -> Result<()> {
    let id = o.positionals.first().cloned().ok_or_else(|| {
        anyhow!(
            "{}",
            crate::localization::cli_locale().format(
                "Usage: deixic-code remote stop <session-id> [--reason text]",
                &[]
            )
        )
    })?;
    let config = require_config(&client_opts(o))?;
    let reason = flag(o, &["reason"]).unwrap_or_else(|| "deixic-code remote stop".into());
    let payload = post(
        &config,
        STOP_PATH,
        strip_null(json!({"sessionId": id, "reason": reason})),
    )
    .await?;
    let session = require_session(&payload)?;
    if json_flag(o) {
        print_json(&json!({"session": session, "event": payload.get("event")}))?;
    } else {
        print_session(&session);
    }
    Ok(())
}

async fn cmd_extend(o: &Opts) -> Result<()> {
    let id = o.positionals.first().cloned().ok_or_else(|| {
        anyhow!(
            "{}",
            crate::localization::cli_locale().format(
                "Usage: deixic-code remote extend <session-id> --ttl 2h",
                &[]
            )
        )
    })?;
    let ttl = flag(o, &["ttl", "add-ttl"]).ok_or_else(|| {
        anyhow!(
            "{}",
            crate::localization::cli_locale()
                .format("deixic-code remote extend requires --ttl", &[])
        )
    })?;
    let config = require_config(&client_opts(o))?;
    let body = strip_null(json!({
        "sessionId": id,
        "additionalMinutes": ensure_pos(parse_minutes(Some(&ttl), 0)?, "additionalMinutes", 1440)?,
        "additionalIdleMinutes": ensure_nonneg(parse_minutes_opt(flag(o, &["idle-ttl", "add-idle-ttl"]))?, "additionalIdleMinutes", 1440)?,
        "reason": flag(o, &["reason"]).unwrap_or_else(|| "deixic-code remote extend".into()),
    }));
    let payload = post(&config, EXTEND_PATH, body).await?;
    let session = require_session(&payload)?;
    if json_flag(o) {
        print_json(&json!({"session": session, "event": payload.get("event")}))?;
    } else {
        print_session(&session);
    }
    Ok(())
}

async fn cmd_attach(o: &Opts) -> Result<()> {
    let id = o.positionals.first().cloned().ok_or_else(|| {
        anyhow!(
            "{}",
            crate::localization::cli_locale()
                .format("Usage: deixic-code remote attach <session-id>", &[])
        )
    })?;
    let roles = role_values(o)?;
    let minted = mint(
        &id,
        roles.clone(),
        parse_minutes(flag(o, &["ttl"]).as_deref(), 30)?,
        flag(o, &["subject", "user"]),
        &client_opts(o),
    )
    .await?;

    if should_use_interactive_remote_attach(
        json_flag(o),
        has_flag(o, "print-env"),
        io::stdin().is_terminal(),
        io::stdout().is_terminal(),
    ) {
        return attach_to_remote_runner_session(RemoteAttachInput {
            gateway_base_url: minted.gateway_base_url,
            session_id: id,
            token_id: minted.token.id,
            token_secret: minted.token_secret,
            role: attach_connection_role(&roles),
            client_version: Some(pkg_version()),
            take_control: has_flag(o, "take-control"),
        })
        .await;
    }

    let verified = if has_flag(o, "verify") {
        Some(verify_attach(&minted, &id, has_flag(o, "take-control")).await?)
    } else {
        None
    };
    print_attach_instr(
        &id,
        &minted,
        json_flag(o),
        has_flag(o, "show-secret") || has_flag(o, "print-env") || !json_flag(o),
        verified.as_ref(),
    )?;
    Ok(())
}

async fn cmd_attach_token(o: &Opts) -> Result<()> {
    let id = o.positionals.first().cloned().ok_or_else(|| {
        anyhow!(
            "{}",
            crate::localization::cli_locale()
                .format("Usage: deixic-code remote attach-token <session-id>", &[])
        )
    })?;
    let minted = mint(
        &id,
        role_values(o)?,
        parse_minutes(flag(o, &["ttl"]).as_deref(), 30)?,
        flag(o, &["subject", "user"]),
        &client_opts(o),
    )
    .await?;
    if json_flag(o) {
        print_json(&json!({
            "token": minted.token,
            "tokenSecret": minted.token_secret,
            "gatewayBaseUrl": minted.gateway_base_url,
        }))?;
        return Ok(());
    }
    println!(
        "{}",
        crate::localization::cli_locale()
            .format("Attach token for {0}", std::slice::from_ref(&(id)))
    );
    println!("  gateway: {}", minted.gateway_base_url);
    println!("  token:   {}", minted.token.id);
    println!("  secret:  {}", minted.token_secret);
    println!(
        "  expires: {}",
        minted.token.expires_at.as_deref().unwrap_or("-")
    );
    Ok(())
}

async fn cmd_revoke_token(o: &Opts) -> Result<()> {
    let sid = o.positionals.first().cloned();
    let tid = o.positionals.get(1).cloned();
    let (sid, tid) = match (sid, tid) {
        (Some(s), Some(t)) => (s, t),
        _ => bail!(
            "{}",
            crate::localization::cli_locale().format(
                "Usage: deixic-code remote revoke-token <session-id> <token-id>",
                &[]
            )
        ),
    };
    let config = require_config(&client_opts(o))?;
    let payload = post(
        &config,
        REVOKE_PATH,
        json!({"sessionId": sid, "tokenId": tid}),
    )
    .await?;
    let token = require_token(&payload)?;
    if json_flag(o) {
        print_json(&json!({"token": token, "event": payload.get("event")}))?;
    } else {
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "Revoked attach token {0}",
                std::slice::from_ref(&(token.id))
            )
        );
    }
    Ok(())
}

async fn cmd_target(o: &Opts) -> Result<()> {
    let id = o.positionals.first().cloned().ok_or_else(|| {
        anyhow!(
            "{}",
            crate::localization::cli_locale()
                .format("Usage: deixic-code remote target <session-id>", &[])
        )
    })?;
    let config = resolve_config(&client_opts(o))?.ok_or_else(|| {
        anyhow!(
            "{}",
            crate::localization::cli_locale().format(
                "Remote runner target requires EvalOps organization and access token.",
                &[]
            )
        )
    })?;
    let gw = gateway_url(&config.base_url, &id);
    if json_flag(o) {
        print_json(&json!({"sessionId": id, "gatewayBaseUrl": gw}))?;
    } else {
        println!("{gw}");
    }
    Ok(())
}

pub(crate) async fn mint(
    session_id: &str,
    roles: Vec<String>,
    ttl_minutes: u64,
    subject_id: Option<String>,
    co: &ClientOpts,
) -> Result<Minted> {
    let config = require_config(co)?;
    let roles = if roles.is_empty() {
        vec![ROLE_CONTROLLER.to_owned()]
    } else {
        roles
    };
    let ttl = ensure_nonneg(Some(ttl_minutes), "ttlMinutes", 60)?.unwrap_or(30);
    let body = strip_null(json!({
        "sessionId": session_id,
        "subjectId": subject_id,
        "roles": roles,
        "ttlMinutes": ttl,
    }));
    let payload = post(&config, MINT_PATH, body).await?;
    let secret = first_str(&payload, &["tokenSecret", "token_secret"]).ok_or_else(|| {
        anyhow!(
            "{}",
            crate::localization::cli_locale().format(
                "{0} returned no attach token secret",
                &[(SERVICE).to_string()]
            )
        )
    })?;
    Ok(Minted {
        token: require_token(&payload)?,
        token_secret: secret,
        gateway_base_url: gateway_url(&config.base_url, session_id),
    })
}

pub(crate) async fn get_session(id: &str, co: &ClientOpts) -> Result<Session> {
    let config = require_config(co)?;
    let payload = post(&config, GET_PATH, json!({"sessionId": id})).await?;
    require_session(&payload)
}

pub(crate) async fn wait_ready(
    id: &str,
    co: &ClientOpts,
    timeout_ms: u64,
    poll_ms: u64,
) -> Result<(Session, u32, u64)> {
    if timeout_ms < 1 {
        bail!(
            "{}",
            crate::localization::cli_locale()
                .format("Remote runner wait timeout must be at least 1ms", &[])
        );
    }
    let ready = [STATE_RUNNING, STATE_IDLE];
    let terminal = [STATE_STOPPED, STATE_EXPIRED, STATE_FAILED, STATE_LOST];
    let started = Instant::now();
    let mut attempts = 0u32;
    loop {
        attempts += 1;
        let session = get_session(id, co).await?;
        let elapsed = started.elapsed().as_millis() as u64;
        if state_matches(session.state.as_deref(), &ready) {
            return Ok((session, attempts, elapsed));
        }
        if state_matches(session.state.as_deref(), &terminal) {
            let label = state_label(session.state.as_deref());
            if let Some(r) = session
                .stop_reason
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                bail!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "Remote runner session {0} entered terminal state {1}: {2}",
                        &[(id).to_string(), (label).clone(), (r).to_string()]
                    )
                );
            }
            bail!(
                "{}",
                crate::localization::cli_locale().format(
                    "Remote runner session {0} entered terminal state {1}",
                    &[(id).to_string(), (label).clone()]
                )
            );
        }
        if elapsed >= timeout_ms {
            bail!("{}", crate::localization::cli_locale().format("Timed out after {0}ms waiting for remote runner session {1} to become ready (last state: {2})", &[(timeout_ms).to_string(), (id).to_string(), state_label(session.state.as_deref()).clone()]));
        }
        let sleep = poll_ms.min(timeout_ms.saturating_sub(elapsed));
        if sleep > 0 {
            tokio::time::sleep(Duration::from_millis(sleep)).await;
        }
    }
}

async fn verify_attach(minted: &Minted, session_id: &str, take_control: bool) -> Result<Value> {
    let client = Client::builder()
        .timeout(Duration::from_millis(DEFAULT_TIMEOUT_MS))
        .build()
        .context("create headless verify client")?;
    let url = format!(
        "{}/api/headless/connections",
        minted.gateway_base_url.trim_end_matches('/')
    );
    let body = json!({
        "sessionId": session_id,
        "protocolVersion": HEADLESS_PROTOCOL_VERSION,
        "connectionCapabilityRequired": true,
        "clientInfo": {"name": "maestro-remote-cli", "version": pkg_version()},
        "role": "controller",
        "takeControl": take_control,
        "optOutNotifications": ["heartbeat"],
        "capabilities": {
            "serverRequests": ["approval", "tool_retry"],
            "utilityOperations": ["command_exec", "file_search", "file_read", "file_watch"],
        },
    });
    let response = client
        .post(&url)
        .bearer_auth(&minted.token_secret)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("X-EvalOps-Runner-Attach-Token-Id", &minted.token.id)
        .json(&body)
        .send()
        .await
        .context("remote runner headless gateway request failed")?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "{}",
            crate::localization::cli_locale().format(
                "remote runner headless gateway returned {0}: {1}",
                &[
                    (status.as_u16()).to_string(),
                    (if text.trim().is_empty() {
                        status.canonical_reason().unwrap_or("error")
                    } else {
                        text.trim()
                    })
                    .to_string()
                ]
            )
        );
    }
    let payload: Value = if text.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(&text).context("parse headless connect response")?
    };
    let connection_id = first_str(&payload, &["connection_id", "connectionId"]);
    let disconnect_body = verify_disconnect_body(&payload);
    let runtime_session_id = first_str(&payload, &["session_id", "sessionId"]);
    if connection_id.is_some() || runtime_session_id.is_some() {
        let disconnect_id = runtime_session_id
            .clone()
            .unwrap_or_else(|| session_id.to_owned());
        let disconnect_url = format!(
            "{}/api/headless/sessions/{}/disconnect",
            minted.gateway_base_url.trim_end_matches('/'),
            urlencoding::encode(&disconnect_id)
        );
        let disconnect_response = client
            .post(disconnect_url)
            .bearer_auth(&minted.token_secret)
            .header("Content-Type", "application/json")
            .header("X-EvalOps-Runner-Attach-Token-Id", &minted.token.id)
            .json(&disconnect_body)
            .send()
            .await
            .context("remote runner headless disconnect request failed")?;
        let disconnect_status = disconnect_response.status();
        let disconnect_text = disconnect_response
            .text()
            .await
            .context("read remote runner headless disconnect response")?;
        ensure_verify_disconnect_success(disconnect_status, &disconnect_text)?;
    }
    Ok(json!({
        "sessionId": runtime_session_id,
        "connectionId": connection_id,
        "heartbeatIntervalMs": first_num(&payload, &["heartbeat_interval_ms", "heartbeatIntervalMs"]),
        "role": first_str(&payload, &["role"]),
    }))
}

fn verify_disconnect_body(payload: &Value) -> Value {
    json!({
        "connectionId": first_str(payload, &["connection_id", "connectionId"]),
        "connectionCapability": first_str(payload, &["connection_capability", "connectionCapability"]),
    })
}

fn ensure_verify_disconnect_success(status: StatusCode, body: &str) -> Result<()> {
    if status.is_success() {
        return Ok(());
    }
    let trimmed = body.trim();
    let detail = if trimmed.is_empty() {
        status.canonical_reason().unwrap_or("error").to_owned()
    } else {
        let mut chars = trimmed.chars();
        let mut bounded = chars
            .by_ref()
            .take(VERIFY_ERROR_BODY_MAX_CHARS)
            .collect::<String>();
        if chars.next().is_some() {
            bounded.push('…');
        }
        bounded
    };
    bail!(
        "{}",
        crate::localization::cli_locale().format(
            "remote runner headless disconnect returned {0}: {1}",
            &[(status.as_u16()).to_string(), (detail).clone()]
        )
    )
}

pub(crate) async fn post(config: &Config, path: &str, body: Value) -> Result<Value> {
    let client = Client::builder()
        .timeout(Duration::from_millis(config.timeout_ms))
        .build()
        .context("create remote runner HTTP client")?;
    let url = format!("{}{}", config.base_url.trim_end_matches('/'), path);
    let mut last_err = None;
    for attempt in 0..config.max_attempts.max(1) {
        match client
            .post(&url)
            .bearer_auth(&config.token)
            .header("Content-Type", "application/json")
            .header("Connect-Protocol-Version", CONNECT_VERSION)
            .header("X-Organization-ID", &config.organization_id)
            .json(&body)
            .send()
            .await
        {
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                if retryable(status) && attempt + 1 < config.max_attempts {
                    tokio::time::sleep(Duration::from_millis(100 * (1 << attempt) as u64)).await;
                    last_err = Some(anyhow!("{SERVICE} returned {}: {}", status.as_u16(), text));
                    continue;
                }
                if !status.is_success() {
                    bail!(
                        "{SERVICE} returned {}: {}",
                        status.as_u16(),
                        if text.trim().is_empty() {
                            status.canonical_reason().unwrap_or("error")
                        } else {
                            text.trim()
                        }
                    );
                }
                if text.trim().is_empty() {
                    bail!(
                        "{}",
                        crate::localization::cli_locale()
                            .format("{0} returned empty response", &[(SERVICE).to_string()])
                    );
                }
                return serde_json::from_str(&text)
                    .with_context(|| format!("parse {SERVICE} response"));
            }
            Err(e) if attempt + 1 < config.max_attempts => {
                last_err = Some(anyhow!("{e:#}"));
                tokio::time::sleep(Duration::from_millis(100 * (1 << attempt) as u64)).await;
            }
            Err(e) => return Err(e).context(format!("{SERVICE} request failed")),
        }
    }
    Err(last_err.unwrap_or_else(|| {
        anyhow!(
            "{}",
            crate::localization::cli_locale()
                .format("{0} request failed", &[(SERVICE).to_string()])
        )
    }))
}

fn retryable(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 429) || status.is_server_error()
}

pub(crate) fn require_config(o: &ClientOpts) -> Result<Config> {
    resolve_config(o)?.ok_or_else(|| {
        anyhow!("{}", crate::localization::cli_locale().format("Remote runner requires EvalOps organization and access token. Set MAESTRO_REMOTE_RUNNER_ORG_ID/MAESTRO_EVALOPS_ORG_ID and MAESTRO_REMOTE_RUNNER_TOKEN/MAESTRO_EVALOPS_ACCESS_TOKEN, or run EvalOps login.", &[]))
    })
}

pub(crate) fn resolve_config(o: &ClientOpts) -> Result<Option<Config>> {
    let organization_id = trim(o.organization_id.clone().or_else(|| env_first(ORG_ENV)));
    let token = trim(o.token.clone().or_else(|| env_first(TOKEN_ENV)));
    let stored = if organization_id.is_none() || token.is_none() {
        crate::init_cli::load_evalops_snapshot().ok().flatten()
    } else {
        None
    };
    let organization_id = organization_id.or_else(|| {
        stored
            .as_ref()
            .and_then(|snapshot| snapshot.organization_id.clone())
    });
    let token = token.or_else(|| stored.as_ref().map(|snapshot| snapshot.access.clone()));
    let (Some(organization_id), Some(token)) = (organization_id, token) else {
        return Ok(None);
    };
    let base = trim(o.base_url.clone().or_else(|| env_first(BASE_URL_ENV)));
    Ok(Some(Config {
        base_url: normalize_base(base.as_deref().unwrap_or(DEFAULT_BASE_URL)),
        token,
        organization_id,
        workspace_id: trim(o.workspace_id.clone().or_else(|| env_first(WORKSPACE_ENV))),
        timeout_ms: DEFAULT_TIMEOUT_MS,
        max_attempts: DEFAULT_MAX_ATTEMPTS,
    }))
}

fn workspace_required(cli: Option<String>, co: &ClientOpts, verb: &str) -> Result<String> {
    let config = require_config(co)?;
    trim(cli.or(config.workspace_id)).ok_or_else(|| {
        anyhow!("{}", crate::localization::cli_locale().format("Remote runner {0} requires a workspace id. Pass --workspace or set MAESTRO_REMOTE_RUNNER_WORKSPACE_ID.", &[(verb).to_string()]))
    })
}

pub(crate) fn gateway_url(base: &str, session_id: &str) -> String {
    format!(
        "{}/v1/runner-sessions/{}/headless",
        base.trim_end_matches('/'),
        urlencoding::encode(session_id)
    )
}

pub(crate) fn normalize_base(base: &str) -> String {
    let mut n = base.trim().trim_end_matches('/').to_owned();
    for suffix in [
        CREATE_PATH,
        GET_PATH,
        LIST_PATH,
        STOP_PATH,
        EXTEND_PATH,
        MINT_PATH,
        REVOKE_PATH,
        EVENTS_PATH,
        STATUS_PATH,
        SERVICE_PATH,
        "/v1/runner-sessions",
        "/v1/runner-sessions/",
    ] {
        if n.ends_with(suffix) {
            n.truncate(n.len() - suffix.len());
            n = n.trim_end_matches('/').to_owned();
        }
    }
    if n.is_empty() {
        DEFAULT_BASE_URL.to_owned()
    } else {
        n
    }
}

fn parse_opts(args: &[String]) -> Opts {
    let mut flags = BTreeMap::new();
    let mut positionals = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            positionals.extend(args[i + 1..].iter().cloned());
            break;
        }
        if !arg.starts_with("--") {
            positionals.push(arg.clone());
            i += 1;
            continue;
        }
        let (key, inline) = if let Some((k, v)) = arg[2..].split_once('=') {
            (k.to_owned(), Some(v.to_owned()))
        } else {
            (arg[2..].to_owned(), None)
        };
        let val = if let Some(v) = inline {
            FlagVal::Str(v)
        } else if let Some(next) = args.get(i + 1) {
            if !next.starts_with("--") {
                i += 1;
                FlagVal::Str(next.clone())
            } else {
                FlagVal::Bool
            }
        } else {
            FlagVal::Bool
        };
        flags.entry(key).or_insert_with(Vec::new).push(val);
        i += 1;
    }
    Opts { flags, positionals }
}

fn has_flag(o: &Opts, name: &str) -> bool {
    o.flags.contains_key(name)
}
fn json_flag(o: &Opts) -> bool {
    has_flag(o, "json")
}
fn flag(o: &Opts, names: &[&str]) -> Option<String> {
    for name in names {
        if let Some(vals) = o.flags.get(*name) {
            if let Some(FlagVal::Str(s)) = vals.last() {
                let t = s.trim();
                if !t.is_empty() {
                    return Some(t.to_owned());
                }
            }
        }
    }
    None
}
fn repeated(o: &Opts, names: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for name in names {
        if let Some(vals) = o.flags.get(*name) {
            for v in vals {
                if let FlagVal::Str(s) = v {
                    let t = s.trim();
                    if !t.is_empty() {
                        out.push(t.to_owned());
                    }
                }
            }
        }
    }
    out
}
fn client_opts(o: &Opts) -> ClientOpts {
    ClientOpts {
        base_url: flag(o, &["base-url", "url"]),
        token: flag(o, &["token"]),
        organization_id: flag(o, &["org", "organization"]),
        workspace_id: flag(o, &["workspace"]),
    }
}
fn int_flag(o: &Opts, name: &str, fallback: Option<u64>) -> Result<Option<u64>> {
    match flag(o, &[name]) {
        None => Ok(fallback),
        Some(raw) => {
            let p: i64 = raw.parse().map_err(|_| {
                anyhow!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "--{0} must be a non-negative integer",
                        &[(name).to_string()]
                    )
                )
            })?;
            if p < 0 {
                bail!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "--{0} must be a non-negative integer",
                        &[(name).to_string()]
                    )
                );
            }
            Ok(Some(p as u64))
        }
    }
}

pub fn parse_remote_duration_minutes(raw: Option<&str>, fallback: u64) -> Result<u64> {
    parse_minutes(raw, fallback)
}

pub(crate) fn parse_minutes(raw: Option<&str>, fallback: u64) -> Result<u64> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(fallback);
    };
    let value = raw.to_ascii_lowercase();
    let re =
        regex::Regex::new(r"^(\d+(?:\.\d+)?)(m|min|mins|minute|minutes|h|hr|hrs|hour|hours)?$")
            .unwrap();
    let caps = re.captures(&value).ok_or_else(|| {
        anyhow!(
            "{}",
            crate::localization::cli_locale().format(
                "Invalid duration \"{0}\". Use minutes, 90m, or 2h.",
                &[(raw).to_string()]
            )
        )
    })?;
    let amount: f64 = caps[1].parse().map_err(|_| {
        anyhow!(
            "{}",
            crate::localization::cli_locale().format(
                "Invalid duration \"{0}\". Use minutes, 90m, or 2h.",
                &[(raw).to_string()]
            )
        )
    })?;
    let unit = caps.get(2).map(|m| m.as_str()).unwrap_or("m");
    let minutes = if unit.starts_with('h') {
        amount * 60.0
    } else {
        amount
    };
    if !minutes.is_finite() || minutes <= 0.0 || minutes.fract().abs() > f64::EPSILON {
        bail!(
            "{}",
            crate::localization::cli_locale().format(
                "Invalid duration \"{0}\". Duration must resolve to whole minutes.",
                &[(raw).to_string()]
            )
        );
    }
    Ok(minutes as u64)
}
fn parse_minutes_opt(raw: Option<String>) -> Result<Option<u64>> {
    match raw {
        None => Ok(None),
        Some(v) => Ok(Some(parse_minutes(Some(&v), 0)?)),
    }
}
fn parse_wait_ms(raw: Option<String>, fallback: u64) -> Result<u64> {
    let Some(raw) = raw else { return Ok(fallback) };
    let value = raw.trim().to_ascii_lowercase();
    if value.is_empty() {
        return Ok(fallback);
    }
    let re = regex::Regex::new(
        r"^(\d+(?:\.\d+)?)(ms|msec|millisecond|milliseconds|s|sec|secs|second|seconds|m|min|mins|minute|minutes|h|hr|hrs|hour|hours)?$",
    )
    .unwrap();
    let caps = re.captures(&value).ok_or_else(|| {
        anyhow!(
            "{}",
            crate::localization::cli_locale().format(
                "Invalid wait duration \"{0}\". Use values like 5s, 30s, 5m, or 1h.",
                std::slice::from_ref(&(raw))
            )
        )
    })?;
    let amount: f64 = caps[1].parse().map_err(|_| {
        anyhow!(
            "{}",
            crate::localization::cli_locale().format(
                "Invalid wait duration \"{0}\". Use values like 5s, 30s, 5m, or 1h.",
                std::slice::from_ref(&(raw))
            )
        )
    })?;
    let unit = caps.get(2).map(|m| m.as_str()).unwrap_or("s");
    let ms = if unit == "ms" || unit == "msec" || unit.starts_with("millisecond") {
        amount
    } else if unit.starts_with('m') {
        amount * 60_000.0
    } else if unit.starts_with('h') {
        amount * 3_600_000.0
    } else {
        amount * 1_000.0
    };
    if !ms.is_finite() || ms < 1.0 {
        bail!(
            "{}",
            crate::localization::cli_locale().format(
                "Invalid wait duration \"{0}\". Duration must resolve to at least 1ms.",
                std::slice::from_ref(&(raw))
            )
        );
    }
    Ok(ms.round() as u64)
}
fn parse_metadata(values: &[String]) -> Result<Option<Map<String, Value>>> {
    if values.is_empty() {
        return Ok(None);
    }
    let mut m = Map::new();
    for v in values {
        let eq = v.find('=').filter(|i| *i > 0).ok_or_else(|| {
            anyhow!(
                "{}",
                crate::localization::cli_locale().format(
                    "--metadata values must be key=value pairs: {0}",
                    std::slice::from_ref(v)
                )
            )
        })?;
        let key = v[..eq].trim();
        let raw = v[eq + 1..].trim();
        if key.is_empty() {
            bail!(
                "{}",
                crate::localization::cli_locale().format(
                    "--metadata values must include a key: {0}",
                    std::slice::from_ref(v)
                )
            );
        }
        m.insert(key.to_owned(), Value::String(raw.to_owned()));
    }
    Ok(Some(m))
}
fn role_values(o: &Opts) -> Result<Vec<String>> {
    let raw = repeated(o, &["role", "roles"]);
    let roles: Vec<String> = if !raw.is_empty() {
        raw.into_iter()
            .flat_map(|r| {
                r.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .collect()
    } else if has_flag(o, "viewer") {
        vec!["viewer".into()]
    } else if has_flag(o, "admin") {
        vec!["admin".into()]
    } else {
        vec!["controller".into()]
    };
    roles.into_iter().map(|r| normalize_role(&r)).collect()
}

/// Map minted attach roles to the headless connection role.
/// Viewer-only role sets attach as viewer; any controller/admin capability attaches as controller.
pub(crate) fn attach_connection_role(roles: &[String]) -> AttachRole {
    if !roles.is_empty() && roles.iter().all(|role| role == ROLE_VIEWER) {
        AttachRole::Viewer
    } else {
        AttachRole::Controller
    }
}
fn normalize_role(role: &str) -> Result<String> {
    let n = role
        .trim()
        .to_ascii_uppercase()
        .trim_start_matches("RUNNER_ATTACH_ROLE_")
        .to_owned();
    Ok(match n.as_str() {
        "VIEWER" => ROLE_VIEWER,
        "CONTROLLER" => ROLE_CONTROLLER,
        "ADMIN" => ROLE_ADMIN,
        _ => bail!(
            "{}",
            crate::localization::cli_locale()
                .format("Unknown remote attach role: {0}", &[(role).to_string()])
        ),
    }
    .to_owned())
}
fn normalize_state(state: &str) -> Result<String> {
    let n = state
        .trim()
        .to_ascii_uppercase()
        .trim_start_matches("RUNNER_SESSION_STATE_")
        .to_owned();
    let matched = match n.as_str() {
        "REQUESTED" => "RUNNER_SESSION_STATE_REQUESTED",
        "PROVISIONING" => "RUNNER_SESSION_STATE_PROVISIONING",
        "RUNNING" => STATE_RUNNING,
        "IDLE" => STATE_IDLE,
        "STOPPING" => "RUNNER_SESSION_STATE_STOPPING",
        "STOPPED" => STATE_STOPPED,
        "EXPIRED" => STATE_EXPIRED,
        "FAILED" => STATE_FAILED,
        "LOST" => STATE_LOST,
        _ => bail!(
            "{}",
            crate::localization::cli_locale()
                .format("Unknown runner session state: {0}", &[(state).to_string()])
        ),
    };
    Ok(matched.to_owned())
}
fn state_matches(state: Option<&str>, expected: &[&str]) -> bool {
    let Some(state) = state else { return false };
    match normalize_state(state) {
        Ok(n) => expected.iter().any(|e| *e == n),
        Err(_) => false,
    }
}
fn state_label(state: Option<&str>) -> String {
    state
        .unwrap_or("unknown")
        .trim_start_matches("RUNNER_SESSION_STATE_")
        .to_ascii_lowercase()
        .replace('_', "-")
}
pub(crate) fn require_session(payload: &Value) -> Result<Session> {
    payload
        .get("session")
        .and_then(norm_session)
        .ok_or_else(|| {
            anyhow!(
                "{}",
                crate::localization::cli_locale()
                    .format("{0} returned no runner session", &[(SERVICE).to_string()])
            )
        })
}
fn require_token(payload: &Value) -> Result<AttachToken> {
    payload.get("token").and_then(norm_token).ok_or_else(|| {
        anyhow!(
            "{}",
            crate::localization::cli_locale()
                .format("{0} returned no attach token", &[(SERVICE).to_string()])
        )
    })
}
fn norm_session(v: &Value) -> Option<Session> {
    let o = v.as_object()?;
    Some(Session {
        id: first_str_map(o, &["id"])?,
        workspace_id: first_str_map(o, &["workspaceId", "workspace_id"]),
        state: first_str_map(o, &["state"]),
        runner_profile: first_str_map(o, &["runnerProfile", "runner_profile"]),
        maestro_session_id: first_str_map(o, &["maestroSessionId", "maestro_session_id"]),
        repo_url: first_str_map(o, &["repoUrl", "repo_url"]),
        branch: first_str_map(o, &["branch"]),
        expires_at: first_str_map(o, &["expiresAt", "expires_at"]),
        idle_expires_at: first_str_map(o, &["idleExpiresAt", "idle_expires_at"]),
        stop_reason: first_str_map(o, &["stopReason", "stop_reason"]),
    })
}
fn norm_token(v: &Value) -> Option<AttachToken> {
    let o = v.as_object()?;
    Some(AttachToken {
        id: first_str_map(o, &["id"])?,
        expires_at: first_str_map(o, &["expiresAt", "expires_at"]),
    })
}
pub(crate) fn array_sessions(payload: &Value) -> Vec<Session> {
    payload
        .get("sessions")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(norm_session).collect())
        .unwrap_or_default()
}
pub(crate) fn array_events(payload: &Value) -> Vec<Event> {
    payload
        .get("events")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| {
                    let o = v.as_object()?;
                    Some(Event {
                        sequence: first_num_map(o, &["sequence"]),
                        event_type: first_str_map(o, &["eventType", "event_type"]),
                        occurred_at: first_str_map(o, &["occurredAt", "occurred_at"]),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}
pub(crate) fn first_str(v: &Value, names: &[&str]) -> Option<String> {
    v.as_object().and_then(|m| first_str_map(m, names))
}
fn first_str_map(m: &Map<String, Value>, names: &[&str]) -> Option<String> {
    for n in names {
        if let Some(Value::String(s)) = m.get(*n) {
            let t = s.trim();
            if !t.is_empty() {
                return Some(t.to_owned());
            }
        }
    }
    None
}
pub(crate) fn first_num(v: &Value, names: &[&str]) -> Option<f64> {
    v.as_object().and_then(|m| first_num_map(m, names))
}
fn first_num_map(m: &Map<String, Value>, names: &[&str]) -> Option<f64> {
    for n in names {
        match m.get(*n) {
            Some(Value::Number(num)) => return num.as_f64(),
            Some(Value::String(s)) => {
                if let Ok(p) = s.parse::<f64>() {
                    if p.is_finite() {
                        return Some(p);
                    }
                }
            }
            _ => {}
        }
    }
    None
}
fn ensure_pos(v: u64, field: &str, max: u64) -> Result<u64> {
    if v < 1 || v > max {
        bail!(
            "{}",
            crate::localization::cli_locale().format(
                "{0} must be an integer between 1 and {1}",
                &[(field).to_string(), (max).to_string()]
            )
        );
    }
    Ok(v)
}
fn ensure_nonneg(v: Option<u64>, field: &str, max: u64) -> Result<Option<u64>> {
    match v {
        None => Ok(None),
        Some(x) if x > max => bail!(
            "{}",
            crate::localization::cli_locale().format(
                "{0} must be an integer between 0 and {1}",
                &[(field).to_string(), (max).to_string()]
            )
        ),
        Some(x) => Ok(Some(x)),
    }
}
pub(crate) fn strip_null(v: Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, val) in map {
                if !val.is_null() {
                    out.insert(k, strip_null(val));
                }
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.into_iter().map(strip_null).collect()),
        other => other,
    }
}
pub(crate) fn trim(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}
pub(crate) fn env_first(names: &[&str]) -> Option<String> {
    names.iter().find_map(|n| {
        std::env::var(n)
            .ok()
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    })
}
pub(crate) fn pkg_version() -> String {
    std::env::var("MAESTRO_VERSION")
        .or_else(|_| std::env::var("CARGO_PKG_VERSION"))
        .unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_owned())
}
pub(crate) fn print_json(v: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}
pub(crate) fn print_session(s: &Session) {
    println!("{}", s.id);
    println!("  state:     {}", state_label(s.state.as_deref()));
    println!("  workspace: {}", s.workspace_id.as_deref().unwrap_or("-"));
    println!(
        "  profile:   {}",
        s.runner_profile.as_deref().unwrap_or("-")
    );
    println!("  repo:      {}", s.repo_url.as_deref().unwrap_or("-"));
    println!("  branch:    {}", s.branch.as_deref().unwrap_or("-"));
    println!("  expires:   {}", s.expires_at.as_deref().unwrap_or("-"));
    println!(
        "  idle:      {}",
        s.idle_expires_at.as_deref().unwrap_or("-")
    );
    if let Some(r) = &s.stop_reason {
        println!("  stopped:   {r}");
    }
}
pub(crate) fn print_table(sessions: &[Session]) {
    if sessions.is_empty() {
        println!(
            "{}",
            crate::localization::cli_locale().format("No remote runner sessions found.", &[])
        );
        return;
    }
    let rows: Vec<_> = sessions
        .iter()
        .map(|s| {
            (
                s.id.clone(),
                state_label(s.state.as_deref()),
                s.runner_profile.clone().unwrap_or_else(|| "-".into()),
                s.repo_url.clone().unwrap_or_else(|| "-".into()),
                s.branch.clone().unwrap_or_else(|| "-".into()),
                s.expires_at.clone().unwrap_or_else(|| "-".into()),
            )
        })
        .collect();
    let id_w = rows.iter().map(|r| r.0.len()).max().unwrap_or(0).max(7);
    let st_w = rows.iter().map(|r| r.1.len()).max().unwrap_or(0).max(5);
    let pr_w = rows.iter().map(|r| r.2.len()).max().unwrap_or(0).max(7);
    let re_w = rows.iter().map(|r| r.3.len()).max().unwrap_or(0).max(4);
    let br_w = rows.iter().map(|r| r.4.len()).max().unwrap_or(0).max(6);
    println!(
        "{:<id_w$}  {:<st_w$}  {:<pr_w$}  {:<re_w$}  {:<br_w$}  expires",
        "session", "state", "profile", "repo", "branch"
    );
    for r in rows {
        println!(
            "{:<id_w$}  {:<st_w$}  {:<pr_w$}  {:<re_w$}  {:<br_w$}  {}",
            r.0, r.1, r.2, r.3, r.4, r.5
        );
    }
}
fn print_attach_instr(
    session_id: &str,
    minted: &Minted,
    as_json: bool,
    show_secret: bool,
    verified: Option<&Value>,
) -> Result<()> {
    if as_json {
        print_json(&json!({
            "sessionId": session_id,
            "gatewayBaseUrl": minted.gateway_base_url,
            "tokenId": minted.token.id,
            "tokenSecret": minted.token_secret,
            "expiresAt": minted.token.expires_at,
            "verified": verified,
        }))?;
        return Ok(());
    }
    println!(
        "{}",
        crate::localization::cli_locale().format(
            "Remote runner attach token minted for {0}",
            &[(session_id).to_string()]
        )
    );
    println!("  gateway: {}", minted.gateway_base_url);
    println!("  token:   {}", minted.token.id);
    println!(
        "  expires: {}",
        minted.token.expires_at.as_deref().unwrap_or("-")
    );
    if verified.is_some() {
        println!(
            "{}",
            crate::localization::cli_locale().format("  headless gateway: verified", &[])
        );
    }
    if show_secret || verified.is_none() {
        println!();
        println!(
            "{}",
            crate::localization::cli_locale()
                .format("Ephemeral remote transport environment:", &[])
        );
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "export MAESTRO_REMOTE_BASE_URL={0}",
                &[(shell_quote(&minted.gateway_base_url)).clone()]
            )
        );
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "export MAESTRO_REMOTE_API_KEY={0}",
                &[(shell_quote(&minted.token_secret)).clone()]
            )
        );
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "export MAESTRO_REMOTE_HEADER_X_EVALOPS_RUNNER_ATTACH_TOKEN_ID={0}",
                &[(shell_quote(&minted.token.id)).clone()]
            )
        );
    } else {
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "  token secret hidden; rerun with --show-secret or --json when handoff needs it.",
                &[]
            )
        );
    }
    Ok(())
}
fn shell_quote(v: &str) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| format!("\"{v}\""))
}
pub(crate) fn format_elapsed(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        let s = ms as f64 / 1000.0;
        if ms.is_multiple_of(1000) {
            format!("{s:.0}s")
        } else {
            format!("{s:.1}s")
        }
    } else if ms < 3_600_000 {
        let m = ms as f64 / 60_000.0;
        if ms.is_multiple_of(60_000) {
            format!("{m:.0}m")
        } else {
            format!("{m:.1}m")
        }
    } else {
        let h = ms as f64 / 3_600_000.0;
        if ms.is_multiple_of(3_600_000) {
            format!("{h:.0}h")
        } else {
            format!("{h:.1}h")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        Config {
            base_url: "https://runner.example.test".into(),
            token: "test-token".into(),
            organization_id: "org-1".into(),
            workspace_id: Some("workspace-1".into()),
            timeout_ms: DEFAULT_TIMEOUT_MS,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
        }
    }

    #[test]
    fn start_request_omits_unbound_hydration_locators() {
        let opts = parse_opts(&[
            "--workspace".into(),
            "workspace-1".into(),
            "--idempotency-key".into(),
            "start-1".into(),
        ]);

        let body = start_request(&opts, &test_config()).unwrap();
        let object = body.as_object().unwrap();
        for locator in ["workspaceSource", "repoUrl", "branch"] {
            assert!(
                !object.contains_key(locator),
                "unexpected {locator}: {body}"
            );
        }
        assert_eq!(body["organizationId"], "org-1");
        assert_eq!(body["workspaceId"], "workspace-1");
        assert_eq!(body["idempotencyKey"], "start-1");
        assert_eq!(body["runnerProfile"], "standard");
        assert_eq!(body["ttlMinutes"], 90);
    }

    #[test]
    fn start_rejects_explicit_unbound_hydration_locators() {
        for (name, value) in [
            ("--workspace-source", "git"),
            ("--repo", "https://example.test/repo.git"),
            ("--repo-url", "https://example.test/repo.git"),
            ("--branch", "main"),
        ] {
            let opts = parse_opts(&[
                "--workspace".into(),
                "workspace-1".into(),
                name.into(),
                value.into(),
            ]);
            let error = start_request(&opts, &test_config())
                .expect_err("unsigned locator must fail before the request")
                .to_string();
            assert!(error.contains(name), "{name}: {error}");
            assert!(error.contains("signed immutable hydration envelope"));
        }
    }

    #[test]
    fn parses_ttl_minutes() {
        assert_eq!(parse_minutes(Some("90m"), 1).unwrap(), 90);
        assert_eq!(parse_minutes(Some("2h"), 1).unwrap(), 120);
        assert_eq!(parse_minutes(Some("45"), 1).unwrap(), 45);
        assert_eq!(parse_minutes(None, 30).unwrap(), 30);
    }

    #[test]
    fn rejects_fractional_ttl() {
        assert!(
            parse_minutes(Some("1.5m"), 1)
                .unwrap_err()
                .to_string()
                .contains("whole minutes")
        );
        assert!(
            parse_minutes(Some("soon"), 1)
                .unwrap_err()
                .to_string()
                .contains("Invalid duration")
        );
    }

    #[test]
    fn parses_wait_ms() {
        assert_eq!(parse_wait_ms(Some("2m".into()), 0).unwrap(), 120_000);
        assert_eq!(parse_wait_ms(Some("5s".into()), 0).unwrap(), 5_000);
    }

    #[test]
    fn normalizes_roles_states_base() {
        assert_eq!(normalize_role("viewer").unwrap(), ROLE_VIEWER);
        assert_eq!(normalize_state("running").unwrap(), STATE_RUNNING);
        assert_eq!(
            normalize_base("https://runner.evalops.dev/v1/runner-sessions/"),
            "https://runner.evalops.dev"
        );
    }

    #[test]
    fn maps_attach_connection_role_from_minted_roles() {
        assert_eq!(
            attach_connection_role(&[ROLE_VIEWER.to_owned()]),
            AttachRole::Viewer
        );
        assert_eq!(
            attach_connection_role(&[ROLE_CONTROLLER.to_owned()]),
            AttachRole::Controller
        );
        assert_eq!(
            attach_connection_role(&[ROLE_VIEWER.to_owned(), ROLE_CONTROLLER.to_owned()]),
            AttachRole::Controller
        );
        assert_eq!(attach_connection_role(&[]), AttachRole::Controller);
    }

    #[test]
    fn verify_disconnect_retains_private_connection_capability() {
        let body = verify_disconnect_body(&json!({
            "connection_id": "conn_verify",
            "connection_capability": "cap_00112233445566778899aabbccddeeff",
        }));

        assert_eq!(body["connectionId"], "conn_verify");
        assert_eq!(
            body["connectionCapability"],
            "cap_00112233445566778899aabbccddeeff"
        );
    }

    #[test]
    fn verify_disconnect_rejects_non_success_with_bounded_context() {
        let long_body = format!("cleanup unavailable: {}", "x".repeat(1_024));
        let error = ensure_verify_disconnect_success(StatusCode::BAD_GATEWAY, &long_body)
            .expect_err("non-success cleanup must fail verification")
            .to_string();

        assert!(error.contains("headless disconnect returned 502"));
        assert!(error.contains("cleanup unavailable"));
        assert!(error.ends_with('…'));
        assert!(error.chars().count() < VERIFY_ERROR_BODY_MAX_CHARS + 100);
        assert!(ensure_verify_disconnect_success(StatusCode::NO_CONTENT, "").is_ok());
        assert!(
            ensure_verify_disconnect_success(StatusCode::BAD_GATEWAY, "")
                .expect_err("empty error response must still fail")
                .to_string()
                .contains("Bad Gateway")
        );
    }

    #[tokio::test]
    async fn help_and_unknown() {
        assert_eq!(run_remote(&[]).await.unwrap(), 0);
        assert_eq!(run_remote(&["help".into()]).await.unwrap(), 0);
        assert_eq!(run_remote(&["nope".into()]).await.unwrap(), 1);
    }
}
