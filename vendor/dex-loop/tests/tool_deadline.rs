//! Regression: one tool call must not hold `Engine::run` open past the
//! engine's per-call deadline, and never past `budget.wall`.
//!
//! On 2026-09-29 every Dex computer turn in production stalled inside one
//! `dex.compute` call: tool-execution never dispatched the approved command,
//! and the engine sat in `Tools::run` with nothing bounding it (`budget.wall`
//! is only checked between steps; the mutation path runs with a cancel token
//! that never fires). The turn only ended when dex-tools' own 10-minute
//! remote wait gave up. These tests pin the engine's own bound: a call that
//! overruns is finished as `Failed` (a read: safe to retry) or `Unknown` (a
//! mutation: recorded in the ledger), the model gets that result on its next
//! step, and a call never outlives the wall budget. Paused virtual time keeps
//! them instant.

// Each `tests/*.rs` file is its own crate; this one uses a subset of the
// shared helpers.
#[allow(dead_code)]
mod support;

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use dex_loop::{
    Budget, CallId, CancellationToken, Claim, Context, Cursor, Effects, Engine, ErrorCode, Event,
    Exit, Fenced, Lexicon, Log, Message, Outcome, Output, ProposedCall, ToolName, ToolResult,
};
use serde_json::json;
use support::*;

const NEVER: Duration = Duration::from_secs(60 * 60);

fn assert_complete_tool_history(ctx: &Context, expected: &[CallId]) {
    let proposed: Vec<_> = ctx
        .history()
        .iter()
        .filter_map(|entry| match &entry.message {
            Message::Assistant { calls, .. } => Some(calls.iter().map(|call| call.id.clone())),
            _ => None,
        })
        .flatten()
        .collect();
    let completed: Vec<_> = ctx
        .history()
        .iter()
        .filter_map(|entry| match &entry.message {
            Message::Tool { call, .. } => Some(call.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(proposed, expected, "all proposed calls remain in history");
    assert_eq!(
        completed, expected,
        "every call has exactly one result in history"
    );
}

fn budget_with_wall(wall: Duration) -> Budget {
    Budget {
        max_steps: 10,
        max_tokens: u64::MAX,
        max_cost_micros: u64::MAX,
        wall,
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_read_that_overruns_the_deadline_is_finished_failed_and_the_turn_continues() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![call("search", json!({"key": "slow"}))],
        vec![text("done")],
    ]);
    let tools = FakeTools::new(vec![read_tool("search")]).delay("slow", NEVER);
    let engine = engine(&log, &model, &tools, budget_with_wall(NEVER))
        .with_tool_call_deadline(Duration::from_millis(50));
    let mut ctx = log.start_turn("t1", "look");

    let exit = tokio::time::timeout(
        Duration::from_secs(5),
        engine.run(&mut ctx, &CancellationToken::new()),
    )
    .await
    .expect("Engine::run must return once the call's deadline elapses");

    assert_eq!(exit, Ok(Exit::Done));
    assert_eq!(
        log.shapes_after(1),
        strings(&[
            "step:1",
            "started:t1-1-0",
            "completed::[t1-1-0]",
            "finished:t1-1-0:err",
            "step:2",
            "delta:done",
            "completed:done:[]",
            "final:done",
        ])
    );
    // The model sees the timeout as this call's result, not a dangling call.
    assert_eq!(
        view(&model.seen()[1]),
        strings(&[
            "user:look",
            "assistant::[t1-1-0]",
            "tool:t1-1-0:err:not completed: the call did not finish within Dex's time limit; it is safe to try again",
        ])
    );
    assert!(
        tools.runs().is_empty(),
        "the overrunning read was dropped, not awaited to completion"
    );
    assert_eq!(log.rehydrate(), ctx);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_mutation_that_overruns_the_deadline_is_recorded_unknown() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![call("update", json!({"key": "slow"}))],
        vec![text("checked")],
    ]);
    let tools = FakeTools::new(vec![write_tool("update")]).delay("slow", NEVER);
    let effects = FakeEffects::default();
    let engine = engine_with(&log, &model, &tools, &effects, budget_with_wall(NEVER))
        .with_tool_call_deadline(Duration::from_millis(50))
        // Composing a compactor must preserve the configured per-call bound.
        .with_compactor(dex_loop::Threshold::new(usize::MAX, 1, FakeSummarizer));
    let mut ctx = log.start_turn("t1", "change it");

    let exit = tokio::time::timeout(
        Duration::from_secs(5),
        engine.run(&mut ctx, &CancellationToken::new()),
    )
    .await
    .expect("Engine::run must return once the call's deadline elapses");

    assert_eq!(exit, Ok(Exit::Done));
    assert_eq!(
        log.shapes_after(1),
        strings(&[
            "step:1",
            "completed::[t1-1-0]",
            "started:t1-1-0",
            "finished:t1-1-0:unknown",
            "step:2",
            "delta:checked",
            "completed:checked:[]",
            "final:checked",
        ])
    );
    // The ledger holds the same answer, so a resume adopts it instead of
    // dispatching the mutation again.
    let recorded = effects
        .recorded(&call_id("t1", 1, 0))
        .flatten()
        .expect("the timed-out mutation is recorded under its claim");
    assert_eq!(recorded.outcome, Outcome::Unknown);
    assert_eq!(
        recorded.output,
        Output::Text(
            "outcome unknown: the call did not finish within Dex's time limit; check whether it took effect before trying again"
                .into()
        )
    );
    assert_eq!(log.rehydrate(), ctx);
}

