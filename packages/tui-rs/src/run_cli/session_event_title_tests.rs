use super::*;

#[test]
fn gateway_failure_timeline_keeps_request_id_and_failure_kind() {
    use maestro_runtime_contracts::{SessionEvent, SessionEventLane, SessionEventPhase};

    let mut receipt = SessionEvent::new(
        "event-receipt",
        "2026-05-09T10:03:02.000Z",
        "sess-run-1",
        SessionEventLane::Model,
        "model.gateway.receipt",
        SessionEventPhase::Info,
    );
    receipt.correlation_id = Some("request-123".into());
    let mut failure = SessionEvent::new(
        "event-failure",
        "2026-05-09T10:03:03.000Z",
        "sess-run-1",
        SessionEventLane::Model,
        "model.response.failed",
        SessionEventPhase::Failed,
    );
    failure.name = Some("transientprotocol".into());
    let mut retry = SessionEvent::new(
        "event-retry",
        "2026-05-09T10:03:04.000Z",
        "sess-run-1",
        SessionEventLane::Runtime,
        "retry.scheduled",
        SessionEventPhase::Requested,
    );
    retry.name = Some("provider_stream_timeout".into());

    assert_eq!(
        session_event_title(&receipt),
        "Gateway request ID: request-123"
    );
    assert_eq!(
        session_event_title(&failure),
        "Model response failed (transientprotocol)"
    );
    assert_eq!(
        session_event_title(&retry),
        "Request retry scheduled (provider_stream_timeout)"
    );
}
