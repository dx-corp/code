use super::*;

#[tokio::test]
async fn unauthenticated_owner_and_reused_session_generation_fail_closed() {
    let session = crate::tests::test_session_record("session-1");
    let generation = session.created_at.clone();
    let state =
        crate::tests::test_app_state_with_sessions(HashMap::from([("session-1".into(), session)]));
    assert_eq!(
        binding_for(&state, &AuthContext::default(), "session-1", &generation)
            .await
            .unwrap_err()
            .status,
        401
    );
    let auth = AuthContext {
        unrestricted: true,
        source: AuthSource::StaticGatewayKey,
        ..Default::default()
    };
    assert!(
        binding_for(&state, &auth, "session-1", &generation)
            .await
            .is_ok()
    );
    assert_eq!(
        binding_for(&state, &auth, "session-1", "reused-generation")
            .await
            .unwrap_err()
            .status,
        404
    );
}

#[tokio::test]
async fn attachment_admission_rejects_omitted_or_unconsumed_bytes_and_native_truncation() {
    use base64::Engine;
    let state = crate::tests::test_app_state_with_sessions(HashMap::new());
    for attachment in [
        serde_json::json!({"fileName":"missing.txt","contentOmitted":true}),
        serde_json::json!({"fileName":"binary.pdf","content":BASE64_STANDARD.encode([0xff,0xfe])}),
        serde_json::json!({"fileName":"oversized.txt","content":BASE64_STANDARD.encode("x".repeat(100_001))}),
    ] {
        let request:ChatRequest=serde_json::from_value(serde_json::json!({"messages":[{"role":"user","content":"inspect","attachments":[attachment]}]})).unwrap();
        assert!(
            validate_attachments(&request, &state, "provider/model")
                .await
                .is_err()
        );
    }
    let extracted:ChatRequest=serde_json::from_value(serde_json::json!({"messages":[{"role":"user","content":"inspect","attachments":[{"fileName":"binary.pdf","content":BASE64_STANDARD.encode([0xff,0xfe]),"extractedText":"actual extracted text"}]}]})).unwrap();
    assert!(
        validate_attachments(&extracted, &state, "provider/model")
            .await
            .is_ok()
    );
}

#[test]
fn prompt_text_limit_is_independent_of_attachment_wire_budget() {
    let request: ChatRequest = serde_json::from_value(
        serde_json::json!({"messages":[{"role":"user","content":"x".repeat(64*1024+1)}]}),
    )
    .unwrap();
    assert_eq!(
        validate_request(&request, "session-1").unwrap_err().status,
        400
    );
}
