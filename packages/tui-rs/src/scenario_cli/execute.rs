//! `maestro scenario run --execute`: drive a scripted scenario through the
//! real agent loop.
//!
//! The offline `scenario run` path validates a scripted scenario without
//! executing anything. This module instead injects a
//! [`maestro_ai::ScriptedClient`] into [`crate::agent::NativeAgent`], so the
//! scenario's recorded assistant frames become model responses and the
//! runtime executes their tool calls for real -- real file reads/writes in a
//! hydrated workspace, real session JSONL (inspectable via
//! `maestro run inspect`), real tool-execution receipts -- under auto
//! approval with an optional tool allowlist from the scenario metadata.
//!
//! Determinism: the scripted provider answers from the script alone, so the
//! conversation content is fully determined by the scenario. The
//! [`ScenarioExecution::transcript_sha256`] hash covers that content with
//! the absolute workspace path normalized out, so two executions of the same
//! scenario hash identically even though session ids and timestamps differ.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use maestro_ai::{ScriptedBlock, ScriptedClient, ScriptedResponse, StopReason, UnifiedClient};

use super::{ScriptedScenario, load_workspace_manifest};
use crate::agent::{FromAgent, NativeAgent, NativeAgentConfig};
use crate::session::{
    AppMessage, ContentBlock as SessionContentBlock, CustomEntry, MessageContent, MessageEntry,
    SessionEntry, SessionHeader, SessionWriter, ToolInfo, generate_session_filename,
    sanitize_path_for_dirname, sessions_dir,
};
use crate::state::ApprovalMode;

/// Provider/model id advertised for scripted execution. The provider prefix
/// maps to `AiProvider::Scripted` for labels; the client itself is injected
/// directly and never resolved through the provider registry.
pub const SCRIPTED_PROVIDER: &str = "scripted-replay";
pub const SCRIPTED_MODEL: &str = "maestro-replay-v1";
pub const SCRIPTED_MODEL_ID: &str = "scripted-replay/maestro-replay-v1";

/// Bound on one scripted execution. A well-formed scenario finishes in
/// seconds; this only guards against a runtime wedge hanging the CLI.
const EXECUTION_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(2);

/// One tool call the runtime really executed, in execution order.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutedToolCall {
    pub call_id: String,
    pub tool: String,
    pub args: Value,
    pub success: bool,
}

/// Outcome of one `--execute` run.
#[derive(Debug)]
pub struct ScenarioExecution {
    pub session_id: String,
    pub session_path: PathBuf,
    pub workspace: PathBuf,
    pub tool_executions: Vec<ExecutedToolCall>,
    pub final_text: String,
    pub transcript_sha256: String,
    /// Holds the hydrated workspace alive for hydrated scenarios; `None`
    /// when the scenario ran directly against its own directory.
    _workspace_guard: Option<tempfile::TempDir>,
}

/// Serializable execution evidence embedded in the scenario result JSON.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionSummary {
    pub mode: String,
    pub provider: String,
    pub model: String,
    pub approval_mode: String,
    pub deterministic: bool,
    pub external_credentials_required: bool,
    pub external_network_required: bool,
    pub session_id: String,
    pub session_path: String,
    pub workspace: String,
    pub tool_executions: Vec<ExecutedToolCall>,
    pub final_text: String,
    pub transcript_sha256: String,
}

impl ScenarioExecution {
    #[must_use]
    pub fn summary(&self) -> ExecutionSummary {
        ExecutionSummary {
            mode: "agent-loop".to_string(),
            provider: SCRIPTED_PROVIDER.to_string(),
            model: SCRIPTED_MODEL.to_string(),
            approval_mode: "auto".to_string(),
            deterministic: true,
            external_credentials_required: false,
            external_network_required: false,
            session_id: self.session_id.clone(),
            session_path: self.session_path.display().to_string(),
            workspace: self.workspace.display().to_string(),
            tool_executions: self.tool_executions.clone(),
            final_text: self.final_text.clone(),
            transcript_sha256: self.transcript_sha256.clone(),
        }
    }
}

