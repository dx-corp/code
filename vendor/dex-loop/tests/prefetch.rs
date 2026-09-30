//! Reads that start while the model is still streaming.
//!
//! A read-only call whose arguments fit its schema, whose policy allows it
//! and which runs on the host starts when its `ToolCall` chunk arrives:
//! `ToolStarted` lands before `ModelStepCompleted`. Everything else waits for
//! the commit point, and an attempt that never commits closes the reads it
//! started as not run, so no `ToolStarted` dangles and no result reaches
//! history.

// Each `tests/*.rs` file is its own crate; this one uses a subset of the
// shared helpers.
#[allow(dead_code)]
mod support;

use std::time::Duration;

use dex_loop::{Budget, CancellationToken, Event, Exit, ModelError, Verdict};
use serde_json::json;
use support::*;

const CHUNK: Duration = Duration::from_millis(30);

fn budget() -> Budget {
    Budget {
        max_steps: 10,
        max_tokens: 1_000_000,
        max_cost_micros: 1_000_000,
        wall: Duration::from_secs(30),
    }
}

/// How many `ToolStarted` and `ToolFinished` rows the log holds for `call`.
fn started_and_finished(log: &FakeLog, call: &str) -> (usize, usize) {
    let events = log.events();
    let started = events
        .iter()
        .filter(|event| matches!(event, Event::ToolStarted { call: id, .. } if id.as_str() == call))
        .count();
    let finished = events
        .iter()
        .filter(
            |event| matches!(event, Event::ToolFinished { call: id, .. } if id.as_str() == call),
        )
        .count();
    (started, finished)
}

#[tokio::test]
async fn reads_start_while_the_model_streams_and_the_step_adopts_their_results() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![
            call("search", json!({"key": "a"})),
            call("search", json!({"key": "b"})),
            text("tail"),
        ],
        vec![text("done")],
    ])
    .with_chunk_delay(CHUNK);
    let tools = FakeTools::new(vec![read_tool("search")])
        .delay("a", Duration::from_millis(10))
        .delay("b", Duration::from_millis(10));
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "look");

    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );

    assert_eq!(
        log.shapes_after(1),
        strings(&[
            "step:1",
            "started:t1-1-0",
            "started:t1-1-1",
            "delta:tail",
            "completed:tail:[t1-1-0,t1-1-1]",
            "finished:t1-1-0:ok",
            "finished:t1-1-1:ok",
            "step:2",
            "delta:done",
            "completed:done:[]",
            "final:done",
        ])
    );
    // Adopted, not run again.
    let mut runs = tools.run_ids();
    runs.sort();
    assert_eq!(runs, strings(&["t1-1-0", "t1-1-1"]));
    assert_eq!(
        view(&model.seen()[1]),
        strings(&[
            "user:look",
            "assistant:tail:[t1-1-0,t1-1-1]",
            "tool:t1-1-0:ok:out/t1-1-0",
            "tool:t1-1-1:ok:out/t1-1-1",
        ])
    );
    assert_eq!(log.rehydrate(), ctx);
}

#[tokio::test]
async fn a_read_that_is_still_running_at_the_commit_joins_the_wave() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![call("search", json!({"key": "a"}))],
        vec![text("done")],
    ])
    .with_chunk_delay(Duration::from_millis(5));
    let tools = FakeTools::new(vec![read_tool("search")]).delay("a", Duration::from_millis(150));
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "look");

    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );

    assert_eq!(
        log.shapes_after(1)[..4],
        strings(&[
            "step:1",
            "started:t1-1-0",
            "completed::[t1-1-0]",
            "finished:t1-1-0:ok",
        ])
    );
    assert_eq!(tools.run_ids(), strings(&["t1-1-0"]));
    assert_eq!(started_and_finished(&log, "t1-1-0"), (1, 1));
    assert_eq!(log.rehydrate(), ctx);
}

#[tokio::test]
async fn a_mutation_never_starts_before_the_commit_and_holds_back_the_reads_after_it() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![
            call("search", json!({"key": "a"})),
            call("update", json!({"key": "w"})),
            call("search", json!({"key": "b"})),
        ],
        vec![text("ok")],
    ])
    .with_chunk_delay(Duration::from_millis(5));
    let tools = FakeTools::new(vec![read_tool("search"), write_tool("update")]);
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "change it");

    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );

    assert_eq!(
        log.shapes_after(1)[..8],
        strings(&[
            "step:1",
            "started:t1-1-0",
            "completed::[t1-1-0,t1-1-1,t1-1-2]",
            "finished:t1-1-0:ok",
            "started:t1-1-1",
            "finished:t1-1-1:ok",
            "started:t1-1-2",
            "finished:t1-1-2:ok",
        ])
    );
    // The read after the mutation ran after it, as the model ordered them.
    let write = tools.run_of(&call_id("t1", 1, 1));
    let later_read = tools.run_of(&call_id("t1", 1, 2));
    assert!(later_read.started >= write.finished);
}

