use dex_loop::{
    AUTO_APPROVER, ApprovalId, ApprovalMode, CallId, Event, HEADLESS_AUTO_APPROVER, PrincipalId,
    ProposedCall, ToolName, args_digest,
};

#[test]
fn auto_approval_receipts_decode_losslessly_and_never_become_control_events() {
    let event = Event::AutoApproved {
        call: CallId::new("t1-1-0"),
        approval: ApprovalId::new("ap-1"),
        args_digest: args_digest(&serde_json::json!({"key": "w"})),
        summary: "Send\0email".into(),
        principal: PrincipalId::new(AUTO_APPROVER),
    };
    let exact = serde_json::to_string(&event).expect("serialize receipt");
    let envelope = serde_json::json!({
        "type": Event::STORED_JSON_V1_TYPE,
        "_dex_event_json_v1": exact,
        "summary": "non-authoritative projection",
    });
    assert_eq!(
        Event::from_stored_json(&envelope, Some("auto_approved")).expect("exact receipt"),
        event
    );
    assert!(Event::from_stored_json(&envelope, Some("approval_decided")).is_err());
    assert!(
        !event.is_control(),
        "engine receipts must not advance the host control cursor"
    );
}

#[test]
fn stored_event_decoder_refuses_corrupt_envelopes_and_call_digests() {
    let event = Event::ModelStepCompleted {
        step: 1,
        text: String::new(),
        calls: vec![ProposedCall::new(
            CallId::new("turn-1-0"),
            ToolName::new("dex.report_feedback"),
            serde_json::json!({"diagnosis": "exact\0value"}),
            PrincipalId::new("alice"),
        )],
        reasoning: None,
        served: None,
        timing: None,
    };
    let mut envelope = serde_json::json!({
        "type": Event::STORED_JSON_V1_TYPE,
        "_dex_event_json_v1": serde_json::to_string(&event).unwrap(),
        "calls": [],
    });
    assert_eq!(
        Event::from_stored_json(&envelope, Some("model_step_completed")).unwrap(),
        event
    );
    assert!(Event::from_stored_json(&envelope, Some("tool_started")).is_err());
    envelope[Event::STORED_JSON_V1_KEY] = serde_json::json!("invalid JSON");
    assert!(Event::from_stored_json(&envelope, Some("model_step_completed")).is_err());
    let mut tampered = serde_json::to_value(&event).unwrap();
    tampered["calls"][0]["args"]["diagnosis"] = serde_json::json!("changed");
    assert!(Event::from_stored_json(&tampered, Some("model_step_completed")).is_err());
}

#[test]
fn approval_mode_defaults_to_interactive_for_old_rows_and_round_trips() {
    let old_row = serde_json::json!({
        "type": "user_message",
        "turn": "t1",
        "principal": "alice",
        "text": "hi",
        "attachments": [],
    });
    match serde_json::from_value::<Event>(old_row).expect("old row deserializes") {
        Event::UserMessage { approval_mode, .. } => {
            assert_eq!(approval_mode, ApprovalMode::Interactive);
        }
        other => panic!("expected UserMessage, got {other:?}"),
    }
    assert_eq!(
        serde_json::to_value(ApprovalMode::Headless).expect("serialize"),
        serde_json::json!("headless")
    );
    assert_eq!(
        serde_json::from_value::<ApprovalMode>(serde_json::json!("interactive"))
            .expect("deserialize"),
        ApprovalMode::Interactive
    );
}

#[test]
fn public_approval_labels_decode_as_the_same_recorded_turn_mode() {
    for (mode, wire_name) in [
        (ApprovalMode::Interactive, "interactive"),
        (ApprovalMode::Headless, "headless"),
    ] {
        let row = serde_json::json!({"type": "user_message", "turn": "t1", "principal": "alice", "text": "hi", "attachments": [], "approval_mode": mode.as_str()});
        assert_eq!(row["approval_mode"], wire_name);
        let decoded = Event::from_stored_json(&row, Some("user_message"))
            .expect("public wire label must decode");
        match decoded {
            Event::UserMessage { approval_mode, .. } => assert_eq!(approval_mode, mode),
            other => panic!("expected UserMessage, got {other:?}"),
        }
    }
}

#[test]
fn legacy_headless_approval_rows_keep_the_principal_and_control_semantics() {
    let digest = "a".repeat(64);
    let row = serde_json::json!({
        "type": "approval_decided", "call": "t1-1-0", "approval": "ap-1",
        "args_digest": digest, "approved": true,
        "principal": "policy:headless_auto_approve",
    });
    let decoded = Event::from_stored_json(&row, Some("approval_decided")).expect("legacy row");
    assert_eq!(
        decoded,
        Event::ApprovalDecided {
            call: CallId::new("t1-1-0"),
            approval: ApprovalId::new("ap-1"),
            args_digest: digest,
            approved: true,
            principal: PrincipalId::new(HEADLESS_AUTO_APPROVER),
        }
    );
    assert!(decoded.is_control());
    assert_eq!(
        serde_json::to_value(decoded).expect("legacy round trip"),
        row
    );
}
