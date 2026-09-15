use maestro_runtime_contracts::experiments::{ExperimentAssignment, ExperimentEnrollment};
#[path = "delivery_accounting.rs"]
mod delivery_accounting;
#[path = "journal.rs"]
mod journal;
pub(crate) use journal::TurnJournal;

use std::fs::{self, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use fd_lock::RwLock as FileLock;
use rand::Rng as _;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::visibility::VisibilityEvent;
use super::wide_events::TurnMeasurements;

use super::onboarding::{OnboardingCollectionStatus, OnboardingEvent};

use crate::telemetry::{
    AbortReason, ApprovalMode, CanonicalTurnEvent, ExternalTurnEvent, SampleReason, SandboxMode,
    TelemetryIdentityScope, TurnStatus,
};

const FIRST_PARTY_TELEMETRY_ENDPOINT: &str = "https://app.evalops.dev/v1/maestro/telemetry";
const OUTBOX_CAPACITY: usize = 256;
const OUTBOX_DEAD_LETTER_CAPACITY: usize = 64;
const OUTBOX_DRAIN_MAX_EVENTS: usize = 32;
const OUTBOX_MAX_EVENT_BYTES: usize = 16 * 1024;
const REMOTE_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_DURATION_MS: u64 = 24 * 60 * 60 * 1_000;
const MAX_BYTES: u64 = 1 << 30;
const MAX_COUNT: u32 = 1_000_000;
const MAX_TOKEN_COUNT: u64 = 100_000_000;
const MAX_COST_USD: f64 = 1_000_000.0;

static OUTBOX_DRAIN_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

fn env_value(primary: &str, fallback: &str) -> Option<String> {
    std::env::var(primary)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var(fallback)
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
}

fn true_flag(name: &str) -> bool {
    std::env::var(name).ok().is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn telemetry_flag() -> Option<bool> {
    env_value("MAESTRO_TELEMETRY", "PLAYWRIGHT_TELEMETRY").and_then(|value| {
        match value.trim().to_ascii_lowercase().as_str() {
            "0" | "false" => Some(false),
            "1" | "true" => Some(true),
            _ => None,
        }
    })
}

fn expand_home(path: &str) -> PathBuf {
    if path == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

fn telemetry_file() -> Option<PathBuf> {
    env_value("MAESTRO_TELEMETRY_FILE", "PLAYWRIGHT_TELEMETRY_FILE")
        .map(|path| expand_home(path.trim()))
}

fn default_telemetry_file() -> PathBuf {
    std::env::var("MAESTRO_HOME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|path| expand_home(path.trim()))
        .or_else(|| dirs::home_dir().map(|home| home.join(".maestro")))
        .unwrap_or_else(|| PathBuf::from(".maestro"))
        .join("telemetry.log")
}

fn first_party_outbox_dir() -> PathBuf {
    crate::path_utils::maestro_home_dir()
        .unwrap_or_else(|| PathBuf::from(".maestro"))
        .join("telemetry")
        .join("outbox")
}

fn sample_rate() -> f64 {
    env_value("MAESTRO_TELEMETRY_SAMPLE", "PLAYWRIGHT_TELEMETRY_SAMPLE")
        .and_then(|value| value.parse::<f64>().ok())
        .map(|value| value.clamp(0.0, 1.0))
        .unwrap_or(1.0)
}

fn staged_rollout_event(
    event: &str,
    surface_id: &str,
    surface_type: &str,
    owner: Option<&str>,
    source: &str,
) -> Value {
    let mut metadata = serde_json::Map::new();
    if let Some(owner) = owner {
        metadata.insert("owner".into(), json!(owner));
    }
    metadata.insert("source".into(), json!(source));
    json!({
        "type": "staged-rollout-surface",
        "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "event": event,
        "surfaceId": surface_id,
        "surfaceType": surface_type,
        "metadata": metadata,
    })
}

/// Best-effort staged-rollout telemetry for native CLI surfaces.
pub async fn record_staged_rollout_surface_usage(
    event: &str,
    surface_id: &str,
    surface_type: &str,
    owner: Option<&str>,
    source: &str,
) {
    if first_party_telemetry_disabled() {
        return;
    }

    let file = telemetry_file();
    let endpoint = env_value(
        "MAESTRO_TELEMETRY_ENDPOINT",
        "PLAYWRIGHT_TELEMETRY_ENDPOINT",
    );

    let rate = sample_rate();
    if rate == 0.0 || (rate < 1.0 && rand::rng().random::<f64>() > rate) {
        return;
    }

    let payload = staged_rollout_event(event, surface_id, surface_type, owner, source);
    let encoded = payload.to_string();

    if let Some(endpoint) = endpoint.as_deref() {
        // Best-effort telemetry must never hang the CLI on a dead host.
        let client = reqwest::Client::builder().timeout(REMOTE_TIMEOUT).build();
        if let Ok(client) = client {
            let _ = client
                .post(endpoint)
                .header("content-type", "application/json")
                .body(encoded.clone())
                .send()
                .await;
        }
    }

    append_local_telemetry(file.unwrap_or_else(default_telemetry_file), &encoded);
}

fn append_local_telemetry(path: PathBuf, encoded: &str) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut output) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(output, "{encoded}");
    }
}

/// Closed first-party envelope accepted by `/v1/maestro/telemetry`.
///
/// This is intentionally distinct from [`ExternalTurnEvent`]: the latter is
/// the backwards-compatible custom exporter format, while this type has a
/// UUID idempotency key and native session correlation for Session History.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FirstPartyTurnTelemetryEvent {
    schema_version: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    diagnostics: Option<super::operation::OperationDiagnostics>,
    event_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    partial_reported_cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    delivery: Option<DeliverySnapshot>,
    #[serde(rename = "type")]
    event_type: FirstPartyEventType,
    timestamp: String,
    turn_number: u32,
    model_provider: FirstPartyModelProvider,
    total_duration_ms: u64,
    llm_duration_ms: u64,
    tool_duration_ms: u64,
    queue_wait_ms: Option<u64>,
    tool_count: u32,
    tool_success_count: u32,
    tool_failure_count: u32,
    tokens: FirstPartyTokenUsage,
    cost_usd: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reported_cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_outcomes: Option<FirstPartyToolOutcomes>,
    sandbox_mode: SandboxMode,
    approval_mode: ApprovalMode,
    mcp_server_count: u32,
    context_source_count: u32,
    message_count: u32,
    input_size_bytes: u64,
    output_size_bytes: u64,
    status: TurnStatus,
    error_category: Option<FirstPartyErrorCategory>,
    abort_reason: Option<AbortReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    measurements: Option<FirstPartyTurnMeasurements>,
    sampled: bool,
    sample_reason: SampleReason,
}

