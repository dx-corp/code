//! Native `maestro mission` command — durable mission state, artifacts, and progress.
//!
//! Ports the TypeScript `mission` CLI (init | status | record | set-state | validate)
//! plus enough mission-store / mission-artifacts domain for parity with the CLI surface
//! and core store/artifact behaviors.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use maestro_runtime::coding_acceptance::{
    CODING_ACCEPTANCE_CHILD_RECORDS_KEY, CODING_ACCEPTANCE_METADATA_KEY,
    CODING_ACCEPTANCE_RESULT_METADATA_KEY, CodingAcceptanceChildRecord, CodingAcceptanceContract,
    CodingAcceptanceScope, CodingCompletionSubmission, evaluate_coding_acceptance,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::path_utils::{env_path, maestro_home_dir};
use crate::skill_cli::write_atomic;

const MISSION_STORE_SCHEMA: &str = "evalops.maestro.mission-store.v1";
const MISSION_MANIFEST_VERSION: u32 = 1;
const MISSION_ARTIFACT_VERSION: u32 = 1;
const MISSION_STATE_LOCK_STALE_MS: u128 = 60_000;
const MISSION_STATE_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const MISSION_ARTIFACT_ESCAPE_MESSAGE: &str =
    "Mission artifact path resolves outside the mission store.";

// ── Types ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MissionState {
    AwaitingInput,
    Ready,
    Running,
    Blocked,
    Completed,
    Failed,
}

impl MissionState {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "awaiting-input" => Some(Self::AwaitingInput),
            "ready" => Some(Self::Ready),
            "running" => Some(Self::Running),
            "blocked" => Some(Self::Blocked),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::AwaitingInput => "awaiting-input",
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Blocked => "blocked",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MissionWorkerState {
    pub started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MissionTokenUsage {
    pub input_tokens: f64,
    pub output_tokens: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_tokens: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credits: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionProgressType {
    MissionCreated,
    MissionStarted,
    MissionBlocked,
    MissionCompleted,
    WorkerStarted,
    WorkerCompleted,
    WorkerFailed,
    Note,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MissionProgressEntry {
    #[serde(rename = "type")]
    pub entry_type: MissionProgressType,
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feature_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MissionFeatureStatus {
    Pending,
    InProgress,
    Passed,
    Failed,
    Preempted,
}

impl MissionFeatureStatus {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "in-progress" => Some(Self::InProgress),
            "passed" => Some(Self::Passed),
            "failed" => Some(Self::Failed),
            "preempted" => Some(Self::Preempted),
            _ => None,
        }
    }
}

/// Feature blob kept as JSON so unknown optional fields round-trip cleanly.
pub type MissionFeature = Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MissionStoreSnapshot {
    pub schema_version: String,
    pub mission_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_mission_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub state: MissionState,
    #[serde(default)]
    pub features: Vec<MissionFeature>,
    #[serde(default)]
    pub progress_log: Vec<MissionProgressEntry>,
    #[serde(default)]
    pub worker_session_ids: Vec<String>,
    #[serde(default)]
    pub worker_states: BTreeMap<String, MissionWorkerState>,
    #[serde(default)]
    pub token_usage_by_session_id: BTreeMap<String, MissionTokenUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_usage: Option<MissionTokenUsage>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Default)]
pub struct MissionStoreConfig {
    pub root_dir: Option<PathBuf>,
    pub now: Option<fn() -> String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MissionArtifactLayout {
    pub mission_dir: PathBuf,
    pub mission_markdown: PathBuf,
    pub architecture_markdown: PathBuf,
    pub validation_contract_markdown: PathBuf,
    pub validation_state_json: PathBuf,
    pub features_json: PathBuf,
    pub agents_markdown: PathBuf,
    pub services_yaml: PathBuf,
    pub handoffs_dir: PathBuf,
    pub library_dir: PathBuf,
    pub skills_dir: PathBuf,
    pub state_json: PathBuf,
    pub progress_log_jsonl: PathBuf,
    pub model_settings_json: PathBuf,
}

// ── Path helpers ────────────────────────────────────────────────────────────

pub fn get_mission_store_root(root_dir: Option<&Path>) -> PathBuf {
    if let Some(root) = root_dir {
        return root.to_path_buf();
    }
    if let Some(from_env) = env_path("MAESTRO_MISSION_STORE_DIR") {
        return from_env;
    }
    maestro_home_dir()
        .unwrap_or_else(|| PathBuf::from(".maestro"))
        .join("missions")
}

pub fn sanitize_mission_id(mission_id: &str) -> Result<String> {
    let trimmed = normalize_mission_id_input(mission_id)?;
    let mut safe = String::with_capacity(trimmed.len());
    let mut last_dash = false;
    for ch in trimmed.chars() {
        if ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' || ch == '-' {
            safe.push(ch);
            last_dash = ch == '-';
        } else if !last_dash {
            safe.push('-');
            last_dash = true;
        }
    }
    let safe = safe.trim_matches('-').to_string();
    if safe.is_empty() || !safe.chars().any(|c| c.is_ascii_alphanumeric()) {
        bail!(
            "{}",
            crate::localization::cli_locale().format(
                "missionId must include at least one alphanumeric character",
                &[]
            )
        );
    }
    Ok(safe)
}

fn normalize_mission_id_input(mission_id: &str) -> Result<String> {
    let trimmed = mission_id.trim();
    if trimmed.is_empty() {
        bail!(
            "{}",
            crate::localization::cli_locale().format("missionId is required", &[])
        );
    }
    Ok(trimmed.to_string())
}

pub fn get_mission_dir(mission_id: &str, root_dir: Option<&Path>) -> Result<PathBuf> {
    Ok(get_mission_store_root(root_dir).join(sanitize_mission_id(mission_id)?))
}

pub fn get_mission_state_path(mission_id: &str, root_dir: Option<&Path>) -> Result<PathBuf> {
    Ok(get_mission_dir(mission_id, root_dir)?.join("state.json"))
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn default_now(config: &MissionStoreConfig) -> String {
    config.now.map(|f| f()).unwrap_or_else(now_iso)
}

// ── Feature validation ──────────────────────────────────────────────────────

fn is_mission_feature_status(value: &Value) -> bool {
    value
        .as_str()
        .and_then(MissionFeatureStatus::parse)
        .is_some()
}

fn is_nonempty_str(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty())
}

fn is_valid_iso(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|s| chrono::DateTime::parse_from_rfc3339(s).is_ok() || parse_js_date(s))
}

/// Accept the broader JS Date.parse subset used by TypeScript (ISO-ish).
fn parse_js_date(s: &str) -> bool {
    chrono::DateTime::parse_from_rfc3339(s).is_ok()
        || chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.fZ").is_ok()
        || chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%SZ").is_ok()
        || chrono::DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f%z").is_ok()
}

fn is_mission_handoff_item_kind(value: &Value) -> bool {
    matches!(value.as_str(), Some("unfinished_work" | "discovered_issue"))
}

fn is_mission_discovered_issue(value: &Value) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    matches!(
        obj.get("severity").and_then(Value::as_str),
        Some("blocking" | "non_blocking")
    ) && is_nonempty_str(obj.get("description"))
        && obj.get("suggestedFix").is_none_or(Value::is_string)
}

fn is_mission_verification_command(value: &Value) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    is_nonempty_str(obj.get("command"))
        && obj.get("exitCode").is_none_or(Value::is_number)
        && obj.get("observation").is_none_or(Value::is_string)
}

fn is_mission_verification(value: &Value) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    match obj.get("commandsRun") {
        None => true,
        Some(Value::Array(items)) => items.iter().all(is_mission_verification_command),
        _ => false,
    }
}

fn is_mission_worker_handoff(value: &Value) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    is_nonempty_str(obj.get("workerId"))
        && obj.get("success").is_some_and(Value::is_boolean)
        && obj.get("repoPath").is_none_or(Value::is_string)
        && obj.get("commitId").is_none_or(Value::is_string)
        && obj.get("summary").is_none_or(Value::is_string)
        && obj.get("whatWasImplemented").is_none_or(Value::is_string)
        && obj.get("whatWasLeftUndone").is_none_or(Value::is_string)
        && match obj.get("discoveredIssues") {
            None => true,
            Some(Value::Array(items)) => items.iter().all(is_mission_discovered_issue),
            _ => false,
        }
        && match obj.get("verification") {
            None => true,
            Some(v) => is_mission_verification(v),
        }
        && is_valid_iso(obj.get("handedOffAt"))
}

fn is_mission_handoff_dismissal(value: &Value) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    obj.get("kind").is_some_and(is_mission_handoff_item_kind)
        && is_nonempty_str(obj.get("key"))
        && is_nonempty_str(obj.get("justification"))
        && is_valid_iso(obj.get("dismissedAt"))
}

fn is_mission_tracked_handoff_item(value: &Value) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    is_nonempty_str(obj.get("sourceFeatureId"))
        && obj.get("kind").is_some_and(is_mission_handoff_item_kind)
        && is_nonempty_str(obj.get("key"))
        && is_valid_iso(obj.get("trackedAt"))
        && obj.get("note").is_none_or(Value::is_string)
}

pub fn is_mission_feature(value: &Value) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    is_nonempty_str(obj.get("id"))
        && is_nonempty_str(obj.get("description"))
        && obj.get("status").is_some_and(is_mission_feature_status)
        && obj.get("milestone").is_none_or(Value::is_string)
        && obj.get("skillName").is_none_or(Value::is_string)
        && match obj.get("fulfills") {
            Some(Value::Array(items)) => items.iter().all(Value::is_string),
            _ => false,
        }
        && match obj.get("handoff") {
            None => true,
            Some(v) => is_mission_worker_handoff(v),
        }
        && match obj.get("handoffDismissals") {
            None => true,
            Some(Value::Array(items)) => items.iter().all(is_mission_handoff_dismissal),
            _ => false,
        }
        && match obj.get("trackedHandoffItems") {
            None => true,
            Some(Value::Array(items)) => items.iter().all(is_mission_tracked_handoff_item),
            _ => false,
        }
        && obj
            .get("handoffSourceFeatureId")
            .is_none_or(Value::is_string)
        && match obj.get("handoffFollowUpKind") {
            None => true,
            Some(v) => is_mission_handoff_item_kind(v),
        }
        && obj.get("handoffItemKey").is_none_or(Value::is_string)
}

// ── JSON IO ─────────────────────────────────────────────────────────────────

fn write_json_file(path: &Path, value: &impl Serialize) -> Result<()> {
    let content = serde_json::to_string_pretty(value)?;
    // Match TS writeJsonFile: trailing newline after pretty JSON.
    let mut with_nl = content;
    if !with_nl.ends_with('\n') {
        with_nl.push('\n');
    }
    write_atomic(path, &with_nl)
}

fn read_json_value(path: &Path) -> Result<Value> {
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&content).with_context(|| format!("invalid JSON: {}", path.display()))
}

fn read_json_value_optional(path: &Path, rotate_on_parse_fail: bool) -> Result<Option<Value>> {
    if !path.exists() {
        return Ok(None);
    }
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    if content.trim().is_empty() {
        return Ok(None);
    }
    match serde_json::from_str(&content) {
        Ok(value) => Ok(Some(value)),
        Err(_) if rotate_on_parse_fail => {
            let _ = rotate_corrupt_json_file(path, &content);
            Ok(None)
        }
        Err(error) => Err(error).with_context(|| format!("invalid JSON: {}", path.display())),
    }
}

fn rotate_corrupt_json_file(path: &Path, _content: &str) -> Option<PathBuf> {
    let stamp = now_iso().replace(':', "-");
    let rotated = path.with_file_name(format!(
        "{}.corrupt.{}",
        path.file_name()?.to_string_lossy(),
        stamp
    ));
    fs::rename(path, &rotated).ok()?;
    Some(rotated)
}

// ── Snapshot helpers ────────────────────────────────────────────────────────

