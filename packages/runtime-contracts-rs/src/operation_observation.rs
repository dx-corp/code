//! Bounded content-free measurements emitted by the native operation owner.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OperationObservation {
    Admitted {
        turn_id: String,
        thinking_level: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        experiment: Option<crate::experiments::ExperimentObservation>,
    },
    Prepared {
        response_id: String,
        model_id: String,
        model_provider: String,
        message_count: u32,
        input_size_bytes: Option<u64>,
    },
    ReasoningUsage {
        response_id: String,
        tokens: u64,
    },
    GatewayReceipt {
        response_id: String,
        request_id: String,
        record_id: String,
        lineage_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_tools_sha256: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_tool_count: Option<u32>,
    },
}
