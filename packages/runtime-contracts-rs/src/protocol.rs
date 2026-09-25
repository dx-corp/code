//! Producer-owned semantics for the existing Maestro headless JSON protocol.
//!
//! This module deliberately describes the protocol without owning a transport.
//! The hosted runner, socket framing, and child-process lifecycle remain in
//! their current owners.  The typed projections here are the executable
//! contract that those owners can validate against while preserving the
//! existing wire format.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

/// Stable schema identity for the native headless protocol projection.
pub const HEADLESS_PROTOCOL_SCHEMA_VERSION: &str = "evalops.maestro.headless-protocol.v1";
/// Current headless JSON protocol version.
pub const HEADLESS_PROTOCOL_VERSION: &str = "2026-08-08";
/// Stable version of the response/turn terminal reducer semantics.
pub const HEADLESS_TERMINAL_REDUCER_VERSION: &str = "evalops.maestro.headless-terminal-reducer.v1";
/// Producer correlation labels used for turn-level terminal messages.
pub const HEADLESS_TURN_TERMINAL_RESPONSE_IDS: &[&str] = &["done", "continue"];
const HEADLESS_PROTOCOL_CAPABILITY_DIGEST_PREFIX: &str = "sha256:";
/// Versions accepted by this runtime at the compatibility boundary.
pub const SUPPORTED_HEADLESS_PROTOCOL_VERSIONS: &[&str] = &[HEADLESS_PROTOCOL_VERSION];

/// A tagged message whose type is newer than the current typed projection.
///
/// The raw JSON is retained so an adapter can emit an audit/receipt record
/// instead of silently converting an additive event into `None`.  Execution
/// paths may still reject this outcome when they cannot safely act on it.
#[derive(Clone, Debug, PartialEq)]
pub struct UnknownWireMessage {
    /// Unsupported wire tag supplied by the producer.
    pub type_name: String,
    /// Exact parsed JSON object, retained for audit and forward-compatibility
    /// handling.
    pub raw: Value,
}

/// Outcome of decoding a tagged message against an explicitly known type set.
#[derive(Clone, Debug, PartialEq)]
pub enum TaggedMessageDecode<T> {
    /// The message matched the current typed model.
    Known(T),
    /// The message was well-formed JSON with an additive, unsupported tag.
    Unknown(UnknownWireMessage),
}

/// Strict decoding failures for malformed JSON or a known tag with invalid
/// fields.  Unknown additive tags are not errors; they are represented by
/// [`TaggedMessageDecode::Unknown`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaggedMessageDecodeError {
    /// Input was not valid JSON.
    InvalidJson(String),
    /// Input was valid JSON but did not expose a string `type` tag.
    MissingType,
    /// A currently known tag failed typed deserialization.
    InvalidKnownMessage { type_name: String, error: String },
}

impl std::fmt::Display for TaggedMessageDecodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidJson(error) => write!(formatter, "invalid tagged message JSON: {error}"),
            Self::MissingType => formatter.write_str("tagged message is missing a string type"),
            Self::InvalidKnownMessage { type_name, error } => {
                write!(formatter, "invalid {type_name} message: {error}")
            }
        }
    }
}

impl std::error::Error for TaggedMessageDecodeError {}

/// Decodes one tagged JSON message without losing unsupported additive events.
///
/// `known_types` is owned by the producer-facing adapter and should be the
/// exact versioned projection for the message direction.  Unknown tags return
/// [`TaggedMessageDecode::Unknown`] with their raw object; a known tag with
/// invalid fields remains a strict error for execution safety.
pub fn decode_tagged_message<T>(
    raw: &str,
    known_types: &[&str],
) -> Result<TaggedMessageDecode<T>, TaggedMessageDecodeError>
where
    T: DeserializeOwned,
{
    let raw_value: Value = serde_json::from_str(raw)
        .map_err(|error| TaggedMessageDecodeError::InvalidJson(error.to_string()))?;
    let type_name = raw_value
        .get("type")
        .and_then(Value::as_str)
        .ok_or(TaggedMessageDecodeError::MissingType)?
        .to_string();
    if !known_types.contains(&type_name.as_str()) {
        return Ok(TaggedMessageDecode::Unknown(UnknownWireMessage {
            type_name,
            raw: raw_value,
        }));
    }
    serde_json::from_value(raw_value)
        .map(TaggedMessageDecode::Known)
        .map_err(|error| TaggedMessageDecodeError::InvalidKnownMessage {
            type_name,
            error: error.to_string(),
        })
}

/// Returns whether an optional client version can be served by this runtime.
///
/// `None` preserves the pre-negotiation behavior of the existing JSON
/// transport.  A present version must be explicitly listed in the producer's
/// supported-version set; the runtime does not silently downgrade its emitted
/// terminal messages.
#[must_use]
pub fn headless_protocol_version_is_supported(client_version: Option<&str>) -> bool {
    match client_version {
        None => true,
        Some(version) => SUPPORTED_HEADLESS_PROTOCOL_VERSIONS.contains(&version),
    }
}

/// Negotiated protocol identity returned after a compatible client handshake.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NegotiatedHeadlessProtocol {
    /// The exact producer version used for the connection.
    pub protocol_version: &'static str,
}

/// Failure returned when a client announces an unsupported protocol version.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnsupportedHeadlessProtocolVersion {
    /// Version announced by the client.
    pub client_version: String,
}

impl std::fmt::Display for UnsupportedHeadlessProtocolVersion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "unsupported client protocol version: {}; this agent speaks {}",
            self.client_version,
            SUPPORTED_HEADLESS_PROTOCOL_VERSIONS.join(", ")
        )
    }
}

