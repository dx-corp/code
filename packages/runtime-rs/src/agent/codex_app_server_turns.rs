//! Codex app-server turn transport for `openai-codex/*` models.
//!
//! Routes turns through `CodexAppServerClient` (`thread/start`, `turn/start`)
//! so ChatGPT OAuth refresh stays owned by Codex. Dynamic tools and approval
//! server-requests are queued for the native agent to service.

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::env;
use std::path::Path;
use std::time::Duration;

use super::native_host::NativeCodexAuth;
use crate::codex_app_server::{
    CodexAppServerClient, IncomingServerRequest, InitializeOptions, JsonRpcError, Notification,
    ServerRequestWaitError, ThreadInjectItemsParams, ThreadStartParams, TurnStartParams,
    agent_message_completed_text, agent_message_text_from_notifications,
    is_agent_message_notification,
};
use crate::codex_session::{
    CodexCapabilities, CodexSessionKey, CodexSessionManifest, CodexSessionOpen, CodexThreadBinding,
};
use maestro_ai::{ContentBlock, Message, MessageContent, Role};

/// Result of a single text turn over Codex app-server.
#[derive(Debug, Clone)]
pub struct CodexAppServerTurnResult {
    pub thread_id: String,
    pub turn_id: String,
    pub assistant_text: String,
    /// True when `assistant_text` is an authoritative full response rather
    /// than the unconsumed suffix assembled from delta notifications.
    ///
    /// The current adapter emits suffixes. This explicit mode keeps native
    /// chronology reconciliation compatible with app-server versions that
    /// surface full text through `item/completed`.
    pub assistant_text_is_full: bool,
    pub raw_completion: Value,
    /// Provider-facing failure from `turn/completed` (`turn.status = failed`
    /// and/or `turn.error`) or an `error` notification. Empty assistant text
    /// with this set is a failed turn, not a transient empty completion.
    pub failure: Option<String>,
}

impl CodexAppServerTurnResult {
    /// Non-empty provider failure text, if Codex declared the turn failed.
    pub fn provider_failure(&self) -> Option<&str> {
        self.failure
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }
}

/// One dynamic tool exposed to Codex app-server.
#[derive(Debug, Clone)]
pub struct DynamicToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Session wrapper that owns one app-server process and one thread.
pub struct CodexAppServerTurnSession {
    client: CodexAppServerClient,
    thread_id: String,
    model: String,
    open_kind: CodexSessionOpen,
    profile: String,
    compatibility: CodexCompatibilityReport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexPersistentThreadOpen {
    pub thread_id: String,
    pub open_kind: CodexSessionOpen,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexCompatibilityReport {
    pub protocol_version: String,
    pub resume: bool,
    pub steering: bool,
    pub missing_required: Vec<String>,
}

pub struct CodexThreadPayload<'a> {
    pub dynamic_tools: &'a [DynamicToolSpec],
    pub instructions: Option<String>,
    pub restored_messages: &'a [Message],
}

impl CodexCompatibilityReport {
    pub fn is_ready(&self) -> bool {
        self.missing_required.is_empty()
    }

    fn legacy_unknown() -> Self {
        Self {
            protocol_version: "unknown".to_owned(),
            resume: false,
            steering: false,
            missing_required: Vec::new(),
        }
    }
}

// Wire contract reviewed against openai/codex commit
// 459a79eb85400af759e9220c7bafb4429ae07516, protocol/v2/{model,turn}.rs.
#[derive(Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexReasoningModels {
    data: Vec<CodexReasoningModel>,
    next_cursor: Option<String>,
}

#[derive(Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexReasoningModel {
    id: String,
    model: String,
    default_reasoning_effort: String,
    supported_reasoning_efforts: Vec<CodexReasoningOption>,
}

#[derive(Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexReasoningOption {
    reasoning_effort: String,
}

impl CodexReasoningModel {
    fn select_effort(&self, enabled: bool, budget: u32) -> Result<String> {
        // These boundaries match ThinkingLevel::to_config. Arbitrary budgets
        // round up to the next exposed level; Max uses advertised capabilities.
        let requested = if !enabled {
            self.default_reasoning_effort.as_str()
        } else if budget <= 1_024 {
            "minimal"
        } else if budget <= 4_096 {
            "low"
        } else if budget <= 10_000 {
            "medium"
        } else if budget <= 20_000 {
            "high"
        } else if budget <= 32_000 {
            // ThinkingLevel::XHigh. Prefer the matching level and only step up
            // when the model does not advertise it; without this band XHigh and
            // Max both resolved to the model's top effort.
            [
                "xhigh", "ultra", "max", "high", "medium", "low", "minimal", "none",
            ]
            .into_iter()
            .find(|effort| self.supports(effort))
            .context("Codex model has no supported bounded reasoning effort")?
        } else {
            [
                "ultra", "max", "xhigh", "high", "medium", "low", "minimal", "none",
            ]
            .into_iter()
            .find(|effort| self.supports(effort))
            .context("Codex model has no supported bounded reasoning effort")?
        };
        if requested.is_empty() || !self.supports(requested) {
            bail!(
                "Codex model {} does not support reasoning effort {requested}",
                self.model
            );
        }
        Ok(requested.to_owned())
    }

    fn supports(&self, effort: &str) -> bool {
        self.supported_reasoning_efforts
            .iter()
            .any(|option| option.reasoning_effort == effort)
    }
}

impl CodexAppServerTurnSession {
    /// Spawn app-server, initialize, and start a thread for `model`.
    ///
    /// `instructions` is the Maestro system prompt / prompt context (when
    /// present). Passed as `developerInstructions` on `thread/start` so Codex
    /// receives the same standing instructions the HTTP path embeds.
    ///
    /// This compatibility entry point intentionally fails closed. Auth and
    /// state roots belong to the composing host; use
    /// [`Self::connect_with_auth`] after resolving them there.
    pub async fn connect(
        _model: impl Into<String>,
        _cwd: Option<String>,
        _approval_policy: Option<String>,
        _sandbox: Option<String>,
        _dynamic_tools: &[DynamicToolSpec],
        _instructions: Option<String>,
        _restored_messages: &[Message],
    ) -> Result<Self> {
        bail!("Codex app-server auth must be resolved by the composing host")
    }

    /// Spawn app-server with an auth/profile decision supplied by the host.
    pub async fn connect_with_auth(
        model: impl Into<String>,
        cwd: Option<String>,
        approval_policy: Option<String>,
        sandbox: Option<String>,
        payload: CodexThreadPayload<'_>,
        auth: &NativeCodexAuth,
    ) -> Result<Self> {
        let CodexThreadPayload {
            dynamic_tools,
            instructions,
            restored_messages,
        } = payload;
        let model = model.into();
        let (command, args) = codex_app_server_spawn_override_from_env()?;
        let client = CodexAppServerClient::spawn_with_env(command, args, None, &auth.child_env)
            .await
            .context("spawn Codex app-server")?;
        let initialized = initialize_client(&client).await?;
        let compatibility = codex_compatibility_from_initialize(&initialized);
        ensure_codex_compatibility_ready(&compatibility)?;
        Self::start_fresh_thread(
            client,
            ThreadStartParams {
                model,
                cwd,
                approval_policy,
                sandbox,
                extra: thread_start_extra(dynamic_tools, instructions.clone()),
            },
            CodexThreadPayload {
                dynamic_tools,
                instructions,
                restored_messages,
            },
            None,
            auth.profile_name.clone(),
            compatibility,
        )
        .await
    }

    /// This compatibility entry point intentionally fails closed. Auth and
    /// persistence roots belong to the composing host; use
    /// [`Self::connect_persistent_with_auth`] after resolving them there.
    pub async fn connect_persistent(
        _model: impl Into<String>,
        _cwd: Option<String>,
        _approval_policy: Option<String>,
        _sandbox: Option<String>,
        _session_id: Option<&str>,
        _payload: CodexThreadPayload<'_>,
        _state_root: &Path,
    ) -> Result<Self> {
        bail!("Codex app-server auth must be resolved by the composing host")
    }

    /// Open or resume the persistent app-server thread using host-resolved
    /// auth and persistence roots.
    pub async fn connect_persistent_with_auth(
        model: impl Into<String>,
        cwd: Option<String>,
        approval_policy: Option<String>,
        sandbox: Option<String>,
        session_id: Option<&str>,
        payload: CodexThreadPayload<'_>,
        auth: &NativeCodexAuth,
    ) -> Result<Self> {
        let model = model.into();
        let (command, args) = codex_app_server_spawn_override_from_env()?;
        let workspace = cwd
            .as_deref()
            .map(std::path::Path::new)
            .unwrap_or_else(|| std::path::Path::new("."));
        let key = CodexSessionKey::new(&auth.profile_name, workspace, &model)?
            .with_session_id(session_id);
        let manifest = CodexSessionManifest {
            key,
            approval_policy: approval_policy.clone().unwrap_or_default(),
            sandbox: sandbox.clone().unwrap_or_default(),
            capabilities: CodexCapabilities::default(),
        };
        let client = CodexAppServerClient::spawn_with_env(command, args, None, &auth.child_env)
            .await
            .context("spawn Codex app-server")?;
        Self::connect_with_client_and_manifest(
            client,
            manifest,
            &auth.state_root,
            payload.dynamic_tools,
            payload.instructions,
            payload.restored_messages,
        )
        .await
    }

    pub async fn connect_with_client_and_manifest(
        client: CodexAppServerClient,
        manifest: CodexSessionManifest,
        state_root: &Path,
        dynamic_tools: &[DynamicToolSpec],
        instructions: Option<String>,
        restored_messages: &[Message],
    ) -> Result<Self> {
        let initialized = initialize_client(&client).await?;
        let compatibility = codex_compatibility_from_initialize(&initialized);
        ensure_codex_compatibility_ready(&compatibility)?;
        let open = open_persistent_thread(
            &client,
            &manifest,
            state_root,
            CodexThreadPayload {
                dynamic_tools,
                instructions,
                restored_messages,
            },
            &initialized,
            &compatibility,
        )
        .await?;
        let thread_id = open.thread_id;
        let open_kind = open.open_kind;
        let profile = manifest.key.profile.clone();
        Ok(Self {
            client,
            thread_id,
            model: manifest.key.model,
            open_kind,
            profile,
            compatibility,
        })
    }

