use super::{Context, Cursor, Message};
use crate::event::{ApprovalMode, Event, PrincipalId, ThreadId, TurnId};

fn user(text: &str) -> Event {
    Event::UserMessage {
        interaction_mode: crate::InteractionMode::Unspecified,
        turn: TurnId::new("t1"),
        message_id: None,
        principal: PrincipalId::new("alice"),
        text: text.into(),
        attachments: vec![],
        client_tools: vec![],
        authorized_tools: Vec::new(),
        model_binding: None,
        context_window_tokens: None,
        voice: None,
        approval_mode: ApprovalMode::Interactive,
        page: None,
    }
}

#[test]
fn a_model_projection_shares_history_until_a_steer_is_written() {
    let mut ctx = Context::new(ThreadId {
        org: "org".into(),
        workspace: "ws".into(),
        thread: "t".into(),
    });
    ctx.observe(Cursor(1), &user("hello"));
    let mut projected = ctx.clone();
    assert!(std::sync::Arc::ptr_eq(&ctx.history, &projected.history));
    projected.observe(
        Cursor(2),
        &Event::StepStarted {
            step: 1,
            control_through: Cursor(1),
        },
    );
    assert!(std::sync::Arc::ptr_eq(&ctx.history, &projected.history));
    assert_eq!(ctx.history().len(), 1);

    let mut steered = ctx.clone();
    steered.observe(
        Cursor(3),
        &Event::Steer {
            principal: PrincipalId::new("alice"),
            text: "more".into(),
        },
    );
    assert!(std::sync::Arc::ptr_eq(&ctx.history, &steered.history));
    steered.observe(
        Cursor(4),
        &Event::StepStarted {
            step: 1,
            control_through: Cursor(3),
        },
    );
    assert!(!std::sync::Arc::ptr_eq(&ctx.history, &steered.history));
    assert_eq!(ctx.history().len(), 1);
    assert_eq!(steered.history().len(), 2);
    let Message::User { text, .. } = &steered.history()[1].message else {
        panic!("steer was not written into the projected history");
    };
    assert_eq!(text, "more");
}