pub fn create_mission_store_snapshot(
    mission_id: &str,
    title: Option<&str>,
    features: Vec<MissionFeature>,
    now: &str,
) -> Result<MissionStoreSnapshot> {
    let source = normalize_mission_id_input(mission_id)?;
    let sanitized = sanitize_mission_id(mission_id)?;
    Ok(MissionStoreSnapshot {
        schema_version: MISSION_STORE_SCHEMA.to_string(),
        mission_id: sanitized,
        source_mission_id: Some(source),
        title: title.map(str::to_string),
        state: MissionState::AwaitingInput,
        features,
        progress_log: vec![MissionProgressEntry {
            entry_type: MissionProgressType::MissionCreated,
            timestamp: now.to_string(),
            message: title.map(str::to_string),
            feature_id: None,
            worker_session_id: None,
            exit_code: None,
        }],
        worker_session_ids: vec![],
        worker_states: BTreeMap::new(),
        token_usage_by_session_id: BTreeMap::new(),
        token_usage: None,
        created_at: now.to_string(),
        updated_at: now.to_string(),
    })
}

pub fn sum_mission_token_usage(
    usages: &BTreeMap<String, MissionTokenUsage>,
) -> Option<MissionTokenUsage> {
    if usages.is_empty() {
        return None;
    }
    let mut total = MissionTokenUsage {
        input_tokens: 0.0,
        output_tokens: 0.0,
        cache_creation_tokens: Some(0.0),
        cache_read_tokens: Some(0.0),
        thinking_tokens: Some(0.0),
        credits: Some(0.0),
    };
    for usage in usages.values() {
        total.input_tokens += usage.input_tokens;
        total.output_tokens += usage.output_tokens;
        *total.cache_creation_tokens.as_mut().unwrap() +=
            usage.cache_creation_tokens.unwrap_or(0.0);
        *total.cache_read_tokens.as_mut().unwrap() += usage.cache_read_tokens.unwrap_or(0.0);
        *total.thinking_tokens.as_mut().unwrap() += usage.thinking_tokens.unwrap_or(0.0);
        *total.credits.as_mut().unwrap() += usage.credits.unwrap_or(0.0);
    }
    Some(total)
}

fn derive_worker_state(
    progress_log: &[MissionProgressEntry],
    previous: &BTreeMap<String, MissionWorkerState>,
) -> BTreeMap<String, MissionWorkerState> {
    let mut next = previous.clone();
    for entry in progress_log {
        let Some(session_id) = entry.worker_session_id.as_ref() else {
            continue;
        };
        next.entry(session_id.clone())
            .or_insert_with(|| MissionWorkerState {
                started_at: entry.timestamp.clone(),
                completed_at: None,
                exit_code: None,
            });
        if matches!(
            entry.entry_type,
            MissionProgressType::WorkerCompleted | MissionProgressType::WorkerFailed
        ) {
            let started = next
                .get(session_id)
                .map(|s| s.started_at.clone())
                .unwrap_or_else(|| entry.timestamp.clone());
            next.insert(
                session_id.clone(),
                MissionWorkerState {
                    started_at: started,
                    completed_at: Some(entry.timestamp.clone()),
                    exit_code: entry.exit_code,
                },
            );
        }
    }
    next
}

fn feature_worker_ids(features: &[MissionFeature]) -> Vec<String> {
    let mut ids = Vec::new();
    for feature in features {
        if let Some(worker_id) = feature
            .get("handoff")
            .and_then(|h| h.get("workerId"))
            .and_then(Value::as_str)
        {
            ids.push(worker_id.to_string());
        }
    }
    ids
}

fn normalize_snapshot(mut snapshot: MissionStoreSnapshot) -> Result<MissionStoreSnapshot> {
    snapshot.schema_version = MISSION_STORE_SCHEMA.to_string();
    snapshot.mission_id = sanitize_mission_id(&snapshot.mission_id)?;
    let worker_states = derive_worker_state(&snapshot.progress_log, &snapshot.worker_states);
    let mut ids: BTreeSet<String> = snapshot.worker_session_ids.into_iter().collect();
    ids.extend(worker_states.keys().cloned());
    ids.extend(feature_worker_ids(&snapshot.features));
    snapshot.worker_session_ids = ids.into_iter().collect();
    snapshot.worker_states = worker_states;
    snapshot.token_usage = sum_mission_token_usage(&snapshot.token_usage_by_session_id);
    Ok(snapshot)
}

fn snapshot_from_value(value: Value) -> Result<MissionStoreSnapshot> {
    let snapshot: MissionStoreSnapshot =
        serde_json::from_value(value).context("invalid mission store snapshot")?;
    normalize_snapshot(snapshot)
}

fn json_equal(left: &impl Serialize, right: &impl Serialize) -> bool {
    let l = serde_json::to_value(left).unwrap_or(Value::Null);
    let r = serde_json::to_value(right).unwrap_or(Value::Null);
    l == r
}

fn field_changed<T: Serialize>(intended: &T, base: &T) -> bool {
    !json_equal(intended, base)
}

fn max_iso_timestamp(left: &str, right: &str) -> String {
    if left >= right {
        left.to_string()
    } else {
        right.to_string()
    }
}

fn progress_entry_key(entry: &MissionProgressEntry) -> String {
    serde_json::to_string(entry).unwrap_or_default()
}

fn state_to_progress_type(state: MissionState) -> MissionProgressType {
    match state {
        MissionState::Running => MissionProgressType::MissionStarted,
        MissionState::Blocked => MissionProgressType::MissionBlocked,
        MissionState::Completed => MissionProgressType::MissionCompleted,
        _ => MissionProgressType::Note,
    }
}

fn default_state_progress_message(state: MissionState) -> Option<&'static str> {
    match state {
        MissionState::Running => Some("Mission started"),
        MissionState::Blocked => Some("Mission is blocked"),
        MissionState::Completed => Some("Mission completed"),
        _ => None,
    }
}

// ── Merge ───────────────────────────────────────────────────────────────────

fn merge_mission_state(
    base: &MissionStoreSnapshot,
    intended: &MissionStoreSnapshot,
    existing: &MissionStoreSnapshot,
    state_touched: bool,
) -> MissionState {
    let intended_changed = state_touched || field_changed(&intended.state, &base.state);
    if !intended_changed {
        return existing.state;
    }
    if existing.state.is_terminal() && existing.state != intended.state {
        return existing.state;
    }
    if !field_changed(&existing.state, &base.state) || intended.state == existing.state {
        return intended.state;
    }
    existing.state
}

fn without_rejected_state_transition_progress(
    base: &MissionStoreSnapshot,
    intended: &MissionStoreSnapshot,
    merged_state: MissionState,
    state_touched: bool,
) -> Vec<MissionProgressEntry> {
    let intended_changed = state_touched || field_changed(&intended.state, &base.state);
    if !intended_changed || merged_state == intended.state {
        return intended.progress_log.clone();
    }
    let Some(last) = intended.progress_log.last() else {
        return intended.progress_log.clone();
    };
    let base_keys: HashSet<String> = base.progress_log.iter().map(progress_entry_key).collect();
    if base_keys.contains(&progress_entry_key(last))
        || last.timestamp != intended.updated_at
        || last.entry_type != state_to_progress_type(intended.state)
    {
        return intended.progress_log.clone();
    }
    intended.progress_log[..intended.progress_log.len() - 1].to_vec()
}

fn merge_progress_log(
    existing: &[MissionProgressEntry],
    base: &[MissionProgressEntry],
    intended: &[MissionProgressEntry],
) -> Vec<MissionProgressEntry> {
    let base_keys: HashSet<String> = base.iter().map(progress_entry_key).collect();
    let mut existing_keys: HashSet<String> = existing.iter().map(progress_entry_key).collect();
    let mut next = existing.to_vec();
    for entry in intended {
        let key = progress_entry_key(entry);
        if base_keys.contains(&key) || existing_keys.contains(&key) {
            continue;
        }
        next.push(entry.clone());
        existing_keys.insert(key);
    }
    next.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
    next
}

fn merge_features(
    base: &[MissionFeature],
    intended: &[MissionFeature],
    existing: &[MissionFeature],
) -> Result<Vec<MissionFeature>> {
    let feature_id = |f: &MissionFeature| -> Option<String> {
        f.get("id").and_then(Value::as_str).map(str::to_string)
    };
    let base_by_id: BTreeMap<String, &MissionFeature> = base
        .iter()
        .filter_map(|f| feature_id(f).map(|id| (id, f)))
        .collect();
    let intended_by_id: BTreeMap<String, &MissionFeature> = intended
        .iter()
        .filter_map(|f| feature_id(f).map(|id| (id, f)))
        .collect();
    let existing_by_id: BTreeMap<String, &MissionFeature> = existing
        .iter()
        .filter_map(|f| feature_id(f).map(|id| (id, f)))
        .collect();

    let mut ordered_ids = Vec::new();
    let mut seen = HashSet::new();
    for f in existing.iter().chain(intended.iter()) {
        if let Some(id) = feature_id(f) {
            if seen.insert(id.clone()) {
                ordered_ids.push(id);
            }
        }
    }

    let mut merged = Vec::new();
    for id in ordered_ids {
        let base_f = base_by_id.get(&id).copied();
        let intended_f = intended_by_id.get(&id).copied();
        let existing_f = existing_by_id.get(&id).copied();
        let intended_changed = match (intended_f, base_f) {
            (Some(i), Some(b)) => !json_equal(i, b),
            (Some(_), None) => true,
            (None, Some(_)) => true,
            (None, None) => false,
        };
        let existing_changed = match (existing_f, base_f) {
            (Some(e), Some(b)) => !json_equal(e, b),
            (Some(_), None) => true,
            (None, Some(_)) => true,
            (None, None) => false,
        };
        if !intended_changed {
            if let Some(e) = existing_f {
                merged.push(e.clone());
            }
            continue;
        }
        if !existing_changed
            || match (intended_f, existing_f) {
                (Some(i), Some(e)) => json_equal(i, e),
                (None, None) => true,
                _ => false,
            }
        {
            if let Some(i) = intended_f {
                merged.push(i.clone());
            }
            continue;
        }
        bail!(
            "{}",
            crate::localization::cli_locale().format(
                "mission feature {0} changed concurrently; reload before saving",
                std::slice::from_ref(&(id))
            )
        );
    }
    Ok(merged)
}

fn merge_snapshots(
    base: &MissionStoreSnapshot,
    intended: &MissionStoreSnapshot,
    existing: &MissionStoreSnapshot,
    state_touched: bool,
) -> Result<MissionStoreSnapshot> {
    let state = merge_mission_state(base, intended, existing, state_touched);
    let filtered = without_rejected_state_transition_progress(base, intended, state, state_touched);
    let progress_log = merge_progress_log(&existing.progress_log, &base.progress_log, &filtered);

    let mut token_usage_by_session_id = existing.token_usage_by_session_id.clone();
    for (session_id, usage) in &intended.token_usage_by_session_id {
        let base_usage = base.token_usage_by_session_id.get(session_id);
        if base_usage.map(|b| !json_equal(usage, b)).unwrap_or(true) {
            token_usage_by_session_id.insert(session_id.clone(), usage.clone());
        }
    }

    let title = if field_changed(&intended.title, &base.title) {
        intended.title.clone()
    } else {
        existing.title.clone()
    };

    normalize_snapshot(MissionStoreSnapshot {
        schema_version: existing.schema_version.clone(),
        mission_id: existing.mission_id.clone(),
        source_mission_id: existing.source_mission_id.clone(),
        title,
        state,
        features: merge_features(&base.features, &intended.features, &existing.features)?,
        progress_log,
        worker_session_ids: existing.worker_session_ids.clone(),
        worker_states: existing.worker_states.clone(),
        token_usage_by_session_id,
        token_usage: None,
        created_at: existing.created_at.clone(),
        updated_at: max_iso_timestamp(&existing.updated_at, &intended.updated_at),
    })
}

// ── Artifact features overlay ───────────────────────────────────────────────

/// An admitted coding contract cannot be removed through the generic feature
/// editing/overlay path, which would otherwise turn a governed task into a legacy
/// feature immediately before completion. Replanning uses a new task admission.
fn preserve_coding_contracts(
    previous: &MissionStoreSnapshot,
    next: &MissionStoreSnapshot,
) -> Result<()> {
    for feature in &previous.features {
        let Some(contract) = feature.get(CODING_ACCEPTANCE_METADATA_KEY) else {
            continue;
        };
        let id = feature.get("id");
        let replacement = next
            .features
            .iter()
            .find(|candidate| candidate.get("id") == id);
        if replacement.and_then(|feature| feature.get(CODING_ACCEPTANCE_METADATA_KEY))
            != Some(contract)
        {
            bail!("{}", crate::localization::cli_locale().format("An admitted coding contract cannot be removed or replaced through mission features", &[]));
        }
    }
    Ok(())
}