impl std::error::Error for UnsupportedHeadlessProtocolVersion {}

/// Negotiates the producer-owned headless protocol version.
///
/// No wire transport is opened or changed by this function.  It only applies
/// the version rule that a handshake must use an explicitly supported version.
pub fn negotiate_headless_protocol(
    client_version: Option<&str>,
) -> Result<NegotiatedHeadlessProtocol, UnsupportedHeadlessProtocolVersion> {
    if headless_protocol_version_is_supported(client_version) {
        Ok(NegotiatedHeadlessProtocol {
            protocol_version: HEADLESS_PROTOCOL_VERSION,
        })
    } else {
        Err(UnsupportedHeadlessProtocolVersion {
            client_version: client_version.unwrap_or_default().to_string(),
        })
    }
}

/// Wire names sent from a client into the Maestro runtime.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToRuntimeMessageType {
    Hello,
    Init,
    GovernedInit,
    /// Private provider conversation restore command retained outside the
    /// generated public envelope.
    RestoreConversation,
    Prompt,
    GovernedPrompt,
    /// Legacy steering command retained outside the generated public
    /// envelope.
    Steer,
    GovernedSteer,
    Interrupt,
    ToolResponse,
    ClientToolResult,
    GovernedClientToolResult,
    ManagedAuthorizationResult,
    ApplyWorkspaceCapabilitySet,
    ConfigurePromptExperiment,
    ServerRequestResponse,
    UtilityCommandStart,
    UtilityCommandTerminate,
    UtilityCommandStdin,
    UtilityCommandResize,
    UtilityFileSearch,
    UtilityFileRead,
    UtilityFileWatchStart,
    UtilityFileWatchStop,
    Cancel,
    Shutdown,
}

/// Wire names emitted from the Maestro runtime to a client.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FromRuntimeMessageType {
    /// Private provider checkpoint retained for recovery and audit.
    ConversationSnapshot,
    HelloOk,
    Ready,
    ResponseStart,
    AssistantTextObserved,
    ResponseChunk,
    ResponseEnd,
    ToolCall,
    ToolStart,
    ToolOutput,
    ToolEnd,
    ClientToolRequest,
    GovernedClientToolRequest,
    ManagedAuthorizationRequest,
    ServerRequest,
    ServerRequestResolved,
    RawAgentEvent,
    UtilityCommandStarted,
    UtilityCommandResized,
    UtilityCommandOutput,
    UtilityCommandExited,
    UtilityFileSearchResults,
    UtilityFileReadResult,
    UtilityFileWatchStarted,
    UtilityFileWatchEvent,
    UtilityFileWatchStopped,
    Error,
    Status,
    Compaction,
    SessionInfo,
    ConnectionInfo,
    ResponseAccepted,
    TurnCompleted,
    TurnInterrupted,
    ProviderError,
    DelegationEvent,
    WorkspaceCapabilitySetApplied,
    /// Private Codex lifecycle projection retained outside the generated
    /// public envelope.
    CodexSessionState,
    /// Private Codex turn projection retained outside the generated public
    /// envelope.
    CodexTurnState,
    /// Private Codex usage projection retained outside the generated public
    /// envelope.
    CodexUsageState,
    /// Private Codex compatibility projection retained outside the generated
    /// public envelope.
    CodexCompatibility,
    /// Private managed-Gateway inference receipt retained outside the
    /// generated public envelope.
    ManagedGatewayReceipt,
    /// Private process usage checkpoint retained outside the generated public
    /// envelope.
    ProcessBudgetCheckpoint,
}

impl FromRuntimeMessageType {
    /// Reads the `type` tag of one runtime-to-client message.
    ///
    /// Returns `None` for a missing tag or a tag outside the current contract
    /// so adapters classify unknown additive events explicitly instead of
    /// matching visible strings.
    #[must_use]
    pub fn from_message(message: &Value) -> Option<Self> {
        let type_name = message.get("type").and_then(Value::as_str)?;
        serde_json::from_value(Value::String(type_name.to_string())).ok()
    }
}

/// Server-request capability names advertised by the runtime.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerRequestCapability {
    Approval,
    ClientTool,
    UserInput,
    ToolRetry,
}

/// Utility-plane operation names advertised by the runtime.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UtilityOperationCapability {
    CommandExec,
    FileSearch,
    FileWatch,
    FileRead,
}

/// Connection roles supported by the runtime.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionRoleCapability {
    Viewer,
    Controller,
}

/// Notification names that can be subscribed to on a headless connection.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationCapability {
    Status,
    Heartbeat,
    ConnectionInfo,
    Compaction,
}

/// Schema-only request capability retained in the generated projection but
/// intentionally not accepted by the current runtime JSON model.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaOnlyServerRequestCapability {
    McpElicitation,
}

/// Serialized names for every live client-to-runtime message model.
pub const HEADLESS_TO_RUNTIME_MESSAGE_NAMES: &[&str] = &[
    "hello",
    "init",
    "governed_init",
    "prompt",
    "governed_prompt",
    "governed_steer",
    "interrupt",
    "tool_response",
    "client_tool_result",
    "governed_client_tool_result",
    "managed_authorization_result",
    "apply_workspace_capability_set",
    "configure_prompt_experiment",
    "server_request_response",
    "utility_command_start",
    "utility_command_terminate",
    "utility_command_stdin",
    "utility_command_resize",
    "utility_file_search",
    "utility_file_read",
    "utility_file_watch_start",
    "utility_file_watch_stop",
    "cancel",
    "shutdown",
    "restore_conversation",
    "steer",
];

