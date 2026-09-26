//! Shared agent and tool value contracts.
//!
//! These values are deliberately transport-neutral.  They are the smallest
//! set of agent-facing data that already crosses the native runtime, headless
//! adapters, and the runtime gateway.  Execution policy, tool implementations,
//! and UI state remain in their owning crates.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Result of one configured safe-mode validator.
///
/// The validator runner remains a TUI policy concern.  This value is shared
/// because tool receipts persist its complete result alongside write/edit
/// details.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidatorResult {
    pub command: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Maximum serialized size of one managed inference authorization.
pub const MAX_MANAGED_INFERENCE_AUTHORIZATION_BYTES: usize = 64 * 1024;

/// Opaque signed capability for one managed inference turn.
///
/// The transparent serde representation preserves the existing protocol value,
/// while `Debug` is deliberately redacted so command and queue diagnostics
/// cannot reveal it. Validation only checks the value shape; authentication,
/// tenant binding, policy, and approval gates remain with their existing owners.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ManagedInferenceAuthorization(String);

impl ManagedInferenceAuthorization {
    /// Validate the value before it crosses a runtime boundary.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.0.is_empty() {
            return Err("managedInferenceAuthorization must not be empty");
        }
        if self.0.len() > MAX_MANAGED_INFERENCE_AUTHORIZATION_BYTES {
            return Err("managedInferenceAuthorization exceeds 64 KiB");
        }
        if self.0.chars().any(char::is_control) {
            return Err("managedInferenceAuthorization contains control characters");
        }
        Ok(())
    }

    /// Construct an opaque authorization from its signed representation.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the signed representation.
    #[cfg(test)]
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume the wrapper and return the signed representation.
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl std::fmt::Debug for ManagedInferenceAuthorization {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ManagedInferenceAuthorization([REDACTED])")
    }
}

/// A short-lived, tenant-scoped bearer delivered with one managed invocation.
/// This value is never part of the signed inference authorization or session state.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedGatewayCredential {
    token: String,
    expires_at_epoch_seconds: i64,
}

impl ManagedGatewayCredential {
    pub fn new(token: impl Into<String>, expires_at_epoch_seconds: i64) -> Self {
        Self {
            token: token.into(),
            expires_at_epoch_seconds,
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.token.is_empty()
            || self.token.len() > 16 * 1024
            || self.token.chars().any(char::is_control)
        {
            return Err("managed gateway credential is malformed");
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| "system clock is unavailable")?
            .as_secs() as i64;
        if self.expires_at_epoch_seconds <= now.saturating_add(10) {
            return Err("managed gateway credential expires too soon");
        }
        Ok(())
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn expires_at_epoch_seconds(&self) -> i64 {
        self.expires_at_epoch_seconds
    }
}

impl std::fmt::Debug for ManagedGatewayCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ManagedGatewayCredential([REDACTED])")
    }
}

/// Messages sent from a host or client to the native agent.
///
/// The tagged JSON representation is the pre-existing `type`/snake-case
/// protocol used by local callers.  The runtime owns the value contract; a
/// host decides how to transport it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToAgent {
    /// User submitted a prompt.
    Prompt {
        /// User content.
        content: String,
        /// Attached file paths.
        #[serde(default)]
        attachments: Vec<String>,
    },
    /// User interrupted the active operation.
    Interrupt,
    /// Response to a pending tool approval.
    ToolResponse {
        /// Tool call being resolved.
        call_id: String,
        /// Whether execution was approved.
        approved: bool,
        /// Optional caller-produced result.
        result: Option<ToolResult>,
    },
    /// Cancel the active operation.
    Cancel,
    /// Shut down the agent.
    Shutdown,
}