    async fn start_fresh_thread(
        client: CodexAppServerClient,
        params: ThreadStartParams,
        payload: CodexThreadPayload<'_>,
        binding_store: Option<(&Path, CodexSessionManifest, Value)>,
        profile: String,
        compatibility: CodexCompatibilityReport,
    ) -> Result<Self> {
        let model = params.model.clone();
        let thread = client
            .start_thread(params, None)
            .await
            .context("thread/start")?;
        let session = Self::from_started_thread_with_metadata(
            client,
            thread.thread_id,
            model,
            payload.restored_messages,
            profile,
            compatibility,
        )
        .await?;
        if let Some((state_root, manifest, initialized)) = binding_store {
            CodexThreadBinding::fresh(
                manifest.key,
                session.thread_id.clone(),
                initialized
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            )
            .store_at(state_root)?;
        }
        Ok(session)
    }

    #[cfg(test)]
    async fn from_started_thread(
        client: CodexAppServerClient,
        thread_id: String,
        model: String,
        restored_messages: &[Message],
    ) -> Result<Self> {
        Self::from_started_thread_with_metadata(
            client,
            thread_id,
            model,
            restored_messages,
            String::new(),
            CodexCompatibilityReport::legacy_unknown(),
        )
        .await
    }

    async fn from_started_thread_with_metadata(
        client: CodexAppServerClient,
        thread_id: String,
        model: String,
        restored_messages: &[Message],
        profile: String,
        compatibility: CodexCompatibilityReport,
    ) -> Result<Self> {
        let restored_items = semantic_messages_to_codex_items(restored_messages);
        if !restored_items.is_empty() {
            client
                .inject_thread_items(
                    ThreadInjectItemsParams {
                        thread_id: thread_id.clone(),
                        items: Value::Array(restored_items),
                    },
                    None,
                )
                .await
                .context("thread/inject_items")?;
        }

        Ok(Self {
            client,
            thread_id,
            model,
            open_kind: CodexSessionOpen::Created,
            profile,
            compatibility,
        })
    }

    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn open_kind(&self) -> CodexSessionOpen {
        self.open_kind
    }

    pub fn profile(&self) -> &str {
        &self.profile
    }

    pub fn compatibility(&self) -> &CodexCompatibilityReport {
        &self.compatibility
    }

    pub fn client(&self) -> &CodexAppServerClient {
        &self.client
    }

    /// Start a user text turn (returns as soon as `turn/start` succeeds).
    pub async fn start_text_turn(
        &self,
        text: impl Into<String>,
        timeout_ms: Option<u64>,
    ) -> Result<String> {
        let turn = self
            .client
            .start_turn(TurnStartParams::text(&self.thread_id, text), timeout_ms)
            .await
            .context("turn/start")?;
        Ok(turn.turn_id)
    }

    /// Start a turn with Maestro's current thinking setting. Codex persists
    /// effort overrides across turns, so even Off explicitly restores the
    /// model default instead of inheriting a previous turn's override.
    pub async fn start_text_turn_with_thinking(
        &self,
        text: impl Into<String>,
        enabled: bool,
        budget: u32,
        timeout_ms: Option<u64>,
    ) -> Result<String> {
        let effort = self
            .reasoning_model(timeout_ms)
            .await?
            .select_effort(enabled, budget)?;
        let mut params = TurnStartParams::text(&self.thread_id, text);
        params.extra = Some(json!({"effort": effort}));
        let turn = self
            .client
            .start_turn(params, timeout_ms)
            .await
            .context("turn/start")?;
        Ok(turn.turn_id)
    }

    async fn reasoning_model(&self, timeout_ms: Option<u64>) -> Result<CodexReasoningModel> {
        let mut cursor: Option<String> = None;
        let mut seen = std::collections::HashSet::new();
        loop {
            let page: CodexReasoningModels = serde_json::from_value(
                self.client
                    .request(
                        "model/list",
                        Some(json!({"includeHidden": true, "cursor": cursor})),
                        timeout_ms,
                    )
                    .await
                    .context("read Codex reasoning capabilities")?,
            )
            .context("invalid Codex reasoning capabilities")?;
            if let Some(model) = page
                .data
                .iter()
                .find(|m| m.model == self.model || m.id == self.model)
            {
                return Ok(model.clone());
            }
            let Some(next) = page.next_cursor else {
                bail!(
                    "Codex model {} is absent from model/list; cannot apply thinking setting",
                    self.model
                );
            };
            if !seen.insert(next.clone()) || seen.len() > 100 {
                bail!("Codex model/list pagination did not converge");
            }
            cursor = Some(next);
        }
    }

    /// Compare the actual advertised values, including the provider default for Off.
    pub async fn is_reasoning_boost(
        &self,
        original: (bool, u32),
        proposed: (bool, u32),
    ) -> Result<bool> {
        let model = self.reasoning_model(None).await?;
        let old = model.select_effort(original.0, original.1)?;
        let new = model.select_effort(proposed.0, proposed.1)?;
        let rank = |effort: &str| {
            [
                "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
            ]
            .iter()
            .position(|value| *value == effort)
        };
        Ok(matches!((rank(&old), rank(&new)), (Some(old), Some(new)) if new > old))
    }

    /// Interrupt an in-flight turn (`turn/interrupt`).
    pub async fn interrupt_turn(&self, turn_id: &str, timeout_ms: Option<u64>) -> Result<()> {
        use crate::codex_app_server::TurnInterruptParams;
        self.client
            .interrupt_turn(
                TurnInterruptParams {
                    thread_id: self.thread_id.clone(),
                    turn_id: turn_id.to_owned(),
                },
                timeout_ms,
            )
            .await
            .context("turn/interrupt")?;
        Ok(())
    }

    /// Steer the active turn with additional user text (`turn/steer`).
    pub async fn steer_text(
        &self,
        expected_turn_id: &str,
        text: impl Into<String>,
        timeout_ms: Option<u64>,
    ) -> Result<String> {
        use crate::codex_app_server::TurnSteerParams;
        let result = self
            .client
            .steer_turn(
                TurnSteerParams::text(&self.thread_id, expected_turn_id, text),
                timeout_ms,
            )
            .await
            .context("turn/steer")?;
        Ok(result.turn_id)
    }

    /// Drain assistant message notifications (streaming deltas + completed
    /// agentMessage items) and return the best text plus whether it is an
    /// authoritative full message.
    async fn take_assistant_text(&self) -> (String, bool) {
        let notes = self
            .client
            .take_notifications_where(is_agent_message_notification)
            .await;
        assistant_text_from_notifications(&notes)
    }

    /// Drain authoritative completed assistant items at a causal boundary.
    ///
    /// Native turns call this before persisting a pre-tool assistant segment,
    /// so the final completion only contains the post-tool segment and cannot
    /// duplicate already-checkpointed text.
    pub async fn take_completed_assistant_text(&self) -> String {
        let notes = self
            .client
            .take_notifications_where(|note| agent_message_completed_text(note).is_some())
            .await;
        completed_assistant_text_from_notifications(&notes)
    }

    /// Wait until the turn completes; returns assistant text collected so far.
    pub async fn wait_turn_complete(
        &self,
        turn_id: &str,
        timeout_ms: Option<u64>,
    ) -> Result<CodexAppServerTurnResult> {
        let completed = self
            .client
            .wait_for_turn_completion(turn_id, timeout_ms)
            .await
            .context("wait for turn completion")?;

        let (assistant_text, assistant_text_is_full) = self.take_assistant_text().await;
        let errors = self.take_error_messages().await;
        let raw_completion = completed.params;
        let failure =
            turn_failure_from_params(&raw_completion).or_else(|| errors.into_iter().next());

        Ok(CodexAppServerTurnResult {
            thread_id: self.thread_id.clone(),
            turn_id: turn_id.to_owned(),
            assistant_text,
            assistant_text_is_full,
            raw_completion,
            failure,
        })
    }

    /// Wait for either a server-request (tool/approval) or turn completion.
    pub async fn wait_server_request_or_turn_complete(
        &self,
        turn_id: &str,
        timeout_ms: Option<u64>,
    ) -> Result<TurnWaitEvent> {
        let wait_ms = timeout_ms.unwrap_or(10 * 60 * 1000);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(wait_ms);

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Ok(TurnWaitEvent::Pending);
            }
            let slice_ms = remaining.min(Duration::from_millis(250)).as_millis() as u64;

            // Prefer server-requests (tool calls / approvals) with a short wait.
            match self.client.wait_for_server_request(Some(slice_ms)).await {
                Ok(request) => return Ok(TurnWaitEvent::ServerRequest(request)),
                Err(ServerRequestWaitError::Timeout) => {}
                Err(ServerRequestWaitError::Closed) => {
                    bail!("Codex app-server client is closed");
                }
            }

