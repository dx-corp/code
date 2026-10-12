use std::time::{Duration, Instant};

use serde_json::json;
use uuid::Uuid;

use super::{
    CommandAuthority, HostToRunnerFrame, MAX_IN_FLIGHT_REQUESTS, RENDEZVOUS_PROTOCOL_VERSION,
    RendezvousAccepted, RendezvousAck, RendezvousCommandError, RendezvousCommandOutcome,
    RendezvousExecution, RendezvousIdentity, RendezvousLatencyMilestones, RendezvousLifecycle,
    RendezvousLifecycleError, RendezvousMode, RendezvousNonce, RendezvousRequest,
    RendezvousRequestDisposition, RevocationReason, RunnerToHostFrame,
};

fn nonce() -> RendezvousNonce {
    RendezvousNonce::parse("proof-0123456789abcdef0123456789abcdef").unwrap()
}

fn identity() -> RendezvousIdentity {
    RendezvousIdentity {
        organization_id: "org_123".to_string(),
        workspace_id: "workspace_123".to_string(),
        sandbox_id: Uuid::parse_str("f7404c4a-7a0d-4f66-884e-5ec41ce36f34").unwrap(),
        placement_generation: 17,
        runner_session_id: "runner_123".to_string(),
    }
}

fn accepted(
    activation_id: Uuid,
    outbound_commands_enabled: bool,
    replay_from_sequence: u64,
) -> RendezvousAccepted {
    RendezvousAccepted {
        activation_id,
        outbound_commands_enabled,
        replay_from_sequence,
    }
}

#[test]
fn wire_contract_carries_first_request_on_the_accepted_stream() {
    let activation_id = Uuid::parse_str("f16b80ac-81af-46fc-8b9d-8ae04e81c5cb").unwrap();
    let mut lifecycle = RendezvousLifecycle::new(RendezvousMode::Outbound, identity(), nonce());
    lifecycle.set_runtime_ready(true);

    let open = lifecycle.open(activation_id).unwrap();
    assert_eq!(open.protocol_version, RENDEZVOUS_PROTOCOL_VERSION);
    assert_eq!(open.max_in_flight_requests, MAX_IN_FLIGHT_REQUESTS);
    assert_eq!(open.resume_after_sequence, None);
    assert_eq!(
        serde_json::to_value(&open).unwrap()["rendezvous_nonce"],
        "proof-0123456789abcdef0123456789abcdef"
    );
    lifecycle.accept(&accepted(activation_id, true, 1)).unwrap();

    let request = HostToRunnerFrame::Request(RendezvousRequest {
        activation_id,
        sequence: 1,
        idempotency_key: "request_1".to_string(),
        payload: json!({"type": "initialize", "model": "test"}),
    });
    assert_eq!(
        serde_json::to_value(request).unwrap(),
        json!({
            "type": "request",
            "activation_id": activation_id,
            "sequence": 1,
            "idempotency_key": "request_1",
            "payload": {"type": "initialize", "model": "test"}
        })
    );
}

#[test]
fn rendezvous_nonce_is_bounded_and_redacted() {
    assert!(RendezvousNonce::parse("").is_err());
    assert!(RendezvousNonce::parse("x".repeat(257)).is_err());
    let nonce = nonce();
    assert_eq!(format!("{nonce:?}"), "RendezvousNonce(<redacted>)");
    assert!(!format!("{nonce:?}").contains("0123456789abcdef"));
}

#[test]
fn exact_replay_is_classified_before_command_execution() {
    let activation_id = Uuid::new_v4();
    let mut lifecycle = RendezvousLifecycle::new(RendezvousMode::Outbound, identity(), nonce());
    lifecycle.set_runtime_ready(true);
    lifecycle.open(activation_id).unwrap();
    lifecycle.accept(&accepted(activation_id, true, 1)).unwrap();
    assert_eq!(
        lifecycle.request_disposition(1, "command-1").unwrap(),
        RendezvousRequestDisposition::Execute
    );
    lifecycle.record_applied_request(1, "command-1").unwrap();
    assert_eq!(
        lifecycle.request_disposition(1, "command-1").unwrap(),
        RendezvousRequestDisposition::Replay
    );
    assert!(
        lifecycle
            .request_disposition(1, "different-command")
            .is_err()
    );
}

