//! Typed, deterministic delegation through the managed Computer MCP contract.
//!
//! The model-facing subagent tools use this adapter as a backend. Computer's
//! individual MCP tools stay an implementation detail here so callers cannot
//! accidentally reorder creation, launch replay, readiness, or message
//! delivery.

use std::sync::Arc;

use futures::future::BoxFuture;
use maestro_runtime::{
    DelegationControlAction, DelegationEvent, DelegationEventKind, DelegationLifecycleState,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::mcp::{McpApiCapabilities, McpClient, McpToolResult};
use crate::orb_connection::HostedOrbOwnerBinding;

pub(crate) const DEFAULT_ORB_MCP_SERVER: &str = "orb";
/// Required one-command hosted-run capability. Splitting creation, launch, and
/// first-message delivery cannot provide durable same-request replay.
pub(crate) const ATOMIC_HOSTED_RUN_TOOL: &str = "computer_launch";
const LEGACY_ATOMIC_HOSTED_RUN_TOOL: &str = "orb_launch_hosted_task";
const HOSTED_COMPUTER_API_VERSION: &str = "1.0.0";
const HOSTED_COMPUTER_MINIMUM_CLIENT_VERSION: &str = "0.1.0";
const HOSTED_COMPUTER_REQUIRED_FEATURE: &str = "hosted_maestro_delegation";
const HOSTED_COMPUTER_CONTRACT_DIGEST: &str =
    "sha256:ac1ce494cafeef63ed6a466b6263a312b1bb1c132e2a3c58415f37a623228043";

/// Native console operations for a durable hosted Computer task.
///
/// These are intentionally expressed in product terms rather than MCP tool
/// names. The adapter remains the only layer that knows how Computer implements
/// the controls over MCP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrbConsoleAction {
    List,
    Status {
        id: String,
    },
    Followup {
        id: String,
        prompt: String,
    },
    Pause {
        id: String,
    },
    Resume {
        id: String,
    },
    Cancel {
        id: String,
    },
    Collect {
        id: String,
    },
    HandoffCreate {
        source_id: String,
        target_thread_id: String,
        files: Vec<String>,
        artifact_ids: Vec<String>,
        include_diff: bool,
    },
    HandoffList {
        target_thread_id: String,
    },
    HandoffRead {
        target_thread_id: String,
        package_id: String,
    },
}

/// Map Orb's owner-authored lifecycle vocabulary to the shared delegation
/// projection. Unknown states fail closed as `unavailable`; they must never be
/// rendered as a made-up successful state by the native console.
pub(crate) fn normalize_orb_lifecycle(raw: &str) -> DelegationLifecycleState {
    let state = raw.trim().to_ascii_lowercase().replace(['-', ' '], "_");
    match state.as_str() {
        "queued" | "pending" | "submitted" => DelegationLifecycleState::Queued,
        "starting" | "provisioning" | "connecting" | "reconnecting" | "active" | "running"
        | "working" | "processing" => DelegationLifecycleState::Active,
        "needs_attention" | "attention" => DelegationLifecycleState::NeedsAttention,
        "approval_required" | "waiting_for_approval" | "input_required" => {
            DelegationLifecycleState::ApprovalRequired
        }
        "paused" => DelegationLifecycleState::Paused,
        "resumed" => DelegationLifecycleState::Resumed,
        "cancelled" | "canceled" => DelegationLifecycleState::Cancelled,
        "completed" | "succeeded" | "success" => DelegationLifecycleState::Completed,
        "failed" | "rejected" | "timed_out" | "interrupted" => DelegationLifecycleState::Failed,
        _ => DelegationLifecycleState::Unavailable,
    }
}

/// Convert Orb's raw command names to the typed controls exposed by Maestro.
/// Unknown commands are ignored so a provider can add capabilities without
/// leaking implementation names into the native surface.
pub(crate) fn normalize_orb_controls(raw_commands: &[String]) -> Vec<DelegationControlAction> {
    raw_commands
        .iter()
        .fold(Vec::new(), |mut controls, command| {
            let normalized = command.trim().to_ascii_lowercase().replace(['-', ' '], "_");
            let command = normalized.strip_prefix("orb_").unwrap_or(&normalized);
            let action = match command {
                "steer" => Some(DelegationControlAction::Steer),
                "followup" | "follow_up" | "send_message" => {
                    Some(DelegationControlAction::Followup)
                }
                "collect" | "get" | "status" | "task_status" | "wait_task" => {
                    Some(DelegationControlAction::Collect)
                }
                "interrupt" | "interrupt_task" => Some(DelegationControlAction::Interrupt),
                "pause" | "pause_task" => Some(DelegationControlAction::Pause),
                "resume" | "resume_task" => Some(DelegationControlAction::Resume),
                "cancel" | "cancel_task" | "cancelled" | "canceled" => {
                    Some(DelegationControlAction::Cancel)
                }
                "retry" | "retry_task" => Some(DelegationControlAction::Retry),
                "request_review" | "review" => Some(DelegationControlAction::RequestReview),
                "rerun_checks" | "rerun_check" => Some(DelegationControlAction::RerunChecks),
                _ => None,
            };
            if let Some(action) = action {
                if !controls.contains(&action) {
                    controls.push(action);
                }
            }
            controls
        })
}

/// Build the same typed event used by native lifecycle notifications and the
/// headless protocol. This keeps the CLI and TUI projections in lockstep.
pub(crate) fn orb_delegation_event(
    event_id: &str,
    delegation_id: &str,
    attempt: u32,
    raw_state: &str,
    summary: Option<&str>,
    error: Option<&str>,
    raw_commands: &[String],
) -> DelegationEvent {
    let lifecycle_state = normalize_orb_lifecycle(raw_state);
    let status = lifecycle_state.as_str();
    let mut event = DelegationEvent::from_subagent_lifecycle(
        event_id,
        delegation_id,
        attempt,
        status,
        summary,
        error,
    );
    event.lifecycle_state = lifecycle_state;
    event.kind = match lifecycle_state {
        DelegationLifecycleState::Queued | DelegationLifecycleState::Active => {
            DelegationEventKind::Progress
        }
        DelegationLifecycleState::Paused
        | DelegationLifecycleState::Resumed
        | DelegationLifecycleState::Cancelled => DelegationEventKind::Control,
        DelegationLifecycleState::ApprovalRequired => DelegationEventKind::ApprovalRequired,
        DelegationLifecycleState::Completed => DelegationEventKind::Completion,
        DelegationLifecycleState::NeedsAttention | DelegationLifecycleState::Failed => {
            DelegationEventKind::NeedsAttention
        }
        DelegationLifecycleState::Unavailable => DelegationEventKind::Unavailable,
    };
    event.available_controls = normalize_orb_controls(raw_commands);
    event
}

/// The narrow MCP call surface needed by the adapter.
///
/// Keeping this seam separate from [`McpClient`] makes the lifecycle contract
/// testable with protocol fixtures without starting an Orb control plane.
pub(crate) trait OrbToolCaller: Send + Sync {
    fn call<'a>(
        &'a self,
        tool: &'a str,
        arguments: Value,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<McpToolResult, String>>;

