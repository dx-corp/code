//! Content-free Platform operating-plane ledger summary.
//!
//! Ports `src/platform/operating-plane-summary.ts`. Never forwards prompt text,
//! artifact bodies, or evidence summaries — only operator-safe status fields.

use serde::Serialize;

use crate::operating_plane_client::{
    OperatingPlaneEvidence, OperatingPlaneInspection, OperatingPlaneRun, OperatingPlaneUsage,
};

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct OperatingPlaneStatusReport {
    pub contract_version: String,
    pub generated_at: String,
    pub run_count: usize,
    pub unavailable_sources: Vec<String>,
    pub runs: Vec<OperatingPlaneRunStatus>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct OperatingPlaneRunStatus {
    pub run_id: String,
    pub title: String,
    pub status: String,
    pub surface: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity_subject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operator_summary: Option<String>,
    pub signals_present: Vec<String>,
    pub signals_missing: Vec<String>,
    pub artifact_refs: Vec<OperatingPlaneArtifactStatus>,
    pub next_actions: Vec<String>,
    pub blockers: Vec<String>,
    pub withheld: Vec<String>,
    pub usage: OperatingPlaneUsageStatus,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OperatingPlaneArtifactStatus {
    pub id: String,
    pub source: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    pub available: bool,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OperatingPlaneUsageStatus {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_cost_micros: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
}

pub fn summarize_operating_plane_inspection(
    inspection: &OperatingPlaneInspection,
    max_runs: Option<usize>,
) -> OperatingPlaneStatusReport {
    let max_runs = normalize_positive_limit(max_runs);
    let runs: Vec<_> = match max_runs {
        Some(limit) => inspection.runs.iter().take(limit).collect(),
        None => inspection.runs.iter().collect(),
    };
    OperatingPlaneStatusReport {
        contract_version: inspection.contract_version.clone(),
        generated_at: inspection.generated_at.clone(),
        run_count: inspection.runs.len(),
        unavailable_sources: unique_strings(
            inspection
                .unavailable_sources
                .iter()
                .flatten()
                .map(|value| Some(value.clone())),
        ),
        runs: runs
            .into_iter()
            .map(summarize_operating_plane_run)
            .collect(),
    }
}

pub fn format_operating_plane_status_report(report: &OperatingPlaneStatusReport) -> String {
    let mut lines = vec![
        format!(
            "Agent operating-plane status ({} {})",
            report.run_count,
            pluralize("run", report.run_count)
        ),
        format!("Generated: {}", report.generated_at),
    ];
    if !report.unavailable_sources.is_empty() {
        lines.push(format!(
            "Unavailable sources: {}",
            report.unavailable_sources.join(", ")
        ));
    }
    if report.runs.is_empty() {
        lines.push("No operating-plane runs matched the query.".to_owned());
        return lines.join("\n");
    }

    for run in &report.runs {
        lines.push(format!(
            "- {} [{}] on {} ({})",
            if run.title.is_empty() {
                run.run_id.as_str()
            } else {
                run.title.as_str()
            },
            if run.status.is_empty() {
                "unknown"
            } else {
                run.status.as_str()
            },
            if run.surface.is_empty() {
                "unknown"
            } else {
                run.surface.as_str()
            },
            run.run_id
        ));
        push_line(&mut lines, "  Summary", run.operator_summary.as_deref());
        push_line(&mut lines, "  Thread", run.channel_thread_id.as_deref());
        push_line(&mut lines, "  Trace", run.trace_id.as_deref());
        push_line(&mut lines, "  Identity", run.identity_subject.as_deref());
        if !run.signals_present.is_empty() {
            lines.push(format!(
                "  Signals present: {}",
                run.signals_present.join(", ")
            ));
        }
        if !run.signals_missing.is_empty() {
            lines.push(format!(
                "  Signals missing: {}",
                run.signals_missing.join(", ")
            ));
        }
        if !run.artifact_refs.is_empty() {
            lines.push(format!(
                "  Artifacts: {}",
                run.artifact_refs
                    .iter()
                    .map(format_artifact_ref)
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
        }
        for next_action in &run.next_actions {
            lines.push(format!("  Next action: {next_action}"));
        }
        for blocker in &run.blockers {
            lines.push(format!("  Blocker: {blocker}"));
        }
        if !run.withheld.is_empty() {
            lines.push(format!(
                "  Withheld/out of scope: {}",
                run.withheld.join(", ")
            ));
        }
        if let Some(usage) = format_usage(&run.usage) {
            lines.push(format!("  Usage: {usage}"));
        }
    }

    lines.join("\n")
}

fn summarize_operating_plane_run(run: &OperatingPlaneRun) -> OperatingPlaneRunStatus {
    let (signals_present, signals_missing) = summarize_value_signals(run);
    OperatingPlaneRunStatus {
        run_id: run.agent_run_id.clone(),
        title: one_line(Some(&run.title)).unwrap_or_default(),
        status: one_line(Some(&run.status)).unwrap_or_default(),
        surface: one_line(Some(&run.surface)).unwrap_or_default(),
        channel_thread_id: one_line(run.channel_thread_id.as_deref()),
        trace_id: one_line(run.trace_id.as_deref()),
        identity_subject: operating_plane_identity_subject(run),
        operator_summary: one_line(
            run.runtime_signals
                .as_ref()
                .and_then(|signals| signals.operator_summary.as_deref()),
        ),
        signals_present,
        signals_missing,
        artifact_refs: summarize_artifact_refs(run.evidence_refs.as_deref()),
        next_actions: unique_strings(
            run.work_items
                .iter()
                .flatten()
                .map(|item| one_line(item.next_action.as_deref())),
        ),
        blockers: unique_strings(
            run.work_items
                .iter()
                .flatten()
                .map(|item| one_line(item.blocker.as_deref())),
        ),
        withheld: operating_plane_withheld_reasons(run),
        usage: summarize_usage(run.usage.as_ref()),
    }
}

fn summarize_value_signals(run: &OperatingPlaneRun) -> (Vec<String>, Vec<String>) {
    let signals = run.runtime_signals.as_ref();
    let fields: [(&str, Option<bool>); 7] = [
        ("identity", signals.and_then(|s| s.identity_bound)),
        ("model", signals.and_then(|s| s.model_observed)),
        ("tool", signals.and_then(|s| s.tool_observed)),
        ("approval", signals.and_then(|s| s.approval_observed)),
        ("trace", signals.and_then(|s| s.trace_linked)),
        ("artifact", signals.and_then(|s| s.evidence_linked)),
        ("cost", signals.and_then(|s| s.cost_attributed)),
    ];
    let signals_present = fields
        .iter()
        .filter(|(_, value)| *value == Some(true))
        .map(|(label, _)| (*label).to_owned())
        .collect();
    let mut missing: Vec<Option<String>> = fields
        .iter()
        .filter(|(_, value)| *value != Some(true))
        .map(|(label, _)| Some((*label).to_owned()))
        .collect();
    if let Some(signals) = signals {
        if let Some(extra) = &signals.missing_signals {
            for value in extra {
                missing.push(operator_facing_signal_label(value));
            }
        }
    } else {
        missing.push(Some("runtime signal unavailable".to_owned()));
    }
    (signals_present, unique_strings(missing))
}

fn summarize_artifact_refs(
    evidence_refs: Option<&[OperatingPlaneEvidence]>,
) -> Vec<OperatingPlaneArtifactStatus> {
    evidence_refs
        .unwrap_or(&[])
        .iter()
        .filter_map(|evidence| {
            let id = one_line(Some(&evidence.id)).filter(|value| !value.is_empty())?;
            Some(OperatingPlaneArtifactStatus {
                id,
                source: one_line(Some(&evidence.source)).unwrap_or_default(),
                kind: one_line(Some(&evidence.kind)).unwrap_or_default(),
                uri: one_line(evidence.uri.as_deref()),
                revision: one_line(evidence.revision.as_deref()),
                available: evidence.available,
            })
        })
        .collect()
}

fn summarize_usage(usage: Option<&OperatingPlaneUsage>) -> OperatingPlaneUsageStatus {
    let Some(usage) = usage else {
        return OperatingPlaneUsageStatus::default();
    };
    OperatingPlaneUsageStatus {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        total_tokens: usage.total_tokens,
        estimated_cost_micros: usage.estimated_cost_micros,
        currency: one_line(usage.currency.as_deref()),
    }
}

fn operating_plane_identity_subject(run: &OperatingPlaneRun) -> Option<String> {
    let identity = run.identity.as_ref()?;
    first_string([
        identity.gateway_authenticated_subject.as_deref(),
        identity.gateway_authenticated_user_subject.as_deref(),
        identity.gateway_authenticated_service.as_deref(),
        identity.principal_id.as_deref(),
        identity.actor_id.as_deref(),
        identity.agent_id.as_deref(),
    ])
}

fn operating_plane_withheld_reasons(run: &OperatingPlaneRun) -> Vec<String> {
    let mut values: Vec<Option<String>> = run
        .withholding_reasons
        .iter()
        .flatten()
        .map(|value| Some(value.clone()))
        .collect();
    if let Some(count) = run.redaction_count.filter(|count| *count > 0) {
        values.push(Some(format!(
            "{count} {}",
            pluralize("redaction", count as usize)
        )));
    }
    if let Some(sources) = &run.unavailable_sources {
        for source in sources {
            values.push(Some(source.clone()));
        }
    }
    unique_strings(values)
}

fn format_artifact_ref(evidence: &OperatingPlaneArtifactStatus) -> String {
    let mut details = vec![
        format!("{}/{}", evidence.source, evidence.kind),
        if evidence.available {
            "available".to_owned()
        } else {
            "unavailable".to_owned()
        },
    ];
    if let Some(uri) = &evidence.uri {
        details.push(format!("uri {uri}"));
    }
    if let Some(revision) = &evidence.revision {
        details.push(format!("revision {revision}"));
    }
    format!(
        "{} ({})",
        evidence.id,
        unique_strings(details.into_iter().map(Some)).join(", ")
    )
}

fn operator_facing_signal_label(value: &str) -> Option<String> {
    let normalized = one_line(Some(value))?;
    let replaced = regex_replace_word(&normalized, "proof", "signal");
    let replaced = regex_replace_word(&replaced, "evidence", "artifact");
    Some(replaced)
}

fn regex_replace_word(input: &str, from: &str, to: &str) -> String {
    // Case-insensitive whole-word replacement matching JS \b.../giu.
    let re = regex::RegexBuilder::new(&format!(r"\b{}\b", regex::escape(from)))
        .case_insensitive(true)
        .build()
        .expect("static word regex");
    re.replace_all(input, to).into_owned()
}

fn format_usage(usage: &OperatingPlaneUsageStatus) -> Option<String> {
    let parts = [
        usage
            .total_tokens
            .map(|value| format!("{value} total tokens")),
        usage
            .input_tokens
            .map(|value| format!("{value} input tokens")),
        usage
            .output_tokens
            .map(|value| format!("{value} output tokens")),
        usage
            .estimated_cost_micros
            .map(|value| format!("{value} cost micros")),
        usage.currency.clone(),
    ];
    let joined = unique_strings(parts).join(", ");
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

fn first_string<'a>(values: impl IntoIterator<Item = Option<&'a str>>) -> Option<String> {
    for value in values {
        if let Some(normalized) = one_line(value) {
            return Some(normalized);
        }
    }
    None
}

fn one_line(value: Option<&str>) -> Option<String> {
    let normalized = value?.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn push_line(lines: &mut Vec<String>, label: &str, value: Option<&str>) {
    if let Some(value) = value {
        lines.push(format!("{label}: {value}"));
    }
}

fn unique_strings(values: impl IntoIterator<Item = Option<String>>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut normalized = Vec::new();
    for value in values {
        let Some(value) = value else {
            continue;
        };
        let Some(clean) = one_line(Some(&value)) else {
            continue;
        };
        if seen.insert(clean.clone()) {
            normalized.push(clean);
        }
    }
    normalized
}

fn pluralize(noun: &str, count: usize) -> String {
    if count == 1 {
        noun.to_owned()
    } else {
        format!("{noun}s")
    }
}

fn normalize_positive_limit(value: Option<usize>) -> Option<usize> {
    value.filter(|value| *value > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operating_plane_client::{
        OperatingPlaneEvidence, OperatingPlaneIdentity, OperatingPlaneRuntimeSignals,
        OperatingPlaneWorkItem,
    };

    fn sample_inspection() -> OperatingPlaneInspection {
        OperatingPlaneInspection {
            contract_version: "agent-operating-plane.v1".to_owned(),
            generated_at: "2026-05-17T06:05:00Z".to_owned(),
            unavailable_sources: Some(vec!["meter".to_owned()]),
            runs: vec![OperatingPlaneRun {
                agent_run_id: "run_1".to_owned(),
                agent_run_step_id: None,
                title: "Slack answer".to_owned(),
                status: "succeeded".to_owned(),
                surface: "slack".to_owned(),
                channel_thread_id: Some("C123:1740000000.000100".to_owned()),
                trace_id: Some("trace-1".to_owned()),
                identity: Some(OperatingPlaneIdentity {
                    workspace_id: Some("ws_evalops".to_owned()),
                    tenant_id: Some("tenant_1".to_owned()),
                    actor_id: None,
                    principal_id: None,
                    agent_id: None,
                    gateway_authenticated_subject: Some("user:alice".to_owned()),
                    gateway_authenticated_user_subject: None,
                    gateway_authenticated_service: None,
                }),
                redaction_count: Some(2),
                withholding_reasons: Some(vec!["customer_content".to_owned()]),
                unavailable_sources: Some(vec!["tool-execution".to_owned()]),
                evidence_refs: Some(vec![OperatingPlaneEvidence {
                    id: "gateway:req_123".to_owned(),
                    source: "llm_gateway".to_owned(),
                    kind: "model_event".to_owned(),
                    uri: None,
                    revision: Some("rev_1".to_owned()),
                    available: true,
                    summary: Some("SECRET raw artifact summary".to_owned()),
                }]),
                work_items: Some(vec![OperatingPlaneWorkItem {
                    kind: Some("followup".to_owned()),
                    state: Some("waiting".to_owned()),
                    next_action: Some("Post allowed artifact revision to operator".to_owned()),
                    blocker: Some("approval pending".to_owned()),
                }]),
                usage: Some(OperatingPlaneUsage {
                    input_tokens: None,
                    output_tokens: None,
                    total_tokens: Some(1234),
                    estimated_cost_micros: Some(4567),
                    currency: Some("USD".to_owned()),
                }),
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
                    missing_signals: Some(vec![
                        "tool ledger".to_owned(),
                        "approval ledger".to_owned(),
                    ]),
                }),
                canonical_attributes: Some(serde_json::json!({
                    "evalops.raw_prompt": "SECRET raw customer prompt"
                })),
            }],
        }
    }

    #[test]
    fn turns_inspections_into_content_free_runtime_status() {
        let report = summarize_operating_plane_inspection(&sample_inspection(), None);
        assert_eq!(report.contract_version, "agent-operating-plane.v1");
        assert_eq!(report.run_count, 1);
        assert_eq!(report.unavailable_sources, vec!["meter"]);
        let run = &report.runs[0];
        assert_eq!(run.run_id, "run_1");
        assert_eq!(run.identity_subject.as_deref(), Some("user:alice"));
        assert_eq!(
            run.signals_present,
            vec!["identity", "model", "trace", "artifact", "cost"]
        );
        assert_eq!(
            run.signals_missing,
            vec!["tool", "approval", "tool ledger", "approval ledger"]
        );
        assert_eq!(
            run.withheld,
            vec!["customer_content", "2 redactions", "tool-execution"]
        );
        let formatted = format_operating_plane_status_report(&report);
        assert!(formatted.contains("Agent operating-plane status"));
        assert!(formatted.contains("Identity: user:alice"));
        assert!(formatted.contains(
            "Artifacts: gateway:req_123 (llm_gateway/model_event, available, revision rev_1)"
        ));
        assert!(formatted.contains("Next action: Post allowed artifact revision to operator"));
        assert!(
            formatted
                .contains("Withheld/out of scope: customer_content, 2 redactions, tool-execution")
        );
        assert!(!formatted.contains("SECRET raw customer prompt"));
        assert!(!formatted.contains("SECRET raw artifact summary"));
    }

    #[test]
    fn formats_empty_inspections_as_useful_miss() {
        let report = summarize_operating_plane_inspection(
            &OperatingPlaneInspection {
                contract_version: "agent-operating-plane.v1".to_owned(),
                generated_at: "2026-05-17T06:05:00Z".to_owned(),
                unavailable_sources: Some(vec!["agentruntime".to_owned()]),
                runs: vec![],
            },
            None,
        );
        assert_eq!(report.run_count, 0);
        let formatted = format_operating_plane_status_report(&report);
        assert!(formatted.contains("No operating-plane runs matched the query."));
    }

    #[test]
    fn treats_absent_runtime_signals_as_missing_telemetry() {
        let report = summarize_operating_plane_inspection(
            &OperatingPlaneInspection {
                contract_version: "agent-operating-plane.v1".to_owned(),
                generated_at: "2026-05-17T06:05:00Z".to_owned(),
                unavailable_sources: None,
                runs: vec![OperatingPlaneRun {
                    agent_run_id: "run_partial".to_owned(),
                    agent_run_step_id: None,
                    title: "Partial payload".to_owned(),
                    status: "running".to_owned(),
                    surface: "slack".to_owned(),
                    channel_thread_id: None,
                    trace_id: None,
                    identity: None,
                    redaction_count: None,
                    withholding_reasons: None,
                    unavailable_sources: None,
                    evidence_refs: None,
                    work_items: None,
                    usage: None,
                    runtime_signals: None,
                    canonical_attributes: None,
                }],
            },
            None,
        );
        assert!(report.runs[0].signals_present.is_empty());
        assert_eq!(
            report.runs[0].signals_missing,
            vec![
                "identity",
                "model",
                "tool",
                "approval",
                "trace",
                "artifact",
                "cost",
                "runtime signal unavailable",
            ]
        );
    }

    #[test]
    fn operator_signal_labels_rewrite_proof_and_evidence() {
        assert_eq!(
            operator_facing_signal_label("tool proof missing"),
            Some("tool signal missing".to_owned())
        );
        assert_eq!(
            operator_facing_signal_label("evidence ledger"),
            Some("artifact ledger".to_owned())
        );
    }
}