/// Reuse the owner's acceptance evaluator at every local terminal write. Stored
/// child records originate in the native executor; submissions alone never
/// create them. This guards CLI/artifact shortcuts, not privileged file tampering.
fn validate_coding_completion(
    snapshot: &MissionStoreSnapshot,
    check_live_head: bool,
) -> Result<()> {
    for feature in &snapshot.features {
        let Some(contract_value) = feature.get(CODING_ACCEPTANCE_METADATA_KEY) else {
            continue;
        };
        let contract: CodingAcceptanceContract = serde_json::from_value(contract_value.clone())
            .context("Invalid admitted coding acceptance contract")?;
        contract.validate().map_err(|error| anyhow!(error))?;
        if feature.get("id").and_then(Value::as_str) != Some(contract.task_id.as_str()) {
            bail!(
                "{}",
                crate::localization::cli_locale().format(
                    "Coding contract task does not match its mission feature",
                    &[]
                )
            );
        }
        if snapshot.state != MissionState::Completed
            && feature.get("status").and_then(Value::as_str) != Some("passed")
        {
            continue;
        }
        let submission: CodingCompletionSubmission = serde_json::from_value(
            feature
                .get(CODING_ACCEPTANCE_RESULT_METADATA_KEY)
                .cloned()
                .ok_or_else(|| {
                    anyhow!(
                        "{}",
                        crate::localization::cli_locale().format(
                            "Coding task {0} has no completion submission",
                            std::slice::from_ref(&(contract.task_id))
                        )
                    )
                })?,
        )
        .context("Invalid coding completion submission")?;
        let workflow = feature.get("codingWorkflow").ok_or_else(|| {
            anyhow!(
                "{}",
                crate::localization::cli_locale()
                    .format("Coding completion requires native workflow identity", &[])
            )
        })?;
        for (key, expected) in [
            ("workId", submission.work_id.as_str()),
            (
                "implementationSessionId",
                submission.implementation_session_id.as_str(),
            ),
            ("revision", submission.revision.as_str()),
            ("contractDigest", submission.contract_digest.as_str()),
        ] {
            if workflow.get(key).and_then(Value::as_str) != Some(expected) {
                bail!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "Coding completion does not match workflow {0}",
                        &[(key).to_string()]
                    )
                );
            }
        }
        let children: Vec<CodingAcceptanceChildRecord> = serde_json::from_value(
            feature
                .get(CODING_ACCEPTANCE_CHILD_RECORDS_KEY)
                .cloned()
                .unwrap_or_else(|| json!([])),
        )
        .context("Invalid coding validation child records")?;
        let scope = CodingAcceptanceScope {
            organization_id: "",
            workspace_id: "",
            work_id: &submission.work_id,
            implementation_session_id: &submission.implementation_session_id,
        };
        let decision = evaluate_coding_acceptance(&contract, Some(&submission), &scope, &children);
        if !decision.accepted {
            bail!(
                "{}",
                crate::localization::cli_locale().format(
                    "Coding task {0} is not accepted: {1}",
                    &[
                        (contract.task_id).clone(),
                        (decision.reasons.join("; ")).clone()
                    ]
                )
            );
        }
        if check_live_head {
            let root = workflow
                .get("repositoryRoot")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    anyhow!(
                        "{}",
                        crate::localization::cli_locale()
                            .format("Coding completion requires its repository root", &[])
                    )
                })?;
            let root = Path::new(root);
            if !root.is_absolute() {
                bail!(
                    "{}",
                    crate::localization::cli_locale()
                        .format("Coding repository root must be absolute", &[])
                );
            }
            let actual_root = crate::git::repo_root(root).ok_or_else(|| {
                anyhow!(
                    "{}",
                    crate::localization::cli_locale()
                        .format("Cannot inspect coding repository root", &[])
                )
            })?;
            if dunce::canonicalize(root)? != dunce::canonicalize(actual_root)? {
                bail!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "Coding repository root does not match the actual checkout",
                        &[]
                    )
                );
            }
            let output = std::process::Command::new("git")
                .args(["rev-parse", "--verify", "HEAD"])
                .current_dir(root)
                .output()
                .context("Cannot inspect coding repository HEAD")?;
            if !output.status.success()
                || String::from_utf8_lossy(&output.stdout).trim() != submission.revision
            {
                bail!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "Coding completion revision is stale; rerun validation at current HEAD",
                        &[]
                    )
                );
            }
        }
    }
    Ok(())
}

fn validate_new_coding_completions(
    previous: Option<&MissionStoreSnapshot>,
    next: &MissionStoreSnapshot,
) -> Result<()> {
    validate_coding_completion(next, false)?;
    let completing_mission = next.state == MissionState::Completed
        && previous.is_none_or(|snapshot| snapshot.state != MissionState::Completed);
    for feature in &next.features {
        if feature.get(CODING_ACCEPTANCE_METADATA_KEY).is_none() {
            continue;
        }
        let old = previous.and_then(|snapshot| {
            snapshot
                .features
                .iter()
                .find(|old| old.get("id") == feature.get("id"))
        });
        let terminal_feature = next.state == MissionState::Completed
            || feature.get("status").and_then(Value::as_str) == Some("passed");
        let changed_terminal_proof = terminal_feature
            && old.is_none_or(|old| {
                old.get("status") != feature.get("status")
                    || old.get(CODING_ACCEPTANCE_RESULT_METADATA_KEY)
                        != feature.get(CODING_ACCEPTANCE_RESULT_METADATA_KEY)
                    || old.get("codingWorkflow") != feature.get("codingWorkflow")
                    || old.get(CODING_ACCEPTANCE_CHILD_RECORDS_KEY)
                        != feature.get(CODING_ACCEPTANCE_CHILD_RECORDS_KEY)
            });
        if completing_mission || changed_terminal_proof {
            let mut candidate = next.clone();
            candidate.features = vec![feature.clone()];
            validate_coding_completion(&candidate, true)?;
        }
    }
    Ok(())
}

fn get_valid_artifact_features(
    value: &Value,
    snapshot: &MissionStoreSnapshot,
) -> Option<Vec<MissionFeature>> {
    let obj = value.as_object()?;
    if !obj.get("version")?.is_number() {
        return None;
    }
    let mission_id_raw = obj.get("missionId")?.as_str()?;
    let mission_id = sanitize_mission_id(mission_id_raw).ok()?;
    if mission_id != snapshot.mission_id {
        return None;
    }
    let updated_at = obj.get("updatedAt")?.as_str()?;
    if chrono::DateTime::parse_from_rfc3339(updated_at).is_err() && !parse_js_date(updated_at) {
        return None;
    }
    if updated_at < snapshot.updated_at.as_str() {
        return None;
    }
    let features = obj.get("features")?.as_array()?;
    if features.is_empty() && !snapshot.features.is_empty() {
        return None;
    }
    let mut ids = HashSet::new();
    for feature in features {
        if !is_mission_feature(feature) {
            return None;
        }
        let id = feature.get("id")?.as_str()?;
        if !ids.insert(id.to_string()) {
            return None;
        }
    }
    Some(features.clone())
}

fn apply_artifact_features_to_snapshot(
    snapshot: MissionStoreSnapshot,
    root_dir: Option<&Path>,
) -> Result<MissionStoreSnapshot> {
    let features_path = get_mission_dir(&snapshot.mission_id, root_dir)?.join("features.json");
    if !features_path.exists() {
        return Ok(snapshot);
    }
    let Some(manifest) = read_json_value_optional(&features_path, true)? else {
        return Ok(snapshot);
    };
    match get_valid_artifact_features(&manifest, &snapshot) {
        Some(features) => {
            let updated_at = manifest
                .get("updatedAt")
                .and_then(Value::as_str)
                .unwrap_or(&snapshot.updated_at)
                .to_string();
            let applied = MissionStoreSnapshot {
                features,
                updated_at,
                ..snapshot.clone()
            };
            preserve_coding_contracts(&snapshot, &applied)?;
            validate_coding_completion(&applied, false)?;
            Ok(applied)
        }
        None => Ok(snapshot),
    }
}

fn is_newer_mission_manifest(manifest: &Value, snapshot: &MissionStoreSnapshot) -> bool {
    get_valid_artifact_features(manifest, snapshot).is_some()
        && manifest
            .get("updatedAt")
            .and_then(Value::as_str)
            .is_some_and(|ts| ts > snapshot.updated_at.as_str())
}

fn write_mission_manifest(snapshot: &MissionStoreSnapshot, root_dir: Option<&Path>) -> Result<()> {
    let path = get_mission_dir(&snapshot.mission_id, root_dir)?.join("features.json");
    let existing = read_json_value_optional(&path, true)?;
    if let Some(ref manifest) = existing {
        if is_newer_mission_manifest(manifest, snapshot) {
            return Ok(());
        }
    }
    let version = existing
        .as_ref()
        .and_then(|v| v.get("version"))
        .and_then(Value::as_u64)
        .unwrap_or(MISSION_MANIFEST_VERSION as u64);
    let milestones = existing
        .as_ref()
        .and_then(|v| v.get("milestones"))
        .cloned()
        .unwrap_or_else(|| json!([]));
    let created_at = existing
        .as_ref()
        .and_then(|v| v.get("createdAt"))
        .and_then(Value::as_str)
        .unwrap_or(&snapshot.created_at)
        .to_string();
    write_json_file(
        &path,
        &json!({
            "version": version,
            "missionId": snapshot.mission_id,
            "milestones": milestones,
            "features": snapshot.features,
            "createdAt": created_at,
            "updatedAt": snapshot.updated_at,
        }),
    )
}

// ── Locking ─────────────────────────────────────────────────────────────────

fn write_mission_lock_owner(lock_path: &Path) {
    let owner = json!({
        "pid": std::process::id(),
        "createdAt": now_iso(),
    });
    let _ = write_json_file(&lock_path.join("owner.json"), &owner);
}

