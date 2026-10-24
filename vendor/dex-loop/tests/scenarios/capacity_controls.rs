//! Controls and explicit policy retain precedence over capacity preparation.
use super::*;
use dex_loop::{
    Context, Entry, Message, Model, ModelChunk, RemainingBudget, RequestSize, Summarize, Summary,
    ToolSpec,
};
use futures_util::Stream;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

fn historical_turn(log: &FakeLog) -> Context {
    log.start_turn("old", "Never publish without approval.");
    log.host_append(Event::ModelStepCompleted {
        interruption: None,
        step: 1,
        text: "historical analysis ".repeat(100),
        calls: vec![],
        reasoning: None,
        served: None,
        timing: None,
    });
    log.host_append(Event::Final {
        text: "unfinished".into(),
    });
    log.start_turn("current", "Continue without publishing.")
}

struct PaidSummary(Arc<AtomicUsize>);
impl Summarize for PaidSummary {
    async fn summarize(&self, _: &Context, _: &[Entry]) -> Summary {
        self.0.fetch_add(1, Ordering::SeqCst);
        Summary {
            text: Some("Approval remains required; work is unfinished.".into()),
            usage: dex_loop::Usage {
                cost_micros: 13,
                ..Default::default()
            },
        }
    }
}

fn capacity_error() -> Result<ModelChunk, ModelError> {
    Err(ModelError {
        class: dex_loop::ErrorClass::ContextCapacity,
        message: "request too large".into(),
    })
}

#[derive(Clone)]
struct Measurement {
    principal: Option<PrincipalId>,
    history: Vec<Message>,
    budget: RemainingBudget,
}
struct ControlDuringRead {
    inner: FakeModel,
    log: FakeLog,
    interrupt: bool,
    measurements: Arc<Mutex<Vec<Measurement>>>,
}
impl Model for ControlDuringRead {
    async fn request_size(&self, ctx: &Context, _: &[&ToolSpec]) -> Option<RequestSize> {
        let first = {
            let mut measurements = self.measurements.lock().unwrap();
            let first = measurements.is_empty();
            measurements.push(Measurement {
                principal: ctx.acting_principal().cloned(),
                history: ctx
                    .history()
                    .iter()
                    .map(|entry| entry.message.clone())
                    .collect(),
                budget: ctx.remaining_budget().expect("advisory budget"),
            });
            first
        };
        if first {
            self.log.host_append(if self.interrupt {
                Event::Interrupt { principal: bob() }
            } else {
                Event::Steer {
                    principal: bob(),
                    text: "Retain this newly admitted evidence exactly.".into(),
                }
            });
        }
        let compacted = ctx
            .history()
            .iter()
            .any(|entry| matches!(entry.message, Message::Summary { .. }));
        Some(RequestSize {
            input_units: if compacted { 1_000 } else { 10_000 },
            wire_bytes: if compacted { 2_000 } else { 20_000 },
        })
    }
    fn stream<'a>(
        &'a self,
        ctx: &'a Context,
        tools: &'a [&'a ToolSpec],
    ) -> impl Stream<Item = Result<ModelChunk, ModelError>> + Send + 'a {
        self.inner.stream(ctx, tools)
    }
}

#[tokio::test]
async fn interrupt_during_before_size_read_prevents_paid_summary() {
    let log = FakeLog::default();
    let mut ctx = historical_turn(&log);
    let calls = Arc::new(AtomicUsize::new(0));
    let inner = FakeModel::new(vec![vec![capacity_error()]]);
    let measurements = Arc::new(Mutex::new(Vec::new()));
    let engine = Engine::new(
        log.clone(),
        ControlDuringRead {
            inner: inner.clone(),
            log: log.clone(),
            interrupt: true,
            measurements: measurements.clone(),
        },
        FakeTools::new(vec![]),
        FakeEffects::default(),
        Lexicon::default(),
        budget(),
    )
    .with_compactor(Threshold::for_turns(48 * 1024, PaidSummary(calls.clone())));
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Interrupted)
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "durable interruption wins before paid summarization"
    );
    assert_eq!(ctx.usage().cost_micros, 0);
    assert_eq!(inner.calls(), 1);
    assert_eq!(measurements.lock().unwrap().len(), 1);
    assert!(
        !log.events()
            .iter()
            .any(|event| matches!(event, Event::Compaction { .. } | Event::Usage(_)))
    );
    assert_eq!(ctx, log.rehydrate());
}