            // Check if turn completed notifications arrived during the wait.
            let completed = self
                .client
                .take_notifications_where(|n| {
                    let matches_turn = n
                        .params
                        .as_ref()
                        .and_then(|p| {
                            p.get("turnId")
                                .or_else(|| p.get("turn").and_then(|t| t.get("id")))
                                .or_else(|| p.get("id"))
                        })
                        .and_then(Value::as_str)
                        .map(|id| id == turn_id)
                        .unwrap_or(true);
                    matches_turn
                        && (n.method == "turn/completed"
                            || n.method == "turn/complete"
                            || n.method == "turn/completed/v2"
                            || (n.method == "codex/event"
                                && n.params
                                    .as_ref()
                                    .and_then(|p| p.get("msg"))
                                    .and_then(|m| m.get("type"))
                                    .and_then(Value::as_str)
                                    == Some("turn_complete")))
                })
                .await;
            if let Some(notification) = completed.into_iter().next() {
                let (assistant_text, assistant_text_is_full) = self.take_assistant_text().await;
                let errors = self.take_error_messages().await;
                let raw_completion = notification.params.unwrap_or(Value::Null);
                let failure =
                    turn_failure_from_params(&raw_completion).or_else(|| errors.into_iter().next());
                return Ok(TurnWaitEvent::Completed(CodexAppServerTurnResult {
                    thread_id: self.thread_id.clone(),
                    turn_id: turn_id.to_owned(),
                    assistant_text,
                    assistant_text_is_full,
                    raw_completion,
                    failure,
                }));
            }
        }
    }

    async fn take_error_messages(&self) -> Vec<String> {
        let notes = self
            .client
            .take_notifications_where(|note| note.method == "error")
            .await;
        notes
            .iter()
            .filter_map(error_notification_message)
            .collect()
    }

    /// Drain recent agent message deltas without removing completion events.
    pub async fn take_message_deltas(&self) -> Vec<Notification> {
        self.client
            .take_notifications_where(|n| n.method.starts_with("item/agentMessage"))
            .await
    }

    /// Drain buffered notifications that may carry file-change paths.
    ///
    /// v2 `item/fileChange/requestApproval` often has only `itemId`. Paths
    /// arrive earlier on item notifications; the native runner feeds those
    /// into its correlation map before deciding the approval. Assistant and
    /// tool-call traffic is left in the buffer so the turn loop still sees it.
    pub async fn take_file_change_item_notifications(&self) -> Vec<Notification> {
        self.client
            .take_notifications_where(is_file_change_item_notification)
            .await
    }

    /// Drain completed Codex-native command/file operations.
    ///
    /// Codex executes these operations itself after Maestro answers the
    /// requestApproval RPC. Their authoritative completion arrives as an
    /// `item/completed` notification rather than an `item/tool/call` result,
    /// so the native runner must consume it explicitly to close the public
    /// ToolCall/ToolEnd lifecycle.
    pub async fn take_native_operation_completion_notifications(&self) -> Vec<Notification> {
        self.client
            .take_notifications_where(is_native_operation_completion_notification)
            .await
    }

    /// Drain `turn/usage` notifications for a specific turn.
    pub async fn take_usage_notifications_for_turn(&self, turn_id: &str) -> Vec<Value> {
        self.client
            .take_notifications_where(|n| {
                n.method == "turn/usage" && notification_matches_turn_id(n, turn_id)
            })
            .await
            .into_iter()
            .filter_map(|notification| notification.params)
            .collect()
    }
}

pub async fn open_persistent_thread(
    client: &CodexAppServerClient,
    manifest: &CodexSessionManifest,
    state_root: &Path,
    payload: CodexThreadPayload<'_>,
    initialized: &Value,
    compatibility: &CodexCompatibilityReport,
) -> Result<CodexPersistentThreadOpen> {
    let binding = CodexThreadBinding::load_at(state_root, &manifest.key)?;
    if compatibility.resume {
        if let Some(binding) = binding.as_ref() {
            match client
                .resume_thread(thread_resume_params(manifest, &binding.thread_id), None)
                .await
            {
                Ok(resumed) => {
                    return Ok(CodexPersistentThreadOpen {
                        thread_id: resumed.thread_id,
                        open_kind: CodexSessionOpen::Resumed,
                    });
                }
                Err(error) if is_thread_not_found_error(&error) => {
                    CodexThreadBinding::quarantine_at(state_root, &manifest.key)?;
                }
                Err(error) => {
                    let message = error.to_string();
                    return Err(error).context(format!("thread/resume: {message}"));
                }
            }
        }
    }

    let thread_id = start_and_store_persistent_thread(
        client,
        manifest,
        state_root,
        payload.dynamic_tools,
        payload.instructions,
        payload.restored_messages,
        initialized,
    )
    .await?;
    Ok(CodexPersistentThreadOpen {
        thread_id,
        open_kind: CodexSessionOpen::Created,
    })
}

/// Convert the persisted sandbox field back to the optional protocol value.
///
/// Older manifests represent "no sandbox" as an empty string, while the
/// app-server protocol treats `sandbox` as an enum and rejects that string.
/// `default` and `inherit` are also local sentinel values, not valid Codex
/// `thread/start` variants.
fn persisted_sandbox_value(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty() && !matches!(value, "default" | "inherit")).then(|| value.to_owned())
}

async fn start_and_store_persistent_thread(
    client: &CodexAppServerClient,
    manifest: &CodexSessionManifest,
    state_root: &Path,
    dynamic_tools: &[DynamicToolSpec],
    instructions: Option<String>,
    restored_messages: &[Message],
    initialized: &Value,
) -> Result<String> {
    let thread = client
        .start_thread(
            ThreadStartParams {
                model: manifest.key.model.clone(),
                cwd: Some(manifest.key.workspace.to_string_lossy().to_string()),
                approval_policy: Some(manifest.approval_policy.clone()),
                sandbox: persisted_sandbox_value(&manifest.sandbox),
                extra: thread_start_extra(dynamic_tools, instructions),
            },
            None,
        )
        .await
        .context("thread/start")?;
    let restored_items = semantic_messages_to_codex_items(restored_messages);
    if !restored_items.is_empty() {
        client
            .inject_thread_items(
                ThreadInjectItemsParams {
                    thread_id: thread.thread_id.clone(),
                    items: Value::Array(restored_items),
                },
                None,
            )
            .await
            .context("thread/inject_items")?;
    }
    CodexThreadBinding::fresh(
        manifest.key.clone(),
        thread.thread_id.clone(),
        initialized
            .get("protocolVersion")
            .and_then(Value::as_str)
            .map(str::to_owned),
    )
    .store_at(state_root)?;
    Ok(thread.thread_id)
}

fn thread_resume_params(
    manifest: &CodexSessionManifest,
    thread_id: &str,
) -> crate::codex_app_server::ThreadResumeParams {
    crate::codex_app_server::ThreadResumeParams {
        thread_id: thread_id.to_owned(),
        model: Some(manifest.key.model.clone()),
        cwd: Some(manifest.key.workspace.to_string_lossy().to_string()),
        path: None,
        extra: None,
    }
}

/// True for buffered item notifications that may carry file-change paths.
///
/// Excludes assistant message traffic (including v2 `item/completed`
/// agentMessage items) so `take_completed_assistant_text` still sees them.
fn is_file_change_item_notification(n: &Notification) -> bool {
    if is_agent_message_notification(n)
        || n.method == "item/tool/call"
        || is_native_operation_completion_notification(n)
    {
        return false;
    }
    let method = n.method.as_str();
    method.contains("fileChange")
        || method.contains("FileChange")
        || method.contains("file_change")
        || method.starts_with("item/")
}

fn is_native_operation_completion_notification(n: &Notification) -> bool {
    if n.method != "item/completed" {
        return false;
    }
    matches!(
        n.params
            .as_ref()
            .and_then(|params| params.get("item"))
            .and_then(|item| item.get("type"))
            .and_then(Value::as_str),
        Some("fileChange" | "commandExecution")
    )
}

fn notification_matches_turn_id(n: &Notification, turn_id: &str) -> bool {
    n.params
        .as_ref()
        .and_then(notification_turn_id)
        .map(|id| id == turn_id)
        .unwrap_or(false)
}

fn notification_turn_id(params: &Value) -> Option<&str> {
    params
        .get("turnId")
        .or_else(|| params.get("turn_id"))
        .or_else(|| params.get("turn").and_then(|turn| turn.get("id")))
        .or_else(|| params.get("id"))
        .and_then(Value::as_str)
}

