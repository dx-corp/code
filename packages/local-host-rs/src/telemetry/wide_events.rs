//! Wide Events Implementation
//!
//! Canonical turn event types and the `TurnCollector` for accumulating
//! context during a turn and emitting a single wide event at completion.

use std::collections::HashMap;
use std::time::Instant;

use chrono::Utc;
use rand::RngExt;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ─────────────────────────────────────────────────────────────────────────────
// Types
// ─────────────────────────────────────────────────────────────────────────────

/// Thinking/reasoning level for the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    #[default]
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Max,
    Ultra,
}

impl std::fmt::Display for ThinkingLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Off => write!(f, "off"),
            Self::Minimal => write!(f, "minimal"),
            Self::Low => write!(f, "low"),
            Self::Medium => write!(f, "medium"),
            Self::High => write!(f, "high"),
            Self::Max => write!(f, "max"),
            Self::Ultra => write!(f, "ultra"),
        }
    }
}

/// Information about a single tool execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExecution {
    pub name: String,
    pub call_id: String,
    pub duration_ms: u64,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_size_bytes: Option<u64>,
}

/// Token usage statistics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<u64>,
}

/// Model information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub provider: String,
    pub thinking_level: ThinkingLevel,
}

impl Default for ModelInfo {
    fn default() -> Self {
        Self {
            id: "unknown".to_string(),
            provider: "unknown".to_string(),
            thinking_level: ThinkingLevel::Off,
        }
    }
}

/// Turn completion status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Success,
    Error,
    Aborted,
    RateLimited,
}

impl std::fmt::Display for TurnStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Success => write!(f, "success"),
            Self::Error => write!(f, "error"),
            Self::Aborted => write!(f, "aborted"),
            Self::RateLimited => write!(f, "rate_limited"),
        }
    }
}

/// Reason for abort.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AbortReason {
    User,
    Timeout,
    ContextOverflow,
    RateLimit,
}

/// Reason why an event was sampled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleReason {
    Always,
    Error,
    Slow,
    FirstTurn,
    Random,
}

/// Sandbox execution mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SandboxMode {
    Docker,
    Local,
    #[default]
    None,
}

/// Approval mode for tool execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalMode {
    Auto,
    #[default]
    Prompt,
    Fail,
}

/// Feature flags active during the turn.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FeatureFlags {
    #[serde(default)]
    pub boost_suggested: bool,
    #[serde(default)]
    pub boost_requested: bool,
    #[serde(default)]
    pub boost_applied: bool,
    pub safe_mode: bool,
    pub guardian_enabled: bool,
    pub compaction_enabled: bool,
    pub hook_count: u32,
}

/// Error details for failed turns.
///
/// Deliberately not `Serialize`: `message` is raw provider or tool text and
/// can carry paths, prompts, and secrets. Only `category` reaches an
/// exporter, through [`ExternalTurnEvent::error_category`].
#[derive(Debug, Clone, Default)]
pub struct ErrorDetails {
    pub category: Option<String>,
    pub message: Option<String>,
}

/// Authorized Identity tenant scope retained with a native turn solely for
/// first-party telemetry delivery.
///
/// This is deliberately absent from [`ExternalTurnEvent`]. The private
/// outbox uses it to ensure a delayed record is sent only while the same
/// organization/workspace scope is active; Platform continues to derive the
/// authoritative tenant from the bearer rather than accepting these values on
/// the wire.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TelemetryIdentityScope {
    organization_id: String,
    workspace_id: String,
}

impl std::fmt::Debug for TelemetryIdentityScope {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TelemetryIdentityScope")
            .field("bound", &true)
            .finish()
    }
}

impl TelemetryIdentityScope {
    pub(crate) fn experiment_assignment(
        &self,
        seed: &str,
        revision: u32,
    ) -> maestro_runtime_contracts::experiments::ExperimentAssignment {
        maestro_runtime_contracts::experiments::ExperimentAssignment::derive(
            seed,
            &self.organization_id,
            &self.workspace_id,
            revision,
        )
    }

    /// Build a scope from the Identity session already verified at the native
    /// model-admission boundary. Platform's telemetry ingress requires both
    /// organization and workspace, so incomplete sessions keep local telemetry
    /// only and never produce a deliverable remote record.
    pub fn new(organization_id: &str, workspace_id: Option<&str>) -> Option<Self> {
        let organization_id = organization_id.trim();
        if organization_id.is_empty() {
            return None;
        }
        let workspace_id = workspace_id?.trim();
        if workspace_id.is_empty() {
            return None;
        }
        Some(Self {
            organization_id: organization_id.to_owned(),
            workspace_id: workspace_id.to_owned(),
        })
    }

    pub(crate) fn is_complete(&self) -> bool {
        !self.organization_id.trim().is_empty() && !self.workspace_id.trim().is_empty()
    }
}

