use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::{Args, Subcommand, ValueEnum};
use fs2::FileExt;
use prost::Message;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use ureq::Agent;

mod transport;
use transport::{WireEncoding, compress_body, preferred_encoding};
mod checkpoint;
use checkpoint::{CaptureOptions, SourceCheckpoint, read_capture};
mod compression;
use compression::{SpoolEncoding, encode_spool, read_segment, verify_segment_storage};

mod proto;
mod provenance_compat;

use crate::proto as sessions_pb;
use crate::provenance_compat::{Redactor, clear_hook_session, touch_hook_session};

const MANIFEST_VERSION: u32 = 1;
const REDACTION_POLICY_VERSION: &str = "transcript-redaction-v2";
const LEGACY_REDACTION_POLICY_VERSION: &str = "transcript-redaction-v1";
const MAX_SEGMENT_BYTES: usize = 512 * 1024;
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;
const TRANSCRIPT_METHOD: &str = "sessions.v1.SessionsService/RecordTranscriptSegment";
const RECEIPT_JOURNAL_FILE: &str = ".receipts.jsonl";
const WIRE_CAPABILITY_CACHE_FILE: &str = ".wire-capability.json";
const WIRE_CAPABILITY_TTL_SECS: u64 = 10 * 60;
const UPLOAD_SCHEDULE_FILE: &str = ".upload-scheduled";
const UPLOAD_SCHEDULE_STALE_SECS: u64 = 10 * 60;

#[derive(Debug, Args)]
pub struct TranscriptArgs {
    #[command(subcommand)]
    command: TranscriptCommand,
}