#[test]
fn reconnecting_same_activation_revokes_authority_until_reaccepted() {
    let activation_id = Uuid::new_v4();
    let mut lifecycle = RendezvousLifecycle::new(RendezvousMode::Outbound, identity(), nonce());
    lifecycle.set_runtime_ready(true);
    lifecycle.open(activation_id).unwrap();
    lifecycle.accept(&accepted(activation_id, true, 1)).unwrap();
    assert_eq!(
        lifecycle.command_authority(),
        Some(CommandAuthority::Outbound)
    );
    lifecycle.open(activation_id).unwrap();
    assert_eq!(lifecycle.command_authority(), None);
    lifecycle.accept(&accepted(activation_id, true, 1)).unwrap();
    assert_eq!(
        lifecycle.command_authority(),
        Some(CommandAuthority::Outbound)
    );
}

#[test]
fn rollout_modes_never_enable_two_command_authorities() {
    assert_eq!(
        RendezvousMode::Inbound.command_authority(),
        Some(CommandAuthority::Inbound)
    );
    assert_eq!(
        RendezvousMode::OutboundShadow.command_authority(),
        Some(CommandAuthority::Inbound)
    );
    assert_eq!(
        RendezvousMode::Outbound.command_authority(),
        Some(CommandAuthority::Outbound)
    );
    assert!(!RendezvousMode::OutboundShadow.outbound_commands_enabled());
    assert!(RendezvousMode::Outbound.outbound_commands_enabled());

    let shadow_activation = Uuid::new_v4();
    let mut shadow = RendezvousLifecycle::new(RendezvousMode::OutboundShadow, identity(), nonce());
    shadow.set_runtime_ready(true);
    shadow.open(shadow_activation).unwrap();
    assert_eq!(
        shadow.accept(&accepted(shadow_activation, true, 1)),
        Err(RendezvousLifecycleError::AcceptanceAuthorityMismatch {
            expected: false,
            received: true,
        })
    );
    shadow
        .accept(&accepted(shadow_activation, false, 1))
        .unwrap();
    assert_eq!(shadow.command_authority(), Some(CommandAuthority::Inbound));
    assert_eq!(
        shadow.record_applied_request(1, "shadow_request"),
        Err(RendezvousLifecycleError::OutboundCommandsDisabled)
    );

    let outbound_activation = Uuid::new_v4();
    let mut outbound = RendezvousLifecycle::new(RendezvousMode::Outbound, identity(), nonce());
    outbound.set_runtime_ready(true);
    outbound.open(outbound_activation).unwrap();
    assert_eq!(outbound.command_authority(), None);
    assert_eq!(
        outbound.accept(&accepted(outbound_activation, false, 1)),
        Err(RendezvousLifecycleError::AcceptanceAuthorityMismatch {
            expected: true,
            received: false,
        })
    );
    outbound
        .accept(&accepted(outbound_activation, true, 1))
        .unwrap();
    assert_eq!(
        outbound.command_authority(),
        Some(CommandAuthority::Outbound)
    );
}

#[test]
fn readiness_loss_revokes_active_outbound_before_future_admission() {
    let activation_id = Uuid::parse_str("ae5484e8-602d-4d64-b7ad-8c12feb7f4af").unwrap();
    let mut lifecycle = RendezvousLifecycle::new(RendezvousMode::Outbound, identity(), nonce());
    lifecycle.set_runtime_ready(true);
    lifecycle.open(activation_id).unwrap();
    lifecycle.accept(&accepted(activation_id, true, 1)).unwrap();

    let close = lifecycle.set_runtime_ready(false).unwrap();
    assert_eq!(close.activation_id, activation_id);
    assert_eq!(close.reason, RevocationReason::RuntimeUnavailable);
    assert_eq!(lifecycle.command_authority(), None);
    assert_eq!(lifecycle.active_activation_id(), None);
    assert_eq!(lifecycle.set_runtime_ready(false), None);
    assert_eq!(
        lifecycle.open(Uuid::new_v4()),
        Err(RendezvousLifecycleError::RuntimeNotReady)
    );

    assert_eq!(
        serde_json::to_value(RunnerToHostFrame::Close(close)).unwrap(),
        json!({
            "type": "close",
            "activation_id": activation_id,
            "reason": "runtime_unavailable"
        })
    );
}