/// Serialized names for every live runtime-to-client message model.
pub const HEADLESS_FROM_RUNTIME_MESSAGE_NAMES: &[&str] = &[
    "hello_ok",
    "ready",
    "response_start",
    "assistant_text_observed",
    "response_chunk",
    "response_end",
    "tool_call",
    "tool_start",
    "tool_output",
    "tool_end",
    "client_tool_request",
    "governed_client_tool_request",
    "managed_authorization_request",
    "server_request",
    "server_request_resolved",
    "raw_agent_event",
    "utility_command_started",
    "utility_command_resized",
    "utility_command_output",
    "utility_command_exited",
    "utility_file_search_results",
    "utility_file_read_result",
    "utility_file_watch_started",
    "utility_file_watch_event",
    "utility_file_watch_stopped",
    "error",
    "status",
    "compaction",
    "session_info",
    "connection_info",
    "response_accepted",
    "turn_completed",
    "turn_interrupted",
    "provider_error",
    "delegation_event",
    "workspace_capability_set_applied",
    "conversation_snapshot",
    "codex_session_state",
    "codex_turn_state",
    "codex_usage_state",
    "codex_compatibility",
    "managed_gateway_receipt",
    "process_budget_checkpoint",
];

/// Serialized names retained by the JSON compatibility edge but absent from
/// the generated public protobuf envelope.
pub const HEADLESS_RUNTIME_ONLY_TO_RUNTIME_MESSAGE_NAMES: &[&str] =
    &["restore_conversation", "steer"];
/// Serialized names retained for recovery/lifecycle compatibility but absent
/// from the generated public protobuf envelope.
pub const HEADLESS_RUNTIME_ONLY_FROM_RUNTIME_MESSAGE_NAMES: &[&str] = &[
    "conversation_snapshot",
    "codex_session_state",
    "codex_turn_state",
    "codex_usage_state",
    "codex_compatibility",
    "managed_gateway_receipt",
    "process_budget_checkpoint",
];

/// Typed capability projection used by the producer-owned protocol contract.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HeadlessCapabilityProjection {
    /// Requests the runtime may send to a client.
    pub server_requests: &'static [ServerRequestCapability],
    /// Schema-declared requests deliberately not accepted by this runtime
    /// version.  They remain visible for compatibility and audit.
    pub schema_only_server_requests: &'static [SchemaOnlyServerRequestCapability],
    /// Utility operations the runtime accepts from a client.
    pub utility_operations: &'static [UtilityOperationCapability],
    /// Roles that can be negotiated for a connection.
    pub connection_roles: &'static [ConnectionRoleCapability],
    /// Notifications available to subscriptions.
    pub notifications: &'static [NotificationCapability],
}

/// Serialized projection of the terminal-reduction rules used by the
/// producer-owned runtime contract.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HeadlessTerminalProjection {
    /// Stable version of these terminal-reduction semantics.
    pub version: &'static str,
    /// Whether `ResponseEnd` is itself terminal.  The current reducer keeps
    /// it non-terminal until an explicit turn terminal arrives.
    pub response_end_terminal: bool,
    /// Whether a turn must receive an explicit completion, interruption, or
    /// provider failure after response output ends.
    pub explicit_turn_terminal_required: bool,
    /// Whether the first terminal event fences later terminal events.
    pub first_terminal_wins: bool,
    /// Whether events for a response other than the active response are
    /// ignored as stale.  A new response start within the same turn rotates
    /// the active response identity after tool work.
    pub stale_response_events_ignored: bool,
    /// Whether one turn may contain multiple response stream identities,
    /// as happens when the direct provider loop pauses for tool execution.
    pub response_rotation_allowed: bool,
    /// Sentinel response labels that close the active turn rather than a
    /// particular response stream.
    pub turn_terminal_response_ids: &'static [&'static str],
    /// Whether explicit turn terminals are correlated to the active turn
    /// rather than requiring their producer correlation label to equal the
    /// last response stream identity.
    pub turn_terminal_response_id_independent: bool,
    /// Whether a fatal error is classified as an interruption for compatibility
    /// with the existing hosted-thread reducer.
    pub fatal_errors_are_interruptions: bool,
}

/// Complete typed projection of the current headless compatibility contract.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HeadlessProtocolContract {
    /// Stable schema identity for this projection.
    pub schema_version: &'static str,
    /// Exact protocol version emitted by this runtime.
    pub protocol_version: &'static str,
    /// Versioned terminal-reduction semantics applied to response/turn
    /// events.  This is a pre-persistence contract projection; it does not
    /// transfer durable terminal acceptance or receipt ownership to Maestro.
    pub terminal_reducer: HeadlessTerminalProjection,
    /// Client-to-runtime message names.
    pub to_runtime_messages: &'static [ToRuntimeMessageType],
    /// Runtime-to-client message names.
    pub from_runtime_messages: &'static [FromRuntimeMessageType],
    /// Client-to-runtime compatibility messages outside the generated public
    /// envelope.
    pub runtime_only_to_runtime_messages: &'static [ToRuntimeMessageType],
    /// Runtime-to-client compatibility messages outside the generated public
    /// envelope.
    pub runtime_only_from_runtime_messages: &'static [FromRuntimeMessageType],
    /// Capability names and roles used by the handshake.
    pub capabilities: HeadlessCapabilityProjection,
}