fn is_process_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    #[cfg(unix)]
    {
        // SAFETY: kill(pid, 0) is a standard existence probe.
        let result = unsafe { libc::kill(pid, 0) };
        if result == 0 {
            return true;
        }
        let err = std::io::Error::last_os_error();
        err.raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

fn recover_stale_mission_state_lock(lock_path: &Path) -> bool {
    let Ok(meta) = fs::metadata(lock_path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    let age_ms = SystemTime::now()
        .duration_since(modified)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    if age_ms <= MISSION_STATE_LOCK_STALE_MS {
        return false;
    }
    let owner_path = lock_path.join("owner.json");
    if let Ok(Some(owner)) = read_json_value_optional(&owner_path, false) {
        if let Some(pid) = owner.get("pid").and_then(Value::as_i64) {
            if is_process_alive(pid as i32) {
                return false;
            }
        }
    }
    let _ = fs::remove_dir_all(lock_path);
    true
}

fn with_mission_state_lock<T>(path: &Path, operation: impl FnOnce() -> Result<T>) -> Result<T> {
    let lock_path = PathBuf::from(format!("{}.lock", path.display()));
    let started = Instant::now();
    loop {
        match fs::create_dir(&lock_path) {
            Ok(()) => {
                write_mission_lock_owner(&lock_path);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if recover_stale_mission_state_lock(&lock_path) {
                    continue;
                }
                if started.elapsed() > MISSION_STATE_LOCK_TIMEOUT {
                    bail!(
                        "{}",
                        crate::localization::cli_locale().format(
                            "timed out waiting for mission state lock: {0}",
                            &[(path.display()).to_string()]
                        )
                    );
                }
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => {
                // Parent may be missing — create and retry once path is ready.
                if let Some(parent) = path.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                if error.kind() == std::io::ErrorKind::NotFound {
                    if let Some(parent) = lock_path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    continue;
                }
                return Err(error).with_context(|| {
                    format!(
                        "failed to acquire mission state lock: {}",
                        lock_path.display()
                    )
                });
            }
        }
    }
    let result = operation();
    let _ = fs::remove_dir_all(&lock_path);
    result
}

// ── MissionStore ────────────────────────────────────────────────────────────

pub struct MissionStore {
    snapshot: MissionStoreSnapshot,
    last_saved_snapshot: MissionStoreSnapshot,
    state_touched: bool,
    root_dir: Option<PathBuf>,
    now: fn() -> String,
}

impl MissionStore {
    pub fn new(snapshot: MissionStoreSnapshot, config: MissionStoreConfig) -> Result<Self> {
        let snapshot = normalize_snapshot(snapshot)?;
        validate_coding_completion(&snapshot, false)?;
        let now = config.now.unwrap_or(now_iso);
        Ok(Self {
            last_saved_snapshot: snapshot.clone(),
            snapshot,
            state_touched: false,
            root_dir: config.root_dir,
            now,
        })
    }

    pub fn create(
        mission_id: &str,
        title: Option<&str>,
        config: MissionStoreConfig,
    ) -> Result<Self> {
        assert_mission_create_target_available(mission_id, config.root_dir.as_deref())?;
        let now = default_now(&config);
        let snapshot = create_mission_store_snapshot(mission_id, title, vec![], &now)?;
        Self::new(snapshot, config)
    }

    pub fn load(mission_id: &str, config: MissionStoreConfig) -> Result<Self> {
        let path = get_mission_state_path(mission_id, config.root_dir.as_deref())?;
        if !path.exists() {
            bail!(
                "{}",
                crate::localization::cli_locale().format(
                    "mission not found: {0}",
                    &[sanitize_mission_id(mission_id)?.clone()]
                )
            );
        }
        let requested_id = normalize_mission_id_input(mission_id)?;
        let value = read_json_value(&path)?;
        let mut snapshot = snapshot_from_value(value)?;
        if let Some(source) = snapshot
            .source_mission_id
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            if source != requested_id && snapshot.mission_id != requested_id {
                bail!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "missionId \"{0}\" collides with existing mission \"{1}\"",
                        &[(requested_id).clone(), (source).clone()]
                    )
                );
            }
        }
        snapshot = apply_artifact_features_to_snapshot(snapshot, config.root_dir.as_deref())?;
        Self::new(snapshot, config)
    }

    pub fn get_snapshot(&self) -> Result<MissionStoreSnapshot> {
        let snapshot = normalize_snapshot(self.snapshot.clone())?;
        validate_coding_completion(&snapshot, false)?;
        Ok(snapshot)
    }

    pub fn save(&mut self) -> Result<MissionStoreSnapshot> {
        let snapshot = self.get_snapshot()?;
        let path = get_mission_state_path(&snapshot.mission_id, self.root_dir.as_deref())?;
        let dir = get_mission_dir(&snapshot.mission_id, self.root_dir.as_deref())?;
        fs::create_dir_all(&dir)?;

        let root = self.root_dir.clone();
        let last_saved = self.last_saved_snapshot.clone();
        let state_touched = self.state_touched;
        let intended = snapshot;

        let merged = with_mission_state_lock(&path, || {
            assert_no_mission_id_collision(&path, &intended)?;
            let persisted_existing = if path.exists() {
                let value = read_json_value(&path)?;
                Some(snapshot_from_value(value)?)
            } else {
                None
            };
            let existing = persisted_existing
                .clone()
                .map(|snapshot| apply_artifact_features_to_snapshot(snapshot, root.as_deref()))
                .transpose()?;
            let merged = if let Some(ref existing) = existing {
                merge_snapshots(&last_saved, &intended, existing, state_touched)?
            } else {
                intended.clone()
            };
            preserve_coding_contracts(&last_saved, &merged)?;
            if let Some(existing) = &existing {
                preserve_coding_contracts(existing, &merged)?;
            }
            // Previously accepted features remain historical receipts while a
            // later feature changes the checkout. Recheck HEAD on new terminal
            // submissions and on the mission's transition to completed.
            validate_new_coding_completions(persisted_existing.as_ref(), &merged)?;
            if let Err(error) = (|| -> Result<()> {
                write_json_file(&path, &merged)?;
                write_mission_manifest(&merged, root.as_deref())?;
                Ok(())
            })() {
                restore_mission_state_file(&path, existing.as_ref())?;
                return Err(error);
            }
            Ok(merged)
        })?;

        self.snapshot = merged.clone();
        self.last_saved_snapshot = merged.clone();
        self.state_touched = false;
        Ok(merged)
    }

    pub fn set_state(
        &mut self,
        state: MissionState,
        message: Option<&str>,
    ) -> Result<MissionStoreSnapshot> {
        if self.snapshot.state.is_terminal() {
            if self.snapshot.state != state {
                bail!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "mission {0} is already {1}",
                        &[
                            (self.snapshot.mission_id).clone(),
                            (self.snapshot.state.as_str()).to_string()
                        ]
                    )
                );
            }
            return self.get_snapshot();
        }
        let timestamp = (self.now)();
        let progress_type = state_to_progress_type(state);
        let previous = self.snapshot.clone();
        let previous_touched = self.state_touched;
        self.state_touched = true;

        let mut progress_log = self.snapshot.progress_log.clone();
        let should_skip_note = progress_type == MissionProgressType::Note
            && message.map(str::trim).unwrap_or("").is_empty();
        if !should_skip_note {
            let msg = message
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .or_else(|| default_state_progress_message(state).map(str::to_string));
            progress_log.push(MissionProgressEntry {
                entry_type: progress_type,
                timestamp: timestamp.clone(),
                message: msg,
                feature_id: None,
                worker_session_id: None,
                exit_code: None,
            });
        }
        self.snapshot.state = state;
        self.snapshot.progress_log = progress_log;
        self.snapshot.updated_at = timestamp;
        self.save_or_restore(previous, previous_touched)
    }

    pub fn set_features(&mut self, features: Vec<MissionFeature>) -> Result<MissionStoreSnapshot> {
        let previous = self.snapshot.clone();
        let previous_touched = self.state_touched;
        self.snapshot.features = features;
        self.snapshot.updated_at = (self.now)();
        self.save_or_restore(previous, previous_touched)
    }

    pub fn append_progress(
        &mut self,
        mut entry: MissionProgressEntry,
    ) -> Result<MissionStoreSnapshot> {
        if entry.timestamp.is_empty() {
            entry.timestamp = (self.now)();
        }
        let previous = self.snapshot.clone();
        let previous_touched = self.state_touched;
        let ts = entry.timestamp.clone();
        self.snapshot.progress_log.push(entry);
        self.snapshot.updated_at = ts;
        self.save_or_restore(previous, previous_touched)
    }

    pub fn set_session_token_usage(
        &mut self,
        session_id: &str,
        token_usage: MissionTokenUsage,
    ) -> Result<MissionStoreSnapshot> {
        let previous = self.snapshot.clone();
        let previous_touched = self.state_touched;
        self.snapshot
            .token_usage_by_session_id
            .insert(session_id.to_string(), token_usage);
        self.snapshot.updated_at = (self.now)();
        self.save_or_restore(previous, previous_touched)
    }

    fn save_or_restore(
        &mut self,
        previous: MissionStoreSnapshot,
        previous_touched: bool,
    ) -> Result<MissionStoreSnapshot> {
        match self.save() {
            Ok(snapshot) => Ok(snapshot),
            Err(error) => {
                self.snapshot = previous;
                self.state_touched = previous_touched;
                Err(error)
            }
        }
    }
}

fn restore_mission_state_file(path: &Path, previous: Option<&MissionStoreSnapshot>) -> Result<()> {
    if let Some(prev) = previous {
        write_json_file(path, prev)?;
    } else {
        let _ = fs::remove_file(path);
    }
    Ok(())
}

fn assert_no_mission_id_collision(path: &Path, snapshot: &MissionStoreSnapshot) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let Some(existing_value) = read_json_value_optional(path, false)? else {
        return Ok(());
    };
    let existing: MissionStoreSnapshot = match serde_json::from_value(existing_value) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    let Some(existing_source) = existing.source_mission_id.as_ref() else {
        return Ok(());
    };
    let Some(snapshot_source) = snapshot.source_mission_id.as_ref() else {
        return Ok(());
    };
    if existing_source != snapshot_source {
        bail!(
            "{}",
            crate::localization::cli_locale().format(
                "missionId collision: {0} maps to existing mission {1}",
                &[(snapshot_source).clone(), (existing_source).clone()]
            )
        );
    }
    Ok(())
}

fn assert_mission_create_target_available(mission_id: &str, root_dir: Option<&Path>) -> Result<()> {
    let state_path = get_mission_state_path(mission_id, root_dir)?;
    if state_path.exists() {
        let value = read_json_value(&state_path)?;
        let snapshot = snapshot_from_value(value)?;
        let requested_id = normalize_mission_id_input(mission_id)?;
        if let Some(source) = snapshot
            .source_mission_id
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            if source != requested_id {
                bail!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "missionId \"{0}\" collides with existing mission \"{1}\"",
                        &[(requested_id).clone(), (source).clone()]
                    )
                );
            }
        }
        bail!(
            "{}",
            crate::localization::cli_locale().format(
                "mission already exists: {0}",
                &[sanitize_mission_id(mission_id)?.clone()]
            )
        );
    }
    let mission_dir = get_mission_dir(mission_id, root_dir)?;
    if mission_dir.exists() {
        let nonempty = fs::read_dir(&mission_dir)?.next().is_some();
        if nonempty {
            bail!(
                "{}",
                crate::localization::cli_locale().format(
                    "mission already exists without durable state: {0}",
                    &[sanitize_mission_id(mission_id)?.clone()]
                )
            );
        }
    }
    Ok(())
}

pub fn list_mission_store_snapshots(root_dir: Option<&Path>) -> Result<Vec<MissionStoreSnapshot>> {
    let root = get_mission_store_root(root_dir);
    if !root.exists() {
        return Ok(vec![]);
    }
    let mut snapshots = Vec::new();
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path().join("state.json");
        if !path.exists() {
            continue;
        }
        match read_json_value_optional(&path, true) {
            Ok(Some(value)) => {
                if let Ok(snapshot) = snapshot_from_value(value) {
                    if let Ok(applied) = apply_artifact_features_to_snapshot(snapshot, root_dir) {
                        if let Ok(normalized) = normalize_snapshot(applied) {
                            if validate_coding_completion(&normalized, false).is_ok() {
                                snapshots.push(normalized);
                            }
                        }
                    }
                }
            }
            _ => continue,
        }
    }
    snapshots.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(snapshots)
}

// ── Artifacts ───────────────────────────────────────────────────────────────

pub fn get_mission_artifact_layout(
    mission_id: &str,
    root_dir: Option<&Path>,
) -> Result<MissionArtifactLayout> {
    let mission_dir = get_mission_dir(mission_id, root_dir)?;
    Ok(MissionArtifactLayout {
        mission_markdown: mission_dir.join("mission.md"),
        architecture_markdown: mission_dir.join("architecture.md"),
        validation_contract_markdown: mission_dir.join("validation-contract.md"),
        validation_state_json: mission_dir.join("validation-state.json"),
        features_json: mission_dir.join("features.json"),
        agents_markdown: mission_dir.join("AGENTS.md"),
        services_yaml: mission_dir.join("services.yaml"),
        handoffs_dir: mission_dir.join("handoffs"),
        library_dir: mission_dir.join("library"),
        skills_dir: mission_dir.join("skills"),
        state_json: mission_dir.join("state.json"),
        progress_log_jsonl: mission_dir.join("progress_log.jsonl"),
        model_settings_json: mission_dir.join("model-settings.json"),
        mission_dir,
    })
}