/// Content-free receipt outcomes; absence denotes an older producer.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FirstPartyToolOutcomes {
    succeeded: u32,
    failed: u32,
    denied: u32,
    cancelled: u32,
    indeterminate: u32,
    unknown: u32,
    measured_execution_count: u32,
    execution_duration_ms: u64,
}

/// Queue observations attached to the next turn; no extra upload lifecycle.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeliverySnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    accounting: Option<delivery_accounting::DeliveryAccounting>,
    pending_events: u32,
    rejected_events: u32,
    oldest_pending_age_seconds: u64,
    queue_capacity: u32,
}

fn delivery_snapshot(outbox: &Path, scope: &TelemetryIdentityScope) -> DeliverySnapshot {
    let pending = outbox_paths(outbox)
        .into_iter()
        .filter(|path| {
            read_bounded_outbox_record(path).is_some_and(|record| record.identity_scope == *scope)
        })
        .collect::<Vec<_>>();
    let oldest_pending_age_seconds = pending
        .iter()
        .filter_map(|path| {
            let name = path.file_name()?.to_str()?;
            let micros: i64 = name.split_once('_')?.0.parse().ok()?;
            Some(
                chrono::Utc::now()
                    .timestamp_micros()
                    .saturating_sub(micros)
                    .max(0) as u64
                    / 1_000_000,
            )
        })
        .max()
        .unwrap_or(0);
    let rejected_events = outbox_paths(&dead_letter_dir(outbox))
        .into_iter()
        .filter(|path| {
            read_bounded_outbox_record(path).is_some_and(|record| record.identity_scope == *scope)
        })
        .count() as u32;
    DeliverySnapshot {
        accounting: delivery_accounting::read(outbox, scope).or_else(|| {
            with_outbox_lock(outbox, || {
                delivery_accounting::update(outbox, scope, |_| {})?;
                delivery_accounting::read(outbox, scope)
            })
        }),
        pending_events: pending.len() as u32,
        rejected_events,
        oldest_pending_age_seconds,
        queue_capacity: OUTBOX_CAPACITY as u32,
    }
}

