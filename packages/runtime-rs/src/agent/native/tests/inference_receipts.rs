use super::*;

#[test]
fn prompt_experiment_excludes_auxiliary_compaction_receipts() {
    for eligible in [false, true] {
        let receipt = maestro_ai::ManagedGatewayReceipt {
            request_id: "request".into(),
            record_id: "record".into(),
            lineage_id: "lineage".into(),
            record_status: "planned".into(),
            provider_prompt_sha256: Some("sha256:verified".into()),
            provider_tools_sha256: Some("sha256:tools".into()),
            provider_tool_count: Some(3),
        };
        let FromAgent::ManagedGatewayReceipt {
            record_id,
            provider_prompt_sha256,
            provider_tools_sha256,
            provider_tool_count,
            ..
        } = NativeAgentRunner::managed_gateway_receipt_event(receipt, eligible)
        else {
            panic!("gateway receipt must be preserved")
        };
        assert_eq!(record_id, "record");
        assert_eq!(
            provider_tools_sha256.as_deref(),
            eligible.then_some("sha256:tools")
        );
        assert_eq!(provider_tool_count, eligible.then_some(3));
        assert_eq!(
            provider_prompt_sha256.as_deref(),
            eligible.then_some("sha256:verified")
        );
    }
}
