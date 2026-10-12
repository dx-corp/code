//! Native `maestro operating-plane` CLI.
//!
//! Ports `src/cli/commands/operating-plane.ts`. Fetches the Platform operating-plane
//! ledger and prints a content-free runtime status summary for operators.

use anyhow::{Result, bail};

use crate::operating_plane_client::{
    OperatingPlaneInspection, OperatingPlaneRunQuery, inspect_operating_plane_runs,
};
use crate::operating_plane_summary::{
    OperatingPlaneStatusReport, format_operating_plane_status_report,
    summarize_operating_plane_inspection,
};

const VALUE_FLAGS: &[&str] = &[
    "--agent-id",
    "--artifact-id",
    "--audience",
    "--auth-subject",
    "--autonomy-session-id",
    "--channel-thread-id",
    "--evidence-id",
    "--gateway-authenticated-subject",
    "--limit",
    "--run-id",
    "--session-id",
    "--thread-id",
    "--trace-id",
    "--work-envelope-id",
    "--workspace-id",
];

const BOOLEAN_FLAGS: &[&str] = &["--help", "--include-gates", "--json"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedOperatingPlaneArgs {
    pub subcommand: String,
    pub query: OperatingPlaneRunQuery,
    pub json: bool,
    pub help: bool,
}

pub async fn run_operating_plane(args: &[String]) -> Result<i32> {
    let parsed = match parse_operating_plane_args(args) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("{error:#}");
            return Ok(1);
        }
    };

    if parsed.help || parsed.subcommand == "help" {
        println!("{}", operating_plane_help_text());
        return Ok(0);
    }
    if parsed.subcommand != "status" && parsed.subcommand != "inspect" {
        eprintln!(
            "{}",
            maestro_local_host::localization::cli_locale().format(
                "Unknown operating-plane command: {0}",
                std::slice::from_ref(&(parsed.subcommand))
            )
        );
        return Ok(1);
    }

    match run_status(&parsed).await {
        Ok(output) => {
            println!("{output}");
            Ok(0)
        }
        Err(error) => {
            eprintln!("{error:#}");
            Ok(1)
        }
    }
}

async fn run_status(parsed: &ParsedOperatingPlaneArgs) -> Result<String> {
    let inspection = inspect_operating_plane_runs(&parsed.query, None).await?;
    Ok(render_report(
        &summarize_operating_plane_inspection(&inspection, None),
        parsed.json,
    ))
}

/// Test/helper entry that accepts an already-fetched inspection payload.
pub fn render_from_inspection(inspection: &OperatingPlaneInspection, json: bool) -> String {
    render_report(
        &summarize_operating_plane_inspection(inspection, None),
        json,
    )
}

fn render_report(report: &OperatingPlaneStatusReport, json: bool) -> String {
    if json {
        serde_json::to_string_pretty(report).unwrap_or_else(|_| "{}".to_owned())
    } else {
        format_operating_plane_status_report(report)
    }
}

pub fn parse_operating_plane_args(args: &[String]) -> Result<ParsedOperatingPlaneArgs> {
    let mut positionals: Vec<String> = Vec::new();
    let mut flags: std::collections::HashMap<String, FlagValue> = std::collections::HashMap::new();
    let mut index = 0usize;
    while index < args.len() {
        let arg = &args[index];
        if arg.is_empty() {
            index += 1;
            continue;
        }
        if arg == "--" {
            positionals.extend(args[index + 1..].iter().cloned());
            break;
        }
        if !arg.starts_with("--") {
            positionals.push(arg.clone());
            index += 1;
            continue;
        }

        let (flag, inline_value) = split_flag(arg);
        if !VALUE_FLAGS.contains(&flag.as_str()) && !BOOLEAN_FLAGS.contains(&flag.as_str()) {
            bail!(
                "{}",
                maestro_local_host::localization::cli_locale().format(
                    "Unknown operating-plane option: {0}",
                    std::slice::from_ref(&(flag))
                )
            );
        }
        if let Some(inline) = inline_value {
            flags.insert(flag, FlagValue::String(inline));
            index += 1;
            continue;
        }
        if BOOLEAN_FLAGS.contains(&flag.as_str()) {
            flags.insert(flag, FlagValue::Bool(true));
            index += 1;
            continue;
        }
        let next = args.get(index + 1);
        if next.is_none_or(|value| value == "--" || value.starts_with("--")) {
            bail!("{flag} requires a value");
        }
        flags.insert(flag, FlagValue::String(next.unwrap().clone()));
        index += 2;
    }

    let mut positionals = positionals.into_iter();
    let subcommand = positionals
        .next()
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_else(|| "status".to_owned());

    Ok(ParsedOperatingPlaneArgs {
        subcommand,
        query: query_from_flags(&flags)?,
        json: flag_boolean(&flags, "--json")? == Some(true),
        help: flag_boolean(&flags, "--help")? == Some(true),
    })
}