impl FirstPartyToolOutcomes {
    fn is_valid(&self, tool_count: u32) -> bool {
        let total: u64 = [
            self.succeeded,
            self.failed,
            self.denied,
            self.cancelled,
            self.indeterminate,
            self.unknown,
        ]
        .into_iter()
        .map(u64::from)
        .sum();
        total == u64::from(tool_count)
            && self.measured_execution_count <= tool_count
            && self.execution_duration_ms <= MAX_DURATION_MS
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum FirstPartyCollection {
    AllEligible,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FirstPartyTurnMeasurements {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    context_estimation: Option<maestro_context::context_usage::ContextEstimationMeasurements>,
    collection: FirstPartyCollection,
    first_output_ms: Option<u64>,
    compaction_duration_ms: Option<u64>,
    stream_stall_count: Option<u32>,
    stream_open_failure_count: Option<u32>,
    stream_disconnect_count: Option<u32>,
    stream_retry_count: Option<u32>,
    stream_recovery_count: Option<u32>,
    request_retry_count: u32,
    compaction_count: u32,
    automatic_compaction_count: u32,
    compacted_input_tokens: u64,
    response_count: u32,
    responses_with_usage: u32,
    responses_with_cost: u32,
}

impl FirstPartyTurnMeasurements {
    fn from_observed(m: &TurnMeasurements) -> Self {
        Self {
            context_estimation: m.context_estimation.clone(),
            collection: FirstPartyCollection::AllEligible,
            first_output_ms: m.first_output_ms.map(|v| v.min(MAX_DURATION_MS)),
            compaction_duration_ms: m.compaction_duration_ms.map(|v| v.min(MAX_DURATION_MS)),
            stream_stall_count: m.stream_stall_count.map(|v| v.min(MAX_COUNT)),
            stream_open_failure_count: m.stream_open_failure_count.map(|v| v.min(MAX_COUNT)),
            stream_disconnect_count: m.stream_disconnect_count.map(|v| v.min(MAX_COUNT)),
            stream_retry_count: m.stream_retry_count.map(|v| v.min(MAX_COUNT)),
            stream_recovery_count: m.stream_recovery_count.map(|v| v.min(MAX_COUNT)),
            request_retry_count: m.request_retry_count.min(MAX_COUNT),
            compaction_count: m.compaction_count.min(MAX_COUNT),
            automatic_compaction_count: m.automatic_compaction_count.min(MAX_COUNT),
            response_count: m.response_count.min(MAX_COUNT),
            responses_with_usage: m.responses_with_usage.min(MAX_COUNT),
            responses_with_cost: m.responses_with_cost.min(MAX_COUNT),
            compacted_input_tokens: m.compacted_input_tokens.min(MAX_TOKEN_COUNT),
        }
    }
    fn is_valid(&self) -> bool {
        self.first_output_ms.is_none_or(|v| v <= MAX_DURATION_MS)
            && self
                .compaction_duration_ms
                .is_none_or(|v| v <= MAX_DURATION_MS)
            && [
                self.stream_stall_count,
                self.stream_open_failure_count,
                self.stream_disconnect_count,
                self.stream_retry_count,
                self.stream_recovery_count,
            ]
            .into_iter()
            .all(|v| v.is_none_or(|n| n <= MAX_COUNT))
            && self.request_retry_count <= MAX_COUNT
            && self.compaction_count <= MAX_COUNT
            && self.automatic_compaction_count <= self.compaction_count
            && self.compacted_input_tokens <= MAX_TOKEN_COUNT
            && self.response_count <= MAX_COUNT
            && self.responses_with_usage <= self.response_count
            && self.context_estimation.as_ref().is_none_or(|m| {
                m.responses > 0
                    && m.responses <= u64::from(self.responses_with_usage)
                    && m.underestimated_responses <= m.responses
                    && m.estimated_input_tokens <= MAX_TOKEN_COUNT
                    && m.observed_input_tokens <= MAX_TOKEN_COUNT
                    && m.absolute_error_tokens
                        <= m.estimated_input_tokens
                            .saturating_add(m.observed_input_tokens)
                    && m.absolute_error_tokens
                        >= m.estimated_input_tokens.abs_diff(m.observed_input_tokens)
            })
            && self.responses_with_cost <= self.response_count
            && {
                let counts = [
                    self.stream_stall_count,
                    self.stream_open_failure_count,
                    self.stream_disconnect_count,
                    self.stream_retry_count,
                    self.stream_recovery_count,
                ];
                counts.iter().all(Option::is_none) || counts.iter().all(Option::is_some)
            }
    }
}

/// Private, durable delivery envelope. The event remains the exact closed
/// Platform contract; `identity_scope` is never serialized into an HTTP body.
/// It binds a retry to the organization/workspace that admitted the native
/// turn, preventing a later account switch from re-attributing the record.
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FirstPartyOutboxRecord {
    identity_scope: TelemetryIdentityScope,
    event: FirstPartyTelemetryEvent,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum FirstPartyTelemetryEvent {
    Turn(Box<FirstPartyTurnTelemetryEvent>),
    Onboarding(OnboardingEvent),
    Visibility(VisibilityEvent),
    ExperimentEnrollment(ExperimentEnrollment),
}
impl FirstPartyTelemetryEvent {
    fn is_server_valid(&self) -> bool {
        match self {
            Self::Turn(event) => event.is_server_valid(),
            Self::Onboarding(event) => event.is_server_valid(),
            Self::Visibility(event) => event.is_server_valid(),
            Self::ExperimentEnrollment(event) => {
                event.is_valid() && chrono::DateTime::parse_from_rfc3339(&event.timestamp).is_ok()
            }
        }
    }
    fn event_id(&self) -> Uuid {
        match self {
            Self::Turn(event) => event.event_id,
            Self::Onboarding(event) => event.event_id,
            Self::Visibility(event) => event.event_id,
            Self::ExperimentEnrollment(event) => event.event_id,
        }
    }
}
impl FirstPartyTelemetryEvent {
    fn experiment_assignment(&self) -> Option<&ExperimentAssignment> {
        match self {
            Self::ExperimentEnrollment(event) => Some(&event.assignment),
            Self::Turn(event) => event
                .diagnostics
                .as_ref()?
                .experiment
                .as_ref()
                .map(|e| &e.assignment),
            _ => None,
        }
    }
}
impl From<ExperimentEnrollment> for FirstPartyTelemetryEvent {
    fn from(event: ExperimentEnrollment) -> Self {
        Self::ExperimentEnrollment(event)
    }
}

pub fn queue_experiment_enrollment(
    scope: &TelemetryIdentityScope,
    event: &ExperimentEnrollment,
) -> bool {
    if !crate::experiments::permits(scope, &event.assignment) {
        return false;
    }
    let queued = persist_first_party_event(&first_party_outbox_dir(), scope, event).is_some();
    if queued {
        schedule_first_party_outbox_drain();
    }
    queued
}

pub fn experiments_telemetry_disabled() -> bool {
    first_party_telemetry_disabled()
}

/// Revoked observations are deleted locally, including unfinished journal records.
/// The sender rechecks consent immediately before each HTTP request as well.
pub fn purge_revoked_experiment_events() {
    fn purge(directory: &Path) {
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                purge(&path);
                continue;
            }
            let Some(record) = read_bounded_outbox_record(&path) else {
                continue;
            };
            if record
                .event
                .experiment_assignment()
                .is_some_and(|a| !crate::experiments::permits(&record.identity_scope, a))
            {
                let _ = fs::remove_file(&path);
            }
        }
    }
    let outbox = first_party_outbox_dir();
    let _ = with_outbox_lock(&outbox, || {
        purge(&outbox);
        Some(())
    });
}

impl From<FirstPartyTurnTelemetryEvent> for FirstPartyTelemetryEvent {
    fn from(event: FirstPartyTurnTelemetryEvent) -> Self {
        Self::Turn(Box::new(event))
    }
}
impl From<VisibilityEvent> for FirstPartyTelemetryEvent {
    fn from(event: VisibilityEvent) -> Self {
        Self::Visibility(event)
    }
}
impl From<OnboardingEvent> for FirstPartyTelemetryEvent {
    fn from(event: OnboardingEvent) -> Self {
        Self::Onboarding(event)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FirstPartyTokenUsage {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
enum FirstPartyEventType {
    #[serde(rename = "canonical-turn")]
    CanonicalTurn,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum FirstPartyModelProvider {
    Anthropic,
    #[serde(rename = "azure-openai")]
    AzureOpenai,
    Bedrock,
    Google,
    Llamacpp,
    Ollama,
    Openai,
    Openrouter,
    Other,
    Unknown,
    #[serde(rename = "vertex-ai")]
    VertexAi,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum FirstPartyErrorCategory {
    Interrupted,
    Provider,
    Runtime,
    Other,
}

impl FirstPartyTurnTelemetryEvent {
    fn is_server_valid(&self) -> bool {
        self.schema_version == 1
            && self.diagnostics.as_ref().is_none_or(|d| {
                d.is_valid() && (d.completion_observed || self.reported_cost_usd.is_none())
            })
            && [&self.session_id, &self.turn_id, &self.model_id]
                .into_iter()
                .all(|value| {
                    value
                        .as_ref()
                        .is_none_or(|v| !v.is_empty() && v.len() <= 1024 && v.trim() == v)
                })
            && self.partial_reported_cost_usd.is_none_or(|cost| {
                cost.is_finite()
                    && (0.0..=MAX_COST_USD).contains(&cost)
                    && self.reported_cost_usd.is_none()
                    && self.measurements.as_ref().is_some_and(|m| {
                        m.responses_with_cost > 0
                            && (m.responses_with_cost < m.response_count
                                || self
                                    .diagnostics
                                    .as_ref()
                                    .is_some_and(|d| !d.completion_observed))
                    })
            })
            && chrono::DateTime::parse_from_rfc3339(&self.timestamp).is_ok()
            && self.turn_number <= MAX_COUNT
            && self.tool_count <= MAX_COUNT
            && self.tool_success_count <= MAX_COUNT
            && self.tool_failure_count <= MAX_COUNT
            && self.mcp_server_count <= MAX_COUNT
            && self.context_source_count <= MAX_COUNT
            && self.message_count <= MAX_COUNT
            && self
                .tool_success_count
                .saturating_add(self.tool_failure_count)
                == self.tool_count
            && self.total_duration_ms <= MAX_DURATION_MS
            && self.llm_duration_ms <= MAX_DURATION_MS
            && self.tool_duration_ms <= MAX_DURATION_MS
            && self
                .queue_wait_ms
                .is_none_or(|value| value <= MAX_DURATION_MS)
            && self.input_size_bytes <= MAX_BYTES
            && self.output_size_bytes <= MAX_BYTES
            && self.tokens.input <= MAX_TOKEN_COUNT
            && self.tokens.output <= MAX_TOKEN_COUNT
            && self.tokens.cache_read <= MAX_TOKEN_COUNT
            && self.tokens.cache_write <= MAX_TOKEN_COUNT
            && self
                .tokens
                .thinking
                .is_none_or(|value| value <= MAX_TOKEN_COUNT)
            && self.cost_usd.is_finite()
            && (0.0..=MAX_COST_USD).contains(&self.cost_usd)
            && self
                .measurements
                .as_ref()
                .is_none_or(|m| m.is_valid() && matches!(self.sample_reason, SampleReason::Always))
            && self
                .tool_outcomes
                .as_ref()
                .is_none_or(|v| v.is_valid(self.tool_count))
            && self.reported_cost_usd.is_none_or(|cost| {
                cost.is_finite()
                    && (0.0..=MAX_COST_USD).contains(&cost)
                    && self.measurements.as_ref().is_some_and(|m| {
                        m.response_count > 0 && m.responses_with_cost == m.response_count
                    })
            })
            && self.sampled
            && matches!(
                (self.status, self.abort_reason),
                (TurnStatus::Aborted, Some(_))
                    | (
                        TurnStatus::Success | TurnStatus::Error | TurnStatus::RateLimited,
                        None
                    )
            )
    }
}

fn first_party_event(external: &ExternalTurnEvent) -> Option<FirstPartyTurnTelemetryEvent> {
    let abort_reason = match (external.status, external.abort_reason) {
        (TurnStatus::Aborted, Some(reason)) => Some(reason),
        (TurnStatus::Aborted, None) | (_, Some(_)) => return None,
        _ => None,
    };
    let tool_count = external.tool_count.min(MAX_COUNT);
    let tool_success_count = external.tool_success_count.min(tool_count);
    let tool_failure_count = tool_count.saturating_sub(tool_success_count);
    let cost_usd = if external.cost_usd.is_finite() && external.cost_usd >= 0.0 {
        external.cost_usd.min(MAX_COST_USD)
    } else {
        0.0
    };
    let timestamp = chrono::DateTime::parse_from_rfc3339(&external.timestamp)
        .map(|value| value.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_else(|_| {
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        });

    Some(FirstPartyTurnTelemetryEvent {
        schema_version: 1,
        diagnostics: None,
        event_id: Uuid::new_v4(),
        session_id: None,
        turn_id: None,
        model_id: None,
        delivery: None,
        partial_reported_cost_usd: external.measurements.as_ref().and_then(|m| {
            (m.responses_with_cost > 0
                && m.responses_with_cost < m.response_count
                && external.cost_usd.is_finite()
                && external.cost_usd >= 0.0)
                .then_some(external.cost_usd)
        }),
        event_type: FirstPartyEventType::CanonicalTurn,
        timestamp,
        turn_number: external.turn_number.min(MAX_COUNT),
        model_provider: first_party_model_provider(&external.model_provider),
        total_duration_ms: external.total_duration_ms.min(MAX_DURATION_MS),
        llm_duration_ms: external.llm_duration_ms.min(MAX_DURATION_MS),
        tool_duration_ms: external.tool_duration_ms.min(MAX_DURATION_MS),
        queue_wait_ms: external
            .queue_wait_ms
            .map(|value| value.min(MAX_DURATION_MS)),
        tool_count,
        tool_success_count,
        tool_failure_count,
        tokens: FirstPartyTokenUsage {
            input: external.tokens.input.min(MAX_TOKEN_COUNT),
            output: external.tokens.output.min(MAX_TOKEN_COUNT),
            cache_read: external.tokens.cache_read.min(MAX_TOKEN_COUNT),
            cache_write: external.tokens.cache_write.min(MAX_TOKEN_COUNT),
            thinking: external
                .tokens
                .thinking
                .map(|value| value.min(MAX_TOKEN_COUNT)),
        },
        cost_usd,
        reported_cost_usd: external.reported_cost_usd.filter(|cost| {
            cost.is_finite()
                && (0.0..=MAX_COST_USD).contains(cost)
                && external.measurements.as_ref().is_some_and(|m| {
                    m.response_count > 0 && m.responses_with_cost == m.response_count
                })
        }),
        tool_outcomes: {
            let value = FirstPartyToolOutcomes {
                succeeded: external.tool_outcomes.succeeded,
                failed: external.tool_outcomes.failed,
                denied: external.tool_outcomes.denied,
                cancelled: external.tool_outcomes.cancelled,
                indeterminate: external.tool_outcomes.indeterminate,
                unknown: external.tool_outcomes.unknown,
                measured_execution_count: external.tool_outcomes.measured_execution_count,
                execution_duration_ms: external.tool_outcomes.execution_duration_ms,
            };
            value.is_valid(tool_count).then_some(value)
        },
        sandbox_mode: external.sandbox_mode,
        approval_mode: external.approval_mode,
        mcp_server_count: external.mcp_server_count.min(MAX_COUNT),
        context_source_count: external.context_source_count.min(MAX_COUNT),
        message_count: external.message_count.min(MAX_COUNT),
        input_size_bytes: external.input_size_bytes.min(MAX_BYTES),
        output_size_bytes: external.output_size_bytes.min(MAX_BYTES),
        status: external.status,
        error_category: first_party_error_category(external.error_category.as_deref()),
        abort_reason,
        measurements: external
            .measurements
            .as_ref()
            .map(FirstPartyTurnMeasurements::from_observed),
        sampled: true,
        sample_reason: if external.measurements.is_some() {
            SampleReason::Always
        } else {
            external.sample_reason
        },
    })
}

fn first_party_model_provider(provider: &str) -> FirstPartyModelProvider {
    let provider = provider.trim();
    if provider.is_empty() {
        FirstPartyModelProvider::Unknown
    } else if provider.eq_ignore_ascii_case("anthropic") {
        FirstPartyModelProvider::Anthropic
    } else if provider.eq_ignore_ascii_case("azure-openai")
        || provider.eq_ignore_ascii_case("azure_openai")
        || provider.eq_ignore_ascii_case("azure")
    {
        FirstPartyModelProvider::AzureOpenai
    } else if provider.eq_ignore_ascii_case("bedrock")
        || provider.eq_ignore_ascii_case("aws-bedrock")
    {
        FirstPartyModelProvider::Bedrock
    } else if provider.eq_ignore_ascii_case("google") || provider.eq_ignore_ascii_case("gemini") {
        FirstPartyModelProvider::Google
    } else if provider.eq_ignore_ascii_case("llamacpp")
        || provider.eq_ignore_ascii_case("llama.cpp")
        || provider.eq_ignore_ascii_case("llama-cpp")
    {
        FirstPartyModelProvider::Llamacpp
    } else if provider.eq_ignore_ascii_case("ollama") {
        FirstPartyModelProvider::Ollama
    } else if provider.eq_ignore_ascii_case("openai") {
        FirstPartyModelProvider::Openai
    } else if provider.eq_ignore_ascii_case("openrouter") {
        FirstPartyModelProvider::Openrouter
    } else if provider.eq_ignore_ascii_case("vertex-ai")
        || provider.eq_ignore_ascii_case("vertex_ai")
    {
        FirstPartyModelProvider::VertexAi
    } else {
        FirstPartyModelProvider::Other
    }
}

fn first_party_error_category(category: Option<&str>) -> Option<FirstPartyErrorCategory> {
    let category = category?.trim();
    if category.is_empty() {
        None
    } else if ["interrupted", "cancelled", "canceled", "user_cancelled"]
        .iter()
        .any(|allowed| category.eq_ignore_ascii_case(allowed))
    {
        Some(FirstPartyErrorCategory::Interrupted)
    } else if [
        "provider",
        "provider_stream",
        "provider_response",
        "authentication",
        "rate_limit",
    ]
    .iter()
    .any(|allowed| category.eq_ignore_ascii_case(allowed))
    {
        Some(FirstPartyErrorCategory::Provider)
    } else if ["runtime", "tool", "sandbox"]
        .iter()
        .any(|allowed| category.eq_ignore_ascii_case(allowed))
    {
        Some(FirstPartyErrorCategory::Runtime)
    } else {
        Some(FirstPartyErrorCategory::Other)
    }
}

fn outbox_paths(outbox_dir: &Path) -> Vec<PathBuf> {
    let mut paths = fs::read_dir(outbox_dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let file_type = entry.file_type().ok()?;
            if !file_type.is_file() || !is_outbox_file_name(&entry.file_name()) {
                return None;
            }
            Some(entry.path())
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

fn dead_letter_dir(outbox_dir: &Path) -> PathBuf {
    outbox_dir.join("dead-letter")
}

fn ensure_private_directory(path: &Path) -> Option<()> {
    fs::create_dir_all(path).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).ok()?;
    }
    Some(())
}

/// Serialize the short list/write/trim transaction across Maestro processes.
/// Ordinary network sends happen outside this lock. Experiment sends hold it
/// for their bounded request so withdrawal can finish after any in-flight send.
fn with_outbox_lock<T>(outbox_dir: &Path, operation: impl FnOnce() -> Option<T>) -> Option<T> {
    ensure_private_directory(outbox_dir)?;
    let lock_path = outbox_dir.join(".lock");
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path)
        .ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        file.set_permissions(fs::Permissions::from_mode(0o600))
            .ok()?;
    }
    let mut lock = FileLock::new(file);
    let guard = lock.write().ok()?;
    let result = operation();
    drop(guard);
    result
}

fn is_outbox_file_name(file_name: &std::ffi::OsStr) -> bool {
    let Some(file_name) = file_name.to_str() else {
        return false;
    };
    let Some(stem) = file_name.strip_suffix(".json") else {
        return false;
    };
    let Some((timestamp, event_id)) = stem.split_once('_') else {
        return false;
    };
    timestamp.parse::<i64>().is_ok() && Uuid::parse_str(event_id).is_ok()
}

fn trim_outbox_paths_to_capacity(
    paths: Vec<PathBuf>,
    capacity: usize,
    preserve: Option<&Path>,
) -> Option<()> {
    let excess = paths.len().saturating_sub(capacity);
    for path in paths
        .into_iter()
        .filter(|path| preserve != Some(path.as_path()))
        .take(excess)
    {
        let scope = read_bounded_outbox_record(&path).map(|r| r.identity_scope);
        fs::remove_file(&path).ok()?;
        if let Some(scope) = scope {
            if delivery_accounting::update(path.parent()?, &scope, |a| {
                a.evicted = a.evicted.saturating_add(1);
            })
            .is_none()
            {
                tracing::warn!("telemetry eviction accounting could not be persisted");
            }
        }
    }
    Some(())
}

fn persist_first_party_event<E: Clone + Into<FirstPartyTelemetryEvent>>(
    outbox_dir: &Path,
    identity_scope: &TelemetryIdentityScope,
    event: &E,
) -> Option<PathBuf> {
    persist_first_party_event_with_writer(
        outbox_dir,
        identity_scope,
        event,
        crate::path_utils::atomic_private_write,
    )
}

fn persist_first_party_event_with_writer<E: Clone + Into<FirstPartyTelemetryEvent>, F>(
    outbox_dir: &Path,
    identity_scope: &TelemetryIdentityScope,
    event: &E,
    writer: F,
) -> Option<PathBuf>
where
    F: FnOnce(&Path, &[u8]) -> anyhow::Result<()>,
{
    let event: FirstPartyTelemetryEvent = event.clone().into();
    if !identity_scope.is_complete() || !event.is_server_valid() {
        return None;
    }
    let encoded = serde_json::to_vec(&FirstPartyOutboxRecord {
        identity_scope: identity_scope.clone(),
        event: event.clone(),
    })
    .ok()?;
    if encoded.len() > OUTBOX_MAX_EVENT_BYTES {
        return None;
    }

    with_outbox_lock(outbox_dir, || {
        if event
            .experiment_assignment()
            .is_some_and(|a| !crate::experiments::permits(identity_scope, a))
        {
            return None;
        }
        let path = outbox_dir.join(format!(
            "{:020}_{}.json",
            chrono::Utc::now().timestamp_micros(),
            event.event_id()
        ));
        // Write first. If the filesystem cannot admit the new record, retain
        // every existing durable event instead of evicting one for nothing.
        if writer(&path, &encoded).is_err() {
            let _ = delivery_accounting::update(outbox_dir, identity_scope, |a| {
                a.write_failures = a.write_failures.saturating_add(1);
            });
            return None;
        }
        let _ = delivery_accounting::update(outbox_dir, identity_scope, |a| {
            a.queued = a.queued.saturating_add(1);
        });
        trim_outbox_paths_to_capacity(outbox_paths(outbox_dir), OUTBOX_CAPACITY, Some(&path))?;
        Some(path)
    })
}

fn read_bounded_outbox_record(path: &Path) -> Option<FirstPartyOutboxRecord> {
    let file = fs::File::open(path).ok()?;
    let mut reader = file.take((OUTBOX_MAX_EVENT_BYTES + 1) as u64);
    let mut encoded = Vec::new();
    reader.read_to_end(&mut encoded).ok()?;
    if encoded.len() > OUTBOX_MAX_EVENT_BYTES {
        return None;
    }
    let record = serde_json::from_slice::<FirstPartyOutboxRecord>(&encoded).ok()?;
    (record.identity_scope.is_complete() && record.event.is_server_valid()).then_some(record)
}

struct FirstPartyDeliverySession {
    access_token: String,
    identity_scope: TelemetryIdentityScope,
}

fn drain_first_party_outbox(outbox_dir: &Path, identity: &FirstPartyDeliverySession) {
    drain_first_party_outbox_to_endpoint(outbox_dir, identity, FIRST_PARTY_TELEMETRY_ENDPOINT);
}

fn is_permanent_client_rejection(status: StatusCode) -> bool {
    status.is_client_error()
        && !matches!(
            status,
            StatusCode::UNAUTHORIZED
                | StatusCode::FORBIDDEN
                | StatusCode::REQUEST_TIMEOUT
                | StatusCode::CONFLICT
                | StatusCode::TOO_MANY_REQUESTS
        )
}

fn move_to_dead_letter(outbox_dir: &Path, path: &Path) -> bool {
    with_outbox_lock(outbox_dir, || {
        let dead_letters = dead_letter_dir(outbox_dir);
        ensure_private_directory(&dead_letters)?;
        let destination = dead_letters.join(path.file_name()?);
        fs::rename(path, &destination).ok()?;
        trim_outbox_paths_to_capacity(
            outbox_paths(&dead_letters),
            OUTBOX_DEAD_LETTER_CAPACITY,
            Some(&destination),
        )?;
        Some(())
    })
    .is_some()
}

fn drain_first_party_outbox_to_endpoint(
    outbox_dir: &Path,
    identity: &FirstPartyDeliverySession,
    endpoint: &str,
) {
    let Ok(client) = reqwest::blocking::Client::builder()
        .timeout(REMOTE_TIMEOUT)
        .build()
    else {
        return;
    };

    let mut attempted = 0;
    for path in outbox_paths(outbox_dir) {
        if attempted >= OUTBOX_DRAIN_MAX_EVENTS {
            break;
        }
        let Some(record) = read_bounded_outbox_record(&path) else {
            continue;
        };
        // An event may wait through an account or organization switch. Do not
        // let the current bearer reattribute it; retain it until its original
        // authorized scope returns.
        if record.identity_scope != identity.identity_scope {
            continue;
        }
        if record
            .event
            .experiment_assignment()
            .is_some_and(|a| !crate::experiments::permits(&record.identity_scope, a))
        {
            let _ = fs::remove_file(&path);
            continue;
        }
        attempted += 1;
        let Ok(encoded) = serde_json::to_vec(&record.event) else {
            continue;
        };
        let request = client
            .post(endpoint)
            .bearer_auth(&identity.access_token)
            .header("content-type", "application/json")
            .body(encoded);
        let response = if let Some(assignment) = record.event.experiment_assignment() {
            // Withdrawal waits for at most the bounded in-flight send, then
            // purges. No prechecked experiment request can start after it returns.
            let Some(response) = with_outbox_lock(outbox_dir, || {
                if !crate::experiments::permits(&record.identity_scope, assignment) {
                    let _ = fs::remove_file(&path);
                    return None;
                }
                Some(request.send())
            }) else {
                continue;
            };
            response
        } else {
            request.send()
        };
        match response {
            Ok(response) if response.status().is_success() => {
                // A failed delete is harmless: the server idempotency key is
                // the UUID embedded in this same durable record, so a later
                // retry is a safe duplicate rather than a new event.
                let _ = with_outbox_lock(outbox_dir, || {
                    if path.exists() {
                        fs::remove_file(&path).ok()?;
                        delivery_accounting::update(outbox_dir, &record.identity_scope, |a| {
                            a.acknowledged = a.acknowledged.saturating_add(1);
                        })?;
                    }
                    Some(())
                });
            }
            Ok(response) if is_permanent_client_rejection(response.status()) => {
                // A closed-schema rejection can never become valid through a
                // token refresh. Quarantine it and continue so it cannot
                // permanently poison later valid records in FIFO order.
                if !move_to_dead_letter(outbox_dir, &path) {
                    break;
                }
            }
            Ok(_) | Err(_) => {
                // Retain auth refresh, 429, and transient failures for a
                // later turn. Stop to avoid a busy client repeatedly hitting
                // a degraded first-party service.
                break;
            }
        }
    }
}

#[cfg(test)]
fn first_party_delivery_session() -> Option<FirstPartyDeliverySession> {
    // Unit tests must never discover a developer's stored Identity session
    // and issue an unexpected production request. Transport behavior is
    // tested through the injected loopback endpoint below instead.
    None
}

#[cfg(not(test))]
fn first_party_delivery_session() -> Option<FirstPartyDeliverySession> {
    let session = crate::credential_mode::current_verified_identity_session().ok()?;
    Some(FirstPartyDeliverySession {
        access_token: session.access_token,
        identity_scope: TelemetryIdentityScope::new(
            &session.organization_id,
            session.workspace_id.as_deref(),
        )?,
    })
}

fn first_party_telemetry_disabled() -> bool {
    true_flag("MAESTRO_INTERNAL_TELEMETRY_DISABLED")
        || true_flag("EVALOPS_INTERNAL_TELEMETRY_DISABLED")
        || telemetry_flag() == Some(false)
}

fn schedule_first_party_outbox_drain() {
    if first_party_telemetry_disabled() {
        return;
    }
    if OUTBOX_DRAIN_IN_FLIGHT
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }

    let outbox_dir = first_party_outbox_dir();
    let worker = std::thread::Builder::new()
        .name("maestro-telemetry".to_owned())
        .spawn(move || {
            if let Some(identity) = first_party_delivery_session() {
                drain_first_party_outbox(&outbox_dir, &identity);
            }
            OUTBOX_DRAIN_IN_FLIGHT.store(false, Ordering::Release);
        });
    if worker.is_err() {
        OUTBOX_DRAIN_IN_FLIGHT.store(false, Ordering::Release);
    }
}

/// Give an already queued terminal event a bounded chance to reach the cloud.
/// Detached workers otherwise die at `process::exit` in short-lived exec runs.
/// Unsent records remain durable for retry; this never starts collection.
pub async fn flush_first_party_telemetry() {
    flush_outbox_worker(
        &OUTBOX_DRAIN_IN_FLIGHT,
        Duration::from_secs(3),
        schedule_first_party_outbox_drain,
    )
    .await;
}

async fn flush_outbox_worker(in_flight: &AtomicBool, budget: Duration, schedule: impl FnOnce()) {
    let started = Instant::now();
    wait_for_outbox_worker(in_flight, budget).await;
    let remaining = budget.saturating_sub(started.elapsed());
    if in_flight.load(Ordering::Acquire) || remaining.is_zero() {
        return;
    }
    // The previous worker may have snapshotted the outbox before the last
    // terminal event was queued. Give that durable record an upload attempt.
    schedule();
    wait_for_outbox_worker(in_flight, remaining).await;
}

async fn wait_for_outbox_worker(in_flight: &AtomicBool, budget: Duration) {
    let _ = tokio::time::timeout(budget, async {
        while in_flight.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
}

fn schedule_custom_export(endpoint: String, encoded: String) {
    // `MAESTRO_TELEMETRY_ENDPOINT` remains a user-configured, content-free
    // compatibility exporter. It never receives the Identity bearer used by
    // the fixed first-party telemetry route.
    let _ = std::thread::Builder::new()
        .name("maestro-telemetry-custom".to_owned())
        .spawn(move || {
            let Ok(client) = reqwest::blocking::Client::builder()
                .timeout(REMOTE_TIMEOUT)
                .build()
            else {
                return;
            };
            let _ = client
                .post(endpoint)
                .header("content-type", "application/json")
                .body(encoded)
                .send();
        });
}

/// Collect against the currently verified scope. Before login no event is queued;
/// a later login must never attribute earlier unauthenticated activity to its tenant.
/// Uses the existing bounded private outbox, retry worker, and telemetry opt-out.
pub async fn record_onboarding_event(
    event: OnboardingEvent,
    origin: Option<TelemetryIdentityScope>,
) -> OnboardingCollectionStatus {
    record_first_party_event_with(
        &event,
        origin,
        first_party_delivery_session,
        schedule_first_party_outbox_drain,
    )
}

fn record_first_party_event_with<E: Clone + Into<FirstPartyTelemetryEvent>>(
    event: &E,
    origin: Option<TelemetryIdentityScope>,
    verified_session: impl FnOnce() -> Option<FirstPartyDeliverySession>,
    schedule_drain: impl FnOnce(),
) -> OnboardingCollectionStatus {
    record_first_party_event_at(
        event,
        origin,
        verified_session,
        schedule_drain,
        &first_party_outbox_dir(),
    )
}

fn record_first_party_event_at<E: Clone + Into<FirstPartyTelemetryEvent>>(
    event: &E,
    origin: Option<TelemetryIdentityScope>,
    verified_session: impl FnOnce() -> Option<FirstPartyDeliverySession>,
    schedule_drain: impl FnOnce(),
    outbox: &Path,
) -> OnboardingCollectionStatus {
    if first_party_telemetry_disabled() {
        return OnboardingCollectionStatus::Disabled;
    }
    let Some(origin) = origin else {
        return OnboardingCollectionStatus::Unavailable;
    };
    let Some(identity) = verified_session() else {
        return OnboardingCollectionStatus::Unavailable;
    };
    if identity.identity_scope != origin {
        return OnboardingCollectionStatus::Unavailable;
    }
    if persist_first_party_event(outbox, &origin, event).is_none() {
        return OnboardingCollectionStatus::Failed;
    }
    schedule_drain();
    OnboardingCollectionStatus::Queued
}

/// Queue information-only visibility using the existing Identity-bound outbox.
pub async fn record_first_party_visibility_event(
    event: &VisibilityEvent,
    origin: Option<TelemetryIdentityScope>,
) -> OnboardingCollectionStatus {
    record_first_party_event_with(
        event,
        origin,
        first_party_delivery_session,
        schedule_first_party_outbox_drain,
    )
}

// Exercise the real persisted outbox boundary without starting a production
// drain from a unit test. The session still comes from live fixture Identity.
#[cfg(any(test, feature = "test-support"))]
pub fn test_record_visibility_with_session(
    event: &VisibilityEvent,
    origin: Option<TelemetryIdentityScope>,
    session: crate::credential_mode::PlatformSession,
    outbox: &Path,
) -> OnboardingCollectionStatus {
    record_first_party_event_at(
        event,
        origin,
        || {
            Some(FirstPartyDeliverySession {
                identity_scope: TelemetryIdentityScope::new(
                    &session.organization_id,
                    session.workspace_id.as_deref(),
                )?,
                access_token: session.access_token,
            })
        },
        || {},
        outbox,
    )
}

/// Capture the selected account before asynchronous work. These coordinates are
/// only a restriction: collection still requires matching live Identity authority.
/// An absent origin cannot be filled in by a later login.
pub fn onboarding_identity_scope() -> Option<TelemetryIdentityScope> {
    let env = std::env::vars().collect();
    let snapshot = crate::init_cli::load_evalops_snapshot().ok().flatten();
    let session = crate::credential_mode::platform_session_from(snapshot.as_ref(), &env)?;
    TelemetryIdentityScope::new(&session.organization_id, session.workspace_id.as_deref())
}

/// Persist completed native turn measurements and session correlation.
///
/// Every eligible, non-opted-out turn is queued for the fixed first-party
/// endpoint. Sampling still controls local receipts and custom exports. The Identity
/// bearer is loaded only by a background worker and is never written to the
/// outbox or sent to a configured custom exporter.
pub fn record_canonical_turn_event(event: &CanonicalTurnEvent) {
    record_canonical_turn_event_inner(event, true);
}

fn canonical_cloud_projection(event: &CanonicalTurnEvent) -> Option<FirstPartyTurnTelemetryEvent> {
    let mut first_party = first_party_event(&event.external_projection())?;
    first_party.session_id = (!event.session_id.is_empty()).then(|| event.session_id.clone());
    first_party.turn_id = Some(event.turn_id.clone());
    first_party.diagnostics = event.diagnostics.clone();
    if event
        .diagnostics
        .as_ref()
        .is_some_and(|d| !d.completion_observed)
    {
        first_party.reported_cost_usd = None;
        first_party.partial_reported_cost_usd = event
            .measurements
            .as_ref()
            .filter(|m| m.responses_with_cost > 0)
            .map(|_| event.cost_usd);
    }
    first_party.model_id = Some(event.model.id.clone());
    while serde_json::to_vec(&first_party).ok()?.len() > OUTBOX_MAX_EVENT_BYTES.saturating_sub(2048)
    {
        let diagnostics = first_party.diagnostics.as_mut()?;
        if diagnostics.attempts.is_empty() {
            return None;
        }
        diagnostics.attempts.pop();
        diagnostics.omitted_attempts = diagnostics.omitted_attempts.saturating_add(1);
    }
    Some(first_party)
}

fn record_canonical_turn_event_inner(event: &CanonicalTurnEvent, persist: bool) {
    if first_party_telemetry_disabled() {
        return;
    }

    let configured_file = telemetry_file();
    let configured_endpoint = env_value(
        "MAESTRO_TELEMETRY_ENDPOINT",
        "PLAYWRIGHT_TELEMETRY_ENDPOINT",
    );
    let external = event.external_projection();
    let Ok(encoded) = serde_json::to_string(&external) else {
        return;
    };

    if let (true, Some(identity_scope), Some(mut first_party)) = (
        persist,
        event.identity_scope.as_ref(),
        canonical_cloud_projection(event),
    ) {
        first_party.delivery = Some(delivery_snapshot(&first_party_outbox_dir(), identity_scope));
        if persist_first_party_event(&first_party_outbox_dir(), identity_scope, &first_party)
            .is_some()
        {
            schedule_first_party_outbox_drain();
        }
    }

    // Sampling remains a local/custom-export choice, never a first-party denominator.
    if !event.sampled {
        return;
    }
    // Keep the original local durable receipt even when a first-party or
    // custom endpoint is configured. `maestro value` consumes this log and
    // short-lived CLI processes retain their terminal turn at process exit.
    append_local_telemetry(
        configured_file.unwrap_or_else(default_telemetry_file),
        &encoded,
    );

    if let Some(endpoint) = configured_endpoint {
        let format =
            std::env::var("MAESTRO_TELEMETRY_FORMAT").unwrap_or_else(|_| "json".to_owned());
        if let Some(payload) = super::customer_export::encode(&external, &format) {
            schedule_custom_export(endpoint, payload);
        }
    }
}

#[cfg(test)]
#[path = "staged_rollout_test.rs"]
mod tests;
