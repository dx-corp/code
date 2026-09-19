//! Native `maestro value` customer-value report (MVP + A2A multi-agent).
//!
//! Builds a useful local report from [`SessionManager`] session stats and the
//! A2A task ledger (`~/.maestro/a2a/tasks.json`). Residual gaps vs the full
//! TypeScript report are listed in [`RESIDUAL_GAPS`] and emitted in JSON under
//! `residualGaps`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, bail};
use chrono::{Local, TimeZone, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::a2a_cli::{
    TaskLedgerEntry, get_task_ledger_path, is_action_required_state, is_completed_state,
    is_failed_state, is_final_state, load_task_ledger,
};
use crate::session::{SessionInfo, SessionManager};

const CUSTOMER_VALUE_MANIFEST_VERSION: &str = "maestro.customer-value.manifest.v1";
const NATIVE_REPORT_SCHEMA: &str = "maestro.customer-value.native.v1";
const HOURLY_VALUE_USD: f64 = 150.0;
const SESSION_SCAN_LIMIT: usize = 500;
const RECENT_TASK_LIMIT: usize = 5;
const TOP_PEER_LIMIT: usize = 5;

/// Documented residual gaps versus the full TypeScript customer-value report.
pub const RESIDUAL_GAPS: &[&str] = &[
    "Ambient learner outcomes and automation / playbook learning opportunities",
    "Mission store + agent work board projections (missions, todos, GitHub tasks)",
    "Telemetry log rollups (tool-execution, evaluation, canonical-turn, policy-approval events)",
    "Durable handoff status derived from todo-store open work and unfinished goals",
    "Admin control surface inventory and collection-gap diagnostics beyond local sessions",
    "Usage DB (usage.json) cross-join with sessions; native report uses session stats only",
    "Memory extraction provenance hashing and memory-backed session rollups",
    "A2A cockpit next-action severity routing (native multi-agent uses lightweight heuristics)",
];

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ValueRange {
    label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    since: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    until: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TrustCard {
    session_id: String,
    title: String,
    model: String,
    cwd: String,
    message_count: usize,
    assistant_turn_count: usize,
    tool_call_count: usize,
    usage: TrustCardUsage,
    summary: Option<String>,
    evidence: TrustCardEvidence,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TrustCardUsage {
    requests: usize,
    tokens: u64,
    cost_usd: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TrustCardEvidence {
    session_path: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MultiAgentPeer {
    peer: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
    task_count: usize,
    completed_task_count: usize,
    failed_task_count: usize,
    action_required_task_count: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MultiAgentRecentTask {
    id: String,
    peer: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    peer_display_name: Option<String>,
    state: String,
    status: String,
    text: String,
    updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    completed_at: Option<String>,
    work_graph: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    work_graph_summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_text: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MultiAgentNextAction {
    id: String,
    label: String,
    command: String,
    severity: String,
    peer: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_id: Option<String>,
    reason: String,
}

/// A2A multi-agent coordination slice (TS `MultiAgentValue` parity core).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MultiAgentValue {
    tasks_path: String,
    task_count: usize,
    delegated_task_count: usize,
    peer_count: usize,
    completed_task_count: usize,
    failed_task_count: usize,
    action_required_task_count: usize,
    work_graph_task_count: usize,
    work_graph_child_run_count: usize,
    work_graph_blocked_item_count: usize,
    work_graph_waiting_item_count: usize,
    work_graph_pending_tool_call_count: usize,
    codex_subagent_edge_count: usize,
    transcript_message_count: usize,
    realized_hours_saved: f64,
    realized_value_usd: f64,
    pending_task_count: usize,
    audit_ready_task_count: usize,
    evidence_gap_count: usize,
    delegated_failed_task_count: usize,
    delegated_pending_task_count: usize,
    delegated_evidence_gap_count: usize,
    next_actions: Vec<MultiAgentNextAction>,
    top_peers: Vec<MultiAgentPeer>,
    recent_tasks: Vec<MultiAgentRecentTask>,
    collection_gaps: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ValueSummary {
    session_count: usize,
    trust_card_count: usize,
    message_count: usize,
    assistant_turn_count: usize,
    tool_call_count: usize,
    total_tokens: u64,
    total_cost_usd: f64,
    estimated_hours_saved: f64,
    estimated_value_usd: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    value_multiple: Option<f64>,
    multi_agent_estimated_hours_saved: f64,
    multi_agent_estimated_value_usd: f64,
    multi_agent_task_count: usize,
    multi_agent_peer_count: usize,
    multi_agent_work_graph_task_count: usize,
    multi_agent_child_run_count: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ValueReport {
    schema_version: &'static str,
    generated_at: String,
    range: ValueRange,
    sources: ValueSources,
    summary: ValueSummary,
    trust_cards: Vec<TrustCard>,
    multi_agent: MultiAgentValue,
    residual_gaps: Vec<&'static str>,
    notes: Vec<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ValueSources {
    session_dir: String,
    workspace_dir: String,
    a2a_tasks_path: String,
    implementation: &'static str,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ArtifactManifest {
    protocol_version: &'static str,
    generated_at: String,
    range: ValueRange,
    artifacts: ArtifactPaths,
    hashes: ArtifactHashes,
    sources: ValueSources,
    summary: ValueSummary,
    residual_gaps: Vec<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ArtifactPaths {
    report_json_path: String,
    report_markdown_path: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ArtifactHashes {
    report_json_sha256: String,
    report_markdown_sha256: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ArtifactWriteResult {
    output_dir: String,
    report_json_path: String,
    report_markdown_path: String,
    manifest_path: String,
    report_json_sha256: String,
    report_markdown_sha256: String,
    manifest_sha256: String,
    manifest: ArtifactManifest,
}

#[derive(Debug, Default)]
struct ValueArgs {
    period: Option<String>,
    format: String,
    write: bool,
    output_dir: Option<PathBuf>,
    session_dir: Option<PathBuf>,
    a2a_tasks_path: Option<String>,
}

/// Run `maestro value` with argv after the command name.
pub async fn run_value(args: &[String]) -> Result<i32> {
    let parsed = parse_args(args)?;
    if parsed.format == "help" {
        println!("{}", value_help());
        return Ok(0);
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let report = build_report(&cwd, &parsed)?;

    let artifacts = if parsed.write {
        Some(write_artifacts(&report, parsed.output_dir.as_deref())?)
    } else {
        None
    };

    match parsed.format.as_str() {
        "json" => {
            if let Some(artifacts) = artifacts {
                let payload = serde_json::json!({
                    "report": report,
                    "artifacts": artifacts,
                });
                println!("{}", serde_json::to_string_pretty(&payload)?);
            } else {
                println!("{}", serde_json::to_string_pretty(&report)?);
            }
        }
        "md" | "markdown" => {
            println!("{}", format_markdown(&report));
            if let Some(artifacts) = &artifacts {
                println!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "\nSaved value artifact manifest: {0}",
                        std::slice::from_ref(&(artifacts.manifest_path))
                    )
                );
            }
        }
        _ => {
            println!("{}", format_text(&report));
            if let Some(artifacts) = &artifacts {
                println!();
                println!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "Saved value artifacts: {0}",
                        std::slice::from_ref(&(artifacts.output_dir))
                    )
                );
                println!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "Manifest: {0}",
                        std::slice::from_ref(&(artifacts.manifest_path))
                    )
                );
                println!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "Report JSON: {0}",
                        std::slice::from_ref(&(artifacts.report_json_path))
                    )
                );
                println!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "Report Markdown: {0}",
                        std::slice::from_ref(&(artifacts.report_markdown_path))
                    )
                );
            }
        }
    }

    Ok(0)
}

fn value_help() -> &'static str {
    crate::localization::cli_locale().translate("Usage: maestro value [today|yesterday|week|7d|month|30d|all] [options]\n\nOptions:\n--format json|md|text   Output format (default: text)\n--write                 Persist report JSON, Markdown, and manifest\n--output-dir <path>     Artifact directory (default: ~/.maestro/value-reports)\n--session-dir <path>    Override session root (lists .jsonl files in this directory)\n--a2a-tasks <path>      Override A2A task ledger path (default: ~/.maestro/a2a/tasks.json)\n--help                  Show this help\n\nNative report uses SessionManager session stats plus the A2A task ledger.\nResidual gaps vs the full TypeScript report are listed in JSON residualGaps.")
}

fn parse_args(args: &[String]) -> Result<ValueArgs> {
    let mut out = ValueArgs {
        format: "text".to_string(),
        ..ValueArgs::default()
    };
    let mut i = 0usize;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "help" | "--help" | "-h" => {
                out.format = "help".to_string();
                return Ok(out);
            }
            "--write" => out.write = true,
            "--format" | "-f" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    bail!(
                        "{}",
                        crate::localization::cli_locale()
                            .format("--format requires a value (json|md|text)", &[])
                    );
                };
                out.format = normalize_format(value)?;
            }
            s if s.starts_with("--format=") => {
                out.format = normalize_format(s.trim_start_matches("--format="))?;
            }
            "--output-dir" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    bail!(
                        "{}",
                        crate::localization::cli_locale()
                            .format("--output-dir requires a path", &[])
                    );
                };
                out.output_dir = Some(PathBuf::from(value));
            }
            s if s.starts_with("--output-dir=") => {
                out.output_dir = Some(PathBuf::from(s.trim_start_matches("--output-dir=")));
            }
            "--session-dir" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    bail!(
                        "{}",
                        crate::localization::cli_locale()
                            .format("--session-dir requires a path", &[])
                    );
                };
                out.session_dir = Some(PathBuf::from(value));
            }
            s if s.starts_with("--session-dir=") => {
                out.session_dir = Some(PathBuf::from(s.trim_start_matches("--session-dir=")));
            }
            "--a2a-tasks" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    bail!(
                        "{}",
                        crate::localization::cli_locale()
                            .format("--a2a-tasks requires a path", &[])
                    );
                };
                out.a2a_tasks_path = Some(value.clone());
            }
            s if s.starts_with("--a2a-tasks=") => {
                out.a2a_tasks_path = Some(s.trim_start_matches("--a2a-tasks=").to_string());
            }
            "today" | "yesterday" | "week" | "7d" | "month" | "30d" | "all" => {
                out.period = Some(a.to_string());
            }
            other if other.starts_with('-') => {
                bail!(
                    "{}",
                    crate::localization::cli_locale()
                        .format("unknown value flag: {0}", &[(other).to_string()])
                );
            }
            other => {
                bail!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "unknown value period: {0} (use today|yesterday|week|7d|month|30d|all)",
                        &[(other).to_string()]
                    )
                );
            }
        }
        i += 1;
    }
    Ok(out)
}

