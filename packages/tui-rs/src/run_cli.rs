//! Native `maestro run` offline reconstruction CLI.
//!
//! Reconstructs a saved session into a human-readable or JSON report covering
//! timeline, trajectory, and AgentRuntime ledger / replay / promotion slices.
//!
//! Replay pins the tool behavior versions recorded in tool-result receipts
//! (e.g. `BashDetails.version`) before classifying replayed commands, so
//! historical sessions replay with the tool behavior they were recorded
//! under; entries without a supported recorded version replay under current
//! behavior.
//!
//! Legacy entries and structured tool details are normalized into derived
//! timeline events, and every ledger entry expands to native dry-run Platform
//! step/work-item/wait promotion operations.
//!
//! Trajectory score + inspection follow the frozen replay-lab contract:
//! [`scoreAgentTrajectoryReport`] with
//! [`DEFAULT_AGENT_TRAJECTORY_REPLAY_LAB_RULES`] (at least
//! `final-event-has-evidence`) and a redacted inspection report with
//! timeline items, score findings, and the full omitted-fields catalog.
//!
//! Session load uses the existing Rust [`SessionManager`]. Timeline is built from
//! the parsed session header, messages, metadata, and compaction / model / thinking
//! change entries.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{Map, Value as JsonValue, json};

use crate::agent::ToolReceiptDetails;
use crate::session::{
    AppMessage, ContentBlock, ParsedSession, SessionHeader, SessionManager, ThinkingLevel,
};
use crate::tools::ToolExecutor;
use crate::tools::details::ToolDetails;
use crate::tools::versions::is_supported_version;

const RUN_RECONSTRUCTION_SCHEMA: &str = "evalops.maestro.run-reconstruction.v1";
const AGENT_TRAJECTORY_SCHEMA: &str = "evalops.maestro.agent-trajectory.v1";
const AGENT_TRAJECTORY_REPLAY_SCHEMA: &str = "evalops.maestro.agent-trajectory-replay.v1";
const AGENT_TRAJECTORY_SCORE_SCHEMA: &str = "evalops.maestro.agent-trajectory-score.v1";
const AGENT_TRAJECTORY_INSPECTION_SCHEMA: &str = "evalops.maestro.agent-trajectory-inspection.v1";
const AGENT_RUNTIME_LEDGER_SCHEMA: &str = "evalops.maestro.agent-runtime-ledger.v1";
const AGENT_RUNTIME_REPLAY_SUMMARY_SCHEMA: &str = "evalops.maestro.agent-runtime-replay-summary.v1";
const AGENT_RUNTIME_PROMOTION_PLAN_SCHEMA: &str = "evalops.maestro.agent-runtime-promotion-plan.v1";
const DETERMINISTIC_EVIDENCE_ENVELOPE_SCHEMA: &str =
    "evalops.maestro.deterministic-evidence-envelope.v1";

const RUN_SUBCOMMANDS: &[&str] = &[
    "inspect", "timeline", "context", "ledger", "replay", "promote",
];

