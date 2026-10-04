use super::managed_gateway_receipt_status;

#[test]
fn managed_gateway_receipt_status_contains_safe_camel_case_fields() {
    let status = managed_gateway_receipt_status(
        "request-1".to_string(),
        "record-1".to_string(),
        "lineage-1".to_string(),
        "planned".to_string(),
    );

    assert_eq!(status["type"], "status");
    assert_eq!(status["status"], "managed_gateway_receipt");
    assert_eq!(status["details"]["requestId"], "request-1");
    assert_eq!(status["details"]["recordId"], "record-1");
    assert_eq!(status["details"]["lineageId"], "lineage-1");
    assert_eq!(status["details"]["recordStatus"], "planned");
    assert!(
        !status
            .to_string()
            .contains("managed_inference_authorization")
    );
}
