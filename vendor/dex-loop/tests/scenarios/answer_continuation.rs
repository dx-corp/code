//! Interrupted answer retention and bounded continuation.

use super::*;

// 8d3. A stream that fails after the customer saw text keeps that text as
// the answer, marked as cut off: a long answer that loses its stream near
// the end must not vanish.
#[tokio::test]
async fn a_stream_that_fails_after_text_keeps_the_answer_marked_cut_off() {
    let (log, model, exit) = run_to_budget(
        budget(),
        FakeModel::new(vec![vec![
            text("A long answer, "),
            text("nearly done"),
            usage(10, 5, 100),
            Err(ModelError {
                class: dex_loop::ErrorClass::Unknown,
                message: "provider_stream_timeout: provider stream timed out".into(),
            }),
        ]]),
    )
    .await;
    assert_eq!(exit, Exit::Done);
    let kept = format!("A long answer, nearly done{CUT_OFF_NOTICE}");
    assert_eq!(
        log.shapes_after(2),
        strings(&[
            &format!("delta:{kept}"),
            "usage:15",
            &format!("completed:{kept}:[]"),
            &format!("final:{kept}"),
        ])
    );
    assert_eq!(
        model.calls(),
        1,
        "text was shown, so the step is not retried"
    );
}

#[tokio::test]
async fn a_transient_stream_failure_continues_the_visible_answer_once_without_tools() {
    for class in [
        dex_loop::ErrorClass::Transport,
        dex_loop::ErrorClass::Truncated,
    ] {
        let log = FakeLog::default();
        let model = FakeModel::new(vec![
            vec![
                text("The first finding is "),
                Err(ModelError {
                    class,
                    message: "provider stream broke".into(),
                }),
            ],
            vec![text("confirmed by the owner record.")],
        ]);
        let tools = FakeTools::new(vec![write_tool("update")]);
        let engine = engine(&log, &model, &tools, budget());
        let mut ctx = log.start_turn("t1", "Explain the findings");
        assert_eq!(
            engine.run(&mut ctx, &CancellationToken::new()).await,
            Ok(Exit::Done)
        );
        assert_eq!(
            model.calls(),
            2,
            "{class:?}: the interrupted answer must continue"
        );
        assert!(
            model.offered()[1].is_empty(),
            "continuation cannot run tools"
        );
        assert!(tools.runs().is_empty());
        assert_eq!(
            view(&model.seen()[1]),
            strings(&[
                "user:Explain the findings",
                "assistant:The first finding is :[]",
            ])
        );
        assert!(
            !log.shapes()
                .iter()
                .any(|shape| shape.contains(CUT_OFF_NOTICE))
        );
        assert!(
            matches!(log.events().last(), Some(Event::Final { text }) if text == "The first finding is confirmed by the owner record."),
            "Final must retain the prefix for browser and channel replay"
        );
        assert_eq!(ctx, log.rehydrate());
    }
}

#[tokio::test]
async fn an_interrupted_answer_gets_only_one_continuation() {
    let log = FakeLog::default();
    let failure = || {
        Err(ModelError {
            class: dex_loop::ErrorClass::Transport,
            message: "provider stream broke".into(),
        })
    };
    let model = FakeModel::new(vec![
        vec![text("First part, "), failure()],
        vec![text("second part"), failure()],
        vec![text("must never be requested")],
    ]);
    let tools = FakeTools::new(vec![]);
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "explain");
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(model.calls(), 2);
    assert!(
        matches!(log.events().last(), Some(Event::Final { text }) if text == &format!("First part, second part{CUT_OFF_NOTICE}"))
    );
    assert_eq!(ctx, log.rehydrate());
}

#[tokio::test]
async fn answer_continuation_never_executes_a_proposed_mutation() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![
            text("Known facts: "),
            call("update", json!({"key":"before-cut"})),
            Err(ModelError {
                class: dex_loop::ErrorClass::Transport,
                message: "provider stream broke".into(),
            }),
        ],
        vec![call("update", json!({"key":"on-continuation"}))],
    ]);
    let tools = FakeTools::new(vec![write_tool("update")]);
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "explain");
    let _ = engine.run(&mut ctx, &CancellationToken::new()).await;
    assert_eq!(model.calls(), 2);
    assert!(model.offered()[1].is_empty());
    assert!(tools.runs().is_empty());
    assert!(
        !log.events()
            .iter()
            .any(|event| matches!(event, Event::ToolStarted { .. }))
    );
    assert_eq!(ctx, log.rehydrate());
}