const TO_RUNTIME_MESSAGES: &[ToRuntimeMessageType] = &[
    ToRuntimeMessageType::Hello,
    ToRuntimeMessageType::Init,
    ToRuntimeMessageType::GovernedInit,
    ToRuntimeMessageType::Prompt,
    ToRuntimeMessageType::GovernedPrompt,
    ToRuntimeMessageType::GovernedSteer,
    ToRuntimeMessageType::Interrupt,
    ToRuntimeMessageType::ToolResponse,
    ToRuntimeMessageType::ClientToolResult,
    ToRuntimeMessageType::GovernedClientToolResult,
    ToRuntimeMessageType::ManagedAuthorizationResult,
    ToRuntimeMessageType::ApplyWorkspaceCapabilitySet,
    ToRuntimeMessageType::ConfigurePromptExperiment,
    ToRuntimeMessageType::ServerRequestResponse,
    ToRuntimeMessageType::UtilityCommandStart,
    ToRuntimeMessageType::UtilityCommandTerminate,
    ToRuntimeMessageType::UtilityCommandStdin,
    ToRuntimeMessageType::UtilityCommandResize,
    ToRuntimeMessageType::UtilityFileSearch,
    ToRuntimeMessageType::UtilityFileRead,
    ToRuntimeMessageType::UtilityFileWatchStart,
    ToRuntimeMessageType::UtilityFileWatchStop,
    ToRuntimeMessageType::Cancel,
    ToRuntimeMessageType::Shutdown,
    ToRuntimeMessageType::RestoreConversation,
    ToRuntimeMessageType::Steer,
];

const FROM_RUNTIME_MESSAGES: &[FromRuntimeMessageType] = &[
    FromRuntimeMessageType::HelloOk,
    FromRuntimeMessageType::Ready,
    FromRuntimeMessageType::ResponseStart,
    FromRuntimeMessageType::AssistantTextObserved,
    FromRuntimeMessageType::ResponseChunk,
    FromRuntimeMessageType::ResponseEnd,
    FromRuntimeMessageType::ToolCall,
    FromRuntimeMessageType::ToolStart,
    FromRuntimeMessageType::ToolOutput,
    FromRuntimeMessageType::ToolEnd,
    FromRuntimeMessageType::ClientToolRequest,
    FromRuntimeMessageType::GovernedClientToolRequest,
    FromRuntimeMessageType::ManagedAuthorizationRequest,
    FromRuntimeMessageType::ServerRequest,
    FromRuntimeMessageType::ServerRequestResolved,
    FromRuntimeMessageType::RawAgentEvent,
    FromRuntimeMessageType::UtilityCommandStarted,
    FromRuntimeMessageType::UtilityCommandResized,
    FromRuntimeMessageType::UtilityCommandOutput,
    FromRuntimeMessageType::UtilityCommandExited,
    FromRuntimeMessageType::UtilityFileSearchResults,
    FromRuntimeMessageType::UtilityFileReadResult,
    FromRuntimeMessageType::UtilityFileWatchStarted,
    FromRuntimeMessageType::UtilityFileWatchEvent,
    FromRuntimeMessageType::UtilityFileWatchStopped,
    FromRuntimeMessageType::Error,
    FromRuntimeMessageType::Status,
    FromRuntimeMessageType::Compaction,
    FromRuntimeMessageType::SessionInfo,
    FromRuntimeMessageType::ConnectionInfo,
    FromRuntimeMessageType::ResponseAccepted,
    FromRuntimeMessageType::TurnCompleted,
    FromRuntimeMessageType::TurnInterrupted,
    FromRuntimeMessageType::ProviderError,
    FromRuntimeMessageType::DelegationEvent,
    FromRuntimeMessageType::WorkspaceCapabilitySetApplied,
    FromRuntimeMessageType::ConversationSnapshot,
    FromRuntimeMessageType::CodexSessionState,
    FromRuntimeMessageType::CodexTurnState,
    FromRuntimeMessageType::CodexUsageState,
    FromRuntimeMessageType::CodexCompatibility,
    FromRuntimeMessageType::ManagedGatewayReceipt,
    FromRuntimeMessageType::ProcessBudgetCheckpoint,
];

const SERVER_REQUEST_CAPABILITIES: &[ServerRequestCapability] = &[
    ServerRequestCapability::Approval,
    ServerRequestCapability::ClientTool,
    ServerRequestCapability::UserInput,
    ServerRequestCapability::ToolRetry,
];
const SCHEMA_ONLY_SERVER_REQUEST_CAPABILITIES: &[SchemaOnlyServerRequestCapability] =
    &[SchemaOnlyServerRequestCapability::McpElicitation];
const UTILITY_OPERATION_CAPABILITIES: &[UtilityOperationCapability] = &[
    UtilityOperationCapability::CommandExec,
    UtilityOperationCapability::FileSearch,
    UtilityOperationCapability::FileWatch,
    UtilityOperationCapability::FileRead,
];
const CONNECTION_ROLE_CAPABILITIES: &[ConnectionRoleCapability] = &[
    ConnectionRoleCapability::Viewer,
    ConnectionRoleCapability::Controller,
];
const NOTIFICATION_CAPABILITIES: &[NotificationCapability] = &[
    NotificationCapability::Status,
    NotificationCapability::Heartbeat,
    NotificationCapability::ConnectionInfo,
    NotificationCapability::Compaction,
];

