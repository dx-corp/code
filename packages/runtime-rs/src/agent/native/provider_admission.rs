//! Existing provider policy hooks and gateway receipt projection.

use super::*;

impl NativeAgentRunner {
    pub(super) async fn admit_provider_request(
        &self,
        kind: &str,
        request_id: &str,
        model: Option<&str>,
    ) -> Result<()> {
        let policy_model =
            model.map(|model| provider_request_policy_model_id(&self.config.model, model));
        if let Some(reason) = policy_model
            .as_deref()
            .and_then(|model| self.tool_executor.model_allowed(model))
        {
            return Err(anyhow::Error::new(ProviderAdmissionDenied {
                kind: kind.to_owned(),
                request_id: request_id.to_owned(),
                reason,
            }));
        }
        let result = self
            .hooks
            .hook_pre_provider_request(kind, request_id, model)
            .await;
        let reason = match result {
            NativeHookResult::Continue => return Ok(()),
            NativeHookResult::Block { reason } => reason,
            NativeHookResult::ModifyInput { .. } => {
                "provider admission hook returned an unsupported input modification".to_owned()
            }
            NativeHookResult::InjectContext { .. } => {
                "provider admission hook returned unsupported context injection".to_owned()
            }
        };
        Err(anyhow::Error::new(ProviderAdmissionDenied {
            kind: kind.to_owned(),
            request_id: request_id.to_owned(),
            reason,
        }))
    }

    pub(super) fn managed_gateway_receipt_event(
        receipt: maestro_ai::ManagedGatewayReceipt,
        experiment_eligible: bool,
    ) -> FromAgent {
        FromAgent::ManagedGatewayReceipt {
            request_id: receipt.request_id,
            record_id: receipt.record_id,
            lineage_id: receipt.lineage_id,
            record_status: receipt.record_status,
            provider_tools_sha256: if experiment_eligible {
                receipt.provider_tools_sha256
            } else {
                None
            },
            provider_tool_count: if experiment_eligible {
                receipt.provider_tool_count
            } else {
                None
            },
            // Auxiliary compaction uses its own instructions. Keep its cost
            // receipt, but never label it as exposure to the turn's treatment.
            provider_prompt_sha256: if experiment_eligible {
                receipt.provider_prompt_sha256
            } else {
                None
            },
        }
    }
}
