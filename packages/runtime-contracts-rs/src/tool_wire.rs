//! Headless tool payloads shared by runtime producers and Platform consumers.
//! These types describe the wire format; admission, ownership and receipt
//! verification remain with the authenticated controller.

use crate::ExecutionReceipt;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GovernedClientToolResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_tool_cost_micros: Option<u64>,
    pub call_id: String,
    pub content: Vec<ClientToolResultContent>,
    pub is_error: bool,
    pub tool_execution_id: String,
    pub client_instance_id: String,
    pub grant_id: String,
    pub grant_version: u64,
    pub grant_hash: String,
    pub turn_digest: String,
    pub definition_digest: String,
    pub args_digest: String,
    pub owner_lease_epoch: u64,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GovernedClientToolRequest {
    pub call_id: String,
    pub tool_execution_id: String,
    pub tool: String,
    pub args: serde_json::Value,
    pub provider_tool_name: String,
    pub tool_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_binding_id: Option<String>,
    pub client_instance_id: String,
    pub grant_id: String,
    pub grant_version: u64,
    pub grant_hash: String,
    pub turn_digest: String,
    pub definition_digest: String,
    pub args_digest: String,
    pub owner_lease_epoch: u64,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolEnd {
    pub call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_execution_id: Option<String>,
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<ExecutionReceipt>,
}

/// Content returned from a client-side tool execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientToolResultContent {
    Text {
        text: String,
    },
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
}

/// Controller-to-runtime governed tool result. The type tag is part of the contract.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolClientMessage {
    GovernedClientToolResult(GovernedClientToolResult),
}

/// Native acceptance of a control response by its consumer, not transport enqueueing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResponseAccepted {
    pub request_id: String,
}

/// Runtime-to-controller requests, completions and response consumption acknowledgements.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolServerMessage {
    GovernedClientToolRequest(GovernedClientToolRequest),
    ToolEnd(ToolEnd),
    ResponseAccepted(ResponseAccepted),
}

#[cfg(test)]
mod tests;
