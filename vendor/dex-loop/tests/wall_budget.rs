//! Regression: `budget.wall` must bound the whole `Engine::run` call,
//! including time spent waiting on the model stream, not only the time
//! between steps.
//!
//! Before the fix this accompanies, a model stream that yields chunks and
//! then never resolves again (no error, no `None`, and never cancelled) hung
//! `Engine::run` forever: the budget check in the outer loop only runs
//! *between* `model_step` calls, and the inner chunk-reading loop raced the
//! stream only against `cancel`, never against the wall deadline. This test
//! fails (times out) on the pre-fix engine and passes on the fixed one,
//! using paused virtual time so it costs no real wall-clock time either way.

// This test only needs a handful of `support` helpers; the rest are dead
// code from this test binary's own point of view (each `tests/*.rs` file is
// its own crate), even though `scenarios.rs` uses the whole module.
#[allow(dead_code)]
mod support;

use std::time::Duration;

use dex_loop::{
    Budget, CancellationToken, Engine, Event, Exit, Lexicon, ModelChunk, ModelError, PrincipalId,
};
use futures_util::{Stream, StreamExt, stream};
use support::*;

/// A model that yields text and usage, then a stream that never produces another
/// item and never ends -- the adversarial "stalled stream" case: no error, no
/// natural end, and (in this test) no interrupt ever arrives either.
#[derive(Clone, Default)]
struct StallingModel;

impl dex_loop::Model for StallingModel {
    fn stream<'a>(
        &'a self,
        _ctx: &'a dex_loop::Context,
        _tools: &'a [&'a dex_loop::ToolSpec],
    ) -> impl Stream<Item = Result<ModelChunk, ModelError>> + Send + 'a {
        stream::iter([
            Ok(ModelChunk::Text("partial answer".into())),
            usage(2, 3, 5),
        ])
        .chain(stream::pending())
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_stalled_model_stream_is_bounded_by_the_wall_budget() {
    let log = FakeLog::default();
    let tools = FakeTools::new(vec![]);
    let engine = Engine::new(
        log.clone(),
        StallingModel,
        tools,
        FakeEffects::default(),
        Lexicon::default(),
        budget_with_wall(Duration::from_millis(50)),
    );
    let mut ctx = log.start_turn("t1", "hi");
    let cancel = CancellationToken::new();

    // Paused virtual time auto-advances once the run future is the only
    // thing left to make progress on, so this resolves instantly in real
    // time regardless of the 50ms budget; before the fix it never resolves
    // at all (the inner loop has no deadline), and this `timeout` -- itself
    // generous relative to the 50ms budget -- is what turns that hang into a
    // failing test rather than one that never finishes.
    let result = tokio::time::timeout(Duration::from_secs(5), engine.run(&mut ctx, &cancel))
        .await
        .expect("Engine::run must return once budget.wall elapses, even mid-stream");

    assert_eq!(result, Ok(Exit::Failed));
    assert_eq!(
        log.shapes_after(1),
        strings(&[
            "step:1",
            "delta:partial answer",
            "usage:5",
            "abandoned:1",
            "error:budget_exhausted:wall budget exhausted: 50ms"
        ])
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn an_interrupt_still_wins_a_race_with_the_wall_deadline() {
    // A wall deadline far in the future must not change ordinary interrupt
    // behavior: cancelling still ends the turn as `Interrupted`, not as a
    // spurious budget failure.
    let log = FakeLog::default();
    let tools = FakeTools::new(vec![]);
    let engine = Engine::new(
        log.clone(),
        StallingModel,
        tools,
        FakeEffects::default(),
        Lexicon::default(),
        budget_with_wall(Duration::from_secs(3600)),
    );
    let mut ctx = log.start_turn("t1", "hi");
    let cancel = CancellationToken::new();

    let run = engine.run(&mut ctx, &cancel);
    tokio::pin!(run);
    // Give the stream a chance to yield its chunks before interrupting.
    tokio::task::yield_now().await;
    log.host_append(Event::Interrupt {
        principal: PrincipalId::new("alice"),
    });
    cancel.cancel();

    let result = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("an interrupted stream must not hang");
    assert_eq!(result, Ok(Exit::Interrupted));
}

fn budget_with_wall(wall: Duration) -> Budget {
    Budget {
        max_steps: 10,
        max_tokens: u64::MAX,
        max_cost_micros: u64::MAX,
        wall,
    }
}