/// Returns the producer-owned typed projection used to validate generated
/// protocol surfaces and checked-in fixtures.
#[must_use]
pub const fn headless_protocol_contract() -> HeadlessProtocolContract {
    HeadlessProtocolContract {
        schema_version: HEADLESS_PROTOCOL_SCHEMA_VERSION,
        protocol_version: HEADLESS_PROTOCOL_VERSION,
        terminal_reducer: HeadlessTerminalProjection {
            version: HEADLESS_TERMINAL_REDUCER_VERSION,
            response_end_terminal: false,
            explicit_turn_terminal_required: true,
            first_terminal_wins: true,
            stale_response_events_ignored: true,
            response_rotation_allowed: true,
            turn_terminal_response_ids: HEADLESS_TURN_TERMINAL_RESPONSE_IDS,
            turn_terminal_response_id_independent: true,
            fatal_errors_are_interruptions: true,
        },
        to_runtime_messages: TO_RUNTIME_MESSAGES,
        from_runtime_messages: FROM_RUNTIME_MESSAGES,
        runtime_only_to_runtime_messages: &[
            ToRuntimeMessageType::RestoreConversation,
            ToRuntimeMessageType::Steer,
        ],
        runtime_only_from_runtime_messages: &[
            FromRuntimeMessageType::ConversationSnapshot,
            FromRuntimeMessageType::CodexSessionState,
            FromRuntimeMessageType::CodexTurnState,
            FromRuntimeMessageType::CodexUsageState,
            FromRuntimeMessageType::CodexCompatibility,
            FromRuntimeMessageType::ManagedGatewayReceipt,
            FromRuntimeMessageType::ProcessBudgetCheckpoint,
        ],
        capabilities: HeadlessCapabilityProjection {
            server_requests: SERVER_REQUEST_CAPABILITIES,
            schema_only_server_requests: SCHEMA_ONLY_SERVER_REQUEST_CAPABILITIES,
            utility_operations: UTILITY_OPERATION_CAPABILITIES,
            connection_roles: CONNECTION_ROLE_CAPABILITIES,
            notifications: NOTIFICATION_CAPABILITIES,
        },
    }
}

/// Recursively sort object keys before serializing the producer contract.
///
/// The compatibility manifest uses the same compact, UTF-8 JSON projection
/// with lexicographically sorted object keys. The current typed contract only
/// contains strings, booleans, arrays, and objects, so serde_json's stable
/// primitive encoding is sufficient for this shared RFC8785-style boundary.
fn canonicalize_json_for_digest(value: &Value) -> Value {
    match value {
        Value::Array(values) => {
            Value::Array(values.iter().map(canonicalize_json_for_digest).collect())
        }
        Value::Object(object) => {
            let mut entries: Vec<_> = object.iter().collect();
            entries.sort_unstable_by_key(|(key, _)| *key);
            let mut canonical = serde_json::Map::new();
            for (key, value) in entries {
                canonical.insert(key.clone(), canonicalize_json_for_digest(value));
            }
            Value::Object(canonical)
        }
        primitive => primitive.clone(),
    }
}

fn canonical_json_for_digest(value: &Value) -> Vec<u8> {
    serde_json::to_vec(&canonicalize_json_for_digest(value))
        .expect("producer-owned protocol contract must serialize canonically")
}

/// Returns the digest of the producer-owned serialized protocol projection.
///
/// Hosted lifecycle receipts include this value so Platform can compare the
/// capability semantics that produced the evidence without relying on a
/// manually mirrored adapter list.
#[must_use]
pub fn headless_protocol_capability_digest() -> String {
    let contract = serde_json::to_value(headless_protocol_contract())
        .expect("producer-owned protocol contract must serialize");
    format!(
        "{}{:x}",
        HEADLESS_PROTOCOL_CAPABILITY_DIGEST_PREFIX,
        Sha256::digest(canonical_json_for_digest(&contract))
    )
}

/// Events that affect the terminal state of one response/turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TerminalEvent {
    /// A new response stream segment began within the active turn.
    ///
    /// The direct provider loop may emit multiple response identities around
    /// tool execution; each new identity replaces the active response fence.
    ResponseStarted { response_id: String },
    /// Model output ended, but the turn still requires an explicit terminal.
    ResponseEnded { response_id: String },
    /// The runtime accepted the turn as successfully complete.
    ///
    /// This is turn-scoped: `response_id` is a producer correlation label and
    /// does not have to equal the last response stream identity.
    TurnCompleted { response_id: String },
    /// The runtime interrupted the turn with a durable reason.
    ///
    /// This is turn-scoped: `response_id` is a producer correlation label and
    /// does not have to equal the last response stream identity.
    TurnInterrupted { response_id: String, reason: String },
    /// The provider boundary declared a terminal failure.
    ProviderFailed {
        response_id: Option<String>,
        kind: TerminalErrorKind,
        message: String,
    },
    /// A protocol error that may or may not terminate the turn.
    Error {
        response_id: Option<String>,
        fatal: bool,
        terminal: bool,
        kind: Option<TerminalErrorKind>,
        message: String,
    },
}

/// Error categories relevant to terminal reduction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalErrorKind {
    /// Recoverable provider or transport error.
    Transient,
    /// Runtime-declared fatal error.
    Fatal,
    /// Tool execution failure that is terminal for the turn.
    Tool,
    /// Explicit cancellation or interruption.
    Cancelled,
    /// Protocol validation or framing failure.
    Protocol,
    /// Provider stream ended before its required terminal protocol event.
    TransientProtocol,
    /// Provider ended because the configured output token budget was
    /// exhausted.
    OutputTokenExhaustion,
    /// Provider reported another typed incomplete-response reason.
    IncompleteResponse,
    /// Provider emitted an explicit failed terminal event.
    ProviderDeclaredFailure,
}