/// Omitted fields catalog for trajectory inspection (matches TS inspection report).
const INSPECTION_OMITTED_FIELDS: &[&str] = &[
    "raw prompts",
    "raw tool arguments",
    "raw tool outputs",
    "full file diffs",
    "timeline metadata values",
    "secrets",
];

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TimelineItem {
    id: String,
    session_id: String,
    timestamp: String,
    #[serde(rename = "type")]
    item_type: String,
    title: String,
    visibility: String,
    source: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<JsonValue>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ComposerRunTimeline {
    session_id: String,
    source: String,
    generated_at: String,
    platform_backed: bool,
    pending_request_count: u64,
    items: Vec<TimelineItem>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CountSummary {
    timeline_items: usize,
    by_type: BTreeMap<String, usize>,
    by_status: BTreeMap<String, usize>,
    by_visibility: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReconstructionCoverage {
    prompt_inputs: bool,
    assistant_responses: bool,
    tool_requests: bool,
    tool_results: bool,
    context_manifest: bool,
    context_diagnostics: bool,
    file_changes: bool,
    artifacts: bool,
    policy_decisions: bool,
    diagnostics: bool,
    compactions: bool,
    pending_requests: bool,
    mcp_context: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PromptContextSummary {
    entries: usize,
    project_docs: usize,
    mcp_servers: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ContextManifestSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    protocol_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    manifest_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    manifest_sha256_verified: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    entries: usize,
    project_docs: usize,
    mcp_servers: usize,
    mcp_resources: usize,
    mcp_prompts: usize,
    diagnostics: usize,
    by_kind: BTreeMap<String, usize>,
    by_source: BTreeMap<String, usize>,
    by_status: BTreeMap<String, usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_doc_bytes_read: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_doc_max_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionReport {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resume_summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    memory_extraction_hash: Option<String>,
    created_at: String,
    updated_at: String,
    message_count: usize,
    session_file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DurabilitySummary {
    reconstructable: bool,
    session_file_present: bool,
    resume_summary_present: bool,
    memory_extraction_hash_present: bool,
    context_manifest_present: bool,
    compaction_checkpoints: usize,
    pending_requests: usize,
    agent_runtime_ledger_entries: usize,
    replay_deterministic: bool,
    promotion_idempotency_key: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct EvidenceEnvelope {
    schema_version: &'static str,
    generated_at: String,
    source_ref: String,
    derived_from: Vec<String>,
    agent_id: Option<String>,
    objective_id: Option<String>,
    action_ids: Vec<String>,
    policy_decision_ids: Vec<String>,
    inspector: EvidenceInspector,
    digests: EvidenceDigests,
    terminal: EvidenceTerminal,
    redaction_state: &'static str,
    missing_signals: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct EvidenceInspector {
    product: &'static str,
    package_version: &'static str,
    reconstruction_schema_version: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    build_digest: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct EvidenceDigests {
    #[serde(skip_serializing_if = "Option::is_none")]
    context_manifest_sha256: Option<String>,
    trajectory_sha256: String,
    inspection_sha256: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct EvidenceTerminal {
    state: &'static str,
    outcome: &'static str,
    final_response_present: bool,
    failed_items: usize,
    pending_items: usize,
    failure_requiredness: &'static str,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RunReconstructionReport {
    schema_version: &'static str,
    session: SessionReport,
    counts: CountSummary,
    coverage: ReconstructionCoverage,
    prompt_context: PromptContextSummary,
    context_manifest: ContextManifestSummary,
    timeline: ComposerRunTimeline,
    context_budget: ContextBudgetReport,
    trajectory: JsonValue,
    trajectory_replay: JsonValue,
    trajectory_score: JsonValue,
    trajectory_inspection: JsonValue,
    agent_runtime_ledger: JsonValue,
    evidence_envelope: EvidenceEnvelope,
    execution_evidence: ExecutionEvidenceSummary,
    durability: DurabilitySummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    residual: Option<JsonValue>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct ContextBudgetReport {
    latest: Option<maestro_context::ContextBudgetSnapshot>,
    latest_compaction: Option<ContextCompactionComparison>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ContextCompactionComparison {
    before: maestro_context::ContextBudgetSnapshot,
    after: Option<maestro_context::ContextBudgetSnapshot>,
    input_token_delta: Option<i64>,
}

/// A projection of recorded tool results, independent of assistant completion prose.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExecutionEvidenceSummary {
    recorded: usize,
    succeeded: usize,
    failed: usize,
    denied: usize,
    cancelled: usize,
    indeterminate: usize,
    unverified: usize,
    file_changes: usize,
    commands: Vec<CommandEvidence>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CommandEvidence {
    call_id: String,
    exit_code: i32,
    status: crate::agent::ExecutionStatus,
}

fn execution_evidence_summary(session: &ParsedSession) -> ExecutionEvidenceSummary {
    use crate::agent::ExecutionStatus;
    let mut summary = ExecutionEvidenceSummary::default();
    // Replayed entries and reconciliation updates describe the same call.
    // Project its latest recorded result instead of counting another execution.
    let mut calls = BTreeMap::new();
    for message in &session.messages {
        if let AppMessage::ToolResult {
            tool_call_id,
            tool_name,
            receipt,
            is_error,
            ..
        } = message
        {
            calls.insert(tool_call_id, (tool_name, receipt, is_error));
        }
    }
    for (tool_call_id, (tool_name, receipt, is_error)) in calls {
        let Some(receipt) = receipt.as_ref().filter(|r| {
            r.call_id == *tool_call_id
                && r.tool_name == *tool_name
                && !(r.status == ExecutionStatus::Succeeded && *is_error)
        }) else {
            summary.unverified += 1;
            continue;
        };
        summary.recorded += 1;
        match receipt.status {
            ExecutionStatus::Succeeded => summary.succeeded += 1,
            ExecutionStatus::Failed => summary.failed += 1,
            ExecutionStatus::Denied => summary.denied += 1,
            ExecutionStatus::Cancelled { .. } => summary.cancelled += 1,
            ExecutionStatus::Indeterminate => summary.indeterminate += 1,
        }
        match &receipt.details {
            ToolReceiptDetails::BuiltIn(ToolDetails::Bash(details)) => {
                summary.commands.push(CommandEvidence {
                    call_id: tool_call_id.clone(),
                    exit_code: details.exit_code,
                    status: receipt.status,
                });
            }
            ToolReceiptDetails::BuiltIn(ToolDetails::Write(_) | ToolDetails::Edit(_))
                if receipt.status == ExecutionStatus::Succeeded =>
            {
                summary.file_changes += 1;
            }
            _ => {}
        }
    }
    summary
}

/// Dispatch `maestro run <subcommand> <session-id> [--json]`.
pub async fn run_run(args: &[String]) -> Result<i32> {
    let subcommand = args.first().map(String::as_str).unwrap_or("help");
    if matches!(subcommand, "help" | "--help" | "-h") {
        println!("{}", run_help());
        return Ok(0);
    }
    if !RUN_SUBCOMMANDS.contains(&subcommand) {
        eprintln!(
            "{}",
            crate::localization::cli_locale().format("Run subcommand required.", &[])
        );
        eprintln!("{}", run_help());
        return Ok(1);
    }

    let rest = &args[1..];
    let json = rest.iter().any(|arg| arg == "--json");
    let session_id = rest
        .iter()
        .find(|arg| !arg.starts_with('-'))
        .map(String::as_str);
    let Some(session_id) = session_id else {
        eprintln!(
            "{}",
            crate::localization::cli_locale().format("Session id required.", &[])
        );
        eprintln!("{}", run_help());
        return Ok(1);
    };

    let report = match build_run_reconstruction_report(session_id) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("{error:#}");
            eprintln!("{}", run_help());
            return Ok(1);
        }
    };

    match subcommand {
        "timeline" if json => {
            println!("{}", serde_json::to_string_pretty(&report.timeline)?);
        }
        "timeline" => {
            println!("{}", render_timeline_swimlane(&report.timeline));
        }
        "context" if json => {
            println!("{}", serde_json::to_string_pretty(&report.context_budget)?);
        }
        "context" => {
            println!("{}", render_context_budget(&report.context_budget));
        }
        "ledger" => {
            println!(
                "{}",
                serde_json::to_string_pretty(&report.agent_runtime_ledger)?
            );
        }
        "replay" => {
            let replay = report
                .agent_runtime_ledger
                .get("replay")
                .cloned()
                .unwrap_or_else(|| json!({}));
            println!("{}", serde_json::to_string_pretty(&replay)?);
        }
        "promote" => {
            let promotion = report
                .agent_runtime_ledger
                .get("promotion")
                .cloned()
                .unwrap_or_else(|| json!({}));
            println!("{}", serde_json::to_string_pretty(&promotion)?);
        }
        "inspect" if json => {
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        "inspect" => {
            println!("{}", render_run_reconstruction(&report));
        }
        _ => unreachable!(),
    }
    Ok(0)
}

fn run_help() -> &'static str {
    "Usage: maestro run inspect|timeline|context|ledger|replay|promote <session-id> [--json]

Commands:
  inspect   Reconstruct timeline, trajectory, and durability summary
  timeline  Render the redacted session-event swimlane
  context   Render the prepared-request context-budget waterfall
  ledger    Print the AgentRuntime ledger projection (JSON)
  replay    Print the AgentRuntime replay summary (JSON)
  promote   Print the dry-run Platform promotion plan (JSON)

Options:
  --json    Machine-readable output for inspect, timeline, or context
  --help    Show this help

Notes:
  Trajectory score uses DEFAULT lab rules (final-event-has-evidence) and
  inspection emits redacted timeline items + score findings."
}

fn build_run_reconstruction_report(session_id: &str) -> Result<RunReconstructionReport> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let manager = SessionManager::new(cwd.to_string_lossy().to_string());
    let session = manager
        .load_session(session_id)
        .with_context(|| format!("Session not found: {session_id}"))?;

    let generated_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    Ok(build_report_from_session(&session, &generated_at))
}

fn build_report_from_session(
    session: &ParsedSession,
    generated_at: &str,
) -> RunReconstructionReport {
    let timeline = build_timeline(session, generated_at);
    let context_budget = build_context_budget_report(session);
    let counts = count_timeline(&timeline);
    let prompt_context = prompt_context_summary(&session.header, &timeline);
    let context_manifest = context_manifest_summary(&session.header, &timeline, &prompt_context);
    let coverage = build_coverage(&counts, &prompt_context, &context_manifest);

    let trajectory = build_trajectory(session, &timeline, generated_at);
    let trajectory_replay = build_trajectory_replay(&trajectory, generated_at);
    let trajectory_score =
        score_agent_trajectory_report(&trajectory, &default_agent_trajectory_replay_lab_rules());
    let trajectory_inspection = build_trajectory_inspection(
        &timeline,
        &trajectory,
        &trajectory_replay,
        &trajectory_score,
    );
    let agent_runtime_ledger =
        build_agent_runtime_ledger(session, &timeline, &trajectory, generated_at);
    let evidence_envelope = build_evidence_envelope(
        session,
        &timeline,
        &context_manifest,
        &trajectory,
        &trajectory_inspection,
        generated_at,
    );

    let session_report = session_report(session);
    let durability = build_durability(
        &session_report,
        &counts,
        &context_manifest,
        &agent_runtime_ledger,
    );

    RunReconstructionReport {
        schema_version: RUN_RECONSTRUCTION_SCHEMA,
        session: session_report,
        counts,
        coverage,
        prompt_context,
        context_manifest,
        timeline,
        context_budget,
        trajectory,
        trajectory_replay,
        trajectory_score,
        trajectory_inspection,
        agent_runtime_ledger,
        evidence_envelope,
        execution_evidence: execution_evidence_summary(session),
        durability,
        residual: None,
    }
}

fn build_context_budget_report(session: &ParsedSession) -> ContextBudgetReport {
    let latest = session.context_budget_snapshots.last().cloned();
    let before = session
        .context_budget_snapshots
        .iter()
        .rev()
        .find(|snapshot| snapshot.phase == maestro_context::ContextBudgetPhase::BeforeCompaction)
        .cloned();
    let latest_compaction = before.map(|before| {
        let after = session
            .context_budget_snapshots
            .iter()
            .find(|snapshot| {
                snapshot.timestamp > before.timestamp
                    && snapshot.phase == maestro_context::ContextBudgetPhase::PreparedRequest
            })
            .cloned();
        let input_token_delta = after.as_ref().map(|after| {
            i64::try_from(after.input_tokens()).unwrap_or(i64::MAX)
                - i64::try_from(before.input_tokens()).unwrap_or(i64::MAX)
        });
        ContextCompactionComparison {
            before,
            after,
            input_token_delta,
        }
    });
    ContextBudgetReport {
        latest,
        latest_compaction,
    }
}

fn session_report(session: &ParsedSession) -> SessionReport {
    let title = session.meta.as_ref().and_then(|m| m.title.clone());
    let summary = session.meta.as_ref().and_then(|m| m.summary.clone());
    let resume_summary = session.meta.as_ref().and_then(|m| m.resume_summary.clone());
    let memory_extraction_hash = session
        .meta
        .as_ref()
        .and_then(|m| m.memory_extraction_hash.clone());
    let updated_at = session
        .meta
        .as_ref()
        .map(|m| m.timestamp.clone())
        .filter(|ts| !ts.is_empty())
        .or_else(|| {
            session
                .messages
                .last()
                .map(|m| millis_to_rfc3339(m.timestamp()))
        })
        .unwrap_or_else(|| session.header.timestamp.clone());

    SessionReport {
        id: session.header.id.clone(),
        title,
        summary,
        resume_summary,
        memory_extraction_hash,
        created_at: session.header.timestamp.clone(),
        updated_at,
        message_count: session.stats.total_messages(),
        session_file: session.file_path.clone(),
        cwd: nonempty_opt(&session.header.cwd),
        model: nonempty_opt(&session.header.model),
    }
}

fn nonempty_opt(value: &str) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn build_timeline(session: &ParsedSession, generated_at: &str) -> ComposerRunTimeline {
    let session_id = session.header.id.clone();
    let mut items = Vec::new();

    items.push(TimelineItem {
        id: format!("session-started:{session_id}"),
        session_id: session_id.clone(),
        timestamp: session.header.timestamp.clone(),
        item_type: "session.started".into(),
        title: "Session started".into(),
        visibility: "user".into(),
        source: "local".into(),
        status: "info".into(),
        role: None,
        summary: None,
        tool_call_id: None,
        tool_name: None,
        metadata: nonempty_opt(&session.header.cwd).map(|cwd| json!({ "cwd": cwd })),
    });

    if let Some(parent_session) = session
        .header
        .branched_from
        .as_ref()
        .or(session.header.parent_session.as_ref())
        .filter(|_| {
            !session
                .session_events
                .iter()
                .any(|event| matches!(event.kind.as_str(), "session.forked" | "session.rewound"))
        })
    {
        items.push(TimelineItem {
            id: format!("session-forked:{session_id}"),
            session_id: session_id.clone(),
            timestamp: session.header.timestamp.clone(),
            item_type: "session.forked".into(),
            title: "Session forked".into(),
            visibility: "admin".into(),
            source: "local".into(),
            status: "info".into(),
            role: None,
            summary: None,
            tool_call_id: None,
            tool_name: None,
            metadata: Some(json!({ "parentSessionId": parent_session })),
        });
    }

    if let Some(meta) = &session.meta {
        items.push(TimelineItem {
            id: "session-updated:meta".into(),
            session_id: session_id.clone(),
            timestamp: meta.timestamp.clone(),
            item_type: "session.updated".into(),
            title: "Session metadata updated".into(),
            visibility: "admin".into(),
            source: "local".into(),
            status: "info".into(),
            role: None,
            summary: compact_summary(
                meta.title
                    .as_deref()
                    .or(meta.resume_summary.as_deref())
                    .or(meta.summary.as_deref()),
            ),
            tool_call_id: None,
            tool_name: None,
            metadata: None,
        });
    }

    for (index, change) in session.thinking_level_changes.iter().enumerate() {
        items.push(TimelineItem {
            id: format!("thinking-change:{index}"),
            session_id: session_id.clone(),
            timestamp: change.timestamp.clone(),
            item_type: "thinking.changed".into(),
            title: "Thinking level changed".into(),
            visibility: "admin".into(),
            source: "local".into(),
            status: "info".into(),
            role: None,
            summary: Some(thinking_level_label(change.thinking_level)),
            tool_call_id: None,
            tool_name: None,
            metadata: None,
        });
    }

    for (index, change) in session.model_changes.iter().enumerate() {
        items.push(TimelineItem {
            id: format!("model-change:{index}"),
            session_id: session_id.clone(),
            timestamp: change.timestamp.clone(),
            item_type: "model.changed".into(),
            title: "Model changed".into(),
            visibility: "admin".into(),
            source: "local".into(),
            status: "info".into(),
            role: None,
            summary: compact_summary(Some(change.model.as_str())),
            tool_call_id: None,
            tool_name: None,
            metadata: None,
        });
    }

    for (index, compaction) in session.compactions.iter().enumerate() {
        let id = compaction
            .id
            .clone()
            .unwrap_or_else(|| format!("compaction-{index}"));
        items.push(TimelineItem {
            id: format!("compaction:{id}"),
            session_id: session_id.clone(),
            timestamp: compaction.timestamp.clone(),
            item_type: "compaction.created".into(),
            title: "Context compacted".into(),
            visibility: "admin".into(),
            source: "local".into(),
            status: "info".into(),
            role: None,
            summary: compact_summary(Some(compaction.summary.as_str())),
            tool_call_id: None,
            tool_name: None,
            metadata: Some(json!({
                "tokensBefore": compaction.tokens_before,
                "auto": compaction.auto,
            })),
        });
    }

    for event in &session.session_events {
        if event.kind == "session.created" {
            continue;
        }
        items.push(TimelineItem {
            id: format!("event:{}", event.event_id),
            session_id: session_id.clone(),
            timestamp: event.timestamp.clone(),
            item_type: event.kind.clone(),
            title: session_event_title(event),
            visibility: "admin".into(),
            source: "local-telemetry".into(),
            status: "info".into(),
            role: None,
            summary: None,
            tool_call_id: event
                .correlation_id
                .clone()
                .filter(|_| event.kind.starts_with("tool.")),
            tool_name: event.name.clone(),
            metadata: Some(json!({
                "lane": event.lane,
                "phase": event.phase,
                "correlationId": event.correlation_id,
                "attempt": event.attempt,
                "delayMs": event.delay_ms,
                "durationMs": event.duration_ms,
                "automatic": event.automatic,
            })),
        });
    }

    for (index, message) in session.messages.iter().enumerate() {
        append_message_items(&mut items, &session_id, message, index, generated_at);
    }

    items.sort_by(|a, b| a.timestamp.cmp(&b.timestamp).then_with(|| a.id.cmp(&b.id)));
    let pending_request_count = items
        .iter()
        .filter(|item| item.item_type == "wait.pending")
        .count() as u64;

    ComposerRunTimeline {
        session_id,
        source: "local".into(),
        generated_at: generated_at.to_string(),
        platform_backed: false,
        pending_request_count,
        items,
    }
}

fn append_message_items(
    items: &mut Vec<TimelineItem>,
    session_id: &str,
    message: &AppMessage,
    index: usize,
    generated_at: &str,
) {
    let base_id = format!("msg-{index}");
    let timestamp = {
        let ms = message.timestamp();
        if ms == 0 {
            generated_at.to_string()
        } else {
            millis_to_rfc3339(ms)
        }
    };

    match message {
        AppMessage::User { .. } => {
            items.push(TimelineItem {
                id: format!("message:{base_id}"),
                session_id: session_id.into(),
                timestamp,
                item_type: "message.user".into(),
                title: "User message".into(),
                visibility: "user".into(),
                source: "local".into(),
                status: "completed".into(),
                role: Some("user".into()),
                summary: compact_summary(Some(&message.text_content())),
                tool_call_id: None,
                tool_name: None,
                metadata: None,
            });
        }
        AppMessage::Assistant {
            content,
            model,
            stop_reason,
            ..
        } => {
            let failed = stop_reason.as_deref() == Some("error");
            items.push(TimelineItem {
                id: format!("message:{base_id}"),
                session_id: session_id.into(),
                timestamp: timestamp.clone(),
                item_type: "message.assistant".into(),
                title: "Assistant response".into(),
                visibility: "user".into(),
                source: "local".into(),
                status: if failed {
                    "failed".into()
                } else {
                    "completed".into()
                },
                role: Some("assistant".into()),
                summary: compact_summary(Some(&message.text_content())),
                tool_call_id: None,
                tool_name: None,
                metadata: model.as_ref().map(|m| json!({ "model": m })),
            });
            for block in content {
                if let ContentBlock::ToolCall { id, name, .. } = block {
                    items.push(TimelineItem {
                        id: format!("tool-requested:{base_id}:{id}"),
                        session_id: session_id.into(),
                        timestamp: timestamp.clone(),
                        item_type: "tool.requested".into(),
                        title: format!("Requested {name}"),
                        visibility: "user".into(),
                        source: "local".into(),
                        status: "running".into(),
                        role: None,
                        summary: None,
                        tool_call_id: Some(id.clone()),
                        tool_name: Some(name.clone()),
                        metadata: None,
                    });
                }
            }
        }
        AppMessage::ToolResult {
            tool_call_id,
            tool_name,
            content,
            details,
            is_error,
            ..
        } => {
            let (item_type, title, status) = if *is_error {
                (
                    "tool.failed",
                    format!("{tool_name} failed"),
                    "failed".to_string(),
                )
            } else {
                (
                    "tool.completed",
                    format!("{tool_name} completed"),
                    "completed".to_string(),
                )
            };
            items.push(TimelineItem {
                id: format!("tool-result:{base_id}:{tool_call_id}"),
                session_id: session_id.into(),
                timestamp: timestamp.clone(),
                item_type: item_type.into(),
                title,
                visibility: "user".into(),
                source: "local".into(),
                status,
                role: Some("tool".into()),
                summary: None,
                tool_call_id: Some(tool_call_id.clone()),
                tool_name: Some(tool_name.clone()),
                metadata: None,
            });
            append_derived_tool_result_items(
                items,
                session_id,
                &base_id,
                &timestamp,
                tool_call_id,
                tool_name,
                content,
                details.as_ref(),
                *is_error,
            );
        }
    }
}

fn session_event_title(event: &maestro_runtime_contracts::SessionEvent) -> String {
    match event.kind.as_str() {
        "model.request.started" => "Model request started".into(),
        "model.response.completed" => "Model response completed".into(),
        "model.response.failed" => event.name.as_ref().map_or_else(
            || "Model response failed".into(),
            |name| format!("Model response failed ({name})"),
        ),
        "model.gateway.receipt" => event.correlation_id.as_ref().map_or_else(
            || "Gateway request accepted".into(),
            |request_id| format!("Gateway request ID: {request_id}"),
        ),
        "turn.started" => "Turn started".into(),
        "turn.completed" => "Turn completed".into(),
        "turn.cancelled" => "Turn cancelled".into(),
        "tool.approval.requested" => event.name.as_ref().map_or_else(
            || "Tool approval requested".into(),
            |name| format!("Approval requested for {name}"),
        ),
        "tool.started" => event
            .name
            .as_ref()
            .map_or_else(|| "Tool started".into(), |name| format!("{name} started")),
        "rate_limit.hit" => "Rate limit observed".into(),
        "retry.scheduled" => event.name.as_ref().map_or_else(
            || "Request retry scheduled".into(),
            |code| format!("Request retry scheduled ({code})"),
        ),
        "retry.started" => "Request retry started".into(),
        "compaction.measured" => "Compaction measured".into(),
        "compaction.completed" => "Compaction completed".into(),
        "session.restarted" => "Runtime transport restarted".into(),
        "session.forked" => "Session forked".into(),
        "session.rewound" => "Session rewound".into(),
        "session.closed" => "Session closed".into(),
        kind if kind.starts_with("worker.") => {
            format!(
                "Worker {}",
                kind.trim_start_matches("worker.").replace('_', " ")
            )
        }
        _ => event.kind.replace(['.', '_'], " "),
    }
}

#[allow(clippy::too_many_arguments)]
fn append_derived_tool_result_items(
    items: &mut Vec<TimelineItem>,
    session_id: &str,
    base_id: &str,
    timestamp: &str,
    tool_call_id: &str,
    tool_name: &str,
    content: &str,
    details: Option<&JsonValue>,
    is_error: bool,
) {
    let details = details.and_then(JsonValue::as_object);
    let normalized_tool = tool_name.to_ascii_lowercase();
    if !is_error && matches!(normalized_tool.as_str(), "write" | "edit" | "apply_patch") {
        let path = detail_string(
            details,
            &["displayPath", "path", "filePath", "relativePath"],
        );
        let previous_exists = detail_bool(details, "previousExists");
        let action = if normalized_tool == "write" && previous_exists == Some(false) {
            "created"
        } else if normalized_tool == "write" {
            "wrote"
        } else {
            "edited"
        };
        let bytes_written = detail_u64(details, "bytesWritten");
        let edits_applied = detail_u64(details, "editsApplied");
        items.push(TimelineItem {
            id: format!("file-change:{base_id}:{tool_call_id}"),
            session_id: session_id.to_string(),
            timestamp: timestamp.to_string(),
            item_type: "file.changed".to_string(),
            title: format!("File {action}"),
            visibility: "user".to_string(),
            source: "local".to_string(),
            status: "completed".to_string(),
            role: None,
            summary: compact_summary(Some(
                &[
                    path.clone(),
                    bytes_written.map(|value| format!("{value} bytes")),
                    edits_applied.map(|value| format!("{value} edits")),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" | "),
            )),
            tool_call_id: Some(tool_call_id.to_string()),
            tool_name: Some(tool_name.to_string()),
            metadata: Some(json!({
                "path": path,
                "action": action,
                "previousExists": previous_exists,
                "bytesWritten": bytes_written,
                "editsApplied": edits_applied,
                "hasDiff": details.and_then(|value| value.get("diff")).and_then(JsonValue::as_str).is_some_and(|value| !value.is_empty()),
            })),
        });
    }

    if let Some(delta) = details
        .and_then(|value| {
            value
                .get("diagnosticDelta")
                .or_else(|| value.get("diagnostic_delta"))
        })
        .and_then(JsonValue::as_object)
    {
        let introduced = detail_u64(Some(delta), "introducedCount").unwrap_or(0);
        let repaired = detail_u64(Some(delta), "repairedCount").unwrap_or(0);
        let remaining = detail_u64(Some(delta), "remainingCount").unwrap_or(0);
        let display_path = detail_string(Some(delta), &["displayPath", "path"])
            .unwrap_or_else(|| "workspace".to_string());
        items.push(TimelineItem {
            id: format!("diagnostic-delta:{base_id}:{tool_call_id}"),
            session_id: session_id.to_string(),
            timestamp: timestamp.to_string(),
            item_type: "diagnostic.delta".to_string(),
            title: format!("Diagnostics for {display_path}"),
            visibility: "user".to_string(),
            source: "local".to_string(),
            status: if introduced > 0 { "failed" } else { "completed" }.to_string(),
            role: None,
            summary: Some(format!(
                "Diagnostic delta: {introduced} introduced, {repaired} repaired, {remaining} remaining."
            )),
            tool_call_id: Some(tool_call_id.to_string()),
            tool_name: Some(tool_name.to_string()),
            metadata: Some(JsonValue::Object(delta.clone())),
        });
    }

    if let Some(skill) = details
        .and_then(|value| {
            value
                .get("skillArtifact")
                .or_else(|| value.get("skill_artifact"))
        })
        .and_then(JsonValue::as_object)
    {
        let name = detail_string(Some(skill), &["name"]).unwrap_or_else(|| "skill".to_string());
        items.push(TimelineItem {
            id: format!("artifact-linked:{base_id}:{tool_call_id}"),
            session_id: session_id.to_string(),
            timestamp: timestamp.to_string(),
            item_type: "artifact.linked".to_string(),
            title: format!("Skill artifact loaded: {name}"),
            visibility: "admin".to_string(),
            source: "local".to_string(),
            status: "completed".to_string(),
            role: None,
            summary: compact_summary(Some(content)),
            tool_call_id: Some(tool_call_id.to_string()),
            tool_name: Some(tool_name.to_string()),
            metadata: Some(JsonValue::Object(skill.clone())),
        });
    }

    if let Some(outcome) = details
        .and_then(|value| {
            value
                .get("governedOutcome")
                .or_else(|| value.get("governed_outcome"))
        })
        .and_then(JsonValue::as_object)
    {
        let classification = detail_string(Some(outcome), &["classification"]);
        let status = match classification.as_deref() {
            Some("denied") => "denied",
            Some(
                "approval_required"
                | "approval_pending"
                | "authentication_required"
                | "rate_limited",
            ) => "pending",
            _ => "info",
        };
        items.push(TimelineItem {
            id: format!("policy-decision:{base_id}:{tool_call_id}"),
            session_id: session_id.to_string(),
            timestamp: timestamp.to_string(),
            item_type: "policy.decision".to_string(),
            title: format!("Policy decision for {tool_name}"),
            visibility: if classification.as_deref() == Some("denied") {
                "user"
            } else {
                "admin"
            }
            .to_string(),
            source: "local".to_string(),
            status: status.to_string(),
            role: None,
            summary: classification.map(|value| format!("Outcome: {value}")),
            tool_call_id: Some(tool_call_id.to_string()),
            tool_name: Some(tool_name.to_string()),
            metadata: Some(JsonValue::Object(outcome.clone())),
        });
    }
}

fn detail_string(details: Option<&Map<String, JsonValue>>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        details?
            .get(*key)
            .and_then(JsonValue::as_str)
            .map(str::to_string)
    })
}

fn detail_u64(details: Option<&Map<String, JsonValue>>, key: &str) -> Option<u64> {
    details?.get(key).and_then(JsonValue::as_u64)
}

fn detail_bool(details: Option<&Map<String, JsonValue>>, key: &str) -> Option<bool> {
    details?.get(key).and_then(JsonValue::as_bool)
}

fn count_timeline(timeline: &ComposerRunTimeline) -> CountSummary {
    let mut by_type = BTreeMap::new();
    let mut by_status = BTreeMap::new();
    let mut by_visibility = BTreeMap::new();
    for item in &timeline.items {
        *by_type.entry(item.item_type.clone()).or_insert(0) += 1;
        *by_status.entry(item.status.clone()).or_insert(0) += 1;
        *by_visibility.entry(item.visibility.clone()).or_insert(0) += 1;
    }
    CountSummary {
        timeline_items: timeline.items.len(),
        by_type,
        by_status,
        by_visibility,
    }
}

fn collect_mcp_from_tools_and_timeline(
    header: &SessionHeader,
    timeline: &ComposerRunTimeline,
    mcp_servers: &mut BTreeSet<String>,
) {
    for tool in &header.tools {
        if let Some(server) = parse_mcp_tool_name(&tool.name) {
            mcp_servers.insert(server);
        }
    }
    for item in &timeline.items {
        if let Some(name) = &item.tool_name {
            if let Some(server) = parse_mcp_tool_name(name) {
                mcp_servers.insert(server);
            }
        }
    }
}

fn prompt_context_summary(
    header: &SessionHeader,
    timeline: &ComposerRunTimeline,
) -> PromptContextSummary {
    let mut project_docs = 0usize;
    let mut mcp_servers = BTreeSet::new();
    let mut entries = 0usize;

    if let Some(manifest) = header.prompt_context_manifest.as_ref() {
        if let Some(list) = manifest.get("entries").and_then(JsonValue::as_array) {
            entries = list.len();
            for entry in list {
                let kind = entry.get("kind").and_then(JsonValue::as_str).unwrap_or("");
                let source_kind = entry
                    .get("sourceKind")
                    .or_else(|| entry.get("source_kind"))
                    .and_then(JsonValue::as_str)
                    .unwrap_or("");
                let resource_kind = entry
                    .get("resourceKind")
                    .or_else(|| entry.get("resource_kind"))
                    .and_then(JsonValue::as_str)
                    .unwrap_or("");
                if kind == "project_doc" || source_kind == "project" || source_kind == "global" {
                    project_docs += 1;
                } else if kind == "mcp_server" || resource_kind == "mcp_server" {
                    let server = entry
                        .get("serverName")
                        .or_else(|| entry.get("server_name"))
                        .or_else(|| entry.get("resourceId"))
                        .or_else(|| entry.get("resource_id"))
                        .or_else(|| entry.get("providerId"))
                        .or_else(|| entry.get("provider_id"))
                        .or_else(|| entry.get("id"))
                        .and_then(JsonValue::as_str)
                        .unwrap_or("unknown");
                    mcp_servers.insert(server.to_string());
                }
            }
        }
    }
    collect_mcp_from_tools_and_timeline(header, timeline, &mut mcp_servers);
    PromptContextSummary {
        entries,
        project_docs,
        mcp_servers: mcp_servers.len(),
    }
}

fn context_manifest_summary(
    header: &SessionHeader,
    timeline: &ComposerRunTimeline,
    prompt_context: &PromptContextSummary,
) -> ContextManifestSummary {
    let mut by_kind = BTreeMap::new();
    let mut by_source = BTreeMap::new();
    let mut by_status = BTreeMap::new();
    let mut mcp_servers = BTreeSet::new();
    let mut mcp_resources = 0usize;
    let mut mcp_prompts = 0usize;
    let mut project_docs = 0usize;

    let Some(manifest) = header.unified_context_manifest.as_ref() else {
        return ContextManifestSummary {
            protocol_version: None,
            version: None,
            manifest_sha256: None,
            manifest_sha256_verified: None,
            cwd: None,
            entries: prompt_context.entries,
            project_docs: prompt_context.project_docs,
            mcp_servers: prompt_context.mcp_servers,
            mcp_resources: 0,
            mcp_prompts: 0,
            diagnostics: 0,
            by_kind,
            by_source,
            by_status,
            project_doc_bytes_read: None,
            project_doc_max_bytes: None,
        };
    };

    if let Some(list) = manifest.get("entries").and_then(JsonValue::as_array) {
        for entry in list {
            if let Some(kind) = entry.get("kind").and_then(JsonValue::as_str) {
                *by_kind.entry(kind.to_string()).or_insert(0) += 1;
                match kind {
                    "project_doc" => project_docs += 1,
                    "mcp_server" => {
                        if let Some(name) = entry
                            .get("serverName")
                            .or_else(|| entry.get("server_name"))
                            .or_else(|| entry.get("id"))
                            .and_then(JsonValue::as_str)
                        {
                            mcp_servers.insert(name.to_string());
                        }
                    }
                    "mcp_resource" => {
                        mcp_resources += 1;
                        if let Some(name) = entry
                            .get("serverName")
                            .or_else(|| entry.get("server_name"))
                            .and_then(JsonValue::as_str)
                        {
                            mcp_servers.insert(name.to_string());
                        }
                    }
                    "mcp_prompt" => {
                        mcp_prompts += 1;
                        if let Some(name) = entry
                            .get("serverName")
                            .or_else(|| entry.get("server_name"))
                            .and_then(JsonValue::as_str)
                        {
                            mcp_servers.insert(name.to_string());
                        }
                    }
                    _ => {}
                }
            }
            if let Some(source) = entry.get("source").and_then(JsonValue::as_str) {
                *by_source.entry(source.to_string()).or_insert(0) += 1;
            }
            if let Some(status) = entry.get("status").and_then(JsonValue::as_str) {
                *by_status.entry(status.to_string()).or_insert(0) += 1;
            }
        }
    }
    collect_mcp_from_tools_and_timeline(header, timeline, &mut mcp_servers);

    let entries_len = manifest
        .get("entries")
        .and_then(JsonValue::as_array)
        .map(|a| a.len())
        .unwrap_or(0);
    let diagnostics = manifest
        .get("diagnostics")
        .and_then(JsonValue::as_array)
        .map(|a| a.len())
        .unwrap_or(0);

    let mut digest_input = (**manifest).clone();
    let declared_manifest_sha256 = digest_input
        .get("manifestSha256")
        .or_else(|| digest_input.get("manifest_sha256"))
        .and_then(JsonValue::as_str)
        .map(str::to_string);
    if let Some(object) = digest_input.as_object_mut() {
        object.remove("manifestSha256");
        object.remove("manifest_sha256");
    }
    let computed_manifest_sha256 = crate::evidence::canonical_json_sha256(&digest_input);

    ContextManifestSummary {
        protocol_version: manifest
            .get("protocolVersion")
            .or_else(|| manifest.get("protocol_version"))
            .and_then(JsonValue::as_str)
            .map(str::to_string),
        version: manifest.get("version").and_then(JsonValue::as_u64),
        manifest_sha256: Some(computed_manifest_sha256.clone()),
        manifest_sha256_verified: declared_manifest_sha256
            .map(|declared| declared == computed_manifest_sha256),
        cwd: manifest
            .get("cwd")
            .and_then(JsonValue::as_str)
            .map(str::to_string),
        entries: entries_len,
        project_docs,
        mcp_servers: mcp_servers.len(),
        mcp_resources,
        mcp_prompts,
        diagnostics,
        by_kind,
        by_source,
        by_status,
        project_doc_bytes_read: manifest
            .get("projectDocs")
            .or_else(|| manifest.get("project_docs"))
            .and_then(|docs| docs.get("bytesRead").or_else(|| docs.get("bytes_read")))
            .and_then(JsonValue::as_u64),
        project_doc_max_bytes: manifest
            .get("projectDocs")
            .or_else(|| manifest.get("project_docs"))
            .and_then(|docs| docs.get("maxBytes").or_else(|| docs.get("max_bytes")))
            .and_then(JsonValue::as_u64),
    }
}

fn build_evidence_envelope(
    session: &ParsedSession,
    timeline: &ComposerRunTimeline,
    context_manifest: &ContextManifestSummary,
    trajectory: &JsonValue,
    trajectory_inspection: &JsonValue,
    generated_at: &str,
) -> EvidenceEnvelope {
    let source_ref = format!("maestro://session/{}", session.header.id);
    let agent_id = prompt_metadata_string(
        &session.header,
        &["agentId", "agent_id", "agentRuntimeId", "agent_runtime_id"],
    );
    let objective_id = prompt_metadata_string(
        &session.header,
        &["objectiveId", "objective_id", "taskId", "task_id"],
    );
    let action_ids = timeline
        .items
        .iter()
        .filter_map(|item| item.tool_call_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let policy_items = timeline
        .items
        .iter()
        .filter(|item| item.item_type == "policy.decision")
        .collect::<Vec<_>>();
    let policy_decision_ids = policy_items
        .iter()
        .filter_map(|item| {
            json_string(
                item.metadata.as_ref(),
                &[
                    "policyDecisionId",
                    "policy_decision_id",
                    "decisionId",
                    "decision_id",
                ],
            )
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let failed_items = timeline
        .items
        .iter()
        .filter(|item| item.status == "failed")
        .count();
    let pending_items = timeline
        .items
        .iter()
        .filter(|item| item.status == "pending")
        .count();
    let final_response_index = timeline
        .items
        .iter()
        .rposition(|item| item.item_type == "message.assistant");
    let last_work_index = timeline.items.iter().rposition(|item| {
        item.item_type.starts_with("tool.")
            || item.item_type == "policy.decision"
            || item.item_type == "wait.pending"
            || item.item_type.starts_with("agent.run.")
    });
    let final_response_present = final_response_index
        .is_some_and(|response| last_work_index.is_none_or(|work| response > work));
    let final_response_failed = final_response_index
        .is_some_and(|index| final_response_present && timeline.items[index].status == "failed");
    let (state, outcome) = if pending_items > 0 {
        ("blocked", "blocked")
    } else if final_response_failed {
        ("failed", "failed")
    } else if final_response_present && failed_items > 0 {
        ("completed", "degraded")
    } else if final_response_present {
        ("completed", "ok")
    } else if failed_items > 0 {
        ("failed", "failed")
    } else {
        ("incomplete", "unknown")
    };
    let build_digest = std::env::var("MAESTRO_BUILD_DIGEST")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let mut missing_signals = Vec::new();
    if agent_id.is_none() {
        missing_signals.push("agent_id".to_string());
    }
    if objective_id.is_none() {
        missing_signals.push("objective_id".to_string());
    }
    if context_manifest.manifest_sha256.is_none() {
        missing_signals.push("context_manifest".to_string());
    }
    if !policy_items.is_empty() && policy_decision_ids.is_empty() {
        missing_signals.push("policy_decision_id".to_string());
    }
    if failed_items > 0 {
        missing_signals.push("failure_requiredness".to_string());
    }
    if build_digest.is_none() {
        missing_signals.push("inspector_build_digest".to_string());
    }

    EvidenceEnvelope {
        schema_version: DETERMINISTIC_EVIDENCE_ENVELOPE_SCHEMA,
        generated_at: generated_at.to_string(),
        source_ref: source_ref.clone(),
        derived_from: vec![
            format!("{source_ref}#timeline"),
            format!("{source_ref}#trajectory"),
        ],
        agent_id,
        objective_id,
        action_ids,
        policy_decision_ids,
        inspector: EvidenceInspector {
            product: "deixic-code",
            package_version: env!("CARGO_PKG_VERSION"),
            reconstruction_schema_version: RUN_RECONSTRUCTION_SCHEMA,
            build_digest,
        },
        digests: EvidenceDigests {
            context_manifest_sha256: context_manifest.manifest_sha256.clone(),
            trajectory_sha256: crate::evidence::canonical_json_sha256(trajectory),
            inspection_sha256: crate::evidence::canonical_json_sha256(trajectory_inspection),
        },
        terminal: EvidenceTerminal {
            state,
            outcome,
            final_response_present,
            failed_items,
            pending_items,
            failure_requiredness: if failed_items > 0 {
                "unknown"
            } else {
                "not_applicable"
            },
        },
        redaction_state: "redacted",
        missing_signals,
    }
}

fn prompt_metadata_string(header: &SessionHeader, keys: &[&str]) -> Option<String> {
    json_string(header.prompt_metadata.as_deref(), keys)
}

fn json_string(value: Option<&JsonValue>, keys: &[&str]) -> Option<String> {
    let value = value?;
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(JsonValue::as_str)
            .map(str::trim)
            .filter(|candidate| !candidate.is_empty())
            .map(str::to_string)
    })
}

fn build_coverage(
    counts: &CountSummary,
    prompt_context: &PromptContextSummary,
    context_manifest: &ContextManifestSummary,
) -> ReconstructionCoverage {
    let type_gt0 = |key: &str| counts.by_type.get(key).copied().unwrap_or(0) > 0;
    ReconstructionCoverage {
        prompt_inputs: type_gt0("message.user"),
        assistant_responses: type_gt0("message.assistant"),
        tool_requests: type_gt0("tool.requested"),
        tool_results: type_gt0("tool.completed") || type_gt0("tool.failed"),
        context_manifest: context_manifest.protocol_version.is_some(),
        context_diagnostics: context_manifest.diagnostics > 0,
        file_changes: type_gt0("file.changed"),
        artifacts: type_gt0("artifact.linked"),
        policy_decisions: type_gt0("policy.decision"),
        diagnostics: type_gt0("diagnostic.delta"),
        compactions: type_gt0("compaction.created"),
        pending_requests: type_gt0("wait.pending"),
        mcp_context: context_manifest.mcp_servers > 0 || prompt_context.mcp_servers > 0,
    }
}

fn build_trajectory(
    session: &ParsedSession,
    timeline: &ComposerRunTimeline,
    generated_at: &str,
) -> JsonValue {
    let mut events = Vec::new();
    let mut by_kind = BTreeMap::new();
    let mut by_phase = BTreeMap::new();
    let mut by_status = BTreeMap::new();
    let mut evidence_anchors = 0usize;

    for (index, item) in timeline.items.iter().enumerate() {
        let kind = kind_for_timeline_item(&item.item_type);
        let phase = phase_for_timeline_item(&item.item_type, item.role.as_deref());
        let actor = actor_for_timeline_item(item);
        let mut evidence = vec![json!({"kind": "timeline_item", "id": item.id})];
        if let Some(tool_call_id) = &item.tool_call_id {
            evidence.push(json!({"kind": "tool_call", "id": tool_call_id}));
        }
        evidence_anchors += evidence.len();
        *by_kind.entry(kind.to_string()).or_insert(0) += 1;
        *by_phase.entry(phase.to_string()).or_insert(0) += 1;
        *by_status.entry(item.status.clone()).or_insert(0) += 1;

        let mut event = Map::new();
        event.insert("id".into(), json!(format!("traj:{}", item.id)));
        event.insert("sequence".into(), json!(index + 1));
        event.insert("timestamp".into(), json!(item.timestamp));
        event.insert("kind".into(), json!(kind));
        event.insert("phase".into(), json!(phase));
        event.insert("actor".into(), json!(actor));
        event.insert("type".into(), json!(item.item_type));
        event.insert("status".into(), json!(item.status));
        event.insert("visibility".into(), json!(item.visibility));
        event.insert("source".into(), json!(item.source));
        event.insert("title".into(), json!(item.title));
        if let Some(summary) = &item.summary {
            event.insert("summary".into(), json!(summary));
        }
        if let Some(tool_name) = &item.tool_name {
            event.insert("toolName".into(), json!(tool_name));
        }
        event.insert("evidence".into(), JsonValue::Array(evidence));
        events.push(JsonValue::Object(event));
    }

    json!({
        "schemaVersion": AGENT_TRAJECTORY_SCHEMA,
        "run": {
            "id": session.header.id,
            "sessionId": session.header.id,
            "source": "local",
            "generatedAt": generated_at,
            "platformBacked": false,
        },
        "counts": {
            "events": events.len(),
            "evidenceAnchors": evidence_anchors,
            "byKind": by_kind,
            "byPhase": by_phase,
            "byStatus": by_status,
        },
        "events": events,
    })
}

fn build_trajectory_replay(trajectory: &JsonValue, generated_at: &str) -> JsonValue {
    let events = trajectory
        .get("events")
        .and_then(JsonValue::as_array)
        .map(|a| a.len())
        .unwrap_or(0);
    let arr = trajectory.get("events").and_then(JsonValue::as_array);
    let start = arr
        .and_then(|a| a.first())
        .and_then(|e| e.get("sequence"))
        .cloned();
    let end = arr
        .and_then(|a| a.last())
        .and_then(|e| e.get("sequence"))
        .cloned();
    json!({
        "schemaVersion": AGENT_TRAJECTORY_REPLAY_SCHEMA,
        "run": {
            "id": trajectory.pointer("/run/id").cloned().unwrap_or(json!(null)),
            "sessionId": trajectory.pointer("/run/sessionId").cloned().unwrap_or(json!(null)),
            "source": "local",
            "generatedAt": generated_at,
            "platformBacked": false,
        },
        "counts": { "events": events, "deltas": 0, "errors": 0, "warnings": 0 },
        "deterministic": true,
        "cursor": { "startSequence": start, "endSequence": end },
        "deltas": [],
        "errors": [],
        "warnings": [],
    })
}

/// Deterministic scorer rule (mirrors `AgentTrajectoryScorerRule` in TS).
#[derive(Debug, Clone)]
struct TrajectoryScorerRule {
    id: String,
    severity: String,
    /// Retained for TS parity / future lab rule tables.
    #[allow(dead_code)]
    description: String,
    predicate: TrajectoryScorerPredicate,
}

/// Full predicate set from `scoreAgentTrajectoryReport` (DEFAULT lab uses
/// `FinalEvidenceCoverage` only; other arms keep engine parity for custom rules).
#[derive(Debug, Clone)]
#[allow(dead_code)]
enum TrajectoryScorerPredicate {
    AnyEvent {
        kind: Option<String>,
        phase: Option<String>,
        event_type: Option<String>,
        status: Option<String>,
        tool_name: Option<String>,
        source: Option<String>,
    },
    ForbidEvent {
        kind: Option<String>,
        phase: Option<String>,
        event_type: Option<String>,
        status: Option<String>,
        tool_name: Option<String>,
        source: Option<String>,
    },
    ToolTerminalStatus {
        tool_call_id: String,
        status: String,
    },
    RequireArtifact {
        tool_call_id: String,
        artifact_id: String,
    },
    ApprovalBeforeToolResult {
        tool_call_id: String,
    },
    RecoveryAfterFailedTool {
        tool_call_id: String,
    },
    ChildRunCompleted {
        parent_agent_run_id: String,
        child_agent_run_id: String,
    },
    FinalEvidenceCoverage,
}

/// DEFAULT lab rules from `agent-trajectory-replay-lab.ts`.
fn default_agent_trajectory_replay_lab_rules() -> Vec<TrajectoryScorerRule> {
    vec![TrajectoryScorerRule {
        id: "final-event-has-evidence".into(),
        severity: "error".into(),
        description: "The final answer or runtime terminal event must have evidence.".into(),
        predicate: TrajectoryScorerPredicate::FinalEvidenceCoverage,
    }]
}

fn event_str<'a>(event: &'a JsonValue, key: &str) -> Option<&'a str> {
    event.get(key).and_then(JsonValue::as_str)
}

fn event_u64(event: &JsonValue, key: &str) -> Option<u64> {
    event.get(key).and_then(JsonValue::as_u64)
}

fn event_evidence(event: &JsonValue) -> &[JsonValue] {
    event
        .get("evidence")
        .and_then(JsonValue::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn evidence_ids(event: &JsonValue, kind: &str) -> Vec<String> {
    event_evidence(event)
        .iter()
        .filter_map(|anchor| {
            let anchor_kind = anchor.get("kind").and_then(JsonValue::as_str)?;
            if anchor_kind != kind {
                return None;
            }
            anchor
                .get("id")
                .and_then(JsonValue::as_str)
                .map(str::to_string)
        })
        .collect()
}

fn event_matches_selector(
    event: &JsonValue,
    kind: &Option<String>,
    phase: &Option<String>,
    event_type: &Option<String>,
    status: &Option<String>,
    tool_name: &Option<String>,
    source: &Option<String>,
) -> bool {
    (kind.is_none() || event_str(event, "kind") == kind.as_deref())
        && (phase.is_none() || event_str(event, "phase") == phase.as_deref())
        && (event_type.is_none() || event_str(event, "type") == event_type.as_deref())
        && (status.is_none() || event_str(event, "status") == status.as_deref())
        && (tool_name.is_none() || event_str(event, "toolName") == tool_name.as_deref())
        && (source.is_none() || event_str(event, "source") == source.as_deref())
}

fn events_for_tool<'a>(events: &'a [JsonValue], tool_call_id: &str) -> Vec<&'a JsonValue> {
    events
        .iter()
        .filter(|event| {
            evidence_ids(event, "tool_call")
                .iter()
                .any(|id| id == tool_call_id)
        })
        .collect()
}

fn event_references_id(event: &JsonValue, kind: &str, id: &str) -> bool {
    evidence_ids(event, kind).iter().any(|value| value == id)
}

fn event_is_compatible_with_parent_run(event: &JsonValue, parent_agent_run_id: &str) -> bool {
    let parent_ids = evidence_ids(event, "parent_agent_run");
    parent_ids.is_empty() || parent_ids.iter().any(|id| id == parent_agent_run_id)
}

fn event_references_child_run(event: &JsonValue, child_agent_run_id: &str) -> bool {
    event_references_id(event, "child_agent_run", child_agent_run_id)
        || event_references_id(event, "agent_run", child_agent_run_id)
}

fn events_for_child_run<'a>(
    events: &'a [JsonValue],
    parent_agent_run_id: &str,
    child_agent_run_id: &str,
) -> Vec<&'a JsonValue> {
    events
        .iter()
        .filter(|event| {
            event_references_child_run(event, child_agent_run_id)
                && event_is_compatible_with_parent_run(event, parent_agent_run_id)
        })
        .collect()
}

fn merge_evidence(events: &[&JsonValue]) -> Vec<JsonValue> {
    let mut seen = BTreeSet::new();
    let mut anchors = Vec::new();
    for event in events {
        for anchor in event_evidence(event) {
            let kind = anchor.get("kind").and_then(JsonValue::as_str).unwrap_or("");
            let id = anchor.get("id").and_then(JsonValue::as_str).unwrap_or("");
            let key = format!("{kind}:{id}");
            if !seen.insert(key) {
                continue;
            }
            anchors.push(anchor.clone());
        }
    }
    anchors.sort_by(|a, b| {
        let ak = a.get("kind").and_then(JsonValue::as_str).unwrap_or("");
        let bk = b.get("kind").and_then(JsonValue::as_str).unwrap_or("");
        match ak.cmp(bk) {
            std::cmp::Ordering::Equal => {
                let aid = a.get("id").and_then(JsonValue::as_str).unwrap_or("");
                let bid = b.get("id").and_then(JsonValue::as_str).unwrap_or("");
                aid.cmp(bid)
            }
            other => other,
        }
    });
    anchors
}

fn pass_finding(rule: &TrajectoryScorerRule, message: &str, events: &[&JsonValue]) -> JsonValue {
    json!({
        "ruleId": rule.id,
        "status": "pass",
        "severity": rule.severity,
        "message": message,
        "eventIds": events.iter().filter_map(|e| event_str(e, "id")).collect::<Vec<_>>(),
        "evidence": merge_evidence(events),
        "remediation": "No action required.",
    })
}

fn fail_finding(
    rule: &TrajectoryScorerRule,
    message: &str,
    remediation: &str,
    events: &[&JsonValue],
) -> JsonValue {
    let status = if rule.severity == "warning" {
        "warn"
    } else {
        "fail"
    };
    json!({
        "ruleId": rule.id,
        "status": status,
        "severity": rule.severity,
        "message": message,
        "eventIds": events.iter().filter_map(|e| event_str(e, "id")).collect::<Vec<_>>(),
        "evidence": merge_evidence(events),
        "remediation": remediation,
    })
}

fn terminal_status_for_tool(events: &[&JsonValue]) -> Option<&'static str> {
    for event in events.iter().rev() {
        match event_str(event, "type") {
            Some("tool.completed") => return Some("completed"),
            Some("tool.failed") => return Some("failed"),
            _ => {}
        }
    }
    None
}

fn score_rule(events: &[JsonValue], rule: &TrajectoryScorerRule) -> JsonValue {
    match &rule.predicate {
        TrajectoryScorerPredicate::AnyEvent {
            kind,
            phase,
            event_type,
            status,
            tool_name,
            source,
        } => {
            let matches: Vec<&JsonValue> = events
                .iter()
                .filter(|event| {
                    event_matches_selector(
                        event, kind, phase, event_type, status, tool_name, source,
                    )
                })
                .collect();
            if matches.is_empty() {
                fail_finding(
                    rule,
                    &format!("No trajectory event matched required selector {}.", rule.id),
                    "Add or preserve a trajectory event matching this required behavior.",
                    &[],
                )
            } else {
                pass_finding(
                    rule,
                    &format!("Matched required event selector {}.", rule.id),
                    &matches,
                )
            }
        }
        TrajectoryScorerPredicate::ForbidEvent {
            kind,
            phase,
            event_type,
            status,
            tool_name,
            source,
        } => {
            let matches: Vec<&JsonValue> = events
                .iter()
                .filter(|event| {
                    event_matches_selector(
                        event, kind, phase, event_type, status, tool_name, source,
                    )
                })
                .collect();
            if matches.is_empty() {
                pass_finding(
                    rule,
                    &format!("No forbidden event matched selector {}.", rule.id),
                    &[],
                )
            } else {
                fail_finding(
                    rule,
                    &format!("Found {} forbidden trajectory event(s).", matches.len()),
                    "Remove the forbidden action or update the scenario policy if it is intentionally allowed.",
                    &matches,
                )
            }
        }
        TrajectoryScorerPredicate::ToolTerminalStatus {
            tool_call_id,
            status,
        } => {
            let tool_events = events_for_tool(events, tool_call_id);
            let observed = terminal_status_for_tool(&tool_events);
            if observed == Some(status.as_str()) {
                pass_finding(
                    rule,
                    &format!("Tool {tool_call_id} reached {}.", observed.unwrap()),
                    &tool_events,
                )
            } else {
                fail_finding(
                    rule,
                    &format!(
                        "Tool {tool_call_id} reached {}; expected {status}.",
                        observed.unwrap_or("no terminal status")
                    ),
                    "Preserve the expected terminal tool outcome or update the scenario expectation.",
                    &tool_events,
                )
            }
        }
        TrajectoryScorerPredicate::RequireArtifact {
            tool_call_id,
            artifact_id,
        } => {
            let tool_events = events_for_tool(events, tool_call_id);
            let matches: Vec<&JsonValue> = tool_events
                .iter()
                .copied()
                .filter(|event| {
                    evidence_ids(event, "artifact")
                        .iter()
                        .any(|id| id == artifact_id)
                })
                .collect();
            if matches.is_empty() {
                fail_finding(
                    rule,
                    &format!(
                        "Tool {tool_call_id} did not produce required artifact {artifact_id}."
                    ),
                    "Ensure the run links the required artifact before completion.",
                    &tool_events,
                )
            } else {
                pass_finding(
                    rule,
                    &format!("Tool {tool_call_id} produced required artifact {artifact_id}."),
                    &matches,
                )
            }
        }
        TrajectoryScorerPredicate::ApprovalBeforeToolResult { tool_call_id } => {
            let tool_events = events_for_tool(events, tool_call_id);
            let approval = tool_events.iter().copied().find(|event| {
                event_str(event, "type") == Some("wait.pending")
                    && !evidence_ids(event, "approval_request").is_empty()
            });
            let result = tool_events.iter().copied().find(|event| {
                matches!(
                    event_str(event, "type"),
                    Some("tool.completed" | "tool.failed")
                )
            });
            match (approval, result) {
                (Some(approval), Some(result))
                    if event_u64(approval, "sequence").unwrap_or(0)
                        < event_u64(result, "sequence").unwrap_or(0) =>
                {
                    pass_finding(
                        rule,
                        &format!("Approval wait preceded tool result for {tool_call_id}."),
                        &[approval, result],
                    )
                }
                _ => fail_finding(
                    rule,
                    &format!(
                        "Tool {tool_call_id} did not show approval evidence before terminal result."
                    ),
                    "Emit approval wait evidence before resuming or failing the governed tool call.",
                    &tool_events,
                ),
            }
        }
        TrajectoryScorerPredicate::RecoveryAfterFailedTool { tool_call_id } => {
            let tool_events = events_for_tool(events, tool_call_id);
            let failed = tool_events
                .iter()
                .copied()
                .find(|event| event_str(event, "type") == Some("tool.failed"));
            let recovery = failed.and_then(|failed_event| {
                let failed_seq = event_u64(failed_event, "sequence").unwrap_or(0);
                tool_events.iter().copied().find(|event| {
                    event_str(event, "phase") == Some("recover")
                        && event_u64(event, "sequence").unwrap_or(0) > failed_seq
                })
            });
            match (failed, recovery) {
                (Some(failed), Some(recovery)) => pass_finding(
                    rule,
                    &format!("Recovery followed failed tool {tool_call_id}."),
                    &[failed, recovery],
                ),
                _ => fail_finding(
                    rule,
                    &format!("No recovery event followed failed tool {tool_call_id}."),
                    "Emit a recovery-phase event after the failed tool result or mark the scenario as non-recoverable.",
                    &tool_events,
                ),
            }
        }
        TrajectoryScorerPredicate::ChildRunCompleted {
            parent_agent_run_id,
            child_agent_run_id,
        } => {
            let child_events =
                events_for_child_run(events, parent_agent_run_id, child_agent_run_id);
            let started = child_events
                .iter()
                .copied()
                .find(|event| event_str(event, "type") == Some("agent.run.started"));
            let completed = child_events
                .iter()
                .copied()
                .find(|event| event_str(event, "type") == Some("agent.run.completed"));
            match (started, completed) {
                (Some(started), Some(completed))
                    if event_u64(started, "sequence").unwrap_or(0)
                        < event_u64(completed, "sequence").unwrap_or(0) =>
                {
                    pass_finding(
                        rule,
                        &format!(
                            "Child agent run {child_agent_run_id} completed under parent {parent_agent_run_id}."
                        ),
                        &[started, completed],
                    )
                }
                _ => fail_finding(
                    rule,
                    &format!(
                        "Child agent run {child_agent_run_id} did not complete under parent {parent_agent_run_id}."
                    ),
                    "Preserve child-run start and completion events with agent_run evidence, and include parent_agent_run/child_agent_run anchors when the timeline source provides them.",
                    &child_events,
                ),
            }
        }
        TrajectoryScorerPredicate::FinalEvidenceCoverage => {
            let final_event = events.last();
            let has_evidence = final_event
                .map(|event| {
                    event_evidence(event).iter().any(|anchor| {
                        // truthy check equivalent to TS `evidence.some(Boolean)`
                        !anchor.is_null()
                    })
                })
                .unwrap_or(false);
            if has_evidence {
                let refs: Vec<&JsonValue> = final_event.into_iter().collect();
                pass_finding(rule, "Final trajectory event has evidence anchors.", &refs)
            } else {
                let refs: Vec<&JsonValue> = final_event.into_iter().collect();
                fail_finding(
                    rule,
                    "Final trajectory event is missing evidence anchors.",
                    "Keep a timeline anchor on the final answer or terminal runtime event.",
                    &refs,
                )
            }
        }
    }
}

/// Port of `scoreAgentTrajectoryReport` from `agent-trajectory-scorers.ts`.
fn score_agent_trajectory_report(
    trajectory: &JsonValue,
    rules: &[TrajectoryScorerRule],
) -> JsonValue {
    let events = trajectory
        .get("events")
        .and_then(JsonValue::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let findings: Vec<JsonValue> = rules.iter().map(|rule| score_rule(events, rule)).collect();
    let failed = findings
        .iter()
        .filter(|f| f.get("status").and_then(JsonValue::as_str) == Some("fail"))
        .count();
    let warnings = findings
        .iter()
        .filter(|f| f.get("status").and_then(JsonValue::as_str) == Some("warn"))
        .count();
    let passed = findings
        .iter()
        .filter(|f| f.get("status").and_then(JsonValue::as_str) == Some("pass"))
        .count();
    json!({
        "schemaVersion": AGENT_TRAJECTORY_SCORE_SCHEMA,
        "trajectorySchemaVersion": trajectory
            .get("schemaVersion")
            .cloned()
            .unwrap_or_else(|| json!(AGENT_TRAJECTORY_SCHEMA)),
        "run": trajectory.get("run").cloned().unwrap_or(json!({})),
        "counts": {
            "rules": rules.len(),
            "passed": passed,
            "failed": failed,
            "warnings": warnings,
        },
        "findings": findings,
    })
}

fn redacted_evidence(evidence: &[JsonValue]) -> Vec<JsonValue> {
    let mut anchors: Vec<JsonValue> = evidence
        .iter()
        .map(|anchor| {
            let kind = anchor.get("kind").and_then(JsonValue::as_str).unwrap_or("");
            let id = anchor.get("id").and_then(JsonValue::as_str).unwrap_or("");
            let mut redacted = Map::new();
            if let Some(obj) = anchor.as_object() {
                for (k, v) in obj {
                    redacted.insert(k.clone(), v.clone());
                }
            }
            redacted.insert("redacted".into(), json!(true));
            redacted.insert("label".into(), json!(format!("{kind}:{id}")));
            JsonValue::Object(redacted)
        })
        .collect();
    anchors.sort_by(|a, b| {
        let ak = a.get("kind").and_then(JsonValue::as_str).unwrap_or("");
        let bk = b.get("kind").and_then(JsonValue::as_str).unwrap_or("");
        match ak.cmp(bk) {
            std::cmp::Ordering::Equal => {
                let aid = a.get("id").and_then(JsonValue::as_str).unwrap_or("");
                let bid = b.get("id").and_then(JsonValue::as_str).unwrap_or("");
                aid.cmp(bid)
            }
            other => other,
        }
    });
    anchors
}

fn timeline_item_ids_for_evidence(evidence: &[JsonValue]) -> Vec<String> {
    let mut ids: Vec<String> = evidence
        .iter()
        .filter_map(|anchor| {
            if anchor.get("kind").and_then(JsonValue::as_str) != Some("timeline_item") {
                return None;
            }
            anchor
                .get("id")
                .and_then(JsonValue::as_str)
                .map(str::to_string)
        })
        .collect();
    ids.sort();
    ids
}

fn metadata_keys(metadata: &Option<JsonValue>) -> Vec<String> {
    match metadata {
        Some(JsonValue::Object(map)) => {
            let mut keys: Vec<String> = map.keys().cloned().collect();
            keys.sort();
            keys
        }
        _ => Vec::new(),
    }
}

fn inspect_timeline_item(item: &TimelineItem) -> JsonValue {
    let mut out = Map::new();
    out.insert("id".into(), json!(item.id));
    out.insert("timestamp".into(), json!(item.timestamp));
    out.insert("type".into(), json!(item.item_type));
    out.insert("status".into(), json!(item.status));
    out.insert("visibility".into(), json!(item.visibility));
    out.insert("source".into(), json!(item.source));
    out.insert("title".into(), json!(item.title));
    if let Some(summary) = &item.summary {
        out.insert("summary".into(), json!(summary));
    }
    if let Some(role) = &item.role {
        out.insert("role".into(), json!(role));
    }
    if let Some(tool_name) = &item.tool_name {
        out.insert("toolName".into(), json!(tool_name));
    }
    out.insert("metadataKeys".into(), json!(metadata_keys(&item.metadata)));
    out.insert("redacted".into(), json!(true));
    JsonValue::Object(out)
}

fn inspect_event(event: &JsonValue) -> JsonValue {
    let evidence = event_evidence(event);
    let mut out = Map::new();
    out.insert("id".into(), event.get("id").cloned().unwrap_or(json!(null)));
    out.insert(
        "sequence".into(),
        event.get("sequence").cloned().unwrap_or(json!(null)),
    );
    out.insert(
        "timestamp".into(),
        event.get("timestamp").cloned().unwrap_or(json!(null)),
    );
    out.insert(
        "kind".into(),
        event.get("kind").cloned().unwrap_or(json!(null)),
    );
    out.insert(
        "phase".into(),
        event.get("phase").cloned().unwrap_or(json!(null)),
    );
    out.insert(
        "actor".into(),
        event.get("actor").cloned().unwrap_or(json!(null)),
    );
    out.insert(
        "type".into(),
        event.get("type").cloned().unwrap_or(json!(null)),
    );
    out.insert(
        "status".into(),
        event.get("status").cloned().unwrap_or(json!(null)),
    );
    out.insert(
        "visibility".into(),
        event.get("visibility").cloned().unwrap_or(json!(null)),
    );
    out.insert(
        "source".into(),
        event.get("source").cloned().unwrap_or(json!(null)),
    );
    out.insert(
        "title".into(),
        event.get("title").cloned().unwrap_or(json!(null)),
    );
    if let Some(summary) = event.get("summary") {
        out.insert("summary".into(), summary.clone());
    }
    if let Some(tool_name) = event.get("toolName") {
        out.insert("toolName".into(), tool_name.clone());
    }
    out.insert(
        "relatedIds".into(),
        event
            .get("relatedIds")
            .cloned()
            .unwrap_or_else(|| json!([])),
    );
    out.insert(
        "timelineItemIds".into(),
        json!(timeline_item_ids_for_evidence(evidence)),
    );
    out.insert("evidence".into(), json!(redacted_evidence(evidence)));
    JsonValue::Object(out)
}

fn final_answer_from_events(events: &[JsonValue]) -> Option<JsonValue> {
    let mut reversed: Vec<&JsonValue> = events.iter().collect();
    reversed.reverse();
    let event = reversed
        .iter()
        .find(|candidate| {
            event_str(candidate, "actor") == Some("assistant")
                && event_str(candidate, "type") == Some("message.assistant")
        })
        .or_else(|| {
            reversed.iter().find(|candidate| {
                matches!(event_str(candidate, "phase"), Some("finish" | "recover"))
            })
        })?;
    let mut out = Map::new();
    out.insert(
        "eventId".into(),
        event.get("id").cloned().unwrap_or(json!(null)),
    );
    out.insert(
        "timelineItemIds".into(),
        event
            .get("timelineItemIds")
            .cloned()
            .unwrap_or_else(|| json!([])),
    );
    out.insert(
        "title".into(),
        event.get("title").cloned().unwrap_or(json!("")),
    );
    if let Some(summary) = event.get("summary") {
        out.insert("summary".into(), summary.clone());
    }
    out.insert("redacted".into(), json!(true));
    Some(JsonValue::Object(out))
}

fn timeline_item_ids_for_event_ids(
    event_ids: &[String],
    events_by_id: &BTreeMap<String, &JsonValue>,
) -> Vec<String> {
    let mut ids = BTreeSet::new();
    for event_id in event_ids {
        if let Some(event) = events_by_id.get(event_id) {
            if let Some(arr) = event.get("timelineItemIds").and_then(JsonValue::as_array) {
                for id in arr {
                    if let Some(s) = id.as_str() {
                        ids.insert(s.to_string());
                    }
                }
            }
        }
    }
    ids.into_iter().collect()
}

/// Port of `buildAgentTrajectoryInspectionReport` from `agent-trajectory-inspection.ts`.
fn build_trajectory_inspection(
    timeline: &ComposerRunTimeline,
    trajectory: &JsonValue,
    replay: &JsonValue,
    score: &JsonValue,
) -> JsonValue {
    let raw_events = trajectory
        .get("events")
        .and_then(JsonValue::as_array)
        .cloned()
        .unwrap_or_default();
    let events: Vec<JsonValue> = raw_events.iter().map(inspect_event).collect();
    let final_answer = final_answer_from_events(&events);
    let events_by_id: BTreeMap<String, &JsonValue> = events
        .iter()
        .filter_map(|event| event_str(event, "id").map(|id| (id.to_string(), event)))
        .collect();

    let replay_deltas: Vec<JsonValue> = replay
        .get("deltas")
        .and_then(JsonValue::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|delta| {
            let evidence = delta
                .get("evidence")
                .and_then(JsonValue::as_array)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let timeline_item_ids =
                if let Some(event_id) = delta.get("eventId").and_then(JsonValue::as_str) {
                    timeline_item_ids_for_event_ids(&[event_id.to_string()], &events_by_id)
                } else {
                    timeline_item_ids_for_evidence(evidence)
                };
            let mut out = Map::new();
            out.insert("id".into(), delta.get("id").cloned().unwrap_or(json!(null)));
            out.insert(
                "severity".into(),
                delta.get("severity").cloned().unwrap_or(json!(null)),
            );
            out.insert(
                "ruleId".into(),
                delta.get("ruleId").cloned().unwrap_or(json!(null)),
            );
            out.insert(
                "message".into(),
                delta.get("message").cloned().unwrap_or(json!(null)),
            );
            if let Some(event_id) = delta.get("eventId") {
                out.insert("eventId".into(), event_id.clone());
            }
            out.insert("timelineItemIds".into(), json!(timeline_item_ids));
            out.insert("evidence".into(), json!(redacted_evidence(evidence)));
            JsonValue::Object(out)
        })
        .collect();

    let score_findings: Vec<JsonValue> = score
        .get("findings")
        .and_then(JsonValue::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|finding| {
            let event_ids: Vec<String> = finding
                .get("eventIds")
                .and_then(JsonValue::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let evidence = finding
                .get("evidence")
                .and_then(JsonValue::as_array)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            json!({
                "ruleId": finding.get("ruleId"),
                "status": finding.get("status"),
                "severity": finding.get("severity"),
                "message": finding.get("message"),
                "eventIds": event_ids,
                "timelineItemIds": timeline_item_ids_for_event_ids(&event_ids, &events_by_id),
                "evidence": redacted_evidence(evidence),
                "remediation": finding.get("remediation"),
            })
        })
        .collect();

    let mut jump_targets = BTreeSet::new();
    for event in &events {
        let event_id = event_str(event, "id").unwrap_or("");
        if let Some(arr) = event.get("timelineItemIds").and_then(JsonValue::as_array) {
            for timeline_item_id in arr {
                if let Some(tid) = timeline_item_id.as_str() {
                    jump_targets.insert(format!("{event_id}->{tid}"));
                }
            }
        }
    }
    for delta in &replay_deltas {
        let delta_id = event_str(delta, "id").unwrap_or("");
        if let Some(arr) = delta.get("timelineItemIds").and_then(JsonValue::as_array) {
            for timeline_item_id in arr {
                if let Some(tid) = timeline_item_id.as_str() {
                    jump_targets.insert(format!("{delta_id}->{tid}"));
                }
            }
        }
    }
    for finding in &score_findings {
        let rule_id = finding
            .get("ruleId")
            .and_then(JsonValue::as_str)
            .unwrap_or("");
        if let Some(arr) = finding.get("timelineItemIds").and_then(JsonValue::as_array) {
            for timeline_item_id in arr {
                if let Some(tid) = timeline_item_id.as_str() {
                    jump_targets.insert(format!("{rule_id}->{tid}"));
                }
            }
        }
    }

    let timeline_items: Vec<JsonValue> = timeline.items.iter().map(inspect_timeline_item).collect();
    let score_failures = score
        .pointer("/counts/failed")
        .and_then(JsonValue::as_u64)
        .unwrap_or(0);
    let score_warnings = score
        .pointer("/counts/warnings")
        .and_then(JsonValue::as_u64)
        .unwrap_or(0);

    let mut report = Map::new();
    report.insert(
        "schemaVersion".into(),
        json!(AGENT_TRAJECTORY_INSPECTION_SCHEMA),
    );
    report.insert(
        "trajectorySchemaVersion".into(),
        trajectory
            .get("schemaVersion")
            .cloned()
            .unwrap_or_else(|| json!(AGENT_TRAJECTORY_SCHEMA)),
    );
    report.insert(
        "replaySchemaVersion".into(),
        replay
            .get("schemaVersion")
            .cloned()
            .unwrap_or_else(|| json!(AGENT_TRAJECTORY_REPLAY_SCHEMA)),
    );
    report.insert(
        "scoreSchemaVersion".into(),
        score
            .get("schemaVersion")
            .cloned()
            .unwrap_or_else(|| json!(AGENT_TRAJECTORY_SCORE_SCHEMA)),
    );
    report.insert(
        "run".into(),
        trajectory.get("run").cloned().unwrap_or(json!({})),
    );
    report.insert(
        "redaction".into(),
        json!({
            "default": "redacted",
            "omitted": INSPECTION_OMITTED_FIELDS,
        }),
    );
    report.insert(
        "counts".into(),
        json!({
            "timelineItems": timeline_items.len(),
            "events": events.len(),
            "replayDeltas": replay_deltas.len(),
            "scoreFindings": score_findings.len(),
            "scoreFailures": score_failures,
            "scoreWarnings": score_warnings,
            "jumpTargets": jump_targets.len(),
        }),
    );
    if let Some(final_answer) = final_answer {
        report.insert("finalAnswer".into(), final_answer);
    }
    report.insert("timelineItems".into(), json!(timeline_items));
    report.insert("events".into(), json!(events));
    report.insert("replayDeltas".into(), json!(replay_deltas));
    report.insert("scoreFindings".into(), json!(score_findings));
    JsonValue::Object(report)
}

/// Behavior-version pins recorded in a session's tool-result receipts.
///
/// The bash tool stamps its contract version into `BashDetails.version` on
/// every result (#3089); replay reads it back here so the replay executor
/// can be pinned to the behavior each command was recorded under. Entries
/// without a receipt, with an empty version, or with a version the catalog
/// no longer supports contribute no pin, so they replay under current
/// behavior. When a session recorded more than one version over its
/// lifetime, the most recent receipt wins.
fn recorded_tool_version_pins(session: &ParsedSession) -> BTreeMap<String, String> {
    let mut pins = BTreeMap::new();
    for message in &session.messages {
        let AppMessage::ToolResult {
            receipt: Some(receipt),
            ..
        } = message
        else {
            continue;
        };
        let ToolReceiptDetails::BuiltIn(ToolDetails::Bash(bash)) = &receipt.details else {
            continue;
        };
        let version = bash.version.trim();
        // "current" is the default resolution, so pinning it is a no-op.
        if version.is_empty() || version == "current" || !is_supported_version("bash", version) {
            continue;
        }
        pins.insert("bash".to_string(), version.to_string());
    }
    pins
}

/// Build the executor session replay runs under: same construction as a
/// live run, then pinned to the recorded behavior versions so replayed
/// tool calls reproduce the approval and execution behavior the session
/// was recorded with.
fn replay_tool_executor(session: &ParsedSession, pins: &BTreeMap<String, String>) -> ToolExecutor {
    let cwd = if session.header.cwd.is_empty() {
        "."
    } else {
        session.header.cwd.as_str()
    };
    let mut executor = ToolExecutor::new(cwd).with_code_authority();
    for (tool, version) in pins {
        // Pins come from `recorded_tool_version_pins`, which validates
        // against the version catalog, so pinning cannot fail here.
        executor
            .pin_tool_version(tool, version)
            .expect("recorded tool-version pins are catalog-validated");
    }
    executor
}

fn build_agent_runtime_ledger(
    session: &ParsedSession,
    timeline: &ComposerRunTimeline,
    trajectory: &JsonValue,
    generated_at: &str,
) -> JsonValue {
    let session_id = &session.header.id;
    let mut entries = Vec::new();
    let mut by_kind = BTreeMap::new();
    let mut by_state = BTreeMap::new();
    let traj_events = trajectory
        .get("events")
        .and_then(JsonValue::as_array)
        .cloned()
        .unwrap_or_default();

    for (index, (item, event)) in timeline.items.iter().zip(traj_events.iter()).enumerate() {
        let kind = ledger_kind_for_item(&item.item_type);
        let state = ledger_state_for_status(&item.status);
        *by_kind.entry(kind.to_string()).or_insert(0) += 1;
        *by_state.entry(state.to_string()).or_insert(0) += 1;
        let mut related = Vec::new();
        if let Some(id) = &item.tool_call_id {
            related.push(id.clone());
        }
        entries.push(json!({
            "id": format!("ledger:{}", item.id),
            "sequence": index + 1,
            "timestamp": item.timestamp,
            "kind": kind,
            "phase": event.get("phase").cloned().unwrap_or(json!("finish")),
            "actor": event.get("actor").cloned().unwrap_or(json!("runtime")),
            "type": item.item_type,
            "state": state,
            "title": item.title,
            "visibility": item.visibility,
            "source": item.source,
            "timelineItemId": item.id,
            "trajectoryEventId": event.get("id").cloned().unwrap_or(json!(null)),
            "toolName": item.tool_name,
            "summary": item.summary,
            "relatedIds": related,
            "evidence": event.get("evidence").cloned().unwrap_or_else(|| json!([])),
            "platformShape": { "stepKind": kind, "workItemKind": kind },
        }));
    }

    let idempotency_key = format!("maestro-local-ledger:{session_id}:{session_id}");
    let mut promotion_ops = vec![json!({
        "operation": "handle_trigger",
        "id": format!("promote:{session_id}:trigger"),
        "payload": {
            "sourceEventType": "maestro.local_ledger_promote",
            "sourceEventId": session_id,
            "idempotencyKey": idempotency_key,
            "sessionId": session_id,
            "generatedAt": generated_at,
        }
    })];
    for entry in &entries {
        let entry_id = entry
            .get("id")
            .and_then(JsonValue::as_str)
            .unwrap_or("ledger-entry");
        let kind = entry
            .get("kind")
            .and_then(JsonValue::as_str)
            .unwrap_or("event");
        let state = entry
            .get("state")
            .and_then(JsonValue::as_str)
            .unwrap_or("info");
        let title = entry
            .get("title")
            .cloned()
            .unwrap_or(json!("Runtime event"));
        let timestamp = entry
            .get("timestamp")
            .cloned()
            .unwrap_or_else(|| json!(generated_at));
        promotion_ops.push(json!({
            "operation": "record_run_step",
            "id": format!("promote:{entry_id}:step"),
            "ledgerEntryId": entry_id,
            "payload": {
                "stepId": entry_id,
                "kind": kind,
                "state": promotion_step_state(state),
                "title": title,
                "timestamp": timestamp,
                "toolName": entry.get("toolName").cloned().unwrap_or(JsonValue::Null),
            }
        }));
        promotion_ops.push(json!({
            "operation": "record_run_work_item",
            "id": format!("promote:{entry_id}:work-item"),
            "ledgerEntryId": entry_id,
            "payload": {
                "workItemId": entry_id,
                "kind": kind,
                "state": state,
                "title": title,
                "timestamp": timestamp,
                "evidenceRefs": entry.get("evidence").cloned().unwrap_or_else(|| json!([])),
                "completionGate": "maestro_agent_runtime_ledger_recorded",
                "sessionId": session_id,
                "timelineItemId": entry.get("timelineItemId").cloned().unwrap_or(JsonValue::Null),
            }
        }));
        if entry.get("type").and_then(JsonValue::as_str) == Some("wait.pending") {
            promotion_ops.push(json!({
                "operation": "wait_run",
                "id": format!("promote:{entry_id}:wait"),
                "ledgerEntryId": entry_id,
                "payload": {
                    "waitId": entry_id,
                    "waitType": "external_input",
                    "title": title,
                    "timestamp": timestamp,
                }
            }));
        }
    }
    if let Some(terminal) = entries.iter().rev().find(|entry| {
        !matches!(
            entry.get("state").and_then(JsonValue::as_str),
            Some("info" | "pending" | "running")
        )
    }) {
        let succeeded = matches!(
            terminal.get("state").and_then(JsonValue::as_str),
            Some("succeeded" | "skipped")
        );
        promotion_ops.push(json!({
            "operation": if succeeded { "complete_run" } else { "fail_run" },
            "id": format!("promote:{session_id}:terminal"),
            "payload": {
                "state": if succeeded { "succeeded" } else { "failed" },
                "timestamp": terminal.get("timestamp").cloned().unwrap_or_else(|| json!(generated_at)),
                "ledgerEntryId": terminal.get("id").cloned().unwrap_or(JsonValue::Null),
                "trajectoryEventId": terminal.get("trajectoryEventId").cloned().unwrap_or(JsonValue::Null),
                "eventType": terminal.get("type").cloned().unwrap_or(JsonValue::Null),
                "title": terminal.get("title").cloned().unwrap_or(json!("Local ledger promotion")),
                "evidenceRefs": terminal.get("evidence").cloned().unwrap_or_else(|| json!([])),
            }
        }));
    }

    let tool_version_pins = recorded_tool_version_pins(session);
    let replay_executor = replay_tool_executor(session, &tool_version_pins);
    let mut replayed_bash_commands = 0_u64;
    let mut replayed_bash_requires_approval = 0_u64;
    for message in &session.messages {
        let AppMessage::ToolResult {
            receipt: Some(receipt),
            ..
        } = message
        else {
            continue;
        };
        let ToolReceiptDetails::BuiltIn(ToolDetails::Bash(bash)) = &receipt.details else {
            continue;
        };
        replayed_bash_commands += 1;
        if replay_executor.requires_approval("bash", &json!({ "command": bash.command })) {
            replayed_bash_requires_approval += 1;
        }
    }

    let replay = json!({
        "schemaVersion": AGENT_RUNTIME_REPLAY_SUMMARY_SCHEMA,
        "deterministic": true,
        "events": entries.len(),
        "deltas": 0,
        "errors": 0,
        "warnings": 0,
        "toolVersionPins": tool_version_pins,
        "replayedBashCommands": {
            "total": replayed_bash_commands,
            "requiringApproval": replayed_bash_requires_approval,
        },
        "cursor": {
            "startSequence": entries.first().and_then(|e| e.get("sequence")).cloned(),
            "endSequence": entries.last().and_then(|e| e.get("sequence")).cloned(),
        }
    });

    json!({
        "schemaVersion": AGENT_RUNTIME_LEDGER_SCHEMA,
        "run": {
            "id": session_id,
            "sessionId": session_id,
            "source": "local",
            "generatedAt": generated_at,
            "platformBacked": false,
            "sessionFile": session.file_path,
            "cwd": if session.header.cwd.is_empty() { JsonValue::Null } else { json!(session.header.cwd) },
            "model": if session.header.model.is_empty() { JsonValue::Null } else { json!(session.header.model) },
        },
        "counts": {
            "entries": entries.len(),
            "promotionOperations": promotion_ops.len(),
            "byKind": by_kind,
            "byState": by_state,
        },
        "entries": entries,
        "replay": replay,
        "promotion": {
            "schemaVersion": AGENT_RUNTIME_PROMOTION_PLAN_SCHEMA,
            "runId": session_id,
            "sessionId": session_id,
            "idempotencyKey": idempotency_key,
            "operations": promotion_ops,
            "warnings": ["Promotion plan is dry-run only; no Platform AgentRuntime writes were performed."],
        },
    })
}

fn promotion_step_state(state: &str) -> &'static str {
    match state {
        "succeeded" | "completed" | "skipped" => "completed",
        "failed" | "denied" | "aborted" => "failed",
        "pending" | "waiting" => "pending",
        "running" => "running",
        _ => "completed",
    }
}

fn build_durability(
    session: &SessionReport,
    counts: &CountSummary,
    context_manifest: &ContextManifestSummary,
    agent_runtime_ledger: &JsonValue,
) -> DurabilitySummary {
    let session_file_present = !session.session_file.is_empty();
    let replay_deterministic = agent_runtime_ledger
        .pointer("/replay/deterministic")
        .and_then(JsonValue::as_bool)
        .unwrap_or(false);
    DurabilitySummary {
        reconstructable: session_file_present && counts.timeline_items > 0 && replay_deterministic,
        session_file_present,
        resume_summary_present: session
            .resume_summary
            .as_ref()
            .is_some_and(|s| !s.trim().is_empty()),
        memory_extraction_hash_present: session
            .memory_extraction_hash
            .as_ref()
            .is_some_and(|s| !s.trim().is_empty()),
        context_manifest_present: context_manifest.protocol_version.is_some(),
        compaction_checkpoints: counts
            .by_type
            .get("compaction.created")
            .copied()
            .unwrap_or(0),
        pending_requests: counts.by_type.get("wait.pending").copied().unwrap_or(0),
        agent_runtime_ledger_entries: agent_runtime_ledger
            .pointer("/counts/entries")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0) as usize,
        replay_deterministic,
        promotion_idempotency_key: agent_runtime_ledger
            .pointer("/promotion/idempotencyKey")
            .and_then(JsonValue::as_str)
            .unwrap_or("")
            .to_string(),
    }
}

fn render_run_reconstruction(report: &RunReconstructionReport) -> String {
    let traj_events = report
        .trajectory
        .pointer("/counts/events")
        .and_then(JsonValue::as_u64)
        .unwrap_or(0);
    let mut lines = vec![
        crate::localization::cli_locale().format("Execution evidence: {0} succeeded, {1} failed, {2} denied, {3} cancelled, {4} unknown outcomes, {5} without matching receipts", &[(report.execution_evidence.succeeded).to_string(), (report.execution_evidence.failed).to_string(), (report.execution_evidence.denied).to_string(), (report.execution_evidence.cancelled).to_string(), (report.execution_evidence.indeterminate).to_string(), (report.execution_evidence.unverified).to_string()]),
        crate::localization::cli_locale().format("Recorded changes: {0} file operations; {1} command results", &[(report.execution_evidence.file_changes).to_string(), (report.execution_evidence.commands.len()).to_string()]),
        crate::localization::cli_locale().format("Run reconstruction: {0}", std::slice::from_ref(&(report.session.id))),
        crate::localization::cli_locale().format("Session file: {0}", std::slice::from_ref(&(report.session.session_file))),
        crate::localization::cli_locale().format("Messages: {0}", &[(report.session.message_count).to_string()]),
        crate::localization::cli_locale().format("Timeline items: {0}", &[(report.counts.timeline_items).to_string()]),
        crate::localization::cli_locale().format("Trajectory events: {0}", &[(traj_events).to_string()]),
        crate::localization::cli_locale().format("Replay deltas: {0} ({1} errors, {2} warnings)", &[(u64_at(&report.trajectory_replay, "/counts/deltas")).to_string(), (u64_at(&report.trajectory_replay, "/counts/errors")).to_string(), (u64_at(&report.trajectory_replay, "/counts/warnings")).to_string()]),
        crate::localization::cli_locale().format("Trajectory score: {0} failed, {1} warnings across {2} rule(s)", &[(u64_at(&report.trajectory_score, "/counts/failed")).to_string(), (u64_at(&report.trajectory_score, "/counts/warnings")).to_string(), (u64_at(&report.trajectory_score, "/counts/rules")).to_string()]),
        crate::localization::cli_locale().format("Replay lab: {0} event/source jump target(s), redaction={1}", &[(u64_at(&report.trajectory_inspection, "/counts/jumpTargets")).to_string(), (report
                .trajectory_inspection
                .pointer("/redaction/default")
                .and_then(JsonValue::as_str)
                .unwrap_or("redacted")).to_string()]),
        crate::localization::cli_locale().format("AgentRuntime ledger: {0} entries, {1} dry-run promotion op(s), replay deterministic={2}", &[(u64_at(&report.agent_runtime_ledger, "/counts/entries")).to_string(), (u64_at(&report.agent_runtime_ledger, "/counts/promotionOperations")).to_string(), (yn(report.durability.replay_deterministic)).to_string()]),
        crate::localization::cli_locale().format("Evidence envelope: {0} / {1}, redaction={2}, missing signals={3}", &[(report.evidence_envelope.terminal.state).to_string(), (report.evidence_envelope.terminal.outcome).to_string(), (report.evidence_envelope.redaction_state).to_string(), (report.evidence_envelope.missing_signals.len()).to_string()]),
        crate::localization::cli_locale().format("Durability: reconstructable={0}, resume summary={1}, memory hash={2}, checkpoints={3}, pending waits={4}", &[(yn(report.durability.reconstructable)).to_string(), (yn(report.durability.resume_summary_present)).to_string(), (yn(report.durability.memory_extraction_hash_present)).to_string(), (report.durability.compaction_checkpoints).to_string(), (report.durability.pending_requests).to_string()]),
        crate::localization::cli_locale().format("Coverage: {0}", &[(render_coverage(&report.coverage)).clone()]),
        crate::localization::cli_locale().format("Prompt context: {0} entries ({1} docs, {2} MCP servers)", &[(report.prompt_context.entries).to_string(), (report.prompt_context.project_docs).to_string(), (report.prompt_context.mcp_servers).to_string()]),
        crate::localization::cli_locale().format("Context manifest: {0} entries ({1} docs, {2} MCP servers, {3} resources, {4} prompts, {5} diagnostics)", &[(report.context_manifest.entries).to_string(), (report.context_manifest.project_docs).to_string(), (report.context_manifest.mcp_servers).to_string(), (report.context_manifest.mcp_resources).to_string(), (report.context_manifest.mcp_prompts).to_string(), (report.context_manifest.diagnostics).to_string()]),
        crate::localization::cli_locale().format("Context generation: {0} (declared digest verified={1})", &[(report
                .context_manifest
                .manifest_sha256
                .as_deref()
                .unwrap_or("unavailable")).to_string(), (report
                .context_manifest
                .manifest_sha256_verified
                .map(yn)
                .unwrap_or("not declared")).to_string()]),
        String::new(),
        crate::localization::cli_locale().translate("Timeline preview").into(),
    ];
    for item in report.timeline.items.iter().take(12) {
        let mut parts = vec![
            item.timestamp.clone(),
            item.item_type.clone(),
            item.status.clone(),
            item.title.clone(),
        ];
        if let Some(summary) = &item.summary {
            parts.push(summary.clone());
        }
        lines.push(format!("  - {}", parts.join(" | ")));
    }
    if report.timeline.items.len() > 12 {
        lines.push(crate::localization::cli_locale().format(
            "  ... {0} more item(s)",
            &[(report.timeline.items.len() - 12).to_string()],
        ));
    }
    if report.execution_evidence.indeterminate > 0 {
        lines.push(
            crate::localization::cli_locale()
                .translate("Reconcile unknown remote outcomes before retrying those calls.")
                .into(),
        );
    }
    if report.execution_evidence.failed
        + report.execution_evidence.denied
        + report.execution_evidence.cancelled
        > 0
    {
        lines.push(
            crate::localization::cli_locale()
                .translate("Inspect the recorded failures or denials before resuming the session.")
                .into(),
        );
    }
    lines.join("\n")
}

fn render_context_budget(report: &ContextBudgetReport) -> String {
    let Some(snapshot) = report.latest.as_ref() else {
        return "No prepared-request context telemetry is recorded for this session.".into();
    };
    let mut lines = vec![
        format!("Context budget · {}", snapshot.model),
        format!(
            "Snapshot: {} · {:?}",
            snapshot.timestamp, snapshot.confidence
        ),
    ];
    if let Some(window) = snapshot.context_window {
        lines.push(format!(
            "[{}] {} / {} tokens budgeted",
            context_budget_bar(snapshot, 48),
            snapshot.occupied_tokens(),
            window
        ));
    } else {
        lines.push("Context window: unknown".into());
    }
    for (label, marker, tokens) in context_budget_rows(snapshot) {
        let pct = snapshot
            .context_window
            .filter(|window| *window > 0)
            .map_or(0.0, |window| tokens as f64 * 100.0 / window as f64);
        lines.push(format!(" {marker} {label:<18} {tokens:>9}  {pct:>5.1}%"));
    }
    lines.push(" S system · D schemas · R results · C conversation · O other".into());
    lines.push(" P response reserve · ! safety margin · . remaining".into());
    if let Some(comparison) = &report.latest_compaction {
        let after = comparison
            .after
            .as_ref()
            .map_or("pending".into(), |snapshot| {
                snapshot.input_tokens().to_string()
            });
        let delta = comparison
            .input_token_delta
            .map_or("pending".into(), |delta| format!("{delta:+}"));
        lines.push(format!(
            "Compaction: {} -> {after} input tokens ({delta})",
            comparison.before.input_tokens()
        ));
    }
    lines.join("\n")
}

fn context_budget_rows(
    snapshot: &maestro_context::ContextBudgetSnapshot,
) -> [(&'static str, char, u64); 8] {
    [
        ("System prompt", 'S', snapshot.system_prompt),
        ("Tool schemas", 'D', snapshot.tool_schemas),
        ("Tool results", 'R', snapshot.tool_results),
        ("Conversation", 'C', snapshot.conversation),
        ("Other", 'O', snapshot.other),
        ("Response reserve", 'P', snapshot.response_reserve),
        ("Safety margin", '!', snapshot.safety_margin),
        ("Remaining", '.', snapshot.remaining_headroom().unwrap_or(0)),
    ]
}

fn context_budget_bar(snapshot: &maestro_context::ContextBudgetSnapshot, width: usize) -> String {
    let rows = context_budget_rows(snapshot);
    let total = snapshot.context_window.unwrap_or_else(|| {
        rows.iter()
            .map(|(_, _, tokens)| *tokens)
            .sum::<u64>()
            .max(1)
    });
    (0..width)
        .map(|cell| {
            let target = (cell as u128 * u128::from(total)) / width.max(1) as u128;
            let mut cumulative = 0u128;
            rows.iter()
                .find_map(|(_, marker, tokens)| {
                    cumulative = cumulative.saturating_add(u128::from(*tokens));
                    (target < cumulative).then_some(*marker)
                })
                .unwrap_or('.')
        })
        .collect()
}

fn render_timeline_swimlane(timeline: &ComposerRunTimeline) -> String {
    const WIDTH: usize = 20;
    let lanes = ["USER", "RUNTIME", "MODEL", "TOOLS", "WORKER"];
    let mut lines = vec![
        format!("Session {} event timeline", timeline.session_id),
        format!(
            "{:<12} {}",
            "TIME",
            lanes
                .iter()
                .map(|lane| format!("{lane:<WIDTH$}"))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        format!("{:-<12} {}", "", "-".repeat((WIDTH + 1) * lanes.len() - 1)),
    ];
    for item in &timeline.items {
        let mut cells = vec![String::new(); lanes.len()];
        let marker = match item.status.as_str() {
            "failed" => "!",
            "cancelled" => "x",
            "running" | "pending" => ">",
            _ => "*",
        };
        cells[timeline_lane(item)] = truncate_cell(&format!("{marker} {}", item.title), WIDTH);
        lines.push(format!(
            "{:<12} {}",
            short_event_time(&item.timestamp),
            cells
                .iter()
                .map(|cell| format!("{cell:<WIDTH$}"))
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }
    lines.join("\n")
}

fn timeline_lane(item: &TimelineItem) -> usize {
    if let Some(lane) = item
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("lane"))
        .and_then(JsonValue::as_str)
    {
        match lane {
            "user" => return 0,
            "runtime" => return 1,
            "model" => return 2,
            "tools" => return 3,
            "worker" => return 4,
            _ => {}
        }
    }
    if item.item_type == "message.user" {
        0
    } else if item.item_type.starts_with("model.") || item.item_type == "message.assistant" {
        2
    } else if item.item_type.starts_with("tool.")
        || matches!(
            item.item_type.as_str(),
            "file.changed" | "artifact.linked" | "diagnostic.delta" | "policy.decision"
        )
    {
        3
    } else if item.item_type.starts_with("worker.") {
        4
    } else {
        1
    }
}

fn short_event_time(timestamp: &str) -> String {
    timestamp
        .split('T')
        .nth(1)
        .unwrap_or(timestamp)
        .trim_end_matches('Z')
        .chars()
        .take(12)
        .collect()
}

fn truncate_cell(value: &str, width: usize) -> String {
    let mut chars = value.chars();
    let mut output = chars.by_ref().take(width).collect::<String>();
    if chars.next().is_some() && width > 0 {
        output.pop();
        output.push('…');
    }
    output
}

fn u64_at(value: &JsonValue, pointer: &str) -> u64 {
    value
        .pointer(pointer)
        .and_then(JsonValue::as_u64)
        .unwrap_or(0)
}

fn render_coverage(coverage: &ReconstructionCoverage) -> String {
    let labels: [(&str, bool); 13] = [
        (
            crate::localization::cli_locale().translate("prompt inputs"),
            coverage.prompt_inputs,
        ),
        (
            crate::localization::cli_locale().translate("assistant responses"),
            coverage.assistant_responses,
        ),
        (
            crate::localization::cli_locale().translate("tool requests"),
            coverage.tool_requests,
        ),
        (
            crate::localization::cli_locale().translate("tool results"),
            coverage.tool_results,
        ),
        (
            crate::localization::cli_locale().translate("context manifest"),
            coverage.context_manifest,
        ),
        (
            crate::localization::cli_locale().translate("context diagnostics"),
            coverage.context_diagnostics,
        ),
        (
            crate::localization::cli_locale().translate("file changes"),
            coverage.file_changes,
        ),
        ("artifacts", coverage.artifacts),
        (
            crate::localization::cli_locale().translate("policy decisions"),
            coverage.policy_decisions,
        ),
        ("diagnostics", coverage.diagnostics),
        ("compactions", coverage.compactions),
        (
            crate::localization::cli_locale().translate("pending waits"),
            coverage.pending_requests,
        ),
        (
            crate::localization::cli_locale().translate("MCP context"),
            coverage.mcp_context,
        ),
    ];
    labels
        .iter()
        .map(|(label, present)| format!("{} {label}", if *present { "yes" } else { "no" }))
        .collect::<Vec<_>>()
        .join(", ")
}

fn yn(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn compact_summary(text: Option<&str>) -> Option<String> {
    let text = text?.trim();
    if text.is_empty() {
        return None;
    }
    let chars: Vec<char> = text.chars().collect();
    if chars.len() > 160 {
        Some(format!("{}…", chars[..157].iter().collect::<String>()))
    } else {
        Some(text.to_string())
    }
}

fn millis_to_rfc3339(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let nsecs = ((ms % 1000) * 1_000_000) as u32;
    chrono::DateTime::from_timestamp(secs, nsecs)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

fn thinking_level_label(level: ThinkingLevel) -> String {
    level.label().to_lowercase()
}

fn parse_mcp_tool_name(name: &str) -> Option<String> {
    let rest = name.strip_prefix("mcp__")?;
    let (server, _) = rest.split_once("__")?;
    if server.is_empty() {
        None
    } else {
        Some(server.to_string())
    }
}

fn kind_for_timeline_item(item_type: &str) -> &'static str {
    if item_type.starts_with("session.") {
        "session"
    } else if item_type.starts_with("message.") {
        "message"
    } else if item_type.starts_with("tool.") {
        "tool"
    } else if item_type.starts_with("file.") || item_type.starts_with("diagnostic.") {
        "evidence"
    } else if item_type.starts_with("policy.") {
        "governance"
    } else if item_type == "wait.pending" {
        "wait"
    } else if item_type.starts_with("agent.run.") {
        "agent"
    } else if item_type.starts_with("artifact.") {
        "artifact"
    } else if item_type.starts_with("compaction.")
        || item_type.starts_with("branch.")
        || item_type.starts_with("model.")
        || item_type.starts_with("thinking.")
    {
        "context"
    } else {
        "runtime"
    }
}

fn phase_for_timeline_item(item_type: &str, role: Option<&str>) -> &'static str {
    match kind_for_timeline_item(item_type) {
        "session" | "context" => "setup",
        "message" => {
            if role == Some("assistant") {
                "think"
            } else {
                "observe"
            }
        }
        "tool" => {
            if item_type == "tool.requested" {
                "act"
            } else {
                "verify"
            }
        }
        "agent" => {
            if item_type == "agent.run.started" {
                "act"
            } else {
                "verify"
            }
        }
        "evidence" | "artifact" => "verify",
        "governance" => "govern",
        "wait" => "wait",
        _ => "finish",
    }
}

fn actor_for_timeline_item(item: &TimelineItem) -> &'static str {
    match item.role.as_deref() {
        Some("user") => return "user",
        Some("assistant") => return "assistant",
        Some("tool") => return "tool",
        _ => {}
    }
    if item.item_type == "tool.requested" {
        return "assistant";
    }
    if item.item_type.starts_with("agent.run.") {
        return "agent";
    }
    if item.item_type.starts_with("session.")
        || item.item_type.starts_with("compaction.")
        || item.item_type.starts_with("branch.")
        || item.item_type.starts_with("model.")
        || item.item_type.starts_with("thinking.")
    {
        return "system";
    }
    "runtime"
}

fn ledger_kind_for_item(item_type: &str) -> &'static str {
    match kind_for_timeline_item(item_type) {
        "session" => "run",
        "message" => "message",
        "tool" if item_type == "tool.requested" => "tool_call",
        "tool" => "tool_result",
        "wait" => "wait",
        "context" if item_type.starts_with("compaction.") => "checkpoint",
        "context" => "context",
        "artifact" => "artifact",
        "evidence" => "evidence",
        "governance" => "governance",
        "agent" => "child_run",
        _ => "runtime",
    }
}

fn ledger_state_for_status(status: &str) -> &'static str {
    match status {
        "pending" => "pending",
        "running" => "running",
        "failed" => "failed",
        "completed" | "info" => "succeeded",
        "cancelled" => "cancelled",
        _ => "succeeded",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{
        CompactionEntry, MessageContent, ParsedSession, SessionHeader, SessionMeta, SessionStats,
        ThinkingLevel, ToolInfo,
    };
    use tempfile::TempDir;

    fn budget_snapshot(
        timestamp: &str,
        phase: maestro_context::ContextBudgetPhase,
        conversation: u64,
    ) -> maestro_context::ContextBudgetSnapshot {
        maestro_context::ContextBudgetSnapshot {
            schema_version: maestro_context::CONTEXT_BUDGET_SCHEMA.into(),
            snapshot_id: format!("snapshot-{conversation}"),
            timestamp: timestamp.into(),
            model: "openai/gpt-5.5".into(),
            phase,
            compaction_id: None,
            context_window: Some(1_000),
            system_prompt: 100,
            tool_schemas: 100,
            tool_results: 100,
            conversation,
            other: 50,
            response_reserve: 100,
            safety_margin: 10,
            confidence: maestro_context::BudgetCountConfidence::Measured,
        }
    }

    fn sample_session(dir: &TempDir) -> ParsedSession {
        let path = dir.path().join("sample.jsonl");
        let header = SessionHeader {
            version: Some(2),
            id: "sess-run-1".into(),
            timestamp: "2026-05-09T10:00:00.000Z".into(),
            cwd: "/workspace/app".into(),
            model: "openai/gpt-5.5".into(),
            subject: None,
            model_metadata: None,
            thinking_level: ThinkingLevel::Medium,
            system_prompt: None,
            prompt_metadata: Some(Box::new(json!({
                "agentId": "agent-42",
                "objectiveId": "objective-7"
            }))),
            prompt_context_manifest: Some(Box::new(json!({
                "entries": [{ "path": "/workspace/app/AGENTS.md", "sourceKind": "project" }]
            }))),
            unified_context_manifest: Some(Box::new(json!({
                "protocolVersion": "maestro.unified-context-manifest.v1",
                "version": 1,
                "cwd": "/workspace/app",
                "projectDocs": { "bytesRead": 11 },
                "diagnostics": [{ "code": "warn" }],
                "entries": [
                    { "id": "project_doc:1", "kind": "project_doc", "source": "filesystem", "status": "loaded" },
                    { "id": "mcp_server:platform", "kind": "mcp_server", "source": "mcp_runtime", "status": "connected", "serverName": "platform" },
                    { "id": "mcp_resource:1", "kind": "mcp_resource", "source": "mcp_runtime", "status": "loaded", "serverName": "platform" },
                    { "id": "mcp_prompt:1", "kind": "mcp_prompt", "source": "mcp_runtime", "status": "loaded", "serverName": "platform" }
                ]
            }))),
            tools: vec![ToolInfo {
                name: "mcp__platform__search".into(),
                label: None,
                description: None,
            }],
            branched_from: None,
            parent_session: None,
        };
        let meta = SessionMeta {
            timestamp: "2026-05-09T10:05:00.000Z".into(),
            summary: Some("Docs reconstruction".into()),
            resume_summary: Some("Resume from checkpoint.".into()),
            memory_extraction_hash: Some("sha256:session-memory-hash".into()),
            archived_at: None,
            archived: None,
            title: Some("Run inspect fixture".into()),
            tags: vec![],
            favorite: false,
        };
        ParsedSession {
            selective_summary_context: None,
            header,
            messages: vec![
                AppMessage::User {
                    content: MessageContent::Text("hello".into()),
                    attachments: None,
                    timestamp: 1_715_247_601_000,
                },
                AppMessage::Assistant {
                    content: vec![
                        ContentBlock::Text {
                            text: "working".into(),
                        },
                        ContentBlock::ToolCall {
                            id: "call-1".into(),
                            name: "edit".into(),
                            args: json!({ "path": "README.md" }),
                            contract: None,
                            gemini_context: None,
                        },
                    ],
                    api: None,
                    provider: None,
                    model: Some("openai/gpt-5.5".into()),
                    usage: None,
                    stop_reason: Some("tool_use".into()),
                    timestamp: 1_715_247_602_000,
                },
                AppMessage::ToolResult {
                    tool_call_id: "call-1".into(),
                    tool_name: "edit".into(),
                    content: "ok".into(),
                    details: Some(json!({
                        "path": "README.md",
                        "previousExists": true,
                        "editsApplied": 1,
                        "diagnosticDelta": {
                            "displayPath": "README.md",
                            "introducedCount": 0,
                            "repairedCount": 1,
                            "remainingCount": 0
                        },
                        "skillArtifact": {
                            "name": "docs",
                            "version": "1"
                        },
                        "governedOutcome": {
                            "classification": "allowed",
                            "code": "policy_allow",
                            "decisionId": "decision-9"
                        }
                    })),
                    receipt: None,
                    is_error: false,
                    timestamp: 1_715_247_603_000,
                },
            ],
            meta: Some(meta),
            stats: SessionStats {
                user_messages: 1,
                assistant_messages: 1,
                tool_calls: 1,
                tool_results: 1,
                total_input_tokens: 0,
                total_output_tokens: 0,
                total_cost: 0.0,
            },
            thinking_level_changes: vec![],
            model_changes: vec![],
            compactions: vec![CompactionEntry {
                id: Some("cmp-1".into()),
                parent_id: None,
                timestamp: "2026-05-09T10:04:00.000Z".into(),
                summary: "Compacted history".into(),
                first_kept_entry_id: None,
                first_kept_entry_index: None,
                tokens_before: 300,
                auto: true,
                custom_instructions: None,
                continuation: None,
            }],
            session_events: vec![maestro_runtime_contracts::SessionEvent::new(
                "event-retry",
                "2026-05-09T10:03:00.000Z",
                "sess-run-1",
                maestro_runtime_contracts::SessionEventLane::Runtime,
                "retry.scheduled",
                maestro_runtime_contracts::SessionEventPhase::Info,
            )],
            context_budget_snapshots: vec![
                budget_snapshot(
                    "2026-05-09T10:03:59.000Z",
                    maestro_context::ContextBudgetPhase::BeforeCompaction,
                    400,
                ),
                budget_snapshot(
                    "2026-05-09T10:04:01.000Z",
                    maestro_context::ContextBudgetPhase::PreparedRequest,
                    150,
                ),
            ],
            lifecycle_notifications: vec![],
            pending_lifecycle_agent_notes: vec![],
            side_questions: vec![],
            plan_review_events: vec![],
            usage_entries: vec![],
            file_path: path.display().to_string(),
        }
    }

    #[test]
    fn builds_timeline_and_coverage_from_session_messages() {
        let dir = TempDir::new().unwrap();
        let report = build_report_from_session(&sample_session(&dir), "2026-05-09T10:06:00.000Z");
        let counts = &report.counts;
        assert!(counts.by_type.get("message.user").copied().unwrap_or(0) >= 1);
        assert!(counts.by_type.get("tool.requested").copied().unwrap_or(0) >= 1);
        assert!(counts.by_type.get("tool.completed").copied().unwrap_or(0) >= 1);
        for derived in [
            "file.changed",
            "diagnostic.delta",
            "artifact.linked",
            "policy.decision",
        ] {
            assert_eq!(counts.by_type.get(derived).copied(), Some(1), "{derived}");
        }
        assert!(
            counts
                .by_type
                .get("compaction.created")
                .copied()
                .unwrap_or(0)
                >= 1
        );
        assert_eq!(
            report.context_manifest.protocol_version.as_deref(),
            Some("maestro.unified-context-manifest.v1")
        );
        let operations = report.agent_runtime_ledger["promotion"]["operations"]
            .as_array()
            .expect("promotion operations");
        assert!(
            operations
                .iter()
                .any(|operation| { operation["operation"] == "record_run_step" })
        );
        assert!(
            operations
                .iter()
                .any(|operation| { operation["operation"] == "record_run_work_item" })
        );
        assert_eq!(report.context_manifest.entries, 4);
        assert_eq!(report.context_manifest.mcp_servers, 1);
        assert_eq!(
            report
                .context_manifest
                .manifest_sha256
                .as_deref()
                .map(str::len),
            Some(71)
        );
        assert_eq!(report.context_manifest.manifest_sha256_verified, None);
        assert!(report.coverage.prompt_inputs);
        assert!(report.coverage.tool_results);
        assert!(report.coverage.mcp_context);
    }

    #[test]
    fn inspect_json_includes_schema_version() {
        let dir = TempDir::new().unwrap();
        let report = build_report_from_session(&sample_session(&dir), "2026-05-09T10:06:00.000Z");
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(
            value.get("schemaVersion").and_then(JsonValue::as_str),
            Some(RUN_RECONSTRUCTION_SCHEMA)
        );
        assert_eq!(
            value
                .pointer("/trajectory/schemaVersion")
                .and_then(JsonValue::as_str),
            Some(AGENT_TRAJECTORY_SCHEMA)
        );
        assert_eq!(
            value
                .pointer("/agentRuntimeLedger/promotion/idempotencyKey")
                .and_then(JsonValue::as_str),
            Some("maestro-local-ledger:sess-run-1:sess-run-1")
        );
        assert_eq!(
            value
                .pointer("/evidenceEnvelope/schemaVersion")
                .and_then(JsonValue::as_str),
            Some(DETERMINISTIC_EVIDENCE_ENVELOPE_SCHEMA)
        );
        assert_eq!(
            value
                .pointer("/evidenceEnvelope/sourceRef")
                .and_then(JsonValue::as_str),
            Some("maestro://session/sess-run-1")
        );
        assert_eq!(
            value
                .pointer("/evidenceEnvelope/agentId")
                .and_then(JsonValue::as_str),
            Some("agent-42")
        );
        assert_eq!(
            value
                .pointer("/evidenceEnvelope/objectiveId")
                .and_then(JsonValue::as_str),
            Some("objective-7")
        );
        assert_eq!(
            value
                .pointer("/evidenceEnvelope/policyDecisionIds/0")
                .and_then(JsonValue::as_str),
            Some("decision-9")
        );
        assert_eq!(
            value
                .pointer("/evidenceEnvelope/redactionState")
                .and_then(JsonValue::as_str),
            Some("redacted")
        );
        for pointer in [
            "/evidenceEnvelope/digests/contextManifestSha256",
            "/evidenceEnvelope/digests/trajectorySha256",
            "/evidenceEnvelope/digests/inspectionSha256",
        ] {
            assert_eq!(
                value
                    .pointer(pointer)
                    .and_then(JsonValue::as_str)
                    .map(str::len),
                Some(71),
                "{pointer}"
            );
        }
        let human = render_run_reconstruction(&report);
        assert!(human.contains("Run reconstruction: sess-run-1"));
        assert!(human.contains("Evidence envelope: incomplete / unknown"));
        assert!(human.contains("Timeline preview"));
    }

    #[test]
    fn evidence_terminal_distinguishes_completed_degraded_from_failed() {
        let dir = TempDir::new().unwrap();
        let mut session = sample_session(&dir);
        session.messages.push(AppMessage::ToolResult {
            tool_call_id: "call-2".into(),
            tool_name: "bash".into(),
            content: "optional probe failed".into(),
            details: None,
            receipt: None,
            is_error: true,
            timestamp: 1_715_247_604_000,
        });
        session.messages.push(AppMessage::Assistant {
            content: vec![ContentBlock::Text {
                text: "Finished with the optional probe unavailable.".into(),
            }],
            api: None,
            provider: None,
            model: Some("openai/gpt-5.5".into()),
            usage: None,
            stop_reason: Some("end_turn".into()),
            timestamp: 1_715_247_605_000,
        });

        let report = build_report_from_session(&session, "2026-05-09T10:06:00.000Z");
        assert_eq!(report.evidence_envelope.terminal.state, "completed");
        assert_eq!(report.evidence_envelope.terminal.outcome, "degraded");
        assert_eq!(
            report.evidence_envelope.terminal.failure_requiredness,
            "unknown"
        );
        assert!(
            report
                .evidence_envelope
                .missing_signals
                .contains(&"failure_requiredness".to_string())
        );

        session.messages.pop();
        let report = build_report_from_session(&session, "2026-05-09T10:06:00.000Z");
        assert_eq!(report.evidence_envelope.terminal.state, "failed");
        assert_eq!(report.evidence_envelope.terminal.outcome, "failed");
    }

    #[test]
    fn scores_default_lab_rule_final_event_has_evidence() {
        let dir = TempDir::new().unwrap();
        let report = build_report_from_session(&sample_session(&dir), "2026-05-09T10:06:00.000Z");
        assert_eq!(
            report
                .trajectory_score
                .get("schemaVersion")
                .and_then(JsonValue::as_str),
            Some(AGENT_TRAJECTORY_SCORE_SCHEMA)
        );
        assert_eq!(
            report
                .trajectory_score
                .pointer("/counts/rules")
                .and_then(JsonValue::as_u64),
            Some(1)
        );
        assert_eq!(
            report
                .trajectory_score
                .pointer("/counts/passed")
                .and_then(JsonValue::as_u64),
            Some(1)
        );
        assert_eq!(
            report
                .trajectory_score
                .pointer("/counts/failed")
                .and_then(JsonValue::as_u64),
            Some(0)
        );
        let findings = report
            .trajectory_score
            .get("findings")
            .and_then(JsonValue::as_array)
            .expect("findings");
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].get("ruleId").and_then(JsonValue::as_str),
            Some("final-event-has-evidence")
        );
        assert_eq!(
            findings[0].get("status").and_then(JsonValue::as_str),
            Some("pass")
        );
        assert!(
            findings[0]
                .get("evidence")
                .and_then(JsonValue::as_array)
                .map(|a| !a.is_empty())
                .unwrap_or(false)
        );
        // No residual stub on score once lab rules are live.
        assert!(report.trajectory_score.get("residual").is_none());
    }

    #[test]
    fn inspection_includes_redacted_timeline_score_findings_and_catalog() {
        let dir = TempDir::new().unwrap();
        let report = build_report_from_session(&sample_session(&dir), "2026-05-09T10:06:00.000Z");
        let inspection = &report.trajectory_inspection;
        assert_eq!(
            inspection.get("schemaVersion").and_then(JsonValue::as_str),
            Some(AGENT_TRAJECTORY_INSPECTION_SCHEMA)
        );
        assert_eq!(
            inspection
                .get("scoreSchemaVersion")
                .and_then(JsonValue::as_str),
            Some(AGENT_TRAJECTORY_SCORE_SCHEMA)
        );
        assert_eq!(
            inspection
                .pointer("/redaction/default")
                .and_then(JsonValue::as_str),
            Some("redacted")
        );
        let omitted = inspection
            .pointer("/redaction/omitted")
            .and_then(JsonValue::as_array)
            .expect("omitted catalog");
        for field in [
            "raw prompts",
            "raw tool arguments",
            "raw tool outputs",
            "full file diffs",
            "timeline metadata values",
            "secrets",
        ] {
            assert!(
                omitted.iter().any(|v| v.as_str() == Some(field)),
                "missing omitted field {field}"
            );
        }

        let timeline_items = inspection
            .get("timelineItems")
            .and_then(JsonValue::as_array)
            .expect("timelineItems");
        assert!(!timeline_items.is_empty());
        assert_eq!(
            timeline_items[0]
                .get("redacted")
                .and_then(JsonValue::as_bool),
            Some(true)
        );
        assert!(timeline_items[0].get("metadataKeys").is_some());

        let score_findings = inspection
            .get("scoreFindings")
            .and_then(JsonValue::as_array)
            .expect("scoreFindings");
        assert_eq!(score_findings.len(), 1);
        assert_eq!(
            score_findings[0].get("ruleId").and_then(JsonValue::as_str),
            Some("final-event-has-evidence")
        );
        assert!(score_findings[0].get("timelineItemIds").is_some());
        let finding_evidence = score_findings[0]
            .get("evidence")
            .and_then(JsonValue::as_array)
            .expect("finding evidence");
        assert!(!finding_evidence.is_empty());
        assert_eq!(
            finding_evidence[0]
                .get("redacted")
                .and_then(JsonValue::as_bool),
            Some(true)
        );
        assert!(finding_evidence[0].get("label").is_some());

        assert!(
            inspection
                .pointer("/counts/jumpTargets")
                .and_then(JsonValue::as_u64)
                .unwrap_or(0)
                > 0
        );
        assert!(inspection.get("finalAnswer").is_some());
        assert!(inspection.get("residual").is_none());
    }

    #[test]
    fn final_evidence_coverage_fails_when_last_event_has_no_evidence() {
        let trajectory = json!({
            "schemaVersion": AGENT_TRAJECTORY_SCHEMA,
            "run": { "id": "r1", "sessionId": "r1", "source": "local", "platformBacked": false },
            "events": [{
                "id": "traj:empty",
                "sequence": 1,
                "type": "message.assistant",
                "kind": "message",
                "phase": "finish",
                "actor": "assistant",
                "status": "completed",
                "visibility": "user",
                "source": "local",
                "title": "empty",
                "evidence": []
            }]
        });
        let score = score_agent_trajectory_report(
            &trajectory,
            &default_agent_trajectory_replay_lab_rules(),
        );
        assert_eq!(
            score.pointer("/counts/failed").and_then(JsonValue::as_u64),
            Some(1)
        );
        assert_eq!(
            score
                .pointer("/findings/0/status")
                .and_then(JsonValue::as_str),
            Some("fail")
        );
        assert_eq!(
            score
                .pointer("/findings/0/ruleId")
                .and_then(JsonValue::as_str),
            Some("final-event-has-evidence")
        );
    }

    #[test]
    fn help_mentions_subcommands() {
        let help = run_help();
        for sub in ["inspect", "ledger", "replay", "promote"] {
            assert!(help.contains(sub));
        }
    }

    #[test]
    fn missing_session_returns_error() {
        let err = build_run_reconstruction_report("does-not-exist-zzzz").unwrap_err();
        assert!(err.to_string().contains("Session not found"));
    }

    #[test]
    fn parse_mcp_tool_name_extracts_server() {
        assert_eq!(
            parse_mcp_tool_name("mcp__platform__search").as_deref(),
            Some("platform")
        );
        assert_eq!(parse_mcp_tool_name("read"), None);
    }

    #[test]
    fn run_run_help_exits_zero() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert_eq!(rt.block_on(run_run(&["help".into()])).unwrap(), 0);
    }

    #[test]
    fn run_run_requires_session_id() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert_eq!(rt.block_on(run_run(&["inspect".into()])).unwrap(), 1);
    }

    fn bash_receipt_message(version: &str, command: &str) -> AppMessage {
        let details = crate::tools::details::BashDetails {
            command: command.into(),
            exit_code: 0,
            version: version.into(),
            ..Default::default()
        };
        AppMessage::ToolResult {
            tool_call_id: "call-bash".into(),
            tool_name: "bash".into(),
            content: "ok".into(),
            details: None,
            receipt: Some(crate::agent::ExecutionReceipt {
                code_authority: None,
                call_id: "call-bash".into(),
                tool_name: "bash".into(),
                source: crate::agent::ExecutionSource::Native,
                status: crate::agent::ExecutionStatus::Succeeded,
                duration_ms: Some(1),
                policy: None,
                details: ToolReceiptDetails::BuiltIn(ToolDetails::Bash(details)),
            }),
            is_error: false,
            timestamp: 1_715_247_604_000,
        }
    }

    fn session_with_bash_receipt(dir: &TempDir, version: &str) -> ParsedSession {
        let mut session = sample_session(dir);
        session
            .messages
            .push(bash_receipt_message(version, "cargo check"));
        session
    }

    #[test]
    fn execution_summary_requires_matching_receipts_and_preserves_unknown_outcomes() {
        let dir = TempDir::new().unwrap();
        let mut session = sample_session(&dir);
        session.messages = vec![bash_receipt_message("current", "cargo check")];
        let mut unknown = bash_receipt_message("current", "private command text");
        if let AppMessage::ToolResult {
            tool_call_id,
            receipt: Some(receipt),
            ..
        } = &mut unknown
        {
            *tool_call_id = "unknown-call".into();
            receipt.call_id.clone_from(tool_call_id);
            receipt.status = crate::agent::ExecutionStatus::Indeterminate;
        }
        session.messages.push(unknown);
        let mut mismatched = bash_receipt_message("current", "secret command text");
        if let AppMessage::ToolResult {
            tool_call_id,
            receipt: Some(receipt),
            ..
        } = &mut mismatched
        {
            *tool_call_id = "mismatched-call".into();
            receipt.call_id = "different-call".into();
        }
        session.messages.push(mismatched);
        // Persist and reopen through the production JSONL boundary, including compaction.
        let path = dir.path().join("evidence.jsonl");
        let mut writer =
            crate::session::SessionWriter::create(&path, session.header.clone()).unwrap();
        for (index, message) in session.messages.iter().enumerate() {
            writer
                .write_entry(crate::session::SessionEntry::Message(
                    crate::session::MessageEntry {
                        id: Some(format!("entry-{index}")),
                        parent_id: None,
                        timestamp: "2026-09-04T00:00:00Z".into(),
                        message: message.clone(),
                    },
                ))
                .unwrap();
        }
        for compaction in &session.compactions {
            writer
                .write_entry(crate::session::SessionEntry::Compaction(compaction.clone()))
                .unwrap();
        }
        writer.flush().unwrap();
        drop(writer);
        let session = crate::session::SessionReader::read_file(&path).unwrap();
        let summary = execution_evidence_summary(&session);
        assert_eq!(summary.succeeded, 1);
        assert_eq!(summary.indeterminate, 1);
        assert_eq!(summary.unverified, 1);
        let wire = serde_json::to_string(&summary).unwrap();
        assert!(!wire.contains("command text"));
        let report = build_report_from_session(&session, "2026-09-04T00:00:00Z");
        assert!(render_run_reconstruction(&report).contains("Reconcile unknown remote outcomes"));
    }

    #[test]
    fn execution_summary_counts_replayed_calls_once() {
        let dir = TempDir::new().unwrap();
        let mut session = sample_session(&dir);
        let result = bash_receipt_message("current", "cargo check");
        session.messages = vec![result.clone(), result];
        let summary = execution_evidence_summary(&session);
        assert_eq!(summary.recorded, 1);
        assert_eq!(summary.commands.len(), 1);
        if let AppMessage::ToolResult { is_error, .. } = session.messages.last_mut().unwrap() {
            *is_error = true;
        }
        let summary = execution_evidence_summary(&session);
        assert_eq!(summary.succeeded, 0);
        assert_eq!(summary.unverified, 1);
    }

    #[test]
    fn replay_pins_bash_version_recorded_in_session_receipt() {
        let dir = TempDir::new().unwrap();
        let session = session_with_bash_receipt(&dir, "legacy-1");

        let pins = recorded_tool_version_pins(&session);
        assert_eq!(pins.get("bash").map(String::as_str), Some("legacy-1"));

        // The replay executor replays under legacy approval semantics:
        // `cargo check` was auto-approved under legacy-1 but requires
        // approval under current behavior.
        let executor = replay_tool_executor(&session, &pins);
        assert!(!executor.requires_approval("bash", &json!({ "command": "cargo check" })));

        let report = build_report_from_session(&session, "2026-05-09T10:06:00.000Z");
        let replay = &report.agent_runtime_ledger["replay"];
        assert_eq!(replay["toolVersionPins"]["bash"], json!("legacy-1"));
        assert_eq!(replay["replayedBashCommands"]["total"], json!(1));
        assert_eq!(
            replay["replayedBashCommands"]["requiringApproval"],
            json!(0)
        );
    }

    #[test]
    fn replay_without_recorded_version_uses_current_behavior() {
        let dir = TempDir::new().unwrap();

        // Absent, default, and unknown recorded versions must not pin, so
        // replay falls back to current behavior.
        for version in ["", "current", "legacy-99"] {
            let session = session_with_bash_receipt(&dir, version);
            let pins = recorded_tool_version_pins(&session);
            assert!(pins.is_empty(), "recorded version {version:?} must not pin");

            let executor = replay_tool_executor(&session, &pins);
            assert!(
                executor.requires_approval("bash", &json!({ "command": "cargo check" })),
                "recorded version {version:?} must replay under current approval semantics"
            );

            let report = build_report_from_session(&session, "2026-05-09T10:06:00.000Z");
            let replay = &report.agent_runtime_ledger["replay"];
            assert_eq!(replay["toolVersionPins"], json!({}));
            assert_eq!(
                replay["replayedBashCommands"]["requiringApproval"],
                json!(1)
            );
        }

        // Sessions recorded before tool versioning have no bash receipts
        // at all; nothing is pinned and nothing is reclassified.
        let session = sample_session(&dir);
        assert!(recorded_tool_version_pins(&session).is_empty());
        let report = build_report_from_session(&session, "2026-05-09T10:06:00.000Z");
        let replay = &report.agent_runtime_ledger["replay"];
        assert_eq!(replay["toolVersionPins"], json!({}));
        assert_eq!(replay["replayedBashCommands"]["total"], json!(0));
    }

    #[test]
    fn renders_session_event_swimlane_with_runtime_telemetry() {
        let dir = TempDir::new().unwrap();
        let mut session = sample_session(&dir);
        session
            .session_events
            .push(maestro_runtime_contracts::SessionEvent::new(
                "event-turn",
                "2026-05-09T10:03:01.000Z",
                "sess-run-1",
                maestro_runtime_contracts::SessionEventLane::User,
                "turn.started",
                maestro_runtime_contracts::SessionEventPhase::Started,
            ));
        let report = build_report_from_session(&session, "2026-05-09T10:06:00.000Z");
        let rendered = render_timeline_swimlane(&report.timeline);
        assert!(rendered.contains("USER"));
        assert!(rendered.contains("RUNTIME"));
        assert!(rendered.contains("MODEL"));
        assert!(rendered.contains("TOOLS"));
        assert!(rendered.contains("WORKER"));
        assert!(rendered.contains("Request retry"));
        let turn_row = rendered
            .lines()
            .find(|line| line.contains("Turn started"))
            .unwrap();
        assert!(turn_row[13..33].contains("Turn started"));
        assert!(!rendered.contains("hello"));
    }

    #[test]
    fn gateway_failure_timeline_keeps_request_id_and_failure_kind() {
        use maestro_runtime_contracts::{SessionEvent, SessionEventLane, SessionEventPhase};

        let mut receipt = SessionEvent::new(
            "event-receipt",
            "2026-05-09T10:03:02.000Z",
            "sess-run-1",
            SessionEventLane::Model,
            "model.gateway.receipt",
            SessionEventPhase::Info,
        );
        receipt.correlation_id = Some("request-123".into());
        let mut failure = SessionEvent::new(
            "event-failure",
            "2026-05-09T10:03:03.000Z",
            "sess-run-1",
            SessionEventLane::Model,
            "model.response.failed",
            SessionEventPhase::Failed,
        );
        failure.name = Some("transientprotocol".into());
        let mut retry = SessionEvent::new(
            "event-retry",
            "2026-05-09T10:03:04.000Z",
            "sess-run-1",
            SessionEventLane::Runtime,
            "retry.scheduled",
            SessionEventPhase::Requested,
        );
        retry.name = Some("provider_stream_timeout".into());

        assert_eq!(
            session_event_title(&receipt),
            "Gateway request ID: request-123"
        );
        assert_eq!(
            session_event_title(&failure),
            "Model response failed (transientprotocol)"
        );
        assert_eq!(
            session_event_title(&retry),
            "Request retry scheduled (provider_stream_timeout)"
        );
    }

    #[test]
    fn renders_context_waterfall_and_compaction_delta() {
        let dir = TempDir::new().unwrap();
        let report = build_report_from_session(&sample_session(&dir), "2026-05-09T10:06:00.000Z");
        let rendered = render_context_budget(&report.context_budget);
        assert!(rendered.contains("Context budget"));
        assert!(rendered.contains("Response reserve"));
        assert!(rendered.contains("Safety margin"));
        assert!(rendered.contains("Remaining"));
        assert!(rendered.contains("Compaction: 750 -> 500 input tokens (-250)"));
    }
}