#[derive(Debug, Clone)]
enum FlagValue {
    Bool(bool),
    String(String),
}

fn split_flag(arg: &str) -> (String, Option<String>) {
    match arg.split_once('=') {
        Some((flag, value)) => (flag.to_owned(), Some(value.to_owned())),
        None => (arg.to_owned(), None),
    }
}

fn query_from_flags(
    flags: &std::collections::HashMap<String, FlagValue>,
) -> Result<OperatingPlaneRunQuery> {
    Ok(OperatingPlaneRunQuery {
        workspace_id: flag_string(flags, "--workspace-id"),
        run_id: flag_string(flags, "--run-id"),
        work_envelope_id: flag_string(flags, "--work-envelope-id"),
        autonomy_session_id: flag_string(flags, "--autonomy-session-id"),
        agent_id: flag_string(flags, "--agent-id"),
        thread_id: flag_string(flags, "--thread-id"),
        channel_thread_id: flag_string(flags, "--channel-thread-id"),
        trace_id: flag_string(flags, "--trace-id"),
        session_id: flag_string(flags, "--session-id"),
        evidence_id: flag_string(flags, "--artifact-id")
            .or_else(|| flag_string(flags, "--evidence-id")),
        gateway_authenticated_subject: flag_string(flags, "--gateway-authenticated-subject")
            .or_else(|| flag_string(flags, "--auth-subject")),
        auth_subject: None,
        audience: flag_string(flags, "--audience"),
        include_gates: flag_boolean(flags, "--include-gates")?,
        limit: flag_non_negative_int(flags, "--limit")?,
    })
}

fn flag_string(flags: &std::collections::HashMap<String, FlagValue>, name: &str) -> Option<String> {
    match flags.get(name) {
        Some(FlagValue::String(value)) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_owned())
            }
        }
        _ => None,
    }
}

fn flag_boolean(
    flags: &std::collections::HashMap<String, FlagValue>,
    name: &str,
) -> Result<Option<bool>> {
    match flags.get(name) {
        None => Ok(None),
        Some(FlagValue::Bool(value)) => Ok(Some(*value)),
        Some(FlagValue::String(value)) => match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" => Ok(Some(true)),
            "0" | "false" | "no" => Ok(Some(false)),
            _ => bail!(
                "{}",
                maestro_local_host::localization::cli_locale()
                    .format("{0} must be true or false", &[(name).to_string()])
            ),
        },
    }
}

fn flag_non_negative_int(
    flags: &std::collections::HashMap<String, FlagValue>,
    name: &str,
) -> Result<Option<u32>> {
    let Some(value) = flag_string(flags, name) else {
        return Ok(None);
    };
    let parsed = value
        .parse::<u32>()
        .ok()
        .filter(|parsed| parsed.to_string() == value);
    match parsed {
        Some(value) => Ok(Some(value)),
        None => bail!(
            "{}",
            maestro_local_host::localization::cli_locale()
                .format("{0} must be a non-negative integer", &[(name).to_string()])
        ),
    }
}