/// Identity contract for a tool call that outlives the process that made it.
///
/// A session transcript stores each tool call alongside this contract. A host
/// recomputes the identity from its current registry before dispatch and must
/// refuse a call when the server or parameter surface changed. The contract is
/// data-only; it does not authorize execution or replace the owning policy
/// and approval gates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallContract {
    /// Provider-assigned id of the call this contract was recorded for.
    #[serde(rename = "callId", alias = "call_id")]
    pub call_id: String,
    /// Model-facing tool name, including any `mcp__<server>__` prefix.
    #[serde(rename = "toolName", alias = "tool_name")]
    pub tool_name: String,
    /// Hex SHA-256 over the tool name and its parameter surface.
    #[serde(rename = "schemaDigest", alias = "schema_digest")]
    pub schema_digest: String,
    /// MCP server that owned the tool, when the name carries one.
    #[serde(
        rename = "serverId",
        alias = "server_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub server_id: Option<String>,
}

impl ToolCallContract {
    /// Record the identity of `tool_name` as it exists right now.
    #[must_use]
    pub fn record(call_id: &str, tool_name: &str, input_schema: Option<&Value>) -> Self {
        Self {
            call_id: call_id.to_string(),
            tool_name: tool_name.to_string(),
            schema_digest: schema_digest(tool_name, input_schema),
            server_id: mcp_server_id(tool_name),
        }
    }
}

/// Server component of an `mcp__<server>__<tool>` dispatch name.
#[must_use]
pub fn mcp_server_id(tool_name: &str) -> Option<String> {
    let rest = tool_name.strip_prefix("mcp__")?;
    let (server, _tool) = rest.split_once("__")?;
    if server.is_empty() {
        None
    } else {
        Some(server.to_string())
    }
}

/// Hex SHA-256 over the tool name and its parameter surface.
///
/// The parameter surface is the sorted set of `properties` keys plus the
/// sorted set of `required` entries. Types, descriptions, titles, and examples
/// are excluded so a server that regenerates prose does not invalidate an
/// approval.
#[must_use]
pub fn schema_digest(tool_name: &str, input_schema: Option<&Value>) -> String {
    let mut properties: BTreeSet<&str> = BTreeSet::new();
    let mut required: BTreeSet<&str> = BTreeSet::new();
    if let Some(schema) = input_schema {
        if let Some(map) = schema.get("properties").and_then(Value::as_object) {
            for key in map.keys() {
                properties.insert(key.as_str());
            }
        }
        if let Some(list) = schema.get("required").and_then(Value::as_array) {
            for entry in list {
                if let Some(name) = entry.as_str() {
                    required.insert(name);
                }
            }
        }
    }
    let mut hasher = Sha256::new();
    hasher.update(tool_name.as_bytes());
    hasher.update([0]);
    for name in &properties {
        hasher.update(name.as_bytes());
        hasher.update([1]);
    }
    hasher.update([0]);
    for name in &required {
        hasher.update(name.as_bytes());
        hasher.update([1]);
    }
    format!("{:x}", hasher.finalize())
}

/// Compare a recorded contract with the identity computed from the live
/// registry. `Err` carries the refusal text handed to the model.
pub fn validate_identity(
    recorded: &ToolCallContract,
    live: Option<&ToolCallContract>,
) -> Result<(), String> {
    let recorded_digest = digest_label(&recorded.schema_digest).ok_or_else(|| {
        format!(
            "Refusing resumed tool call {}: tool \"{}\" has an invalid recorded schema digest.",
            recorded.call_id, recorded.tool_name
        )
    })?;
    let Some(live) = live else {
        return Err(format!(
            "Refusing resumed tool call {}: tool \"{}\" is no longer available from {}. \
             The call was recorded against schema {}.",
            recorded.call_id,
            recorded.tool_name,
            recorded.server_id.as_deref().unwrap_or("this workspace"),
            recorded_digest
        ));
    };
    if recorded.server_id != live.server_id {
        return Err(format!(
            "Refusing resumed tool call {}: tool \"{}\" now belongs to server {}, expected {}.",
            recorded.call_id,
            recorded.tool_name,
            live.server_id.as_deref().unwrap_or("<none>"),
            recorded.server_id.as_deref().unwrap_or("<none>")
        ));
    }
    if recorded.schema_digest != live.schema_digest {
        let live_digest = digest_label(&live.schema_digest).ok_or_else(|| {
            format!(
                "Refusing resumed tool call {}: tool \"{}\" has an invalid live schema digest.",
                recorded.call_id, recorded.tool_name
            )
        })?;
        return Err(format!(
            "Refusing resumed tool call {}: tool \"{}\" changed its parameters since this session was saved \
             (expected schema {}, found {}). Re-issue the call so it is approved against the current definition.",
            recorded.call_id, recorded.tool_name, recorded_digest, live_digest
        ));
    }
    Ok(())
}