#[test]
fn rotation_and_late_close_are_idempotent() {
    let first = Uuid::parse_str("2b31e11c-f574-4163-a834-713054ecfc50").unwrap();
    let replacement = Uuid::parse_str("4c3993cb-f4de-4cd7-b8c4-bf7393e1ad6f").unwrap();
    let mut lifecycle = RendezvousLifecycle::new(RendezvousMode::Outbound, identity(), nonce());
    lifecycle.set_runtime_ready(true);
    lifecycle.open(first).unwrap();
    lifecycle.accept(&accepted(first, true, 1)).unwrap();
    for sequence in 1..=7 {
        lifecycle
            .record_applied_request(sequence, format!("request_{sequence}"))
            .unwrap();
    }

    let rotation = lifecycle.rotate(first, replacement).unwrap();
    assert_eq!(rotation.close.activation_id, first);
    assert_eq!(rotation.close.reason, RevocationReason::Rotated);
    assert_eq!(rotation.open.activation_id, replacement);
    assert_eq!(rotation.open.resume_after_sequence, Some(7));
    assert_eq!(lifecycle.active_activation_id(), Some(replacement));
    assert_eq!(lifecycle.command_authority(), None);
    lifecycle.accept(&accepted(replacement, true, 8)).unwrap();
    assert_eq!(
        lifecycle.command_authority(),
        Some(CommandAuthority::Outbound)
    );
    assert!(!lifecycle.close(first));
    assert_eq!(lifecycle.active_activation_id(), Some(replacement));

    assert_eq!(lifecycle.open(replacement).unwrap(), rotation.open);
    assert_eq!(lifecycle.active_activation_id(), Some(replacement));
}

#[test]
fn replay_requires_contiguous_sequences_and_stable_idempotency_keys() {
    let mut lifecycle = RendezvousLifecycle::new(RendezvousMode::Outbound, identity(), nonce());
    lifecycle.set_runtime_ready(true);
    let activation_id = Uuid::new_v4();
    lifecycle.open(activation_id).unwrap();
    lifecycle.accept(&accepted(activation_id, true, 1)).unwrap();

    lifecycle.record_applied_request(1, "request_1").unwrap();
    lifecycle.record_applied_request(2, "request_2").unwrap();
    lifecycle.record_applied_request(2, "request_2").unwrap();

    assert_eq!(
        lifecycle.record_applied_request(4, "request_4"),
        Err(RendezvousLifecycleError::SequenceGap {
            expected: 3,
            received: 4,
        })
    );
    assert_eq!(
        lifecycle.record_applied_request(2, "request_2_changed"),
        Err(RendezvousLifecycleError::SequenceConflict { sequence: 2 })
    );
}

#[test]
fn latency_sample_pairs_activation_first_command_and_first_frame() {
    let activation_started_at = Instant::now();
    let first_command_at = activation_started_at + Duration::from_millis(12);
    let first_frame_at = activation_started_at + Duration::from_millis(20);
    let mut milestones = RendezvousLatencyMilestones::started(activation_started_at);

    milestones.record_first_command(first_command_at).unwrap();
    let sample = milestones.record_first_frame(first_frame_at).unwrap();
    assert_eq!(
        sample.activation_to_first_command,
        Duration::from_millis(12)
    );
    assert_eq!(
        sample.first_command_to_first_frame,
        Duration::from_millis(8)
    );
    assert_eq!(sample.activation_to_first_frame, Duration::from_millis(20));

    milestones
        .record_first_command(first_command_at + Duration::from_secs(1))
        .unwrap();
    assert_eq!(
        milestones
            .record_first_frame(first_frame_at + Duration::from_secs(1))
            .unwrap(),
        sample
    );
}

#[test]
fn acknowledgement_outcome_is_additive_and_old_ack_remains_valid() {
    let activation_id = Uuid::new_v4();
    let old_ack: RunnerToHostFrame = serde_json::from_value(json!({
        "type": "ack",
        "activation_id": activation_id,
        "sequence": 4,
        "idempotency_key": "command-4"
    }))
    .unwrap();
    let RunnerToHostFrame::Ack(old_ack) = old_ack else {
        panic!("expected Ack");
    };
    assert!(old_ack.result.is_none());
    assert!(old_ack.error.is_none());
    assert!(old_ack.has_valid_outcome());

    let ack = RendezvousAck {
        activation_id,
        sequence: 4,
        idempotency_key: "command-4".to_string(),
        result: Some(RendezvousCommandOutcome {
            execution: RendezvousExecution::RuntimeHandled,
            message: "accepted".to_string(),
            idempotency_finalized: true,
        }),
        error: None,
    };
    assert!(ack.has_valid_outcome());
    assert_eq!(
        serde_json::to_value(RunnerToHostFrame::Ack(ack)).unwrap()["result"],
        json!({
            "execution": "runtime_handled",
            "message": "accepted",
            "idempotency_finalized": true
        })
    );

    let invalid = RendezvousAck {
        activation_id,
        sequence: 4,
        idempotency_key: "command-4".to_string(),
        result: None,
        error: Some(RendezvousCommandError {
            code: "execution_failed".to_string(),
            message: "failed".to_string(),
            retryable: false,
        }),
    };
    assert!(invalid.has_valid_outcome());
}