/// Durable terminal classification for one response/turn.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TerminalStatus {
    /// No response is active.
    #[default]
    Idle,
    /// Response chunks may still arrive.
    Streaming,
    /// Response output ended, but `TurnCompleted` or another terminal is
    /// still required.  `ResponseEnd` is intentionally not terminal.
    AwaitingTurnTerminal,
    /// The turn completed successfully.
    Completed,
    /// The turn was interrupted or cancelled.
    Interrupted,
    /// The turn failed at a terminal boundary.
    Failed,
}

impl TerminalStatus {
    /// Returns whether this status is terminal for the current reducer.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Interrupted | Self::Failed)
    }
}

/// Result of applying one event to the terminal reducer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TerminalTransition {
    /// The event changed the durable classification.
    Applied {
        previous: TerminalStatus,
        current: TerminalStatus,
    },
    /// A second terminal cannot overwrite the first terminal evidence.
    IgnoredAfterTerminal { status: TerminalStatus },
    /// An event for another response was rejected as stale.
    IgnoredStaleResponse {
        expected: Option<String>,
        received: String,
    },
    /// A non-terminal error was observed without changing terminal state.
    IgnoredNonTerminalError,
}

/// Reducer for the producer-owned response/turn terminal semantics.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TerminalReducer {
    status: TerminalStatus,
    response_id: Option<String>,
    seen_response_ids: BTreeSet<String>,
}

impl TerminalReducer {
    /// Creates a reducer at the start of a response/turn.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            status: TerminalStatus::Idle,
            response_id: None,
            seen_response_ids: BTreeSet::new(),
        }
    }

    /// Returns the current terminal classification.
    #[must_use]
    pub const fn status(&self) -> TerminalStatus {
        self.status
    }

    /// Returns the response identity currently fenced by this reducer.
    #[must_use]
    pub fn response_id(&self) -> Option<&str> {
        self.response_id.as_deref()
    }

    /// Applies one producer event while preserving first-terminal evidence.
    ///
    /// `ResponseEnd` only moves to [`TerminalStatus::AwaitingTurnTerminal`];
    /// it does not accept the turn.  Platform-facing persistence can therefore
    /// distinguish streamed output from the later terminal decision.
    pub fn apply(&mut self, event: TerminalEvent) -> TerminalTransition {
        if self.status.is_terminal() {
            return TerminalTransition::IgnoredAfterTerminal {
                status: self.status,
            };
        }

        let (response_id, next_status) = match event {
            TerminalEvent::ResponseStarted { response_id } => {
                if self.response_id.as_deref() != Some(response_id.as_str())
                    && self.seen_response_ids.contains(&response_id)
                {
                    return self.stale(response_id);
                }
                self.seen_response_ids.insert(response_id.clone());
                (Some(response_id), TerminalStatus::Streaming)
            }
            TerminalEvent::ResponseEnded { response_id } => {
                if !is_turn_terminal_response_id(&response_id)
                    && !self.matches_response(&response_id)
                {
                    return self.stale(response_id);
                }
                (
                    if is_turn_terminal_response_id(&response_id) {
                        self.response_id.clone()
                    } else {
                        Some(response_id)
                    },
                    TerminalStatus::AwaitingTurnTerminal,
                )
            }
            TerminalEvent::TurnCompleted { .. } => {
                (self.response_id.clone(), TerminalStatus::Completed)
            }
            TerminalEvent::TurnInterrupted { .. } => {
                (self.response_id.clone(), TerminalStatus::Interrupted)
            }
            TerminalEvent::ProviderFailed {
                response_id, kind, ..
            } => {
                if let Some(response_id) = response_id {
                    if !self.matches_response(&response_id) {
                        return self.stale(response_id);
                    }
                    (Some(response_id), TerminalStatus::Failed)
                } else {
                    (
                        self.response_id.clone(),
                        terminal_status_for_error(kind, false),
                    )
                }
            }
            TerminalEvent::Error {
                response_id,
                fatal,
                terminal,
                kind,
                ..
            } => {
                if !fatal && !terminal {
                    return TerminalTransition::IgnoredNonTerminalError;
                }
                if let Some(response_id) = response_id {
                    if !self.matches_response(&response_id) {
                        return self.stale(response_id);
                    }
                    (
                        Some(response_id),
                        terminal_status_for_error(
                            kind.unwrap_or(TerminalErrorKind::Protocol),
                            fatal,
                        ),
                    )
                } else {
                    (
                        self.response_id.clone(),
                        terminal_status_for_error(
                            kind.unwrap_or(TerminalErrorKind::Protocol),
                            fatal,
                        ),
                    )
                }
            }
        };

        let previous = self.status;
        self.response_id = response_id;
        self.status = next_status;
        TerminalTransition::Applied {
            previous,
            current: next_status,
        }
    }

    fn matches_response(&self, response_id: &str) -> bool {
        self.response_id
            .as_deref()
            .is_none_or(|expected| expected == response_id)
    }

    fn stale(&self, received: String) -> TerminalTransition {
        TerminalTransition::IgnoredStaleResponse {
            expected: self.response_id.clone(),
            received,
        }
    }
}

fn terminal_status_for_error(kind: TerminalErrorKind, fatal: bool) -> TerminalStatus {
    if fatal || kind == TerminalErrorKind::Cancelled {
        TerminalStatus::Interrupted
    } else {
        TerminalStatus::Failed
    }
}