    /// Return the connected server's advertised tool names for capability
    /// negotiation. An empty catalog fails hosted delegation before any
    /// mutation because the atomic launch contract cannot be confirmed.
    fn available_tools(&self) -> BoxFuture<'_, Result<Vec<String>, String>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    /// Return the authenticated server API compatibility envelope. The
    /// fail-closed default keeps callers from dispatching a mutating launch
    /// without an explicit admission implementation.
    fn api_capabilities(&self) -> BoxFuture<'_, Result<McpApiCapabilities, String>> {
        Box::pin(async { Err("Computer API capability negotiation is unavailable".to_string()) })
    }
}

/// Revalidate the managed connection snapshot immediately before each Orb
/// lifecycle request. The adapter owns an immutable client snapshot, so a
/// default-account switch must invalidate that adapter before it can dispatch
/// a request through the old client.
pub(crate) type OrbOperationValidator = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;

/// Whether a model-visible MCP name is one of Orb's raw lifecycle tools.
///
/// The adapter calls these names directly through the typed caller.  Blocking
/// them at the generic execution boundary prevents a configured external tool
/// definition or a hand-authored model call from bypassing the adapter.
pub(crate) fn is_reserved_orb_tool(name: &str) -> bool {
    let Some(rest) = name
        .strip_prefix("mcp__")
        .or_else(|| name.strip_prefix("mcp_"))
    else {
        return false;
    };
    let Some((server, tool)) = rest.split_once("__").or_else(|| rest.split_once('_')) else {
        return false;
    };
    server.eq_ignore_ascii_case(DEFAULT_ORB_MCP_SERVER)
        && (tool.starts_with("orb_") || tool.eq_ignore_ascii_case(ATOMIC_HOSTED_RUN_TOOL))
}

struct McpOrbToolCaller {
    client: Arc<McpClient>,
    server: String,
}

impl OrbToolCaller for McpOrbToolCaller {
    fn call<'a>(
        &'a self,
        tool: &'a str,
        arguments: Value,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<McpToolResult, String>> {
        let prefixed_name = format!("mcp__{}__{tool}", self.server);
        Box::pin(async move {
            self.client
                .call_tool_with_metadata_cancellable(&prefixed_name, arguments, cancel)
                .await
                .map(|(_, _, result)| result)
                .map_err(|error| error.to_string())
        })
    }