#[tokio::test]
async fn steer_during_before_size_read_restarts_comparable_measurement() {
    let log = FakeLog::default();
    let mut ctx = historical_turn(&log);
    let calls = Arc::new(AtomicUsize::new(0));
    let inner = FakeModel::new(vec![
        vec![capacity_error()],
        vec![text("Finished with the admitted evidence.")],
    ]);
    let measurements = Arc::new(Mutex::new(Vec::new()));
    let engine = Engine::new(
        log.clone(),
        ControlDuringRead {
            inner: inner.clone(),
            log: log.clone(),
            interrupt: false,
            measurements: measurements.clone(),
        },
        FakeTools::new(vec![]),
        FakeEffects::default(),
        Lexicon::default(),
        budget(),
    )
    .with_compactor(Threshold::for_turns(48 * 1024, PaidSummary(calls.clone())));
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    let measured = measurements.lock().unwrap();
    assert_eq!(
        measured.len(),
        3,
        "discard the pre-steer measurement and compare two views with the same admitted controls"
    );
    assert_eq!(measured[0].principal, Some(alice()));
    for measurement in &measured[1..] {
        assert_eq!(measurement.principal, Some(bob()));
        assert!(measurement.history.iter().any(|message| matches!(message, Message::User { text, .. } if text == "Retain this newly admitted evidence exactly.")));
    }
    assert_eq!(measured[1].budget, measured[2].budget);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(ctx.usage().cost_micros, 13);
    assert_eq!(
        inner.calls(),
        2,
        "restart preparation without retrying inference before compaction"
    );
    assert_eq!(
        log.events()
            .iter()
            .filter(|event| matches!(event, Event::Compaction { .. }))
            .count(),
        1
    );
    assert_eq!(ctx, log.rehydrate());
}

struct PressureModel(FakeModel);
impl Model for PressureModel {
    async fn needs_compaction(&self, _: &Context, _: &[&ToolSpec]) -> Option<bool> {
        Some(true)
    }
    fn stream<'a>(
        &'a self,
        ctx: &'a Context,
        tools: &'a [&'a ToolSpec],
    ) -> impl Stream<Item = Result<ModelChunk, ModelError>> + Send + 'a {
        self.0.stream(ctx, tools)
    }
}

#[tokio::test]
async fn explicit_policy_controls_proactive_compaction_but_allows_hard_recovery() {
    for (policy, hard_failure, expected_summaries, expected_exit) in [
        ("explicit", false, 0, Exit::Done),
        ("model", false, 1, Exit::Done),
        ("disabled", false, 0, Exit::Done),
        ("explicit", true, 1, Exit::Done),
        ("disabled", true, 0, Exit::Failed),
    ] {
        let log = FakeLog::default();
        let mut ctx = historical_turn(&log);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut steps = vec![];
        if hard_failure {
            steps.push(vec![capacity_error()]);
        }
        steps.push(vec![text("Answer preserves approval constraints.")]);
        let inner = FakeModel::new(steps);
        let summarizer = PaidSummary(calls.clone());
        let threshold = match policy {
            "model" => Threshold::for_model_budget(48 * 1024, summarizer),
            "disabled" => Threshold::for_turns(usize::MAX, summarizer),
            _ => Threshold::for_turns(1_000_000, summarizer),
        };
        let engine = Engine::new(
            log.clone(),
            PressureModel(inner.clone()),
            FakeTools::new(vec![]),
            FakeEffects::default(),
            Lexicon::default(),
            budget(),
        )
        .with_compactor(threshold);
        assert_eq!(
            engine.run(&mut ctx, &CancellationToken::new()).await,
            Ok(expected_exit),
            "{policy}, hard_failure={hard_failure}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            expected_summaries,
            "{policy}, hard_failure={hard_failure}"
        );
        assert_eq!(ctx.usage().cost_micros, 13 * expected_summaries as u64);
        assert_eq!(
            inner.calls(),
            if hard_failure && policy != "disabled" {
                2
            } else {
                1
            }
        );
        assert_eq!(
            log.events()
                .iter()
                .filter(|event| matches!(event, Event::Compaction { .. }))
                .count(),
            expected_summaries
        );
        assert_eq!(ctx, log.rehydrate());
    }
}