fn normalize_format(value: &str) -> Result<String> {
    match value.to_ascii_lowercase().as_str() {
        "json" => Ok("json".to_string()),
        "md" | "markdown" => Ok("md".to_string()),
        "text" | "txt" | "plain" => Ok("text".to_string()),
        other => bail!(
            "{}",
            crate::localization::cli_locale().format(
                "unsupported --format: {0} (use json|md|text)",
                &[(other).to_string()]
            )
        ),
    }
}

fn resolve_range(period: Option<&str>, now_ms: i64) -> ValueRange {
    let day = 86_400_000i64;
    match period.unwrap_or("30d") {
        "today" => {
            let local = Local
                .timestamp_millis_opt(now_ms)
                .single()
                .unwrap_or_else(Local::now);
            let midnight = local
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .and_then(|dt| Local.from_local_datetime(&dt).single())
                .map(|dt| dt.timestamp_millis())
                .unwrap_or(now_ms);
            ValueRange {
                label: "Today".to_string(),
                since: Some(midnight),
                until: None,
            }
        }
        "yesterday" => {
            let local = Local
                .timestamp_millis_opt(now_ms)
                .single()
                .unwrap_or_else(Local::now);
            let today_midnight = local
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .and_then(|dt| Local.from_local_datetime(&dt).single())
                .map(|dt| dt.timestamp_millis())
                .unwrap_or(now_ms);
            ValueRange {
                label: "Yesterday".to_string(),
                since: Some(today_midnight - day),
                until: Some(today_midnight),
            }
        }
        "week" | "7d" => ValueRange {
            label: "Last 7 Days".to_string(),
            since: Some(now_ms - 7 * day),
            until: None,
        },
        "all" => ValueRange {
            label: "All Time".to_string(),
            since: None,
            until: None,
        },
        "month" | "30d" => ValueRange {
            label: "Last 30 Days".to_string(),
            since: Some(now_ms - 30 * day),
            until: None,
        },
        _ => ValueRange {
            label: "Last 30 Days".to_string(),
            since: Some(now_ms - 30 * day),
            until: None,
        },
    }
}

