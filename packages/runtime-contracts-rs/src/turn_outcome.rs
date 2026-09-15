//! Closed vocabulary for the terminal outcome of one Platform-driven Maestro turn.
//!
//! Platform reports the seam between its durable turn owner and the Maestro
//! runtime with a bounded phase and failure class so dashboards, canaries, and
//! alerts share one typed vocabulary instead of matching free-form error codes.

use serde::{Deserialize, Serialize};

/// Lifecycle phase of one Platform-driven Maestro turn.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnPhase {
    /// Ingress, binding, and process or client admission before any protocol traffic.
    Admission,
    /// Waiting for the runtime `ready` announcement and protocol negotiation.
    Ready,
    /// Controller `hello` exchange, binding acknowledgement, and tool-catalog validation.
    Hello,
    /// Session `init` and optional experiment configuration before the prompt.
    Init,
    /// Prompt submitted; streaming responses and terminal events.
    Prompt,
    /// A governed Platform tool call requested by the runtime is executing.
    ToolBridge,
    /// Turn accepted by Platform; shutdown and post-turn bookkeeping.
    Completion,
}

impl TurnPhase {
    pub const ALL: [Self; 7] = [
        Self::Admission,
        Self::Ready,
        Self::Hello,
        Self::Init,
        Self::Prompt,
        Self::ToolBridge,
        Self::Completion,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Admission => "admission",
            Self::Ready => "ready",
            Self::Hello => "hello",
            Self::Init => "init",
            Self::Prompt => "prompt",
            Self::ToolBridge => "tool_bridge",
            Self::Completion => "completion",
        }
    }
}

/// Bounded classification of why one Maestro turn did not complete.
///
/// Classes are derived from the stable Platform error-code families for the
/// Maestro seam. Codes outside every family classify as [`Self::Other`] so a
/// new code never silently joins a class that drives retry or alert policy.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnFailureClass {
    /// Process spawn, stdio, or native client transport failed or closed.
    Transport,
    /// Runtime declared a transient failure that Platform may retry.
    Transient,
    /// Protocol ordering, encoding, version, or unknown-event violation.
    Protocol,
    /// Protocol or turn deadline elapsed before completion.
    Timeout,
    /// Traffic, tool-call, or token budget exhausted.
    Budget,
    /// Model provider reported a typed failure.
    Provider,
    /// Governed tool bridge refused, failed, or was misused.
    Tool,
    /// Controller, model, capability, or prompt-exposure binding mismatch.
    Binding,
    /// Engine, ingress, or experiment configuration is invalid or unavailable.
    Configuration,
    /// Turn cancelled or interrupted by the runtime or the owner.
    Cancelled,
    /// Runtime finished without an acceptable result.
    Runtime,
    /// Code outside the Maestro seam families.
    Other,
}

