//! Shared receipt value models.
//!
//! These are evidence values produced by an execution owner.  They carry the
//! policy and code-authority projections that were already attached to TUI
//! receipts; moving the values here lets session/context and future runtime
//! hosts persist the same JSON without taking a dependency on TUI policy
//! implementation.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{ExecutionSource, ExecutionStatus, ToolDetails};

/// Safe managed-policy identity attached to an execution receipt.
///
/// The signed policy body, public key, and signature intentionally remain
/// outside this value.  Policy verification and admission stay in the
/// owning policy implementation.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ManagedPolicyMetadata {
    pub org_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    pub policy_version: u64,
    pub issued_at: u64,
    pub expires_at: u64,
    pub key_id: String,
    pub policy_hash: String,
    pub kill_switch: bool,
}

/// A verified Code device authority decision attached to a tool receipt.
///
/// This is a value projection only.  The live authority, challenge binding,
/// tenant checks, and effect admission remain in the TUI/host adapter.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodeAuthorityDecision {
    pub allowed: bool,
    pub device_id: String,
    pub decision_id: String,
    pub policy_id: String,
    pub policy_version: String,
    pub request_digest: String,
    #[serde(deserialize_with = "read_i64")]
    pub expires_at_unix_seconds: i64,
}

impl CodeAuthorityDecision {
    /// Return whether this decision currently permits the associated call.
    ///
    /// This helper only evaluates the value's local expiry projection.  It
    /// does not replace the live authority check performed before dispatch.
    #[must_use]
    pub fn is_current(&self) -> bool {
        self.allowed && self.expires_at_unix_seconds > unix_seconds()
    }
}

/// Typed evidence captured for an execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "details", rename_all = "snake_case")]
pub enum ToolReceiptDetails {
    BuiltIn(ToolDetails),
    /// A local feedback proposal, with no send authority or selected evidence.
    FeedbackDraft {
        description: String,
        expected_behavior: String,
        reproduction_steps: String,
    },
    Mcp {
        server: String,
        tool: String,
        is_error: bool,
    },
    /// Provenance string for a tool whose output has no dedicated
    /// [`ToolDetails`] variant (e.g. `gh_issue`, `websearch`) but whose raw
    /// `details` JSON carried an `origin`/`url`/`query` field.
    Origin(String),
    /// Actual active-provider execution; missing usage remains unknown.
    ModelInference {
        provider: String,
        model: String,
        usage: Option<crate::TokenUsage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost: Option<f64>,
    },
    Cached,
    None,
}

/// Audit information that must not be sent as provider tool-result content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionReceipt {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_authority: Option<Box<CodeAuthorityDecision>>,
    pub call_id: String,
    pub tool_name: String,
    pub source: ExecutionSource,
    pub status: ExecutionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<Box<ManagedPolicyMetadata>>,
    pub details: ToolReceiptDetails,
}

fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(i64::MAX, |duration| duration.as_secs() as i64)
}

fn read_i64<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<i64, D::Error> {
    let value = Value::deserialize(deserializer)?;
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        .ok_or_else(|| serde::de::Error::custom("invalid int64"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BashDetails;

    #[test]
    fn receipt_round_trip_preserves_nested_authority_and_policy_json() {
        let receipt = ExecutionReceipt {
            code_authority: Some(Box::new(CodeAuthorityDecision {
                allowed: true,
                device_id: "device-1".to_owned(),
                decision_id: "decision-1".to_owned(),
                policy_id: "identity-code-hardware-authority".to_owned(),
                policy_version: "1".to_owned(),
                request_digest: "digest-1".to_owned(),
                expires_at_unix_seconds: 2_000_000_000,
            })),
            call_id: "call-1".to_owned(),
            tool_name: "bash".to_owned(),
            source: ExecutionSource::Native,
            status: ExecutionStatus::Succeeded,
            duration_ms: Some(17),
            policy: Some(Box::new(ManagedPolicyMetadata {
                org_id: "org-1".to_owned(),
                workspace_id: Some("workspace-1".to_owned()),
                policy_version: 4,
                issued_at: 1_700_000_000,
                expires_at: 1_800_000_000,
                key_id: "policy-key".to_owned(),
                policy_hash: "policy-hash".to_owned(),
                kill_switch: false,
            })),
            details: ToolReceiptDetails::BuiltIn(ToolDetails::Bash(BashDetails::success(
                "echo hi",
            ))),
        };

        let encoded = serde_json::to_value(&receipt).expect("receipt serializes");
        assert_eq!(encoded["code_authority"]["deviceId"], "device-1");
        assert_eq!(
            encoded["code_authority"]["expiresAtUnixSeconds"],
            2_000_000_000i64
        );
        assert_eq!(encoded["policy"]["workspaceId"], "workspace-1");
        assert_eq!(encoded["details"]["kind"], "built_in");
        assert_eq!(encoded["details"]["details"]["tool_type"], "bash");

        let decoded: ExecutionReceipt =
            serde_json::from_value(encoded.clone()).expect("receipt decodes");
        assert_eq!(
            serde_json::to_value(decoded).expect("decoded receipt serializes"),
            encoded
        );
    }

    #[test]
    fn code_authority_accepts_legacy_string_timestamp() {
        let decision: CodeAuthorityDecision = serde_json::from_value(serde_json::json!({
            "allowed": true,
            "deviceId": "device-1",
            "decisionId": "decision-1",
            "policyId": "policy",
            "policyVersion": "1",
            "requestDigest": "digest",
            "expiresAtUnixSeconds": "2000000000"
        }))
        .expect("legacy timestamp decodes");
        assert_eq!(decision.expires_at_unix_seconds, 2_000_000_000);
    }
}