fn system_time_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Convert scenario frames into scripted provider responses. Each frame maps
/// to one assistant response; `text` statements become text blocks and
/// `tool_call` statements become tool-use blocks. `end` statements only mark
/// the recording boundary -- the final frame's `EndTurn` stop reason
/// terminates the loop instead.
fn scenario_to_scripted_responses(scenario: &ScriptedScenario) -> Result<Vec<ScriptedResponse>> {
    let mut responses = Vec::new();
    for frame in &scenario.frames {
        let mut blocks = Vec::new();
        for statement in &frame.statements {
            match statement.get("kind").and_then(Value::as_str) {
                Some("text") => {
                    let text = statement
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    if !text.is_empty() {
                        blocks.push(ScriptedBlock::Text(text));
                    }
                }
                Some("tool_call") => {
                    let name = statement
                        .get("tool")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    if name.is_empty() {
                        bail!(
                            "scripted scenario {} frame {} has a tool_call without a tool name",
                            scenario.id,
                            frame.index,
                        );
                    }
                    let id = statement
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("call-{}-{}-{name}", frame.index, blocks.len()));
                    let input = statement
                        .get("input")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({}));
                    blocks.push(ScriptedBlock::ToolUse { id, name, input });
                }
                Some("end") => {}
                other => {
                    bail!(
                        "scripted scenario {} frame {} has unsupported statement kind {other:?} for --execute",
                        scenario.id,
                        frame.index,
                    );
                }
            }
        }
        let has_tool_use = blocks
            .iter()
            .any(|block| matches!(block, ScriptedBlock::ToolUse { .. }));
        responses.push(ScriptedResponse {
            blocks,
            stop_reason: if has_tool_use {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            },
            error: None,
        });
    }
    if responses.is_empty() {
        bail!("scripted scenario {} has no frames to execute", scenario.id);
    }
    Ok(responses)
}

/// Hydrate the workspace for execution. With a workspace manifest that lists
/// files, copy those files (from the manifest hydration root) into a fresh
/// temp dir so write tool calls land in a disposable workspace. Without one,
/// execute against the scenario's own directory.
fn hydrate_workspace(
    scenario: &ScriptedScenario,
    base_dir: &Path,
) -> Result<(PathBuf, Option<tempfile::TempDir>)> {
    let Some(manifest_path) = scenario.workspace_manifest_path.as_ref() else {
        return Ok((dunce::canonicalize(base_dir)?, None));
    };
    let manifest = load_workspace_manifest(&base_dir.join(manifest_path))?;
    if manifest.files.is_empty() {
        return Ok((dunce::canonicalize(base_dir)?, None));
    }
    let hydration_root = manifest
        .hydration
        .root_path
        .as_ref()
        .map(|root| base_dir.join(root))
        .unwrap_or_else(|| base_dir.to_path_buf());
    let temp = tempfile::tempdir().context("create scenario execution workspace")?;
    for file in &manifest.files {
        let source = hydration_root.join(&file.path);
        let target = temp.path().join(&file.path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create workspace dir {}", parent.display()))?;
        }
        std::fs::copy(&source, &target).with_context(|| {
            format!(
                "hydrate workspace file {} from {}",
                file.path,
                source.display()
            )
        })?;
    }
    let workspace = dunce::canonicalize(temp.path())?;
    Ok((workspace, Some(temp)))
}

fn write_session_entry(writer: &mut SessionWriter, entry: SessionEntry) -> Result<()> {
    writer
        .write_entry(entry)
        .map_err(|err| anyhow::anyhow!("write scenario execution session entry: {err}"))
}