fn operating_plane_help_text() -> &'static str {
    maestro_local_host::localization::cli_locale().translate("Usage: maestro operating-plane status [filters]\n\nFilters:\n--thread-id <id>                  Slack/channel thread id\n--artifact-id <id>                Runtime artifact ref id\n--auth-subject <subject>          Gateway-authenticated subject\n--trace-id <id>                   Trace id\n--session-id <id>                 Maestro/session id\n--run-id <id>                     Agent runtime run id\n--workspace-id <id>               Workspace id\n--audience <audience>             agent, channel, audit, system, ...\n--include-gates=<true|false>      Include release/replay gates\n--limit <n>                       Maximum runs\n--json                            Emit safe summary JSON")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operating_plane_client::{
        OperatingPlaneEvidence, OperatingPlaneIdentity, OperatingPlaneRuntimeSignals,
        OperatingPlaneWorkItem,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn s(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn parses_status_lookups_without_swallowing_operator_filters() {
        let parsed = parse_operating_plane_args(&s(&[
            "status",
            "--thread-id",
            "C123:1740000000.000100",
            "--artifact-id=gateway:req_123",
            "--auth-subject",
            "user:alice",
            "--audience",
            "audit",
            "--include-gates=false",
            "--limit",
            "5",
        ]))
        .unwrap();

        assert_eq!(parsed.subcommand, "status");
        assert!(!parsed.json);
        assert_eq!(
            parsed.query.thread_id.as_deref(),
            Some("C123:1740000000.000100")
        );
        assert_eq!(parsed.query.evidence_id.as_deref(), Some("gateway:req_123"));
        assert_eq!(
            parsed.query.gateway_authenticated_subject.as_deref(),
            Some("user:alice")
        );
        assert_eq!(parsed.query.audience.as_deref(), Some("audit"));
        assert_eq!(parsed.query.include_gates, Some(false));
        assert_eq!(parsed.query.limit, Some(5));
    }

    #[test]
    fn defaults_subcommand_to_status_and_supports_help_json() {
        let parsed = parse_operating_plane_args(&s(&["--json", "--help"])).unwrap();
        assert_eq!(parsed.subcommand, "status");
        assert!(parsed.json);
        assert!(parsed.help);

        let inspect = parse_operating_plane_args(&s(&["inspect", "--run-id", "run_1"])).unwrap();
        assert_eq!(inspect.subcommand, "inspect");
        assert_eq!(inspect.query.run_id.as_deref(), Some("run_1"));
    }

    #[test]
    fn rejects_unknown_options_and_invalid_limit() {
        assert!(
            parse_operating_plane_args(&s(&["status", "--nope"]))
                .unwrap_err()
                .to_string()
                .contains("Unknown operating-plane option")
        );
        assert!(
            parse_operating_plane_args(&s(&["status", "--limit", "-1"]))
                .unwrap_err()
                .to_string()
                .contains("non-negative integer")
        );
        assert!(
            parse_operating_plane_args(&s(&["status", "--thread-id"]))
                .unwrap_err()
                .to_string()
                .contains("requires a value")
        );
    }

    fn sample_inspection() -> OperatingPlaneInspection {
        OperatingPlaneInspection {
            contract_version: "agent-operating-plane.v1".to_owned(),
            generated_at: "2026-05-17T06:25:00Z".to_owned(),
            unavailable_sources: None,
            runs: vec![crate::operating_plane_client::OperatingPlaneRun {
                agent_run_id: "run_1".to_owned(),
                agent_run_step_id: None,
                title: "Slack answer".to_owned(),
                status: "succeeded".to_owned(),
                surface: "slack".to_owned(),
                channel_thread_id: Some("C123:1740000000.000100".to_owned()),
                trace_id: Some("trace-1".to_owned()),
                identity: Some(OperatingPlaneIdentity {
                    workspace_id: Some("ws_evalops".to_owned()),
                    tenant_id: None,
                    actor_id: None,
                    principal_id: None,
                    agent_id: None,
                    gateway_authenticated_subject: Some("user:alice".to_owned()),
                    gateway_authenticated_user_subject: None,
                    gateway_authenticated_service: None,
                }),
                redaction_count: None,
                withholding_reasons: Some(vec!["customer_content".to_owned()]),
                unavailable_sources: None,
                evidence_refs: Some(vec![OperatingPlaneEvidence {
                    id: "gateway:req_123".to_owned(),
                    source: "llm_gateway".to_owned(),
                    kind: "model_event".to_owned(),
                    uri: None,
                    revision: Some("rev_1".to_owned()),
                    available: true,
                    summary: Some("SECRET artifact summary".to_owned()),
                }]),
                work_items: Some(vec![OperatingPlaneWorkItem {
                    kind: Some("followup".to_owned()),
                    state: Some("waiting".to_owned()),
                    next_action: Some("Post allowed artifact revision to operator".to_owned()),
                    blocker: Some("approval pending".to_owned()),
                }]),
                usage: None,
                runtime_signals: Some(OperatingPlaneRuntimeSignals {
                    operation_id: Some("run_1".to_owned()),
                    operator_summary: Some("Gateway request is tied to Slack thread".to_owned()),
                    identity_bound: Some(true),
                    model_observed: Some(true),
                    tool_observed: Some(false),
                    approval_observed: Some(false),
                    trace_linked: Some(true),
                    evidence_linked: Some(true),
                    cost_attributed: Some(true),
                    missing_signals: None,
                }),
                canonical_attributes: Some(serde_json::json!({
                    "raw_prompt": "SECRET customer prompt"
                })),
            }],
        }
    }

    #[test]
    fn renders_content_free_status_and_json() {
        let text = render_from_inspection(&sample_inspection(), false);
        assert!(text.contains("Agent operating-plane status"));
        assert!(text.contains("Identity: user:alice"));
        assert!(text.contains(
            "Artifacts: gateway:req_123 (llm_gateway/model_event, available, revision rev_1)"
        ));
        assert!(text.contains("Next action: Post allowed artifact revision to operator"));
        assert!(text.contains("Withheld/out of scope: customer_content"));
        assert!(!text.contains("SECRET customer prompt"));
        assert!(!text.contains("SECRET artifact summary"));

        let json = render_from_inspection(&sample_inspection(), true);
        let report: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(report["runs"][0]["runId"], "run_1");
        assert_eq!(
            report["runs"][0]["artifactRefs"][0]["id"],
            "gateway:req_123"
        );
        assert!(!json.contains("SECRET customer prompt"));
        assert!(!json.contains("SECRET artifact summary"));
    }

    #[tokio::test]
    async fn inspect_retries_and_sends_platform_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buffer = vec![0_u8; 8192];
                let read = stream.read(&mut buffer).await.unwrap();
                let request = String::from_utf8_lossy(&buffer[..read]);
                assert!(request.contains("GET /v1/agent-operating-plane/runs?"));
                assert!(
                    request.contains("thread_id=C123%3A1740000000.000100")
                        || request.contains("thread_id=C123:1740000000.000100")
                );
                assert!(
                    request
                        .to_ascii_lowercase()
                        .contains("authorization: bearer plane-token")
                );
                assert!(
                    request.contains("X-Organization-ID: org_plane")
                        || request.contains("x-organization-id: org_plane")
                );
                let response = if attempt == 0 {
                    "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 11\r\nRetry-After-Ms: 1\r\nConnection: close\r\n\r\nunavailable"
                        .to_owned()
                } else {
                    let body = serde_json::to_string(&sample_inspection()).unwrap();
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });

        let config = crate::operating_plane_client::PlatformServiceConfig {
            base_url: format!("http://{address}"),
            token: Some("plane-token".to_owned()),
            organization_id: Some("org_plane".to_owned()),
            workspace_id: Some("ws_plane".to_owned()),
            timeout_ms: 2_000,
            max_attempts: 2,
        };
        let inspection = inspect_operating_plane_runs(
            &OperatingPlaneRunQuery {
                thread_id: Some("C123:1740000000.000100".to_owned()),
                ..OperatingPlaneRunQuery::default()
            },
            Some(config),
        )
        .await
        .unwrap();
        server.await.unwrap();
        assert_eq!(inspection.runs[0].agent_run_id, "run_1");
        let output = render_from_inspection(&inspection, false);
        assert!(output.contains("Identity: user:alice"));
        assert!(!output.contains("SECRET"));
    }

    #[tokio::test]
    async fn help_exits_successfully_without_service_config() {
        let code = run_operating_plane(&s(&["help"])).await.unwrap();
        assert_eq!(code, 0);
        let code = run_operating_plane(&s(&["--help"])).await.unwrap();
        assert_eq!(code, 0);
    }

    #[tokio::test]
    async fn unknown_subcommand_returns_nonzero() {
        let code = run_operating_plane(&s(&["wat"])).await.unwrap();
        assert_eq!(code, 1);
    }
}