fn log_interrupted_answer(log: &FakeLog) {
    log.start_turn("t1", "explain");
    log.host_append(Event::StepStarted {
        step: 1,
        control_through: Cursor(1),
    });
    // Exercise durable decoding as a restarted runtime sees it, including
    // the additive field that older successful events do not contain.
    log.host_append(
        serde_json::from_value(json!({
            "type": "model_step_completed", "step": 1, "text": "First part, ",
            "calls": [], "interruption": "transport",
        }))
        .expect("durable interrupted step"),
    );
}

#[tokio::test]
async fn interrupted_answer_replay_continues_with_the_prefix_and_no_tools() {
    let log = FakeLog::default();
    log_interrupted_answer(&log);
    let model = FakeModel::new(vec![vec![text("the rest.")]]);
    let tools = FakeTools::new(vec![write_tool("update")]);
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.rehydrate();
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(model.calls(), 1);
    assert!(model.offered()[0].is_empty());
    assert!(
        matches!(log.events().last(), Some(Event::Final { text }) if text == "First part, the rest.")
    );
    assert_eq!(ctx, log.rehydrate());
}

#[tokio::test]
async fn a_crashed_continuation_does_not_get_another_model_call() {
    let log = FakeLog::default();
    log_interrupted_answer(&log);
    log.host_append(Event::StepStarted {
        step: 2,
        control_through: Cursor(3),
    });
    log.host_append(Event::TextDelta {
        text: "uncommitted continuation".into(),
    });
    let model = FakeModel::new(vec![vec![text("must not be requested")]]);
    let tools = FakeTools::new(vec![]);
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.rehydrate();
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(
        model.calls(),
        0,
        "replay must preserve the consumed continuation allowance"
    );
    assert!(
        matches!(log.events().last(), Some(Event::Final { text }) if text == &format!("First part, {CUT_OFF_NOTICE}"))
    );
    assert_eq!(ctx, log.rehydrate());
}

#[tokio::test]
async fn a_user_steer_supersedes_pending_answer_continuation() {
    let log = FakeLog::default();
    log_interrupted_answer(&log);
    log.host_append(Event::Steer {
        principal: alice(),
        text: "Answer a different question".into(),
    });
    let model = FakeModel::new(vec![vec![text("The new answer.")]]);
    let tools = FakeTools::new(vec![write_tool("update")]);
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.rehydrate();
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(model.calls(), 1);
    assert!(
        !model.offered()[0].is_empty(),
        "the user's new request retains ordinary tool admission"
    );
    assert!(
        matches!(log.events().last(), Some(Event::Final { text }) if text == "The new answer.")
    );
    assert_eq!(ctx, log.rehydrate());
}

#[tokio::test]
async fn cancellation_prevents_pending_answer_continuation() {
    let log = FakeLog::default();
    log_interrupted_answer(&log);
    let model = FakeModel::new(vec![vec![text("must not be requested")]]);
    let tools = FakeTools::new(vec![]);
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.rehydrate();
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(engine.run(&mut ctx, &cancel).await, Ok(Exit::Interrupted));
    assert_eq!(model.calls(), 0);
    assert!(
        !log.events()
            .iter()
            .any(|event| matches!(event, Event::Final { .. }))
    );
    assert_eq!(ctx, log.rehydrate());
}

#[tokio::test]
async fn exhausted_budget_prevents_pending_answer_continuation() {
    for axis in ["steps", "tokens", "cost", "wall"] {
        let log = FakeLog::default();
        log_interrupted_answer(&log);
        let model = FakeModel::new(vec![vec![text("must not be requested")]]);
        let tools = FakeTools::new(vec![]);
        let mut cap = budget();
        match axis {
            "steps" => cap.max_steps = 0,
            "tokens" => cap.max_tokens = 0,
            "cost" => cap.max_cost_micros = 0,
            "wall" => cap.wall = Duration::ZERO,
            _ => unreachable!(),
        }
        let engine = engine(&log, &model, &tools, cap);
        let mut ctx = log.rehydrate();
        assert_eq!(
            engine.run(&mut ctx, &CancellationToken::new()).await,
            Ok(Exit::Failed),
            "{axis}"
        );
        assert_eq!(model.calls(), 0, "{axis}");
        assert!(matches!(
            log.events().last(),
            Some(Event::Error {
                code: dex_loop::ErrorCode::BudgetExhausted,
                ..
            })
        ));
        assert_eq!(ctx, log.rehydrate());
    }
}