/// Replace the absolute workspace path so the transcript hash is stable no
/// matter which temp dir the workspace was hydrated into.
fn normalize_for_transcript(text: &str, workspace: &Path) -> String {
    text.replace(&workspace.display().to_string(), "<workspace>")
}

fn normalize_json_for_transcript(value: &Value, workspace: &Path) -> Value {
    match value {
        Value::String(text) => Value::String(normalize_for_transcript(text, workspace)),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| normalize_json_for_transcript(item, workspace))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), normalize_json_for_transcript(item, workspace)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Optional overrides for [`execute_scripted_scenario`].
#[derive(Debug, Default, Clone)]
pub struct ExecuteOptions {
    /// Session-store home override. Tests use this to keep the recorded
    /// session inside a temp dir instead of mutating the process-wide `HOME`
    /// env var (which races other tests that read it).
    pub session_home: Option<PathBuf>,
}

fn session_dir_for(cwd: &str, session_home: Option<&Path>) -> PathBuf {
    match session_home {
        Some(home) => home
            .join(".composer")
            .join("agent")
            .join("sessions")
            .join(format!("--{}--", sanitize_path_for_dirname(cwd))),
        None => sessions_dir(cwd),
    }
}

/// Run the scenario through the real agent loop and return the execution
/// evidence. The session JSONL is written to the standard session store
/// (`~/.composer/agent/sessions/--<workspace>--/`), so
/// `maestro run inspect <session-id> --json` reconstructs it afterwards.
pub async fn execute_scripted_scenario(
    scenario: &ScriptedScenario,
    base_dir: &Path,
) -> Result<ScenarioExecution> {
    execute_scripted_scenario_with_options(scenario, base_dir, &ExecuteOptions::default()).await
}