/// Canonical Turn Event - One wide event per agent turn.
///
/// Contains all context needed to debug and analyze any turn without
/// correlating multiple log lines. Designed for high-cardinality querying.
/// This type is deliberately **not** `Serialize`. It holds the session id,
/// tool names and arguments, MCP server names, and raw error text. Making it
/// serializable is what would let a future exporter put all of that on the
/// wire with one `serde_json::to_string`. The only serializable turn event is
/// [`ExternalTurnEvent`], produced by
/// [`CanonicalTurnEvent::external_projection`]. Use `Debug` for local
/// inspection.
#[derive(Debug, Clone)]
pub struct CanonicalTurnEvent {
    pub event_type: String,
    pub timestamp: String,

    // ─── Identity ───────────────────────────────────────────────────────────
    pub session_id: String,
    pub turn_id: String,
    pub turn_number: u32,
    pub trace_id: Option<String>,
    pub diagnostics: Option<super::operation::OperationDiagnostics>,

    /// Private delivery binding set by [`crate::telemetry::TurnTracker`] when
    /// the turn begins. It is intentionally not part of the serializable
    /// external projection.
    #[doc(hidden)]
    pub identity_scope: Option<TelemetryIdentityScope>,

    // ─── Model Context ──────────────────────────────────────────────────────
    pub model: ModelInfo,

    // ─── Timing ─────────────────────────────────────────────────────────────
    pub total_duration_ms: u64,
    pub llm_duration_ms: u64,
    pub tool_duration_ms: u64,
    pub queue_wait_ms: Option<u64>,

    // ─── Tool Executions ────────────────────────────────────────────────────
    pub tools: Vec<ToolExecution>,
    pub tool_count: u32,
    pub tool_success_count: u32,
    pub tool_failure_count: u32,
    pub tool_outcomes: ToolOutcomeMeasurements,

    // ─── Token Economics ────────────────────────────────────────────────────
    pub tokens: TokenUsage,
    pub cost_usd: f64,
    /// Provider-reported cost only when every response supplied a cost.
    pub reported_cost_usd: Option<f64>,
    pub measurements: Option<TurnMeasurements>,

    // ─── Business Context ───────────────────────────────────────────────────
    pub sandbox_mode: SandboxMode,
    pub approval_mode: ApprovalMode,
    pub mcp_server_count: u32,
    pub mcp_servers: Option<Vec<String>>,
    pub context_source_count: u32,
    pub message_count: u32,
    pub input_size_bytes: u64,
    pub output_size_bytes: u64,

    // ─── Feature Flags ──────────────────────────────────────────────────────
    pub features: FeatureFlags,

    // ─── Outcome ────────────────────────────────────────────────────────────
    pub status: TurnStatus,
    pub error_category: Option<String>,
    pub error_message: Option<String>,
    pub abort_reason: Option<AbortReason>,

    // ─── Sampling Metadata ──────────────────────────────────────────────────
    pub sampled: bool,
    pub sample_reason: SampleReason,
}

/// Observations made by the native event collector. Missing latency means no
/// nonempty output was observed, not a zero-latency response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TurnMeasurements {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_estimation: Option<maestro_context::context_usage::ContextEstimationMeasurements>,
    pub first_output_ms: Option<u64>,
    pub compaction_duration_ms: Option<u64>,
    pub stream_stall_count: Option<u32>,
    pub stream_open_failure_count: Option<u32>,
    pub stream_disconnect_count: Option<u32>,
    pub stream_retry_count: Option<u32>,
    pub stream_recovery_count: Option<u32>,
    pub request_retry_count: u32,
    pub compaction_count: u32,
    pub automatic_compaction_count: u32,
    pub compacted_input_tokens: u64,
    pub response_count: u32,
    pub responses_with_usage: u32,
    pub responses_with_cost: u32,
}

/// Receipt-backed outcomes. Legacy producers without a matching receipt remain
/// unknown; absent duration is not a zero-duration execution.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolOutcomeMeasurements {
    pub succeeded: u32,
    pub failed: u32,
    pub denied: u32,
    pub cancelled: u32,
    pub indeterminate: u32,
    pub unknown: u32,
    pub measured_execution_count: u32,
    pub execution_duration_ms: u64,
}

