//! Owner-tool authority belongs to the admitted message, survives replay,
//! and cannot be inherited by a steering actor or a queued turn.
use dex_loop::{Context, Cursor, Event, PrincipalId, ThreadId, ToolName, TurnId};

fn message(turn: &str, principal: &str, tools: &[&str]) -> Event {
    Event::UserMessage {
        turn: TurnId::new(turn),
        message_id: None,
        principal: PrincipalId::new(principal),
        text: "Find the demo request".into(),
        attachments: vec![],
        client_tools: vec![],
        authorized_tools: tools.iter().map(|name| ToolName::new(*name)).collect(),
        model_binding: None,
        voice: None,
        approval_mode: dex_loop::ApprovalMode::Interactive,
    }
}

fn context() -> Context {
    Context::new(ThreadId {
        org: "org".into(),
        workspace: "workspace".into(),
        thread: "thread".into(),
    })
}

#[test]
fn authority_replays_with_the_original_principal_and_does_not_leak_to_the_queue() {
    let events = [
        message("first", "reader", &["capture_form.get_submission"]),
        Event::Steer {
            principal: PrincipalId::new("writer"),
            text: "Read it for me".into(),
        },
        Event::StepStarted {
            step: 1,
            control_through: Cursor(2),
        },
        message("second", "reader", &[]),
    ];
    let mut live = context();
    for (index, event) in events.iter().enumerate() {
        live.observe(Cursor(index as i64 + 1), event);
    }
    assert_eq!(live.acting_principal(), Some(&PrincipalId::new("writer")));
    assert_eq!(
        live.authorized_principal(),
        Some(&PrincipalId::new("reader"))
    );
    assert_eq!(
        live.authorized_tools(),
        &[ToolName::new("capture_form.get_submission")]
    );
    let mut replay = context();
    for (index, event) in events.iter().enumerate() {
        let persisted = serde_json::to_vec(event).expect("persist event");
        let decoded = serde_json::from_slice(&persisted).expect("decode event");
        replay.observe(Cursor(index as i64 + 1), &decoded);
    }
    assert_eq!(replay.authorized_principal(), live.authorized_principal());
    assert_eq!(replay.authorized_tools(), live.authorized_tools());
    replay.observe(
        Cursor(5),
        &Event::Final {
            text: "done".into(),
        },
    );
    assert_eq!(replay.turn(), Some(&TurnId::new("second")));
    assert_eq!(
        replay.authorized_principal(),
        Some(&PrincipalId::new("reader"))
    );
    assert!(replay.authorized_tools().is_empty());
}

#[test]
fn older_log_rows_have_no_owner_tool_authority() {
    let mut persisted = serde_json::to_value(message("legacy", "reader", &[])).expect("event");
    assert!(
        persisted
            .as_object_mut()
            .expect("object")
            .remove("authorized_tools")
            .is_some()
    );
    let event: Event = serde_json::from_value(persisted).expect("legacy event");
    let mut restored = context();
    restored.observe(Cursor(1), &event);
    assert!(restored.authorized_tools().is_empty());
}
