//! The transport-neutral native Maestro runtime product boundary.
//!
//! This crate records the identity and process topology of the native hosted
//! runtime. It intentionally does not own a socket, child process, database,
//! tenant lease, or retry loop. Those concerns remain in their existing owners
//! while callers adopt the boundary incrementally.

#![forbid(unsafe_code)]

mod boundary;
pub mod cache_topology;
pub mod contracts;
pub mod default_models;
pub mod delegation;
mod launch_spec;
pub mod operation_observation;
pub mod passport;
pub mod protocol;
pub mod receipt_contract;
mod receipts;
mod session_event;
pub mod telemetry;
pub mod tool_details;
pub mod tool_operation;
pub mod tool_wire;
pub mod turn_outcome;

pub use boundary::{
    HostedRuntimeAuthMode, HostedRuntimeBoundary, HostedRuntimeBoundaryInput, RuntimeBoundaryError,
};
pub use contracts::{
    DenialReason, ExecutionPhase, ExecutionSource, ExecutionStatus,
    MAX_MANAGED_INFERENCE_AUTHORIZATION_BYTES, ManagedInferenceAuthorization, ToAgent, TokenUsage,
    ToolCallContract, ToolError, ToolOutcome, ToolOutput, ToolResult, ValidatorResult,
    mcp_server_id, schema_digest, validate_identity,
};
pub use default_models::DefaultModel;
pub use delegation::{
    DELEGATION_OWNER_PROJECTION_METADATA_KEY, DELEGATION_PROJECTION_SCHEMA_VERSION,
    DelegationControlAction, DelegationControlProjection, DelegationControlState, DelegationEvent,
    DelegationEventKind, DelegationLifecycleState,
};
pub use launch_spec::{
    HOSTED_LAUNCH_SPEC_VERSION, HostedLaunchIdentity, HostedLaunchModelContract,
    HostedLaunchRendezvous, HostedLaunchRendezvousMode, HostedLaunchRestoreIntent,
    HostedLaunchRuntime, HostedLaunchSecretFileRefs, HostedLaunchSpec, HostedLaunchSpecError,
    HostedLaunchSpecInput, HostedLaunchWorkloadIdentity, HostedLaunchWorkspace,
};
pub use passport::{
    RUNTIME_CONFORMANCE_PROFILE, RUNTIME_CONFORMANCE_VERSION, RUNTIME_PASSPORT_PREDICATE_TYPE,
    RUNTIME_PASSPORT_VERSION, RuntimeArtifactIdentity, RuntimeArtifactIdentityInput,
    RuntimeArtifactKind, RuntimeConformanceIdentity, RuntimeConformanceIdentityInput,
    RuntimePassport, RuntimePassportError, RuntimePassportInput, RuntimeToolchainIdentity,
    RuntimeToolchainIdentityInput, runtime_conformance_contract, runtime_passport_contract,
};
pub use protocol::{
    ConnectionRoleCapability, FromRuntimeMessageType, HEADLESS_FROM_RUNTIME_MESSAGE_NAMES,
    HEADLESS_PROTOCOL_SCHEMA_VERSION, HEADLESS_PROTOCOL_VERSION,
    HEADLESS_RUNTIME_ONLY_FROM_RUNTIME_MESSAGE_NAMES,
    HEADLESS_RUNTIME_ONLY_TO_RUNTIME_MESSAGE_NAMES, HEADLESS_TERMINAL_REDUCER_VERSION,
    HEADLESS_TO_RUNTIME_MESSAGE_NAMES, HEADLESS_TURN_TERMINAL_RESPONSE_IDS,
    HeadlessCapabilityProjection, HeadlessProtocolContract, HeadlessTerminalProjection,
    NegotiatedHeadlessProtocol, NotificationCapability, SUPPORTED_HEADLESS_PROTOCOL_VERSIONS,
    SchemaOnlyServerRequestCapability, ServerRequestCapability, TaggedMessageDecode,
    TaggedMessageDecodeError, TerminalErrorKind, TerminalEvent, TerminalReducer, TerminalStatus,
    TerminalTransition, ToRuntimeMessageType, UnknownWireMessage,
    UnsupportedHeadlessProtocolVersion, UtilityOperationCapability, decode_tagged_message,
    headless_protocol_capability_digest, headless_protocol_contract,
    headless_protocol_version_is_supported, negotiate_headless_protocol,
};
pub use receipt_contract::{
    CodeAuthorityDecision, ExecutionReceipt, ManagedPolicyMetadata, ToolReceiptDetails,
};
pub use receipts::{
    MAX_RUNTIME_RECEIPT_STRING_BYTES, RUNTIME_RECEIPT_VERSION, RuntimeLifecycleState,
    RuntimeReceipt, RuntimeReceiptError, RuntimeReceiptInput, RuntimeReceiptKind,
    RuntimeTerminalClassification, runtime_receipt_validation_contract,
};
pub use session_event::{
    SESSION_EVENT_CUSTOM_TYPE, SESSION_EVENT_SCHEMA, SessionEvent, SessionEventLane,
    SessionEventPhase,
};
pub use telemetry::{
    Telemetry, TelemetryConfig, TelemetryGuard, TraceHeaders, approval_span, client_span,
    consumer_span, current_trace_headers, inject_evalops_context, inject_span_context, model_span,
    record_http_response, record_model_usage, record_outcome, route_class, server_span,
    terminal_span, tool_span, tool_span_for_call, trace_headers_for_span, turn_span,
};
pub use tool_details::{
    BashDetails, BatchDetails, DiffDetails, EditDetails, GlobDetails, GrepDetails, ImageDetails,
    InlineToolDetails, ListDetails, ReadDetails, ToolDetails, WebFetchDetails, WriteDetails,
};
pub use tool_operation::{
    MAX_TOOL_OPERATION_PROGRESS_BYTES, TOOL_OPERATION_CUSTOM_TYPE, ToolOperationError,
    ToolOperationLedger, ToolOperationOutcome, ToolOperationPhase, ToolOperationRecord,
    ToolOperationRecovery, ToolReplayPolicy,
};
pub use turn_outcome::{TurnFailureClass, TurnPhase};