fn build_report(cwd: &Path, args: &ValueArgs) -> Result<ValueReport> {
    let now_ms = Utc::now().timestamp_millis();
    let range = resolve_range(args.period.as_deref(), now_ms);

    let manager = if let Some(session_dir) = args.session_dir.as_ref() {
        SessionManager::with_sessions_dir(cwd.to_string_lossy().to_string(), session_dir.clone())
    } else {
        SessionManager::new(cwd.to_string_lossy().to_string())
    };
    let session_dir = manager.sessions_dir().to_path_buf();

    let sessions = manager
        .recent_sessions(SESSION_SCAN_LIMIT)
        .context("Failed to load sessions for value report")?;

    let mut trust_cards: Vec<TrustCard> = sessions
        .into_iter()
        .filter(|s| session_in_range(s, &range))
        .map(session_to_trust_card)
        .collect();
    trust_cards.sort_by(|a, b| {
        b.usage
            .cost_usd
            .partial_cmp(&a.usage.cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.message_count.cmp(&a.message_count))
    });

    let message_count = trust_cards.iter().map(|c| c.message_count).sum();
    let assistant_turn_count = trust_cards.iter().map(|c| c.assistant_turn_count).sum();
    let tool_call_count = trust_cards.iter().map(|c| c.tool_call_count).sum();
    let total_tokens = trust_cards.iter().map(|c| c.usage.tokens).sum();
    let total_cost_usd: f64 = trust_cards.iter().map(|c| c.usage.cost_usd).sum();
    let session_hours_saved = estimate_hours_saved(assistant_turn_count, tool_call_count);

    let multi_agent = summarize_multi_agent(args.a2a_tasks_path.as_deref(), &range);
    let multi_agent_hours = multi_agent.realized_hours_saved;
    let estimated_hours_saved = session_hours_saved + multi_agent_hours;
    let estimated_value_usd = estimated_hours_saved * HOURLY_VALUE_USD;
    let value_multiple = if total_cost_usd > 0.0 {
        Some(estimated_value_usd / total_cost_usd)
    } else {
        None
    };

    let notes = vec![
        "Native report: session trust cards from SessionManager plus A2A multi-agent ledger rollups.",
        "Session hours use assistant_turn*0.08 + tool_call*0.03; multi-agent completed delegations use 0.25h (+0.1h with workGraph); value uses $150/hr.",
        "Estimated hours/value in summary include multi-agent realized hours (TS parity).",
    ];

    Ok(ValueReport {
        schema_version: NATIVE_REPORT_SCHEMA,
        generated_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        range,
        sources: ValueSources {
            session_dir: session_dir.display().to_string(),
            workspace_dir: cwd.display().to_string(),
            a2a_tasks_path: multi_agent.tasks_path.clone(),
            implementation: "rust-native-session-a2a-mvp",
        },
        summary: ValueSummary {
            session_count: trust_cards.len(),
            trust_card_count: trust_cards.len(),
            message_count,
            assistant_turn_count,
            tool_call_count,
            total_tokens,
            total_cost_usd,
            estimated_hours_saved,
            estimated_value_usd,
            value_multiple,
            multi_agent_estimated_hours_saved: multi_agent_hours,
            multi_agent_estimated_value_usd: multi_agent_hours * HOURLY_VALUE_USD,
            multi_agent_task_count: multi_agent.task_count,
            multi_agent_peer_count: multi_agent.peer_count,
            multi_agent_work_graph_task_count: multi_agent.work_graph_task_count,
            multi_agent_child_run_count: multi_agent.work_graph_child_run_count,
        },
        trust_cards,
        multi_agent,
        residual_gaps: RESIDUAL_GAPS.to_vec(),
        notes,
    })
}

fn summarize_multi_agent(tasks_path_override: Option<&str>, range: &ValueRange) -> MultiAgentValue {
    let tasks_path = get_task_ledger_path(tasks_path_override)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| {
            dirs::home_dir()
                .map(|h| h.join(".maestro/a2a/tasks.json").display().to_string())
                .unwrap_or_else(|| "~/.maestro/a2a/tasks.json".to_string())
        });

    let mut collection_gaps = Vec::new();
    let tasks: Vec<TaskLedgerEntry> = match load_task_ledger(tasks_path_override) {
        Ok(ledger) => ledger
            .tasks
            .into_iter()
            .filter(|task| a2a_task_in_range(task, range))
            .collect(),
        Err(error) => {
            collection_gaps.push(format!("A2A task ledger could not be read: {error:#}."));
            Vec::new()
        }
    };

    if tasks.is_empty() && collection_gaps.is_empty() {
        collection_gaps.push(if range.since.is_none() && range.until.is_none() {
            format!("No A2A delegated task evidence found in {tasks_path}.")
        } else {
            format!("No A2A delegated task evidence found in {tasks_path} for the selected range.")
        });
    }

    let rollup = summarize_task_rollup(&tasks);
    let delegated: Vec<&TaskLedgerEntry> =
        tasks.iter().filter(|t| t.kind == "delegation").collect();
    let delegated_rollup = summarize_task_rollup_refs(&delegated);

    let mut work_graph_child_run_count = 0usize;
    let mut work_graph_blocked_item_count = 0usize;
    let mut work_graph_waiting_item_count = 0usize;
    let mut work_graph_pending_tool_call_count = 0usize;
    let mut codex_subagent_edge_count = 0usize;
    for task in &tasks {
        if let Some(graph) = task.work_graph.as_ref() {
            work_graph_child_run_count += work_graph_child_runs(graph);
            work_graph_blocked_item_count += json_u64(graph, "blockedItemCount") as usize;
            work_graph_waiting_item_count += json_u64(graph, "waitingItemCount") as usize;
            work_graph_pending_tool_call_count += json_u64(graph, "pendingToolCallCount") as usize;
            codex_subagent_edge_count += codex_subagent_edges(graph);
        }
    }

    let completed_delegations: Vec<&TaskLedgerEntry> = delegated
        .iter()
        .copied()
        .filter(|task| is_audit_ready_delegation(task))
        .collect();
    let realized_hours_saved = {
        let base = completed_delegations.len() as f64 * 0.25;
        let with_graph = completed_delegations
            .iter()
            .filter(|t| t.work_graph.is_some())
            .count() as f64
            * 0.1;
        ((base + with_graph) * 100.0).round() / 100.0
    };

    let peer_count = tasks
        .iter()
        .map(|t| t.peer.as_str())
        .collect::<HashSet<_>>()
        .len();

    MultiAgentValue {
        tasks_path,
        task_count: rollup.task_count,
        delegated_task_count: rollup.delegated_task_count,
        peer_count,
        completed_task_count: rollup.completed_task_count,
        failed_task_count: rollup.failed_task_count,
        action_required_task_count: rollup.action_required_task_count,
        work_graph_task_count: rollup.work_graph_task_count,
        work_graph_child_run_count,
        work_graph_blocked_item_count,
        work_graph_waiting_item_count,
        work_graph_pending_tool_call_count,
        codex_subagent_edge_count,
        transcript_message_count: rollup.transcript_message_count,
        realized_hours_saved,
        realized_value_usd: realized_hours_saved * HOURLY_VALUE_USD,
        pending_task_count: rollup.action_required_task_count + rollup.running_task_count,
        audit_ready_task_count: rollup.audit_ready_task_count,
        evidence_gap_count: rollup.evidence_gap_count,
        delegated_failed_task_count: delegated_rollup.failed_task_count,
        delegated_pending_task_count: delegated_rollup.action_required_task_count
            + delegated_rollup.running_task_count,
        delegated_evidence_gap_count: delegated_rollup.evidence_gap_count,
        next_actions: summarize_next_actions(&delegated),
        top_peers: summarize_top_peers(&tasks),
        recent_tasks: summarize_recent_tasks(&tasks),
        collection_gaps,
    }
}

