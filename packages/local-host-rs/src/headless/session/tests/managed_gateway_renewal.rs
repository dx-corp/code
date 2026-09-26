use super::*;

#[test]
fn managed_inference_authorization_crosses_wire_but_not_session_journals() {
    let temp = TempDir::new().expect("session root");
    let mut recorder = SessionRecorder::new(temp.path()).expect("session recorder");
    let session_id = recorder.id().to_string();
    let message = ToAgentMessage::Prompt {
        content: "managed prompt".to_string(),
        attachments: None,
        managed_inference_authorization: Some(crate::agent::ManagedInferenceAuthorization::new(
            "signed-capability-marker",
        )),
    };

    let wire = serde_json::to_string(&message).expect("serialize transport message");
    assert!(wire.contains("managed_inference_authorization"));
    assert!(wire.contains("signed-capability-marker"));

    recorder.record_sent(&message).expect("record sent message");
    recorder
        .record_sent(&ToAgentMessage::ManagedAuthorizationResult {
            request_id: "invocation-1".into(),
            authorization: crate::agent::ManagedInferenceAuthorization::new(
                "renewed-capability-marker",
            ),
            gateway_credential: Some(maestro_runtime_contracts::ManagedGatewayCredential::new(
                "bearer-secret-marker",
                i64::MAX,
            )),
        })
        .expect("record renewal delivery");
    recorder.flush_checkpoint().expect("flush session state");
    drop(recorder);

    for path in [
        temp.path().join(format!("{session_id}.jsonl")),
        temp.path().join(format!("{session_id}.replay.json")),
    ] {
        let durable = fs::read_to_string(path).expect("read durable session data");
        assert!(!durable.contains("managed_inference_authorization"));
        assert!(!durable.contains("signed-capability-marker"));
        assert!(!durable.contains("renewed-capability-marker"));
        assert!(!durable.contains("bearer-secret-marker"));
    }
}