pub fn initialize_mission_artifacts(
    mission_id: &str,
    title: Option<&str>,
    root_dir: Option<&Path>,
    now: Option<&str>,
) -> Result<MissionArtifactLayout> {
    let now = now.map(str::to_string).unwrap_or_else(now_iso);
    let sanitized = sanitize_mission_id(mission_id)?;
    let layout = get_mission_artifact_layout(&sanitized, root_dir)?;
    fs::create_dir_all(&layout.handoffs_dir)?;
    fs::create_dir_all(&layout.library_dir)?;
    fs::create_dir_all(&layout.skills_dir)?;

    let display_title = title.unwrap_or(sanitized.as_str());
    if !layout.mission_markdown.exists() {
        write_atomic(
            &layout.mission_markdown,
            &format!("# {display_title}\n\nCreated: {now}\n\n## Objective\n\nTBD\n"),
        )?;
    }
    if !layout.architecture_markdown.exists() {
        write_atomic(
            &layout.architecture_markdown,
            "# Architecture\n\nDocument system boundaries, responsibilities, and invariants here.\n",
        )?;
    }
    if !layout.validation_contract_markdown.exists() {
        write_atomic(
            &layout.validation_contract_markdown,
            "# Validation Contract\n\nAdd durable behavioral assertions before decomposing features.\n",
        )?;
    }
    if !layout.validation_state_json.exists() {
        write_json_file(
            &layout.validation_state_json,
            &json!({
                "version": MISSION_ARTIFACT_VERSION,
                "assertions": {},
                "updatedAt": now,
            }),
        )?;
    }
    if !layout.features_json.exists() {
        write_json_file(
            &layout.features_json,
            &json!({
                "version": MISSION_ARTIFACT_VERSION,
                "missionId": sanitized,
                "milestones": [],
                "features": [],
                "createdAt": now,
                "updatedAt": now,
            }),
        )?;
    }
    if !layout.agents_markdown.exists() {
        write_atomic(
            &layout.agents_markdown,
            "# Mission Agent Guidance\n\nKeep worker guidance, known constraints, and validation notes here.\n",
        )?;
    }
    if !layout.services_yaml.exists() {
        write_atomic(
            &layout.services_yaml,
            "version: 1\ncommands: {}\nservices: {}\n",
        )?;
    }
    Ok(layout)
}

pub fn summarize_mission_snapshot(snapshot: &MissionStoreSnapshot) -> String {
    let mut total = 0usize;
    let mut pending = 0usize;
    let mut running = 0usize;
    let mut passed = 0usize;
    let mut failed = 0usize;
    for feature in &snapshot.features {
        total += 1;
        match feature.get("status").and_then(Value::as_str) {
            Some("pending") => pending += 1,
            Some("in-progress") => running += 1,
            Some("passed") => passed += 1,
            Some("failed") => failed += 1,
            _ => {}
        }
    }
    let title = snapshot
        .title
        .as_deref()
        .unwrap_or(snapshot.mission_id.as_str());
    format!(
        "{title} ({})\nfeatures: {total} total, {pending} pending, {running} running, {passed} passed, {failed} failed\nworkers: {}",
        snapshot.state.as_str(),
        snapshot.worker_session_ids.len()
    )
}

#[derive(Debug, Clone, Serialize)]
struct ValidateResult {
    path: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

fn validate_features_json(value: &Value, expected_mission_id: &str) -> Result<()> {
    let obj = value.as_object().ok_or_else(|| {
        anyhow!(
            "{}",
            crate::localization::cli_locale().format("features.json must be a JSON object", &[])
        )
    })?;
    if !obj.get("version").is_some_and(Value::is_number) {
        bail!(
            "{}",
            crate::localization::cli_locale().format("features.json requires numeric version", &[])
        );
    }
    let mission_id = obj
        .get("missionId")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            anyhow!(
                "{}",
                crate::localization::cli_locale().format("features.json requires missionId", &[])
            )
        })?;
    if sanitize_mission_id(mission_id)? != expected_mission_id {
        bail!(
            "{}",
            crate::localization::cli_locale().format(
                "features.json missionId {0} does not match mission directory {1}",
                &[(mission_id).to_string(), (expected_mission_id).to_string()]
            )
        );
    }
    if !is_valid_iso(obj.get("updatedAt")) {
        bail!(
            "{}",
            crate::localization::cli_locale().format("features.json requires valid updatedAt", &[])
        );
    }
    let features = obj
        .get("features")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            anyhow!(
                "{}",
                crate::localization::cli_locale()
                    .format("features.json requires features array", &[])
            )
        })?;
    let mut ids = HashSet::new();
    for (index, feature) in features.iter().enumerate() {
        if !feature.is_object() {
            bail!(
                "{}",
                crate::localization::cli_locale()
                    .format("feature {0} must be an object", &[(index).to_string()])
            );
        }
        if !is_mission_feature(feature) {
            bail!(
                "{}",
                crate::localization::cli_locale().format(
                    "feature {0} must match MissionFeature schema",
                    &[(index).to_string()]
                )
            );
        }
        let feature_id = feature
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !ids.insert(feature_id.to_string()) {
            bail!(
                "{}",
                crate::localization::cli_locale().format(
                    "features.json contains duplicate feature id {0}",
                    &[(feature_id).to_string()]
                )
            );
        }
    }
    Ok(())
}

fn is_mission_progress_entry_value(value: &Value) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    let type_ok = matches!(
        obj.get("type").and_then(Value::as_str),
        Some(
            "mission_created"
                | "mission_started"
                | "mission_blocked"
                | "mission_completed"
                | "worker_started"
                | "worker_completed"
                | "worker_failed"
                | "note"
        )
    );
    if !type_ok {
        return false;
    }
    if !is_valid_iso(obj.get("timestamp")) {
        return false;
    }
    if obj.get("message").is_some_and(|v| !v.is_string()) {
        return false;
    }
    if obj.get("featureId").is_some_and(|v| !v.is_string()) {
        return false;
    }
    if obj.get("workerSessionId").is_some_and(|v| !v.is_string()) {
        return false;
    }
    if obj.get("exitCode").is_some_and(|v| !v.is_number()) {
        return false;
    }
    true
}

fn is_mission_token_usage_value(value: &Value) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    if !obj.get("inputTokens").is_some_and(Value::is_number)
        || !obj.get("outputTokens").is_some_and(Value::is_number)
    {
        return false;
    }
    for key in [
        "cacheCreationTokens",
        "cacheReadTokens",
        "thinkingTokens",
        "credits",
    ] {
        if obj.get(key).is_some_and(|v| !v.is_number()) {
            return false;
        }
    }
    true
}

fn validate_mission_state_json(value: &Value, expected_mission_id: &str) -> Result<()> {
    let obj = value.as_object().ok_or_else(|| {
        anyhow!(
            "{}",
            crate::localization::cli_locale().format("state.json must be a JSON object", &[])
        )
    })?;
    if obj.get("schemaVersion").and_then(Value::as_str) != Some(MISSION_STORE_SCHEMA) {
        bail!(
            "{}",
            crate::localization::cli_locale()
                .format("state.json requires mission store schemaVersion", &[])
        );
    }
    let mission_id = obj
        .get("missionId")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            anyhow!(
                "{}",
                crate::localization::cli_locale().format("state.json requires missionId", &[])
            )
        })?;
    if sanitize_mission_id(mission_id)? != expected_mission_id {
        bail!(
            "{}",
            crate::localization::cli_locale().format(
                "state.json missionId {0} does not match mission directory {1}",
                &[(mission_id).to_string(), (expected_mission_id).to_string()]
            )
        );
    }
    let state = obj.get("state").and_then(Value::as_str);
    if state.and_then(MissionState::parse).is_none() {
        bail!(
            "{}",
            crate::localization::cli_locale().format("state.json requires valid state", &[])
        );
    }
    let features = obj
        .get("features")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            anyhow!(
                "{}",
                crate::localization::cli_locale()
                    .format("state.json requires valid features array", &[])
            )
        })?;
    if !features.iter().all(is_mission_feature) {
        bail!(
            "{}",
            crate::localization::cli_locale()
                .format("state.json requires valid features array", &[])
        );
    }
    let mut ids = HashSet::new();
    for feature in features {
        let id = feature
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !ids.insert(id.to_string()) {
            bail!(
                "{}",
                crate::localization::cli_locale().format(
                    "state.json contains duplicate feature id {0}",
                    &[(id).to_string()]
                )
            );
        }
    }
    let progress = obj
        .get("progressLog")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            anyhow!(
                "{}",
                crate::localization::cli_locale()
                    .format("state.json requires progressLog array", &[])
            )
        })?;
    for (index, entry) in progress.iter().enumerate() {
        if !is_mission_progress_entry_value(entry) {
            bail!(
                "{}",
                crate::localization::cli_locale().format(
                    "state.json progressLog {0} must be valid",
                    &[(index).to_string()]
                )
            );
        }
    }
    if !obj.get("workerSessionIds").is_some_and(Value::is_array) {
        bail!(
            "{}",
            crate::localization::cli_locale()
                .format("state.json requires workerSessionIds array", &[])
        );
    }
    if !obj.get("workerStates").is_some_and(Value::is_object) {
        bail!(
            "{}",
            crate::localization::cli_locale()
                .format("state.json requires workerStates object", &[])
        );
    }
    let token_map = obj
        .get("tokenUsageBySessionId")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            anyhow!(
                "{}",
                crate::localization::cli_locale()
                    .format("state.json requires tokenUsageBySessionId object", &[])
            )
        })?;
    for (session_id, usage) in token_map {
        if !is_mission_token_usage_value(usage) {
            bail!(
                "{}",
                crate::localization::cli_locale().format(
                    "state.json tokenUsageBySessionId {0} must be valid",
                    std::slice::from_ref(session_id)
                )
            );
        }
    }
    if obj
        .get("tokenUsage")
        .is_some_and(|v| !is_mission_token_usage_value(v))
    {
        bail!(
            "{}",
            crate::localization::cli_locale().format("state.json tokenUsage must be valid", &[])
        );
    }
    if !is_valid_iso(obj.get("createdAt")) || !is_valid_iso(obj.get("updatedAt")) {
        bail!(
            "{}",
            crate::localization::cli_locale().format("state.json requires valid timestamps", &[])
        );
    }
    Ok(())
}

fn resolve_real_through_parents(absolute: &Path) -> Option<PathBuf> {
    let mut suffix = Vec::new();
    let mut current = absolute.to_path_buf();
    loop {
        if current.exists() {
            let real = dunce::canonicalize(&current).ok()?;
            if suffix.is_empty() {
                return Some(real);
            }
            let mut out = real;
            for part in suffix.into_iter().rev() {
                out.push(part);
            }
            return Some(out);
        }
        let parent = current.parent()?;
        if parent == current {
            return None;
        }
        if let Some(name) = current.file_name() {
            suffix.push(name.to_os_string());
        }
        current = parent.to_path_buf();
    }
}

fn is_path_inside(root: &Path, target: &Path) -> bool {
    target == root || target.starts_with(root)
}

struct ResolvedArtifactPath {
    real_mission_store_root: PathBuf,
    real_artifact_path: PathBuf,
    is_within_mission_store: bool,
    escapes_mission_store: bool,
}

