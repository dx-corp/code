//! Writing rules are enforced before model prose becomes visible or effects run.

#[allow(dead_code)]
mod support;

use std::time::Duration;

use dex_loop::{
    ApprovalMode, Budget, CancellationToken, Context, Event, Exit, TurnContentPolicy, TurnId,
    TurnVoice,
};
use serde_json::json;
use support::*;

fn budget() -> Budget {
    Budget {
        max_steps: 10,
        max_tokens: 1_000_000,
        max_cost_micros: 1_000_000,
        wall: Duration::from_secs(30),
    }
}

fn start(log: &FakeLog, policy: TurnContentPolicy) -> Context {
    log.host_append(Event::UserMessage {
        interaction_mode: dex_loop::InteractionMode::Unspecified,
        turn: TurnId::new("policy-turn"),
        message_id: None,
        principal: alice(),
        text: "Prepare the update".into(),
        attachments: Vec::new(),
        client_tools: Vec::new(),
        authorized_tools: Vec::new(),
        model_binding: None,
        voice: Some(Box::new(TurnVoice {
            policy: Some(policy),
            tone: Vec::new(),
        })),
        approval_mode: ApprovalMode::Interactive,
    });
    log.rehydrate()
}

#[tokio::test]
async fn prohibited_text_split_across_chunks_never_leaks_or_dispatches_a_mutation() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![
            text("We fos"),
            text("ter growth."),
            call("update", json!({})),
            usage(3, 4, 7),
        ],
        vec![text("Updated.")],
    ]);
    let tools = FakeTools::new(vec![write_tool("update")]);
    let mut ctx = start(
        &log,
        TurnContentPolicy {
            forbidden_terms: vec!["foster".into()],
            ..TurnContentPolicy::default()
        },
    );
    assert_eq!(
        engine(&log, &model, &tools, budget())
            .run(&mut ctx, &CancellationToken::new())
            .await,
        Ok(Exit::Failed)
    );
    assert!(
        log.text_writes().is_empty(),
        "unchecked text reached the visible log"
    );
    assert!(
        tools.runs().is_empty(),
        "a rejected step dispatched its mutation"
    );
    assert_eq!(ctx.usage().cost_micros, 7);
    assert_eq!(ctx, log.rehydrate());
}

#[tokio::test]
async fn guidance_only_policy_preserves_incremental_streaming() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![vec![text("Hel"), text("lo")]]);
    let tools = FakeTools::new(vec![]);
    let mut ctx = start(
        &log,
        TurnContentPolicy {
            response_guidance: "Keep it brief.".into(),
            ..TurnContentPolicy::default()
        },
    );
    assert_eq!(
        engine(&log, &model, &tools, budget())
            .run(&mut ctx, &CancellationToken::new())
            .await,
        Ok(Exit::Done)
    );
    assert_eq!(log.text_writes(), vec!["Hel", "lo"]);
}

#[tokio::test]
async fn prohibited_thinking_is_rejected_before_any_visible_prose() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![vec![
        thinking("fos"),
        thinking("ter"),
        text("Safe answer"),
        usage(2, 3, 5),
    ]]);
    let tools = FakeTools::new(vec![]);
    let mut ctx = start(
        &log,
        TurnContentPolicy {
            forbidden_terms: vec!["foster".into()],
            ..Default::default()
        },
    );
    assert_eq!(
        engine(&log, &model, &tools, budget())
            .run(&mut ctx, &CancellationToken::new())
            .await,
        Ok(Exit::Failed)
    );
    assert!(log.text_writes().is_empty());
    assert!(
        !log.events()
            .iter()
            .any(|event| matches!(event, Event::ThinkingDelta { .. } | Event::Final { .. }))
    );
    assert_eq!(ctx.usage().cost_micros, 5);
    assert_eq!(ctx, log.rehydrate());
}

#[tokio::test]
async fn partial_controlled_answer_is_not_published_as_a_cutoff_answer() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![vec![
        text("Deixic partial"),
        usage(2, 3, 5),
        Err(dex_loop::ModelError {
            class: dex_loop::ErrorClass::Truncated,
            message: "Stream ended early".into(),
        }),
    ]]);
    let tools = FakeTools::new(vec![]);
    let mut ctx = start(
        &log,
        TurnContentPolicy {
            required_terms: vec!["Deixic".into()],
            ..Default::default()
        },
    );
    assert_eq!(
        engine(&log, &model, &tools, budget())
            .run(&mut ctx, &CancellationToken::new())
            .await,
        Ok(Exit::Failed)
    );
    assert!(log.text_writes().is_empty());
    assert!(!log.events().iter().any(|event| matches!(
        event,
        Event::Final { .. } | Event::ModelStepCompleted { .. }
    )));
    assert_eq!(ctx.usage().cost_micros, 5);
    assert_eq!(ctx, log.rehydrate());
}