fn digest_label(digest: &str) -> Option<&str> {
    (digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| &digest[..16])
}

/// Model-safe output emitted by a completed tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolOutput(String);

impl ToolOutput {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

/// Classified failure returned by a tool invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolError {
    Validation { message: String },
    Execution { message: String },
    Transport { message: String },
}

impl ToolError {
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::Validation { message }
            | Self::Execution { message }
            | Self::Transport { message } => message,
        }
    }
}

/// Stage at which a tool invocation was cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionPhase {
    Queued,
    Running,
}

/// Reason a requested tool was not allowed to execute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DenialReason {
    IdentityAuthority { message: String },
    User,
    SandboxPolicy { message: String },
    ActionFirewall { message: String },
}

impl DenialReason {
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::User => "Tool call was denied by user",
            Self::SandboxPolicy { message }
            | Self::ActionFirewall { message }
            | Self::IdentityAuthority { message } => message,
        }
    }
}

/// Semantic result of one tool invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolOutcome {
    Succeeded {
        output: ToolOutput,
    },
    Failed {
        error: ToolError,
        #[serde(skip_serializing_if = "Option::is_none")]
        partial_output: Option<ToolOutput>,
    },
    Denied {
        reason: DenialReason,
    },
    Cancelled {
        phase: ExecutionPhase,
    },
    /// The executor stopped without learning whether a remote write committed.
    Indeterminate {
        reason: String,
    },
}

/// Lifecycle classification persisted with a receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ExecutionStatus {
    Succeeded,
    Failed,
    Denied,
    Cancelled { phase: ExecutionPhase },
    Indeterminate,
}

impl ToolOutcome {
    #[must_use]
    pub fn status(&self) -> ExecutionStatus {
        match self {
            Self::Succeeded { .. } => ExecutionStatus::Succeeded,
            Self::Failed { .. } => ExecutionStatus::Failed,
            Self::Denied { .. } => ExecutionStatus::Denied,
            Self::Cancelled { phase } => ExecutionStatus::Cancelled { phase: *phase },
            Self::Indeterminate { .. } => ExecutionStatus::Indeterminate,
        }
    }
}

/// Where a tool result was produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionSource {
    Native,
    RemoteClient,
    Cache,
}

/// Legacy wire result of a tool execution.
///
/// This shape is intentionally permissive and remains the compatibility value
/// used by native, headless, and gateway adapters.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolResult {
    /// Whether the tool succeeded.
    pub success: bool,
    /// Complete or partial model-visible output.
    pub output: String,
    /// Error text when the tool failed.
    #[serde(default)]
    pub error: Option<String>,
    /// Tool-specific execution metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

impl ToolResult {
    /// Create a successful tool result.
    pub fn success(output: impl Into<String>) -> Self {
        Self {
            success: true,
            output: output.into(),
            ..Default::default()
        }
    }

    /// Create a failed tool result.
    pub fn failure(error: impl Into<String>) -> Self {
        Self {
            success: false,
            error: Some(error.into()),
            ..Default::default()
        }
    }

    /// Attach structured execution metadata.
    #[must_use]
    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }

    /// Whether the legacy details mark this result as cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.details
            .as_ref()
            .and_then(|details| details.get("cancelled"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    }
}