#[tokio::test(start_paused = true)]
async fn legacy_and_disabled_policies_do_not_wait_for_unused_pressure_reads() {
    struct SlowPressure {
        inner: FakeModel,
        polls: Arc<AtomicUsize>,
    }
    impl Model for SlowPressure {
        async fn needs_compaction(&self, _: &Context, _: &[&ToolSpec]) -> Option<bool> {
            self.polls.fetch_add(1, Ordering::SeqCst);
            std::future::pending().await
        }
        fn stream<'a>(
            &'a self,
            ctx: &'a Context,
            tools: &'a [&'a ToolSpec],
        ) -> impl Stream<Item = Result<ModelChunk, ModelError>> + Send + 'a {
            self.inner.stream(ctx, tools)
        }
    }
    for policy in ["none", "explicit", "disabled"] {
        let log = FakeLog::default();
        let mut ctx = historical_turn(&log);
        let polls = Arc::new(AtomicUsize::new(0));
        let summaries = Arc::new(AtomicUsize::new(0));
        let inner = FakeModel::new(vec![vec![text("Answer without an unused owner read.")]]);
        let engine = Engine::new(
            log.clone(),
            SlowPressure {
                inner: inner.clone(),
                polls: polls.clone(),
            },
            FakeTools::new(vec![]),
            FakeEffects::default(),
            Lexicon::default(),
            Budget {
                wall: Duration::from_millis(100),
                ..budget()
            },
        );
        let cancel = CancellationToken::new();
        let exit = match policy {
            "none" => engine.run(&mut ctx, &cancel).await,
            _ => {
                engine
                    .with_compactor(Threshold::for_turns(
                        if policy == "disabled" {
                            usize::MAX
                        } else {
                            1_000_000
                        },
                        PaidSummary(summaries.clone()),
                    ))
                    .run(&mut ctx, &cancel)
                    .await
            }
        };
        assert_eq!(
            exit,
            Ok(Exit::Done),
            "{policy}: unused pressure resolution must not exhaust the turn wall"
        );
        assert_eq!(polls.load(Ordering::SeqCst), 0, "{policy}");
        assert_eq!(summaries.load(Ordering::SeqCst), 0);
        assert_eq!(inner.calls(), 1);
        assert_eq!(ctx.usage().cost_micros, 0);
        assert_eq!(ctx, log.rehydrate());
    }
}

#[tokio::test]
async fn capacity_recovery_reduces_history_once_before_retrying_the_same_turn() {
    for second_fails in [false, true] {
        let log = FakeLog::default();
        log.start_turn("old", "Never publish without approval. Budget is $10.");
        log.host_append(Event::ModelStepCompleted {
            interruption: None,
            step: 1,
            text: "historical analysis ".repeat(100),
            calls: vec![],
            reasoning: None,
            served: None,
            timing: None,
        });
        log.host_append(Event::Final {
            text: "unfinished".into(),
        });
        let mut ctx = log.start_turn("current", "continue investigating, do not publish");
        let capacity = || {
            Err(ModelError {
                class: dex_loop::ErrorClass::ContextCapacity,
                message: "request too large".into(),
            })
        };
        let model = FakeModel::new(vec![
            vec![usage(3, 0, 5), capacity()],
            if second_fails {
                vec![capacity()]
            } else {
                vec![text("recovered answer"), usage(7, 2, 11)]
            },
        ]);
        let tools = FakeTools::new(vec![]);
        // History is below the proactive threshold: the complete request's
        // capacity error, not its text, must trigger recovery.
        let engine = support::engine(&log, &model, &tools, budget())
            .with_compactor(Threshold::for_turns(48 * 1024, FakeSummarizer));
        assert_eq!(
            engine.run(&mut ctx, &CancellationToken::new()).await,
            Ok(if second_fails {
                Exit::Failed
            } else {
                Exit::Done
            })
        );
        assert_eq!(model.calls(), 2);
        assert_eq!(ctx.usage().cost_micros, if second_fails { 5 } else { 16 });
        assert_eq!(
            log.events()
                .iter()
                .filter(|e| matches!(e, Event::Compaction { .. }))
                .count(),
            1
        );
        assert_eq!(
            log.events()
                .iter()
                .filter(|e| matches!(e, Event::Error { .. }))
                .count(),
            usize::from(second_fails)
        );
        assert!(matches!(
            &model.seen()[1][0],
            dex_loop::Message::Summary { .. }
        ));
        assert!(model.seen()[1].iter().any(|m| matches!(m,
            dex_loop::Message::User { text, .. } if text == "continue investigating, do not publish")));
        assert_eq!(ctx, log.rehydrate());
        assert!(tools.runs().is_empty());
    }
}