#[derive(Default)]
struct TaskRollup {
    task_count: usize,
    delegated_task_count: usize,
    completed_task_count: usize,
    failed_task_count: usize,
    action_required_task_count: usize,
    running_task_count: usize,
    work_graph_task_count: usize,
    transcript_message_count: usize,
    audit_ready_task_count: usize,
    evidence_gap_count: usize,
}

fn summarize_task_rollup(tasks: &[TaskLedgerEntry]) -> TaskRollup {
    summarize_task_rollup_refs(&tasks.iter().collect::<Vec<_>>())
}

fn summarize_task_rollup_refs(tasks: &[&TaskLedgerEntry]) -> TaskRollup {
    let mut rollup = TaskRollup {
        task_count: tasks.len(),
        ..TaskRollup::default()
    };
    for task in tasks {
        if task.kind == "delegation" {
            rollup.delegated_task_count += 1;
        }
        if is_completed_state(&task.state) {
            rollup.completed_task_count += 1;
        } else if is_failed_state(&task.state) {
            rollup.failed_task_count += 1;
        } else if is_action_required_state(&task.state) {
            rollup.action_required_task_count += 1;
        } else {
            rollup.running_task_count += 1;
        }
        if task.work_graph.is_some() {
            rollup.work_graph_task_count += 1;
        }
        rollup.transcript_message_count += task.transcript.len();
        let gaps = a2a_task_evidence_gaps(task);
        rollup.evidence_gap_count += gaps;
        if task.kind == "delegation" && gaps == 0 {
            rollup.audit_ready_task_count += 1;
        }
    }
    rollup
}

fn a2a_task_evidence_gaps(task: &TaskLedgerEntry) -> usize {
    let mut gaps = 0usize;
    if !is_completed_state(&task.state) {
        gaps += 1;
    }
    if task.work_graph.is_none() {
        gaps += 1;
    }
    let has_response = task
        .response_text
        .as_deref()
        .map(str::trim)
        .as_ref()
        .is_some_and(|s| !s.is_empty());
    let has_agent_transcript = task
        .transcript
        .iter()
        .any(|entry| entry.role.eq_ignore_ascii_case("agent") && !entry.text.trim().is_empty());
    if !has_response && !has_agent_transcript {
        gaps += 1;
    }
    let mut roles = HashSet::new();
    for entry in &task.transcript {
        roles.insert(entry.role.to_ascii_lowercase());
    }
    if !roles.contains("user") || !roles.contains("agent") {
        gaps += 1;
    }
    gaps
}

fn is_audit_ready_delegation(task: &TaskLedgerEntry) -> bool {
    task.kind == "delegation" && a2a_task_evidence_gaps(task) == 0
}

fn a2a_task_status(state: &str) -> &'static str {
    if is_action_required_state(state) {
        "waiting"
    } else if is_completed_state(state) {
        "completed"
    } else if is_failed_state(state) {
        "failed"
    } else if !is_final_state(state) {
        "running"
    } else {
        "unknown"
    }
}

fn a2a_task_in_range(task: &TaskLedgerEntry, range: &ValueRange) -> bool {
    let status = a2a_task_status(&task.state);
    if matches!(status, "failed" | "waiting" | "running") {
        return true;
    }
    if range.since.is_none() && range.until.is_none() {
        return true;
    }
    let Some((start, end)) = a2a_task_active_range(task) else {
        return false;
    };
    if let Some(until) = range.until {
        if start >= until {
            return false;
        }
    }
    if let Some(since) = range.since {
        if end < since {
            return false;
        }
    }
    true
}

fn a2a_task_active_range(task: &TaskLedgerEntry) -> Option<(i64, i64)> {
    let mut timestamps = Vec::new();
    if let Some(ms) = parse_timestamp_ms(&task.created_at) {
        timestamps.push(ms);
    }
    if let Some(ms) = parse_timestamp_ms(&task.updated_at) {
        timestamps.push(ms);
    }
    if let Some(completed) = &task.completed_at {
        if let Some(ms) = parse_timestamp_ms(completed) {
            timestamps.push(ms);
        }
    }
    if timestamps.is_empty() {
        return None;
    }
    let start = *timestamps.iter().min().unwrap();
    if !is_final_state(&task.state) {
        return Some((start, i64::MAX));
    }
    let end = *timestamps.iter().max().unwrap();
    Some((start, end))
}

fn task_sort_timestamp(task: &TaskLedgerEntry) -> i64 {
    task.completed_at
        .as_deref()
        .and_then(parse_timestamp_ms)
        .or_else(|| parse_timestamp_ms(&task.updated_at))
        .or_else(|| parse_timestamp_ms(&task.created_at))
        .unwrap_or(0)
}

