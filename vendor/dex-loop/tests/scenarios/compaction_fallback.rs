//! A declined model summary falls back to the deterministic recap, so a
//! turn whose history outgrew the model's context still answers.

use super::*;

/// A model summary that declines no longer ends the turn: the deterministic
/// recap of the same history replaces it, keeping the person's own words, and
/// the turn answers.
#[tokio::test]
async fn a_declined_model_summary_falls_back_to_the_recap_and_the_turn_answers() {
    struct Decline;
    impl dex_loop::Summarize for Decline {
        async fn summarize(
            &self,
            _: &dex_loop::Context,
            _: &[dex_loop::Entry],
        ) -> dex_loop::Summary {
            dex_loop::Summary::default()
        }
    }
    let log = FakeLog::default();
    log.start_turn("old", "Never publish without approval. Budget is $10.");
    log.host_append(Event::final_text("analysis unfinished"));
    let mut ctx = log.start_turn("current", "continue investigating");
    let model = FakeModel::new(vec![vec![text("recovered answer")]]);
    let tools = FakeTools::new(vec![]);
    let engine = support::engine(&log, &model, &tools, budget())
        .with_compactor(Threshold::new(1, 1, Decline));
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(model.calls(), 1);
    let recap = model.seen()[0]
        .iter()
        .find_map(|message| match message {
            dex_loop::Message::Summary { text } => Some(text.clone()),
            _ => None,
        })
        .expect("the recap replaced the earlier turn");
    assert!(recap.contains("Never publish without approval. Budget is $10."));
    assert!(
        !log.events()
            .iter()
            .any(|event| matches!(event, Event::Error { .. }))
    );
    assert_eq!(ctx, log.rehydrate());
}

/// The Slack DM of 2026-11-16: one long personal conversation outgrew the
/// model's context, the model summary declined, and the turn ended with "start
/// a new conversation", which a DM cannot do. The recap now replaces the
/// history and the question is answered.
#[tokio::test]
async fn a_dm_at_the_context_limit_with_a_failed_model_summary_still_answers() {
    struct Decline;
    impl dex_loop::Summarize for Decline {
        async fn summarize(
            &self,
            _: &dex_loop::Context,
            _: &[dex_loop::Entry],
        ) -> dex_loop::Summary {
            dex_loop::Summary::default()
        }
    }
    let log = FakeLog::default();
    for turn in 0..6 {
        log.start_turn(&format!("old-{turn}"), &format!("earlier question {turn}"));
        log.host_append(Event::ModelStepCompleted {
            interruption: None,
            step: 1,
            text: "A long earlier answer about vendors and their reviews. ".repeat(80),
            calls: vec![],
            reasoning: None,
            served: None,
            timing: None,
        });
        log.host_append(Event::final_text("A long earlier answer."));
    }
    let mut ctx = log.start_turn(
        "current",
        "which of our vendors need a security review soonest, and can you start it?",
    );
    let model = FakeModel::new(vec![
        vec![Err(ModelError {
            class: dex_loop::ErrorClass::ContextCapacity,
            message: "context_length_exceeded".into(),
        })],
        vec![text(
            "Acme and Globex are due first; I started Acme's review.",
        )],
    ]);
    let tools = FakeTools::new(vec![]);
    let engine = support::engine(&log, &model, &tools, budget())
        .with_compactor(Threshold::for_turns(48 * 1024, Decline));
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(model.calls(), 2);
    assert!(
        log.events()
            .iter()
            .any(|event| matches!(event, Event::Compaction { .. }))
    );
    assert!(
        !log.events()
            .iter()
            .any(|event| matches!(event, Event::Error { .. }))
    );
    assert!(log.events().iter().any(|event| matches!(
        event,
        Event::Final { text, .. } if text.starts_with("Acme and Globex")
    )));
    // The question itself is still sent exactly.
    assert!(model.seen()[1].iter().any(|message| matches!(
        message,
        dex_loop::Message::User { text, .. }
            if text == "which of our vendors need a security review soonest, and can you start it?"
    )));
    assert_eq!(ctx, log.rehydrate());
}