// The tool timer and between-step wall check must share the same clock.
// Advancing virtual time must exhaust the wall before another model step.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_call_never_outlives_the_wall_budget() {
    // The per-call deadline is generous; the wall budget is not. The call is
    // cut at the wall, its result appended, and the turn ends on the wall
    // budget -- with the call's result in history, so the thread stays well
    // formed for the next turn.
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![call("search", json!({"key": "slow"}))],
        vec![text("never reached")],
    ]);
    let tools = FakeTools::new(vec![read_tool("search")]).delay("slow", NEVER);
    let engine = engine(
        &log,
        &model,
        &tools,
        budget_with_wall(Duration::from_millis(100)),
    )
    .with_tool_call_deadline(NEVER);
    let mut ctx = log.start_turn("t1", "look");

    let exit = tokio::time::timeout(
        Duration::from_secs(5),
        engine.run(&mut ctx, &CancellationToken::new()),
    )
    .await
    .expect("Engine::run must return once budget.wall elapses, even mid-call");

    assert_eq!(exit, Ok(Exit::Failed));
    assert_eq!(
        log.shapes_after(1),
        strings(&[
            "step:1",
            "started:t1-1-0",
            "completed::[t1-1-0]",
            "finished:t1-1-0:err",
            "error:budget_exhausted:wall budget exhausted: 100ms",
        ])
    );
    assert!(log.events().iter().any(|event| matches!(
        event,
        Event::Error {
            class: Some(dex_loop::ErrorClass::BudgetExhausted),
            code: ErrorCode::BudgetExhausted,
            ..
        }
    )));
    assert_eq!(model.seen().len(), 1, "no model step after the wall budget");
}

#[tokio::test(flavor = "current_thread")]
async fn a_read_that_consumes_the_wall_does_not_start_the_next_mutation() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![vec![
        call("search", json!({"key": "slow"})),
        call("update", json!({"key": "fresh"})),
    ]]);
    let mutation_hits = Arc::new(AtomicUsize::new(0));
    let hits = mutation_hits.clone();
    let tools = FakeTools::new(vec![read_tool("search"), write_tool("update")])
        .delay("slow", NEVER)
        .on_run(move |call| {
            if call.tool.as_str() == "update" {
                // Runs on the future's first poll, including the first poll
                // of an otherwise expired Tokio timeout.
                hits.fetch_add(1, Ordering::SeqCst);
            }
        });
    let effects = FakeEffects::default();
    let engine = engine_with(
        &log,
        &model,
        &tools,
        &effects,
        budget_with_wall(Duration::from_millis(100)),
    )
    .with_tool_call_deadline(NEVER);
    let mut ctx = log.start_turn("t1", "read then change it");

    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Failed)
    );
    assert_eq!(mutation_hits.load(Ordering::SeqCst), 0);
    assert_eq!(
        effects.recorded(&call_id("t1", 1, 1)),
        None,
        "fresh mutation must not even claim its effect"
    );
    assert_eq!(
        log.shapes_after(1),
        strings(&[
            "step:1",
            "started:t1-1-0",
            "completed::[t1-1-0,t1-1-1]",
            "finished:t1-1-0:err",
            "finished:t1-1-1:err",
            "error:budget_exhausted:wall budget exhausted: 100ms",
        ])
    );
    assert_complete_tool_history(&ctx, &[call_id("t1", 1, 0), call_id("t1", 1, 1)]);
    assert_eq!(ctx, log.rehydrate());
}