fn parse_timestamp_ms(value: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

fn summarize_top_peers(tasks: &[TaskLedgerEntry]) -> Vec<MultiAgentPeer> {
    let mut peers: HashMap<String, MultiAgentPeer> = HashMap::new();
    for task in tasks {
        let entry = peers
            .entry(task.peer.clone())
            .or_insert_with(|| MultiAgentPeer {
                peer: truncate_label(&task.peer, 80),
                display_name: task
                    .peer_display_name
                    .as_ref()
                    .map(|name| truncate_label(name, 100)),
                task_count: 0,
                completed_task_count: 0,
                failed_task_count: 0,
                action_required_task_count: 0,
            });
        if entry.display_name.is_none() {
            entry.display_name = task
                .peer_display_name
                .as_ref()
                .map(|name| truncate_label(name, 100));
        }
        entry.task_count += 1;
        match a2a_task_status(&task.state) {
            "completed" => entry.completed_task_count += 1,
            "failed" => entry.failed_task_count += 1,
            "waiting" => entry.action_required_task_count += 1,
            _ => {}
        }
    }
    let mut list: Vec<_> = peers.into_values().collect();
    list.sort_by(|a, b| {
        b.task_count
            .cmp(&a.task_count)
            .then_with(|| a.peer.cmp(&b.peer))
    });
    list.truncate(TOP_PEER_LIMIT);
    list
}

fn summarize_recent_tasks(tasks: &[TaskLedgerEntry]) -> Vec<MultiAgentRecentTask> {
    let mut ordered: Vec<&TaskLedgerEntry> = tasks.iter().collect();
    ordered.sort_by_key(|b| std::cmp::Reverse(task_sort_timestamp(b)));
    ordered
        .into_iter()
        .take(RECENT_TASK_LIMIT)
        .map(|task| MultiAgentRecentTask {
            id: truncate_label(&task.task_id, 120),
            peer: truncate_label(&task.peer, 80),
            peer_display_name: task
                .peer_display_name
                .as_ref()
                .map(|name| truncate_label(name, 100)),
            state: truncate_label(&task.state, 80),
            status: a2a_task_status(&task.state).to_string(),
            text: truncate_label(&task.text, 140),
            updated_at: task.updated_at.clone(),
            completed_at: task.completed_at.clone(),
            work_graph: task.work_graph.is_some(),
            work_graph_summary: task
                .work_graph
                .as_ref()
                .and_then(format_work_graph_summary)
                .map(|s| truncate_label(&s, 320)),
            response_text: task
                .response_text
                .as_ref()
                .map(|text| truncate_label(text, 180)),
        })
        .collect()
}

fn summarize_next_actions(delegated: &[&TaskLedgerEntry]) -> Vec<MultiAgentNextAction> {
    let mut actions = Vec::new();
    for task in delegated.iter().take(5) {
        let status = a2a_task_status(&task.state);
        let (label, command, severity, reason) = match status {
            "waiting" => (
                format!("Reply to delegated task on {}", task.peer),
                format!(
                    "deixic-code a2a reply {} {} 'RESPONSE_TEXT' --wait --work-graph",
                    shell_quote(&task.peer),
                    shell_quote(&task.task_id)
                ),
                "high",
                "Task is waiting on input before multi-agent value can complete.".to_string(),
            ),
            "failed" => (
                format!("Inspect failed delegated task on {}", task.peer),
                format!(
                    "deixic-code a2a tasks --peer {} --task {}",
                    shell_quote(&task.peer),
                    shell_quote(&task.task_id)
                ),
                "high",
                "Failed delegated work should be reviewed before claiming multi-agent value."
                    .to_string(),
            ),
            "running" => (
                format!("Wait for delegated task on {}", task.peer),
                format!(
                    "deixic-code a2a wait {} {} --work-graph",
                    shell_quote(&task.peer),
                    shell_quote(&task.task_id)
                ),
                "medium",
                "Running delegated work is not yet realized value.".to_string(),
            ),
            _ => continue,
        };
        actions.push(MultiAgentNextAction {
            id: format!("{}:{}", status, truncate_label(&task.task_id, 80)),
            label: truncate_label(&label, 180),
            command: truncate_label(&command, 240),
            severity: severity.to_string(),
            peer: truncate_label(&task.peer, 80),
            task_id: Some(truncate_label(&task.task_id, 120)),
            reason: truncate_label(&reason, 240),
        });
        if actions.len() >= 5 {
            break;
        }
    }
    actions
}

fn multi_agent_decision_line(multi_agent: &MultiAgentValue) -> String {
    if let Some(action) = multi_agent.next_actions.first() {
        return format!("{} ({})", action.label, action.command);
    }
    if multi_agent.delegated_failed_task_count > 0 {
        return "Refresh or inspect failed delegated work before claiming value.".to_string();
    }
    if multi_agent.delegated_pending_task_count > 0 {
        return "Wait for running delegated work before counting realized value.".to_string();
    }
    if multi_agent.delegated_evidence_gap_count > 0 {
        return if multi_agent.audit_ready_task_count > 0 {
            "Some delegated work is audit-ready, but evidence gaps remain to close.".to_string()
        } else {
            "Completed delegated work is missing audit evidence; collect work graphs, responses, or transcripts before claiming value.".to_string()
        };
    }
    if multi_agent.realized_hours_saved <= 0.0 {
        return "No completed delegated work found; delegate and complete A2A work before claiming realized multi-agent value.".to_string();
    }
    "No action required; completed delegated work is ready for audit.".to_string()
}

fn work_graph_child_runs(graph: &serde_json::Value) -> usize {
    let declared = json_u64(graph, "childRunCount") as usize;
    let ids = graph
        .get("childRunIds")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let codex_ids = graph
        .get("codexSubagents")
        .and_then(|v| v.get("childRunIds"))
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    declared.max(ids).max(codex_ids)
}

fn codex_subagent_edges(graph: &serde_json::Value) -> usize {
    let Some(codex) = graph.get("codexSubagents") else {
        return 0;
    };
    if let Some(count) = codex.get("edgeCount").and_then(|v| v.as_u64()) {
        return count as usize;
    }
    codex
        .get("edges")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0)
}

fn format_work_graph_summary(graph: &serde_json::Value) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(state) = graph.get("state").and_then(|v| v.as_str()) {
        parts.push(state.to_string());
    }
    for (label, key) in [
        ("blocked", "blockedItemCount"),
        ("waiting", "waitingItemCount"),
        ("child runs", "childRunCount"),
        ("pending tools", "pendingToolCallCount"),
    ] {
        let count = json_u64(graph, key);
        if count > 0 {
            parts.push(format!("{count} {label}"));
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(format!("workGraph: {}", parts.join(", ")))
    }
}

fn json_u64(value: &serde_json::Value, key: &str) -> u64 {
    value
        .get(key)
        .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)))
        .unwrap_or(0)
}

fn truncate_label(value: &str, max: usize) -> String {
    let trimmed = value.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':'))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn estimate_hours_saved(assistant_turn_count: usize, tool_call_count: usize) -> f64 {
    let raw = assistant_turn_count as f64 * 0.08 + tool_call_count as f64 * 0.03;
    (raw * 100.0).round() / 100.0
}

fn session_to_trust_card(session: SessionInfo) -> TrustCard {
    let message_count = session.stats.user_messages + session.stats.assistant_messages;
    let summary = session.meta.as_ref().and_then(|m| m.summary.clone());
    TrustCard {
        session_id: session.id.clone(),
        title: session.title(),
        model: session.model.clone(),
        cwd: session.cwd.clone(),
        message_count,
        assistant_turn_count: session.stats.assistant_messages,
        tool_call_count: session.stats.tool_calls,
        usage: TrustCardUsage {
            requests: session.stats.assistant_messages,
            tokens: session
                .stats
                .total_input_tokens
                .saturating_add(session.stats.total_output_tokens),
            cost_usd: session.stats.total_cost,
        },
        summary,
        evidence: TrustCardEvidence {
            session_path: session.path.display().to_string(),
        },
    }
}

fn session_in_range(session: &SessionInfo, range: &ValueRange) -> bool {
    let Some(since) = range.since else {
        return true;
    };
    let ts = session_timestamp_ms(session);
    if ts < since {
        return false;
    }
    if let Some(until) = range.until {
        if ts >= until {
            return false;
        }
    }
    true
}

fn session_timestamp_ms(session: &SessionInfo) -> i64 {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&session.timestamp) {
        return dt.timestamp_millis();
    }
    if let Some(modified) = session.modified {
        if let Ok(duration) = modified.duration_since(UNIX_EPOCH) {
            return duration.as_millis() as i64;
        }
    }
    0
}

