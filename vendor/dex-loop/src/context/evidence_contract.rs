use crate::{
    ApprovalMode, CallId, Cursor, Event, Message, Outcome, Output, PrincipalId, ProposedCall,
    TOOL_EVIDENCE_LIMIT, ThreadId, ToolName, TurnId, rehydrate,
};

#[test]
fn typed_owner_evidence_is_bounded_and_survives_compaction_and_full_replay() {
    let thread = ThreadId {
        org: "org".into(),
        workspace: "ws".into(),
        thread: "thread".into(),
    };
    let mut events = Vec::new();
    fn push(events: &mut Vec<(Cursor, Event)>, event: Event) {
        events.push((Cursor(events.len() as i64 + 1), event));
    }
    // A real thread starts from accepted input before owner completions.
    push(
        &mut events,
        Event::UserMessage {
            turn: TurnId::new("turn-1"),
            message_id: None,
            principal: PrincipalId::new("alice"),
            text: "Read owner results".into(),
            attachments: vec![],
            client_tools: vec![],
            authorized_tools: vec![],
            model_binding: None,
            voice: None,
            approval_mode: ApprovalMode::Interactive,
        },
    );
    for index in 0..=TOOL_EVIDENCE_LIMIT {
        let call = ProposedCall::new(
            CallId::new(format!("call-{index}")),
            ToolName::new("owner.read"),
            serde_json::json!({"id":index}),
            PrincipalId::new("alice"),
        );
        push(
            &mut events,
            Event::ModelStepCompleted {
                step: index as u32,
                text: String::new(),
                calls: vec![call.clone()],
                reasoning: None,
                served: None,
                timing: None,
            },
        );
        push(
            &mut events,
            Event::ToolFinished {
                call: call.id,
                outcome: Outcome::Succeeded,
                output: Output::Text(format!("owner-result-{index}")),
                receipt: None,
                summary: None,
            },
        );
    }
    // A turn-boundary compaction covers completed history. The active
    // turn's exact input remains protected until its terminal event.
    push(
        &mut events,
        Event::Final {
            text: String::new(),
        },
    );
    let covers_to = Cursor(events.len() as i64);
    push(
        &mut events,
        Event::Compaction {
            covers_to_cursor: covers_to,
            summary: "model claims any call passed".into(),
        },
    );
    // Production first rehydrates the accepted input, establishing the
    // control replay floor. UserMessage itself is not a control event.
    // Then its warm actor observes subsequent owner events and compaction.
    let mut warm = rehydrate(thread.clone(), &events[..1]);
    for (cursor, event) in &events[1..] {
        warm.observe(*cursor, event);
    }
    let restarted = rehydrate(thread, &events);
    assert_eq!(warm, restarted);
    assert_eq!(warm.tool_evidence().len(), TOOL_EVIDENCE_LIMIT);
    assert_eq!(warm.tool_evidence()[0].call.id.as_str(), "call-1");
    assert_eq!(warm.tool_evidence()[0].cursor, Cursor(5));
    assert_eq!(warm.tool_evidence().last().unwrap().cursor, Cursor(259));
    assert!(
        warm.history()
            .iter()
            .all(|entry| matches!(entry.message, Message::Summary { .. }))
    );
    // Unmatched completions and summary prose cannot create owner evidence.
    warm.observe(
        Cursor(covers_to.0 + 2),
        &Event::ToolFinished {
            call: CallId::new("forged"),
            outcome: Outcome::Succeeded,
            output: Output::Text("passed".into()),
            receipt: None,
            summary: None,
        },
    );
    assert_eq!(warm.tool_evidence(), restarted.tool_evidence());
}