/// Token usage reported at an agent/provider boundary.
///
/// Field names intentionally preserve the existing snake-case JSON identity.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    #[serde(default)]
    pub cost: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_agent_and_tool_tags_round_trip() {
        let prompt = serde_json::to_value(ToAgent::Prompt {
            content: "hello".to_owned(),
            attachments: vec!["src/lib.rs".to_owned()],
        })
        .expect("prompt serializes");
        assert_eq!(prompt["type"], "prompt");
        assert_eq!(prompt["attachments"][0], "src/lib.rs");

        let result = ToolResult::success("ok").with_details(serde_json::json!({
            "exit_code": 0,
        }));
        let response = serde_json::to_value(ToAgent::ToolResponse {
            call_id: "call-1".to_owned(),
            approved: true,
            result: Some(result),
        })
        .expect("tool response serializes");
        assert_eq!(response["type"], "tool_response");
        assert_eq!(response["result"]["success"], true);
        assert_eq!(response["result"]["details"]["exit_code"], 0);

        let decoded: ToAgent = serde_json::from_value(response).expect("tool response decodes");
        assert!(matches!(
            decoded,
            ToAgent::ToolResponse { approved: true, .. }
        ));

        assert_eq!(
            serde_json::to_value(ToolResult::success("ok")).expect("legacy result serializes"),
            serde_json::json!({"success": true, "output": "ok", "error": null})
        );
    }

    #[test]
    fn shared_values_keep_wire_field_names() {
        let usage = serde_json::to_value(TokenUsage {
            input_tokens: 1,
            output_tokens: 2,
            cache_read_tokens: 3,
            cache_write_tokens: 4,
            cost: Some(0.5),
        })
        .expect("usage serializes");
        assert_eq!(
            usage,
            serde_json::json!({
                "input_tokens": 1,
                "output_tokens": 2,
                "cache_read_tokens": 3,
                "cache_write_tokens": 4,
                "cost": 0.5,
            })
        );

        let outcome = ToolOutcome::Cancelled {
            phase: ExecutionPhase::Running,
        };
        assert_eq!(
            serde_json::to_value(outcome).expect("outcome serializes"),
            serde_json::json!({"status": "cancelled", "phase": "running"})
        );
    }

    #[test]
    fn tool_call_contract_preserves_wire_identity_and_aliases() {
        let schema = serde_json::json!({
            "properties": {
                "path": {"type": "string", "description": "ignored prose"}
            },
            "required": ["path"]
        });
        let contract = ToolCallContract::record("call-1", "mcp__fs__read", Some(&schema));
        assert_eq!(contract.server_id.as_deref(), Some("fs"));

        let wire = serde_json::to_value(&contract).expect("contract serializes");
        assert_eq!(
            wire,
            serde_json::json!({
                "callId": "call-1",
                "toolName": "mcp__fs__read",
                "schemaDigest": contract.schema_digest.clone(),
                "serverId": "fs"
            })
        );

        let decoded: ToolCallContract = serde_json::from_value(serde_json::json!({
            "call_id": "call-1",
            "tool_name": "mcp__fs__read",
            "schema_digest": wire["schemaDigest"].clone(),
            "server_id": "fs"
        }))
        .expect("legacy aliases decode");
        assert_eq!(decoded, contract);
    }

    #[test]
    fn managed_authorization_is_validated_and_redacted() {
        let empty = ManagedInferenceAuthorization::new("");
        assert_eq!(
            empty.validate(),
            Err("managedInferenceAuthorization must not be empty")
        );
        let value = ManagedInferenceAuthorization::new("signed-capability");
        assert!(value.validate().is_ok());
        assert_eq!(value.as_str(), "signed-capability");
        assert_eq!(
            serde_json::to_value(&value).expect("authorization serializes"),
            serde_json::json!("signed-capability")
        );
        assert_eq!(
            format!("{value:?}"),
            "ManagedInferenceAuthorization([REDACTED])"
        );
    }

    #[test]
    fn managed_gateway_credential_expires_and_redacts_debug_output() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let usable = ManagedGatewayCredential::new("secret-marker", now + 60);
        assert!(usable.validate().is_ok());
        assert!(!format!("{usable:?}").contains("secret-marker"));
        assert!(
            ManagedGatewayCredential::new("secret-marker", now + 10)
                .validate()
                .is_err()
        );
        assert!(
            ManagedGatewayCredential::new("bad\nsecret", now + 60)
                .validate()
                .is_err()
        );
    }
}