impl TurnFailureClass {
    pub const ALL: [Self; 12] = [
        Self::Transport,
        Self::Transient,
        Self::Protocol,
        Self::Timeout,
        Self::Budget,
        Self::Provider,
        Self::Tool,
        Self::Binding,
        Self::Configuration,
        Self::Cancelled,
        Self::Runtime,
        Self::Other,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Transport => "transport",
            Self::Transient => "transient",
            Self::Protocol => "protocol",
            Self::Timeout => "timeout",
            Self::Budget => "budget",
            Self::Provider => "provider",
            Self::Tool => "tool",
            Self::Binding => "binding",
            Self::Configuration => "configuration",
            Self::Cancelled => "cancelled",
            Self::Runtime => "runtime",
            Self::Other => "other",
        }
    }

    /// Classifies one stable Platform error code from the Maestro seam.
    #[must_use]
    pub fn from_error_code(code: &str) -> Self {
        if code == "runner_turn_timeout" {
            return Self::Timeout;
        }
        let Some(family) = code.strip_prefix("maestro_") else {
            return Self::Other;
        };
        // The native transport prefixes its codes with `native_`; the failure
        // family is the same vocabulary as the process transport.
        let family = family.strip_prefix("native_").unwrap_or(family);
        match family {
            "transient_failure" => return Self::Transient,
            "protocol_timeout" | "turn_deadline_exceeded" | "auxiliary_timeout" => {
                return Self::Timeout;
            }
            "turn_budget_exceeded" | "tool_call_limit_exceeded" | "tool_budget_exhausted" => {
                return Self::Budget;
            }
            "turn_cancelled" | "turn_interrupted" => return Self::Cancelled,
            "turn_failed"
            | "empty_response"
            | "auxiliary_empty_payload"
            | "prompt_failed"
            | "prompt_encode_failed"
            | "response_too_large" => return Self::Runtime,
            "process_start_failed"
            | "start_failed"
            | "client_unavailable"
            | "client_resolution_failed" => return Self::Transport,
            "ingress_invalid"
            | "experiment_store_unavailable"
            | "prompt_experiment_unsupported"
            | "provider_config_invalid"
            | "provider_scope_mismatch"
            | "managed_client_required"
            | "managed_scope_mismatch"
            | "tool_approval_mode_invalid" => return Self::Configuration,
            "client_tool_unsupported"
            | "required_tool_not_selected"
            | "auxiliary_tool_call_rejected"
            | "server_request_unhandled"
            | "duplicate_tool_call" => return Self::Tool,
            _ => {}
        }
        if family.starts_with("transport_") {
            Self::Transport
        } else if family.starts_with("protocol_") {
            Self::Protocol
        } else if family.starts_with("provider_") {
            Self::Provider
        } else if family.starts_with("tool_catalog_")
            || family.starts_with("controller_binding_")
            || family.starts_with("model_binding_")
            || family.starts_with("prompt_exposure_")
            || family.starts_with("capability_catalog_")
            || family.starts_with("capability_manifest")
        {
            Self::Binding
        } else if family.starts_with("tool_") {
            Self::Tool
        } else if family.starts_with("prompt_experiment_") {
            Self::Configuration
        } else {
            Self::Other
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_class_round_trips_through_its_wire_name() {
        for class in TurnFailureClass::ALL {
            let encoded = serde_json::to_value(class).expect("class serializes");
            assert_eq!(
                encoded,
                serde_json::Value::String(class.as_str().to_string())
            );
            let decoded: TurnFailureClass =
                serde_json::from_value(encoded).expect("class deserializes");
            assert_eq!(decoded, class);
        }
        for phase in TurnPhase::ALL {
            let encoded = serde_json::to_value(phase).expect("phase serializes");
            assert_eq!(
                encoded,
                serde_json::Value::String(phase.as_str().to_string())
            );
        }
    }

    #[test]
    fn seam_error_codes_classify_into_closed_families() {
        use TurnFailureClass as C;
        let cases = [
            ("maestro_transport_closed", C::Transport),
            ("maestro_transport_failed", C::Transport),
            ("maestro_process_start_failed", C::Transport),
            ("maestro_native_client_unavailable", C::Transport),
            ("maestro_transient_failure", C::Transient),
            ("maestro_protocol_order_invalid", C::Protocol),
            ("maestro_protocol_unknown_event", C::Protocol),
            ("maestro_protocol_version_mismatch", C::Protocol),
            ("maestro_protocol_timeout", C::Timeout),
            ("maestro_turn_deadline_exceeded", C::Timeout),
            ("runner_turn_timeout", C::Timeout),
            ("maestro_turn_budget_exceeded", C::Budget),
            ("maestro_tool_call_limit_exceeded", C::Budget),
            ("maestro_provider_declared_failure", C::Provider),
            ("maestro_provider_output_token_exhaustion", C::Provider),
            ("maestro_tool_bridge_unavailable", C::Tool),
            ("maestro_tool_failed", C::Tool),
            ("maestro_tool_ownership_conflict", C::Tool),
            ("maestro_required_tool_not_selected", C::Tool),
            ("maestro_controller_binding_mismatch", C::Binding),
            ("maestro_model_binding_mismatch", C::Binding),
            ("maestro_prompt_exposure_missing", C::Binding),
            ("maestro_capability_manifest", C::Binding),
            ("maestro_native_capability_catalog_invalid", C::Binding),
            ("maestro_ingress_invalid", C::Configuration),
            ("maestro_prompt_experiment_config_invalid", C::Configuration),
            ("maestro_experiment_store_unavailable", C::Configuration),
            ("maestro_turn_cancelled", C::Cancelled),
            ("maestro_turn_interrupted", C::Cancelled),
            ("maestro_turn_failed", C::Runtime),
            ("maestro_empty_response", C::Runtime),
            ("maestro_native_transport_closed", C::Transport),
            ("maestro_native_start_failed", C::Transport),
            ("maestro_native_client_resolution_failed", C::Transport),
            ("maestro_native_protocol_order_invalid", C::Protocol),
            ("maestro_native_provider_error", C::Provider),
            ("maestro_native_turn_deadline_exceeded", C::Timeout),
            ("maestro_native_tool_call_limit_exceeded", C::Budget),
            ("maestro_native_tool_budget_exhausted", C::Budget),
            ("maestro_native_turn_interrupted", C::Cancelled),
            ("maestro_native_turn_failed", C::Runtime),
            ("maestro_native_empty_response", C::Runtime),
            ("maestro_native_prompt_failed", C::Runtime),
            ("maestro_native_prompt_encode_failed", C::Runtime),
            ("maestro_native_response_too_large", C::Runtime),
            ("maestro_native_model_binding_mismatch", C::Binding),
            ("maestro_native_model_binding_invalid", C::Binding),
            ("maestro_native_tool_catalog_invalid", C::Binding),
            ("maestro_native_tool_catalog_empty", C::Binding),
            ("maestro_native_tool_call_invalid", C::Tool),
            ("maestro_native_tool_unsupported", C::Tool),
            ("maestro_native_tool_not_admitted", C::Tool),
            ("maestro_native_tool_approval_invalid", C::Tool),
            ("maestro_native_tool_response_closed", C::Tool),
            ("maestro_native_duplicate_tool_call", C::Tool),
            ("maestro_native_managed_client_required", C::Configuration),
            ("maestro_native_managed_scope_mismatch", C::Configuration),
            ("maestro_native_provider_scope_mismatch", C::Configuration),
            (
                "maestro_native_prompt_experiment_unsupported",
                C::Configuration,
            ),
            ("swarm_native_scheduler_unavailable", C::Other),
            ("maestro_future_code", C::Other),
            ("", C::Other),
        ];
        for (code, expected) in cases {
            assert_eq!(
                TurnFailureClass::from_error_code(code),
                expected,
                "{code} must classify as {expected:?}"
            );
        }
    }
}