fn error_object_message(error: &Value) -> Option<String> {
    error
        .get("message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn error_object_info(error: &Value) -> Option<&str> {
    error
        .get("codexErrorInfo")
        .or_else(|| error.get("code"))
        .and_then(Value::as_str)
}

fn error_notification_message(note: &Notification) -> Option<String> {
    if note.method != "error" {
        return None;
    }
    let params = note.params.as_ref()?;
    let error = params.get("error").unwrap_or(params);
    format_codex_turn_failure(Some(error), None)
}

fn format_codex_turn_failure(error: Option<&Value>, status: Option<&str>) -> Option<String> {
    let message = error.and_then(error_object_message);
    let info = error.and_then(error_object_info);
    if status == Some("failed") || message.is_some() || info == Some("usageLimitExceeded") {
        Some(message.unwrap_or_else(|| match info {
            Some(info) => format!("Codex turn failed ({info})"),
            None => "Codex turn failed".to_owned(),
        }))
    } else {
        None
    }
}

fn turn_failure_from_params(params: &Value) -> Option<String> {
    let turn = params.get("turn").unwrap_or(params);
    let status = turn.get("status").and_then(Value::as_str);
    let error = turn.get("error").or_else(|| params.get("error"));
    format_codex_turn_failure(error, status)
}

fn assistant_text_from_notifications(notes: &[Notification]) -> (String, bool) {
    let completed_text = completed_assistant_text_from_notifications(notes);
    if completed_text.is_empty() {
        (agent_message_text_from_notifications(notes), false)
    } else {
        (completed_text, true)
    }
}

fn completed_assistant_text_from_notifications(notes: &[Notification]) -> String {
    notes
        .iter()
        .filter_map(agent_message_completed_text)
        .collect::<String>()
}

async fn initialize_client(client: &CodexAppServerClient) -> Result<Value> {
    let initialized = client
        .initialize(InitializeOptions {
            experimental_api: true,
            ..Default::default()
        })
        .await
        .context("initialize Codex app-server")?;

    // Native agent will answer item/tool/call and approval RPCs.
    client.set_external_server_requests(true);
    Ok(initialized)
}

fn ensure_codex_compatibility_ready(compatibility: &CodexCompatibilityReport) -> Result<()> {
    if compatibility.is_ready() {
        return Ok(());
    }
    bail!(
        "Codex app-server is missing required capabilities: {}",
        compatibility.missing_required.join(", ")
    )
}

pub fn codex_compatibility_from_initialize(result: &Value) -> CodexCompatibilityReport {
    let protocol_version = result
        .get("protocolVersion")
        .or_else(|| result.get("protocol_version"))
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    let Some(capabilities) = result.get("capabilities").filter(|value| value.is_object()) else {
        return CodexCompatibilityReport {
            protocol_version,
            ..CodexCompatibilityReport::legacy_unknown()
        };
    };

    let declared = collect_capability_maps(capabilities);
    if declared.is_empty() {
        return CodexCompatibilityReport {
            protocol_version,
            ..CodexCompatibilityReport::legacy_unknown()
        };
    }
    let has_method = |capability: &str| {
        declared
            .methods
            .iter()
            .any(|declared| capability_alias_matches(declared, capability))
    };
    let has_notification = |capability: &str| {
        declared
            .notifications
            .iter()
            .any(|declared| capability_alias_matches(declared, capability))
    };
    let has_terminal_notification = || {
        [
            "turn/completed",
            "turn/complete",
            "turn/completed/v2",
            "codex/event",
        ]
        .into_iter()
        .any(has_notification)
    };
    let mut missing_required = Vec::new();
    for required in ["thread/start", "turn/start", "turn/interrupt"] {
        if !has_method(required) {
            missing_required.push(required.to_owned());
        }
    }
    for required in ["item/tool/call", "item/agentMessage/delta"] {
        if !has_notification(required) {
            missing_required.push(required.to_owned());
        }
    }
    if !has_terminal_notification() {
        missing_required.push("turn/completed".to_owned());
    }

    CodexCompatibilityReport {
        protocol_version,
        resume: has_method("thread/resume"),
        steering: has_method("turn/steer"),
        missing_required,
    }
}

#[derive(Debug, Default)]
struct CapabilityMaps {
    methods: Vec<String>,
    notifications: Vec<String>,
}

impl CapabilityMaps {
    fn is_empty(&self) -> bool {
        self.methods.is_empty() && self.notifications.is_empty()
    }
}

fn collect_capability_maps(capabilities: &Value) -> CapabilityMaps {
    let mut maps = CapabilityMaps::default();
    let explicit_methods = capabilities.get("methods");
    let explicit_notifications = capabilities.get("notifications");
    if explicit_methods.is_some() || explicit_notifications.is_some() {
        if let Some(methods) = explicit_methods {
            collect_capability_strings(methods, &mut maps.methods);
        }
        if let Some(notifications) = explicit_notifications {
            collect_capability_strings(notifications, &mut maps.notifications);
        }
        return maps;
    }

    let mut legacy = Vec::new();
    collect_capability_strings(capabilities, &mut legacy);
    maps.methods = legacy.clone();
    maps.notifications = legacy;
    maps
}

fn collect_capability_strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) => out.push(text.clone()),
        Value::Array(values) => {
            for value in values {
                collect_capability_strings(value, out);
            }
        }
        Value::Object(map) => {
            for (key, value) in map {
                if value.as_bool() == Some(true) && key.contains('/') {
                    out.push(key.clone());
                }
                collect_capability_strings(value, out);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn capability_alias_matches(declared: &str, expected: &str) -> bool {
    let declared = declared.trim();
    declared == expected
        || declared.eq_ignore_ascii_case(expected)
        || declared.replace('_', "/").eq_ignore_ascii_case(expected)
}

pub fn is_supported_codex_notification(method: &str) -> bool {
    matches!(
        method,
        "turn/completed"
            | "turn/complete"
            | "turn/completed/v2"
            | "turn/usage"
            | "item/agentMessage/delta"
            | "item/agentMessage/completed"
            | "item/tool/call"
            | "item/completed"
            | "codex/event"
    )
}

fn thread_start_extra(
    dynamic_tools: &[DynamicToolSpec],
    instructions: Option<String>,
) -> Option<Value> {
    let mut extra = Map::new();
    if !dynamic_tools.is_empty() {
        let tools: Vec<Value> = dynamic_tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "inputSchema": tool.input_schema,
                })
            })
            .collect();
        extra.insert("dynamicTools".to_owned(), Value::Array(tools));
    }
    if let Some(instructions) = instructions.filter(|s| !s.trim().is_empty()) {
        // `ThreadStartParams` (app-server-protocol v2) field for standing
        // instructions. There is no `instructions` key, and setting
        // `baseInstructions` would replace Codex's own base prompt.
        extra.insert("developerInstructions".to_owned(), json!(instructions));
    }
    (!extra.is_empty()).then_some(Value::Object(extra))
}

pub fn is_thread_not_found_error(error: &anyhow::Error) -> bool {
    if let Some(rpc_error) = error.downcast_ref::<JsonRpcError>() {
        // No vendored protocol fixture in this checkout exposes a stable
        // app-server-specific code/data shape for `thread/resume` missing
        // threads. The narrow fallback is the exact legacy JSON-RPC error
        // observed by the existing transport tests.
        return rpc_error.code == Some(-32000)
            && rpc_error.data.is_none()
            && normalize_resume_error_message(&rpc_error.message) == "thread not found";
    }
    normalize_resume_error_message(&error.to_string()) == "thread not found"
}

fn normalize_resume_error_message(message: &str) -> String {
    message
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

/// Optional process-local override for the app-server executable. Hosted
/// children receive these variables through their own transport environment;
/// the normal desktop path leaves both unset and resolves Codex as before.
pub(super) fn codex_app_server_spawn_override_from_env()
-> Result<(Option<String>, Option<Vec<String>>)> {
    let command = env::var("MAESTRO_CODEX_APP_SERVER_COMMAND")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let args = env::var("MAESTRO_CODEX_APP_SERVER_ARGS_JSON")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| {
            serde_json::from_str::<Vec<String>>(&value)
                .context("invalid MAESTRO_CODEX_APP_SERVER_ARGS_JSON")
        })
        .transpose()?;
    if args.is_some() && command.is_none() {
        bail!("MAESTRO_CODEX_APP_SERVER_ARGS_JSON requires MAESTRO_CODEX_APP_SERVER_COMMAND");
    }
    Ok((command, args))
}

/// Convert persisted semantic history into protocol-defined Responses API
/// items. Tool-call/result IDs remain paired in the provider-visible thread.
fn semantic_messages_to_codex_items(messages: &[Message]) -> Vec<Value> {
    let mut items = Vec::new();
    for message in messages {
        match &message.content {
            MessageContent::Text(text) if !text.is_empty() => {
                items.push(codex_message_item(message.role, text));
            }
            MessageContent::Text(_) => {}
            MessageContent::Blocks(blocks) => {
                for block in blocks {
                    match block {
                        ContentBlock::Text { text } if !text.is_empty() => {
                            items.push(codex_message_item(message.role, text));
                        }
                        ContentBlock::Text { .. } => {}
                        ContentBlock::ToolUse {
                            id, name, input, ..
                        } => items.push(json!({
                            "type": "function_call",
                            "call_id": id,
                            "name": name,
                            "arguments": input.to_string(),
                        })),
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } => items.push(json!({
                            "type": "function_call_output",
                            "call_id": tool_use_id,
                            "output": content,
                        })),
                        // These blocks are excluded at checkpoint creation.
                        ContentBlock::Image { .. } | ContentBlock::Thinking { .. } => {}
                    }
                }
            }
        }
    }
    items
}

fn codex_message_item(role: Role, text: &str) -> Value {
    let content_type = if role == Role::Assistant {
        "output_text"
    } else {
        "input_text"
    };
    json!({
        "type": "message",
        "role": match role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "developer",
        },
        "content": [{ "type": content_type, "text": text }],
    })
}

/// Event while driving a Codex app-server turn.
pub enum TurnWaitEvent {
    ServerRequest(IncomingServerRequest),
    Completed(CodexAppServerTurnResult),
    Pending,
}

/// True when the configured model should use Codex app-server turns.
pub fn model_should_use_app_server_turns(model: &str) -> bool {
    model
        .split_once('/')
        .map(|(provider, model_id)| {
            !model_id.trim().is_empty()
                && matches!(
                    provider.trim().to_ascii_lowercase().as_str(),
                    "openai-codex" | "codex"
                )
        })
        .unwrap_or(false)
}

/// Return the canonical model id sent to `thread/start`.
pub fn codex_thread_model_id(model: &str) -> String {
    if model_should_use_app_server_turns(model) {
        model.split_once('/').map_or_else(
            || model.trim().to_owned(),
            |(_, model_id)| model_id.trim().to_owned(),
        )
    } else {
        model.trim().to_owned()
    }
}

/// Retire the exact persistent binding before a standing prompt changes.
///
/// App-server exposes standing `developerInstructions` only on `thread/start`;
/// resuming the existing binding would therefore preserve stale instructions.
/// The caller must also invalidate its live session and restore semantic
/// history before the next turn.
pub fn retire_persistent_thread_for_prompt_change(
    _model: &str,
    _cwd: &str,
    _session_id: Option<&str>,
) -> Result<bool> {
    bail!("Codex app-server auth must be resolved by the composing host")
}

pub fn retire_persistent_thread_for_prompt_change_with_auth(
    model: &str,
    cwd: &str,
    session_id: Option<&str>,
    auth: &NativeCodexAuth,
) -> Result<bool> {
    if !model_should_use_app_server_turns(model) {
        return Ok(false);
    }
    let key = CodexSessionKey::new(
        auth.profile_name.clone(),
        std::path::Path::new(cwd),
        codex_thread_model_id(model),
    )?
    .with_session_id(session_id);
    CodexThreadBinding::quarantine_at(&auth.state_root, &key)
}