/// Stable product identifier for the native Maestro runtime boundary.
pub const RUNTIME_PRODUCT_ID: &str = "evalops.maestro.runtime";
/// Version identifier for the serialized hosted runtime boundary contract.
pub const RUNTIME_BOUNDARY_VERSION: &str = "evalops.maestro.runtime-boundary.v1";
/// Existing hosted-runner process topology preserved by the native boundary.
pub const HOSTED_RUNTIME_TOPOLOGY: &str = "hosted-runner->maestro-headless-child";

/// Stable product and boundary-version identity for the native runtime crate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeProductIdentity {
    /// Stable product identifier.
    pub product_id: &'static str,
    /// Versioned boundary contract identifier.
    pub boundary_version: &'static str,
    /// Published native runtime package version.
    pub package_version: &'static str,
}

/// Returns the native runtime product identity used by startup and release
/// checks.
#[must_use]
pub const fn product_identity() -> RuntimeProductIdentity {
    RuntimeProductIdentity {
        product_id: RUNTIME_PRODUCT_ID,
        boundary_version: RUNTIME_BOUNDARY_VERSION,
        package_version: env!("CARGO_PKG_VERSION"),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        HOSTED_RUNTIME_TOPOLOGY, RUNTIME_BOUNDARY_VERSION, RUNTIME_PRODUCT_ID, product_identity,
    };

    #[test]
    fn product_identity_is_stable() {
        let identity = product_identity();
        assert_eq!(identity.product_id, RUNTIME_PRODUCT_ID);
        assert_eq!(identity.boundary_version, RUNTIME_BOUNDARY_VERSION);
        assert_eq!(identity.package_version, "0.1.0");
    }

    #[test]
    fn hosted_topology_is_the_existing_process_shape() {
        assert_eq!(
            HOSTED_RUNTIME_TOPOLOGY,
            "hosted-runner->maestro-headless-child"
        );
    }
}

mod thinking;
pub use thinking::ThinkingLevel;

pub mod experiments;