#[tokio::test]
async fn controlled_output_is_bounded_before_publication() {
    let log = FakeLog::default();
    let oversized = "x".repeat(256 * 1024 + 1);
    let model = FakeModel::new(vec![vec![text(&oversized)]]);
    let tools = FakeTools::new(vec![]);
    let mut ctx = start(
        &log,
        TurnContentPolicy {
            forbidden_terms: vec!["foster".into()],
            ..Default::default()
        },
    );
    assert_eq!(
        engine(&log, &model, &tools, budget())
            .run(&mut ctx, &CancellationToken::new())
            .await,
        Ok(Exit::Failed)
    );
    assert!(log.text_writes().is_empty());
    assert!(log.events().iter().any(|event| matches!(event, Event::Error { message, .. } if message.starts_with("content_policy_output_too_large:"))));
}

#[tokio::test]
async fn canceled_controlled_answer_leaves_no_unchecked_text_in_recovery() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![vec![text("Deixic partial"), usage(2, 3, 5)]]).hanging();
    let tools = FakeTools::new(vec![]);
    let mut ctx = start(
        &log,
        TurnContentPolicy {
            required_terms: vec!["Deixic".into()],
            ..Default::default()
        },
    );
    let engine = engine(&log, &model, &tools, budget());
    let cancel = CancellationToken::new();
    let (exit, ()) = tokio::join!(engine.run(&mut ctx, &cancel), async {
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();
    });
    assert_eq!(exit, Ok(Exit::Interrupted));
    assert!(log.text_writes().is_empty());
    assert!(
        !log.events().iter().any(
            |event| matches!(event, Event::ModelStepCompleted { text, .. } if !text.is_empty())
        )
    );
    assert_eq!(ctx.usage().cost_micros, 5);
    assert_eq!(ctx, log.rehydrate());
}

#[tokio::test]
async fn rehydrated_policy_requires_final_terms_sources_and_response_word_limit() {
    for (policy, answer) in [
        (
            TurnContentPolicy {
                required_terms: vec!["Deixic".into()],
                ..Default::default()
            },
            "Evidence only",
        ),
        (
            TurnContentPolicy {
                require_citations: true,
                ..Default::default()
            },
            "No source",
        ),
        (
            TurnContentPolicy {
                allowed_citation_domains: vec!["example.com".into()],
                ..Default::default()
            },
            "[Source](https://evilexample.com/a)",
        ),
        (
            TurnContentPolicy {
                max_response_words: 1,
                ..Default::default()
            },
            "Too many words",
        ),
    ] {
        let log = FakeLog::default();
        start(&log, policy);
        let mut recovered = log.rehydrate();
        let model = FakeModel::new(vec![vec![text(answer)]]);
        let tools = FakeTools::new(vec![]);
        assert_eq!(
            engine(&log, &model, &tools, budget())
                .run(&mut recovered, &CancellationToken::new())
                .await,
            Ok(Exit::Failed)
        );
        assert!(log.text_writes().is_empty());
        assert_eq!(recovered, log.rehydrate());
    }
}

#[tokio::test]
async fn rejected_step_closes_prefetched_reads_without_committing_them_or_writes() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![vec![
        call("search", json!({"key": "a"})),
        text("fos"),
        text("ter"),
        call("update", json!({})),
        usage(2, 3, 5),
    ]])
    .with_chunk_delay(Duration::from_millis(5));
    let tools = FakeTools::new(vec![read_tool("search"), write_tool("update")]);
    let mut ctx = start(
        &log,
        TurnContentPolicy {
            forbidden_terms: vec!["foster".into()],
            ..Default::default()
        },
    );
    assert_eq!(
        engine(&log, &model, &tools, budget())
            .run(&mut ctx, &CancellationToken::new())
            .await,
        Ok(Exit::Failed)
    );
    assert!(log.text_writes().is_empty());
    assert_eq!(tools.run_ids(), vec!["policy-turn-1-0"]);
    assert_eq!(
        log.events()
            .iter()
            .filter(|event| matches!(event, Event::ToolStarted { .. }))
            .count(),
        1
    );
    assert_eq!(
        log.events()
            .iter()
            .filter(|event| matches!(event, Event::ToolFinished { .. }))
            .count(),
        1
    );
    assert!(
        !log.events()
            .iter()
            .any(|event| matches!(event, Event::ModelStepCompleted { .. }))
    );
    assert_eq!(ctx.usage().cost_micros, 5);
    assert_eq!(ctx, log.rehydrate());
}

#[tokio::test]
async fn compliant_controlled_answer_and_progress_are_published_after_validation() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![vec![
        thinking("Checking evidence."),
        text("Dei"),
        text("xic approved. [Source](https://docs.example.com/a)"),
        usage(2, 3, 5),
    ]]);
    let tools = FakeTools::new(vec![]);
    let mut ctx = start(
        &log,
        TurnContentPolicy {
            required_terms: vec!["Deixic".into()],
            require_citations: true,
            allowed_citation_domains: vec!["example.com".into()],
            max_response_words: 3,
            ..Default::default()
        },
    );
    assert_eq!(
        engine(&log, &model, &tools, budget())
            .run(&mut ctx, &CancellationToken::new())
            .await,
        Ok(Exit::Done)
    );
    assert_eq!(
        log.text_writes(),
        vec!["Deixic approved. [Source](https://docs.example.com/a)"]
    );
    assert!(log.events().iter().any(
        |event| matches!(event, Event::ThinkingDelta { text } if text == "Checking evidence.")
    ));
    assert_eq!(ctx, log.rehydrate());
}