fn resolve_mission_artifact_path(
    file_path: &Path,
    root_dir: Option<&Path>,
) -> ResolvedArtifactPath {
    let absolute_path = if file_path.is_absolute() {
        file_path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(file_path)
    };
    // Normalize lexically without requiring existence.
    let absolute_path = normalize_path(&absolute_path);
    let mission_store_root = normalize_path(&get_mission_store_root(root_dir));
    let real_mission_store_root = resolve_real_through_parents(&mission_store_root)
        .unwrap_or_else(|| mission_store_root.clone());
    let real_artifact_path =
        resolve_real_through_parents(&absolute_path).unwrap_or_else(|| absolute_path.clone());
    let is_lexically = is_path_inside(&mission_store_root, &absolute_path);
    let is_really = is_path_inside(&real_mission_store_root, &real_artifact_path);
    let _ = absolute_path;
    ResolvedArtifactPath {
        real_mission_store_root,
        real_artifact_path,
        is_within_mission_store: is_lexically || is_really,
        escapes_mission_store: is_lexically && !is_really,
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MissionArtifactKind {
    Mission,
    Architecture,
    ValidationContract,
    ValidationState,
    Features,
    Agents,
    Services,
    State,
    ProgressLog,
    ModelSettings,
    Handoff,
    Library,
    Skill,
}

fn classify_mission_artifact_path(
    file_path: &Path,
    root_dir: Option<&Path>,
) -> Option<(MissionArtifactKind, PathBuf)> {
    let resolved = resolve_mission_artifact_path(file_path, root_dir);
    if !resolved.is_within_mission_store || resolved.escapes_mission_store {
        return None;
    }
    let after_root = resolved
        .real_artifact_path
        .strip_prefix(&resolved.real_mission_store_root)
        .ok()?;
    let mut components = after_root.components();
    let mission_id = components.next()?.as_os_str();
    let mission_dir = resolved.real_mission_store_root.join(mission_id);
    let rel = resolved
        .real_artifact_path
        .strip_prefix(&mission_dir)
        .ok()?;
    let rel_str = rel.to_string_lossy();
    if rel_str.starts_with("handoffs/") || rel_str.starts_with("handoffs\\") {
        return Some((MissionArtifactKind::Handoff, mission_dir));
    }
    if rel_str.starts_with("library/") || rel_str.starts_with("library\\") {
        return Some((MissionArtifactKind::Library, mission_dir));
    }
    if rel_str.starts_with("skills/") || rel_str.starts_with("skills\\") {
        return Some((MissionArtifactKind::Skill, mission_dir));
    }
    let file = resolved
        .real_artifact_path
        .file_name()
        .and_then(|n| n.to_str())?;
    let kind = match file {
        "mission.md" => MissionArtifactKind::Mission,
        "architecture.md" => MissionArtifactKind::Architecture,
        "validation-contract.md" => MissionArtifactKind::ValidationContract,
        "validation-state.json" => MissionArtifactKind::ValidationState,
        "features.json" => MissionArtifactKind::Features,
        "AGENTS.md" => MissionArtifactKind::Agents,
        "services.yaml" | "services.yml" => MissionArtifactKind::Services,
        "state.json" => MissionArtifactKind::State,
        "progress_log.jsonl" => MissionArtifactKind::ProgressLog,
        "model-settings.json" => MissionArtifactKind::ModelSettings,
        _ => return None,
    };
    Some((kind, mission_dir))
}

pub fn validate_mission_artifact_content(
    file_path: &Path,
    content: &str,
    root_dir: Option<&Path>,
) -> Result<(), String> {
    let resolved = resolve_mission_artifact_path(file_path, root_dir);
    if resolved.escapes_mission_store {
        return Err(MISSION_ARTIFACT_ESCAPE_MESSAGE.to_string());
    }
    let Some((kind, mission_dir)) = classify_mission_artifact_path(file_path, root_dir) else {
        return Ok(());
    };
    let expected_mission_id = mission_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    match kind {
        MissionArtifactKind::Features => {
            let value: Value = serde_json::from_str(content).map_err(|e| e.to_string())?;
            validate_features_json(&value, expected_mission_id).map_err(|e| e.to_string())
        }
        MissionArtifactKind::ValidationState
        | MissionArtifactKind::ModelSettings
        | MissionArtifactKind::Handoff => {
            serde_json::from_str::<Value>(content).map_err(|e| e.to_string())?;
            Ok(())
        }
        MissionArtifactKind::State => {
            let value: Value = serde_json::from_str(content).map_err(|e| e.to_string())?;
            validate_mission_state_json(&value, expected_mission_id).map_err(|e| e.to_string())
        }
        MissionArtifactKind::ProgressLog => {
            for (index, line) in content.split('\n').enumerate() {
                let line = line.trim_end_matches('\r');
                if line.trim().is_empty() {
                    continue;
                }
                if serde_json::from_str::<Value>(line).is_err() {
                    return Err(format!("Invalid JSONL at line {}", index + 1));
                }
            }
            Ok(())
        }
        MissionArtifactKind::Services => {
            serde_yaml::from_str::<Value>(content).map_err(|e| e.to_string())?;
            Ok(())
        }
        _ => Ok(()),
    }
}

// ── CLI ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct MissionCliArgs {
    subcommand: Option<String>,
    positionals: Vec<String>,
    json: bool,
    help: bool,
}

fn parse_mission_args(args: &[String]) -> MissionCliArgs {
    let mut parsed = MissionCliArgs::default();
    for arg in args {
        match arg.as_str() {
            "--json" => parsed.json = true,
            "help" | "--help" | "-h" => parsed.help = true,
            other if other.starts_with('-') => {
                // Unknown flags are ignored for positional collection parity.
            }
            other => {
                if parsed.subcommand.is_none() {
                    parsed.subcommand = Some(other.to_string());
                } else {
                    parsed.positionals.push(other.to_string());
                }
            }
        }
    }
    parsed
}

fn mission_help() -> &'static str {
    "Usage: maestro mission <command> [args] [--json]

Commands:
  init <mission-id> [title...]   Create mission state + artifact layout
  status [mission-id]            Show one mission or list all
  record <mission-id> <message>  Append a progress note
  set-state <mission-id> <state> [message...]
                                 Transition mission state
  validate <mission-id>          Validate required mission artifacts
  run <mission-id> [options]     Execute admitted mission features with native workflow

States: awaiting-input | ready | running | blocked | completed | failed

Options:
  --json                         Machine-readable output
  --help, -h                     Show this help"
}

pub async fn run_mission(args: &[String]) -> Result<i32> {
    run_mission_sync(args)
}

pub fn run_mission_sync(args: &[String]) -> Result<i32> {
    let parsed = parse_mission_args(args);
    if parsed.help && parsed.subcommand.is_none() {
        println!("{}", mission_help());
        return Ok(0);
    }
    let subcommand = parsed.subcommand.as_deref().unwrap_or("status");
    match subcommand {
        "help" | "--help" | "-h" => {
            println!("{}", mission_help());
            Ok(0)
        }
        "init" => handle_mission_init(&parsed.positionals, parsed.json),
        "status" => handle_mission_status(&parsed.positionals, parsed.json),
        "record" => handle_mission_record(&parsed.positionals, parsed.json),
        "set-state" => handle_mission_set_state(&parsed.positionals, parsed.json),
        "validate" => handle_mission_validate(&parsed.positionals, parsed.json),
        other => {
            bail!("{}", crate::localization::cli_locale().format("Unknown mission subcommand: {0}. Use init, status, record, set-state, or validate.", &[(other).to_string()]));
        }
    }
}

fn should_seed_features_artifact_from_state(
    features_path: &Path,
    snapshot: &MissionStoreSnapshot,
) -> bool {
    if !features_path.exists() {
        return true;
    }
    if snapshot.features.is_empty() {
        return false;
    }
    let Ok(Some(manifest)) = read_json_value_optional(features_path, true) else {
        return true;
    };
    manifest
        .get("features")
        .and_then(Value::as_array)
        .is_none_or(|f| f.is_empty())
}

fn handle_mission_init(args: &[String], json: bool) -> Result<i32> {
    let mission_id = args
        .first()
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "{}",
                crate::localization::cli_locale().format("mission init requires a mission id", &[])
            )
        })?;
    let title = {
        let joined = args[1..].join(" ");
        let trimmed = joined.trim();
        if trimmed.is_empty() {
            mission_id.to_string()
        } else {
            trimmed.to_string()
        }
    };
    let layout = get_mission_artifact_layout(mission_id, None)?;
    let state_path = get_mission_state_path(mission_id, None)?;
    if !state_path.exists()
        && layout.mission_dir.exists()
        && fs::read_dir(&layout.mission_dir)?.next().is_some()
    {
        bail!("{}", crate::localization::cli_locale().format("mission state missing for existing mission: {0}. Restore state.json instead of re-running init.", &[(mission_id).to_string()]));
    }
    let state_exists = state_path.exists();
    let snapshot = if state_exists {
        MissionStore::load(mission_id, MissionStoreConfig::default())?.get_snapshot()?
    } else {
        MissionStore::create(mission_id, Some(&title), MissionStoreConfig::default())?.save()?
    };
    if let Some(source) = snapshot.source_mission_id.as_ref() {
        let requested = mission_id.trim();
        if source != requested && snapshot.mission_id != requested {
            bail!(
                "{}",
                crate::localization::cli_locale().format(
                    "missionId \"{0}\" collides with existing mission \"{1}\"",
                    &[(requested).to_string(), (source).clone()]
                )
            );
        }
    }
    let should_seed =
        state_exists && should_seed_features_artifact_from_state(&layout.features_json, &snapshot);
    let initialized_layout = initialize_mission_artifacts(mission_id, Some(&title), None, None)?;
    if should_seed {
        write_json_file(
            &initialized_layout.features_json,
            &json!({
                "version": MISSION_MANIFEST_VERSION,
                "missionId": snapshot.mission_id,
                "milestones": [],
                "features": snapshot.features,
                "createdAt": snapshot.created_at,
                "updatedAt": snapshot.updated_at,
            }),
        )?;
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "layout": initialized_layout,
                "snapshot": snapshot,
            }))?
        );
    } else {
        println!(
            "{}",
            crate::localization::cli_locale().format("Mission initialized", &[])
        );
        println!("id: {}", snapshot.mission_id);
        println!("dir: {}", initialized_layout.mission_dir.display());
    }
    Ok(0)
}

fn handle_mission_status(args: &[String], json: bool) -> Result<i32> {
    if let Some(mission_id) = args.first().map(String::as_str).filter(|s| !s.is_empty()) {
        let snapshot =
            MissionStore::load(mission_id, MissionStoreConfig::default())?.get_snapshot()?;
        if json {
            println!("{}", serde_json::to_string_pretty(&snapshot)?);
        } else {
            println!("{}", summarize_mission_snapshot(&snapshot));
        }
        return Ok(0);
    }
    let snapshots = list_mission_store_snapshots(None)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&snapshots)?);
        return Ok(0);
    }
    if snapshots.is_empty() {
        println!(
            "{}",
            crate::localization::cli_locale().format("No missions found.", &[])
        );
        return Ok(0);
    }
    for (index, snapshot) in snapshots.iter().enumerate() {
        println!("{}", summarize_mission_snapshot(snapshot));
        if index + 1 < snapshots.len() {
            println!();
        }
    }
    Ok(0)
}

fn handle_mission_record(args: &[String], json: bool) -> Result<i32> {
    let mission_id = args
        .first()
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "{}",
                crate::localization::cli_locale()
                    .format("mission record requires a mission id", &[])
            )
        })?;
    let message = args[1..].join(" ");
    let message = message.trim();
    if message.is_empty() {
        bail!(
            "{}",
            crate::localization::cli_locale().format("mission record requires a message", &[])
        );
    }
    let mut store = MissionStore::load(mission_id, MissionStoreConfig::default())?;
    let snapshot = store.append_progress(MissionProgressEntry {
        entry_type: MissionProgressType::Note,
        timestamp: String::new(),
        message: Some(message.to_string()),
        feature_id: None,
        worker_session_id: None,
        exit_code: None,
    })?;
    if json {
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
    } else {
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "Recorded mission note for {0}.",
                std::slice::from_ref(&(snapshot.mission_id))
            )
        );
    }
    Ok(0)
}

fn handle_mission_set_state(args: &[String], json: bool) -> Result<i32> {
    let mission_id = args.first().map(String::as_str).filter(|s| !s.is_empty());
    let state_raw = args.get(1).map(String::as_str);
    let (Some(mission_id), Some(state_raw)) = (mission_id, state_raw) else {
        bail!(
            "{}",
            crate::localization::cli_locale()
                .format("mission set-state requires <mission-id> <state>", &[])
        );
    };
    let Some(state) = MissionState::parse(state_raw) else {
        bail!(
            "{}",
            crate::localization::cli_locale()
                .format("invalid mission state: {0}", &[(state_raw).to_string()])
        );
    };
    let message = {
        let joined = args[2..].join(" ");
        let trimmed = joined.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    };
    let mut store = MissionStore::load(mission_id, MissionStoreConfig::default())?;
    let snapshot = store.set_state(state, message.as_deref())?;
    if json {
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
    } else {
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "Mission {0} is now {1}.",
                &[
                    (snapshot.mission_id).clone(),
                    (snapshot.state.as_str()).to_string()
                ]
            )
        );
    }
    Ok(0)
}