#[tokio::test(flavor = "current_thread")]
async fn a_read_that_consumes_the_wall_does_not_request_client_execution_or_approval() {
    for read_only in [true, false] {
        let log = FakeLog::default();
        let model = FakeModel::new(vec![vec![
            call("search", json!({"key": "slow"})),
            call("browser.act", json!({})),
        ]]);
        let tools = FakeTools::new(vec![
            read_tool("search"),
            client_executed_tool("browser.act", read_only),
        ])
        .delay("slow", NEVER);
        let engine = engine(
            &log,
            &model,
            &tools,
            budget_with_wall(Duration::from_millis(100)),
        )
        .with_tool_call_deadline(NEVER);
        let mut ctx = log.start_turn("t1", "read then use my browser");

        assert_eq!(
            engine.run(&mut ctx, &CancellationToken::new()).await,
            Ok(Exit::Failed)
        );
        assert!(log.events().iter().all(|event| !matches!(
            event,
            Event::ClientToolRequested { .. } | Event::ApprovalRequested { .. }
        )));
        assert!(log.events().iter().any(|event| matches!(event,
            Event::ToolFinished { call, outcome: Outcome::Failed, .. }
            if call == &call_id("t1", 1, 1)
        )));
        assert!(log.events().iter().any(|event| matches!(
            event,
            Event::Error {
                code: ErrorCode::BudgetExhausted,
                ..
            }
        )));
        assert_complete_tool_history(&ctx, &[call_id("t1", 1, 0), call_id("t1", 1, 1)]);
        assert_eq!(ctx, log.rehydrate());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn exhausted_wall_still_adopts_an_already_started_mutations_ledger_result() {
    let log = FakeLog::default();
    log.start_turn("t1", "resume the calls");
    let read = ProposedCall::new(
        call_id("t1", 1, 0),
        ToolName::new("search"),
        json!({"key": "slow"}),
        alice(),
    );
    let write = ProposedCall::new(
        call_id("t1", 1, 1),
        ToolName::new("update"),
        json!({"key": "existing"}),
        alice(),
    );
    log.host_append(Event::StepStarted {
        step: 1,
        control_through: Cursor::START,
    });
    log.host_append(Event::ModelStepCompleted {
        served: None,
        step: 1,
        text: String::new(),
        calls: vec![read, write.clone()],
        reasoning: None,
        timing: None,
    });
    log.host_append(Event::ToolStarted {
        call: write.id.clone(),
        tool: write.tool.clone(),
        label: "Update".into(),
        principal: write.principal.clone(),
    });
    let recorded = ToolResult::unknown("executor lost the result; check the effect");
    let effects = FakeEffects::default().seed(write.id.clone(), Some(recorded.clone()));
    let model = FakeModel::new(vec![]);
    let tools =
        FakeTools::new(vec![read_tool("search"), write_tool("update")]).delay("slow", NEVER);
    let engine = engine_with(
        &log,
        &model,
        &tools,
        &effects,
        budget_with_wall(Duration::from_millis(100)),
    )
    .with_tool_call_deadline(NEVER);
    let mut ctx = log.rehydrate();

    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Failed)
    );
    assert!(
        tools.runs().is_empty(),
        "historical mutation must not execute again"
    );
    assert_eq!(effects.recorded(&write.id), Some(Some(recorded.clone())));
    assert!(log.events().iter().any(|event| matches!(event,
        Event::ToolFinished { call, outcome, output, .. }
        if call == &write.id && outcome == &recorded.outcome && output == &recorded.output
    )));
    assert!(log.events().iter().any(|event| matches!(
        event,
        Event::Error {
            code: ErrorCode::BudgetExhausted,
            ..
        }
    )));
    assert_complete_tool_history(&ctx, &[call_id("t1", 1, 0), call_id("t1", 1, 1)]);
    assert_eq!(ctx, log.rehydrate());
}