/// Map native tool definitions into app-server dynamic tool specs.
pub fn dynamic_tools_from_native(
    tools: &HashMap<String, crate::agent::ToolDefinition>,
) -> Vec<DynamicToolSpec> {
    let mut specs: Vec<DynamicToolSpec> = tools
        .values()
        .map(|definition| {
            let tool = &definition.tool;
            DynamicToolSpec {
                name: sanitize_dynamic_tool_name(&tool.name),
                description: tool.description.clone(),
                input_schema: tool.input_schema.clone(),
            }
        })
        .collect();
    specs.sort_by(|a, b| a.name.cmp(&b.name));
    specs
}

fn sanitize_dynamic_tool_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        out = "maestro_tool".to_owned();
    }
    if out == "mcp" || out.starts_with("mcp__") {
        out = format!("maestro_{out}");
    }
    out.chars().take(128).collect()
}

/// Build the JSON-RPC result body for a successful dynamic tool call.
///
/// Shape matches `codex-rs/app-server-protocol` `DynamicToolCallResponse`:
/// `contentItems` + `success`.
pub fn tool_call_success_result(text: impl Into<String>) -> Value {
    json!({
        "success": true,
        "contentItems": [
            { "type": "inputText", "text": text.into() }
        ]
    })
}

/// Build the JSON-RPC result body for a failed dynamic tool call.
pub fn tool_call_error_result(message: impl Into<String>) -> Value {
    json!({
        "success": false,
        "contentItems": [
            { "type": "inputText", "text": message.into() }
        ]
    })
}