/// Closed, content-free projection allowed to cross the external telemetry
/// boundary. The richer canonical event remains local; tool names, call IDs,
/// MCP server names, error text, prompts, paths, and arguments are excluded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalTurnEvent {
    pub schema_version: u16,
    #[serde(default)]
    pub boost_suggested: bool,
    #[serde(default)]
    pub boost_requested: bool,
    #[serde(default)]
    pub boost_applied: bool,
    #[serde(rename = "type")]
    pub event_type: String,
    pub timestamp: String,
    pub turn_number: u32,
    pub model_provider: String,
    pub total_duration_ms: u64,
    pub llm_duration_ms: u64,
    pub tool_duration_ms: u64,
    pub queue_wait_ms: Option<u64>,
    pub tool_count: u32,
    pub tool_success_count: u32,
    pub tool_failure_count: u32,
    #[serde(default)]
    pub tool_outcomes: ToolOutcomeMeasurements,
    pub tokens: TokenUsage,
    pub cost_usd: f64,
    /// Provider-reported cost only when every response supplied a cost.
    #[serde(default)]
    pub reported_cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measurements: Option<TurnMeasurements>,
    pub sandbox_mode: SandboxMode,
    pub approval_mode: ApprovalMode,
    pub mcp_server_count: u32,
    pub context_source_count: u32,
    pub message_count: u32,
    pub input_size_bytes: u64,
    pub output_size_bytes: u64,
    pub status: TurnStatus,
    pub error_category: Option<String>,
    pub abort_reason: Option<AbortReason>,
    pub sampled: bool,
    pub sample_reason: SampleReason,
}

