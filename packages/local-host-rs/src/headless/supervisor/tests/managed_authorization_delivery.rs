use super::*;

#[test]
fn supervisor_managed_authorization_request_waits_for_native_acknowledgement() {
    let mut supervisor = AgentSupervisor::new(SupervisorConfig::default());
    let event = supervisor
        .apply_agent_message(FromAgentMessage::ManagedAuthorizationRequest {
            request_id: "invocation-1".into(),
        })
        .expect("authorization request must reach controller");
    supervisor.event_tx.send(event).unwrap();
    assert!(
        matches!(supervisor.drain_available_agent_messages().as_slice(),
        [FromAgentMessage::ManagedAuthorizationRequest { request_id }] if request_id == "invocation-1")
    );
    let response = ToAgentMessage::ManagedAuthorizationResult {
        request_id: "invocation-1".into(),
        authorization: maestro_runtime_contracts::ManagedInferenceAuthorization::new("opaque"),
        gateway_credential: None,
    };
    assert_eq!(response_ack_request_id(&response), Some("invocation-1"));
    supervisor.state.handle_sent_message(&response);
    assert_eq!(
        supervisor.state.pending_managed_authorizations,
        ["invocation-1"]
    );
    supervisor.apply_agent_message(FromAgentMessage::ResponseAccepted(
        tool_wire::ResponseAccepted {
            request_id: "other".into(),
        },
    ));
    assert_eq!(
        supervisor.state.pending_managed_authorizations,
        ["invocation-1"]
    );
    supervisor.apply_agent_message(FromAgentMessage::ResponseAccepted(
        tool_wire::ResponseAccepted {
            request_id: "invocation-1".into(),
        },
    ));
    assert!(supervisor.state.pending_managed_authorizations.is_empty());
}