/// Extract tool name + arguments from an `item/tool/call` params object.
pub fn parse_tool_call_params(params: &Value) -> Result<(String, String, Value)> {
    let tool = params
        .get("tool")
        .or_else(|| params.get("toolName"))
        .or_else(|| params.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    if tool.is_empty() {
        bail!("item/tool/call missing tool name");
    }
    let call_id = params
        .get("callId")
        .or_else(|| params.get("toolCallId"))
        .or_else(|| params.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| anyhow::anyhow!("item/tool/call missing callId"))?;
    let args = params
        .get("arguments")
        .cloned()
        .or_else(|| params.get("args").cloned())
        .unwrap_or_else(|| json!({}));
    Ok((tool, call_id, args))
}

/// Approval decision payload for Codex app-server.
pub fn approval_decision(accept: bool) -> Value {
    if accept {
        json!({ "decision": "accept" })
    } else {
        json!({ "decision": "decline" })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_codex_provider_selects_app_server_turns() {
        assert!(model_should_use_app_server_turns("openai-codex/gpt-5.5"));
        assert!(model_should_use_app_server_turns("OPENAI-CODEX/gpt-5.5"));
        assert!(!model_should_use_app_server_turns("gpt-5.1-codex-max"));
        assert!(!model_should_use_app_server_turns("openai/gpt-5.5"));
        assert!(!model_should_use_app_server_turns("openai/codex-gpt"));
        assert!(!model_should_use_app_server_turns(
            "anthropic/claude-sonnet-4"
        ));
    }

    #[test]
    fn strips_provider_prefix_for_thread_model() {
        assert_eq!(codex_thread_model_id("openai-codex/gpt-5.5"), "gpt-5.5");
        assert_eq!(codex_thread_model_id("OPENAI-CODEX/gpt-5.5"), "gpt-5.5");
        assert_eq!(codex_thread_model_id("codex/gpt-5.5"), "gpt-5.5");
        assert_eq!(codex_thread_model_id("gpt-5.5"), "gpt-5.5");
        assert_eq!(
            codex_thread_model_id("openai/codex-gpt"),
            "openai/codex-gpt"
        );
    }

    #[tokio::test]
    async fn interrupt_forwards_the_active_turn_identity() {
        let (client, mock) = CodexAppServerClient::mock();
        let session = CodexAppServerTurnSession::from_started_thread(
            client,
            "thr-1".to_owned(),
            "gpt-5.5".to_owned(),
            &[],
        )
        .await
        .expect("session");
        let task = tokio::spawn(async move { session.interrupt_turn("turn-9", Some(1_000)).await });
        let request = mock.next_request().await.expect("turn/interrupt");
        assert_eq!(request["method"], "turn/interrupt");
        assert_eq!(request["params"]["threadId"], "thr-1");
        assert_eq!(request["params"]["turnId"], "turn-9");
        mock.respond(request["id"].as_u64().unwrap(), json!({}));
        task.await.unwrap().expect("interrupt response");
    }

    #[test]
    fn parses_tool_call_params() {
        let (tool, call_id, args) = parse_tool_call_params(&json!({
            "tool": "read",
            "callId": "c1",
            "arguments": { "path": "src/main.rs" }
        }))
        .unwrap();
        assert_eq!(tool, "read");
        assert_eq!(call_id, "c1");
        assert_eq!(args["path"], "src/main.rs");
    }

    #[test]
    fn rejects_tool_call_without_call_id() {
        let err = parse_tool_call_params(&json!({
            "tool": "read",
            "arguments": {}
        }))
        .unwrap_err();
        assert!(err.to_string().contains("callId"));
    }

    #[test]
    fn sanitizes_dynamic_tool_names() {
        assert_eq!(sanitize_dynamic_tool_name("bash tool"), "bash_tool");
        assert_eq!(sanitize_dynamic_tool_name("mcp"), "maestro_mcp");
    }

    #[test]
    fn completed_agent_text_is_marked_as_authoritative_full_text() {
        let notes = vec![
            Notification {
                method: "item/agentMessage/delta".to_owned(),
                params: Some(json!({ "turnId": "turn-1", "delta": "partial" })),
            },
            Notification {
                method: "item/completed".to_owned(),
                params: Some(json!({
                    "turnId": "turn-1",
                    "item": {
                        "id": "message-1",
                        "type": "agentMessage",
                        "text": "full answer"
                    }
                })),
            },
        ];

        assert_eq!(
            assistant_text_from_notifications(&notes),
            ("full answer".to_owned(), true)
        );
    }

    #[test]
    fn failed_turn_completion_is_a_provider_failure_not_empty_text() {
        let params = json!({
            "threadId": "thr-1",
            "turn": {
                "id": "turn-1",
                "status": "failed",
                "error": {
                    "message": "You've hit your usage limit. try again later.",
                    "codexErrorInfo": "usageLimitExceeded"
                },
                "items": []
            }
        });
        let failure = turn_failure_from_params(&params).expect("failed turn");
        assert!(failure.contains("usage limit"), "{failure}");
        assert!(!failure.contains("empty_assistant"));
    }

    #[test]
    fn error_notification_usage_limit_is_a_provider_failure() {
        let note = Notification {
            method: "error".to_owned(),
            params: Some(json!({
                "error": {
                    "message": "You've hit your usage limit. Visit https://chatgpt.com/codex/settings/usage to purchase more credits or try again later.",
                    "codexErrorInfo": "usageLimitExceeded",
                    "additionalDetails": null
                },
                "willRetry": false,
                "threadId": "thr-1",
                "turnId": "turn-1"
            })),
        };
        let failure = error_notification_message(&note).expect("error notification");
        assert!(failure.contains("usage limit"), "{failure}");
        assert!(!failure.contains("empty_assistant"));
    }

    #[test]
    fn failed_turn_without_message_uses_codex_error_info() {
        let params = json!({
            "turn": {
                "id": "turn-1",
                "status": "failed",
                "error": { "codexErrorInfo": "usageLimitExceeded" },
                "items": []
            }
        });
        let failure = turn_failure_from_params(&params).expect("failed turn");
        assert_eq!(failure, "Codex turn failed (usageLimitExceeded)");
    }

    #[test]
    fn file_change_drain_preserves_assistant_completions() {
        let assistant_completed = Notification {
            method: "item/completed".to_owned(),
            params: Some(json!({
                "item": {
                    "id": "message-1",
                    "type": "agentMessage",
                    "text": "full answer"
                }
            })),
        };
        let assistant_delta = Notification {
            method: "item/agentMessage/delta".to_owned(),
            params: Some(json!({ "delta": "partial" })),
        };
        let file_change_item = Notification {
            method: "item/completed".to_owned(),
            params: Some(json!({
                "item": {
                    "id": "fc-1",
                    "type": "fileChange",
                    "changes": [{ "path": "src/lib.rs", "kind": "update" }]
                }
            })),
        };
        let file_change_named = Notification {
            method: "item/fileChange/updated".to_owned(),
            params: Some(json!({ "itemId": "fc-2" })),
        };
        let tool_call = Notification {
            method: "item/tool/call".to_owned(),
            params: Some(json!({ "tool": "read" })),
        };

        assert!(!is_file_change_item_notification(&assistant_completed));
        assert!(!is_file_change_item_notification(&assistant_delta));
        assert!(!is_file_change_item_notification(&tool_call));
        assert!(is_native_operation_completion_notification(
            &file_change_item
        ));
        assert!(
            !is_file_change_item_notification(&file_change_item),
            "item/completed is owned exclusively by the native completion drain"
        );
        assert!(is_file_change_item_notification(&file_change_named));
    }

    #[test]
    fn thread_not_found_classifier_requires_exact_legacy_shape() {
        let exact = anyhow::anyhow!(JsonRpcError {
            code: Some(-32000),
            message: " Thread\nnot   found ".to_owned(),
            data: None,
        });
        assert!(is_thread_not_found_error(&exact));

        let wrong_data = anyhow::anyhow!(JsonRpcError {
            code: Some(-32000),
            message: "thread not found".to_owned(),
            data: Some(json!({ "reason": "storage_unavailable" })),
        });
        assert!(!is_thread_not_found_error(&wrong_data));

        let wrong_code = anyhow::anyhow!(JsonRpcError {
            code: Some(-32603),
            message: "thread not found".to_owned(),
            data: None,
        });
        assert!(!is_thread_not_found_error(&wrong_code));

        let broad_message = anyhow::anyhow!(JsonRpcError {
            code: Some(-32000),
            message: "thread backend not found during outage".to_owned(),
            data: None,
        });
        assert!(!is_thread_not_found_error(&broad_message));
    }

    #[test]
    fn semantic_history_becomes_provider_visible_message_and_tool_pair_items() {
        let items = semantic_messages_to_codex_items(&[
            Message {
                role: Role::User,
                content: MessageContent::text("first prompt"),
            },
            Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                    id: "call-42".to_owned(),
                    name: "read".to_owned(),
                    input: json!({ "path": "src/lib.rs" }),
                    gemini_context: None,
                }]),
            },
            Message {
                role: Role::User,
                content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: "call-42".to_owned(),
                    content: "[tool result omitted from checkpoint]".to_owned(),
                    is_error: Some(false),
                }]),
            },
        ]);

        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[0]["content"][0]["text"], "first prompt");
        assert_eq!(items[1]["type"], "function_call");
        assert_eq!(items[1]["call_id"], "call-42");
        assert_eq!(items[2]["type"], "function_call_output");
        assert_eq!(items[2]["call_id"], "call-42");
    }

    #[test]
    fn semantic_history_preserves_mixed_block_order() {
        let items = semantic_messages_to_codex_items(&[Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: "I'll inspect it.".to_owned(),
                },
                ContentBlock::ToolUse {
                    id: "call-1".to_owned(),
                    name: "read".to_owned(),
                    input: json!({ "path": "src/lib.rs" }),
                    gemini_context: None,
                },
                ContentBlock::Text {
                    text: "Then I'll explain.".to_owned(),
                },
            ]),
        }]);

        assert_eq!(items.len(), 3);
        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[0]["content"][0]["text"], "I'll inspect it.");
        assert_eq!(items[1]["type"], "function_call");
        assert_eq!(items[1]["call_id"], "call-1");
        assert_eq!(items[2]["type"], "message");
        assert_eq!(items[2]["content"][0]["text"], "Then I'll explain.");
    }

    #[derive(Debug, serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct AppServerConformanceFixture {
        version: u32,
        initialize: Vec<AppServerConformanceInitializeCase>,
        notifications: Vec<AppServerConformanceNotificationCase>,
    }

    #[derive(Debug, serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct AppServerConformanceInitializeCase {
        name: String,
        result: Value,
        expect_ready: bool,
        expect_resume: bool,
        expect_steering: bool,
    }

    #[derive(Debug, serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct AppServerConformanceNotificationCase {
        method: String,
        required: bool,
        expect_known: bool,
    }

    #[test]
    fn codex_compatibility_fixture_covers_required_optional_and_unknown_notifications() {
        let fixture: AppServerConformanceFixture = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../test/fixtures/codex/app-server-conformance-v1.json"
        )))
        .expect("app-server conformance fixture parses");
        assert_eq!(fixture.version, 1);

        for case in fixture.initialize {
            let compatibility = codex_compatibility_from_initialize(&case.result);
            assert_eq!(
                compatibility.is_ready(),
                case.expect_ready,
                "case {} readiness mismatch: {:?}",
                case.name,
                compatibility
            );
            assert_eq!(compatibility.resume, case.expect_resume, "{}", case.name);
            assert_eq!(
                compatibility.steering, case.expect_steering,
                "{}",
                case.name
            );
            if !case.expect_ready {
                assert!(
                    !compatibility.missing_required.is_empty(),
                    "{} should fail visibly with missing required capabilities",
                    case.name
                );
            }
        }

        for case in fixture.notifications {
            let known = is_supported_codex_notification(&case.method);
            assert_eq!(known, case.expect_known, "{}", case.method);
            assert!(
                known || !case.required,
                "unknown required notification {} must not be silently accepted",
                case.method
            );
        }
    }

    fn reasoning_model_fixture() -> Value {
        json!({
            "id": "gpt-6-astra", "model": "gpt-6-astra",
            "defaultReasoningEffort": "medium",
            "supportedReasoningEfforts": [
                {"reasoningEffort": "low"}, {"reasoningEffort": "medium"},
                {"reasoningEffort": "high"}, {"reasoningEffort": "xhigh"},
                {"reasoningEffort": "max"}, {"reasoningEffort": "ultra"}
            ]
        })
    }

    #[test]
    fn codex_thinking_levels_use_model_capabilities() {
        let mut model: CodexReasoningModel =
            serde_json::from_value(reasoning_model_fixture()).unwrap();
        use crate::agent::ThinkingLevel;
        for (level, expected) in [
            (ThinkingLevel::Off, "medium"),
            (ThinkingLevel::Low, "low"),
            (ThinkingLevel::Medium, "medium"),
            (ThinkingLevel::High, "high"),
            (ThinkingLevel::XHigh, "xhigh"),
            (ThinkingLevel::Max, "ultra"),
        ] {
            let (enabled, budget) = level.to_config();
            assert_eq!(model.select_effort(enabled, budget).unwrap(), expected);
        }
        assert!(
            model
                .select_effort(true, 1_024)
                .unwrap_err()
                .to_string()
                .contains("minimal")
        );
        model
            .supported_reasoning_efforts
            .retain(|option| !["ultra", "max"].contains(&option.reasoning_effort.as_str()));
        assert_eq!(model.select_effort(true, 50_000).unwrap(), "xhigh");
        model
            .supported_reasoning_efforts
            .push(CodexReasoningOption {
                reasoning_effort: "minimal".into(),
            });
        assert_eq!(model.select_effort(true, 1_024).unwrap(), "minimal");
        model.supported_reasoning_efforts.clear();
        assert!(model.select_effort(true, 50_000).is_err());
        assert!(model.select_effort(false, 0).is_err());
    }

    #[tokio::test]
    async fn codex_boost_compares_advertised_default_before_claiming_an_increase() {
        for (default, expected) in [("medium", true), ("ultra", false)] {
            let (client, mock) = CodexAppServerClient::mock();
            let session = CodexAppServerTurnSession::from_started_thread(
                client,
                "boost-check".into(),
                "gpt-6-astra".into(),
                &[],
            )
            .await
            .unwrap();
            let task = tokio::spawn(async move {
                session
                    .is_reasoning_boost((false, 0), (true, 50_000))
                    .await
                    .unwrap()
            });
            let request = mock.next_request().await.unwrap();
            assert_eq!(request["method"], "model/list");
            let mut model = reasoning_model_fixture();
            model["defaultReasoningEffort"] = json!(default);
            mock.respond(
                request["id"].as_u64().unwrap(),
                json!({"data": [model], "nextCursor": null}),
            );
            assert_eq!(task.await.unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn codex_turns_apply_thinking_changes_and_reset_previous_effort() {
        let (client, mock) = CodexAppServerClient::mock();
        let session = CodexAppServerTurnSession::from_started_thread(
            client,
            "thread-effort".into(),
            "gpt-6-astra".into(),
            &[],
        )
        .await
        .unwrap();
        let turns = tokio::spawn(async move {
            for (enabled, budget) in [(true, 20_000), (false, 0), (true, 50_000), (true, 4_096)] {
                session
                    .start_text_turn_with_thinking("continue", enabled, budget, Some(1_000))
                    .await
                    .unwrap();
            }
        });
        for expected in ["high", "medium", "ultra", "low"] {
            let list = mock.next_request().await.unwrap();
            assert_eq!(list["method"], "model/list");
            assert_eq!(list["params"]["includeHidden"], true);
            mock.respond(
                list["id"].as_u64().unwrap(),
                json!({"data": [reasoning_model_fixture()], "nextCursor": null}),
            );
            let turn = mock.next_request().await.unwrap();
            assert_eq!(turn["method"], "turn/start");
            assert_eq!(turn["params"]["effort"], expected);
            assert_eq!(turn["params"]["threadId"], "thread-effort");
            assert_eq!(turn["params"]["input"][0]["text"], "continue");
            mock.respond(
                turn["id"].as_u64().unwrap(),
                json!({"turn": {"id": expected}}),
            );
        }
        turns.await.unwrap();
    }

    #[tokio::test]
    async fn codex_thinking_lookup_follows_pages_and_rejects_unsupported_effort() {
        let (client, mock) = CodexAppServerClient::mock();
        let session = CodexAppServerTurnSession::from_started_thread(
            client,
            "thread-effort".into(),
            "gpt-6-astra".into(),
            &[],
        )
        .await
        .unwrap();
        let task = tokio::spawn(async move {
            session
                .start_text_turn_with_thinking("continue", true, 1_024, Some(1_000))
                .await
        });
        let first = mock.next_request().await.unwrap();
        mock.respond(
            first["id"].as_u64().unwrap(),
            json!({"data": [], "nextCursor": "page-2"}),
        );
        let second = mock.next_request().await.unwrap();
        assert_eq!(second["method"], "model/list");
        assert_eq!(second["params"]["cursor"], "page-2");
        mock.respond(
            second["id"].as_u64().unwrap(),
            json!({"data": [reasoning_model_fixture()], "nextCursor": null}),
        );
        assert!(
            task.await
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("does not support reasoning effort minimal")
        );
    }

    #[tokio::test]
    async fn codex_thinking_lookup_rejects_repeated_cursors() {
        let (client, mock) = CodexAppServerClient::mock();
        let session = CodexAppServerTurnSession::from_started_thread(
            client,
            "thread-effort".into(),
            "gpt-6-astra".into(),
            &[],
        )
        .await
        .unwrap();
        let task = tokio::spawn(async move {
            session
                .start_text_turn_with_thinking("continue", true, 20_000, Some(1_000))
                .await
        });
        for _ in 0..2 {
            let request = mock.next_request().await.unwrap();
            mock.respond(
                request["id"].as_u64().unwrap(),
                json!({"data": [], "nextCursor": "same-page"}),
            );
        }
        assert!(
            task.await
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("pagination did not converge")
        );
    }

    #[tokio::test]
    async fn restored_items_are_injected_before_the_next_turn_starts() {
        let (client, mock) = CodexAppServerClient::mock();
        let history = vec![Message {
            role: Role::User,
            content: MessageContent::text("first turn"),
        }];
        let session_task = tokio::spawn(async move {
            CodexAppServerTurnSession::from_started_thread(
                client,
                "thr-restored".to_owned(),
                "gpt-5.5".to_owned(),
                &history,
            )
            .await
        });

        let inject = mock.next_request().await.expect("inject before turn");
        assert_eq!(inject["method"], "thread/inject_items");
        assert_eq!(
            inject["params"]["items"][0]["content"][0]["text"],
            "first turn"
        );
        mock.respond(inject["id"].as_u64().unwrap(), json!({}));
        let session = session_task.await.unwrap().expect("restored session");

        let turn_task =
            tokio::spawn(async move { session.start_text_turn("second turn", Some(1_000)).await });
        let turn = mock.next_request().await.expect("next turn");
        assert_eq!(turn["method"], "turn/start");
        assert_eq!(turn["params"]["threadId"], "thr-restored");
        assert_eq!(turn["params"]["input"][0]["text"], "second turn");
        mock.respond(
            turn["id"].as_u64().unwrap(),
            json!({ "turn": { "id": "turn-2" } }),
        );
        assert_eq!(turn_task.await.unwrap().unwrap(), "turn-2");
    }

    #[tokio::test]
    async fn persisted_exact_binding_resumes_without_history_injection() {
        let state_root = tempfile::tempdir().expect("state root");
        let workspace = tempfile::tempdir().expect("workspace");
        let key = crate::codex_session::CodexSessionKey::new("work", workspace.path(), "gpt-5.5")
            .expect("session key");
        crate::codex_session::CodexThreadBinding::new(
            key.clone(),
            "thread-persisted",
            Some("2025-01-01".to_owned()),
            1_725_000_000,
        )
        .store_at(state_root.path())
        .expect("store binding");
        let manifest = crate::codex_session::CodexSessionManifest {
            key,
            approval_policy: "on-request".to_owned(),
            sandbox: "read-only".to_owned(),
            capabilities: crate::codex_session::CodexCapabilities::default(),
        };
        let history = vec![Message {
            role: Role::User,
            content: MessageContent::text("restored prompt must not replay"),
        }];
        let (client, mock) = CodexAppServerClient::mock();
        let state_root_path = state_root.path().to_path_buf();

        let task = tokio::spawn(async move {
            CodexAppServerTurnSession::connect_with_client_and_manifest(
                client,
                manifest,
                &state_root_path,
                &[],
                None,
                &history,
            )
            .await
        });

        let initialize = mock.next_request().await.expect("initialize");
        assert_eq!(initialize["method"], "initialize");
        mock.respond(
            initialize["id"].as_u64().unwrap(),
            json!({
                "protocolVersion": "2025-01-01",
                "capabilities": {
                    "methods": ["thread/start", "turn/start", "turn/interrupt", "thread/resume"],
                    "notifications": ["item/tool/call", "item/agentMessage/delta", "turn/completed"]
                }
            }),
        );
        let initialized = mock.next_request().await.expect("initialized notification");
        assert_eq!(initialized["method"], "initialized");
        let resume = mock.next_request().await.expect("resume");
        assert_eq!(resume["method"], "thread/resume");
        assert_eq!(resume["params"]["threadId"], "thread-persisted");
        assert_eq!(resume["params"]["model"], "gpt-5.5");
        mock.respond(
            resume["id"].as_u64().unwrap(),
            json!({ "thread": { "id": "thread-persisted" } }),
        );

        let session = task.await.unwrap().expect("resumed session");
        assert_eq!(session.thread_id(), "thread-persisted");
        assert_eq!(
            session.open_kind(),
            crate::codex_session::CodexSessionOpen::Resumed
        );
        let extra =
            tokio::time::timeout(std::time::Duration::from_millis(100), mock.next_request()).await;
        assert!(
            extra.is_err(),
            "successful resume must not send thread/inject_items or thread/start"
        );
    }

    #[tokio::test]
    async fn explicit_thread_not_found_quarantines_and_replaces_binding() {
        let state_root = tempfile::tempdir().expect("state root");
        let workspace = tempfile::tempdir().expect("workspace");
        let key = crate::codex_session::CodexSessionKey::new("work", workspace.path(), "gpt-5.5")
            .expect("session key");
        let binding =
            crate::codex_session::CodexThreadBinding::new(key.clone(), "missing-thread", None, 1);
        binding.store_at(state_root.path()).expect("store binding");
        let old_path = binding.path_at(state_root.path());
        let manifest = crate::codex_session::CodexSessionManifest {
            key: key.clone(),
            approval_policy: "on-request".to_owned(),
            sandbox: "read-only".to_owned(),
            capabilities: crate::codex_session::CodexCapabilities::default(),
        };
        let history = vec![Message {
            role: Role::User,
            content: MessageContent::text("restored context"),
        }];
        let (client, mock) = CodexAppServerClient::mock();
        let state_root_path = state_root.path().to_path_buf();

        let task = tokio::spawn(async move {
            CodexAppServerTurnSession::connect_with_client_and_manifest(
                client,
                manifest,
                &state_root_path,
                &[],
                None,
                &history,
            )
            .await
        });

        let initialize = mock.next_request().await.expect("initialize");
        mock.respond(
            initialize["id"].as_u64().unwrap(),
            json!({
                "protocolVersion": "2025-01-01",
                "capabilities": {
                    "methods": ["thread/start", "turn/start", "turn/interrupt", "thread/resume"],
                    "notifications": ["item/tool/call", "item/agentMessage/delta", "turn/completed"]
                }
            }),
        );
        let initialized = mock.next_request().await.expect("initialized");
        assert_eq!(initialized["method"], "initialized");
        let resume = mock.next_request().await.expect("resume");
        assert_eq!(resume["method"], "thread/resume");
        mock.reject(resume["id"].as_u64().unwrap(), "thread not found");
        let start = mock.next_request().await.expect("replacement start");
        assert_eq!(start["method"], "thread/start");
        mock.respond(
            start["id"].as_u64().unwrap(),
            json!({ "thread": { "id": "replacement-thread" } }),
        );
        let inject = mock.next_request().await.expect("replacement inject");
        assert_eq!(inject["method"], "thread/inject_items");
        mock.respond(inject["id"].as_u64().unwrap(), json!({}));

        let session = task.await.unwrap().expect("replacement session");
        assert_eq!(session.thread_id(), "replacement-thread");
        assert_eq!(
            session.open_kind(),
            crate::codex_session::CodexSessionOpen::Created
        );
        assert!(
            old_path.exists(),
            "replacement binding should occupy the exact-key path"
        );
        let loaded = crate::codex_session::CodexThreadBinding::load_at(state_root.path(), &key)
            .expect("load replacement")
            .expect("replacement binding");
        assert_eq!(loaded.thread_id, "replacement-thread");
    }

    #[tokio::test]
    async fn cleared_binding_runtime_connect_starts_fresh_with_live_manifest() {
        let state_root = tempfile::tempdir().expect("state root");
        let workspace = tempfile::tempdir().expect("workspace");
        let key = crate::codex_session::CodexSessionKey::new("work", workspace.path(), "gpt-5.5")
            .expect("session key");
        let binding =
            crate::codex_session::CodexThreadBinding::new(key.clone(), "missing-thread", None, 1);
        binding.store_at(state_root.path()).expect("store binding");
        crate::codex_session::CodexThreadBinding::quarantine_at(state_root.path(), &key)
            .expect("clear binding");
        let manifest = crate::codex_session::CodexSessionManifest {
            key: key.clone(),
            approval_policy: "never".to_owned(),
            sandbox: "workspace-write".to_owned(),
            capabilities: crate::codex_session::CodexCapabilities::default(),
        };
        let dynamic_tools = vec![DynamicToolSpec {
            name: "project_search".to_owned(),
            description: "Search indexed project files".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        }];
        let (client, mock) = CodexAppServerClient::mock();
        let state_root_path = state_root.path().to_path_buf();

        let task = tokio::spawn(async move {
            CodexAppServerTurnSession::connect_with_client_and_manifest(
                client,
                manifest,
                &state_root_path,
                &dynamic_tools,
                Some("Use the workspace-specific developer instructions.".to_owned()),
                &[],
            )
            .await
        });

        let initialize = mock.next_request().await.expect("initialize");
        mock.respond(
            initialize["id"].as_u64().unwrap(),
            json!({
                "protocolVersion": "2025-01-01",
                "capabilities": {
                    "methods": ["thread/start", "turn/start", "turn/interrupt", "thread/resume"],
                    "notifications": ["item/tool/call", "item/agentMessage/delta", "turn/completed"]
                }
            }),
        );
        let initialized = mock.next_request().await.expect("initialized");
        assert_eq!(initialized["method"], "initialized");
        let start = mock.next_request().await.expect("fresh start");
        assert_eq!(start["method"], "thread/start");
        assert_eq!(start["params"]["model"], "gpt-5.5");
        assert_eq!(
            start["params"]["cwd"],
            key.workspace.to_string_lossy().as_ref()
        );
        assert_eq!(start["params"]["approvalPolicy"], "never");
        assert_eq!(start["params"]["sandbox"], "workspace-write");
        assert_eq!(start["params"]["dynamicTools"][0]["name"], "project_search");
        assert_eq!(
            start["params"]["dynamicTools"][0]["inputSchema"]["properties"]["query"]["type"],
            "string"
        );
        assert_eq!(
            start["params"]["developerInstructions"],
            "Use the workspace-specific developer instructions."
        );
        assert!(start["params"].get("input").is_none());
        mock.respond(
            start["id"].as_u64().unwrap(),
            json!({ "thread": { "id": "runtime-thread" } }),
        );

        let session = task.await.unwrap().expect("fresh runtime session");
        assert_eq!(session.thread_id(), "runtime-thread");
        assert_eq!(
            session.open_kind(),
            crate::codex_session::CodexSessionOpen::Created
        );
        let loaded = crate::codex_session::CodexThreadBinding::load_at(state_root.path(), &key)
            .expect("load binding")
            .expect("binding");
        assert_eq!(loaded.thread_id, "runtime-thread");
    }

    #[tokio::test]
    async fn fresh_persistent_thread_omits_empty_sandbox() {
        let state_root = tempfile::tempdir().expect("state root");
        let workspace = tempfile::tempdir().expect("workspace");
        let key = crate::codex_session::CodexSessionKey::new("work", workspace.path(), "gpt-5.5")
            .expect("session key");
        let manifest = crate::codex_session::CodexSessionManifest {
            key,
            approval_policy: "on-request".to_owned(),
            sandbox: String::new(),
            capabilities: crate::codex_session::CodexCapabilities::default(),
        };
        let (client, mock) = CodexAppServerClient::mock();
        let state_root_path = state_root.path().to_path_buf();

        let task = tokio::spawn(async move {
            CodexAppServerTurnSession::connect_with_client_and_manifest(
                client,
                manifest,
                &state_root_path,
                &[],
                None,
                &[],
            )
            .await
        });

        let initialize = mock.next_request().await.expect("initialize");
        mock.respond(
            initialize["id"].as_u64().unwrap(),
            json!({
                "protocolVersion": "2025-01-01",
                "capabilities": {
                    "methods": ["thread/start", "turn/start", "turn/interrupt"],
                    "notifications": ["item/tool/call", "item/agentMessage/delta", "turn/completed"]
                }
            }),
        );
        let initialized = mock.next_request().await.expect("initialized");
        assert_eq!(initialized["method"], "initialized");
        let start = mock.next_request().await.expect("fresh start");
        assert_eq!(start["method"], "thread/start");
        assert_eq!(start["params"]["approvalPolicy"], "on-request");
        assert!(
            start["params"].get("sandbox").is_none(),
            "an empty persisted sandbox must not be sent to Codex: {start}"
        );
        mock.respond(
            start["id"].as_u64().unwrap(),
            json!({ "thread": { "id": "runtime-thread" } }),
        );

        let session = task.await.unwrap().expect("fresh runtime session");
        assert_eq!(session.thread_id(), "runtime-thread");
    }

    #[tokio::test]
    async fn resume_unavailable_skips_persisted_binding_and_replaces_it() {
        let state_root = tempfile::tempdir().expect("state root");
        let workspace = tempfile::tempdir().expect("workspace");
        let key = crate::codex_session::CodexSessionKey::new("work", workspace.path(), "gpt-5.5")
            .expect("session key");
        crate::codex_session::CodexThreadBinding::new(key.clone(), "thread-old", None, 1)
            .store_at(state_root.path())
            .expect("store binding");
        let manifest = crate::codex_session::CodexSessionManifest {
            key: key.clone(),
            approval_policy: "on-request".to_owned(),
            sandbox: "read-only".to_owned(),
            capabilities: crate::codex_session::CodexCapabilities::default(),
        };
        let (client, mock) = CodexAppServerClient::mock();
        let state_root_path = state_root.path().to_path_buf();

        let task = tokio::spawn(async move {
            CodexAppServerTurnSession::connect_with_client_and_manifest(
                client,
                manifest,
                &state_root_path,
                &[],
                None,
                &[],
            )
            .await
        });

        let initialize = mock.next_request().await.expect("initialize");
        assert_eq!(initialize["method"], "initialize");
        mock.respond(
            initialize["id"].as_u64().unwrap(),
            json!({
                "protocolVersion": "2025-01-01",
                "capabilities": {
                    "methods": ["thread/start", "turn/start", "turn/interrupt"],
                    "notifications": ["item/tool/call", "item/agentMessage/delta", "turn/completed"]
                }
            }),
        );
        let initialized = mock.next_request().await.expect("initialized");
        assert_eq!(initialized["method"], "initialized");
        let start = mock.next_request().await.expect("fresh start");
        assert_eq!(start["method"], "thread/start");
        assert_ne!(
            start["method"], "thread/resume",
            "resume=false must skip thread/resume"
        );
        mock.respond(
            start["id"].as_u64().unwrap(),
            json!({ "thread": { "id": "thread-new" } }),
        );

        let session = task.await.unwrap().expect("fresh session");
        assert_eq!(session.thread_id(), "thread-new");
        assert_eq!(
            session.open_kind(),
            crate::codex_session::CodexSessionOpen::Created
        );
        let loaded = crate::codex_session::CodexThreadBinding::load_at(state_root.path(), &key)
            .expect("load binding")
            .expect("binding");
        assert_eq!(loaded.thread_id, "thread-new");
    }

    #[tokio::test]
    async fn non_not_found_resume_failure_does_not_start_replacement() {
        let state_root = tempfile::tempdir().expect("state root");
        let workspace = tempfile::tempdir().expect("workspace");
        let key = crate::codex_session::CodexSessionKey::new("work", workspace.path(), "gpt-5.5")
            .expect("session key");
        crate::codex_session::CodexThreadBinding::new(key.clone(), "thread-persisted", None, 1)
            .store_at(state_root.path())
            .expect("store binding");
        let manifest = crate::codex_session::CodexSessionManifest {
            key,
            approval_policy: "on-request".to_owned(),
            sandbox: "read-only".to_owned(),
            capabilities: crate::codex_session::CodexCapabilities::default(),
        };
        let (client, mock) = CodexAppServerClient::mock();
        let state_root_path = state_root.path().to_path_buf();

        let task = tokio::spawn(async move {
            CodexAppServerTurnSession::connect_with_client_and_manifest(
                client,
                manifest,
                &state_root_path,
                &[],
                None,
                &[],
            )
            .await
        });

        let initialize = mock.next_request().await.expect("initialize");
        mock.respond(
            initialize["id"].as_u64().unwrap(),
            json!({
                "protocolVersion": "2025-01-01",
                "capabilities": {
                    "methods": ["thread/start", "turn/start", "turn/interrupt", "thread/resume"],
                    "notifications": ["item/tool/call", "item/agentMessage/delta", "turn/completed"]
                }
            }),
        );
        let initialized = mock.next_request().await.expect("initialized");
        assert_eq!(initialized["method"], "initialized");
        let resume = mock.next_request().await.expect("resume");
        assert_eq!(resume["method"], "thread/resume");
        mock.reject(resume["id"].as_u64().unwrap(), "rate limit");

        let error = match task.await.unwrap() {
            Ok(_) => panic!("resume failure should stay visible"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("thread/resume"));
        if let Ok(Ok(extra)) =
            tokio::time::timeout(std::time::Duration::from_millis(100), mock.next_request()).await
        {
            assert_ne!(
                extra["method"], "thread/start",
                "non-not-found resume errors must not start replacement threads"
            );
        }
    }

    #[tokio::test]
    async fn resume_error_with_thread_not_found_words_but_wrong_structure_does_not_replace() {
        let state_root = tempfile::tempdir().expect("state root");
        let workspace = tempfile::tempdir().expect("workspace");
        let key = crate::codex_session::CodexSessionKey::new("work", workspace.path(), "gpt-5.5")
            .expect("session key");
        crate::codex_session::CodexThreadBinding::new(key.clone(), "thread-persisted", None, 1)
            .store_at(state_root.path())
            .expect("store binding");
        let manifest = crate::codex_session::CodexSessionManifest {
            key: key.clone(),
            approval_policy: "on-request".to_owned(),
            sandbox: "read-only".to_owned(),
            capabilities: crate::codex_session::CodexCapabilities::default(),
        };
        let old_path =
            crate::codex_session::CodexThreadBinding::new(key, "thread-persisted", None, 1)
                .path_at(state_root.path());
        let (client, mock) = CodexAppServerClient::mock();
        let state_root_path = state_root.path().to_path_buf();

        let task = tokio::spawn(async move {
            CodexAppServerTurnSession::connect_with_client_and_manifest(
                client,
                manifest,
                &state_root_path,
                &[],
                None,
                &[],
            )
            .await
        });

        let initialize = mock.next_request().await.expect("initialize");
        mock.respond(
            initialize["id"].as_u64().unwrap(),
            json!({
                "protocolVersion": "2025-01-01",
                "capabilities": {
                    "methods": ["thread/start", "turn/start", "turn/interrupt", "thread/resume"],
                    "notifications": ["item/tool/call", "item/agentMessage/delta", "turn/completed"]
                }
            }),
        );
        let initialized = mock.next_request().await.expect("initialized");
        assert_eq!(initialized["method"], "initialized");
        let resume = mock.next_request().await.expect("resume");
        assert_eq!(resume["method"], "thread/resume");
        mock.reject_with_error(
            resume["id"].as_u64().unwrap(),
            -32603,
            "thread backend not found during outage",
            Some(json!({ "reason": "storage_unavailable" })),
        );

        let error = match task.await.unwrap() {
            Ok(_) => panic!("resume error should stay visible"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("thread/resume"));
        assert!(
            error
                .to_string()
                .contains("thread backend not found during outage"),
            "unexpected error: {error}"
        );
        assert!(
            old_path.exists(),
            "wrong structured error must not quarantine the binding"
        );
        if let Ok(Ok(extra)) =
            tokio::time::timeout(std::time::Duration::from_millis(100), mock.next_request()).await
        {
            assert_ne!(
                extra["method"], "thread/start",
                "wrong structured error must not start a replacement"
            );
        }
    }
}