fn format_text(report: &ValueReport) -> String {
    let mut lines = vec![
        format!("Customer Value Report ({})", report.range.label),
        "=====================".to_string(),
        String::new(),
        format!("Generated: {}", report.generated_at),
        format!("Sessions: {}", report.summary.session_count),
        format!("Trust cards: {}", report.summary.trust_card_count),
        format!("Messages: {}", report.summary.message_count),
        format!("Tool calls: {}", report.summary.tool_call_count),
        crate::localization::cli_locale().format(
            "Usage: {0} tokens, ${1}",
            &[
                (report.summary.total_tokens).to_string(),
                format!("{:.4}", report.summary.total_cost_usd),
            ],
        ),
        {
            let multiple = report
                .summary
                .value_multiple
                .map(|m| format!(" ({m:.1}x spend)"))
                .unwrap_or_default();
            format!(
                "Estimated value: {:.1} hours / ${:.2}{multiple}",
                report.summary.estimated_hours_saved, report.summary.estimated_value_usd
            )
        },
        format!(
            "Multi-agent realized value: {:.1} hours / ${:.2}",
            report.summary.multi_agent_estimated_hours_saved,
            report.summary.multi_agent_estimated_value_usd
        ),
        format!(
            "Multi-agent tasks: {} across {} peer(s), {} with work graphs, {} child run(s)",
            report.summary.multi_agent_task_count,
            report.summary.multi_agent_peer_count,
            report.summary.multi_agent_work_graph_task_count,
            report.summary.multi_agent_child_run_count
        ),
        String::new(),
        "Top Trust Cards".to_string(),
        "---------------".to_string(),
    ];

    if report.trust_cards.is_empty() {
        lines.push("No sessions found for this range.".to_string());
        lines.push(String::new());
    } else {
        for card in report.trust_cards.iter().take(5) {
            lines.push(format!("- {} ({})", card.title, card.session_id));
            lines.push(format!(
                "  {} messages, {} tool calls, ${:.4} spend",
                card.message_count, card.tool_call_count, card.usage.cost_usd
            ));
            lines.push(format!("  Evidence: {}", card.evidence.session_path));
            if let Some(summary) = &card.summary {
                lines.push(format!("  Summary: {summary}"));
            }
        }
        lines.push(String::new());
    }

    lines.push("Multi-Agent Coordination".to_string());
    lines.push("------------------------".to_string());
    if report.multi_agent.task_count == 0 {
        if report.multi_agent.collection_gaps.is_empty() {
            lines.push("No A2A delegated tasks found for this range.".to_string());
        } else {
            for gap in &report.multi_agent.collection_gaps {
                lines.push(format!("- {gap}"));
            }
        }
        lines.push(String::new());
    } else {
        lines.push(format!(
            "Decision: {}",
            multi_agent_decision_line(&report.multi_agent)
        ));
        lines.push(format!(
            "Realized value: {:.1} hours / ${:.2} from completed delegated work",
            report.multi_agent.realized_hours_saved, report.multi_agent.realized_value_usd
        ));
        lines.push(format!(
            "Pending work: {} task(s), {} evidence gap(s), {} audit-ready task(s)",
            report.multi_agent.delegated_pending_task_count,
            report.multi_agent.delegated_evidence_gap_count,
            report.multi_agent.audit_ready_task_count
        ));
        lines.push(format!(
            "Tasks: {} total, {} completed, {} failed, {} waiting on input",
            report.multi_agent.task_count,
            report.multi_agent.completed_task_count,
            report.multi_agent.failed_task_count,
            report.multi_agent.action_required_task_count
        ));
        lines.push(format!(
            "Peers: {}; workGraph-backed tasks: {}",
            report.multi_agent.peer_count, report.multi_agent.work_graph_task_count
        ));
        lines.push(format!(
            "Work graph pressure: {} blocked item(s), {} waiting item(s), {} pending tool call(s), {} child run(s)",
            report.multi_agent.work_graph_blocked_item_count,
            report.multi_agent.work_graph_waiting_item_count,
            report.multi_agent.work_graph_pending_tool_call_count,
            report.multi_agent.work_graph_child_run_count
        ));
        lines.push(format!("Evidence: {}", report.multi_agent.tasks_path));
        if !report.multi_agent.top_peers.is_empty() {
            lines.push("Peer rollup:".to_string());
            for peer in &report.multi_agent.top_peers {
                lines.push(format!(
                    "- {}: {} task(s), {} completed, {} failed, {} waiting",
                    peer.display_name.as_deref().unwrap_or(&peer.peer),
                    peer.task_count,
                    peer.completed_task_count,
                    peer.failed_task_count,
                    peer.action_required_task_count
                ));
            }
        }
        if !report.multi_agent.next_actions.is_empty() {
            lines.push("Next actions:".to_string());
            for action in &report.multi_agent.next_actions {
                lines.push(format!("- [{}] {}", action.severity, action.label));
                lines.push(format!("  Command: {}", action.command));
                lines.push(format!("  Reason: {}", action.reason));
            }
        }
        for task in &report.multi_agent.recent_tasks {
            lines.push(format!(
                "- {}: {} ({})",
                task.peer_display_name.as_deref().unwrap_or(&task.peer),
                task.status,
                task.state
            ));
            lines.push(format!("  {}", task.text));
            if let Some(response) = &task.response_text {
                lines.push(format!("  Response: {response}"));
            }
            if let Some(summary) = &task.work_graph_summary {
                lines.push(format!("  {summary}"));
            }
        }
        lines.push(String::new());
    }

    lines.push("Residual Gaps (native MVP)".to_string());
    lines.push("--------------------------".to_string());
    for gap in &report.residual_gaps {
        lines.push(format!("- {gap}"));
    }
    lines.push(String::new());
    lines.push(format!("Session dir: {}", report.sources.session_dir));
    lines.push(format!("A2A tasks: {}", report.sources.a2a_tasks_path));
    lines.join("\n")
}