    fn available_tools(&self) -> BoxFuture<'_, Result<Vec<String>, String>> {
        Box::pin(async move {
            self.client
                .list_tools_by_server()
                .await
                .into_iter()
                .find(|(server, _)| server == &self.server)
                .map(|(_, tools)| tools)
                .ok_or_else(|| format!("Computer MCP server `{}` is not connected", self.server))
        })
    }

    fn api_capabilities(&self) -> BoxFuture<'_, Result<McpApiCapabilities, String>> {
        Box::pin(async move {
            self.client
                .api_capabilities_for_server(&self.server)
                .await
                .map_err(|error| error.to_string())
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum OrbDelegationError {
    #[error("Computer MCP call failed: {0}")]
    Call(String),
    #[error("Hosted Computer owner binding changed; remote operation refused: {0}")]
    OwnerBinding(String),
    #[error("Computer MCP tool {tool} returned an error: {message}")]
    Tool { tool: &'static str, message: String },
    #[error("Computer MCP tool {tool} returned invalid JSON: {message}")]
    Json { tool: &'static str, message: String },
    #[error("Computer atomic hosted-run response did not include a durable thread and receipt")]
    InvalidAtomicRunReceipt,
    #[error(
        "Computer server does not advertise computer_launch or its compatibility alias; refusing non-atomic hosted delegation"
    )]
    AtomicHostedRunUnavailable,
    #[error("Computer hosted capability negotiation failed: {0}")]
    CapabilityNegotiation(String),
    #[error("Computer hosted launch requires non-empty project and repository intent")]
    MissingAtomicLaunchInput,
    #[error("Computer repository URLs must be valid absolute URLs")]
    InvalidRepositoryUrl,
    #[error("Computer repository URLs must not contain embedded credentials")]
    CredentialBearingRepositoryUrl,
    #[error("Computer placement is controlled by the hosted project policy")]
    PlacementPolicyOverride,
    #[error("invalid Computer handoff selection: {0}")]
    InvalidHandoffSelection(String),
}

#[derive(Clone)]
pub(crate) struct OrbDelegationAdapter {
    caller: Arc<dyn OrbToolCaller>,
    owner_binding: Option<HostedOrbOwnerBinding>,
    operation_validator: Option<OrbOperationValidator>,
}

impl OrbDelegationAdapter {
    pub(crate) fn from_mcp_client_with_validator(
        client: Arc<McpClient>,
        server: impl Into<String>,
        owner_binding: HostedOrbOwnerBinding,
        operation_validator: Option<OrbOperationValidator>,
    ) -> Self {
        Self {
            caller: Arc::new(McpOrbToolCaller {
                client,
                server: server.into(),
            }),
            owner_binding: Some(owner_binding),
            operation_validator,
        }
    }

    pub(crate) fn owner_binding(&self) -> Option<&HostedOrbOwnerBinding> {
        self.owner_binding.as_ref()
    }

    #[cfg(test)]
    fn from_caller(caller: Arc<dyn OrbToolCaller>) -> Self {
        Self {
            caller,
            owner_binding: None,
            operation_validator: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn from_caller_with_owner_binding(
        caller: Arc<dyn OrbToolCaller>,
        owner_binding: HostedOrbOwnerBinding,
    ) -> Self {
        Self {
            caller,
            owner_binding: Some(owner_binding),
            operation_validator: None,
        }
    }

    #[cfg(test)]
    fn from_caller_with_owner_binding_and_validator(
        caller: Arc<dyn OrbToolCaller>,
        owner_binding: HostedOrbOwnerBinding,
        operation_validator: OrbOperationValidator,
    ) -> Self {
        Self {
            caller,
            owner_binding: Some(owner_binding),
            operation_validator: Some(operation_validator),
        }
    }

    fn validate_operation(&self) -> Result<(), OrbDelegationError> {
        self.operation_validator
            .as_ref()
            .map(|validator| validator().map_err(OrbDelegationError::OwnerBinding))
            .unwrap_or(Ok(()))
    }

    async fn call_json<T: for<'de> Deserialize<'de>>(
        &self,
        tool: &'static str,
        arguments: Value,
        cancel: &CancellationToken,
    ) -> Result<T, OrbDelegationError> {
        self.validate_operation()?;
        let result = self
            .caller
            .call(tool, arguments, cancel)
            .await
            .map_err(OrbDelegationError::Call)?;
        if result.is_error {
            return Err(OrbDelegationError::Tool {
                tool,
                message: result.as_string(),
            });
        }
        serde_json::from_str(&result.as_string()).map_err(|error| OrbDelegationError::Json {
            tool,
            message: error.to_string(),
        })
    }

    async fn call_ok(
        &self,
        tool: &'static str,
        arguments: Value,
        cancel: &CancellationToken,
    ) -> Result<McpToolResult, OrbDelegationError> {
        self.validate_operation()?;
        let result = self
            .caller
            .call(tool, arguments, cancel)
            .await
            .map_err(OrbDelegationError::Call)?;
        if result.is_error {
            return Err(OrbDelegationError::Tool {
                tool,
                message: result.as_string(),
            });
        }
        Ok(result)
    }

    pub(crate) async fn send_message(
        &self,
        thread_id: &str,
        content: &str,
        idempotency_key: &str,
        cancel: &CancellationToken,
    ) -> Result<(), OrbDelegationError> {
        self.call_ok(
            "orb_send_message",
            serde_json::json!({
                "thread_id": thread_id,
                "content": content,
                "idempotency_key": idempotency_key,
            }),
            cancel,
        )
        .await
        .map(|_| ())
    }

    pub(crate) async fn follow_up(
        &self,
        thread_id: &str,
        content: &str,
        idempotency_key: &str,
        cancel: &CancellationToken,
    ) -> Result<(), OrbDelegationError> {
        self.send_message(thread_id, content, idempotency_key, cancel)
            .await
    }

    pub(crate) async fn direct_task(
        &self,
        thread_id: &str,
        idempotency_key: &str,
        directive: &str,
        cancel: &CancellationToken,
    ) -> Result<OrbTaskStatus, OrbDelegationError> {
        self.call_json(
            "orb_direct_task",
            serde_json::json!({
                "thread_id": thread_id,
                "idempotency_key": idempotency_key,
                "directive": directive,
            }),
            cancel,
        )
        .await
    }

    pub(crate) async fn status(
        &self,
        thread_id: &str,
        cancel: &CancellationToken,
    ) -> Result<OrbTaskStatus, OrbDelegationError> {
        self.call_json(
            "orb_task_status",
            serde_json::json!({ "thread_id": thread_id }),
            cancel,
        )
        .await
    }

    pub(crate) async fn pause(
        &self,
        thread_id: &str,
        idempotency_key: &str,
        cancel: &CancellationToken,
    ) -> Result<OrbTaskStatus, OrbDelegationError> {
        self.command("orb_pause_task", thread_id, idempotency_key, cancel)
            .await
    }

    pub(crate) async fn resume(
        &self,
        thread_id: &str,
        idempotency_key: &str,
        cancel: &CancellationToken,
    ) -> Result<OrbTaskStatus, OrbDelegationError> {
        self.command("orb_resume_task", thread_id, idempotency_key, cancel)
            .await
    }

    pub(crate) async fn cancel(
        &self,
        thread_id: &str,
        idempotency_key: &str,
        cancel: &CancellationToken,
    ) -> Result<OrbTaskStatus, OrbDelegationError> {
        self.command("orb_cancel_task", thread_id, idempotency_key, cancel)
            .await
    }

    async fn command(
        &self,
        tool: &'static str,
        thread_id: &str,
        idempotency_key: &str,
        cancel: &CancellationToken,
    ) -> Result<OrbTaskStatus, OrbDelegationError> {
        let value: Value = self
            .call_json(
                tool,
                serde_json::json!({
                    "thread_id": thread_id,
                    "idempotency_key": idempotency_key,
                }),
                cancel,
            )
            .await?;
        let detail = value.get("detail").cloned().unwrap_or(value);
        serde_json::from_value(detail).map_err(|error| OrbDelegationError::Json {
            tool,
            message: error.to_string(),
        })
    }

    pub(crate) async fn collect(
        &self,
        thread_id: &str,
        cancel: &CancellationToken,
    ) -> Result<OrbThreadReport, OrbDelegationError> {
        self.call_json(
            "orb_get_thread",
            serde_json::json!({ "thread_id": thread_id }),
            cancel,
        )
        .await
    }

    /// Freeze a bounded, immutable handoff package in Orb's tenant-scoped
    /// workspace store. Maestro supplies only the source/target thread
    /// identities and the user's explicit selection; Orb remains authoritative
    /// for workspace bytes, artifact ownership, package persistence, and
    /// digesting.
    pub(crate) async fn create_handoff_package(
        &self,
        source_thread_id: &str,
        target_thread_id: &str,
        files: &[String],
        artifact_ids: &[String],
        include_diff: bool,
        cancel: &CancellationToken,
    ) -> Result<Value, OrbDelegationError> {
        validate_handoff_thread_id("source thread", source_thread_id)?;
        validate_handoff_thread_id("target thread", target_thread_id)?;
        let files = normalize_handoff_selection(files, "file")?;
        let artifact_ids = normalize_handoff_selection(artifact_ids, "artifact")?;
        let item_count = files.len() + artifact_ids.len() + usize::from(include_diff);
        if item_count == 0 {
            return Err(OrbDelegationError::InvalidHandoffSelection(
                "select at least one file, artifact, or --include-diff".to_string(),
            ));
        }
        if item_count > 32 {
            return Err(OrbDelegationError::InvalidHandoffSelection(
                "a handoff can contain at most 32 items".to_string(),
            ));
        }
        self.call_json(
            "orb_create_handoff_package",
            serde_json::json!({
                "source_thread_id": source_thread_id.trim(),
                "target_thread_id": target_thread_id.trim(),
                "files": files,
                "artifact_ids": artifact_ids,
                "include_diff": include_diff,
            }),
            cancel,
        )
        .await
    }

    /// List handoff manifests addressed to a target thread. The Orb service
    /// enforces same-tenant visibility for this read.
    pub(crate) async fn list_handoff_packages(
        &self,
        target_thread_id: &str,
        cancel: &CancellationToken,
    ) -> Result<Value, OrbDelegationError> {
        validate_handoff_thread_id("target thread", target_thread_id)?;
        self.call_json(
            "orb_list_handoff_packages",
            serde_json::json!({ "target_thread_id": target_thread_id.trim() }),
            cancel,
        )
        .await
    }

    /// Read one immutable handoff package by its content-addressed id.
    pub(crate) async fn read_handoff_package(
        &self,
        target_thread_id: &str,
        package_id: &str,
        cancel: &CancellationToken,
    ) -> Result<Value, OrbDelegationError> {
        validate_handoff_thread_id("target thread", target_thread_id)?;
        let package_id = package_id.trim();
        if package_id.is_empty() {
            return Err(OrbDelegationError::InvalidHandoffSelection(
                "package id must not be empty".to_string(),
            ));
        }
        self.call_json(
            "orb_read_handoff_package",
            serde_json::json!({
                "target_thread_id": target_thread_id.trim(),
                "package_id": package_id,
            }),
            cancel,
        )
        .await
    }

    async fn atomic_hosted_run_tool(&self) -> Result<Option<&'static str>, OrbDelegationError> {
        self.validate_operation()?;
        let capabilities = self
            .caller
            .api_capabilities()
            .await
            .map_err(OrbDelegationError::CapabilityNegotiation)?;
        validate_hosted_capabilities(&capabilities)?;
        let tools = self
            .caller
            .available_tools()
            .await
            .map_err(OrbDelegationError::Call)?;
        if tools.iter().any(|tool| tool == ATOMIC_HOSTED_RUN_TOOL) {
            return Ok(Some(ATOMIC_HOSTED_RUN_TOOL));
        }
        Ok(tools
            .iter()
            .any(|tool| tool == LEGACY_ATOMIC_HOSTED_RUN_TOOL)
            .then_some(LEGACY_ATOMIC_HOSTED_RUN_TOOL))
    }

    async fn run_atomic_hosted_task(
        &self,
        tool: &'static str,
        request: &OrbDelegateRequest,
        cancel: &CancellationToken,
    ) -> Result<OrbDelegationHandle, OrbDelegationError> {
        validate_hosted_settings(&request.settings)?;
        let Some(project) = request
            .project
            .as_deref()
            .map(str::trim)
            .filter(|project| !project.is_empty())
        else {
            return Err(OrbDelegationError::MissingAtomicLaunchInput);
        };
        let Some(repository_url) = request
            .settings
            .repository_url
            .as_deref()
            .map(str::trim)
            .filter(|repository_url| !repository_url.is_empty())
        else {
            return Err(OrbDelegationError::MissingAtomicLaunchInput);
        };

        // `computer_launch` and its legacy alias are typed control-plane
        // operations. Do not
        // serialize OrbSpawnSettings wholesale: its legacy names include
        // policy-owned fields that are not part of HostedLaunchRequest
        // (`resource_profile` becomes `runtime_profile`, and
        // `agent_selection` becomes `agent`).
        let mut arguments = serde_json::json!({
            "project": project,
            "repository_url": repository_url,
            "prompt": request.prompt,
            "agent": request.settings.agent_selection,
            "model": request.settings.model,
            "runtime_profile": request
                .profile
                .as_ref()
                .or(request.settings.resource_profile.as_ref()),
            "max_run_cost_credits": request.settings.max_run_cost_credits,
            "idempotency_key": request.start_idempotency_key,
        });
        if let Some(object) = arguments.as_object_mut() {
            object.retain(|_, value| !value.is_null());
        }

        let value: Value = self.call_json(tool, arguments, cancel).await?;
        let thread_id = first_string(
            &value,
            &[
                &["thread_id"],
                &["threadId"],
                &["task", "thread_id"],
                &["task", "threadId"],
                &["receipt", "thread_id"],
                &["receipt", "threadId"],
            ],
        );
        let receipt_id = first_string(
            &value,
            &[
                &["receipt_id"],
                &["receiptId"],
                &["receipt", "id"],
                &["launch_receipt", "id"],
                &["launch", "id"],
            ],
        );
        let (Some(thread_id), Some(receipt_id)) = (thread_id, receipt_id) else {
            return Err(OrbDelegationError::InvalidAtomicRunReceipt);
        };
        Ok(OrbDelegationHandle {
            thread_id,
            receipt_id,
            start_idempotency_key: request.start_idempotency_key.clone(),
        })
    }

    /// Execute the one conceptual initial delegation operation.
    pub(crate) async fn delegate(
        &self,
        request: &OrbDelegateRequest,
        cancel: &CancellationToken,
    ) -> Result<OrbDelegationHandle, OrbDelegationError> {
        let tool = self
            .atomic_hosted_run_tool()
            .await?
            .ok_or(OrbDelegationError::AtomicHostedRunUnavailable)?;
        self.run_atomic_hosted_task(tool, request, cancel).await
    }
}

fn validate_hosted_capabilities(
    capabilities: &McpApiCapabilities,
) -> Result<(), OrbDelegationError> {
    if capabilities.api_version != HOSTED_COMPUTER_API_VERSION {
        return Err(OrbDelegationError::CapabilityNegotiation(format!(
            "unsupported api_version {} (expected {})",
            capabilities.api_version, HOSTED_COMPUTER_API_VERSION
        )));
    }
    if capabilities.minimum_client_version != HOSTED_COMPUTER_MINIMUM_CLIENT_VERSION {
        return Err(OrbDelegationError::CapabilityNegotiation(format!(
            "unsupported minimum_client_version {} (expected {})",
            capabilities.minimum_client_version, HOSTED_COMPUTER_MINIMUM_CLIENT_VERSION
        )));
    }
    if !capabilities
        .features
        .iter()
        .any(|feature| feature == HOSTED_COMPUTER_REQUIRED_FEATURE)
    {
        return Err(OrbDelegationError::CapabilityNegotiation(format!(
            "required feature {} is not advertised",
            HOSTED_COMPUTER_REQUIRED_FEATURE
        )));
    }
    if capabilities.contract_digest != HOSTED_COMPUTER_CONTRACT_DIGEST {
        return Err(OrbDelegationError::CapabilityNegotiation(format!(
            "contract_digest {} does not match the pinned Computer contract",
            capabilities.contract_digest
        )));
    }
    Ok(())
}

fn validate_handoff_thread_id(label: &str, value: &str) -> Result<(), OrbDelegationError> {
    if value.trim().is_empty() {
        return Err(OrbDelegationError::InvalidHandoffSelection(format!(
            "{label} id must not be empty"
        )));
    }
    Ok(())
}

fn normalize_handoff_selection(
    values: &[String],
    kind: &str,
) -> Result<Vec<String>, OrbDelegationError> {
    let mut normalized = values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    normalized.sort();
    normalized.dedup();
    if kind == "file"
        && normalized.iter().any(|path| {
            let path = std::path::Path::new(path);
            path.is_absolute()
                || path
                    .components()
                    .any(|component| matches!(component, std::path::Component::ParentDir))
        })
    {
        return Err(OrbDelegationError::InvalidHandoffSelection(
            "files must be workspace-relative and cannot contain '..'".to_string(),
        ));
    }
    Ok(normalized)
}

fn first_string(value: &Value, paths: &[&[&str]]) -> Option<String> {
    paths.iter().find_map(|path| {
        let mut current = value;
        for key in *path {
            current = current.get(*key)?;
        }
        current
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

fn validate_hosted_settings(settings: &OrbSpawnSettings) -> Result<(), OrbDelegationError> {
    if settings.provisioner.is_some() || settings.machine.is_some() {
        return Err(OrbDelegationError::PlacementPolicyOverride);
    }
    let Some(repository_url) = settings.repository_url.as_deref() else {
        return Ok(());
    };
    let repository_url =
        url::Url::parse(repository_url).map_err(|_| OrbDelegationError::InvalidRepositoryUrl)?;
    let has_credentials =
        !repository_url.username().is_empty() || repository_url.password().is_some();
    if has_credentials {
        return Err(OrbDelegationError::CredentialBearingRepositoryUrl);
    }
    Ok(())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct OrbSpawnSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_selection: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_identity_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_policy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_approval: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provisioner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_run_cost_credits: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct OrbDelegationConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Credential-free project/repository intent resolved by Orb's project
    /// policy. This is not a provider, machine, or provisioner selector.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// High-level hosted capacity intent.  Maestro resolves `None` from the
    /// task role/policy; Orb remains authoritative for the catalog and final
    /// placement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(flatten)]
    pub settings: OrbSpawnSettings,
}

#[derive(Debug, Clone)]
pub(crate) struct OrbDelegateRequest {
    pub prompt: String,
    pub project: Option<String>,
    pub profile: Option<String>,
    pub settings: OrbSpawnSettings,
    pub start_idempotency_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OrbDelegationHandle {
    pub thread_id: String,
    pub receipt_id: String,
    pub start_idempotency_key: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct OrbTaskStatus {
    pub lifecycle_state: String,
    #[serde(default)]
    pub available_commands: Vec<String>,
    #[serde(default)]
    pub controller: Option<OrbControllerStatus>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct OrbControllerStatus {
    #[serde(default)]
    pub outcome: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct OrbThreadReport {
    #[serde(default)]
    pub summary: OrbThreadSummary,
    #[serde(default)]
    pub recent_messages: Vec<OrbThreadMessage>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct OrbThreadSummary {
    #[serde(default)]
    pub last_message: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct OrbThreadMessage {
    pub role: String,
    pub content: String,
}

impl OrbThreadReport {
    pub(crate) fn latest_assistant_message(&self) -> Option<&str> {
        self.recent_messages
            .iter()
            .rev()
            .find(|message| message.role.eq_ignore_ascii_case("assistant"))
            .map(|message| message.content.as_str())
            .or(self.summary.last_message.as_deref())
    }
}

/// Stable idempotency keys are safe to reuse after a retry or process restart.
pub(crate) fn deterministic_idempotency_key(kind: &str, parts: &[&str]) -> String {
    use sha2::{Digest, Sha256};

    let mut digest = Sha256::new();
    digest.update(kind.as_bytes());
    for part in parts {
        digest.update([0]);
        digest.update(part.as_bytes());
    }
    format!("maestro-orb:{kind}:{:x}", digest.finalize())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;
    use crate::mcp::McpContent;

    struct MockCaller {
        calls: Mutex<Vec<(String, Value)>>,
        responses: Mutex<VecDeque<Result<McpToolResult, String>>>,
        tools: Vec<String>,
        capabilities: Mutex<McpApiCapabilities>,
    }

    impl MockCaller {
        fn new(responses: Vec<Value>) -> Arc<Self> {
            Self::with_tools(responses, Vec::new())
        }

        fn with_tools(responses: Vec<Value>, tools: Vec<&str>) -> Arc<Self> {
            Self::with_faults(responses.into_iter().map(Ok).collect(), tools)
        }

        fn with_faults(responses: Vec<Result<Value, &str>>, tools: Vec<&str>) -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                responses: Mutex::new(
                    responses
                        .into_iter()
                        .map(|result| {
                            result
                                .map(|value| McpToolResult {
                                    structured_content: None,
                                    content: vec![McpContent::Text {
                                        text: serde_json::to_string(&value).unwrap(),
                                    }],
                                    is_error: false,
                                })
                                .map_err(str::to_string)
                        })
                        .collect(),
                ),
                tools: tools.into_iter().map(str::to_string).collect(),
                capabilities: Mutex::new(McpApiCapabilities {
                    api_version: HOSTED_COMPUTER_API_VERSION.to_string(),
                    minimum_client_version: HOSTED_COMPUTER_MINIMUM_CLIENT_VERSION.to_string(),
                    features: vec![HOSTED_COMPUTER_REQUIRED_FEATURE.to_string()],
                    contract_digest: HOSTED_COMPUTER_CONTRACT_DIGEST.to_string(),
                }),
            })
        }

        fn with_capabilities(
            responses: Vec<Value>,
            tools: Vec<&str>,
            capabilities: McpApiCapabilities,
        ) -> Arc<Self> {
            let caller = Self::with_tools(responses, tools);
            *caller.capabilities.lock().unwrap() = capabilities;
            caller
        }
    }

    impl OrbToolCaller for MockCaller {
        fn call<'a>(
            &'a self,
            tool: &'a str,
            arguments: Value,
            _cancel: &'a CancellationToken,
        ) -> BoxFuture<'a, Result<McpToolResult, String>> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .unwrap()
                    .push((tool.to_string(), arguments));
                self.responses.lock().unwrap().pop_front().unwrap()
            })
        }

        fn available_tools(&self) -> BoxFuture<'_, Result<Vec<String>, String>> {
            Box::pin(async move { Ok(self.tools.clone()) })
        }

        fn api_capabilities(&self) -> BoxFuture<'_, Result<McpApiCapabilities, String>> {
            Box::pin(async move { Ok(self.capabilities.lock().unwrap().clone()) })
        }
    }

    #[tokio::test]
    async fn delegate_refuses_non_atomic_server_without_mutating_remote_state() {
        let caller = MockCaller::new(Vec::new());
        let adapter = OrbDelegationAdapter::from_caller(caller.clone());
        let error = adapter
            .delegate(
                &OrbDelegateRequest {
                    prompt: "inspect the repository".to_string(),
                    project: Some("demo".to_string()),
                    profile: None,
                    settings: OrbSpawnSettings::default(),
                    start_idempotency_key: "start-key".to_string(),
                },
                &CancellationToken::new(),
            )
            .await
            .expect_err("non-atomic delegation must fail closed");
        assert!(matches!(
            error,
            OrbDelegationError::AtomicHostedRunUnavailable
        ));
        assert!(caller.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn delegate_refuses_incompatible_api_before_tool_call() {
        let caller = MockCaller::with_capabilities(
            vec![serde_json::json!({
                "thread_id": "must-not-be-created",
                "receipt_id": "must-not-be-created"
            })],
            vec![ATOMIC_HOSTED_RUN_TOOL],
            McpApiCapabilities {
                api_version: "9.9.9".to_string(),
                minimum_client_version: HOSTED_COMPUTER_MINIMUM_CLIENT_VERSION.to_string(),
                features: vec![HOSTED_COMPUTER_REQUIRED_FEATURE.to_string()],
                contract_digest: HOSTED_COMPUTER_CONTRACT_DIGEST.to_string(),
            },
        );
        let adapter = OrbDelegationAdapter::from_caller(caller.clone());
        let request = OrbDelegateRequest {
            prompt: "do not dispatch".to_string(),
            project: Some("demo".to_string()),
            profile: None,
            settings: OrbSpawnSettings {
                repository_url: Some("https://github.com/example/example".to_string()),
                ..OrbSpawnSettings::default()
            },
            start_idempotency_key: "incompatible-key".to_string(),
        };

        let error = adapter
            .delegate(&request, &CancellationToken::new())
            .await
            .expect_err("an incompatible API must fail closed");
        assert!(matches!(
            error,
            OrbDelegationError::CapabilityNegotiation(message)
                if message.contains("api_version")
        ));
        assert!(caller.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn delegate_refuses_unsupported_minimum_client_version_before_tool_call() {
        let caller = MockCaller::with_capabilities(
            vec![serde_json::json!({
                "thread_id": "must-not-be-created",
                "receipt_id": "must-not-be-created"
            })],
            vec![ATOMIC_HOSTED_RUN_TOOL],
            McpApiCapabilities {
                api_version: HOSTED_COMPUTER_API_VERSION.to_string(),
                minimum_client_version: "9.9.9".to_string(),
                features: vec![HOSTED_COMPUTER_REQUIRED_FEATURE.to_string()],
                contract_digest: HOSTED_COMPUTER_CONTRACT_DIGEST.to_string(),
            },
        );
        let adapter = OrbDelegationAdapter::from_caller(caller.clone());
        let request = OrbDelegateRequest {
            prompt: "do not dispatch".to_string(),
            project: Some("demo".to_string()),
            profile: None,
            settings: OrbSpawnSettings {
                repository_url: Some("https://github.com/example/example".to_string()),
                ..OrbSpawnSettings::default()
            },
            start_idempotency_key: "minimum-version-key".to_string(),
        };

        let error = adapter
            .delegate(&request, &CancellationToken::new())
            .await
            .expect_err("an unsupported client version must fail closed");
        assert!(matches!(
            error,
            OrbDelegationError::CapabilityNegotiation(message)
                if message.contains("minimum_client_version")
        ));
        assert!(caller.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn delegate_refuses_missing_required_feature_before_tool_call() {
        let caller = MockCaller::with_capabilities(
            vec![serde_json::json!({
                "thread_id": "must-not-be-created",
                "receipt_id": "must-not-be-created"
            })],
            vec![ATOMIC_HOSTED_RUN_TOOL],
            McpApiCapabilities {
                api_version: HOSTED_COMPUTER_API_VERSION.to_string(),
                minimum_client_version: HOSTED_COMPUTER_MINIMUM_CLIENT_VERSION.to_string(),
                features: Vec::new(),
                contract_digest: HOSTED_COMPUTER_CONTRACT_DIGEST.to_string(),
            },
        );
        let adapter = OrbDelegationAdapter::from_caller(caller.clone());
        let request = OrbDelegateRequest {
            prompt: "do not dispatch".to_string(),
            project: Some("demo".to_string()),
            profile: None,
            settings: OrbSpawnSettings {
                repository_url: Some("https://github.com/example/example".to_string()),
                ..OrbSpawnSettings::default()
            },
            start_idempotency_key: "missing-feature-key".to_string(),
        };

        let error = adapter
            .delegate(&request, &CancellationToken::new())
            .await
            .expect_err("a missing capability must fail closed");
        assert!(matches!(
            error,
            OrbDelegationError::CapabilityNegotiation(message)
                if message.contains("required feature")
        ));
        assert!(caller.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn delegate_refuses_contract_digest_mismatch_before_tool_call() {
        let caller = MockCaller::with_capabilities(
            vec![serde_json::json!({
                "thread_id": "must-not-be-created",
                "receipt_id": "must-not-be-created"
            })],
            vec![ATOMIC_HOSTED_RUN_TOOL],
            McpApiCapabilities {
                api_version: HOSTED_COMPUTER_API_VERSION.to_string(),
                minimum_client_version: HOSTED_COMPUTER_MINIMUM_CLIENT_VERSION.to_string(),
                features: vec![HOSTED_COMPUTER_REQUIRED_FEATURE.to_string()],
                contract_digest: "sha256:wrong".to_string(),
            },
        );
        let adapter = OrbDelegationAdapter::from_caller(caller.clone());
        let request = OrbDelegateRequest {
            prompt: "do not dispatch".to_string(),
            project: Some("demo".to_string()),
            profile: None,
            settings: OrbSpawnSettings {
                repository_url: Some("https://github.com/example/example".to_string()),
                ..OrbSpawnSettings::default()
            },
            start_idempotency_key: "digest-key".to_string(),
        };

        let error = adapter
            .delegate(&request, &CancellationToken::new())
            .await
            .expect_err("a contract mismatch must fail closed");
        assert!(matches!(
            error,
            OrbDelegationError::CapabilityNegotiation(message)
                if message.contains("contract_digest")
        ));
        assert!(caller.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn delegate_prefers_computer_launch_over_the_legacy_alias() {
        let caller = MockCaller::with_tools(
            vec![serde_json::json!({
                "task_id": "task-canonical",
                "thread_id": "thread-canonical",
                "launch": {"id": "receipt-canonical", "thread_id": "thread-canonical", "state": "starting"}
            })],
            vec!["orb_launch_hosted_task", "computer_launch"],
        );
        let adapter = OrbDelegationAdapter::from_caller(caller.clone());
        let request = OrbDelegateRequest {
            prompt: "run the focused check".to_string(),
            project: Some("demo".to_string()),
            profile: None,
            settings: OrbSpawnSettings {
                repository_url: Some("https://github.com/example/example".to_string()),
                ..OrbSpawnSettings::default()
            },
            start_idempotency_key: "canonical-key".to_string(),
        };

        adapter
            .delegate(&request, &CancellationToken::new())
            .await
            .unwrap();

        let calls = caller.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "computer_launch");
    }

    #[tokio::test]
    async fn delegate_prefers_atomic_hosted_run_when_advertised() {
        let caller = MockCaller::with_tools(
            vec![serde_json::json!({
                "task_id": "task-atomic",
                "thread_id": "thread-atomic",
                "launch": {"id": "receipt-atomic", "thread_id": "thread-atomic", "state": "starting"}
            })],
            vec![ATOMIC_HOSTED_RUN_TOOL],
        );
        let adapter = OrbDelegationAdapter::from_caller(caller.clone());
        let cancel = CancellationToken::new();
        let request = OrbDelegateRequest {
            prompt: "run the focused check".to_string(),
            project: Some("demo".to_string()),
            profile: Some("standard".to_string()),
            settings: OrbSpawnSettings {
                repository_url: Some("https://github.com/example/example".to_string()),
                agent_selection: Some("codex".to_string()),
                model: Some("gpt-5".to_string()),
                ..OrbSpawnSettings::default()
            },
            start_idempotency_key: "atomic-key".to_string(),
        };

        let handle = adapter.delegate(&request, &cancel).await.unwrap();
        assert_eq!(handle.thread_id, "thread-atomic");
        assert_eq!(handle.receipt_id, "receipt-atomic");
        let calls = caller.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, ATOMIC_HOSTED_RUN_TOOL);
        assert_eq!(calls[0].1["prompt"], "run the focused check");
        assert_eq!(calls[0].1["idempotency_key"], "atomic-key");
        assert_eq!(calls[0].1["project"], "demo");
        assert_eq!(
            calls[0].1["repository_url"],
            "https://github.com/example/example"
        );
        assert_eq!(calls[0].1["agent"], "codex");
        assert_eq!(calls[0].1["model"], "gpt-5");
        assert_eq!(calls[0].1["runtime_profile"], "standard");
        assert!(calls[0].1.get("title").is_none());
        assert!(calls[0].1.get("resource_profile").is_none());
        assert!(calls[0].1.get("agent_selection").is_none());
    }

    #[tokio::test]
    async fn delegate_accepts_the_legacy_hosted_launch_alias() {
        let caller = MockCaller::with_tools(
            vec![serde_json::json!({
                "task_id": "task-legacy",
                "thread_id": "thread-legacy",
                "launch": {"id": "receipt-legacy", "thread_id": "thread-legacy", "state": "starting"}
            })],
            vec![LEGACY_ATOMIC_HOSTED_RUN_TOOL],
        );
        let adapter = OrbDelegationAdapter::from_caller(caller.clone());
        let request = OrbDelegateRequest {
            prompt: "run the focused check".to_string(),
            project: Some("demo".to_string()),
            profile: None,
            settings: OrbSpawnSettings {
                repository_url: Some("https://github.com/example/example".to_string()),
                ..OrbSpawnSettings::default()
            },
            start_idempotency_key: "legacy-key".to_string(),
        };

        adapter
            .delegate(&request, &CancellationToken::new())
            .await
            .unwrap();

        let calls = caller.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, LEGACY_ATOMIC_HOSTED_RUN_TOOL);
    }

    #[tokio::test]
    async fn hosted_run_rejects_explicit_local_placement() {
        let caller = MockCaller::with_tools(
            vec![serde_json::json!({
                "thread_id": "thread-atomic",
                "receipt_id": "receipt-atomic"
            })],
            vec![ATOMIC_HOSTED_RUN_TOOL],
        );
        let adapter = OrbDelegationAdapter::from_caller(caller.clone());
        let request = OrbDelegateRequest {
            prompt: "inspect".to_string(),
            project: None,
            profile: None,
            settings: OrbSpawnSettings {
                provisioner: Some("local".to_string()),
                ..OrbSpawnSettings::default()
            },
            start_idempotency_key: "key".to_string(),
        };

        assert!(matches!(
            adapter.delegate(&request, &CancellationToken::new()).await,
            Err(OrbDelegationError::PlacementPolicyOverride)
        ));
        assert!(caller.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn hosted_run_rejects_malformed_repository_urls() {
        let error = validate_hosted_settings(&OrbSpawnSettings {
            repository_url: Some("https://secret@".to_string()),
            ..OrbSpawnSettings::default()
        })
        .expect_err("malformed repository URLs must fail closed");

        assert!(matches!(error, OrbDelegationError::InvalidRepositoryUrl));
    }

    #[tokio::test]
    async fn atomic_hosted_run_leaves_profile_resolution_to_computer() {
        let caller = MockCaller::with_tools(
            vec![serde_json::json!({
                "task_id": "task-default",
                "thread_id": "thread-default",
                "launch": {"id": "receipt-default", "thread_id": "thread-default"}
            })],
            vec![ATOMIC_HOSTED_RUN_TOOL],
        );
        let adapter = OrbDelegationAdapter::from_caller(caller.clone());
        let request = OrbDelegateRequest {
            prompt: "use the hosted default profile".to_string(),
            project: Some("demo".to_string()),
            profile: None,
            settings: OrbSpawnSettings {
                repository_url: Some("https://github.com/example/example".to_string()),
                ..OrbSpawnSettings::default()
            },
            start_idempotency_key: "default-key".to_string(),
        };

        adapter
            .delegate(&request, &CancellationToken::new())
            .await
            .unwrap();
        let calls = caller.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].1.get("resource_profile").is_none());
        assert!(calls[0].1.get("runtime_profile").is_none());
        assert_eq!(
            calls[0].1["repository_url"],
            "https://github.com/example/example"
        );
        assert!(calls[0].1.get("provisioner").is_none());
        assert!(calls[0].1.get("machine").is_none());
    }

    #[tokio::test]
    async fn atomic_hosted_run_rejects_missing_project_or_repository() {
        let caller =
            MockCaller::with_tools(vec![serde_json::json!({})], vec![ATOMIC_HOSTED_RUN_TOOL]);
        let adapter = OrbDelegationAdapter::from_caller(caller.clone());
        let request = OrbDelegateRequest {
            prompt: "inspect".to_string(),
            project: Some("demo".to_string()),
            profile: None,
            settings: OrbSpawnSettings::default(),
            start_idempotency_key: "missing-repository".to_string(),
        };

        assert!(matches!(
            adapter.delegate(&request, &CancellationToken::new()).await,
            Err(OrbDelegationError::MissingAtomicLaunchInput)
        ));
        assert!(caller.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn typed_controls_use_their_dedicated_idempotent_tools() {
        let caller = MockCaller::new(vec![
            serde_json::json!({"lifecycle_state":"active"}),
            serde_json::json!({"detail":{"lifecycle_state":"paused"}}),
            serde_json::json!({"detail":{"lifecycle_state":"active"}}),
            serde_json::json!({"detail":{"lifecycle_state":"cancelled"}}),
            serde_json::json!({"lifecycle_state":"active"}),
            serde_json::json!({"accepted":true}),
            serde_json::json!({"summary":{},"recent_messages":[]}),
        ]);
        let adapter = OrbDelegationAdapter::from_caller(caller.clone());
        let cancel = CancellationToken::new();

        assert_eq!(
            adapter
                .status("thread-1", &cancel)
                .await
                .unwrap()
                .lifecycle_state,
            "active"
        );
        assert_eq!(
            adapter
                .pause("thread-1", "pause-key", &cancel)
                .await
                .unwrap()
                .lifecycle_state,
            "paused"
        );
        assert_eq!(
            adapter
                .resume("thread-1", "resume-key", &cancel)
                .await
                .unwrap()
                .lifecycle_state,
            "active"
        );
        assert_eq!(
            adapter
                .cancel("thread-1", "cancel-key", &cancel)
                .await
                .unwrap()
                .lifecycle_state,
            "cancelled"
        );
        assert_eq!(
            adapter
                .direct_task("thread-1", "steer-key", "new directive", &cancel)
                .await
                .unwrap()
                .lifecycle_state,
            "active"
        );
        adapter
            .follow_up("thread-1", "follow up", "followup-1", &cancel)
            .await
            .unwrap();
        let report = adapter.collect("thread-1", &cancel).await.unwrap();
        assert!(report.latest_assistant_message().is_none());

        let calls = caller.calls.lock().unwrap();
        assert_eq!(calls[0].0, "orb_task_status");
        assert_eq!(calls[1].0, "orb_pause_task");
        assert_eq!(calls[1].1["idempotency_key"], "pause-key");
        assert_eq!(calls[2].0, "orb_resume_task");
        assert_eq!(calls[3].0, "orb_cancel_task");
        assert_eq!(calls[4].0, "orb_direct_task");
        assert_eq!(calls[4].1["idempotency_key"], "steer-key");
        assert_eq!(calls[4].1["directive"], "new directive");
        assert_eq!(calls[5].0, "orb_send_message");
        assert_eq!(calls[6].0, "orb_get_thread");
    }

    #[tokio::test]
    async fn handoff_controls_use_tenant_scoped_orb_tools_and_normalize_selection() {
        let caller = MockCaller::new(vec![
            serde_json::json!({
                "manifest": {
                    "package_id": "package-1",
                    "target_thread_id": "thread-target",
                    "items": []
                },
                "items": []
            }),
            serde_json::json!({"packages": []}),
            serde_json::json!({"manifest":{"package_id":"package-1"},"items":[]}),
        ]);
        let adapter = OrbDelegationAdapter::from_caller(caller.clone());
        let cancel = CancellationToken::new();
        let files = vec![
            "z.txt".to_string(),
            "a.txt".to_string(),
            "a.txt".to_string(),
        ];
        let artifacts = vec!["artifact-1".to_string()];

        adapter
            .create_handoff_package(
                "thread-source",
                "thread-target",
                &files,
                &artifacts,
                true,
                &cancel,
            )
            .await
            .unwrap();
        adapter
            .list_handoff_packages("thread-target", &cancel)
            .await
            .unwrap();
        adapter
            .read_handoff_package("thread-target", "package-1", &cancel)
            .await
            .unwrap();

        let calls = caller.calls.lock().unwrap();
        assert_eq!(calls[0].0, "orb_create_handoff_package");
        assert_eq!(calls[0].1["source_thread_id"], "thread-source");
        assert_eq!(calls[0].1["target_thread_id"], "thread-target");
        assert_eq!(calls[0].1["files"], serde_json::json!(["a.txt", "z.txt"]));
        assert_eq!(
            calls[0].1["artifact_ids"],
            serde_json::json!(["artifact-1"])
        );
        assert_eq!(calls[0].1["include_diff"], true);
        assert_eq!(calls[1].0, "orb_list_handoff_packages");
        assert_eq!(calls[2].0, "orb_read_handoff_package");
        assert_eq!(calls[2].1["package_id"], "package-1");
    }

    #[tokio::test]
    async fn handoff_rejects_empty_or_escaping_selection_before_network() {
        let caller = MockCaller::new(vec![]);
        let adapter = OrbDelegationAdapter::from_caller(caller.clone());
        let cancel = CancellationToken::new();

        assert!(matches!(
            adapter
                .create_handoff_package("thread-source", "thread-target", &[], &[], false, &cancel,)
                .await,
            Err(OrbDelegationError::InvalidHandoffSelection(_))
        ));
        assert!(matches!(
            adapter
                .create_handoff_package(
                    "thread-source",
                    "thread-target",
                    &["../secret".to_string()],
                    &[],
                    false,
                    &cancel,
                )
                .await,
            Err(OrbDelegationError::InvalidHandoffSelection(_))
        ));
        assert!(caller.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn owner_switch_between_preflight_and_call_fails_before_remote_dispatch() {
        let caller = MockCaller::new(vec![serde_json::json!({
            "lifecycle_state": "active"
        })]);
        let owner_a = HostedOrbOwnerBinding {
            organization_id: "org-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            connection_ref: "connection-a".to_owned(),
            managed_generation: 1,
        };
        let owner_b = HostedOrbOwnerBinding {
            organization_id: "org-b".to_owned(),
            workspace_id: "workspace-b".to_owned(),
            connection_ref: "connection-b".to_owned(),
            managed_generation: 2,
        };
        let current_owner = Arc::new(Mutex::new(owner_a.clone()));
        let expected_owner = owner_a.clone();
        let validator: OrbOperationValidator = {
            let current_owner = Arc::clone(&current_owner);
            Arc::new(move || {
                let current = current_owner.lock().unwrap().clone();
                if current != expected_owner {
                    return Err(format!(
                        "active owner switched to connection {}",
                        current.connection_ref
                    ));
                }
                Ok(())
            })
        };
        let adapter = OrbDelegationAdapter::from_caller_with_owner_binding_and_validator(
            caller.clone(),
            owner_a,
            validator,
        );

        // This is the execute-time preflight that already passed for owner A.
        assert_eq!(
            adapter.owner_binding().unwrap().connection_ref,
            "connection-a"
        );
        // Model set_default switching the managed connection before the actual
        // status request reaches the caller.
        *current_owner.lock().unwrap() = owner_b;

        let error = adapter
            .status("thread-a", &CancellationToken::new())
            .await
            .expect_err("stale owner must be rejected locally");
        assert!(matches!(error, OrbDelegationError::OwnerBinding(_)));
        assert!(caller.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn idempotency_key_is_stable_and_bounded() {
        let first = deterministic_idempotency_key("start", &["parent-call", "subagent"]);
        assert_eq!(
            first,
            deterministic_idempotency_key("start", &["parent-call", "subagent"])
        );
        assert!(first.len() <= 128);
        assert!(first.starts_with("maestro-orb:start:"));
    }

    #[test]
    fn raw_orb_tools_are_reserved_from_model_dispatch() {
        assert!(is_reserved_orb_tool("mcp__orb__orb_run_task"));
        assert!(is_reserved_orb_tool("mcp_orb_orb_send_message"));
        assert!(is_reserved_orb_tool("mcp__orb__computer_launch"));
        assert!(is_reserved_orb_tool("mcp_orb_computer_launch"));
        assert!(!is_reserved_orb_tool("mcp__calendar__orb_run_task"));
        assert!(!is_reserved_orb_tool("mcp__orb__search"));
    }

    #[test]
    fn native_projection_normalizes_lifecycle_and_controls() {
        assert_eq!(
            normalize_orb_lifecycle("reconnecting"),
            DelegationLifecycleState::Active
        );
        assert_eq!(
            normalize_orb_lifecycle("approval-required"),
            DelegationLifecycleState::ApprovalRequired
        );
        assert_eq!(
            normalize_orb_lifecycle("future_provider_state"),
            DelegationLifecycleState::Unavailable
        );
        let commands = [
            "orb_resume_task".to_string(),
            "orb_pause_task".to_string(),
            "follow_up".to_string(),
            "approve".to_string(),
            "unknown_internal_command".to_string(),
        ];
        let event = orb_delegation_event(
            "event-1",
            "task-1",
            2,
            "paused",
            Some("waiting"),
            None,
            &commands,
        );
        assert_eq!(event.lifecycle_state, DelegationLifecycleState::Paused);
        assert_eq!(event.kind, DelegationEventKind::Control);
        assert_eq!(
            event.available_controls,
            vec![
                DelegationControlAction::Resume,
                DelegationControlAction::Pause,
                DelegationControlAction::Followup
            ]
        );
        assert!(
            !event
                .available_controls
                .contains(&DelegationControlAction::Approve)
        );
        let encoded = serde_json::to_string(&event).expect("projection serializes");
        assert!(!encoded.contains("orb_pause_task"));
        assert!(!encoded.contains("credential"));
    }
}