fn handle_mission_validate(args: &[String], json: bool) -> Result<i32> {
    let mission_id = args
        .first()
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "{}",
                crate::localization::cli_locale()
                    .format("mission validate requires a mission id", &[])
            )
        })?;
    let layout = get_mission_artifact_layout(mission_id, None)?;
    let required = [
        layout.features_json.clone(),
        layout.validation_state_json.clone(),
        layout.services_yaml.clone(),
        layout.state_json.clone(),
    ];
    let mut results = Vec::new();
    for path in required {
        let result = match fs::read_to_string(&path) {
            Ok(content) => match validate_mission_artifact_content(&path, &content, None) {
                Ok(()) => ValidateResult {
                    path: path.display().to_string(),
                    ok: true,
                    message: None,
                },
                Err(message) => ValidateResult {
                    path: path.display().to_string(),
                    ok: false,
                    message: Some(message),
                },
            },
            Err(_) => ValidateResult {
                path: path.display().to_string(),
                ok: false,
                message: Some("Missing required mission artifact".into()),
            },
        };
        results.push(result);
    }
    let failed: Vec<_> = results.iter().filter(|r| !r.ok).collect();
    let exit = i32::from(!failed.is_empty());
    if json {
        println!("{}", serde_json::to_string_pretty(&results)?);
        return Ok(exit);
    }
    if failed.is_empty() {
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "Mission {0} artifacts are valid.",
                &[(mission_id).to_string()]
            )
        );
    } else {
        for result in failed {
            let message = result.message.as_deref().unwrap_or("invalid");
            println!("{}: {message}", result.path);
        }
    }
    Ok(exit)
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use maestro_runtime::coding_acceptance::{
        CodingCommandResult, CodingHandoffDisposition, CodingHandoffItem, CodingValidationReport,
        CodingValidationRole, CodingVerificationStatus,
    };
    use std::sync::{Mutex, OnceLock};
    use tempfile::TempDir;

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn restore_env(name: &str, value: Option<String>) {
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
    }

    fn with_store_dir<T>(f: impl FnOnce(&Path) -> T) -> T {
        let _guard = env_lock();
        let temp = TempDir::new().unwrap();
        let previous = std::env::var("MAESTRO_MISSION_STORE_DIR").ok();
        std::env::set_var("MAESTRO_MISSION_STORE_DIR", temp.path());
        let result = f(temp.path());
        restore_env("MAESTRO_MISSION_STORE_DIR", previous);
        result
    }

    #[test]
    fn help_lists_subcommands() {
        assert!(mission_help().contains("init"));
        assert!(mission_help().contains("validate"));
        assert!(mission_help().contains("set-state"));
    }

    fn coding_completion_fixture() -> MissionStoreSnapshot {
        let contract = CodingAcceptanceContract {
            task_id: "coding-feature".into(),
            repository_id: "repository".into(),
            generation: 1,
            required_assertion_ids: vec!["user-flow".into()],
            require_review: true,
            require_behavior: true,
            readiness_requirements: vec!["test".into()],
            authorized_skips: vec![],
            authorized_dispositions: vec![],
            output_paths: Vec::new(),
        };
        let review = CodingValidationReport {
            child_id: "review-child".into(),
            session_id: "review-session".into(),
            revision: "a".repeat(40),
            status: CodingVerificationStatus::Passed,
            assertions: vec![maestro_runtime::coding_acceptance::CodingAssertionResult {
                assertion_id: "user-flow".into(),
                status: CodingVerificationStatus::Passed,
                evidence_refs: vec!["session/review/assertion".into()],
            }],
            evidence_refs: vec!["session/review/transcript".into()],
        };
        let child = CodingAcceptanceChildRecord {
            organization_id: String::new(),
            workspace_id: String::new(),
            work_id: "coding-work".into(),
            parent_session_id: "implementation-session".into(),
            child_id: review.child_id.clone(),
            session_id: review.session_id.clone(),
            role: CodingValidationRole::Review,
            revision: review.revision.clone(),
            completed_successfully: true,
            report_digest: review.digest(),
        };
        let behavior = CodingValidationReport {
            child_id: "behavior-child".into(),
            session_id: "behavior-session".into(),
            evidence_refs: vec!["session/behavior/transcript".into()],
            ..review.clone()
        };
        let behavior_child = CodingAcceptanceChildRecord {
            child_id: behavior.child_id.clone(),
            session_id: behavior.session_id.clone(),
            role: CodingValidationRole::Behavior,
            report_digest: behavior.digest(),
            ..child.clone()
        };
        let submission = CodingCompletionSubmission {
            computer_proof: None,
            task_id: contract.task_id.clone(),
            work_id: child.work_id.clone(),
            repository_id: contract.repository_id.clone(),
            contract_digest: contract.digest(),
            generation: contract.generation,
            revision: review.revision.clone(),
            implementation_session_id: child.parent_session_id.clone(),
            commands: vec![CodingCommandResult {
                command: "cargo test".into(),
                exit_code: Some(0),
                evidence_refs: vec!["session/test/tool-result".into()],
            }],
            readiness: vec![maestro_runtime::coding_acceptance::CodingAssertionResult {
                assertion_id: "test".into(),
                status: CodingVerificationStatus::Passed,
                evidence_refs: vec!["session/test/tool-result".into()],
            }],
            review: Some(review),
            behavior: Some(behavior),
            handoff_items: vec![],
            outputs: Vec::new(),
        };
        let feature = json!({
            "id": contract.task_id,
            "description": "Coding acceptance regression",
            "status": "pending",
            "fulfills": [],
            CODING_ACCEPTANCE_METADATA_KEY: contract,
            CODING_ACCEPTANCE_RESULT_METADATA_KEY: submission,
            CODING_ACCEPTANCE_CHILD_RECORDS_KEY: [child, behavior_child],
            "codingWorkflow": {
                "workId": submission.work_id,
                "implementationSessionId": submission.implementation_session_id,
                "revision": submission.revision,
                "contractDigest": submission.contract_digest,
                "repositoryRoot": "/repository",
            },
        });
        create_mission_store_snapshot(
            "coding-mission",
            None,
            vec![feature],
            "2026-06-19T00:00:00.000Z",
        )
        .unwrap()
    }

    #[test]
    fn coding_mission_terminal_gate_requires_actual_child_proof_and_disposed_handoffs() {
        let mut snapshot = coding_completion_fixture();
        snapshot.state = MissionState::Completed;
        validate_coding_completion(&snapshot, false).unwrap();
        let mut missing_child = snapshot.clone();
        missing_child.features[0][CODING_ACCEPTANCE_CHILD_RECORDS_KEY] = json!([]);
        assert!(
            validate_coding_completion(&missing_child, false)
                .unwrap_err()
                .to_string()
                .contains("child")
        );
        let mut open_handoff = snapshot.clone();
        open_handoff.features[0][CODING_ACCEPTANCE_RESULT_METADATA_KEY]["handoffItems"] =
            json!([CodingHandoffItem {
                id: "unfinished-verification".into(),
                disposition: CodingHandoffDisposition::Open,
                evidence_refs: vec![],
            }]);
        assert!(validate_coding_completion(&open_handoff, false).is_err());
        let mut stale = snapshot;
        stale.features[0]["codingWorkflow"]["revision"] = json!("b".repeat(40));
        assert!(
            validate_coding_completion(&stale, false)
                .unwrap_err()
                .to_string()
                .contains("revision")
        );
    }

    #[test]
    fn coding_mission_retains_historical_acceptance_without_rechecking_unrelated_saves() {
        let mut previous = coding_completion_fixture();
        previous.features[0]["status"] = json!("passed");
        // The fixture checkout deliberately does not exist. An unchanged
        // accepted feature must not require that historical checkout on a later
        // task's progress update; newly submitted proof still does.
        validate_new_coding_completions(Some(&previous), &previous).unwrap();
        let mut changed = previous.clone();
        changed.features[0]["codingWorkflow"]["repositoryRoot"] = json!("relative-root");
        assert!(validate_new_coding_completions(Some(&previous), &changed).is_err());
    }

    #[test]
    fn coding_mission_set_state_and_direct_save_reject_missing_submission() {
        let temp = TempDir::new().unwrap();
        let mut snapshot = coding_completion_fixture();
        snapshot.features[0]
            .as_object_mut()
            .unwrap()
            .remove(CODING_ACCEPTANCE_RESULT_METADATA_KEY);
        let mut store = MissionStore::new(
            snapshot,
            MissionStoreConfig {
                root_dir: Some(temp.path().to_owned()),
                now: None,
            },
        )
        .unwrap();
        store.save().unwrap();
        assert!(store.set_state(MissionState::Completed, None).is_err());
        assert_ne!(store.get_snapshot().unwrap().state, MissionState::Completed);
        store.snapshot.state = MissionState::Completed;
        assert!(store.save().is_err());
        let loaded = MissionStore::load(
            "coding-mission",
            MissionStoreConfig {
                root_dir: Some(temp.path().to_owned()),
                now: None,
            },
        )
        .unwrap();
        assert_ne!(
            loaded.get_snapshot().unwrap().state,
            MissionState::Completed
        );
    }

    #[test]
    fn coding_mission_contract_cannot_be_removed_by_generic_update_or_artifact_overlay() {
        let temp = TempDir::new().unwrap();
        let snapshot = coding_completion_fixture();
        let mut store = MissionStore::new(
            snapshot.clone(),
            MissionStoreConfig {
                root_dir: Some(temp.path().to_owned()),
                now: None,
            },
        )
        .unwrap();
        store.save().unwrap();
        assert!(store.set_features(vec![]).is_err());
        let mut stripped = snapshot.features[0].clone();
        stripped
            .as_object_mut()
            .unwrap()
            .remove(CODING_ACCEPTANCE_METADATA_KEY);
        let manifest_path = get_mission_dir("coding-mission", Some(temp.path()))
            .unwrap()
            .join("features.json");
        write_json_file(
            &manifest_path,
            &json!({
                "version": 1,
                "missionId": "coding-mission",
                "updatedAt": "2099-06-19T00:00:00.000Z",
                "features": [stripped],
            }),
        )
        .unwrap();
        assert!(store.save().is_err());
        assert!(
            MissionStore::load(
                "coding-mission",
                MissionStoreConfig {
                    root_dir: Some(temp.path().to_owned()),
                    now: None,
                }
            )
            .is_err()
        );
    }

    #[test]
    fn coding_mission_passed_artifact_cannot_bypass_terminal_proof() {
        let temp = TempDir::new().unwrap();
        let mut snapshot = coding_completion_fixture();
        snapshot.features[0]
            .as_object_mut()
            .unwrap()
            .remove(CODING_ACCEPTANCE_RESULT_METADATA_KEY);
        let mut store = MissionStore::new(
            snapshot.clone(),
            MissionStoreConfig {
                root_dir: Some(temp.path().to_owned()),
                now: None,
            },
        )
        .unwrap();
        store.save().unwrap();
        snapshot.features[0]["status"] = json!("passed");
        let manifest_path = get_mission_dir("coding-mission", Some(temp.path()))
            .unwrap()
            .join("features.json");
        write_json_file(
            &manifest_path,
            &json!({
                "version": 1,
                "missionId": "coding-mission",
                "updatedAt": "2099-06-19T00:00:00.000Z",
                "features": snapshot.features,
            }),
        )
        .unwrap();
        assert!(store.save().is_err());
    }

    #[test]
    fn sanitize_mission_id_replaces_unsafe_chars() {
        assert_eq!(
            sanitize_mission_id("customer value").unwrap(),
            "customer-value"
        );
        assert_eq!(sanitize_mission_id("foo+bar").unwrap(), "foo-bar");
        assert!(sanitize_mission_id("***").is_err());
    }

    #[test]
    fn create_load_and_record_progress() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let config = MissionStoreConfig {
            root_dir: Some(root.to_path_buf()),
            now: None,
        };
        let mut store =
            MissionStore::create("customer value", Some("Customer value"), config).unwrap();
        store
            .append_progress(MissionProgressEntry {
                entry_type: MissionProgressType::WorkerStarted,
                timestamp: "2026-06-19T00:01:00.000Z".into(),
                message: None,
                feature_id: Some("feature-1".into()),
                worker_session_id: Some("worker-1".into()),
                exit_code: None,
            })
            .unwrap();
        store
            .append_progress(MissionProgressEntry {
                entry_type: MissionProgressType::WorkerCompleted,
                timestamp: "2026-06-19T00:02:00.000Z".into(),
                message: None,
                feature_id: Some("feature-1".into()),
                worker_session_id: Some("worker-1".into()),
                exit_code: Some(0),
            })
            .unwrap();
        store
            .set_session_token_usage(
                "worker-1",
                MissionTokenUsage {
                    input_tokens: 10.0,
                    output_tokens: 5.0,
                    cache_creation_tokens: None,
                    cache_read_tokens: None,
                    thinking_tokens: Some(2.0),
                    credits: None,
                },
            )
            .unwrap();

        let loaded = MissionStore::load(
            "customer value",
            MissionStoreConfig {
                root_dir: Some(root.to_path_buf()),
                now: None,
            },
        )
        .unwrap()
        .get_snapshot()
        .unwrap();
        assert_eq!(loaded.mission_id, "customer-value");
        assert_eq!(loaded.worker_session_ids, vec!["worker-1".to_string()]);
        assert_eq!(
            loaded.worker_states["worker-1"].completed_at.as_deref(),
            Some("2026-06-19T00:02:00.000Z")
        );
        assert!((loaded.token_usage.as_ref().unwrap().input_tokens - 10.0).abs() < f64::EPSILON);
        assert_eq!(
            list_mission_store_snapshots(Some(root))
                .unwrap()
                .iter()
                .map(|s| s.mission_id.as_str())
                .collect::<Vec<_>>(),
            vec!["customer-value"]
        );
    }

    #[test]
    fn set_state_records_blocked_progress() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fn fixed_now() -> String {
            "2026-06-19T00:00:00.000Z".into()
        }
        let mut store = MissionStore::create(
            "deep",
            None,
            MissionStoreConfig {
                root_dir: Some(root.to_path_buf()),
                now: Some(fixed_now),
            },
        )
        .unwrap();
        store.save().unwrap();
        store.set_state(MissionState::Blocked, None).unwrap();
        let snapshot = MissionStore::load(
            "deep",
            MissionStoreConfig {
                root_dir: Some(root.to_path_buf()),
                now: None,
            },
        )
        .unwrap()
        .get_snapshot()
        .unwrap();
        assert_eq!(snapshot.state, MissionState::Blocked);
        assert!(snapshot.progress_log.iter().any(|e| {
            e.entry_type == MissionProgressType::MissionBlocked
                && e.message.as_deref() == Some("Mission is blocked")
        }));
    }

    #[test]
    fn rejects_reopening_terminal_mission() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let mut store = MissionStore::create(
            "deep",
            None,
            MissionStoreConfig {
                root_dir: Some(root.to_path_buf()),
                now: None,
            },
        )
        .unwrap();
        store.set_state(MissionState::Completed, None).unwrap();
        let err = store
            .set_state(MissionState::Running, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("already completed"));
    }

    #[test]
    fn concurrent_writers_merge_progress_and_tokens() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        MissionStore::create(
            "deep",
            None,
            MissionStoreConfig {
                root_dir: Some(root.to_path_buf()),
                now: None,
            },
        )
        .unwrap()
        .save()
        .unwrap();
        let mut left = MissionStore::load(
            "deep",
            MissionStoreConfig {
                root_dir: Some(root.to_path_buf()),
                now: None,
            },
        )
        .unwrap();
        let mut right = MissionStore::load(
            "deep",
            MissionStoreConfig {
                root_dir: Some(root.to_path_buf()),
                now: None,
            },
        )
        .unwrap();
        left.append_progress(MissionProgressEntry {
            entry_type: MissionProgressType::WorkerStarted,
            timestamp: "2026-06-19T00:01:00.000Z".into(),
            message: None,
            feature_id: None,
            worker_session_id: Some("worker-1".into()),
            exit_code: None,
        })
        .unwrap();
        right
            .append_progress(MissionProgressEntry {
                entry_type: MissionProgressType::WorkerStarted,
                timestamp: "2026-06-19T00:02:00.000Z".into(),
                message: None,
                feature_id: None,
                worker_session_id: Some("worker-2".into()),
                exit_code: None,
            })
            .unwrap();
        left.set_session_token_usage(
            "worker-1",
            MissionTokenUsage {
                input_tokens: 1.0,
                output_tokens: 2.0,
                cache_creation_tokens: None,
                cache_read_tokens: None,
                thinking_tokens: None,
                credits: None,
            },
        )
        .unwrap();
        right
            .set_session_token_usage(
                "worker-2",
                MissionTokenUsage {
                    input_tokens: 3.0,
                    output_tokens: 4.0,
                    cache_creation_tokens: None,
                    cache_read_tokens: None,
                    thinking_tokens: None,
                    credits: None,
                },
            )
            .unwrap();
        let snapshot = MissionStore::load(
            "deep",
            MissionStoreConfig {
                root_dir: Some(root.to_path_buf()),
                now: None,
            },
        )
        .unwrap()
        .get_snapshot()
        .unwrap();
        assert_eq!(
            snapshot.worker_session_ids,
            vec!["worker-1".to_string(), "worker-2".to_string()]
        );
        assert!((snapshot.token_usage.as_ref().unwrap().input_tokens - 4.0).abs() < f64::EPSILON);
    }

    #[test]
    fn init_does_not_reset_existing_mission() {
        with_store_dir(|root| {
            run_mission_sync(&["init".into(), "deep".into(), "Deep Mission".into()]).unwrap();
            let mut store = MissionStore::load(
                "deep",
                MissionStoreConfig {
                    root_dir: Some(root.to_path_buf()),
                    now: None,
                },
            )
            .unwrap();
            store
                .set_features(vec![json!({
                    "id": "feature-1",
                    "description": "Keep me",
                    "status": "pending",
                    "fulfills": []
                })])
                .unwrap();
            store
                .append_progress(MissionProgressEntry {
                    entry_type: MissionProgressType::Note,
                    timestamp: String::new(),
                    message: Some("keep me".into()),
                    feature_id: None,
                    worker_session_id: None,
                    exit_code: None,
                })
                .unwrap();
            let _ = fs::remove_file(root.join("deep/features.json"));

            run_mission_sync(&["init".into(), "deep".into(), "Replacement Title".into()]).unwrap();

            let snapshot = MissionStore::load(
                "deep",
                MissionStoreConfig {
                    root_dir: Some(root.to_path_buf()),
                    now: None,
                },
            )
            .unwrap()
            .get_snapshot()
            .unwrap();
            assert_eq!(snapshot.title.as_deref(), Some("Deep Mission"));
            assert_eq!(
                snapshot.features[0].get("id").and_then(Value::as_str),
                Some("feature-1")
            );
            assert!(
                snapshot
                    .progress_log
                    .iter()
                    .any(|e| e.message.as_deref() == Some("keep me"))
            );
            let repaired: Value =
                serde_json::from_str(&fs::read_to_string(root.join("deep/features.json")).unwrap())
                    .unwrap();
            assert_eq!(repaired["features"][0]["id"].as_str(), Some("feature-1"));
        });
    }

    #[test]
    fn init_allows_sanitized_id_rerun() {
        with_store_dir(|_root| {
            run_mission_sync(&[
                "init".into(),
                "customer value".into(),
                "Customer Value".into(),
            ])
            .unwrap();
            run_mission_sync(&[
                "init".into(),
                "customer-value".into(),
                "Replacement Title".into(),
            ])
            .unwrap();
            let snapshot = MissionStore::load("customer-value", MissionStoreConfig::default())
                .unwrap()
                .get_snapshot()
                .unwrap();
            assert_eq!(snapshot.mission_id, "customer-value");
            assert_eq!(
                snapshot.source_mission_id.as_deref(),
                Some("customer value")
            );
            assert_eq!(snapshot.title.as_deref(), Some("Customer Value"));
        });
    }

    #[test]
    fn init_rejects_different_raw_id_alias() {
        with_store_dir(|_root| {
            run_mission_sync(&["init".into(), "foo+bar".into(), "Foo Plus Bar".into()]).unwrap();
            let err = run_mission_sync(&["init".into(), "foo bar".into(), "Foo Space Bar".into()])
                .unwrap_err()
                .to_string();
            assert!(err.contains("collides with existing mission"));
        });
    }

    #[test]
    fn init_refuses_when_state_missing_but_artifacts_remain() {
        with_store_dir(|root| {
            run_mission_sync(&["init".into(), "deep".into(), "Deep Mission".into()]).unwrap();
            fs::remove_file(root.join("deep/state.json")).unwrap();
            let err = run_mission_sync(&["init".into(), "deep".into(), "Deep Mission".into()])
                .unwrap_err()
                .to_string();
            assert!(err.contains("mission state missing for existing mission: deep"));
        });
    }

    #[test]
    fn validate_fails_when_state_missing() {
        with_store_dir(|root| {
            run_mission_sync(&["init".into(), "deep".into(), "Deep Mission".into()]).unwrap();
            fs::remove_file(root.join("deep/state.json")).unwrap();
            let code = run_mission_sync(&["validate".into(), "deep".into()]).unwrap();
            assert_eq!(code, 1);
            let code =
                run_mission_sync(&["validate".into(), "deep".into(), "--json".into()]).unwrap();
            assert_eq!(code, 1);
        });
    }

    #[test]
    fn validate_passes_for_initialized_mission() {
        with_store_dir(|_root| {
            run_mission_sync(&["init".into(), "deep".into(), "Deep Mission".into()]).unwrap();
            let code = run_mission_sync(&["validate".into(), "deep".into()]).unwrap();
            assert_eq!(code, 0);
        });
    }

    #[test]
    fn record_and_set_state_round_trip() {
        with_store_dir(|_root| {
            run_mission_sync(&["init".into(), "deep".into(), "Deep".into()]).unwrap();
            run_mission_sync(&[
                "record".into(),
                "deep".into(),
                "hello".into(),
                "world".into(),
            ])
            .unwrap();
            run_mission_sync(&["set-state".into(), "deep".into(), "running".into()]).unwrap();
            let snapshot = MissionStore::load("deep", MissionStoreConfig::default())
                .unwrap()
                .get_snapshot()
                .unwrap();
            assert_eq!(snapshot.state, MissionState::Running);
            assert!(
                snapshot
                    .progress_log
                    .iter()
                    .any(|e| e.message.as_deref() == Some("hello world"))
            );
        });
    }

    #[test]
    fn initialize_artifacts_creates_contract_first_files() {
        let temp = TempDir::new().unwrap();
        let layout = initialize_mission_artifacts(
            "launch",
            Some("Launch"),
            Some(temp.path()),
            Some("2026-06-19T00:00:00.000Z"),
        )
        .unwrap();
        assert!(
            fs::read_to_string(&layout.mission_markdown)
                .unwrap()
                .contains("# Launch")
        );
        assert!(
            fs::read_to_string(&layout.validation_contract_markdown)
                .unwrap()
                .contains("Validation Contract")
        );
        assert!(layout.features_json.exists());
        assert!(layout.services_yaml.exists());
    }

    #[test]
    fn validate_features_json_rejects_bad_status() {
        let temp = TempDir::new().unwrap();
        let layout = initialize_mission_artifacts("bad", None, Some(temp.path()), None).unwrap();
        let bad = json!({
            "version": 1,
            "missionId": "bad",
            "features": [{
                "id": "feature-1",
                "description": "Reject typo status",
                "status": "done",
                "fulfills": []
            }],
            "updatedAt": "2026-06-19T00:00:00.000Z"
        });
        let err = validate_mission_artifact_content(
            &layout.features_json,
            &serde_json::to_string(&bad).unwrap(),
            Some(temp.path()),
        )
        .unwrap_err();
        assert!(err.contains("MissionFeature") || err.contains("feature"));
    }

    #[test]
    fn sum_token_usage_across_sessions() {
        let mut usages = BTreeMap::new();
        usages.insert(
            "a".into(),
            MissionTokenUsage {
                input_tokens: 1.0,
                output_tokens: 2.0,
                cache_creation_tokens: None,
                cache_read_tokens: None,
                thinking_tokens: None,
                credits: Some(0.5),
            },
        );
        usages.insert(
            "b".into(),
            MissionTokenUsage {
                input_tokens: 3.0,
                output_tokens: 4.0,
                cache_creation_tokens: None,
                cache_read_tokens: None,
                thinking_tokens: None,
                credits: Some(1.0),
            },
        );
        let sum = sum_mission_token_usage(&usages).unwrap();
        assert!((sum.input_tokens - 4.0).abs() < f64::EPSILON);
        assert!((sum.output_tokens - 6.0).abs() < f64::EPSILON);
        assert_eq!(sum.credits, Some(1.5));
    }

    #[test]
    fn parse_args_captures_json_flag() {
        let parsed = parse_mission_args(&[
            "init".into(),
            "deep".into(),
            "Title".into(),
            "--json".into(),
        ]);
        assert_eq!(parsed.subcommand.as_deref(), Some("init"));
        assert!(parsed.json);
        assert_eq!(parsed.positionals, vec!["deep", "Title"]);
    }

    #[test]
    fn is_mission_feature_accepts_minimal_feature() {
        assert!(is_mission_feature(&json!({
            "id": "f1",
            "description": "desc",
            "status": "pending",
            "fulfills": ["a1"]
        })));
        assert!(!is_mission_feature(&json!({
            "id": "f1",
            "description": "desc",
            "status": "done",
            "fulfills": []
        })));
    }
}