#[derive(Debug, Subcommand)]
enum TranscriptCommand {
    /// Redact and spool one vendor JSONL transcript without network access.
    Prepare(PrepareTranscriptArgs),
    /// Upload pending segments from a prepared manifest and persist receipts.
    Push(PushTranscriptArgs),
    /// Consume a Codex/Claude lifecycle hook payload from stdin.
    #[command(hide = true)]
    Hook(HookTranscriptArgs),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptAgent {
    ClaudeCode,
    Codex,
    Maestro,
    Other,
}

impl TranscriptAgent {
    fn storage_name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude_code",
            Self::Codex => "codex",
            Self::Maestro => "maestro",
            Self::Other => "other",
        }
    }

    fn proto(self) -> sessions_pb::AgentKind {
        match self {
            Self::ClaudeCode => sessions_pb::AgentKind::ClaudeCode,
            Self::Codex => sessions_pb::AgentKind::Codex,
            Self::Maestro => sessions_pb::AgentKind::Maestro,
            Self::Other => sessions_pb::AgentKind::Other,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptCompletenessArg {
    InProgress,
    Complete,
    Partial,
}

impl TranscriptCompletenessArg {
    fn proto(self) -> sessions_pb::TranscriptCompleteness {
        match self {
            Self::InProgress => sessions_pb::TranscriptCompleteness::InProgress,
            Self::Complete => sessions_pb::TranscriptCompleteness::Complete,
            Self::Partial => sessions_pb::TranscriptCompleteness::Partial,
        }
    }
}

#[derive(Debug, Args)]
struct PrepareTranscriptArgs {
    /// Vendor JSONL exported by Claude Code, Codex, Maestro, or another adapter.
    #[arg(long)]
    input: PathBuf,
    #[arg(long, value_enum)]
    agent: TranscriptAgent,
    /// Stable vendor-side session identifier.
    #[arg(long)]
    source_session_id: String,
    /// Platform session identifier. Derived deterministically when omitted.
    #[arg(long)]
    session_id: Option<String>,
    #[arg(long)]
    organization: String,
    #[arg(long)]
    workspace: String,
    #[arg(long)]
    repository_url: Option<String>,
    #[arg(long)]
    working_directory: Option<String>,
    #[arg(long)]
    branch: Option<String>,
    #[arg(long)]
    head_sha: Option<String>,
    #[arg(long)]
    title: Option<String>,
    #[arg(long, value_enum, default_value_t = TranscriptCompletenessArg::Complete)]
    completeness: TranscriptCompletenessArg,
}

#[derive(Args)]
struct PushTranscriptArgs {
    #[arg(long)]
    manifest: PathBuf,
    #[arg(long, env = "PLATFORM_API_URL")]
    endpoint: String,
    #[arg(long, env = "PLATFORM_API_TOKEN")]
    token: Option<String>,
}

#[derive(Debug, Args)]
struct HookTranscriptArgs {
    #[arg(long, value_enum)]
    agent: TranscriptAgent,
    #[arg(long)]
    organization: Option<String>,
    #[arg(long)]
    workspace: Option<String>,
    #[arg(long)]
    endpoint: Option<String>,
    /// Wait for upload completion. Intended for diagnostics and end-to-end tests.
    #[arg(long)]
    wait_for_upload: bool,
}

#[derive(Debug, Deserialize)]
struct AgentHookPayload {
    hook_event_name: String,
    session_id: String,
    cwd: PathBuf,
    #[serde(default)]
    transcript_path: Option<PathBuf>,
    #[serde(default)]
    transcript_size_before: Option<u64>,
    #[serde(default)]
    organization_id: Option<String>,
    #[serde(default)]
    workspace_id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
}

/// Product-owned Maestro lifecycle event captured without a repository hook.
///
/// The tenant identifiers and bearer come from Maestro's verified Identity
/// session. They are passed in memory and are never serialized into the hook
/// payload or spool manifest.
#[derive(Clone)]
pub struct MaestroTranscriptEvent {
    pub event_name: String,
    pub source_session_id: String,
    pub cwd: PathBuf,
    pub transcript_path: Option<PathBuf>,
    pub transcript_size_before: Option<u64>,
    pub organization_id: String,
    pub workspace_id: String,
    pub endpoint: Option<String>,
    pub access_token: Option<String>,
    pub model: Option<String>,
}

impl std::fmt::Debug for PushTranscriptArgs {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PushTranscriptArgs")
            .field("manifest", &self.manifest)
            .field("endpoint", &self.endpoint)
            .field("token", &self.token.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct TranscriptManifest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_checkpoint: Option<SourceCheckpoint>,
    version: u32,
    organization_id: String,
    workspace_id: String,
    session_id: String,
    source_session_id: String,
    agent: TranscriptAgent,
    agent_name: String,
    repository_url: String,
    working_directory: String,
    branch: String,
    head_sha: String,
    #[serde(default)]
    pull_request_url: String,
    title: String,
    completeness: TranscriptCompletenessArg,
    redaction_policy_version: String,
    segments: Vec<SpoolSegment>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SpoolSegment {
    // Older manifests cannot distinguish unsent segments from a lost response.
    // Preserve their frozen descriptor; new segments pin it before first send.
    #[serde(default = "legacy_upload_started")]
    upload_started: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stored_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    encoding: Option<SpoolEncoding>,
    /// Freeze descriptor metadata with the immutable bytes for retry/replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    metadata: Option<TranscriptMetadata>,
    segment_index: u64,
    first_entry_index: u64,
    last_entry_index: u64,
    omitted_entry_count: u64,
    path: String,
    size_bytes: u64,
    sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    upload: Option<UploadReceipt>,
}

/// A projection of redacted producer records, pinned when a segment is prepared.
/// Legacy segments have no projection and keep their existing wire descriptor.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct TranscriptMetadata {
    repository_url: String,
    working_directory: String,
    branch: String,
    head_sha: String,
    pull_request_url: String,
    title: String,
    started_at: Option<String>,
    completeness: TranscriptCompletenessArg,
}

#[derive(Default)]
struct CapturedMetadata {
    first_prompt: Option<String>,
    session_title: Option<String>,
    started_at: Option<String>,
    working_directory: Option<String>,
}

impl CapturedMetadata {
    fn observe(&mut self, event: &Value, source_session_id: &str) {
        if event["type"] == "message"
            && event["message"]["role"] == "user"
            && self.first_prompt.is_none()
        {
            let content = &event["message"]["content"];
            let text = content.as_str().map(str::to_string).or_else(|| {
                content.as_array().map(|blocks| {
                    blocks
                        .iter()
                        .filter(|block| block["type"] == "text")
                        .filter_map(|block| block["text"].as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
            });
            self.first_prompt = text.filter(|text| !text.trim().is_empty());
        }
        if event["type"] == "session" && event["id"] == source_session_id {
            if let Some(timestamp) = event["timestamp"]
                .as_str()
                .filter(|value| proto_timestamp(value).is_some())
            {
                self.started_at.get_or_insert_with(|| timestamp.to_string());
            }
            if let Some(cwd) = event["cwd"]
                .as_str()
                .filter(|value| !value.trim().is_empty())
            {
                self.working_directory = Some(cwd.trim().to_string());
            }
            if let Some(subject) = event["subject"]
                .as_str()
                .map(str::trim)
                .filter(|subject| !subject.is_empty())
            {
                self.session_title
                    .get_or_insert_with(|| subject.chars().take(256).collect());
            }
        }
    }

    fn fallback_title(&self) -> String {
        self.session_title.clone().unwrap_or_else(|| {
            self.first_prompt
                .as_deref()
                .unwrap_or_default()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(256)
                .collect()
        })
    }
}

fn legacy_upload_started() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct UploadReceipt {
    segment_id: String,
    object_id: String,
    version_id: String,
    replayed: bool,
    recorded_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ReceiptJournalEntry {
    segment_index: u64,
    sha256: String,
    receipt: UploadReceipt,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct WireCapabilityCache {
    endpoint: String,
    encoding: WireEncoding,
    expires_at: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum TranscriptError {
    #[error("invalid transcript input: {0}")]
    InvalidInput(String),
    #[error("transcript I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("transcript JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("transcript upload failed: {0}")]
    Upload(String),
    #[error("transcript hook failed: {0}")]
    Hook(String),
}

pub fn execute_transcript(
    args: TranscriptArgs,
    state_dir: Option<&Path>,
) -> Result<Value, TranscriptError> {
    match args.command {
        TranscriptCommand::Prepare(args) => prepare_transcript(args, state_dir),
        TranscriptCommand::Push(args) => push_transcript(args),
        TranscriptCommand::Hook(args) => capture_hook(args, state_dir),
    }
}

fn capture_hook(
    args: HookTranscriptArgs,
    state_dir: Option<&Path>,
) -> Result<Value, TranscriptError> {
    let payload: AgentHookPayload = serde_json::from_reader(std::io::stdin().lock())?;
    capture_hook_payload(args, payload, state_dir, platform_token())
}

/// Capture one Maestro lifecycle event using the authenticated product session.
pub fn capture_maestro_event(
    event: MaestroTranscriptEvent,
    state_dir: Option<&Path>,
) -> Result<Value, TranscriptError> {
    let args = HookTranscriptArgs {
        agent: TranscriptAgent::Maestro,
        organization: Some(event.organization_id.clone()),
        workspace: Some(event.workspace_id.clone()),
        endpoint: event.endpoint.clone(),
        wait_for_upload: false,
    };
    let payload = AgentHookPayload {
        hook_event_name: event.event_name,
        session_id: event.source_session_id,
        cwd: event.cwd,
        transcript_path: event.transcript_path,
        transcript_size_before: event.transcript_size_before,
        organization_id: Some(event.organization_id),
        workspace_id: Some(event.workspace_id),
        model: event.model,
        prompt: None,
    };
    capture_hook_payload(args, payload, state_dir, event.access_token)
}

fn capture_hook_payload(
    args: HookTranscriptArgs,
    payload: AgentHookPayload,
    state_dir: Option<&Path>,
    access_token: Option<String>,
) -> Result<Value, TranscriptError> {
    validate_identifier("hook session_id", &payload.session_id, 1024)?;
    let git_repo = git_value(&payload.cwd, &["rev-parse", "--show-toplevel"]).map(PathBuf::from);
    let repo = git_repo.clone().unwrap_or_else(|| payload.cwd.clone());
    let repository_url = git_value(&repo, &["config", "--get", "remote.origin.url"])
        .unwrap_or_else(|| repo.display().to_string());
    let session_id = stable_session_id(args.agent, &payload.session_id, &repository_url);
    let agent_name = args.agent.storage_name();
    let organization = args
        .organization
        .or(payload.organization_id.clone())
        .or_else(|| {
            first_env(&[
                "MAESTRO_EVALOPS_ORG_ID",
                "EVALOPS_ORGANIZATION_ID",
                "DEIXIC_ORGANIZATION_ID",
                "DX_ORGANIZATION_ID",
            ])
        });
    let workspace = args.workspace.or(payload.workspace_id.clone()).or_else(|| {
        first_env(&[
            "MAESTRO_EVALOPS_WORKSPACE_ID",
            "EVALOPS_WORKSPACE_ID",
            "DEIXIC_WORKSPACE_ID",
            "DX_WORKSPACE_ID",
        ])
    });
    let endpoint = args.endpoint.or_else(platform_endpoint);

    match payload.hook_event_name.as_str() {
        "SessionStart" | "UserPromptSubmit" => {
            if git_repo.is_some() {
                touch_hook_session(
                    &repo,
                    &session_id,
                    &payload.session_id,
                    agent_name,
                    payload.model.as_deref(),
                    payload.prompt.as_deref(),
                )
                .map_err(|error| TranscriptError::Hook(error.to_string()))?;
            }
            let retry = if payload.hook_event_name == "SessionStart" {
                retry_pending_manifests(
                    state_dir,
                    organization.as_deref(),
                    workspace.as_deref(),
                    &repository_url,
                    endpoint.as_deref(),
                    access_token.as_deref(),
                    args.wait_for_upload,
                )?
            } else {
                json!({"status": "not_requested"})
            };
            Ok(json!({
                "operation": "transcript.hook",
                "event": payload.hook_event_name,
                "session_id": session_id,
                "lease": "active",
                "retry": retry,
            }))
        }
        "PostMessage" | "SessionEnd" => {
            // Subagent sessions are stamped with a real session id but never
            // a transcript path (`agent.set_session_context(Some(record.id),
            // "subagent_start", false)` in `tools/subagents.rs` -- "the child
            // record is not a SessionManager transcript cleanup owner"), and
            // exec/print-mode's own top-level session likewise has no
            // SessionManager-owned JSONL file to point at. Neither omission
            // is a hook failure: there is simply nothing to spool, so skip
            // the same way a missing tenant identity is skipped below
            // instead of hard-failing every PostMessage/SessionEnd for these
            // sessions.
            let Some(transcript_path) = payload.transcript_path else {
                if payload.hook_event_name == "SessionEnd" && git_repo.is_some() {
                    clear_hook_session(&repo, &payload.session_id)
                        .map_err(|error| TranscriptError::Hook(error.to_string()))?;
                }
                return Ok(json!({
                    "operation": "transcript.hook",
                    "event": payload.hook_event_name,
                    "session_id": session_id,
                    "capture": "skipped",
                    "reason": "hook payload omitted transcript_path",
                }));
            };
            if payload.hook_event_name == "PostMessage"
                && args.agent == TranscriptAgent::Maestro
                && let Some(size_before) = payload.transcript_size_before
            {
                wait_for_transcript_growth(&transcript_path, size_before)?;
            }
            let (Some(organization), Some(workspace)) = (organization, workspace) else {
                if payload.hook_event_name == "SessionEnd" && git_repo.is_some() {
                    clear_hook_session(&repo, &payload.session_id)
                        .map_err(|error| TranscriptError::Hook(error.to_string()))?;
                }
                return Ok(json!({
                    "operation": "transcript.hook",
                    "event": payload.hook_event_name,
                    "session_id": session_id,
                    "capture": "skipped",
                    "reason": "authenticated organization and workspace identity are required",
                }));
            };
            let prepared = prepare_transcript_with_options(
                PrepareTranscriptArgs {
                    input: transcript_path,
                    agent: args.agent,
                    source_session_id: payload.session_id.clone(),
                    session_id: Some(session_id.clone()),
                    organization,
                    workspace,
                    repository_url: Some(repository_url),
                    working_directory: Some(".".to_string()),
                    branch: git_value(&repo, &["branch", "--show-current"]),
                    head_sha: git_value(&repo, &["rev-parse", "HEAD"]),
                    title: None,
                    completeness: if args.agent == TranscriptAgent::Maestro {
                        // A Maestro session may be switched away from and resumed,
                        // so SessionEnd is a capture boundary, not finality.
                        TranscriptCompletenessArg::InProgress
                    } else {
                        TranscriptCompletenessArg::Complete
                    },
                },
                state_dir,
                true,
            )?;
            let manifest = prepared["manifest"]
                .as_str()
                .map(PathBuf::from)
                .ok_or_else(|| {
                    TranscriptError::Hook("prepare omitted manifest path".to_string())
                })?;
            let upload = if let Some(endpoint) = endpoint {
                validate_endpoint(&endpoint)?;
                if args.wait_for_upload {
                    push_transcript(PushTranscriptArgs {
                        manifest: manifest.clone(),
                        endpoint,
                        token: access_token.clone(),
                    })?
                } else {
                    spawn_background_upload(&manifest, &endpoint, access_token.clone())?;
                    json!({"status": "started"})
                }
            } else {
                json!({"status": "pending", "reason": "PLATFORM_API_URL is not configured"})
            };
            if payload.hook_event_name == "SessionEnd" && git_repo.is_some() {
                clear_hook_session(&repo, &payload.session_id)
                    .map_err(|error| TranscriptError::Hook(error.to_string()))?;
            }
            Ok(json!({
                "operation": "transcript.hook",
                "event": payload.hook_event_name,
                "session_id": session_id,
                "manifest": manifest,
                "capture": "spooled",
                "upload": upload,
            }))
        }
        event => Err(TranscriptError::Hook(format!(
            "unsupported hook_event_name {event:?}"
        ))),
    }
}

fn wait_for_transcript_growth(path: &Path, size_before: u64) -> Result<(), TranscriptError> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if fs::metadata(path)
            .map(|metadata| metadata.len() > size_before)
            .unwrap_or(false)
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(TranscriptError::Hook(format!(
                "Maestro transcript did not flush past byte {size_before} before PostMessage capture"
            )));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn platform_endpoint() -> Option<String> {
    first_env(&[
        "PLATFORM_API_URL",
        "EVALOPS_PLATFORM_API_URL",
        "DEIXIC_PLATFORM_URL",
        "DX_PLATFORM_URL",
        "MAESTRO_EVALOPS_BASE_URL",
    ])
}

fn first_env(names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        std::env::var(name)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

fn platform_token() -> Option<String> {
    first_env(&[
        "PLATFORM_API_TOKEN",
        "MAESTRO_EVALOPS_ACCESS_TOKEN",
        "EVALOPS_ACCESS_TOKEN",
        "DEIXIC_PLATFORM_TOKEN",
        "DX_PLATFORM_TOKEN",
    ])
}

fn git_value(repo: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env_remove("GIT_INDEX_FILE")
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn retry_pending_manifests(
    state_dir: Option<&Path>,
    organization: Option<&str>,
    workspace: Option<&str>,
    repository_url: &str,
    endpoint: Option<&str>,
    access_token: Option<&str>,
    wait_for_upload: bool,
) -> Result<Value, TranscriptError> {
    let (Some(organization), Some(workspace), Some(endpoint)) = (organization, workspace, endpoint)
    else {
        return Ok(json!({"status": "not_configured", "manifests": 0}));
    };
    validate_endpoint(endpoint)?;
    let root = transcript_state_root(state_dir)?;
    let mut pending = Vec::new();
    for entry in fs::read_dir(root)? {
        let Ok(entry) = entry else { continue };
        let manifest_path = entry.path().join("manifest.json");
        if !manifest_path.is_file() {
            continue;
        }
        let Ok(manifest) = read_locked_manifest(&manifest_path) else {
            continue;
        };
        if manifest.organization_id == organization
            && manifest.workspace_id == workspace
            && manifest.repository_url == repository_url
            && manifest
                .segments
                .iter()
                .any(|segment| segment.upload.is_none())
        {
            pending.push(manifest_path);
        }
    }
    if pending.is_empty() {
        return Ok(json!({"status": "complete", "manifests": 0}));
    }
    if !wait_for_upload {
        for manifest in &pending {
            spawn_background_upload(manifest, endpoint, access_token.map(str::to_owned))?;
        }
        return Ok(json!({"status": "started", "manifests": pending.len()}));
    }

    let mut failed = 0_usize;
    for manifest in &pending {
        if push_transcript(PushTranscriptArgs {
            manifest: manifest.clone(),
            endpoint: endpoint.to_string(),
            token: access_token.map(str::to_owned),
        })
        .is_err()
        {
            failed += 1;
        }
    }
    Ok(json!({
        "status": if failed == 0 { "complete" } else { "partial" },
        "manifests": pending.len(),
        "failed": failed,
    }))
}

fn read_locked_manifest(path: &Path) -> Result<TranscriptManifest, TranscriptError> {
    let directory = path.parent().ok_or_else(|| {
        TranscriptError::InvalidInput("manifest path must have a parent directory".to_string())
    })?;
    let _lock = lock_manifest_directory(directory)?;
    Ok(read_manifest_with_receipts(path)?.0)
}

fn read_manifest_with_receipts(path: &Path) -> Result<(TranscriptManifest, bool), TranscriptError> {
    let directory = path.parent().ok_or_else(|| {
        TranscriptError::InvalidInput("manifest path must have a parent directory".to_string())
    })?;
    let mut manifest: TranscriptManifest = serde_json::from_reader(File::open(path)?)?;
    validate_manifest(&manifest)?;
    let journal_path = directory.join(RECEIPT_JOURNAL_FILE);
    if !journal_path.is_file() {
        return Ok((manifest, false));
    }
    let mut reader = BufReader::new(File::open(&journal_path)?);
    let mut line = Vec::new();
    let mut committed_bytes = 0_u64;
    let mut line_number = 0;
    let mut incomplete_tail = false;
    while reader.read_until(b'\n', &mut line)? > 0 {
        if !line.ends_with(b"\n") {
            incomplete_tail = true;
            break;
        }
        committed_bytes += line.len() as u64;
        line_number += 1;
        if line.iter().all(u8::is_ascii_whitespace) {
            line.clear();
            continue;
        }
        let entry: ReceiptJournalEntry = serde_json::from_slice(&line).map_err(|error| {
            TranscriptError::InvalidInput(format!(
                "receipt journal line {} is invalid: {error}",
                line_number
            ))
        })?;
        let index = usize::try_from(entry.segment_index).map_err(|_| {
            TranscriptError::InvalidInput("receipt journal segment index is too large".to_string())
        })?;
        let segment = manifest.segments.get_mut(index).ok_or_else(|| {
            TranscriptError::InvalidInput(format!(
                "receipt journal references missing segment {}",
                entry.segment_index
            ))
        })?;
        if segment.segment_index != entry.segment_index || segment.sha256 != entry.sha256 {
            return Err(TranscriptError::InvalidInput(format!(
                "receipt journal does not match segment {}",
                entry.segment_index
            )));
        }
        if let Some(existing) = &segment.upload {
            if existing != &entry.receipt {
                return Err(TranscriptError::InvalidInput(format!(
                    "receipt journal conflicts with manifest segment {}",
                    entry.segment_index
                )));
            }
        } else {
            segment.upload = Some(entry.receipt);
        }
        line.clear();
    }
    validate_manifest(&manifest)?;
    if incomplete_tail {
        // A crash may interrupt write_all before its newline. Keep validated
        // complete receipts and replay the uncommitted segment idempotently.
        // Callers hold the manifest lock across recovery and journal appends.
        let journal = OpenOptions::new().write(true).open(&journal_path)?;
        journal.set_len(committed_bytes)?;
        journal.sync_all()?;
    }
    Ok((manifest, true))
}

fn append_receipt_journal(
    directory: &Path,
    segment_index: u64,
    sha256: &str,
    receipt: &UploadReceipt,
) -> Result<(), TranscriptError> {
    let mut options = OpenOptions::new();
    options.append(true).create(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(directory.join(RECEIPT_JOURNAL_FILE))?;
    let mut bytes = serde_json::to_vec(&ReceiptJournalEntry {
        segment_index,
        sha256: sha256.to_string(),
        receipt: receipt.clone(),
    })?;
    bytes.push(b'\n');
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}

fn remove_receipt_journal(directory: &Path) -> Result<(), TranscriptError> {
    match fs::remove_file(directory.join(RECEIPT_JOURNAL_FILE)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn compact_receipt_journal(
    manifest_path: &Path,
    manifest: &TranscriptManifest,
) -> Result<(), TranscriptError> {
    let directory = manifest_path.parent().ok_or_else(|| {
        TranscriptError::InvalidInput("manifest path must have a parent directory".to_string())
    })?;
    if !directory.join(RECEIPT_JOURNAL_FILE).is_file() {
        return Ok(());
    }
    // Manifest first, journal second: a crash between these operations leaves
    // a replayable duplicate rather than losing an accepted receipt.
    write_private_json(manifest_path, manifest)?;
    remove_receipt_journal(directory)
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn normalized_endpoint(endpoint: &str) -> &str {
    endpoint.trim_end_matches('/')
}

fn cached_wire_encoding(directory: &Path, endpoint: &str) -> Option<WireEncoding> {
    let cache: WireCapabilityCache =
        serde_json::from_reader(File::open(directory.join(WIRE_CAPABILITY_CACHE_FILE)).ok()?)
            .ok()?;
    (cache.endpoint == normalized_endpoint(endpoint) && cache.expires_at > unix_seconds())
        .then_some(cache.encoding)
}

fn cache_wire_encoding(
    directory: &Path,
    endpoint: &str,
    encoding: WireEncoding,
) -> Result<(), TranscriptError> {
    write_private_json(
        &directory.join(WIRE_CAPABILITY_CACHE_FILE),
        &WireCapabilityCache {
            endpoint: normalized_endpoint(endpoint).to_string(),
            encoding,
            expires_at: unix_seconds().saturating_add(WIRE_CAPABILITY_TTL_SECS),
        },
    )
}

fn clear_wire_encoding_cache(directory: &Path) -> Result<(), TranscriptError> {
    match fs::remove_file(directory.join(WIRE_CAPABILITY_CACHE_FILE)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn claim_upload_schedule(directory: &Path) -> Result<bool, TranscriptError> {
    let path = directory.join(UPLOAD_SCHEDULE_FILE);
    for _ in 0..2 {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut marker) => {
                if let Err(error) = writeln!(marker, "{} {}", std::process::id(), unix_seconds()) {
                    remove_upload_schedule(directory);
                    return Err(error.into());
                }
                if let Err(error) = marker.sync_all() {
                    remove_upload_schedule(directory);
                    return Err(error.into());
                }
                return Ok(true);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = fs::metadata(&path)
                    .and_then(|metadata| metadata.modified())
                    .and_then(|modified| modified.elapsed().map_err(std::io::Error::other))
                    .is_ok_and(|age| age.as_secs() >= UPLOAD_SCHEDULE_STALE_SECS);
                if stale {
                    match fs::remove_file(&path) {
                        Ok(()) => continue,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(error) => return Err(error.into()),
                    }
                }
                return Ok(false);
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(false)
}

fn remove_upload_schedule(directory: &Path) {
    let _ = fs::remove_file(directory.join(UPLOAD_SCHEDULE_FILE));
}

struct UploadScheduleGuard {
    directory: Option<PathBuf>,
}

impl UploadScheduleGuard {
    fn new(directory: &Path) -> Self {
        Self {
            directory: Some(directory.to_path_buf()),
        }
    }

    fn release(&mut self) -> Result<(), TranscriptError> {
        if let Some(directory) = &self.directory {
            match fs::remove_file(directory.join(UPLOAD_SCHEDULE_FILE)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        self.directory = None;
        Ok(())
    }
}

impl Drop for UploadScheduleGuard {
    fn drop(&mut self) {
        if let Some(directory) = &self.directory {
            remove_upload_schedule(directory);
        }
    }
}

fn lock_manifest_directory(directory: &Path) -> Result<File, TranscriptError> {
    lock_directory_file(directory, ".manifest.lock")
}

fn lock_directory_file(directory: &Path, name: &str) -> Result<File, TranscriptError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options.open(directory.join(name))?;
    lock.lock_exclusive()?;
    Ok(lock)
}

fn spawn_background_upload(
    manifest: &Path,
    endpoint: &str,
    token: Option<String>,
) -> Result<(), TranscriptError> {
    let manifest_dir = manifest.parent().ok_or_else(|| {
        TranscriptError::InvalidInput("manifest path must have a parent directory".to_string())
    })?;
    if !claim_upload_schedule(manifest_dir)? {
        return Ok(());
    }
    let manifest = manifest.to_path_buf();
    let endpoint = endpoint.to_string();
    if token.is_some() {
        let spawned = std::thread::Builder::new()
            .name("transcript-upload".to_string())
            .spawn(move || {
                if let Err(error) = push_transcript(PushTranscriptArgs {
                    manifest: manifest.clone(),
                    endpoint,
                    token,
                }) {
                    // The caller may own an active TUI or a protocol stream.
                    // Even a tracing subscriber can target stderr, so retain
                    // this diagnostic beside the spool instead of printing.
                    let _ = record_background_upload_error(&manifest, &error);
                }
            });
        if let Err(error) = spawned {
            remove_upload_schedule(manifest_dir);
            return Err(error.into());
        }
        return Ok(());
    }
    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(error) => {
            remove_upload_schedule(manifest_dir);
            return Err(error.into());
        }
    };
    let mut command = Command::new(executable);
    command
        .args(["transcript", "push", "--manifest"])
        .arg(&manifest)
        .args(["--endpoint", &endpoint])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(token) = platform_token() {
        command.env("PLATFORM_API_TOKEN", token);
    }
    match command.spawn() {
        Ok(_) => Ok(()),
        Err(error) => {
            remove_upload_schedule(manifest_dir);
            Err(error.into())
        }
    }
}

fn record_background_upload_error(
    manifest: &Path,
    error: &TranscriptError,
) -> Result<(), TranscriptError> {
    let directory = manifest.parent().ok_or_else(|| {
        TranscriptError::InvalidInput("manifest path must have a parent directory".into())
    })?;
    let _lock = lock_manifest_directory(directory)?;
    write_private_json(
        &directory.join("last-background-upload-error.json"),
        &serde_json::json!({
            "occurred_at": chrono::Utc::now().to_rfc3339(),
            "error": error.to_string().chars().take(1024).collect::<String>(),
        }),
    )
}

fn prepare_transcript(
    args: PrepareTranscriptArgs,
    state_dir: Option<&Path>,
) -> Result<Value, TranscriptError> {
    prepare_transcript_with_options(args, state_dir, false)
}

fn prepare_transcript_with_options(
    args: PrepareTranscriptArgs,
    state_dir: Option<&Path>,
    allow_partial_tail: bool,
) -> Result<Value, TranscriptError> {
    validate_identifier("organization", &args.organization, 255)?;
    validate_identifier("workspace", &args.workspace, 255)?;
    validate_identifier("source_session_id", &args.source_session_id, 1024)?;
    if !args.input.is_file() {
        return Err(TranscriptError::InvalidInput(format!(
            "--input is not a readable file: {}",
            args.input.display()
        )));
    }
    let repo = args
        .working_directory
        .as_deref()
        .map(PathBuf::from)
        .or_else(|| args.input.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let session_id = args.session_id.unwrap_or_else(|| {
        stable_session_id(
            args.agent,
            &args.source_session_id,
            args.repository_url.as_deref().unwrap_or_default(),
        )
    });
    validate_identifier("session_id", &session_id, 255)?;
    let spool_root = transcript_state_root(state_dir)?.join(&session_id);
    create_private_dir(&spool_root)?;
    let _manifest_lock = lock_manifest_directory(&spool_root)?;

    let manifest_path = spool_root.join("manifest.json");
    let (existing, receipt_journal_present): (Option<TranscriptManifest>, bool) =
        if manifest_path.is_file() {
            let (manifest, journal_present) = read_manifest_with_receipts(&manifest_path)?;
            (Some(manifest), journal_present)
        } else {
            (None, false)
        };
    let mut capture = read_capture(CaptureOptions {
        input: &args.input,
        agent: args.agent,
        source_session_id: &args.source_session_id,
        repo: &repo,
        spool_root: &spool_root,
        repository_url: args.repository_url.as_deref(),
        existing: existing.as_ref(),
        allow_partial_tail,
    })?;
    if capture.parsed_entries == 0 && capture.reused_entries == 0 {
        return Err(TranscriptError::InvalidInput(
            "input contains no JSONL entries".to_string(),
        ));
    }
    let pull_request_url = first_env(&["EVALOPS_PULL_REQUEST_URL", "GITHUB_PULL_REQUEST_URL"])
        .and_then(|value| canonical_pull_request_url(&value, args.repository_url.as_deref()))
        .or_else(|| capture.pull_request_url.clone())
        .or_else(|| {
            existing
                .as_ref()
                .filter(|_| capture.reused_entries > 0)
                .map(|manifest| manifest.pull_request_url.clone())
        })
        .unwrap_or_default();
    let mut manifest = TranscriptManifest {
        source_checkpoint: capture.checkpoint.clone(),
        version: MANIFEST_VERSION,
        organization_id: args.organization,
        workspace_id: args.workspace,
        session_id: session_id.clone(),
        source_session_id: args.source_session_id,
        agent: args.agent,
        agent_name: args.agent.storage_name().to_string(),
        repository_url: args.repository_url.unwrap_or_default(),
        working_directory: args.working_directory.unwrap_or_default(),
        branch: args.branch.unwrap_or_default(),
        head_sha: args.head_sha.unwrap_or_default(),
        pull_request_url,
        title: args.title.unwrap_or_default(),
        completeness: args.completeness,
        redaction_policy_version: REDACTION_POLICY_VERSION.to_string(),
        segments: Vec::new(),
    };
    let mut metadata = transcript_metadata(&manifest, &capture.metadata);
    if capture.reused_entries > 0
        && let Some(previous) = existing
            .as_ref()
            .and_then(|manifest| manifest.segments.last())
            .and_then(|segment| segment.metadata.as_ref())
    {
        metadata.started_at = metadata.started_at.or_else(|| previous.started_at.clone());
        metadata.working_directory = previous.working_directory.clone();
        if manifest.title.is_empty() && !previous.title.is_empty() {
            metadata.title = previous.title.clone();
        }
    }
    let new_segment_start;
    if let Some(existing) = existing {
        validate_append_identity(&existing, &manifest)?;
        if capture.reused_entries > 0 {
            for segment in &existing.segments {
                verify_segment_storage(segment, &spool_root)?;
            }
        }
        let mut segments = existing.segments;
        let next_segment_index = segments.len() as u64;
        new_segment_start = segments.len();
        debug_assert_eq!(
            next_segment_index,
            capture
                .segments
                .first()
                .map_or(next_segment_index, |segment| segment.segment_index)
        );
        segments.extend(capture.segments.iter().cloned());
        manifest.segments = segments;
    } else {
        new_segment_start = 0;
        manifest.segments = capture.segments.clone();
    }
    for segment in &mut manifest.segments[new_segment_start..] {
        segment.metadata = Some(metadata.clone());
    }
    write_private_json(&manifest_path, &manifest)?;
    capture.commit();
    if receipt_journal_present {
        remove_receipt_journal(&spool_root)?;
    }
    Ok(json!({
        "operation": "transcript.prepare",
        "session_id": session_id,
        "manifest": manifest_path,
        "segments": manifest.segments.len(),
        "entries": capture.parsed_entries + capture.reused_entries,
        "reused_entries": capture.reused_entries,
        "size_bytes": manifest.segments.iter().map(|segment| segment.size_bytes).sum::<u64>(),
        "redaction_policy_version": REDACTION_POLICY_VERSION,
    }))
}

fn push_transcript(args: PushTranscriptArgs) -> Result<Value, TranscriptError> {
    push_transcript_after_unlock(args, || {})
}

fn push_transcript_after_unlock(
    args: PushTranscriptArgs,
    after_unlock: impl FnOnce(),
) -> Result<Value, TranscriptError> {
    let manifest_path = args.manifest;
    let manifest_dir = manifest_path.parent().ok_or_else(|| {
        TranscriptError::InvalidInput("manifest path must have a parent directory".to_string())
    })?;
    let mut schedule_guard = UploadScheduleGuard::new(manifest_dir);
    validate_endpoint(&args.endpoint)?;
    // One sender per spool, while preparation uses only the short manifest lock.
    let _upload_lock = lock_directory_file(manifest_dir, ".upload.lock")?;
    let mut manifest_lock = Some(lock_manifest_directory(manifest_dir)?);
    let (mut manifest, mut receipt_journal_present) = read_manifest_with_receipts(&manifest_path)?;
    let http: Agent = Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(5)))
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into();
    let url = format!(
        "{}/{}",
        args.endpoint.trim_end_matches('/'),
        TRANSCRIPT_METHOD
    );
    let mut wire_encoding = cached_wire_encoding(manifest_dir, &args.endpoint);
    let mut wire_bytes = 0_u64;
    let mut uncompressed_bytes = 0_u64;
    let mut body = Vec::new();
    let mut uploaded = 0_u64;
    let mut replayed = 0_u64;
    let mut skipped = 0_u64;
    let mut index = 0;
    loop {
        while index < manifest.segments.len() {
            if manifest.segments[index].upload.is_some() {
                skipped += 1;
                index += 1;
                continue;
            }
            let request = upload_request(&manifest, &manifest.segments[index], manifest_dir)?;
            if !manifest.segments[index].upload_started {
                if let Some(metadata) = manifest.segments[index].metadata.as_mut() {
                    metadata.completeness = manifest.completeness;
                }
                manifest.segments[index].upload_started = true;
                // Persist before any network I/O: an accepted request can lose its
                // response, and the server requires an identical descriptor on retry.
                write_private_json(&manifest_path, &manifest)?;
            }
            // Legacy descriptors derive from mutable manifest fields; retain their
            // old locking behavior until those historical segments are receipted.
            if manifest.segments[index].metadata.is_some() {
                drop(manifest_lock.take());
            }
            body.clear();
            request
                .encode(&mut body)
                .map_err(|error| TranscriptError::Upload(error.to_string()))?;
            let compressed = wire_encoding
                .map(|encoding| compress_body(&body, encoding))
                .transpose()?
                .flatten();
            let send = |payload: &[u8], encoding: Option<WireEncoding>| {
                let mut builder = http
                    .post(&url)
                    .header("Accept", "application/proto")
                    .header("Content-Type", "application/proto")
                    .header("Connect-Protocol-Version", "1")
                    .header("X-Organization-ID", &manifest.organization_id)
                    .header("X-Workspace-ID", &manifest.workspace_id);
                if let Some(token) = args.token.as_deref() {
                    builder = builder.header("Authorization", &format!("Bearer {token}"));
                }
                if let Some(encoding) = encoding {
                    builder = builder.header("Content-Encoding", encoding.header());
                }
                builder.send(payload)
            };
            let payload = compressed.as_deref().unwrap_or(&body);
            wire_bytes += payload.len() as u64;
            uncompressed_bytes += body.len() as u64;
            let response = send(payload, wire_encoding.filter(|_| compressed.is_some()));
            // Rolling deployments can route the next request to an older server.
            // Canonical replay identity makes one uncompressed retry safe.
            let mut compression_fallback = false;
            let mut response = match response {
                Err(ureq::Error::StatusCode(400 | 415)) if compressed.is_some() => {
                    compression_fallback = true;
                    clear_wire_encoding_cache(manifest_dir)?;
                    wire_bytes += body.len() as u64;
                    send(&body, None)
                }
                response => response,
            }
            .map_err(|error| map_upload_error(index, error))?;
            let advertised_encoding = if compression_fallback {
                None
            } else {
                preferred_encoding(
                    response
                        .headers()
                        .get("Accept-Encoding")
                        .and_then(|value| value.to_str().ok()),
                )
            };
            let status = response.status().as_u16();
            if !(200..300).contains(&status) {
                return Err(TranscriptError::Upload(format!(
                    "segment {index} returned HTTP {status}"
                )));
            }
            wire_encoding = advertised_encoding;
            if let Some(encoding) = advertised_encoding {
                cache_wire_encoding(manifest_dir, &args.endpoint, encoding)?;
            }
            let response_bytes = response
                .body_mut()
                .with_config()
                .limit(MAX_RESPONSE_BYTES)
                .read_to_vec()
                .map_err(|error| {
                    TranscriptError::Upload(format!(
                        "segment {index} returned an invalid or oversized response: {error}"
                    ))
                })?;
            let decoded =
                sessions_pb::RecordTranscriptSegmentResponse::decode(response_bytes.as_slice())
                    .map_err(|error| {
                        TranscriptError::Upload(format!(
                            "segment {index} returned invalid protobuf: {error}"
                        ))
                    })?;
            let segment = decoded.segment.as_ref().ok_or_else(|| {
                TranscriptError::Upload(format!(
                    "segment {index} response omitted segment metadata"
                ))
            })?;
            if segment.sha256 != manifest.segments[index].sha256
                || segment.segment_index != manifest.segments[index].segment_index
            {
                return Err(TranscriptError::Upload(format!(
                    "segment {index} response did not match the spooled digest and index"
                )));
            }
            if manifest_lock.is_none() {
                manifest_lock = Some(lock_manifest_directory(manifest_dir)?);
            }
            let (latest, _journal_present) = read_manifest_with_receipts(&manifest_path)?;
            validate_append_identity(&manifest, &latest)?;
            if latest
                .segments
                .get(index)
                .is_none_or(|current| current.sha256 != request.sha256)
            {
                return Err(TranscriptError::InvalidInput(
                    "spool changed during upload".into(),
                ));
            }
            // Merge the receipt into the latest manifest in an append-only journal,
            // retaining concurrently appended segments and their source cursor.
            manifest = latest;
            let receipt = UploadReceipt {
                segment_id: segment.segment_id.clone(),
                object_id: segment.object_id.clone(),
                version_id: segment.version_id.clone(),
                replayed: decoded.replayed,
                recorded_at: segment.recorded_at.clone(),
            };
            append_receipt_journal(
                manifest_dir,
                manifest.segments[index].segment_index,
                &manifest.segments[index].sha256,
                &receipt,
            )?;
            receipt_journal_present = true;
            manifest.segments[index].upload = Some(receipt);
            if decoded.replayed {
                replayed += 1;
            } else {
                uploaded += 1;
            }
            index += 1;
        }

        if manifest_lock.is_none() {
            manifest_lock = Some(lock_manifest_directory(manifest_dir)?);
        }
        let (latest, journal_present) = read_manifest_with_receipts(&manifest_path)?;
        receipt_journal_present |= journal_present;
        validate_append_identity(&manifest, &latest)?;
        let has_new_pending = latest
            .segments
            .iter()
            .enumerate()
            .any(|(position, segment)| position >= index && segment.upload.is_none());
        manifest = latest;
        if !has_new_pending {
            break;
        }
    }
    if receipt_journal_present {
        compact_receipt_journal(&manifest_path, &manifest)?;
    }
    // Release the marker while the final manifest lock still excludes capture.
    // A producer admitted after this point must be able to schedule a successor.
    schedule_guard.release()?;
    drop(manifest_lock);
    after_unlock();
    Ok(json!({
        "operation": "transcript.push",
        "session_id": manifest.session_id,
        "manifest": manifest_path,
        "uploaded": uploaded,
        "wire_bytes": wire_bytes,
        "uncompressed_bytes": uncompressed_bytes,
        "replayed": replayed,
        "already_receipted": skipped,
        "complete": manifest.segments.iter().all(|segment| segment.upload.is_some()),
    }))
}

#[cfg(test)]
fn redact_transcript_value(value: Value, repo: &Path) -> Value {
    redact_transcript_value_with(value, &Redactor::new(repo).unwrap())
}

fn redact_transcript_value_with(mut value: Value, redactor: &Redactor) -> Value {
    fn visit(value: &mut Value, redactor: &Redactor) {
        match value {
            Value::String(text) => *text = redactor.redact(text),
            Value::Array(values) => values.iter_mut().for_each(|value| visit(value, redactor)),
            Value::Object(values) => {
                for (key, value) in values.iter_mut() {
                    if is_secret_key(key) {
                        *value = Value::String("[redacted]".to_string());
                    } else {
                        visit(value, redactor);
                    }
                }
            }
            _ => {}
        }
    }
    visit(&mut value, redactor);
    value
}

fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase().replace('-', "_");
    matches!(
        key.as_str(),
        "authorization"
            | "cookie"
            | "set_cookie"
            | "api_key"
            | "apikey"
            | "secret"
            | "secret_key"
            | "password"
            | "token"
            | "access_token"
            | "refresh_token"
            | "private_key"
            | "headers"
            | "env"
            | "environment"
            | "environ"
    )
}

fn validate_append_identity(
    existing: &TranscriptManifest,
    incoming: &TranscriptManifest,
) -> Result<(), TranscriptError> {
    let matches = existing.version == incoming.version
        && existing.organization_id == incoming.organization_id
        && existing.workspace_id == incoming.workspace_id
        && existing.session_id == incoming.session_id
        && existing.source_session_id == incoming.source_session_id
        && existing.agent == incoming.agent
        && existing.agent_name == incoming.agent_name
        && existing.repository_url == incoming.repository_url
        && existing.working_directory == incoming.working_directory
        && existing.redaction_policy_version == incoming.redaction_policy_version;
    if !matches {
        return Err(TranscriptError::InvalidInput(
            "existing transcript manifest identity does not match this capture".to_string(),
        ));
    }
    Ok(())
}

fn write_segment(
    spool_root: &Path,
    segment_index: u64,
    first_entry_index: u64,
    last_entry_index: u64,
    bytes: &[u8],
) -> Result<SpoolSegment, TranscriptError> {
    let (stored, encoding) = encode_spool(bytes)?;
    let extension = if encoding.is_some() {
        "jsonl.zst"
    } else {
        "jsonl"
    };
    let filename = format!("segment-{segment_index:020}.{extension}");
    let path = spool_root.join(&filename);
    write_private(&path, &stored)?;
    Ok(SpoolSegment {
        upload_started: false,
        stored_sha256: encoding.map(|_| format!("{:x}", Sha256::digest(&stored))),
        encoding,
        metadata: None,
        segment_index,
        first_entry_index,
        last_entry_index,
        omitted_entry_count: 0,
        path: filename,
        size_bytes: bytes.len() as u64,
        sha256: format!("{:x}", Sha256::digest(bytes)),
        upload: None,
    })
}

fn upload_request(
    manifest: &TranscriptManifest,
    segment: &SpoolSegment,
    manifest_dir: &Path,
) -> Result<sessions_pb::RecordTranscriptSegmentRequest, TranscriptError> {
    let content = read_segment(segment, manifest_dir)?;
    let metadata = segment.metadata.as_ref();
    Ok(sessions_pb::RecordTranscriptSegmentRequest {
        organization_id: manifest.organization_id.clone(),
        workspace_id: manifest.workspace_id.clone(),
        session: Some(sessions_pb::AgentSessionDescriptor {
            session_id: manifest.session_id.clone(),
            agent_kind: manifest.agent.proto() as i32,
            agent_name: manifest.agent_name.clone(),
            source_session_id: manifest.source_session_id.clone(),
            repository_url: metadata.map_or_else(
                || manifest.repository_url.clone(),
                |value| value.repository_url.clone(),
            ),
            working_directory: metadata.map_or_else(
                || manifest.working_directory.clone(),
                |value| value.working_directory.clone(),
            ),
            branch: metadata.map_or_else(|| manifest.branch.clone(), |value| value.branch.clone()),
            head_sha: metadata
                .map_or_else(|| manifest.head_sha.clone(), |value| value.head_sha.clone()),
            pull_request_url: metadata.map_or_else(
                || manifest.pull_request_url.clone(),
                |value| value.pull_request_url.clone(),
            ),
            title: metadata.map_or_else(|| manifest.title.clone(), |value| value.title.clone()),
            started_at: metadata
                .and_then(|value| value.started_at.as_deref())
                .and_then(proto_timestamp),
            completeness: if segment.upload_started {
                metadata.map_or(manifest.completeness, |value| value.completeness)
            } else {
                manifest.completeness
            }
            .proto() as i32,
            ..Default::default()
        }),
        segment_index: segment.segment_index,
        first_entry_index: segment.first_entry_index,
        last_entry_index: segment.last_entry_index,
        content,
        sha256: segment.sha256.clone(),
        edge_redacted: true,
        redaction_policy_version: manifest.redaction_policy_version.clone(),
        omitted_entry_count: segment.omitted_entry_count,
    })
}

fn proto_timestamp(value: &str) -> Option<prost_types::Timestamp> {
    let value = chrono::DateTime::parse_from_rfc3339(value).ok()?;
    if !(-62_135_596_800..=253_402_300_799).contains(&value.timestamp()) {
        return None;
    }
    Some(prost_types::Timestamp {
        seconds: value.timestamp(),
        nanos: value.timestamp_subsec_nanos() as i32,
    })
}

fn transcript_metadata(
    manifest: &TranscriptManifest,
    captured: &CapturedMetadata,
) -> TranscriptMetadata {
    let mut metadata = TranscriptMetadata {
        // Keep the historical path-based identity in the manifest. A local path
        // is not a remote repository; do not publish it as one.
        repository_url: if Path::new(&manifest.repository_url).is_absolute() {
            String::new()
        } else {
            manifest.repository_url.clone()
        },
        working_directory: manifest.working_directory.clone(),
        branch: manifest.branch.clone(),
        head_sha: manifest.head_sha.clone(),
        pull_request_url: manifest.pull_request_url.clone(),
        title: manifest.title.clone(),
        started_at: None,
        completeness: manifest.completeness,
    };
    if manifest.agent == TranscriptAgent::Maestro {
        if let Some(started_at) = &captured.started_at {
            metadata.started_at = Some(started_at.clone());
        }
        if let Some(working_directory) = &captured.working_directory {
            metadata.working_directory = working_directory.clone();
        }
        if metadata.title.is_empty() {
            metadata.title = captured.fallback_title();
        }
    }
    metadata
}

fn validate_manifest(manifest: &TranscriptManifest) -> Result<(), TranscriptError> {
    if manifest.version != MANIFEST_VERSION {
        return Err(TranscriptError::InvalidInput(format!(
            "unsupported transcript manifest version {}",
            manifest.version
        )));
    }
    validate_identifier("organization_id", &manifest.organization_id, 255)?;
    validate_identifier("workspace_id", &manifest.workspace_id, 255)?;
    validate_identifier("session_id", &manifest.session_id, 255)?;
    validate_identifier("source_session_id", &manifest.source_session_id, 1024)?;
    if !matches!(
        manifest.redaction_policy_version.as_str(),
        REDACTION_POLICY_VERSION | LEGACY_REDACTION_POLICY_VERSION
    ) {
        return Err(TranscriptError::InvalidInput(format!(
            "unsupported redaction policy version {}",
            manifest.redaction_policy_version
        )));
    }
    if manifest.segments.is_empty() {
        return Err(TranscriptError::InvalidInput(
            "manifest has no transcript segments".to_string(),
        ));
    }
    let mut expected_entry_index = 0_u64;
    for (expected_index, segment) in manifest.segments.iter().enumerate() {
        if segment.segment_index != expected_index as u64
            || segment.first_entry_index != expected_entry_index
            || segment.last_entry_index < segment.first_entry_index
            || segment.size_bytes == 0
            || segment.size_bytes > MAX_SEGMENT_BYTES as u64
        {
            return Err(TranscriptError::InvalidInput(format!(
                "manifest segment {expected_index} has invalid indices or size"
            )));
        }
        expected_entry_index = segment.last_entry_index + 1;
    }
    Ok(())
}

fn safe_segment_path(manifest_dir: &Path, relative: &str) -> Result<PathBuf, TranscriptError> {
    let relative = Path::new(relative);
    let components = relative.components().collect::<Vec<_>>();
    if relative.is_absolute()
        || components.len() != 1
        || !matches!(components[0], std::path::Component::Normal(_))
    {
        return Err(TranscriptError::InvalidInput(
            "manifest segment path must be a single relative filename".to_string(),
        ));
    }
    Ok(manifest_dir.join(relative))
}

fn transcript_state_root(state_dir: Option<&Path>) -> Result<PathBuf, TranscriptError> {
    let root = state_dir.map(Path::to_path_buf).unwrap_or_else(|| {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(".dx")
    });
    let root = root.join("transcripts");
    create_private_dir(&root)?;
    Ok(root)
}

fn create_private_dir(path: &Path) -> Result<(), TranscriptError> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn write_private_json(path: &Path, value: &impl Serialize) -> Result<(), TranscriptError> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    write_private(path, &bytes)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), TranscriptError> {
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            TranscriptError::InvalidInput("spool path has no valid filename".to_string())
        })?;
    let temporary = path.with_file_name(format!(".{filename}.tmp-{}", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, path)?;
    Ok(())
}

fn validate_identifier(name: &str, value: &str, maximum: usize) -> Result<(), TranscriptError> {
    if value.is_empty()
        || value.trim() != value
        || value.len() > maximum
        || value.chars().any(char::is_control)
    {
        return Err(TranscriptError::InvalidInput(format!(
            "{name} must be normalized and between 1 and {maximum} bytes"
        )));
    }
    Ok(())
}

fn validate_endpoint(value: &str) -> Result<(), TranscriptError> {
    let (secure, remainder) = if let Some(remainder) = value.strip_prefix("https://") {
        (true, remainder)
    } else if let Some(remainder) = value.strip_prefix("http://") {
        (false, remainder)
    } else {
        return Err(TranscriptError::InvalidInput(
            "--endpoint must use http or https".to_string(),
        ));
    };
    let authority = remainder.split('/').next().unwrap_or_default();
    if authority.is_empty() || authority.contains('@') || value.chars().any(char::is_control) {
        return Err(TranscriptError::InvalidInput(
            "--endpoint must not contain credentials or control characters".to_string(),
        ));
    }
    let host = if let Some(host) = authority.strip_prefix('[') {
        host.split(']').next().unwrap_or_default()
    } else {
        authority.split(':').next().unwrap_or_default()
    };
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    if !secure && !loopback {
        return Err(TranscriptError::InvalidInput(
            "--endpoint must use HTTPS unless the host is loopback".to_string(),
        ));
    }
    Ok(())
}

fn detect_pull_request_url_bytes(entry: &[u8], repository_url: Option<&str>) -> Option<String> {
    let repository = github_repository_base(repository_url?)?;
    let prefix = format!("{repository}/pull/");
    let text = String::from_utf8_lossy(entry);
    let start = text.find(&prefix)? + prefix.len();
    let number = text[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    (!number.is_empty()).then(|| format!("{prefix}{number}"))
}

fn canonical_pull_request_url(value: &str, repository_url: Option<&str>) -> Option<String> {
    let repository = github_repository_base(repository_url?)?;
    let prefix = format!("{repository}/pull/");
    let value = value.trim().trim_end_matches('/');
    let number = value.strip_prefix(&prefix)?;
    (!number.is_empty() && number.chars().all(|character| character.is_ascii_digit()))
        .then(|| format!("{prefix}{number}"))
}

fn github_repository_base(value: &str) -> Option<String> {
    let value = value.trim().trim_end_matches('/').trim_end_matches(".git");
    let path = value
        .strip_prefix("https://github.com/")
        .or_else(|| value.strip_prefix("http://github.com/"))
        .or_else(|| value.strip_prefix("git@github.com:"))
        .or_else(|| value.strip_prefix("ssh://git@github.com/"))?;
    let mut components = path.split('/');
    let owner = components.next()?;
    let repository = components.next()?;
    if owner.is_empty() || repository.is_empty() || components.next().is_some() {
        return None;
    }
    Some(format!("https://github.com/{owner}/{repository}"))
}

fn stable_session_id(
    agent: TranscriptAgent,
    source_session_id: &str,
    repository_url: &str,
) -> String {
    let mut digest = Sha256::new();
    for value in [agent.storage_name(), source_session_id, repository_url] {
        digest.update((value.len() as u64).to_le_bytes());
        digest.update(value.as_bytes());
    }
    format!("agent-session-{:x}", digest.finalize())
}

fn map_upload_error(segment_index: usize, error: ureq::Error) -> TranscriptError {
    TranscriptError::Upload(match error {
        ureq::Error::StatusCode(status) => {
            format!("segment {segment_index} returned HTTP {status}")
        }
        ureq::Error::Timeout(_) => format!("segment {segment_index} upload timed out"),
        ureq::Error::HostNotFound => {
            format!("segment {segment_index} upload host was not found")
        }
        error => format!("segment {segment_index} upload failed: {error}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;
    use tempfile::tempdir;

    #[test]
    fn subagent_and_exec_sessions_skip_capture_instead_of_failing_without_a_transcript() {
        // Regression test for the exec/subagent capture failure: both
        // `agent.set_session_context(Some(record.id), "subagent_start",
        // false)` (subagents.rs) and exec/print-mode's top-level session
        // stamp a real session id onto the hook system without ever
        // resolving a SessionManager-owned transcript path. Before the fix,
        // `capture_hook_payload` hard-failed PostMessage and SessionEnd with
        // "<event> payload omitted transcript_path" whenever that happened.
        let temp = tempdir().unwrap();
        let state = temp.path().join("state");
        for event_name in ["PostMessage", "SessionEnd"] {
            let event = MaestroTranscriptEvent {
                event_name: event_name.to_string(),
                source_session_id: "subagent-session-1".to_string(),
                cwd: temp.path().to_path_buf(),
                transcript_path: None,
                transcript_size_before: None,
                organization_id: "org-1".to_string(),
                workspace_id: "workspace-1".to_string(),
                endpoint: None,
                access_token: None,
                model: None,
            };
            let result = capture_maestro_event(event, Some(&state)).unwrap_or_else(|error| {
                panic!("{event_name} must skip capture, not fail, without a transcript: {error}")
            });
            assert_eq!(result["capture"], "skipped");
            assert_eq!(result["reason"], "hook payload omitted transcript_path");
        }
    }

    fn prepare_native_fixture(input: PathBuf, state: &Path, branch: &str) -> PathBuf {
        let result = prepare_transcript(
            PrepareTranscriptArgs {
                input,
                agent: TranscriptAgent::Maestro,
                source_session_id: "maestro-quality-fixture".to_string(),
                session_id: Some("session-quality-fixture".to_string()),
                organization: "org-1".to_string(),
                workspace: "workspace-1".to_string(),
                repository_url: Some("/workspace/mono".to_string()),
                working_directory: Some(".".to_string()),
                branch: Some(branch.to_string()),
                head_sha: None,
                title: None,
                completeness: TranscriptCompletenessArg::InProgress,
            },
            Some(state),
        )
        .unwrap();
        PathBuf::from(result["manifest"].as_str().unwrap())
    }

    #[test]
    fn background_upload_errors_stay_out_of_terminal() {
        const CHILD: &str = "MAESTRO_TEST_BACKGROUND_UPLOAD_DIAGNOSTIC";
        if std::env::var_os(CHILD).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::background_upload_errors_stay_out_of_terminal",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "child failed: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                output.stderr.is_empty(),
                "background upload wrote to the terminal"
            );
            return;
        }
        let temp = tempdir().unwrap();
        let input = temp.path().join("session.jsonl");
        fs::write(&input, include_str!("../tests/fixtures/maestro.jsonl")).unwrap();
        let manifest = prepare_native_fixture(input, &temp.path().join("state"), "main");
        // Fail deterministically before network I/O through the real background sender.
        spawn_background_upload(&manifest, "ftp://invalid", Some("test-token".into())).unwrap();
        let diagnostic = manifest
            .parent()
            .unwrap()
            .join("last-background-upload-error.json");
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !diagnostic.exists() && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let report: Value = serde_json::from_reader(File::open(&diagnostic).unwrap()).unwrap();
        assert!(!report["error"].as_str().unwrap().is_empty());
        assert!(!report.to_string().contains("test-token"));
        assert!(report["occurred_at"].as_str().is_some());
        let pending = read_locked_manifest(&manifest).unwrap();
        assert!(
            pending
                .segments
                .iter()
                .all(|segment| segment.upload.is_none())
        );
        assert!(
            !manifest
                .parent()
                .unwrap()
                .join(UPLOAD_SCHEDULE_FILE)
                .exists()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(diagnostic).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn native_maestro_records_preserve_content_and_project_available_metadata() {
        let temp = tempdir().unwrap();
        let input = temp.path().join("session.jsonl");
        fs::write(&input, include_str!("../tests/fixtures/maestro.jsonl")).unwrap();
        let path = prepare_native_fixture(input, &temp.path().join("state"), "main");
        let manifest = read_locked_manifest(&path).unwrap();
        let request =
            upload_request(&manifest, &manifest.segments[0], path.parent().unwrap()).unwrap();
        let session = request.session.unwrap();
        assert_eq!(
            session.started_at.unwrap(),
            prost_types::Timestamp {
                seconds: 1788854400,
                nanos: 123_000_000
            }
        );
        assert_eq!(session.title, "Check the workspace");
        assert_eq!(session.working_directory, "/workspace/mono");
        assert!(
            session.repository_url.is_empty(),
            "a working directory is not a remote repository"
        );
        assert!(
            session.ended_at.is_none(),
            "capture does not establish task finality"
        );
        let records: Vec<Value> = String::from_utf8(request.content)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 5);
        assert_eq!(
            records[2]["event"]["message"]["content"][1]["id"],
            "call-check"
        );
        assert_eq!(records[3]["event"]["message"]["toolCallId"], "call-check");
        assert_eq!(records[3]["event"]["message"]["isError"], false);
        assert_eq!(records[2]["event"]["message"]["usage"]["input"], 12);
        assert_eq!(records[4]["source_index"], 4);
    }

    #[test]
    fn resumed_capture_freezes_existing_segment_metadata_for_retry() {
        let temp = tempdir().unwrap();
        let input = temp.path().join("session.jsonl");
        fs::write(&input, include_str!("../tests/fixtures/maestro.jsonl")).unwrap();
        let state = temp.path().join("state");
        let path = prepare_native_fixture(input.clone(), &state, "before");
        let before = read_locked_manifest(&path).unwrap();
        let request = upload_request(&before, &before.segments[0], path.parent().unwrap()).unwrap();
        writeln!(
            OpenOptions::new().append(true).open(&input).unwrap(),
            "{{\"type\":\"message\",\"message\":{{\"role\":\"user\",\"content\":\"Continue\"}}}}"
        )
        .unwrap();
        prepare_native_fixture(input, &state, "after");
        let after = read_locked_manifest(&path).unwrap();
        assert_eq!(after.segments.len(), 2);
        assert_eq!(
            request.encode_to_vec(),
            upload_request(&after, &after.segments[0], path.parent().unwrap())
                .unwrap()
                .encode_to_vec()
        );
        assert_eq!(
            upload_request(&after, &after.segments[1], path.parent().unwrap())
                .unwrap()
                .session
                .unwrap()
                .branch,
            "after"
        );
    }

    #[test]
    fn metadata_rejects_invalid_header_time_and_falls_back_to_user_text() {
        let temp = tempdir().unwrap();
        let input = temp.path().join("session.jsonl");
        let records = include_str!("../tests/fixtures/maestro.jsonl")
            .lines()
            .map(|line| {
                let mut value: Value = serde_json::from_str(line).unwrap();
                if value["type"] == "session" {
                    value.as_object_mut().unwrap().remove("subject");
                    value["timestamp"] = json!("not-a-timestamp");
                }
                serde_json::to_string(&value).unwrap()
            })
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&input, records).unwrap();
        let path = prepare_native_fixture(input, &temp.path().join("state"), "main");
        let manifest = read_locked_manifest(&path).unwrap();
        let session = upload_request(&manifest, &manifest.segments[0], path.parent().unwrap())
            .unwrap()
            .session
            .unwrap();
        assert!(session.started_at.is_none());
        assert_eq!(session.title, "Check the workspace");
    }

    #[test]
    fn legacy_spool_remains_readable_without_new_metadata() {
        let temp = tempdir().unwrap();
        let input = temp.path().join("session.jsonl");
        fs::write(&input, include_str!("../tests/fixtures/maestro.jsonl")).unwrap();
        let path = prepare_native_fixture(input, &temp.path().join("state"), "main");
        let mut value: Value = serde_json::from_reader(File::open(&path).unwrap()).unwrap();
        value["segments"][0]
            .as_object_mut()
            .unwrap()
            .remove("metadata");
        let manifest: TranscriptManifest = serde_json::from_value(value).unwrap();
        let request =
            upload_request(&manifest, &manifest.segments[0], path.parent().unwrap()).unwrap();
        assert!(request.session.as_ref().unwrap().started_at.is_none());
        assert_eq!(request.session.unwrap().repository_url, "/workspace/mono");
    }

    #[test]
    fn redaction_preserves_output_but_removes_secrets() {
        let value = json!({
            "output": "Bearer secret-value and ghp_abcdefghijklmnopqrstuvwxyz",
            "token": "must-not-survive",
            "nested": {"password": "must-not-survive", "text": "visible"},
        });
        let redacted = redact_transcript_value(value, Path::new("."));
        assert_eq!(redacted["token"], "[redacted]");
        assert_eq!(redacted["nested"]["password"], "[redacted]");
        assert_eq!(redacted["nested"]["text"], "visible");
        assert!(redacted["output"].as_str().unwrap().contains("[redacted]"));
        assert!(!redacted.to_string().contains("must-not-survive"));
    }

    #[test]
    fn prepare_spools_private_canonical_segments_and_manifest() {
        let temp = tempdir().unwrap();
        let input = temp.path().join("codex.jsonl");
        fs::write(
            &input,
            "{\"type\":\"message\",\"token\":\"secret\"}\n{\"type\":\"tool\",\"output\":\"ok\"}\n",
        )
        .unwrap();
        let state = temp.path().join("state");
        let output = prepare_transcript(
            PrepareTranscriptArgs {
                input,
                agent: TranscriptAgent::Codex,
                source_session_id: "source-1".to_string(),
                session_id: Some("session-1".to_string()),
                organization: "org-1".to_string(),
                workspace: "workspace-1".to_string(),
                repository_url: Some("https://github.com/evalops/mono".to_string()),
                working_directory: Some(temp.path().display().to_string()),
                branch: Some("main".to_string()),
                head_sha: Some("0123456789abcdef".to_string()),
                title: Some("test".to_string()),
                completeness: TranscriptCompletenessArg::Complete,
            },
            Some(&state),
        )
        .unwrap();
        let manifest_path = PathBuf::from(output["manifest"].as_str().unwrap());
        let manifest: TranscriptManifest =
            serde_json::from_reader(File::open(&manifest_path).unwrap()).unwrap();
        assert_eq!(manifest.segments.len(), 1);
        let segment_path = manifest_path
            .parent()
            .unwrap()
            .join(&manifest.segments[0].path);
        let content = String::from_utf8(
            read_segment(&manifest.segments[0], segment_path.parent().unwrap()).unwrap(),
        )
        .unwrap();
        assert!(content.ends_with('\n'));
        assert!(content.contains("[redacted]"));
        assert!(!content.contains("\"token\":\"secret\""));
        let request = upload_request(
            &manifest,
            &manifest.segments[0],
            manifest_path.parent().unwrap(),
        )
        .unwrap();
        assert!(request.edge_redacted);
        assert_eq!(request.redaction_policy_version, REDACTION_POLICY_VERSION);
        assert_eq!(request.last_entry_index, 1);
        assert_eq!(request.sha256, manifest.segments[0].sha256);
    }

    #[test]
    fn prepare_detects_the_repository_pull_request_from_redacted_transcript_content() {
        let temp = tempdir().unwrap();
        let input = temp.path().join("codex.jsonl");
        fs::write(
            &input,
            "{\"type\":\"message\",\"text\":\"Opened https://github.com/evalops/mono/pull/8044 for review\"}\n",
        )
        .unwrap();
        let state = temp.path().join("state");
        let output = prepare_transcript(
            PrepareTranscriptArgs {
                input,
                agent: TranscriptAgent::Codex,
                source_session_id: "source-pr-1".to_string(),
                session_id: Some("session-pr-1".to_string()),
                organization: "org-1".to_string(),
                workspace: "workspace-1".to_string(),
                repository_url: Some("git@github.com:evalops/mono.git".to_string()),
                working_directory: Some(temp.path().display().to_string()),
                branch: Some("agent/session-history".to_string()),
                head_sha: Some("0123456789abcdef".to_string()),
                title: Some("PR-aware session".to_string()),
                completeness: TranscriptCompletenessArg::Complete,
            },
            Some(&state),
        )
        .unwrap();
        let manifest_path = PathBuf::from(output["manifest"].as_str().unwrap());
        let manifest: TranscriptManifest =
            serde_json::from_reader(File::open(&manifest_path).unwrap()).unwrap();
        assert_eq!(
            manifest.pull_request_url,
            "https://github.com/evalops/mono/pull/8044"
        );
        let request = upload_request(
            &manifest,
            &manifest.segments[0],
            manifest_path.parent().unwrap(),
        )
        .unwrap();
        assert_eq!(
            request.session.unwrap().pull_request_url,
            "https://github.com/evalops/mono/pull/8044"
        );
    }

    #[test]
    fn preparing_a_resumed_maestro_session_appends_after_receipted_segments() {
        // Maestro sessions can be switched away from and later resumed. The
        // server owns immutable segment indices, so preparing the larger JSONL
        // must retain segment 0 and append the newly observed entry as segment
        // 1 instead of rewriting the earlier receipt.
        let temp = tempdir().unwrap();
        let input = temp.path().join("maestro.jsonl");
        fs::write(&input, "{\"type\":\"message\",\"text\":\"first\"}\n").unwrap();
        let state = temp.path().join("state");
        let prepare = || {
            prepare_transcript(
                PrepareTranscriptArgs {
                    input: input.clone(),
                    agent: TranscriptAgent::Maestro,
                    source_session_id: "maestro-resumed-1".to_string(),
                    session_id: Some("session-resumed-1".to_string()),
                    organization: "org-1".to_string(),
                    workspace: "workspace-1".to_string(),
                    repository_url: Some("https://github.com/evalops/mono".to_string()),
                    working_directory: Some(temp.path().display().to_string()),
                    branch: Some("main".to_string()),
                    head_sha: Some("0123456789abcdef".to_string()),
                    title: None,
                    completeness: TranscriptCompletenessArg::InProgress,
                },
                Some(&state),
            )
            .unwrap()
        };

        let first = prepare();
        let manifest_path = PathBuf::from(first["manifest"].as_str().unwrap());
        let mut first_manifest: TranscriptManifest =
            serde_json::from_reader(File::open(&manifest_path).unwrap()).unwrap();
        let first_digest = first_manifest.segments[0].sha256.clone();
        first_manifest.segments[0].upload = Some(UploadReceipt {
            segment_id: "segment-0".to_string(),
            object_id: "object-0".to_string(),
            version_id: "version-0".to_string(),
            replayed: false,
            recorded_at: "2026-08-31T00:00:00Z".to_string(),
        });
        write_private_json(&manifest_path, &first_manifest).unwrap();

        fs::write(
            &input,
            concat!(
                "{\"type\":\"message\",\"text\":\"first\"}\n",
                "{\"type\":\"message\",\"text\":\"second\"}\n"
            ),
        )
        .unwrap();
        prepare();

        let resumed: TranscriptManifest =
            serde_json::from_reader(File::open(&manifest_path).unwrap()).unwrap();
        assert_eq!(resumed.segments.len(), 2);
        assert_eq!(resumed.segments[0].sha256, first_digest);
        assert_eq!(
            resumed.segments[0].upload.as_ref().unwrap().object_id,
            "object-0"
        );
        assert_eq!(resumed.segments[1].segment_index, 1);
        assert_eq!(resumed.segments[1].first_entry_index, 1);
        assert_eq!(resumed.segments[1].last_entry_index, 1);
    }

    #[test]
    fn preparing_a_resumed_session_rejects_a_changed_spooled_prefix() {
        let temp = tempdir().unwrap();
        let input = temp.path().join("maestro.jsonl");
        fs::write(&input, "{\"type\":\"message\",\"text\":\"first\"}\n").unwrap();
        let state = temp.path().join("state");
        let prepare = || {
            prepare_transcript(
                PrepareTranscriptArgs {
                    input: input.clone(),
                    agent: TranscriptAgent::Maestro,
                    source_session_id: "maestro-changed-1".to_string(),
                    session_id: Some("session-changed-1".to_string()),
                    organization: "org-1".to_string(),
                    workspace: "workspace-1".to_string(),
                    repository_url: Some("https://github.com/evalops/mono".to_string()),
                    working_directory: Some(temp.path().display().to_string()),
                    branch: Some("main".to_string()),
                    head_sha: Some("0123456789abcdef".to_string()),
                    title: None,
                    completeness: TranscriptCompletenessArg::InProgress,
                },
                Some(&state),
            )
        };

        prepare().unwrap();
        fs::write(&input, "{\"type\":\"message\",\"text\":\"changed\"}\n").unwrap();
        let error = prepare().expect_err("an immutable prefix change must fail closed");
        assert!(error.to_string().contains("prefix changed"), "{error}");
    }

    #[test]
    fn manifest_paths_cannot_escape_the_private_spool() {
        let err = safe_segment_path(Path::new("/tmp/spool"), "../secret")
            .expect_err("parent traversal rejected");
        assert!(err.to_string().contains("single relative filename"));
        let err = safe_segment_path(Path::new("/tmp/spool"), "/tmp/secret")
            .expect_err("absolute path rejected");
        assert!(err.to_string().contains("single relative filename"));
    }

    #[test]
    fn remote_plaintext_endpoints_are_rejected_but_loopback_is_allowed() {
        assert!(validate_endpoint("https://platform.example.com").is_ok());
        assert!(validate_endpoint("http://127.0.0.1:8080").is_ok());
        assert!(validate_endpoint("http://localhost:8080").is_ok());
        assert!(validate_endpoint("http://127.evil.example").is_err());
        let error = validate_endpoint("http://platform.example.com")
            .expect_err("remote plaintext endpoint must fail closed");
        assert!(error.to_string().contains("HTTPS"), "{error}");
    }

    #[test]
    fn completed_sender_preserves_a_successor_scheduled_after_manifest_unlock() {
        let temp = tempdir().unwrap();
        let input = temp.path().join("session.jsonl");
        fs::write(&input, include_str!("../tests/fixtures/maestro.jsonl")).unwrap();
        let path = prepare_native_fixture(input, &temp.path().join("state"), "main");
        let directory = path.parent().unwrap();
        let mut saved = read_locked_manifest(&path).unwrap();
        for segment in &mut saved.segments {
            segment.upload = Some(UploadReceipt {
                segment_id: "accepted".into(),
                object_id: "object".into(),
                version_id: "version".into(),
                replayed: false,
                recorded_at: "now".into(),
            });
        }
        write_private_json(&path, &saved).unwrap();
        assert!(claim_upload_schedule(directory).unwrap());
        push_transcript_after_unlock(
            PushTranscriptArgs {
                manifest: path.clone(),
                endpoint: "http://127.0.0.1:1".into(),
                token: None,
            },
            || {
                let _producer_lock = lock_manifest_directory(directory).unwrap();
                assert!(
                    claim_upload_schedule(directory).unwrap(),
                    "the next producer must schedule a successor"
                );
            },
        )
        .unwrap();
        assert!(
            directory.join(UPLOAD_SCHEDULE_FILE).exists(),
            "the previous sender must not delete its successor's marker"
        );
    }

    #[test]
    fn receipt_journal_discards_only_an_unterminated_tail_before_replay() {
        let temp = tempdir().unwrap();
        let input = temp.path().join("session.jsonl");
        fs::write(&input, include_str!("../tests/fixtures/maestro.jsonl")).unwrap();
        let path = prepare_native_fixture(input, &temp.path().join("state"), "main");
        let directory = path.parent().unwrap();
        let saved = read_locked_manifest(&path).unwrap();
        let receipt = UploadReceipt {
            segment_id: "accepted".into(),
            object_id: "object".into(),
            version_id: "version".into(),
            replayed: false,
            recorded_at: "now".into(),
        };
        append_receipt_journal(directory, 0, &saved.segments[0].sha256, &receipt).unwrap();
        let journal = directory.join(RECEIPT_JOURNAL_FILE);
        let complete = fs::read(&journal).unwrap();
        for tail in [b"{\"segment_index\":".as_slice(), b"\xff\xfe".as_slice()] {
            let mut interrupted = complete.clone();
            interrupted.extend_from_slice(tail);
            fs::write(&journal, interrupted).unwrap();
            let recovered = read_locked_manifest(&path).unwrap();
            assert_eq!(recovered.segments[0].upload.as_ref(), Some(&receipt));
            assert_eq!(fs::read(&journal).unwrap(), complete);
        }
        append_receipt_journal(directory, 0, &saved.segments[0].sha256, &receipt).unwrap();
        assert!(read_locked_manifest(&path).is_ok());
        let mut corrupt = complete;
        corrupt.extend_from_slice(b"{invalid}\n");
        fs::write(&journal, &corrupt).unwrap();
        assert!(
            read_locked_manifest(&path).is_err(),
            "terminated corruption must fail closed"
        );
        assert_eq!(fs::read(journal).unwrap(), corrupt);
    }

    #[test]
    fn receipt_journal_recovers_an_accepted_receipt_before_compaction() {
        let temp = tempdir().unwrap();
        let input = temp.path().join("session.jsonl");
        fs::write(&input, include_str!("../tests/fixtures/maestro.jsonl")).unwrap();
        let path = prepare_native_fixture(input, &temp.path().join("state"), "main");
        let saved = read_locked_manifest(&path).unwrap();
        let receipt = UploadReceipt {
            segment_id: "segment-journal".to_string(),
            object_id: "object-journal".to_string(),
            version_id: "version-journal".to_string(),
            replayed: false,
            recorded_at: "2026-09-08T00:00:00Z".to_string(),
        };
        append_receipt_journal(
            path.parent().unwrap(),
            saved.segments[0].segment_index,
            &saved.segments[0].sha256,
            &receipt,
        )
        .unwrap();
        let recovered = read_locked_manifest(&path).unwrap();
        assert_eq!(
            recovered.segments[0].upload.as_ref().unwrap().object_id,
            "object-journal"
        );
        compact_receipt_journal(&path, &recovered).unwrap();
        assert!(!path.parent().unwrap().join(RECEIPT_JOURNAL_FILE).exists());
        let persisted: TranscriptManifest =
            serde_json::from_reader(File::open(&path).unwrap()).unwrap();
        assert_eq!(
            persisted.segments[0].upload.as_ref().unwrap().segment_id,
            "segment-journal"
        );
    }

    #[test]
    fn wire_capability_cache_is_endpoint_scoped_and_removable() {
        let temp = tempdir().unwrap();
        cache_wire_encoding(temp.path(), "https://platform.example/", WireEncoding::Gzip).unwrap();
        assert_eq!(
            cached_wire_encoding(temp.path(), "https://platform.example"),
            Some(WireEncoding::Gzip)
        );
        assert_eq!(
            cached_wire_encoding(temp.path(), "https://other.example"),
            None
        );
        clear_wire_encoding_cache(temp.path()).unwrap();
        assert_eq!(
            cached_wire_encoding(temp.path(), "https://platform.example"),
            None
        );
    }

    #[test]
    fn background_upload_schedule_coalesces_duplicate_notifications() {
        let temp = tempdir().unwrap();
        assert!(claim_upload_schedule(temp.path()).unwrap());
        assert!(!claim_upload_schedule(temp.path()).unwrap());
        remove_upload_schedule(temp.path());
        assert!(claim_upload_schedule(temp.path()).unwrap());
        remove_upload_schedule(temp.path());
    }

    #[test]
    fn push_persists_receipt_and_skips_it_on_retry() {
        let temp = tempdir().unwrap();
        let input = temp.path().join("maestro.jsonl");
        fs::write(&input, include_str!("../tests/fixtures/maestro.jsonl")).unwrap();
        let state = temp.path().join("state");
        let prepared = prepare_transcript(
            PrepareTranscriptArgs {
                input,
                agent: TranscriptAgent::Maestro,
                source_session_id: "maestro-quality-fixture".to_string(),
                session_id: Some("maestro-session-1".to_string()),
                organization: "org-1".to_string(),
                workspace: "workspace-1".to_string(),
                repository_url: None,
                working_directory: Some(temp.path().display().to_string()),
                branch: None,
                head_sha: None,
                title: None,
                completeness: TranscriptCompletenessArg::Complete,
            },
            Some(&state),
        )
        .unwrap();
        let manifest_path = PathBuf::from(prepared["manifest"].as_str().unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut received = Vec::new();
            let mut buffer = [0_u8; 4096];
            let header_end;
            loop {
                let read = stream.read(&mut buffer).unwrap();
                assert!(read > 0);
                received.extend_from_slice(&buffer[..read]);
                if let Some(position) = received.windows(4).position(|window| window == b"\r\n\r\n")
                {
                    header_end = position + 4;
                    break;
                }
            }
            let headers = String::from_utf8(received[..header_end].to_vec()).unwrap();
            let lower_headers = headers.to_ascii_lowercase();
            assert!(lower_headers.contains("content-type: application/proto"));
            assert!(lower_headers.contains("x-organization-id: org-1"));
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|value| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            while received.len() < header_end + content_length {
                let read = stream.read(&mut buffer).unwrap();
                assert!(read > 0);
                received.extend_from_slice(&buffer[..read]);
            }
            let request = sessions_pb::RecordTranscriptSegmentRequest::decode(
                &received[header_end..header_end + content_length],
            )
            .unwrap();
            assert!(request.edge_redacted);
            assert!(request.session.as_ref().unwrap().started_at.is_some());
            assert_eq!(request.last_entry_index, 4);
            let captured = String::from_utf8(request.content.clone()).unwrap();
            assert!(captured.contains("toolCallId"));
            assert!(captured.contains("usage"));
            assert_eq!(
                request.session.as_ref().unwrap().session_id,
                "maestro-session-1"
            );
            let response = sessions_pb::RecordTranscriptSegmentResponse {
                segment: Some(sessions_pb::TranscriptSegment {
                    segment_id: "segment-1".to_string(),
                    organization_id: request.organization_id,
                    workspace_id: request.workspace_id,
                    session_id: "maestro-session-1".to_string(),
                    segment_index: request.segment_index,
                    first_entry_index: request.first_entry_index,
                    last_entry_index: request.last_entry_index,
                    object_id: "object-1".to_string(),
                    version_id: "version-1".to_string(),
                    content_type: "application/x-ndjson".to_string(),
                    size_bytes: request.content.len() as i64,
                    sha256: request.sha256,
                    recorded_at: "2026-08-24T00:00:00Z".to_string(),
                    redaction_policy_version: request.redaction_policy_version,
                    ..Default::default()
                }),
                ..Default::default()
            }
            .encode_to_vec();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/proto\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response.len()
            )
            .unwrap();
            stream.write_all(&response).unwrap();
        });

        let first = push_transcript(PushTranscriptArgs {
            manifest: manifest_path.clone(),
            endpoint: format!("http://{address}"),
            token: None,
        })
        .unwrap();
        server.join().unwrap();
        assert_eq!(first["uploaded"], 1);
        let manifest: TranscriptManifest =
            serde_json::from_reader(File::open(&manifest_path).unwrap()).unwrap();
        assert_eq!(
            manifest.segments[0].upload.as_ref().unwrap().object_id,
            "object-1"
        );

        let retry = push_transcript(PushTranscriptArgs {
            manifest: manifest_path,
            endpoint: format!("http://{address}"),
            token: None,
        })
        .unwrap();
        assert_eq!(retry["already_receipted"], 1);
        assert_eq!(retry["uploaded"], 0);
        assert_eq!(retry["complete"], true);
    }
}

#[cfg(test)]
mod benchmark;

#[cfg(test)]
mod perf_tests;