/// [`execute_scripted_scenario`] with explicit [`ExecuteOptions`].
pub async fn execute_scripted_scenario_with_options(
    scenario: &ScriptedScenario,
    base_dir: &Path,
    options: &ExecuteOptions,
) -> Result<ScenarioExecution> {
    let responses = scenario_to_scripted_responses(scenario)?;
    let (workspace, workspace_guard) = hydrate_workspace(scenario, base_dir)?;
    let cwd = workspace.display().to_string();

    let client = UnifiedClient::Scripted(ScriptedClient::new(SCRIPTED_MODEL_ID, responses));
    let config = NativeAgentConfig {
        model: SCRIPTED_MODEL_ID.to_string(),
        cwd: cwd.clone(),
        // `--execute` is the non-interactive "approval-mode auto" shape: the
        // runner auto-approves and executes every call itself, inline.
        approval_mode: ApprovalMode::Yolo,
        ..NativeAgentConfig::default()
    };
    let allowed_tools: HashSet<String> = scenario
        .metadata
        .tools_expected
        .iter()
        .map(|tool| tool.to_lowercase())
        .collect();
    let (agent, mut events) = if allowed_tools.is_empty() {
        NativeAgent::new_with_client(
            std::sync::Arc::new(maestro_local_host::agent::tool_host::BuiltInToolHost),
            config,
            client,
        )
    } else {
        NativeAgent::new_with_client_and_allowed_tools(
            std::sync::Arc::new(maestro_local_host::agent::tool_host::BuiltInToolHost),
            config,
            &allowed_tools,
            client,
        )
    }
    .context("create scripted-replay agent")?;

    // Real session JSONL in the standard store.
    let session_id = uuid::Uuid::new_v4().to_string();
    let session_dir = session_dir_for(&cwd, options.session_home.as_deref());
    std::fs::create_dir_all(&session_dir)
        .with_context(|| format!("create session dir {}", session_dir.display()))?;
    let session_path = session_dir.join(generate_session_filename(&session_id));
    let unified_context_manifest =
        crate::context_cli::load_unified_context_manifest_json(&workspace)
            .context("capture scenario context manifest")?;
    let header = SessionHeader {
        version: Some(2),
        id: session_id.clone(),
        timestamp: now_rfc3339(),
        cwd: cwd.clone(),
        model: SCRIPTED_MODEL_ID.to_string(),
        subject: Some(format!("scenario run --execute {}", scenario.id)),
        model_metadata: None,
        thinking_level: Default::default(),
        system_prompt: None,
        prompt_metadata: None,
        prompt_context_manifest: None,
        unified_context_manifest: Some(Box::new(unified_context_manifest)),
        tools: allowed_tools
            .iter()
            .map(|name| ToolInfo {
                name: name.clone(),
                label: None,
                description: None,
            })
            .collect(),
        branched_from: None,
        parent_session: None,
    };
    let mut writer = SessionWriter::create(&session_path, header)
        .map_err(|err| anyhow::anyhow!("create scenario execution session: {err}"))?;

    let prompt_text = format!("Replay the scripted scenario {}.", scenario.id);
    write_session_entry(
        &mut writer,
        SessionEntry::Message(MessageEntry {
            id: None,
            parent_id: None,
            timestamp: now_rfc3339(),
            message: AppMessage::User {
                content: MessageContent::Text(prompt_text.clone()),
                attachments: None,
                timestamp: system_time_millis(),
            },
        }),
    )?;
    // Tag the session as a scenario replay (mirrors the documented
    // `scenario_replay` marker from the TypeScript CLI era).
    write_session_entry(
        &mut writer,
        SessionEntry::Custom(CustomEntry {
            id: Some(uuid::Uuid::new_v4().to_string()),
            parent_id: None,
            timestamp: now_rfc3339(),
            custom_type: "scenario_replay".to_string(),
            data: Some(serde_json::json!({
                "replay": true,
                "execute": true,
                "scenarioId": scenario.id,
            })),
        }),
    )?;

    agent.send_ready();
    agent
        .prompt(prompt_text.clone(), vec![])
        .await
        .context("send scripted scenario prompt")?;

    let mut transcript: Vec<Value> = vec![serde_json::json!({
        "role": "user",
        "text": prompt_text,
    })];
    let mut current_text = String::new();
    let mut current_tool_calls: Vec<(String, String, Value)> = Vec::new();
    let mut tool_executions: Vec<ExecutedToolCall> = Vec::new();
    let mut final_text = String::new();

    let drive = async {
        while let Some(event) = events.recv().await {
            match event {
                FromAgent::ResponseChunk {
                    content,
                    is_thinking: false,
                    ..
                } => {
                    current_text.push_str(&content);
                }
                FromAgent::ToolCall {
                    call_id,
                    tool,
                    args,
                    ..
                } => {
                    current_tool_calls.push((call_id, tool, args));
                }
                FromAgent::ToolEnd {
                    call_id,
                    success,
                    result,
                    receipt,
                } => {
                    let tool = current_tool_calls
                        .iter()
                        .find(|(id, _, _)| *id == call_id)
                        .map(|(_, tool, _)| tool.clone())
                        .unwrap_or_else(|| "unknown".to_string());
                    let args = current_tool_calls
                        .iter()
                        .find(|(id, _, _)| *id == call_id)
                        .map(|(_, _, args)| args.clone())
                        .unwrap_or_else(|| serde_json::json!({}));
                    let output = result
                        .as_ref()
                        .map(|tool_result| {
                            if tool_result.success {
                                tool_result.output.clone()
                            } else {
                                tool_result
                                    .error
                                    .clone()
                                    .unwrap_or_else(|| tool_result.output.clone())
                            }
                        })
                        .unwrap_or_default();
                    write_session_entry(
                        &mut writer,
                        SessionEntry::Message(MessageEntry {
                            id: None,
                            parent_id: None,
                            timestamp: now_rfc3339(),
                            message: AppMessage::ToolResult {
                                tool_call_id: call_id.clone(),
                                tool_name: tool.clone(),
                                content: output.clone(),
                                details: result
                                    .as_ref()
                                    .and_then(|tool_result| tool_result.details.clone()),
                                receipt,
                                is_error: !success,
                                timestamp: system_time_millis(),
                            },
                        }),
                    )?;
                    tool_executions.push(ExecutedToolCall {
                        call_id: call_id.clone(),
                        tool,
                        success,
                        args: args.clone(),
                    });
                    transcript.push(serde_json::json!({
                        "role": "toolResult",
                        "callId": call_id,
                        "success": success,
                        "output": normalize_for_transcript(&output, &workspace),
                    }));
                }
                FromAgent::ResponseEnd { .. }
                    if !current_text.is_empty() || !current_tool_calls.is_empty() =>
                {
                    let mut blocks = Vec::new();
                    if !current_text.is_empty() {
                        blocks.push(SessionContentBlock::Text {
                            text: current_text.clone(),
                        });
                    }
                    for (call_id, tool, args) in &current_tool_calls {
                        blocks.push(SessionContentBlock::ToolCall {
                            id: call_id.clone(),
                            name: tool.clone(),
                            args: args.clone(),
                            contract: None,
                            gemini_context: None,
                        });
                    }
                    write_session_entry(
                        &mut writer,
                        SessionEntry::Message(MessageEntry {
                            id: None,
                            parent_id: None,
                            timestamp: now_rfc3339(),
                            message: AppMessage::Assistant {
                                content: blocks,
                                api: Some(SCRIPTED_PROVIDER.to_string()),
                                provider: Some(SCRIPTED_PROVIDER.to_string()),
                                model: Some(SCRIPTED_MODEL_ID.to_string()),
                                usage: None,
                                stop_reason: None,
                                timestamp: system_time_millis(),
                            },
                        }),
                    )?;
                    transcript.push(serde_json::json!({
                        "role": "assistant",
                        "text": current_text,
                        "toolCalls": current_tool_calls
                            .iter()
                            .map(|(call_id, tool, args)| serde_json::json!({
                                "id": call_id,
                                "name": tool,
                                "args": normalize_json_for_transcript(args, &workspace),
                            }))
                            .collect::<Vec<_>>(),
                    }));
                    if !current_text.is_empty() {
                        final_text = current_text.clone();
                    }
                    current_text = String::new();
                    current_tool_calls = Vec::new();
                }
                FromAgent::ResponseEnd { .. } => {}
                FromAgent::TurnCompleted { .. } => {
                    if !current_text.is_empty() {
                        final_text = current_text.clone();
                    }
                    break;
                }
                FromAgent::TurnInterrupted { reason, .. } => {
                    bail!("scripted scenario execution interrupted: {reason}")
                }
                FromAgent::ProviderError { kind, message } => {
                    bail!("scripted scenario provider failure ({kind:?}): {message}")
                }
                FromAgent::Error {
                    message,
                    fatal,
                    terminal,
                    ..
                } if fatal || terminal => bail!("scripted scenario execution failed: {message}"),
                _ => {}
            }
        }
        Ok::<(), anyhow::Error>(())
    };

    match tokio::time::timeout(EXECUTION_TIMEOUT, drive).await {
        Ok(result) => result?,
        Err(_elapsed) => {
            agent.cancel();
            bail!(
                "scripted scenario {} execution timed out after {}s",
                scenario.id,
                EXECUTION_TIMEOUT.as_secs()
            );
        }
    }

    writer
        .flush()
        .map_err(|err| anyhow::anyhow!("flush scenario execution session: {err}"))?;

    let transcript_json = serde_json::to_string(&transcript)?;
    let transcript_sha256 = format!("{:x}", Sha256::digest(transcript_json.as_bytes()));

    Ok(ScenarioExecution {
        session_id,
        session_path,
        workspace,
        tool_executions,
        final_text,
        transcript_sha256,
        _workspace_guard: workspace_guard,
    })
}