#[tokio::test]
async fn a_lone_mutation_starts_only_after_the_commit() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![call("update", json!({"key": "w"}))],
        vec![text("ok")],
    ])
    .with_chunk_delay(Duration::from_millis(5));
    let tools = FakeTools::new(vec![write_tool("update")]);
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "change it");

    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(
        log.shapes_after(1)[..4],
        strings(&[
            "step:1",
            "completed::[t1-1-0]",
            "started:t1-1-0",
            "finished:t1-1-0:ok",
        ])
    );
}

#[tokio::test]
async fn calls_that_wait_for_a_person_or_a_client_never_start_early() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![vec![
        call("ask", json!({"question": "which one?"})),
        call("search", json!({"key": "a"})),
    ]])
    .with_chunk_delay(Duration::from_millis(5));
    let tools = FakeTools::new(vec![ask_tool("ask"), read_tool("search")]);
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "go");

    let exit = engine.run(&mut ctx, &CancellationToken::new()).await;

    assert_eq!(exit, Ok(Exit::Asked(call_id("t1", 1, 0))));
    assert_eq!(
        log.shapes_after(1)[..3],
        strings(&[
            "step:1",
            "completed::[t1-1-0,t1-1-1]",
            "question:t1-1-0:which one?"
        ])
    );
    assert!(tools.run_ids().is_empty());

    let log = FakeLog::default();
    let model = FakeModel::new(vec![vec![call("browser.read", json!({}))]])
        .with_chunk_delay(Duration::from_millis(5));
    let tools = FakeTools::new(vec![client_executed_tool("browser.read", true)]);
    let engine = support::engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "go");

    let exit = engine.run(&mut ctx, &CancellationToken::new()).await;

    assert_eq!(exit, Ok(Exit::AwaitingClientTool(call_id("t1", 1, 0))));
    assert_eq!(
        log.shapes_after(1)[..2],
        strings(&["step:1", "completed::[t1-1-0]"])
    );
    assert!(tools.run_ids().is_empty());
}

#[tokio::test]
async fn a_read_policy_does_not_allow_never_starts_early() {
    for verdict in [Verdict::Deny("no grant".into()), approval("ap-1")] {
        let denied = matches!(verdict, Verdict::Deny(_));
        let log = FakeLog::default();
        let model = FakeModel::new(vec![
            vec![call("search", json!({"key": "a"}))],
            vec![text("ok")],
        ])
        .with_chunk_delay(Duration::from_millis(5));
        let tools = FakeTools::new(vec![read_tool("search")]).verdict("search", verdict);
        let engine = engine(&log, &model, &tools, budget());
        let mut ctx = log.start_turn("t1", "go");

        engine.run(&mut ctx, &CancellationToken::new()).await.ok();

        let shapes = log.shapes_after(1);
        assert_eq!(shapes[..2], strings(&["step:1", "completed::[t1-1-0]"]));
        if denied {
            assert_eq!(shapes[2], "finished:t1-1-0:err");
            assert!(tools.run_ids().is_empty());
        }
    }
}

#[tokio::test]
async fn arguments_that_fail_the_schema_never_start_and_the_model_sees_why() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![
            call("search", json!({"key": 7})),
            call("search", json!({"key": "ok"})),
        ],
        vec![text("done")],
    ])
    .with_chunk_delay(Duration::from_millis(5));
    let tools = FakeTools::new(vec![strict_read_tool("search")]);
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "go");

    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );

    // The invalid call is the first call, so nothing after it starts early
    // either (only an unbroken run of reads from the first call does).
    assert_eq!(
        log.shapes_after(1)[..5],
        strings(&[
            "step:1",
            "completed::[t1-1-0,t1-1-1]",
            "finished:t1-1-0:err",
            "started:t1-1-1",
            "finished:t1-1-1:ok",
        ])
    );
    assert_eq!(tools.run_ids(), strings(&["t1-1-1"]));
    let seen = view(&model.seen()[1]);
    assert!(
        seen[2].starts_with("tool:t1-1-0:err:invalid arguments: "),
        "{seen:?}"
    );
    assert_eq!(log.rehydrate(), ctx);
}

#[tokio::test]
async fn a_failed_stream_closes_the_reads_it_started_and_records_no_result() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![vec![
        call("search", json!({"key": "a"})),
        Err(ModelError {
            message: "connection reset".into(),
        }),
    ]])
    .with_chunk_delay(Duration::from_millis(5));
    let tools = FakeTools::new(vec![read_tool("search")]).delay("a", Duration::from_millis(1));
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "go");

    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Failed)
    );

    let shapes = log.shapes_after(1);
    assert_eq!(
        shapes[..4],
        strings(&[
            "step:1",
            "started:t1-1-0",
            "finished:t1-1-0:err",
            "abandoned:1",
        ])
    );
    assert!(shapes[4].starts_with("error:"), "{shapes:?}");
    assert_eq!(started_and_finished(&log, "t1-1-0"), (1, 1));
    // Nothing committed: the finished row is not a tool result in history.
    assert_eq!(history(&ctx), strings(&["user:go"]));
    assert_eq!(log.rehydrate(), ctx);
}