// Exercise expiry inside claim/start persistence, rather than host scheduling
// exhausting the wall before the engine can claim the mutation.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn mutation_claim_or_start_persistence_consuming_wall_never_polls_the_effect() {
    struct SlowClaim {
        effects: FakeEffects,
        delay: bool,
    }
    impl Effects for SlowClaim {
        async fn claim(&self, call: &ProposedCall) -> Result<Claim, Fenced> {
            if self.delay {
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            self.effects.claim(call).await
        }
        async fn record(&self, call: &CallId, result: &ToolResult) -> Result<(), Fenced> {
            self.effects.record(call, result).await
        }
    }
    struct SlowStart {
        log: FakeLog,
        delay: bool,
    }
    impl Log for SlowStart {
        async fn append(&self, events: &[Event]) -> Result<Vec<Cursor>, Fenced> {
            if self.delay
                && events
                    .iter()
                    .any(|event| matches!(event, Event::ToolStarted { .. }))
            {
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            self.log.append(events).await
        }
        async fn append_text(&self, text: String) -> Result<(), Fenced> {
            self.log.append_text(text).await
        }
        async fn control_since(&self, after: Cursor) -> Result<Vec<(Cursor, Event)>, Fenced> {
            self.log.control_since(after).await
        }
    }
    for delay_claim in [true, false] {
        let log = FakeLog::default();
        let model = FakeModel::new(vec![vec![call("update", json!({"key": "fresh"}))]]);
        let hits = Arc::new(AtomicUsize::new(0));
        let effect_hits = hits.clone();
        let tools = FakeTools::new(vec![write_tool("update")]).on_run(move |_| {
            effect_hits.fetch_add(1, Ordering::SeqCst);
        });
        let effects = FakeEffects::default();
        let engine = Engine::new(
            SlowStart {
                log: log.clone(),
                delay: !delay_claim,
            },
            model,
            tools,
            SlowClaim {
                effects: effects.clone(),
                delay: delay_claim,
            },
            Lexicon::default(),
            budget_with_wall(Duration::from_millis(100)),
        )
        .with_tool_call_deadline(NEVER);
        let mut ctx = log.start_turn("t1", "change it");

        assert_eq!(
            engine.run(&mut ctx, &CancellationToken::new()).await,
            Ok(Exit::Failed)
        );
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        assert_eq!(
            log.events()
                .iter()
                .any(|event| matches!(event, Event::ToolStarted { .. })),
            !delay_claim
        );
        let recorded = effects
            .recorded(&call_id("t1", 1, 0))
            .flatten()
            .expect("claimed but undispatched result must be durable");
        assert_eq!(recorded.outcome, Outcome::Failed);
        assert!(
            matches!(recorded.output, Output::Text(ref text) if text.starts_with("not executed:"))
        );
        assert_complete_tool_history(&ctx, &[call_id("t1", 1, 0)]);
        assert_eq!(ctx, log.rehydrate());
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_call_that_finishes_in_time_is_unaffected() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![call("search", json!({"key": "quick"}))],
        vec![text("done")],
    ]);
    let tools = FakeTools::new(vec![read_tool("search")]).delay("quick", Duration::from_millis(10));
    let engine = engine(&log, &model, &tools, budget_with_wall(NEVER))
        .with_tool_call_deadline(Duration::from_millis(50));
    let mut ctx = log.start_turn("t1", "look");

    let exit = engine.run(&mut ctx, &CancellationToken::new()).await;

    assert_eq!(exit, Ok(Exit::Done));
    assert_eq!(tools.run_ids(), strings(&["t1-1-0"]));
    assert_eq!(
        log.shapes_after(1),
        strings(&[
            "step:1",
            "started:t1-1-0",
            "completed::[t1-1-0]",
            "finished:t1-1-0:ok",
            "step:2",
            "delta:done",
            "completed:done:[]",
            "final:done",
        ])
    );
}

#[tokio::test(flavor = "current_thread")]
async fn compaction_time_consumes_the_tool_calls_remaining_wall_budget() {
    struct SlowSummary;
    impl dex_loop::Summarize for SlowSummary {
        async fn summarize(
            &self,
            _ctx: &dex_loop::Context,
            _entries: &[dex_loop::Entry],
        ) -> dex_loop::Summary {
            tokio::time::sleep(Duration::from_millis(40)).await;
            dex_loop::Summary {
                text: Some("historical summary".into()),
                ..dex_loop::Summary::default()
            }
        }
    }
    let log = FakeLog::default();
    log.start_turn("old", "old history to summarize");
    log.host_append(dex_loop::Event::Final {
        text: "old done".into(),
    });
    let mut ctx = log.start_turn("current", "read");
    let model = FakeModel::new(vec![
        vec![call("search", json!({"key": "slow"}))],
        vec![text("never reached")],
    ]);
    let tools = FakeTools::new(vec![read_tool("search")]).delay("slow", Duration::from_millis(80));
    let engine = engine(
        &log,
        &model,
        &tools,
        budget_with_wall(Duration::from_millis(100)),
    )
    .with_tool_call_deadline(NEVER)
    .with_compactor(dex_loop::Threshold::new(1, 1, SlowSummary));
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Failed)
    );
    assert!(
        tools.runs().is_empty(),
        "a read must not finish using a wall budget restarted after summarization"
    );
    assert!(log.events().iter().all(|event| !matches!(
        event,
        dex_loop::Event::ToolFinished {
            outcome: Outcome::Succeeded,
            ..
        }
    )));
    assert_eq!(ctx, log.rehydrate());
}