fn format_markdown(report: &ValueReport) -> String {
    let mut lines = vec![
        format!("# Customer Value Report ({})", report.range.label),
        String::new(),
        format!("Generated: {}", report.generated_at),
        String::new(),
        "## Summary".to_string(),
        String::new(),
        format!("- Sessions: {}", report.summary.session_count),
        format!("- Trust cards: {}", report.summary.trust_card_count),
        format!("- Messages: {}", report.summary.message_count),
        format!("- Tool calls: {}", report.summary.tool_call_count),
        format!(
            "- Tokens: {} / cost ${:.4}",
            report.summary.total_tokens, report.summary.total_cost_usd
        ),
        format!(
            "- Estimated value: {:.1} hours / ${:.2}",
            report.summary.estimated_hours_saved, report.summary.estimated_value_usd
        ),
        format!(
            "- Multi-agent realized value: {:.1} hours / ${:.2}",
            report.summary.multi_agent_estimated_hours_saved,
            report.summary.multi_agent_estimated_value_usd
        ),
        format!(
            "- Multi-agent tasks: {} across {} peer(s), {} with work graphs, {} child run(s)",
            report.summary.multi_agent_task_count,
            report.summary.multi_agent_peer_count,
            report.summary.multi_agent_work_graph_task_count,
            report.summary.multi_agent_child_run_count
        ),
        String::new(),
        "## Top Trust Cards".to_string(),
        String::new(),
    ];

    if report.trust_cards.is_empty() {
        lines.push("No sessions found for this range.".to_string());
    } else {
        for card in report.trust_cards.iter().take(10) {
            lines.push(format!("### {} (`{}`)", card.title, card.session_id));
            lines.push(String::new());
            lines.push(format!(
                "- Messages: {} | Tool calls: {} | Cost: ${:.4}",
                card.message_count, card.tool_call_count, card.usage.cost_usd
            ));
            lines.push(format!("- Model: `{}`", card.model));
            lines.push(format!("- Evidence: `{}`", card.evidence.session_path));
            if let Some(summary) = &card.summary {
                lines.push(format!("- Summary: {summary}"));
            }
            lines.push(String::new());
        }
    }

    lines.push("## Multi-Agent Coordination".to_string());
    lines.push(String::new());
    if report.multi_agent.task_count == 0 {
        if report.multi_agent.collection_gaps.is_empty() {
            lines.push("No A2A delegated tasks found for this range.".to_string());
        } else {
            for gap in &report.multi_agent.collection_gaps {
                lines.push(format!("- {gap}"));
            }
        }
    } else {
        lines.push(format!(
            "- Decision: {}",
            multi_agent_decision_line(&report.multi_agent)
        ));
        lines.push(format!(
            "- Realized value: {:.1} hours / ${:.2} from completed delegated work",
            report.multi_agent.realized_hours_saved, report.multi_agent.realized_value_usd
        ));
        lines.push(format!(
            "- Pending work: {} task(s)",
            report.multi_agent.delegated_pending_task_count
        ));
        lines.push(format!(
            "- Audit-ready tasks: {}",
            report.multi_agent.audit_ready_task_count
        ));
        lines.push(format!(
            "- Evidence gaps: {}",
            report.multi_agent.delegated_evidence_gap_count
        ));
        lines.push(format!("- Evidence: `{}`", report.multi_agent.tasks_path));
        lines.push(format!(
            "- Tasks: {} total, {} completed, {} failed, {} waiting on input",
            report.multi_agent.task_count,
            report.multi_agent.completed_task_count,
            report.multi_agent.failed_task_count,
            report.multi_agent.action_required_task_count
        ));
        lines.push(format!("- Peers: {}", report.multi_agent.peer_count));
        lines.push(format!(
            "- WorkGraph-backed tasks: {}",
            report.multi_agent.work_graph_task_count
        ));
        lines.push(format!(
            "- Child runs: {}",
            report.multi_agent.work_graph_child_run_count
        ));
        if !report.multi_agent.top_peers.is_empty() {
            lines.push(String::new());
            lines.push("### Peer rollup".to_string());
            lines.push(String::new());
            for peer in &report.multi_agent.top_peers {
                lines.push(format!(
                    "- {}: {} task(s), {} completed, {} failed, {} waiting",
                    peer.display_name.as_deref().unwrap_or(&peer.peer),
                    peer.task_count,
                    peer.completed_task_count,
                    peer.failed_task_count,
                    peer.action_required_task_count
                ));
            }
        }
        if !report.multi_agent.recent_tasks.is_empty() {
            lines.push(String::new());
            lines.push("### Recent tasks".to_string());
            lines.push(String::new());
            for task in &report.multi_agent.recent_tasks {
                lines.push(format!(
                    "- **{}** `{}`: {} ({})",
                    task.peer_display_name.as_deref().unwrap_or(&task.peer),
                    task.id,
                    task.status,
                    task.state
                ));
                lines.push(format!("  - {}", task.text));
            }
        }
    }
    lines.push(String::new());

    lines.push("## Residual Gaps".to_string());
    lines.push(String::new());
    for gap in &report.residual_gaps {
        lines.push(format!("- {gap}"));
    }
    lines.push(String::new());
    lines.push(format!(
        "_Native implementation: {}_",
        report.sources.implementation
    ));
    lines.join("\n")
}

fn write_artifacts(report: &ValueReport, output_dir: Option<&Path>) -> Result<ArtifactWriteResult> {
    let output_dir = output_dir
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".maestro").join("value-reports")))
        .unwrap_or_else(|| PathBuf::from(".maestro/value-reports"));

    fs::create_dir_all(&output_dir)
        .with_context(|| format!("create output dir {}", output_dir.display()))?;

    let base_name = unique_base_name(&output_dir, &artifact_base_name(report))?;
    let report_json_path = output_dir.join(format!("{base_name}.json"));
    let report_markdown_path = output_dir.join(format!("{base_name}.md"));
    let manifest_path = output_dir.join(format!("{base_name}.manifest.json"));

    let report_json = format!("{}\n", serde_json::to_string_pretty(report)?);
    let report_markdown = format!("{}\n", format_markdown(report));
    let report_json_sha256 = sha256_hex(report_json.as_bytes());
    let report_markdown_sha256 = sha256_hex(report_markdown.as_bytes());

    let manifest = ArtifactManifest {
        protocol_version: CUSTOMER_VALUE_MANIFEST_VERSION,
        generated_at: report.generated_at.clone(),
        range: report.range.clone(),
        artifacts: ArtifactPaths {
            report_json_path: report_json_path.display().to_string(),
            report_markdown_path: report_markdown_path.display().to_string(),
        },
        hashes: ArtifactHashes {
            report_json_sha256: report_json_sha256.clone(),
            report_markdown_sha256: report_markdown_sha256.clone(),
        },
        sources: report.sources.clone(),
        summary: report.summary.clone(),
        residual_gaps: report.residual_gaps.clone(),
    };
    let manifest_json = format!("{}\n", serde_json::to_string_pretty(&manifest)?);
    let manifest_sha256 = sha256_hex(manifest_json.as_bytes());

    fs::write(&report_json_path, report_json)
        .with_context(|| format!("write {}", report_json_path.display()))?;
    fs::write(&report_markdown_path, report_markdown)
        .with_context(|| format!("write {}", report_markdown_path.display()))?;
    fs::write(&manifest_path, manifest_json)
        .with_context(|| format!("write {}", manifest_path.display()))?;

    Ok(ArtifactWriteResult {
        output_dir: output_dir.display().to_string(),
        report_json_path: report_json_path.display().to_string(),
        report_markdown_path: report_markdown_path.display().to_string(),
        manifest_path: manifest_path.display().to_string(),
        report_json_sha256,
        report_markdown_sha256,
        manifest_sha256,
        manifest,
    })
}

fn artifact_base_name(report: &ValueReport) -> String {
    let timestamp = report
        .generated_at
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    let range = report
        .range
        .label
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    format!("customer-value-{}-{}", range, timestamp)
}