#[tokio::test]
async fn a_new_turn_after_a_failed_stream_starts_its_own_calls_once() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![
            call("search", json!({"key": "a"})),
            Err(ModelError {
                message: "connection reset".into(),
            }),
        ],
        vec![call("search", json!({"key": "a"}))],
        vec![text("done")],
    ])
    .with_chunk_delay(Duration::from_millis(5));
    let tools = FakeTools::new(vec![read_tool("search")]);
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "go");
    let cancel = CancellationToken::new();
    assert_eq!(engine.run(&mut ctx, &cancel).await, Ok(Exit::Failed));

    let mut ctx = log.start_turn("t2", "again");
    assert_eq!(engine.run(&mut ctx, &cancel).await, Ok(Exit::Done));

    for id in ["t1-1-0", "t2-1-0"] {
        assert_eq!(started_and_finished(&log, id), (1, 1), "{id}");
    }
    // The first turn's read ran (a read has no effect), but its result was
    // never committed; the second turn's call ran once.
    assert_eq!(tools.run_ids(), strings(&["t1-1-0", "t2-1-0"]));
    assert!(
        history(&ctx).iter().all(|row| !row.contains("t1-1-0")),
        "{:?}",
        history(&ctx)
    );
    assert_eq!(log.rehydrate(), ctx);
}

#[tokio::test]
async fn an_interrupt_during_the_stream_ends_cleanly_with_a_read_in_flight() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![vec![call("search", json!({"key": "a"}))]])
        .with_chunk_delay(Duration::from_millis(5))
        .hanging();
    let tools = FakeTools::new(vec![read_tool("search")]).delay("a", Duration::from_secs(10));
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "go");
    let cancel = CancellationToken::new();

    let host = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        log.host_append(Event::Interrupt { principal: alice() });
        cancel.cancel();
    };
    let started = std::time::Instant::now();
    let (exit, ()) = tokio::join!(engine.run(&mut ctx, &cancel), host);

    assert_eq!(exit, Ok(Exit::Interrupted));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(
        log.shapes_after(1),
        strings(&[
            "step:1",
            "started:t1-1-0",
            "interrupt",
            "finished:t1-1-0:err",
            "completed::[]",
            "interrupted",
        ])
    );
    assert_eq!(started_and_finished(&log, "t1-1-0"), (1, 1));
    assert_eq!(history(&ctx), strings(&["user:go", "assistant::[]"]));
}

#[tokio::test]
async fn a_stream_that_outlives_the_wall_budget_closes_its_reads() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![vec![call("search", json!({"key": "a"}))]])
        .with_chunk_delay(Duration::from_millis(5))
        .hanging();
    let tools = FakeTools::new(vec![read_tool("search")]).delay("a", Duration::from_secs(10));
    let engine = engine(
        &log,
        &model,
        &tools,
        Budget {
            wall: Duration::from_millis(150),
            ..budget()
        },
    );
    let mut ctx = log.start_turn("t1", "go");

    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Failed)
    );

    let shapes = log.shapes_after(1);
    assert_eq!(
        shapes[..4],
        strings(&[
            "step:1",
            "started:t1-1-0",
            "finished:t1-1-0:err",
            "abandoned:1",
        ])
    );
    assert_eq!(started_and_finished(&log, "t1-1-0"), (1, 1));
}

#[tokio::test]
async fn a_restart_after_a_read_started_mid_stream_closes_it_and_runs_the_step_once() {
    let log = FakeLog::default();
    // The first engine died mid-stream: the read's `ToolStarted` is on the
    // log, the step never committed.
    log.start_turn("t1", "look");
    log.host_append(Event::StepStarted {
        step: 1,
        control_through: dex_loop::Cursor::START,
    });
    log.host_append(Event::ToolStarted {
        call: call_id("t1", 1, 0),
        tool: dex_loop::ToolName::new("search"),
        label: "Label for search".into(),
        principal: alice(),
    });
    let model = FakeModel::new(vec![
        vec![call("search", json!({"key": "a"}))],
        vec![text("done")],
    ])
    .with_chunk_delay(Duration::from_millis(5));
    let tools = FakeTools::new(vec![read_tool("search")]);
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.rehydrate();

    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );

    assert_eq!(
        log.shapes_after(3)[..6],
        strings(&[
            "finished:t1-1-0:err",
            "abandoned:1",
            "step:2",
            "started:t1-2-0",
            "completed::[t1-2-0]",
            "finished:t1-2-0:ok",
        ])
    );
    assert_eq!(tools.run_ids(), strings(&["t1-2-0"]));
    assert_eq!(log.rehydrate(), ctx);
}
