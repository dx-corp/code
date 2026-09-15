//! Content-free operation diagnostics. Values are observations, never admission authority.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OperationDiagnostics {
    pub kind: OperationKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experiment: Option<maestro_runtime_contracts::experiments::ExperimentObservation>,
    pub parent_turn_id: Option<String>,
    pub completion_observed: bool,
    pub message_count: Option<u32>,
    pub input_size_bytes: Option<u64>,
    pub output_size_bytes: Option<u64>,
    pub mcp_server_count: Option<u32>,
    pub context_source_count: Option<u32>,
    pub thinking_level: Option<String>,
    pub preparation_duration_ms: Option<u64>,
    pub retry_delay_ms: u64,
    pub rate_limit_count: u32,
    pub approval_wait_ms: u64,
    pub approvals_measured: u32,
    pub boost_suggested: bool,
    pub boost_requested: bool,
    pub boost_applied: bool,
    pub attempts: Vec<AttemptDiagnostics>,
    pub omitted_attempts: u32,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    #[default]
    Turn,
    SideQuestion,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AttemptDiagnostics {
    pub response_id: String,
    pub model_id: String,
    pub model_provider: String,
    pub duration_ms: u64,
    pub usage: Option<super::wide_events::TokenUsage>,
    pub reported_cost_usd: Option<f64>,
    /// Gateway identifiers are correlation hints, not evidence of authorization.
    pub gateway_request_id: Option<String>,
    pub gateway_record_id: Option<String>,
    pub gateway_lineage_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_tools_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_tool_count: Option<u32>,
}

impl OperationDiagnostics {
    pub fn is_valid(&self) -> bool {
        fn id(value: &str) -> bool {
            !value.is_empty() && value.len() <= 255 && !value.chars().any(char::is_whitespace)
        }
        fn cost(value: f64) -> bool {
            value.is_finite() && (0.0..=1_000_000.0).contains(&value)
        }
        self.experiment.as_ref().is_none_or(|e| e.is_valid())
            && self.parent_turn_id.as_deref().is_none_or(id)
            && self.thinking_level.as_deref().is_none_or(|v| {
                matches!(
                    v,
                    "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "adaptive"
                )
            })
            && [
                self.message_count,
                self.mcp_server_count,
                self.context_source_count,
            ]
            .into_iter()
            .flatten()
            .all(|n| n <= 1_000_000)
            && [self.input_size_bytes, self.output_size_bytes]
                .into_iter()
                .flatten()
                .all(|n| n <= 1 << 30)
            && self.preparation_duration_ms.is_none_or(|n| n <= 86_400_000)
            && self.retry_delay_ms <= 86_400_000
            && self.approval_wait_ms <= 86_400_000
            && self.rate_limit_count <= 1_000_000
            && self.approvals_measured <= 1_000_000
            && self.omitted_attempts <= 1_000_000
            && self.attempts.len() <= 32
            && self.attempts.iter().all(|a| {
                a.provider_tools_sha256.as_ref().is_none_or(|s| {
                    s.len() == 71
                        && s.starts_with("sha256:")
                        && s[7..]
                            .bytes()
                            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
                }) && a.provider_tool_count.is_none_or(|n| n <= 100_000)
                    && a.provider_tools_sha256.is_some() == a.provider_tool_count.is_some()
                    && id(&a.response_id)
                    && id(&a.model_id)
                    && id(&a.model_provider)
                    && a.duration_ms <= 86_400_000
                    && a.reported_cost_usd.is_none_or(cost)
                    && [
                        &a.gateway_request_id,
                        &a.gateway_record_id,
                        &a.gateway_lineage_id,
                    ]
                    .into_iter()
                    .all(|v| v.as_deref().is_none_or(id))
                    && a.usage.as_ref().is_none_or(|u| {
                        [
                            u.input,
                            u.output,
                            u.cache_read,
                            u.cache_write,
                            u.thinking.unwrap_or(0),
                        ]
                        .into_iter()
                        .all(|n| n <= 100_000_000)
                    })
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_fixture_round_trips_without_private_content() {
        let wire: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tui-rs/src/telemetry/operation_fixture.json"
        ))
        .unwrap();
        let diagnostics: OperationDiagnostics = serde_json::from_value(wire.clone()).unwrap();
        assert!(diagnostics.is_valid());
        assert_eq!(serde_json::to_value(&diagnostics).unwrap(), wire);
        let mut invalid = wire;
        invalid["prompt"] = "private".into();
        assert!(serde_json::from_value::<OperationDiagnostics>(invalid).is_err());
    }
}