fn unique_base_name(output_dir: &Path, base_name: &str) -> Result<String> {
    for attempt in 0..1000 {
        let candidate = if attempt == 0 {
            base_name.to_string()
        } else {
            format!("{base_name}-{}", attempt + 1)
        };
        let json = output_dir.join(format!("{candidate}.json"));
        let md = output_dir.join(format!("{candidate}.md"));
        let manifest = output_dir.join(format!("{candidate}.manifest.json"));
        if !json.exists() && !md.exists() && !manifest.exists() {
            return Ok(candidate);
        }
    }
    bail!(
        "{}",
        crate::localization::cli_locale().format(
            "could not allocate unique value artifact base name in {0}",
            &[(output_dir.display()).to_string()]
        )
    );
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    use crate::a2a_cli::TranscriptEntry;

    #[test]
    fn resolve_range_defaults_to_30d() {
        let range = resolve_range(None, 1_700_000_000_000);
        assert_eq!(range.label, "Last 30 Days");
        assert!(range.since.is_some());
    }

    #[test]
    fn estimate_hours_matches_ts_formula() {
        assert!((estimate_hours_saved(10, 20) - 1.4).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_args_accepts_period_and_format() {
        let args = [
            "week",
            "--format",
            "json",
            "--write",
            "--a2a-tasks",
            "/tmp/tasks.json",
        ]
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>();
        let parsed = parse_args(&args).unwrap();
        assert_eq!(parsed.period.as_deref(), Some("week"));
        assert_eq!(parsed.format, "json");
        assert!(parsed.write);
        assert_eq!(parsed.a2a_tasks_path.as_deref(), Some("/tmp/tasks.json"));
    }

    #[test]
    fn residual_gaps_drop_full_multi_agent_ledger_gap() {
        assert!(
            !RESIDUAL_GAPS
                .iter()
                .any(|gap| gap.contains("A2A task ledger multi-agent coordination"))
        );
    }

    #[test]
    fn session_timestamp_falls_back_to_modified() {
        let mut info = SessionInfo {
            parent_session: None,
            id: "abc".into(),
            path: PathBuf::from("/tmp/s.jsonl"),
            cwd: "/tmp".into(),
            model: "test".into(),
            thinking_level: crate::session::ThinkingLevel::Medium,
            timestamp: "not-a-date".into(),
            stats: Default::default(),
            meta: None,
            preview: None,
            modified: Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(100)),
        };
        assert_eq!(session_timestamp_ms(&info), 100_000);
        info.timestamp = "2020-01-01T00:00:00Z".into();
        assert!(session_timestamp_ms(&info) > 0);
    }

    fn sample_task(kind: &str, state: &str, peer: &str, with_graph: bool) -> TaskLedgerEntry {
        TaskLedgerEntry {
            id: format!("maestro-a2a-task-{peer}"),
            kind: kind.into(),
            peer: peer.into(),
            peer_display_name: Some(peer.into()),
            task_id: format!("codex-a2a-task-{peer}"),
            context_id: None,
            message_id: None,
            text: "Do the work".into(),
            role: None,
            cwd: None,
            state: state.into(),
            response_text: Some("Done".into()),
            metadata: None,
            work_graph: if with_graph {
                Some(serde_json::json!({
                    "state": "done",
                    "childRunCount": 2,
                    "childRunIds": ["r1", "r2"],
                    "blockedItemCount": 0,
                    "waitingItemCount": 1,
                    "pendingToolCallCount": 0,
                    "codexSubagents": { "edgeCount": 1, "edges": [{}], "childRunIds": ["r1"] }
                }))
            } else {
                None
            },
            transcript: vec![
                TranscriptEntry {
                    at: "2026-05-16T06:38:56.864Z".into(),
                    role: "user".into(),
                    text: "Do the work".into(),
                    state: None,
                    message_id: None,
                    extensions: Default::default(),
                },
                TranscriptEntry {
                    at: "2026-05-16T06:39:51.075Z".into(),
                    role: "agent".into(),
                    text: "Done".into(),
                    state: Some(state.into()),
                    message_id: None,
                    extensions: Default::default(),
                },
            ],
            created_at: "2026-05-16T06:38:56.864Z".into(),
            updated_at: "2026-05-16T06:39:51.075Z".into(),
            completed_at: if is_completed_state(state) {
                Some("2026-05-16T06:39:51.075Z".into())
            } else {
                None
            },
            extensions: Default::default(),
        }
    }

    #[test]
    fn multi_agent_realized_hours_match_ts_formula() {
        let tasks = [
            sample_task("delegation", "TASK_STATE_COMPLETED", "peer-a", true),
            sample_task("delegation", "TASK_STATE_COMPLETED", "peer-b", false),
            sample_task("message", "TASK_STATE_COMPLETED", "peer-c", false),
        ];
        // Only audit-ready delegations count: both completed, only first has workGraph.
        // peer-b missing workGraph is NOT audit-ready (evidence gap).
        // realized = 1 * 0.25 + 1 * 0.1 = 0.35 for peer-a only.
        let audit_ready: Vec<_> = tasks
            .iter()
            .filter(|t| is_audit_ready_delegation(t))
            .collect();
        assert_eq!(audit_ready.len(), 1);
        let base = audit_ready.len() as f64 * 0.25;
        let with_graph = audit_ready
            .iter()
            .filter(|t| t.work_graph.is_some())
            .count() as f64
            * 0.1;
        assert!((base + with_graph - 0.35).abs() < f64::EPSILON);
    }

    #[test]
    fn multi_agent_status_classifies_states() {
        assert_eq!(a2a_task_status("TASK_STATE_COMPLETED"), "completed");
        assert_eq!(a2a_task_status("TASK_STATE_FAILED"), "failed");
        assert_eq!(a2a_task_status("TASK_STATE_INPUT_REQUIRED"), "waiting");
        assert_eq!(a2a_task_status("TASK_STATE_WORKING"), "running");
    }

    #[test]
    fn multi_agent_from_temp_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.json");
        let ledger = crate::a2a_cli::TaskLedgerFile {
            tasks: vec![
                sample_task("delegation", "TASK_STATE_COMPLETED", "desk", true),
                sample_task("delegation", "TASK_STATE_WORKING", "mobile", false),
            ],
            orb_delegations: vec![],
            extensions: Default::default(),
        };
        std::fs::write(&path, serde_json::to_string_pretty(&ledger).unwrap()).unwrap();
        let range = ValueRange {
            label: "All Time".into(),
            since: None,
            until: None,
        };
        let multi = summarize_multi_agent(Some(path.to_str().unwrap()), &range);
        assert_eq!(multi.task_count, 2);
        assert_eq!(multi.peer_count, 2);
        assert_eq!(multi.completed_task_count, 1);
        assert_eq!(multi.work_graph_task_count, 1);
        assert_eq!(multi.work_graph_child_run_count, 2);
        assert!((multi.realized_hours_saved - 0.35).abs() < f64::EPSILON);
        assert!(!multi.recent_tasks.is_empty());
        assert!(!multi.top_peers.is_empty());
    }

    #[test]
    fn session_dir_override_lists_custom_root() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SessionManager::with_sessions_dir("/tmp/workspace", dir.path());
        assert_eq!(manager.sessions_dir(), dir.path());
        let sessions = manager.recent_sessions(10).unwrap();
        assert!(sessions.is_empty());
    }
}
