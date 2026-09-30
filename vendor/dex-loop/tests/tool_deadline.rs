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

use std::time::Duration;

use dex_loop::{Budget, CancellationToken, Exit, Outcome, Output};
use serde_json::json;
use support::*;

const NEVER: Duration = Duration::from_secs(60 * 60);

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
        .with_tool_call_deadline(Duration::from_millis(50));
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

// Real time, not paused: the loop's between-steps wall check reads a
// `std::time::Instant`, which tokio's paused clock does not advance. 100ms
// of real time is the whole cost; the stuck call is dropped, never awaited.
#[tokio::test(flavor = "current_thread")]
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
    assert_eq!(model.seen().len(), 1, "no model step after the wall budget");
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