fn is_turn_terminal_response_id(response_id: &str) -> bool {
    HEADLESS_TURN_TERMINAL_RESPONSE_IDS.contains(&response_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_negotiation_is_explicit_and_does_not_downgrade() {
        assert!(headless_protocol_version_is_supported(None));
        assert!(headless_protocol_version_is_supported(Some(
            HEADLESS_PROTOCOL_VERSION
        )));
        assert!(!headless_protocol_version_is_supported(Some("2026-08-07")));
        assert_eq!(
            negotiate_headless_protocol(Some(HEADLESS_PROTOCOL_VERSION))
                .expect("current version should negotiate")
                .protocol_version,
            HEADLESS_PROTOCOL_VERSION
        );
        assert_eq!(
            negotiate_headless_protocol(Some("old"))
                .expect_err("unknown version must be rejected")
                .client_version,
            "old"
        );
    }

    #[test]
    fn contract_publishes_terminal_reducer_semantics() {
        let projection = headless_protocol_contract().terminal_reducer;
        assert_eq!(projection.version, HEADLESS_TERMINAL_REDUCER_VERSION);
        assert!(!projection.response_end_terminal);
        assert!(projection.explicit_turn_terminal_required);
        assert!(projection.first_terminal_wins);
        assert!(projection.stale_response_events_ignored);
        assert!(projection.response_rotation_allowed);
        assert_eq!(
            projection.turn_terminal_response_ids,
            HEADLESS_TURN_TERMINAL_RESPONSE_IDS
        );
        assert!(projection.turn_terminal_response_id_independent);
        assert!(projection.fatal_errors_are_interruptions);
    }

    #[test]
    fn terminal_reducer_requires_explicit_turn_terminal_after_response_end() {
        let mut reducer = TerminalReducer::new();
        assert_eq!(
            reducer.apply(TerminalEvent::ResponseStarted {
                response_id: "response-1".into(),
            }),
            TerminalTransition::Applied {
                previous: TerminalStatus::Idle,
                current: TerminalStatus::Streaming,
            }
        );
        assert_eq!(
            reducer.apply(TerminalEvent::ResponseEnded {
                response_id: "response-1".into(),
            }),
            TerminalTransition::Applied {
                previous: TerminalStatus::Streaming,
                current: TerminalStatus::AwaitingTurnTerminal,
            }
        );
        assert!(!reducer.status().is_terminal());
        assert_eq!(
            reducer.apply(TerminalEvent::TurnCompleted {
                response_id: "response-1".into(),
            }),
            TerminalTransition::Applied {
                previous: TerminalStatus::AwaitingTurnTerminal,
                current: TerminalStatus::Completed,
            }
        );
    }

    #[test]
    fn terminal_reducer_fences_stale_and_late_events() {
        let mut reducer = TerminalReducer::new();
        reducer.apply(TerminalEvent::ResponseStarted {
            response_id: "response-1".into(),
        });
        assert!(matches!(
            reducer.apply(TerminalEvent::ResponseEnded {
                response_id: "response-2".into(),
            }),
            TerminalTransition::IgnoredStaleResponse { .. }
        ));
        reducer.apply(TerminalEvent::TurnCompleted {
            response_id: "response-1".into(),
        });
        assert_eq!(
            reducer.apply(TerminalEvent::TurnInterrupted {
                response_id: "response-1".into(),
                reason: "late cancellation".into(),
            }),
            TerminalTransition::IgnoredAfterTerminal {
                status: TerminalStatus::Completed,
            }
        );
    }

    #[test]
    fn terminal_reducer_allows_tool_turn_response_rotation_and_synthetic_terminal_label() {
        let mut reducer = TerminalReducer::new();
        reducer.apply(TerminalEvent::ResponseStarted {
            response_id: "generated-response-1".into(),
        });
        reducer.apply(TerminalEvent::ResponseEnded {
            response_id: "generated-response-1".into(),
        });
        assert_eq!(
            reducer.apply(TerminalEvent::ResponseStarted {
                response_id: "generated-response-2".into(),
            }),
            TerminalTransition::Applied {
                previous: TerminalStatus::AwaitingTurnTerminal,
                current: TerminalStatus::Streaming,
            }
        );
        assert!(matches!(
            reducer.apply(TerminalEvent::ResponseEnded {
                response_id: "generated-response-1".into(),
            }),
            TerminalTransition::IgnoredStaleResponse {
                expected: Some(expected),
                received
            } if expected == "generated-response-2" && received == "generated-response-1"
        ));
        reducer.apply(TerminalEvent::ResponseEnded {
            response_id: "generated-response-2".into(),
        });
        assert_eq!(
            reducer.apply(TerminalEvent::TurnCompleted {
                response_id: "done".into(),
            }),
            TerminalTransition::Applied {
                previous: TerminalStatus::AwaitingTurnTerminal,
                current: TerminalStatus::Completed,
            }
        );
        assert_eq!(reducer.response_id(), Some("generated-response-2"));
    }

    #[test]
    fn terminal_reducer_accepts_native_done_response_end_before_turn_terminal() {
        let mut reducer = TerminalReducer::new();
        reducer.apply(TerminalEvent::ResponseStarted {
            response_id: "native-generated-uuid".into(),
        });
        assert_eq!(
            reducer.apply(TerminalEvent::ResponseEnded {
                response_id: "done".into(),
            }),
            TerminalTransition::Applied {
                previous: TerminalStatus::Streaming,
                current: TerminalStatus::AwaitingTurnTerminal,
            }
        );
        assert_eq!(
            reducer.apply(TerminalEvent::TurnCompleted {
                response_id: "done".into(),
            }),
            TerminalTransition::Applied {
                previous: TerminalStatus::AwaitingTurnTerminal,
                current: TerminalStatus::Completed,
            }
        );
        assert_eq!(reducer.response_id(), Some("native-generated-uuid"));
    }

    #[test]
    fn non_terminal_errors_do_not_erase_streaming_state() {
        let mut reducer = TerminalReducer::new();
        reducer.apply(TerminalEvent::ResponseStarted {
            response_id: "response-1".into(),
        });
        assert_eq!(
            reducer.apply(TerminalEvent::Error {
                response_id: Some("response-1".into()),
                fatal: false,
                terminal: false,
                kind: Some(TerminalErrorKind::Transient),
                message: "retryable".into(),
            }),
            TerminalTransition::IgnoredNonTerminalError
        );
        assert_eq!(reducer.status(), TerminalStatus::Streaming);
    }

    #[test]
    fn fatal_error_flag_preserves_existing_interruption_classification() {
        let mut reducer = TerminalReducer::new();
        assert_eq!(
            reducer.apply(TerminalEvent::Error {
                response_id: None,
                fatal: true,
                terminal: true,
                kind: Some(TerminalErrorKind::Fatal),
                message: "fatal runtime error".into(),
            }),
            TerminalTransition::Applied {
                previous: TerminalStatus::Idle,
                current: TerminalStatus::Interrupted,
            }
        );

        let mut non_fatal = TerminalReducer::new();
        assert_eq!(
            non_fatal.apply(TerminalEvent::Error {
                response_id: None,
                fatal: false,
                terminal: true,
                kind: Some(TerminalErrorKind::Fatal),
                message: "fatal category without fatal flag".into(),
            }),
            TerminalTransition::Applied {
                previous: TerminalStatus::Idle,
                current: TerminalStatus::Failed,
            }
        );
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct KnownMessage {
        value: String,
    }

    #[test]
    fn unknown_additive_tags_remain_auditable_while_known_tags_stay_strict() {
        let unknown = decode_tagged_message::<KnownMessage>(
            r#"{"type":"future_event","value":"preserve-me","receipt":{"id":"r-1"}}"#,
            &["known"],
        )
        .expect("unknown additive tags are a representable outcome");
        assert_eq!(
            unknown,
            TaggedMessageDecode::Unknown(UnknownWireMessage {
                type_name: "future_event".into(),
                raw: serde_json::json!({
                    "type": "future_event",
                    "value": "preserve-me",
                    "receipt": {"id": "r-1"}
                }),
            })
        );

        let known_error =
            decode_tagged_message::<KnownMessage>(r#"{"type":"known","value":7}"#, &["known"])
                .expect_err("known malformed messages remain strict errors");
        assert!(matches!(
            known_error,
            TaggedMessageDecodeError::InvalidKnownMessage { ref type_name, .. }
                if type_name == "known"
        ));
    }

    #[test]
    fn from_message_reads_known_tags_and_rejects_unknown_or_missing_tags() {
        for (index, name) in HEADLESS_FROM_RUNTIME_MESSAGE_NAMES.iter().enumerate() {
            let typed = FromRuntimeMessageType::from_message(&serde_json::json!({
                "type": name,
                "payload": index,
            }))
            .unwrap_or_else(|| panic!("{name} must decode as a known runtime message"));
            assert_eq!(
                serde_json::to_value(typed).expect("message type serializes"),
                serde_json::Value::String((*name).to_string())
            );
        }
        assert_eq!(
            FromRuntimeMessageType::from_message(&serde_json::json!({"type": "future_event"})),
            None
        );
        assert_eq!(
            FromRuntimeMessageType::from_message(&serde_json::json!({"type": ""})),
            None
        );
        assert_eq!(
            FromRuntimeMessageType::from_message(&serde_json::json!({"type": 7})),
            None
        );
        assert_eq!(
            FromRuntimeMessageType::from_message(&serde_json::json!({"payload": {}})),
            None
        );
        assert_eq!(
            FROM_RUNTIME_MESSAGES.len(),
            HEADLESS_FROM_RUNTIME_MESSAGE_NAMES.len()
        );
    }

    #[test]
    fn decoder_tags_match_the_advertised_typed_contract() {
        let contract = serde_json::to_value(headless_protocol_contract()).unwrap();
        assert_eq!(
            contract["fromRuntimeMessages"],
            serde_json::json!(HEADLESS_FROM_RUNTIME_MESSAGE_NAMES)
        );
        assert_eq!(
            contract["runtimeOnlyFromRuntimeMessages"],
            serde_json::json!(HEADLESS_RUNTIME_ONLY_FROM_RUNTIME_MESSAGE_NAMES)
        );
    }

    #[test]
    fn checked_in_fixture_matches_typed_contract() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../fixtures/headless-protocol-v1.json"))
                .expect("protocol fixture must be valid JSON");
        let contract = serde_json::to_value(headless_protocol_contract())
            .expect("typed protocol contract must serialize");
        assert_eq!(fixture, contract);
    }

    #[test]
    fn capability_digest_matches_canonical_checked_in_fixture() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../fixtures/headless-protocol-v1.json"))
                .expect("protocol fixture must be valid JSON");
        let expected = format!(
            "{HEADLESS_PROTOCOL_CAPABILITY_DIGEST_PREFIX}{:x}",
            Sha256::digest(canonical_json_for_digest(&fixture))
        );
        assert_eq!(headless_protocol_capability_digest(), expected);
        assert_eq!(
            headless_protocol_capability_digest(),
            "sha256:8353f3612f7e7f4f7f5c55020b2c255f7ad52b57417a027ae66ceb657c402ba8"
        );
    }
}