impl CanonicalTurnEvent {
    /// Produce the only event shape approved for an external exporter.
    #[must_use]
    pub fn external_projection(&self) -> ExternalTurnEvent {
        ExternalTurnEvent {
            schema_version: 1,
            boost_suggested: self.features.boost_suggested,
            boost_requested: self.features.boost_requested,
            boost_applied: self.features.boost_applied,
            event_type: self.event_type.clone(),
            timestamp: self.timestamp.clone(),
            turn_number: self.turn_number,
            model_provider: self.model.provider.clone(),
            total_duration_ms: self.total_duration_ms,
            llm_duration_ms: self.llm_duration_ms,
            tool_duration_ms: self.tool_duration_ms,
            queue_wait_ms: self.queue_wait_ms,
            tool_count: self.tool_count,
            tool_success_count: self.tool_success_count,
            tool_failure_count: self.tool_failure_count,
            tool_outcomes: self.tool_outcomes.clone(),
            tokens: self.tokens.clone(),
            cost_usd: self.cost_usd,
            reported_cost_usd: self.reported_cost_usd,
            measurements: self.measurements.clone(),
            sandbox_mode: self.sandbox_mode,
            approval_mode: self.approval_mode,
            mcp_server_count: self.mcp_server_count,
            context_source_count: self.context_source_count,
            message_count: self.message_count,
            input_size_bytes: self.input_size_bytes,
            output_size_bytes: self.output_size_bytes,
            status: self.status,
            error_category: self.error_category.clone(),
            abort_reason: self.abort_reason,
            sampled: self.sampled,
            sample_reason: self.sample_reason,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tail Sampling Configuration
// ─────────────────────────────────────────────────────────────────────────────

/// Configuration for tail sampling decisions.
#[derive(Debug, Clone)]
pub struct TailSamplingConfig {
    /// Sample rate for successful fast turns (0.0 to 1.0)
    pub success_sample_rate: f64,
    /// Threshold in ms above which a turn is considered "slow"
    pub slow_threshold_ms: u64,
    /// Always sample first N turns of a session
    pub always_sample_first_n: u32,
}

impl Default for TailSamplingConfig {
    fn default() -> Self {
        Self {
            success_sample_rate: 0.05, // 5% of successful fast turns
            slow_threshold_ms: 5000,   // 5 seconds
            always_sample_first_n: 1,  // Always sample first turn
        }
    }
}

impl TailSamplingConfig {
    /// Create config from environment variables.
    #[must_use]
    pub fn from_env() -> Self {
        let mut config = Self::default();

        if let Ok(rate) = std::env::var("MAESTRO_WIDE_EVENT_SAMPLE_RATE") {
            if let Ok(r) = rate.parse::<f64>() {
                if (0.0..=1.0).contains(&r) {
                    config.success_sample_rate = r;
                }
            }
        }

        if let Ok(threshold) = std::env::var("MAESTRO_WIDE_EVENT_SLOW_THRESHOLD_MS") {
            if let Ok(t) = threshold.parse::<u64>() {
                config.slow_threshold_ms = t;
            }
        }

        config
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// In-progress Tool Tracking
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct PendingTool {
    name: String,
    start_time: Instant,
    input_size_bytes: Option<u64>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Turn Collector
// ─────────────────────────────────────────────────────────────────────────────

/// Collects context during a turn and emits a single wide event at completion.
///
/// Create a new collector at the start of each turn, record events as they
/// happen, then call `complete()` to emit the canonical event.
#[derive(Debug, Clone)]
pub struct TurnCollector {
    session_id: String,
    turn_number: u32,
    turn_id: String,
    start_time: Instant,
    measurements: TurnMeasurements,
    tool_outcomes: ToolOutcomeMeasurements,
    sampling_config: TailSamplingConfig,

    // Timing
    llm_start_time: Option<Instant>,
    accumulated_llm_duration_ms: u64,
    queue_start_time: Option<Instant>,

    // Model
    model: ModelInfo,
    trace_id: Option<String>,

    // Tools
    pending_tools: HashMap<String, PendingTool>,
    // Receipt observation precedes ToolEnd; its success flag completes this entry.
    observed_tool_terminals: HashMap<String, Option<bool>>,
    completed_tools: Vec<ToolExecution>,

    // Context
    sandbox_mode: SandboxMode,
    approval_mode: ApprovalMode,
    mcp_servers: Vec<String>,
    context_source_count: u32,
    message_count: u32,
    input_size_bytes: u64,
    output_size_bytes: u64,
    features: FeatureFlags,
}

impl TurnCollector {
    /// Create a new turn collector.
    pub fn new(
        session_id: impl Into<String>,
        turn_number: u32,
        config: TailSamplingConfig,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            turn_number,
            turn_id: Uuid::new_v4().to_string(),
            start_time: Instant::now(),
            measurements: TurnMeasurements::default(),
            tool_outcomes: ToolOutcomeMeasurements::default(),
            sampling_config: config,
            llm_start_time: None,
            accumulated_llm_duration_ms: 0,
            queue_start_time: None,
            model: ModelInfo::default(),
            trace_id: None,
            pending_tools: HashMap::new(),
            observed_tool_terminals: HashMap::new(),
            completed_tools: Vec::new(),
            sandbox_mode: SandboxMode::None,
            approval_mode: ApprovalMode::Prompt,
            mcp_servers: Vec::new(),
            context_source_count: 0,
            message_count: 0,
            input_size_bytes: 0,
            output_size_bytes: 0,
            features: FeatureFlags::default(),
        }
    }

    /// Observe the stream owner without inferring coverage for other transports.
    pub fn record_stream_observation(&mut self, observation: crate::ai::StreamObservation) {
        use crate::ai::StreamObservation;
        let m = &mut self.measurements;
        match observation {
            StreamObservation::Observed => {
                m.stream_stall_count.get_or_insert(0);
                m.stream_open_failure_count.get_or_insert(0);
                m.stream_disconnect_count.get_or_insert(0);
                m.stream_retry_count.get_or_insert(0);
                m.stream_recovery_count.get_or_insert(0);
            }
            StreamObservation::OpenFailed => {
                m.stream_open_failure_count =
                    Some(m.stream_open_failure_count.unwrap_or(0).saturating_add(1));
            }
            StreamObservation::IdleTimeout => {
                m.stream_stall_count = Some(m.stream_stall_count.unwrap_or(0).saturating_add(1));
            }
            StreamObservation::Disconnect => {
                m.stream_disconnect_count =
                    Some(m.stream_disconnect_count.unwrap_or(0).saturating_add(1));
            }
            StreamObservation::Retry => {
                m.stream_retry_count = Some(m.stream_retry_count.unwrap_or(0).saturating_add(1));
            }
            StreamObservation::Recovery => {
                m.stream_recovery_count =
                    Some(m.stream_recovery_count.unwrap_or(0).saturating_add(1));
            }
        }
    }

    pub fn record_request_retry(&mut self) {
        self.measurements.request_retry_count =
            self.measurements.request_retry_count.saturating_add(1);
    }

    pub fn record_context_calibration(
        &mut self,
        observation: &maestro_context::context_usage::ContextCalibration,
    ) {
        self.measurements
            .context_estimation
            .get_or_insert_with(Default::default)
            .record(observation);
    }

    pub fn record_compaction_duration(&mut self, duration_ms: u64) {
        self.measurements.compaction_duration_ms = Some(
            self.measurements
                .compaction_duration_ms
                .unwrap_or(0)
                .saturating_add(duration_ms),
        );
    }

    /// Observe output without retaining its content.
    pub fn record_output(&mut self) {
        self.record_output_at(Instant::now());
    }

    /// Preserve the producer-observed first-output instant when collection is
    /// performed by a delayed background worker.
    pub(crate) fn record_output_at(&mut self, observed_at: Instant) {
        self.measurements.first_output_ms.get_or_insert_with(|| {
            observed_at
                .saturating_duration_since(self.start_time)
                .as_millis()
                .min(u128::from(u64::MAX)) as u64
        });
    }

    /// Observe a completed compaction without retaining the summary.
    pub fn record_compaction(&mut self, automatic: bool, tokens_before: u64) {
        self.measurements.compaction_count = self.measurements.compaction_count.saturating_add(1);
        self.measurements.automatic_compaction_count = self
            .measurements
            .automatic_compaction_count
            .saturating_add(u32::from(automatic));
        self.measurements.compacted_input_tokens = self
            .measurements
            .compacted_input_tokens
            .saturating_add(tokens_before);
    }

    /// Track coverage separately from zero-valued provider usage.
    pub fn record_response_coverage(&mut self, has_usage: bool, has_cost: bool) {
        self.measurements.response_count = self.measurements.response_count.saturating_add(1);
        self.measurements.responses_with_usage = self
            .measurements
            .responses_with_usage
            .saturating_add(u32::from(has_usage));
        self.measurements.responses_with_cost = self
            .measurements
            .responses_with_cost
            .saturating_add(u32::from(has_cost));
    }

    // ─── Setters ──────────────────────────────────────────────────────────────

    pub fn set_model(&mut self, model: ModelInfo) -> &mut Self {
        self.model = model;
        self
    }

    pub fn set_turn_id(&mut self, id: String) {
        self.turn_id = id;
    }

    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }

    pub fn set_trace_id(&mut self, trace_id: impl Into<String>) -> &mut Self {
        self.trace_id = Some(trace_id.into());
        self
    }

    pub fn set_sandbox_mode(&mut self, mode: SandboxMode) -> &mut Self {
        self.sandbox_mode = mode;
        self
    }

    pub fn set_approval_mode(&mut self, mode: ApprovalMode) -> &mut Self {
        self.approval_mode = mode;
        self
    }

    pub fn set_mcp_servers(&mut self, servers: Vec<String>) -> &mut Self {
        self.mcp_servers = servers;
        self
    }

    pub fn set_context_source_count(&mut self, count: u32) -> &mut Self {
        self.context_source_count = count;
        self
    }

    pub fn set_message_count(&mut self, count: u32) -> &mut Self {
        self.message_count = count;
        self
    }

    pub fn set_input_size(&mut self, bytes: u64) -> &mut Self {
        self.input_size_bytes = bytes;
        self
    }

    pub fn add_output_size(&mut self, bytes: u64) -> &mut Self {
        self.output_size_bytes += bytes;
        self
    }

    pub fn set_features(&mut self, features: FeatureFlags) -> &mut Self {
        self.features = features;
        self
    }

    // ─── Timing ───────────────────────────────────────────────────────────────

    pub fn record_queue_start(&mut self) -> &mut Self {
        self.queue_start_time = Some(Instant::now());
        self
    }

    pub fn record_llm_start(&mut self) -> &mut Self {
        self.llm_start_time = Some(Instant::now());
        self
    }

    pub fn record_llm_end(&mut self) -> &mut Self {
        // Accumulate LLM duration (turns may have multiple LLM calls)
        if let Some(start) = self.llm_start_time.take() {
            self.accumulated_llm_duration_ms += start.elapsed().as_millis() as u64;
        }
        self
    }

    // ─── Tool Recording ───────────────────────────────────────────────────────

    pub fn record_tool_start(
        &mut self,
        name: impl Into<String>,
        call_id: impl Into<String>,
        input_size_bytes: Option<u64>,
    ) -> &mut Self {
        let call_id = call_id.into();
        self.pending_tools.insert(
            call_id,
            PendingTool {
                name: name.into(),
                start_time: Instant::now(),
                input_size_bytes,
            },
        );
        self
    }

    /// Consume one terminal outcome per call, independently of approval timing.
    pub fn record_tool_receipt(
        &mut self,
        call_id: &str,
        receipt: Option<&maestro_runtime::ExecutionReceipt>,
    ) {
        use maestro_runtime::{ExecutionPhase, ExecutionStatus};
        // Auto-approved native calls can emit a terminal receipt without a
        // preceding approval event. Timing availability is not receipt authority.
        if self.observed_tool_terminals.contains_key(call_id) {
            return;
        }
        self.observed_tool_terminals
            .insert(call_id.to_owned(), None);
        let pending = self.pending_tools.get(call_id);
        let Some(receipt) = receipt.filter(|r| {
            !call_id.is_empty()
                && r.call_id == call_id
                && !r.tool_name.is_empty()
                && pending.is_none_or(|p| r.tool_name == p.name)
        }) else {
            self.tool_outcomes.unknown = self.tool_outcomes.unknown.saturating_add(1);
            return;
        };
        let counts = &mut self.tool_outcomes;
        let count = match receipt.status {
            ExecutionStatus::Succeeded => &mut counts.succeeded,
            ExecutionStatus::Failed => &mut counts.failed,
            ExecutionStatus::Denied => &mut counts.denied,
            ExecutionStatus::Cancelled { .. } => &mut counts.cancelled,
            ExecutionStatus::Indeterminate => &mut counts.indeterminate,
        };
        *count = count.saturating_add(1);
        if !matches!(
            receipt.status,
            ExecutionStatus::Denied
                | ExecutionStatus::Cancelled {
                    phase: ExecutionPhase::Queued
                }
        ) {
            if let Some(duration) = receipt.duration_ms {
                counts.measured_execution_count = counts.measured_execution_count.saturating_add(1);
                counts.execution_duration_ms =
                    counts.execution_duration_ms.saturating_add(duration);
            }
        }
    }

    pub fn record_tool_end(
        &mut self,
        call_id: &str,
        success: bool,
        output_size_bytes: Option<u64>,
        error_code: Option<String>,
    ) -> &mut Self {
        let terminal = self
            .observed_tool_terminals
            .entry(call_id.to_owned())
            .or_default();
        if terminal.is_some() {
            return self;
        }
        *terminal = Some(success);
        if let Some(pending) = self.pending_tools.remove(call_id) {
            let duration_ms = pending.start_time.elapsed().as_millis() as u64;
            self.completed_tools.push(ToolExecution {
                name: pending.name,
                call_id: call_id.to_string(),
                duration_ms,
                success,
                error_code,
                input_size_bytes: pending.input_size_bytes,
                output_size_bytes,
            });
        }
        self
    }

    // ─── Completion ───────────────────────────────────────────────────────────

    /// Complete the turn and emit the canonical event.
    /// Applies tail sampling logic to decide whether to persist.
    #[must_use]
    pub fn complete(
        self,
        status: TurnStatus,
        tokens: TokenUsage,
        cost_usd: f64,
        error_details: Option<ErrorDetails>,
        abort_reason: Option<AbortReason>,
    ) -> CanonicalTurnEvent {
        let total_duration_ms = self.start_time.elapsed().as_millis() as u64;

        // Use accumulated LLM duration (for multi-call turns)
        let llm_duration_ms = self.accumulated_llm_duration_ms;

        let tool_duration_ms: u64 = self.completed_tools.iter().map(|t| t.duration_ms).sum();

        let queue_wait_ms = self.queue_start_time.map(|queue_start| {
            self.start_time
                .saturating_duration_since(queue_start)
                .as_millis() as u64
        });

        // Apply tail sampling
        let (sampled, sample_reason) = self.should_sample(status, total_duration_ms);

        // Terminal events establish counts even when approval did not emit a
        // ToolCall. Keep measured timing separate instead of inventing a start.
        let tool_count = self
            .observed_tool_terminals
            .values()
            .filter(|result| result.is_some())
            .count() as u32;
        let tool_success_count = self
            .observed_tool_terminals
            .values()
            .filter(|result| **result == Some(true))
            .count() as u32;
        let tool_failure_count = tool_count - tool_success_count;

        CanonicalTurnEvent {
            event_type: "canonical-turn".to_string(),
            timestamp: Utc::now().to_rfc3339(),

            // Identity
            session_id: self.session_id,
            turn_id: self.turn_id,
            turn_number: self.turn_number,
            trace_id: self.trace_id,
            diagnostics: None,
            identity_scope: None,

            // Model
            model: self.model,

            // Timing
            total_duration_ms,
            llm_duration_ms,
            tool_duration_ms,
            queue_wait_ms,

            // Tools
            tools: self.completed_tools,
            tool_count,
            tool_success_count,
            tool_failure_count,
            tool_outcomes: self.tool_outcomes,

            // Tokens
            tokens,
            cost_usd,
            reported_cost_usd: None,
            measurements: Some(self.measurements),

            // Business context
            sandbox_mode: self.sandbox_mode,
            approval_mode: self.approval_mode,
            mcp_server_count: self.mcp_servers.len() as u32,
            mcp_servers: if self.mcp_servers.is_empty() {
                None
            } else {
                Some(self.mcp_servers)
            },
            context_source_count: self.context_source_count,
            message_count: self.message_count,
            input_size_bytes: self.input_size_bytes,
            output_size_bytes: self.output_size_bytes,

            // Features
            features: self.features,

            // Outcome
            status,
            error_category: error_details.as_ref().and_then(|e| e.category.clone()),
            error_message: error_details.as_ref().and_then(|e| e.message.clone()),
            abort_reason,

            // Sampling
            sampled,
            sample_reason,
        }
    }

    // ─── Sampling Logic ───────────────────────────────────────────────────────

    fn should_sample(&self, status: TurnStatus, total_duration_ms: u64) -> (bool, SampleReason) {
        // Always retain non-success outcomes, including fast user cancellations.
        if status == TurnStatus::Error {
            return (true, SampleReason::Error);
        }

        if status == TurnStatus::Aborted {
            return (true, SampleReason::Always);
        }

        // Always sample first N turns
        if self.turn_number <= self.sampling_config.always_sample_first_n {
            return (true, SampleReason::FirstTurn);
        }

        // Always sample slow turns
        if total_duration_ms >= self.sampling_config.slow_threshold_ms {
            return (true, SampleReason::Slow);
        }

        // Random sampling for successful fast turns
        let mut rng = rand::rng();
        if rng.random::<f64>() < self.sampling_config.success_sample_rate {
            return (true, SampleReason::Random);
        }

        (false, SampleReason::Random)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_only_receipts_are_counted_once_without_pending_timing() {
        use maestro_runtime::{
            ExecutionReceipt, ExecutionSource, ExecutionStatus, ToolReceiptDetails,
        };
        let mut collector = TurnCollector::new("fixture", 1, TailSamplingConfig::default());
        let mut receipt = ExecutionReceipt {
            code_authority: None,
            call_id: "native-call".to_owned(),
            tool_name: "bash".to_owned(),
            source: ExecutionSource::Native,
            status: ExecutionStatus::Succeeded,
            duration_ms: Some(17),
            policy: None,
            details: ToolReceiptDetails::None,
        };
        collector.record_tool_receipt("native-call", Some(&receipt));
        collector.record_tool_receipt("native-call", Some(&receipt));
        collector.record_tool_end("native-call", true, None, None);
        collector.record_tool_receipt("wrong-call", Some(&receipt));
        receipt.call_id = "denied-call".to_owned();
        receipt.status = ExecutionStatus::Denied;
        collector.record_tool_receipt("denied-call", Some(&receipt));
        assert_eq!(collector.tool_outcomes.succeeded, 1);
        assert_eq!(collector.tool_outcomes.denied, 1);
        assert_eq!(collector.tool_outcomes.unknown, 1);
        assert_eq!(collector.tool_outcomes.measured_execution_count, 1);
        assert_eq!(collector.tool_outcomes.execution_duration_ms, 17);
        assert!(collector.completed_tools.is_empty());
    }

    #[test]
    fn measurement_coverage_counts_actual_responses_and_compaction() {
        let mut collector = TurnCollector::new("fixture", 1, TailSamplingConfig::default());
        collector.record_response_coverage(false, false);
        collector.record_response_coverage(true, false);
        collector.record_response_coverage(true, true);
        collector.record_compaction(true, 1200);
        collector.record_compaction(false, 300);
        collector.record_compaction_duration(42);
        let event = collector.complete(TurnStatus::Success, TokenUsage::default(), 0.0, None, None);
        let m = event.measurements.unwrap();
        assert_eq!(
            (
                m.response_count,
                m.responses_with_usage,
                m.responses_with_cost
            ),
            (3, 2, 1)
        );
        assert_eq!(
            (
                m.compaction_count,
                m.automatic_compaction_count,
                m.compacted_input_tokens
            ),
            (2, 1, 1500)
        );
        assert_eq!(m.compaction_duration_ms, Some(42));
        assert_eq!(m.stream_stall_count, None);
    }

    #[test]
    fn test_turn_collector_basic() {
        let mut collector = TurnCollector::new("session-1", 1, TailSamplingConfig::default());

        collector.set_model(ModelInfo {
            id: "claude-opus-4-5-20251101".to_string(),
            provider: "anthropic".to_string(),
            thinking_level: ThinkingLevel::Medium,
        });

        collector.record_tool_start("bash", "call-1", Some(100));
        std::thread::sleep(std::time::Duration::from_millis(10));
        collector.record_tool_end("call-1", true, Some(50), None);

        let event = collector.complete(
            TurnStatus::Success,
            TokenUsage {
                input: 1000,
                output: 500,
                ..Default::default()
            },
            0.05,
            None,
            None,
        );

        assert_eq!(event.event_type, "canonical-turn");
        assert_eq!(event.session_id, "session-1");
        assert_eq!(event.turn_number, 1);
        assert_eq!(event.tool_count, 1);
        assert_eq!(event.tool_success_count, 1);
        assert!(event.sampled); // First turn is always sampled
        assert_eq!(event.sample_reason, SampleReason::FirstTurn);
    }

    #[test]
    fn fast_aborted_turns_are_retained_when_success_sampling_is_disabled() {
        let collector = TurnCollector::new(
            "fixture",
            10,
            TailSamplingConfig {
                success_sample_rate: 0.0,
                always_sample_first_n: 0,
                ..Default::default()
            },
        );
        assert_eq!(
            collector.should_sample(TurnStatus::Aborted, 0),
            (true, SampleReason::Always)
        );
        assert_eq!(
            collector.should_sample(TurnStatus::Success, 0),
            (false, SampleReason::Random)
        );
    }

    #[test]
    fn test_sampling_error_always_sampled() {
        let collector = TurnCollector::new("session-1", 10, TailSamplingConfig::default());

        let event = collector.complete(
            TurnStatus::Error,
            TokenUsage::default(),
            0.0,
            Some(ErrorDetails {
                category: Some("network".to_string()),
                message: Some("Connection failed".to_string()),
            }),
            None,
        );

        assert!(event.sampled);
        assert_eq!(event.sample_reason, SampleReason::Error);
    }

    #[test]
    fn test_sampling_slow_always_sampled() {
        let config = TailSamplingConfig {
            slow_threshold_ms: 1, // Very low threshold
            ..Default::default()
        };
        let collector = TurnCollector::new("session-1", 10, config);

        // Wait a bit to exceed threshold
        std::thread::sleep(std::time::Duration::from_millis(5));

        let event = collector.complete(TurnStatus::Success, TokenUsage::default(), 0.0, None, None);

        assert!(event.sampled);
        assert_eq!(event.sample_reason, SampleReason::Slow);
    }

    #[test]
    fn test_tool_timing() {
        let mut collector = TurnCollector::new("session-1", 1, TailSamplingConfig::default());

        collector.record_tool_start("read", "call-1", None);
        std::thread::sleep(std::time::Duration::from_millis(20));
        collector.record_tool_end("call-1", true, None, None);

        collector.record_tool_start("write", "call-2", None);
        std::thread::sleep(std::time::Duration::from_millis(10));
        collector.record_tool_end("call-2", false, None, Some("permission_denied".to_string()));

        let event = collector.complete(TurnStatus::Success, TokenUsage::default(), 0.0, None, None);

        assert_eq!(event.tool_count, 2);
        assert_eq!(event.tool_success_count, 1);
        assert_eq!(event.tool_failure_count, 1);
        assert!(event.tool_duration_ms >= 30);
    }

    #[test]
    fn the_serializable_turn_event_is_the_projection_and_it_omits_the_session_id() {
        let collector = TurnCollector::new("session-1", 1, TailSamplingConfig::default());
        let event = collector.complete(TurnStatus::Success, TokenUsage::default(), 0.0, None, None);

        // `serde_json::to_string(&event)` does not compile: `CanonicalTurnEvent`
        // is not `Serialize`. The projection is the only encodable turn event.
        let json = serde_json::to_string(&event.external_projection()).unwrap();
        assert!(json.contains("\"type\":\"canonical-turn\""), "{json}");
        assert!(!json.contains("session-1"), "{json}");
        assert!(!json.contains("session_id"), "{json}");
    }

    #[test]
    fn a_canary_in_error_message_never_reaches_the_exported_bytes() {
        const CANARY: &str = "MAESTRO-TELEMETRY-CANARY-8f21c0";

        let collector = TurnCollector::new("canary-session", 7, TailSamplingConfig::default());
        let event = collector.complete(
            TurnStatus::Error,
            TokenUsage::default(),
            0.0,
            Some(ErrorDetails {
                category: Some("provider_stream".to_string()),
                message: Some(format!("upstream said {CANARY}")),
            }),
            None,
        );
        let raw_message = format!("upstream said {CANARY}");
        assert_eq!(event.error_message.as_deref(), Some(raw_message.as_str()));

        let exported = serde_json::to_vec(&event.external_projection()).unwrap();
        let exported = String::from_utf8(exported).unwrap();
        assert!(
            !exported.contains(CANARY),
            "error_message reached the external boundary: {exported}"
        );
        assert!(
            exported.contains("provider_stream"),
            "error_category is the replacement dimension and must survive: {exported}"
        );
    }

    #[test]
    fn external_projection_excludes_content_and_identifiers() {
        let mut collector = TurnCollector::new("secret-session", 1, TailSamplingConfig::default());
        collector
            .set_model(ModelInfo {
                id: "private-model-deployment".to_string(),
                provider: "openai".to_string(),
                thinking_level: ThinkingLevel::Medium,
            })
            .set_mcp_servers(vec!["customer-filesystem".to_string()]);
        collector.record_tool_start("bash /Users/alice/private", "secret-call-id", Some(12));
        collector.record_tool_end(
            "secret-call-id",
            false,
            Some(5),
            Some("api_key=sk-secret".to_string()),
        );
        let event = collector.complete(
            TurnStatus::Error,
            TokenUsage::default(),
            0.0,
            Some(ErrorDetails {
                category: Some("provider".to_string()),
                message: Some("failed reading /Users/alice/private sk-secret".to_string()),
            }),
            None,
        );

        let json = serde_json::to_string(&event.external_projection()).unwrap();
        for secret in [
            "secret-session",
            "private-model-deployment",
            "customer-filesystem",
            "secret-call-id",
            "/Users/alice/private",
            "sk-secret",
        ] {
            assert!(!json.contains(secret), "external event leaked {secret}");
        }
        assert!(json.contains("\"schema_version\":1"));
        assert!(json.contains("\"tool_failure_count\":1"));
    }
}
