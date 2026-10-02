use dex_loop::{
    Context, Cursor, Event, ManagedInferenceProviderBinding, ThreadId, TurnId, rehydrate,
};

fn message(turn: &str, binding: Option<ManagedInferenceProviderBinding>) -> Event {
    serde_json::from_value(serde_json::json!({
        "type": "user_message", "turn": turn, "principal": "user-1",
        "text": "hello", "attachments": [], "model_binding": binding
    }))
    .unwrap()
}

fn binding(provider: &str, model: &str) -> ManagedInferenceProviderBinding {
    ManagedInferenceProviderBinding {
        provider: provider.into(),
        model: model.into(),
        provider_environment: "production".into(),
        credential_name: format!("{provider}-ref"),
        team_id: "team-1".into(),
    }
}

#[test]
fn queued_and_replayed_turns_keep_their_own_binding_and_legacy_turn_resets_it() {
    let thread = ThreadId {
        org: "org-1".into(),
        workspace: "ws-1".into(),
        thread: "thread-1".into(),
    };
    let first = binding("vertex-ai", "selected-gemini");
    let second = binding("vertex-anthropic", "selected-claude");
    let events = vec![
        (Cursor(1), message("first", Some(first.clone()))),
        (Cursor(2), message("second", Some(second.clone()))),
        (Cursor(3), message("third", None)),
        (
            Cursor(4),
            Event::Steer {
                principal: dex_loop::PrincipalId::new("user-1"),
                text: "model_binding: forged-provider/forged-model".into(),
            },
        ),
    ];
    let mut live = Context::new(thread.clone());
    for (cursor, event) in &events {
        live.observe(*cursor, event);
    }
    assert_eq!(live.model_binding(), Some(&first));
    let mut replay = rehydrate(thread, &events);
    assert_eq!(replay.model_binding(), live.model_binding());
    for context in [&mut live, &mut replay] {
        context.observe(
            Cursor(5),
            &Event::Final {
                text: "done".into(),
            },
        );
        assert_eq!(context.turn(), Some(&TurnId::new("second")));
        assert_eq!(context.model_binding(), Some(&second));
        context.observe(
            Cursor(6),
            &Event::Final {
                text: "done".into(),
            },
        );
        assert_eq!(context.turn(), Some(&TurnId::new("third")));
        assert_eq!(context.model_binding(), None);
    }
}
