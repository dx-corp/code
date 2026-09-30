//! End-to-end scenarios for the loop. Each asserts the exact event sequence
//! the engine appends, and most check that `rehydrate` rebuilds the same
//! context the warm engine holds.

mod support;

use std::time::{Duration, Instant};

use dex_loop::{
    ApprovalId, ApprovalMode, Budget, CUT_OFF_NOTICE, CancellationToken, Cursor, Engine, Event,
    Exit, Fenced, Lexicon, ModelError, OutputRef, PrincipalId, ProposedCall, Threshold, ToolName,
    ToolResult, TurnId, Verdict,
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

fn assert_send<T: Send>(_: &T) {}

// 1. A plain answer: text, then ModelStepCompleted and Final.
#[tokio::test]
async fn plain_answer_streams_text_then_final() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![vec![text("Hel"), text("lo"), usage(3, 2, 7)]]);
    let tools = FakeTools::new(vec![]);
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "hi");
    let cancel = CancellationToken::new();

    let run = engine.run(&mut ctx, &cancel);
    assert_send(&run);
    assert_eq!(run.await, Ok(Exit::Done));

    assert_eq!(
        log.shapes(),
        strings(&[
            "user:hi",
            "step:1",
            "delta:Hello",
            "usage:5",
            "completed:Hello:[]",
            "final:Hello",
        ])
    );
    // Two model chunks, one coalesced row: the engine does not assume a row
    // per delta.
    assert_eq!(log.text_writes(), strings(&["Hel", "lo"]));
    assert_eq!(ctx.usage().cost_micros, 7);
    assert_eq!(
        log.rehydrate(),
        ctx,
        "rehydrate must rebuild the warm context"
    );
    // Running a finished turn again appends nothing.
    assert_eq!(engine.run(&mut ctx, &cancel).await, Ok(Exit::Done));
    assert_eq!(log.len(), 6);
}

// 2. Three reads run as one wave: they overlap, take about one tool delay, and
// their results enter history in call order although they finish in reverse.
#[tokio::test]
async fn read_only_wave_overlaps_and_keeps_call_order() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![
            call("search", json!({"key": "a"})),
            call("search", json!({"key": "b"})),
            call("search", json!({"key": "c"})),
        ],
        vec![text("done")],
    ]);
    let tools = FakeTools::new(vec![read_tool("search")])
        .barrier(&["a", "b", "c"])
        .delay("a", Duration::from_millis(300))
        .delay("b", Duration::from_millis(200))
        .delay("c", Duration::from_millis(100));
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "look");

    let started = Instant::now();
    let exit = engine.run(&mut ctx, &CancellationToken::new()).await;
    let elapsed = started.elapsed();

    assert_eq!(exit, Ok(Exit::Done));
    assert!(
        elapsed < Duration::from_millis(500),
        "wave took {elapsed:?}; serial dispatch takes 600ms"
    );
    assert!(tools.runs().iter().all(|run| !run.cancelled));
    assert_eq!(
        log.shapes(),
        strings(&[
            "user:look",
            "step:1",
            "completed::[t1-1-0,t1-1-1,t1-1-2]",
            "started:t1-1-0",
            "started:t1-1-1",
            "started:t1-1-2",
            "finished:t1-1-2:ok",
            "finished:t1-1-1:ok",
            "finished:t1-1-0:ok",
            "step:2",
            "delta:done",
            "completed:done:[]",
            "final:done",
        ])
    );
    assert_eq!(
        view(&model.seen()[1]),
        strings(&[
            "user:look",
            "assistant::[t1-1-0,t1-1-1,t1-1-2]",
            "tool:t1-1-0:ok:out/t1-1-0",
            "tool:t1-1-1:ok:out/t1-1-1",
            "tool:t1-1-2:ok:out/t1-1-2",
        ])
    );
    assert_eq!(log.rehydrate(), ctx);
}

// 3. A mutation waits for the wave before it; a read after it starts a new
// wave. The mutation is claimed in the ledger and its outcome recorded.
#[tokio::test]
async fn mutating_call_runs_serially_after_the_wave() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![
            call("search", json!({"key": "a"})),
            call("search", json!({"key": "b"})),
            call("update", json!({"key": "w"})),
            call("search", json!({"key": "c"})),
        ],
        vec![text("ok")],
    ]);
    let tools = FakeTools::new(vec![read_tool("search"), write_tool("update")])
        .barrier(&["a", "b"])
        .delay("a", Duration::from_millis(50))
        .delay("b", Duration::from_millis(80));
    let effects = FakeEffects::default();
    let engine = engine_with(&log, &model, &tools, &effects, budget());
    let mut ctx = log.start_turn("t1", "change it");

    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(
        log.shapes(),
        strings(&[
            "user:change it",
            "step:1",
            "completed::[t1-1-0,t1-1-1,t1-1-2,t1-1-3]",
            "started:t1-1-0",
            "started:t1-1-1",
            "finished:t1-1-0:ok",
            "finished:t1-1-1:ok",
            "started:t1-1-2",
            "finished:t1-1-2:ok",
            "started:t1-1-3",
            "finished:t1-1-3:ok",
            "step:2",
            "delta:ok",
            "completed:ok:[]",
            "final:ok",
        ])
    );
    let write = tools.run_of(&call_id("t1", 1, 2));
    for read in [0, 1] {
        let read = tools.run_of(&call_id("t1", 1, read));
        assert!(
            write.started >= read.finished,
            "the write overlapped a read"
        );
    }
    assert!(tools.run_of(&call_id("t1", 1, 3)).started >= write.finished);
    assert_eq!(
        effects.recorded(&call_id("t1", 1, 2)),
        Some(Some(output_for(&call_id("t1", 1, 2))))
    );
    assert_eq!(
        effects.recorded(&call_id("t1", 1, 0)),
        None,
        "reads skip the ledger"
    );
}

// 4a. Policy asks for approval on B: nobody is asked. The receipt lands
// before B runs, bound to B's digest, and the rest of the step runs in
// order with the original arguments.
#[tokio::test]
async fn an_approval_class_call_is_granted_at_once_recorded_and_runs_in_order() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![
            call("search", json!({"key": "a"})),
            call("send_email", json!({"key": "w", "to": "ops@example.com"})),
            call("search", json!({"key": "b"})),
        ],
        vec![text("sent")],
    ]);
    let tools = FakeTools::new(vec![read_tool("search"), write_tool("send_email")])
        .verdict("send_email", approval("ap-1"));
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "email them");
    let cancel = CancellationToken::new();

    assert_eq!(engine.run(&mut ctx, &cancel).await, Ok(Exit::Done));
    assert_eq!(
        log.shapes_after(1),
        strings(&[
            "step:1",
            "completed::[t1-1-0,t1-1-1,t1-1-2]",
            "started:t1-1-0",
            "finished:t1-1-0:ok",
            "auto_approved:t1-1-1",
            "started:t1-1-1",
            "finished:t1-1-1:ok",
            "started:t1-1-2",
            "finished:t1-1-2:ok",
            "step:2",
            "delta:sent",
            "completed:sent:[]",
            "final:sent",
        ])
    );
    let receipt = log
        .events()
        .into_iter()
        .find(|event| matches!(event, Event::AutoApproved { .. }))
        .expect("the receipt");
    assert_eq!(
        receipt,
        Event::AutoApproved {
            call: call_id("t1", 1, 1),
            approval: ApprovalId::new("ap-1"),
            args_digest: dex_loop::args_digest(&json!({"key": "w", "to": "ops@example.com"})),
            summary: "Approve ap-1".into(),
            principal: PrincipalId::new(dex_loop::AUTO_APPROVER),
        }
    );
    assert!(
        !log.events()
            .iter()
            .any(|event| matches!(event, Event::ApprovalRequested { .. })),
        "no approval request is ever written"
    );
    assert_eq!(tools.run_ids(), strings(&["t1-1-0", "t1-1-1", "t1-1-2"]));
    assert_eq!(
        tools.run_of(&call_id("t1", 1, 1)).args,
        json!({"key": "w", "to": "ops@example.com"})
    );
    // Policy ran once per call: nothing resumed, so nothing re-checked.
    let checks: Vec<String> = tools
        .policy_checks()
        .into_iter()
        .map(|(call, _)| call)
        .collect();
    assert_eq!(checks, strings(&["t1-1-0", "t1-1-1", "t1-1-2"]));
    assert_eq!(
        view(&model.seen()[1])[2..],
        strings(&[
            "tool:t1-1-0:ok:out/t1-1-0",
            "tool:t1-1-1:ok:out/t1-1-1",
            "tool:t1-1-2:ok:out/t1-1-2",
        ])
    );
    assert_eq!(log.rehydrate(), ctx);
}

// 4b. A crash between the receipt and `ToolStarted`: the rehydrated engine
// adopts the receipt (no second one, no second policy grant) and runs the
// call once with its original arguments.
#[tokio::test]
async fn a_receipt_survives_a_crash_and_the_call_runs_once_after_rehydrate() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![call("send_email", json!({"key": "w"}))],
        vec![text("sent")],
    ]);
    let tools =
        FakeTools::new(vec![write_tool("send_email")]).verdict("send_email", approval("ap-1"));
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "email them");
    let cancel = CancellationToken::new();
    // user, step, completed, auto_approved land; the `ToolStarted` write is
    // refused, which is the crash.
    log.fence_after(3);
    assert!(matches!(
        engine.run(&mut ctx, &cancel).await,
        Err(Fenced { .. })
    ));
    assert_eq!(
        log.shapes_after(1),
        strings(&["step:1", "completed::[t1-1-0]", "auto_approved:t1-1-0"])
    );
    assert!(tools.runs().is_empty(), "nothing ran before the crash");

    log.fence_after(usize::MAX);
    let engine = support::engine(&log, &model, &tools, budget());
    let mut ctx = log.rehydrate();
    assert_eq!(engine.run(&mut ctx, &cancel).await, Ok(Exit::Done));
    assert_eq!(
        log.shapes_after(4),
        strings(&[
            "started:t1-1-0",
            "finished:t1-1-0:ok",
            "step:2",
            "delta:sent",
            "completed:sent:[]",
            "final:sent",
        ])
    );
    assert_eq!(tools.run_ids(), strings(&["t1-1-0"]));
    assert_eq!(
        log.events()
            .iter()
            .filter(|event| matches!(event, Event::AutoApproved { .. }))
            .count(),
        1,
        "one receipt per call, across the crash"
    );
    assert_eq!(log.rehydrate(), ctx);
}

// 4c. A call an older deploy parked for a human (an `ApprovalRequested` with
// no decision) is granted on its next run, under the same approval id, and
// the step continues; the thread is never stranded.
#[tokio::test]
async fn a_legacy_parked_call_is_granted_on_rehydrate_and_the_step_continues() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![vec![], vec![text("sent")]]);
    let tools = FakeTools::new(vec![read_tool("search"), write_tool("send_email")])
        .verdict("send_email", approval("ap-1"));
    let send = ProposedCall::new(
        call_id("t1", 1, 0),
        ToolName::new("send_email"),
        json!({"key": "w"}),
        alice(),
    );
    let search = ProposedCall::new(
        call_id("t1", 1, 1),
        ToolName::new("search"),
        json!({"key": "b"}),
        alice(),
    );
    for event in [
        Event::UserMessage {
            turn: TurnId::new("t1"),
            message_id: None,
            principal: alice(),
            text: "email them".into(),
            attachments: vec![],
            client_tools: vec![],
            authorized_tools: Vec::new(),
            approval_mode: dex_loop::ApprovalMode::Interactive,
        },
        Event::StepStarted {
            step: 1,
            control_through: Cursor::START,
        },
        Event::ModelStepCompleted {
            step: 1,
            text: String::new(),
            calls: vec![send.clone(), search],
            reasoning: None,
        },
        Event::ApprovalRequested {
            call: send.id.clone(),
            approval: ApprovalId::new("ap-1"),
            args_digest: send.args_digest.clone(),
            summary: "Approve ap-1".into(),
        },
    ] {
        log.host_append(event);
    }
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.rehydrate();
    let cancel = CancellationToken::new();
    assert_eq!(engine.run(&mut ctx, &cancel).await, Ok(Exit::Done));
    assert_eq!(
        log.shapes_after(4),
        strings(&[
            "auto_approved:t1-1-0",
            "started:t1-1-0",
            "finished:t1-1-0:ok",
            "started:t1-1-1",
            "finished:t1-1-1:ok",
            "step:2",
            "delta:sent",
            "completed:sent:[]",
            "final:sent",
        ])
    );
    let receipt = log
        .events()
        .into_iter()
        .find(|event| matches!(event, Event::AutoApproved { .. }))
        .expect("the receipt");
    assert!(
        matches!(&receipt, Event::AutoApproved { approval, args_digest, .. }
            if approval.as_str() == "ap-1" && args_digest == &send.args_digest),
        "{receipt:?}"
    );
    assert_eq!(tools.run_ids(), strings(&["t1-1-0", "t1-1-1"]));
    assert_eq!(tools.run_of(&send.id).args, json!({"key": "w"}));
    assert_eq!(log.rehydrate(), ctx);
}

// 4d. Policy's own denial still denies: a grant is not an override, and a
// stale human decision in the log (from a client that still sends one)
// changes nothing about a call that already has its receipt.
#[tokio::test]
async fn a_policy_denial_still_denies_and_a_stale_decision_is_inert() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![call("send_email", json!({"key": "w"}))],
        vec![text("could not send")],
    ]);
    let tools = FakeTools::new(vec![write_tool("send_email")]).verdict(
        "send_email",
        Verdict::Deny("the mail grant was revoked".into()),
    );
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "email them");
    let cancel = CancellationToken::new();
    assert_eq!(engine.run(&mut ctx, &cancel).await, Ok(Exit::Done));
    assert!(tools.runs().is_empty(), "a denied call still ran");
    assert!(
        !log.events()
            .iter()
            .any(|event| matches!(event, Event::AutoApproved { .. })),
        "a denial writes no receipt"
    );
    assert_eq!(
        view(&model.seen()[1])[2..],
        strings(&["tool:t1-1-0:err:denied: the mail grant was revoked"])
    );

    let before = log.len();
    log.host_append(Event::ApprovalDecided {
        call: call_id("t1", 1, 0),
        approval: ApprovalId::new("ap-1"),
        args_digest: dex_loop::args_digest(&json!({"key": "w"})),
        approved: true,
        principal: alice(),
    });
    let mut ctx = log.rehydrate();
    assert_eq!(engine.run(&mut ctx, &cancel).await, Ok(Exit::Done));
    assert_eq!(log.len(), before + 1, "a stale decision appends nothing");
    assert!(tools.runs().is_empty());
}

// 5. Bob steers in Alice's turn: the steer becomes Bob's user message before
// the next model call, and the calls that step proposes are checked under Bob.
#[tokio::test]
async fn steer_from_another_principal_is_checked_under_that_principal() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![call("search", json!({"key": "a"}))],
        vec![call("update", json!({"key": "w"}))],
        vec![text("Bob cannot update")],
    ]);
    let steer_log = log.clone();
    let tools = FakeTools::new(vec![read_tool("search"), write_tool("update")])
        .verdict_for("update", bob(), Verdict::Deny("bob lacks access".into()))
        .on_run(move |call| {
            if call.tool.as_str() == "search" {
                steer_log.host_append(Event::Steer {
                    principal: bob(),
                    text: "also update staging".into(),
                });
            }
        });
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "check prod");

    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(
        log.shapes(),
        strings(&[
            "user:check prod",
            "step:1",
            "completed::[t1-1-0]",
            "started:t1-1-0",
            "steer:also update staging",
            "finished:t1-1-0:ok",
            "step:2",
            "completed::[t1-2-0]",
            "finished:t1-2-0:err",
            "step:3",
            "delta:Bob cannot update",
            "completed:Bob cannot update:[]",
            "final:Bob cannot update",
        ])
    );
    assert_eq!(
        view(&model.seen()[1]),
        strings(&[
            "user:check prod",
            "assistant::[t1-1-0]",
            "tool:t1-1-0:ok:out/t1-1-0",
            "user:also update staging",
        ])
    );
    assert_eq!(
        tools.policy_checks(),
        vec![
            ("t1-1-0".to_owned(), "alice".to_owned()),
            ("t1-2-0".to_owned(), "bob".to_owned()),
        ]
    );
    assert!(tools.run_ids().iter().all(|id| id != "t1-2-0"));
    assert!(log.events().iter().any(|event| matches!(
        event,
        Event::ToolStarted { principal, .. } if principal == &alice()
    )));
    assert_eq!(log.rehydrate(), ctx);
}

// 5b. A steer that lands while the model is answering continues the turn
// instead of being dropped.
#[tokio::test]
async fn steer_during_a_final_answer_continues_the_turn() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![vec![text("fi"), text("rst")], vec![text("second")]])
        .with_chunk_delay(Duration::from_millis(100));
    let tools = FakeTools::new(vec![]);
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "q");
    let cancel = CancellationToken::new();

    let host = async {
        tokio::time::sleep(Duration::from_millis(30)).await;
        log.host_append(Event::Steer {
            principal: alice(),
            text: "shorter please".into(),
        });
    };
    let (exit, ()) = tokio::join!(engine.run(&mut ctx, &cancel), host);

    assert_eq!(exit, Ok(Exit::Done));
    assert_eq!(
        log.shapes(),
        strings(&[
            "user:q",
            "step:1",
            "steer:shorter please",
            "delta:first",
            "completed:first:[]",
            "step:2",
            "delta:second",
            "completed:second:[]",
            "final:second",
        ])
    );
    assert_eq!(
        view(&model.seen()[1]),
        strings(&["user:q", "assistant:first:[]", "user:shorter please"])
    );
    assert_eq!(log.rehydrate(), ctx);
}

// 6. Interrupt during a wave cancels the running reads, closes the calls that
// never started, and ends the turn.
#[tokio::test]
async fn interrupt_mid_wave_cancels_reads_and_emits_interrupted() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![vec![
        call("search", json!({"key": "a"})),
        call("search", json!({"key": "b"})),
        call("update", json!({"key": "w"})),
    ]]);
    let tools = FakeTools::new(vec![read_tool("search"), write_tool("update")])
        .delay("a", Duration::from_secs(10))
        .delay("b", Duration::from_secs(10));
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "go");
    let cancel = CancellationToken::new();

    let host = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        log.host_append(Event::Interrupt { principal: alice() });
        cancel.cancel();
    };
    let started = Instant::now();
    let (exit, ()) = tokio::join!(engine.run(&mut ctx, &cancel), host);

    assert_eq!(exit, Ok(Exit::Interrupted));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(tools.runs().iter().all(|run| run.cancelled));
    assert_eq!(tools.run_ids().len(), 2, "the write never started");
    let shapes = log.shapes();
    assert_eq!(
        shapes[..6],
        strings(&[
            "user:go",
            "step:1",
            "completed::[t1-1-0,t1-1-1,t1-1-2]",
            "started:t1-1-0",
            "started:t1-1-1",
            "interrupt",
        ])
    );
    // The two reads finish in either order once cancelled.
    let mut reads = shapes[6..8].to_vec();
    reads.sort();
    assert_eq!(
        reads,
        strings(&["finished:t1-1-0:err", "finished:t1-1-1:err"])
    );
    assert_eq!(
        shapes[8..],
        strings(&["finished:t1-1-2:err", "interrupted"])
    );
    assert_eq!(history(&log.rehydrate()), history(&ctx));
    // An interrupted turn stays interrupted.
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Interrupted)
    );
}

// 6b. Interrupt during a mutation lets that mutation complete, then stops
// before the next effect.
#[tokio::test]
async fn interrupt_during_a_mutation_completes_it_then_stops() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![vec![
        call("update", json!({"key": "w"})),
        call("update", json!({"key": "x"})),
    ]]);
    let tools = FakeTools::new(vec![write_tool("update")]).delay("w", Duration::from_millis(300));
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "go");
    let cancel = CancellationToken::new();

    let host = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        log.host_append(Event::Interrupt { principal: alice() });
        cancel.cancel();
    };
    let (exit, ()) = tokio::join!(engine.run(&mut ctx, &cancel), host);

    assert_eq!(exit, Ok(Exit::Interrupted));
    assert_eq!(tools.run_ids(), strings(&["t1-1-0"]));
    assert!(
        !tools.runs()[0].cancelled,
        "a started mutation was cancelled"
    );
    assert_eq!(
        log.shapes(),
        strings(&[
            "user:go",
            "step:1",
            "completed::[t1-1-0,t1-1-1]",
            "started:t1-1-0",
            "interrupt",
            "finished:t1-1-0:ok",
            "finished:t1-1-1:err",
            "interrupted",
        ])
    );
}

// 7. Each budget axis stops the turn with budget_exhausted.
async fn run_to_budget(budget: Budget, model: FakeModel) -> (FakeLog, FakeModel, Exit) {
    let log = FakeLog::default();
    let tools = FakeTools::new(vec![read_tool("search")]);
    let engine = engine(&log, &model, &tools, budget);
    let mut ctx = log.start_turn("t1", "go");
    let exit = engine
        .run(&mut ctx, &CancellationToken::new())
        .await
        .expect("not fenced");
    assert_eq!(log.rehydrate(), ctx);
    (log, model, exit)
}

#[tokio::test]
async fn step_cap_gets_an_answer_only_step_that_ends_in_final() {
    let (log, model, exit) = run_to_budget(
        Budget {
            max_steps: 1,
            ..budget()
        },
        FakeModel::new(vec![
            vec![call("search", json!({}))],
            vec![text("here is what I found")],
        ]),
    )
    .await;
    assert_eq!(exit, Exit::Done);
    assert_eq!(
        log.shapes_after(1),
        strings(&[
            "step:1",
            "completed::[t1-1-0]",
            "started:t1-1-0",
            "finished:t1-1-0:ok",
            "step:2",
            "delta:here is what I found",
            "completed:here is what I found:[]",
            "final:here is what I found",
        ])
    );
    assert_eq!(model.calls(), 2);
    let offered = model.offered();
    assert!(!offered[0].is_empty(), "step 1 offers tools");
    assert!(offered[1].is_empty(), "the answer-only step offers none");
}

#[tokio::test]
async fn tool_call_on_the_answer_only_step_ends_budget_exhausted() {
    let (log, model, exit) = run_to_budget(
        Budget {
            max_steps: 1,
            ..budget()
        },
        FakeModel::new(vec![
            vec![call("search", json!({}))],
            vec![call("search", json!({}))],
        ]),
    )
    .await;
    assert_eq!(exit, Exit::Failed);
    assert_eq!(
        log.shapes().last().cloned(),
        Some(
            "error:budget_exhausted:step budget exhausted: 1 steps and the answer-only step after them"
                .to_owned()
        )
    );
    assert_eq!(model.calls(), 2);
    assert_eq!(
        log.shapes()
            .iter()
            .filter(|s| s.starts_with("started:"))
            .count(),
        1,
        "the refused call never ran"
    );
}

#[tokio::test]
async fn token_budget_exhaustion() {
    let (log, model, exit) = run_to_budget(
        Budget {
            max_tokens: 100,
            ..budget()
        },
        FakeModel::new(vec![vec![usage(90, 20, 0), call("search", json!({}))]]),
    )
    .await;
    assert_eq!(exit, Exit::Failed);
    assert_eq!(
        log.shapes().last().cloned(),
        Some("error:budget_exhausted:token budget exhausted: 110 of 100 tokens".to_owned())
    );
    assert_eq!(model.calls(), 1);
}

#[tokio::test]
async fn cost_budget_exhaustion() {
    let (log, model, exit) = run_to_budget(
        Budget {
            max_cost_micros: 500,
            ..budget()
        },
        FakeModel::new(vec![vec![usage(1, 1, 500), call("search", json!({}))]]),
    )
    .await;
    assert_eq!(exit, Exit::Failed);
    assert_eq!(
        log.shapes().last().cloned(),
        Some("error:budget_exhausted:cost budget exhausted: 500 of 500 micros".to_owned())
    );
    assert_eq!(model.calls(), 1);
}

#[tokio::test]
async fn wall_budget_exhaustion() {
    let (log, model, exit) = run_to_budget(
        Budget {
            wall: Duration::from_millis(100),
            ..budget()
        },
        FakeModel::new(vec![vec![text("thinking"), call("search", json!({}))]])
            .with_chunk_delay(Duration::from_millis(60)),
    )
    .await;
    assert_eq!(exit, Exit::Failed);
    assert_eq!(
        log.shapes().last().cloned(),
        Some("error:budget_exhausted:wall budget exhausted: 100ms".to_owned())
    );
    assert_eq!(model.calls(), 1);
}

// 8a. A mutation crashed after its effect and the ledger cannot reconcile it:
// the outcome is Unknown, the model sees it, and the call is not dispatched
// again.
#[tokio::test]
async fn mutation_crashed_after_effect_is_unknown_and_not_redispatched() {
    let log = FakeLog::default();
    let write = ProposedCall::new(
        call_id("t1", 1, 0),
        ToolName::new("update"),
        json!({"key": "w"}),
        alice(),
    );
    crashed_after_start(&log, &write);
    let effects = FakeEffects::default().seed(
        write.id.clone(),
        Some(ToolResult::unknown(
            "outcome unknown: the executor lost the call",
        )),
    );
    let model = FakeModel::new(vec![vec![text("not sure it applied")]]);
    let tools = FakeTools::new(vec![write_tool("update")]);
    let engine = engine_with(&log, &model, &tools, &effects, budget());
    let mut ctx = log.rehydrate();

    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert!(
        tools.runs().is_empty(),
        "an unknown mutation was dispatched again"
    );
    assert!(tools.policy_checks().is_empty());
    assert_eq!(
        log.shapes_after(4),
        strings(&[
            "finished:t1-1-0:unknown",
            "step:2",
            "delta:not sure it applied",
            "completed:not sure it applied:[]",
            "final:not sure it applied",
        ])
    );
    assert_eq!(
        view(&model.seen()[0])[2..],
        strings(&["tool:t1-1-0:unknown:outcome unknown: the executor lost the call"])
    );
}

// 8b. A recorded outcome is adopted; a claim with no recorded outcome
// settles to unknown -- never `Running`, which nothing would ever update --
// and the ledger is updated to match, so a later resume gets the same
// answer. Neither dispatches again.
#[tokio::test]
async fn crashed_mutation_adopts_the_ledger_outcome() {
    for (recorded, expected_shape, expected_ledger_outcome) in [
        (
            Some(ToolResult::stored(OutputRef::new("ledger/w"), None)),
            "finished:t1-1-0:ok",
            dex_loop::Outcome::Succeeded,
        ),
        (None, "finished:t1-1-0:unknown", dex_loop::Outcome::Unknown),
    ] {
        let log = FakeLog::default();
        let write = ProposedCall::new(
            call_id("t1", 1, 0),
            ToolName::new("update"),
            json!({}),
            alice(),
        );
        crashed_after_start(&log, &write);
        let effects = FakeEffects::default().seed(write.id.clone(), recorded);
        let model = FakeModel::new(vec![vec![text("done")]]);
        let tools = FakeTools::new(vec![write_tool("update")]);
        let engine = engine_with(&log, &model, &tools, &effects, budget());
        let mut ctx = log.rehydrate();
        assert_eq!(
            engine.run(&mut ctx, &CancellationToken::new()).await,
            Ok(Exit::Done)
        );
        assert!(tools.runs().is_empty());
        assert_eq!(log.shapes()[4], expected_shape);
        assert_eq!(
            effects
                .recorded(&write.id)
                .flatten()
                .expect("the ledger must hold a settled outcome")
                .outcome,
            expected_ledger_outcome
        );
    }
}

// 8c. A read that started before a crash simply runs again.
#[tokio::test]
async fn crashed_read_runs_again() {
    let log = FakeLog::default();
    let read = ProposedCall::new(
        call_id("t1", 1, 0),
        ToolName::new("search"),
        json!({"key": "a"}),
        alice(),
    );
    crashed_after_start(&log, &read);
    let model = FakeModel::new(vec![vec![text("found")]]);
    let tools = FakeTools::new(vec![read_tool("search")]);
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.rehydrate();
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(tools.run_ids(), strings(&["t1-1-0"]));
    assert_eq!(
        log.shapes()[4..6],
        strings(&["started:t1-1-0", "finished:t1-1-0:ok"])
    );
}

// 8d. Crash mid-stream: the attempt is marked abandoned, its text leaves the
// model's context, and the model call is issued again.
#[tokio::test]
async fn crash_mid_stream_abandons_the_attempt_and_reissues_it() {
    let log = FakeLog::default();
    log.host_append(Event::UserMessage {
        turn: dex_loop::TurnId::new("t1"),
        message_id: None,
        principal: alice(),
        text: "hi".into(),
        attachments: vec![],
        client_tools: vec![],
        authorized_tools: Vec::new(),
        approval_mode: dex_loop::ApprovalMode::Interactive,
    });
    log.host_append(Event::StepStarted {
        step: 1,
        control_through: dex_loop::Cursor::START,
    });
    log.host_append(Event::TextDelta {
        text: "partial ans".into(),
    });
    let model = FakeModel::new(vec![vec![text("fresh")]]);
    let tools = FakeTools::new(vec![]);
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.rehydrate();

    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(view(&model.seen()[0]), strings(&["user:hi"]));
    assert_eq!(
        log.shapes_after(3),
        strings(&[
            "abandoned:1",
            "step:2",
            "delta:fresh",
            "completed:fresh:[]",
            "final:fresh",
        ])
    );
    assert_eq!(
        history(&log.rehydrate()),
        strings(&["user:hi", "assistant:fresh:[]"])
    );
    assert_eq!(log.rehydrate(), ctx);
}

// 8d2. Usage is batched with whichever terminal event ends the attempt, not
// appended on its own as it streams in. A model attempt that reports usage
// and then fails must still land that usage in the log, alongside
// `ModelAttemptAbandoned`, so budgets never under-count a failed attempt.
#[tokio::test]
async fn usage_reported_before_a_failed_attempt_still_lands_with_the_abandon() {
    let (log, model, exit) = run_to_budget(
        budget(),
        FakeModel::new(vec![vec![
            usage(10, 5, 100),
            Err(ModelError {
                message: "boom".into(),
            }),
        ]]),
    )
    .await;
    assert_eq!(exit, Exit::Failed);
    assert_eq!(
        log.shapes_after(2),
        strings(&["usage:15", "abandoned:1", "error:model_failed:boom",])
    );
    assert_eq!(model.calls(), 1);
}

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

// 8d4. Thinking summaries stream as progress before the answer: sanitized,
// never part of the answer text, the committed step, or the history.
#[tokio::test]
async fn thinking_streams_as_progress_and_never_joins_the_answer() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![vec![
        thinking("Checking "),
        thinking(&"x".repeat(300)),
        thinking("tail"),
        text("Hello"),
        thinking("after the answer began"),
        usage(3, 2, 7),
    ]]);
    let tools = FakeTools::new(vec![]);
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "hi");
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(
        log.shapes_after(2),
        strings(&[
            "thinking:Checking ",
            &format!("thinking:{}", "x".repeat(300)),
            "thinking:tail",
            "delta:Hello",
            "usage:5",
            "completed:Hello:[]",
            "final:Hello",
        ])
    );
    assert_eq!(log.rehydrate(), ctx);
}

// 8e. A `Started` call whose tool vanished from the offered catalog (deploy,
// grant revoke) resolves through the ledger, never as "unknown tool": the
// model must not be told to retry a call that may have already run under
// a different tool identity.
#[tokio::test]
async fn started_call_with_vanished_tool_settles_unknown_and_is_not_redispatched() {
    let log = FakeLog::default();
    let write = ProposedCall::new(
        call_id("t1", 1, 0),
        ToolName::new("deploy"),
        json!({}),
        alice(),
    );
    crashed_after_start(&log, &write);
    let effects = FakeEffects::default();
    let model = FakeModel::new(vec![vec![text("ok")]]);
    // The tool is no longer offered: catalog is empty, as after a grant
    // revoke or a deploy that dropped it.
    let tools = FakeTools::new(vec![]);
    let engine = engine_with(&log, &model, &tools, &effects, budget());
    let mut ctx = log.rehydrate();

    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert!(tools.runs().is_empty(), "a vanished tool was dispatched");
    assert_eq!(log.shapes()[4], "finished:t1-1-0:unknown");
    let recorded = effects
        .recorded(&write.id)
        .expect("a vanished-tool call must claim itself in the ledger")
        .expect("the ledger must record the settled outcome");
    assert_eq!(recorded.outcome, dex_loop::Outcome::Unknown);
    assert_eq!(
        view(&model.seen()[0])[2],
        "tool:t1-1-0:unknown:outcome unknown: this call already started; \
         do not retry it without first checking whether it took effect"
    );
}

// 8f. `CallId` is unique only within its thread (a turn id is
// caller-chosen); two threads that pick the same turn id get the same raw
// call id string. `Tools::run` still receives the call's thread separately,
// so a downstream key built from both stays globally unique -- the contract
// `ports.rs` documents and the dex-tools idempotency-key fix relies on.
#[tokio::test]
async fn same_turn_id_in_two_threads_dispatches_under_distinct_threads() {
    async fn run_one(thread: dex_loop::ThreadId) -> FakeTools {
        let log = FakeLog::default();
        log.host_append(Event::UserMessage {
            turn: dex_loop::TurnId::new("t1"),
            message_id: None,
            principal: alice(),
            text: "go".into(),
            attachments: vec![],
            client_tools: vec![],
            authorized_tools: Vec::new(),
            approval_mode: dex_loop::ApprovalMode::Interactive,
        });
        let model = FakeModel::new(vec![vec![call("update", json!({}))], vec![text("done")]]);
        let tools = FakeTools::new(vec![write_tool("update")]);
        let effects = FakeEffects::default();
        let engine = engine_with(&log, &model, &tools, &effects, budget());
        let mut ctx = dex_loop::rehydrate(thread, &log.entries());
        assert_eq!(
            engine.run(&mut ctx, &CancellationToken::new()).await,
            Ok(Exit::Done)
        );
        tools
    }

    let thread_a = dex_loop::ThreadId {
        org: "org-1".into(),
        workspace: "ws-1".into(),
        thread: "thread-a".into(),
    };
    let thread_b = dex_loop::ThreadId {
        org: "org-2".into(),
        workspace: "ws-1".into(),
        thread: "thread-b".into(),
    };
    let tools_a = run_one(thread_a.clone()).await;
    let tools_b = run_one(thread_b.clone()).await;

    // The raw CallId string collides: same turn, step and index in both
    // threads.
    assert_eq!(tools_a.run_ids(), strings(&["t1-1-0"]));
    assert_eq!(tools_b.run_ids(), strings(&["t1-1-0"]));
    let run_a = tools_a.run_of(&call_id("t1", 1, 0));
    let run_b = tools_b.run_of(&call_id("t1", 1, 0));
    assert_eq!(run_a.thread, thread_a);
    assert_eq!(run_b.thread, thread_b);
    assert_ne!(
        run_a.thread, run_b.thread,
        "the same CallId was dispatched under different threads"
    );
}

// 9. A fenced write stops the engine: no further writes and no further model
// calls.
#[tokio::test]
async fn fenced_write_stops_immediately() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![text("a"), text("b"), text("c"), call("search", json!({}))],
        vec![text("never")],
    ]);
    let tools = FakeTools::new(vec![read_tool("search")]);
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "go");
    // StepStarted and the first text land; the second text is refused.
    log.fence_after(2);

    let result = engine.run(&mut ctx, &CancellationToken::new()).await;

    assert!(matches!(result, Err(dex_loop::Fenced { .. })));
    assert_eq!(log.refused(), 1, "the engine kept writing after the fence");
    assert_eq!(log.shapes(), strings(&["user:go", "step:1", "delta:a"]));
    assert_eq!(model.calls(), 1);
    assert!(tools.runs().is_empty());
}

#[tokio::test]
async fn fenced_during_a_wave_drops_the_remaining_results() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![vec![
        call("search", json!({"key": "a"})),
        call("search", json!({"key": "b"})),
    ]]);
    let tools = FakeTools::new(vec![read_tool("search")]).delay("b", Duration::from_millis(50));
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "go");
    // StepStarted, ModelStepCompleted and the ToolStarted batch land; the
    // first ToolFinished is refused.
    log.fence_after(3);

    let result = engine.run(&mut ctx, &CancellationToken::new()).await;

    assert!(result.is_err());
    assert_eq!(log.refused(), 1);
    assert_eq!(
        log.shapes(),
        strings(&[
            "user:go",
            "step:1",
            "completed::[t1-1-0,t1-1-1]",
            "started:t1-1-0",
            "started:t1-1-1",
        ])
    );
}

// 10. Unknown, unexposed and denied calls produce results the model sees;
// nothing runs for them.
#[tokio::test]
async fn unknown_and_denied_calls_are_visible_to_the_model() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![
            call("no_such_tool", json!({})),
            call("delete_all", json!({})),
            call("crm.lookup", json!({})),
            call("search", json!({"key": "a"})),
        ],
        vec![text("sorry")],
    ]);
    let tools = FakeTools::new(vec![
        read_tool("search"),
        write_tool("delete_all"),
        hidden_read_tool("crm.lookup"),
    ])
    .verdict(
        "delete_all",
        Verdict::Deny("destructive calls are off".into()),
    );
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "clean up");

    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(tools.run_ids(), strings(&["t1-1-3"]));
    assert_eq!(
        log.shapes_after(1),
        strings(&[
            "step:1",
            "completed::[t1-1-0,t1-1-1,t1-1-2,t1-1-3]",
            "finished:t1-1-0:err",
            "finished:t1-1-1:err",
            "finished:t1-1-2:err",
            "started:t1-1-3",
            "finished:t1-1-3:ok",
            "step:2",
            "delta:sorry",
            "completed:sorry:[]",
            "final:sorry",
        ])
    );
    assert_eq!(
        view(&model.seen()[1])[2..],
        strings(&[
            "tool:t1-1-0:err:unknown tool: no_such_tool; use tools.search to find tools",
            "tool:t1-1-1:err:denied: destructive calls are off",
            "tool:t1-1-2:err:unknown tool: crm.lookup; use tools.search to find tools",
            "tool:t1-1-3:ok:out/t1-1-3",
        ])
    );
    assert_eq!(log.rehydrate(), ctx);
}

// 10b. Headless turns: no human can answer, so a `NeedsApproval` verdict is
// approved by policy. The log keeps the request and the decision, attributed
// to the policy principal; a `Deny` verdict stays denied; an interactive turn
// still parks.
fn headless_tools() -> FakeTools {
    FakeTools::new(vec![write_tool("send_email"), write_tool("delete_all")])
        .verdict("send_email", approval("ap-1"))
        .verdict(
            "delete_all",
            Verdict::Deny("destructive calls are off".into()),
        )
}

#[tokio::test]
async fn headless_turn_auto_approves_an_ask_gated_tool_and_records_the_audit_pair() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![call("send_email", json!({"key": "w"}))],
        vec![text("sent")],
    ]);
    let tools = headless_tools();
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn_with_approval_mode("t1", "email them", ApprovalMode::Headless);
    assert_eq!(ctx.approval_mode(), ApprovalMode::Headless);

    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(tools.run_ids(), strings(&["t1-1-0"]));
    assert_eq!(
        log.shapes_after(1),
        strings(&[
            "step:1",
            "completed::[t1-1-0]",
            "approval:t1-1-0",
            "decided:t1-1-0:true",
            "started:t1-1-0",
            "finished:t1-1-0:ok",
            "step:2",
            "delta:sent",
            "completed:sent:[]",
            "final:sent",
        ])
    );
    let call = call_id("t1", 1, 0);
    let events = log.events();
    let requested = events
        .iter()
        .find_map(|event| match event {
            Event::ApprovalRequested {
                approval,
                args_digest,
                ..
            } => Some((approval.clone(), args_digest.clone())),
            _ => None,
        })
        .expect("approval_requested is recorded");
    assert!(events.iter().any(|event| matches!(
        event,
        Event::ApprovalDecided {
            call: decided_call,
            approval,
            args_digest,
            approved: true,
            principal,
        } if *decided_call == call
            && *approval == requested.0
            && *args_digest == requested.1
            && principal.as_str() == dex_loop::HEADLESS_AUTO_APPROVER
    )));
    // Resumes keep the mode: it is on the logged UserMessage.
    let rehydrated = log.rehydrate();
    assert_eq!(rehydrated.approval_mode(), ApprovalMode::Headless);
    assert_eq!(rehydrated, ctx);
}

#[tokio::test]
async fn headless_turn_keeps_a_hard_deny_denied() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![call("delete_all", json!({}))],
        vec![text("refused")],
    ]);
    let tools = headless_tools();
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn_with_approval_mode("t1", "clean up", ApprovalMode::Headless);

    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert!(tools.run_ids().is_empty());
    assert!(
        !log.events().iter().any(|event| matches!(
            event,
            Event::ApprovalRequested { .. } | Event::ApprovalDecided { .. }
        )),
        "a denied call is never offered for approval"
    );
    assert_eq!(
        view(&model.seen()[1])[2..],
        strings(&["tool:t1-1-0:err:denied: destructive calls are off"])
    );
}

#[tokio::test]
async fn interactive_turn_still_parks_on_the_same_ask_gated_tool() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![vec![call("send_email", json!({"key": "w"}))]]);
    let tools = headless_tools();
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn_with_approval_mode("t1", "email them", ApprovalMode::Interactive);

    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Parked(ApprovalId::new("ap-1")))
    );
    assert!(tools.run_ids().is_empty());
    assert_eq!(
        log.shapes_after(1),
        strings(&["step:1", "completed::[t1-1-0]", "approval:t1-1-0"])
    );
}

#[tokio::test]
async fn headless_turn_auto_approves_a_mutating_client_tool() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![vec![call(
        "browser.click",
        json!({"selector": "#buy"}),
    )]]);
    let tools = FakeTools::new(vec![client_executed_tool("browser.click", false)]);
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn_with_approval_mode("t1", "click buy", ApprovalMode::Headless);

    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::AwaitingClientTool(call_id("t1", 1, 0)))
    );
    assert_eq!(
        log.shapes_after(1),
        strings(&[
            "step:1",
            "completed::[t1-1-0]",
            "approval:t1-1-0",
            "decided:t1-1-0:true",
            "client_tool:t1-1-0:browser.click",
        ])
    );
}

// 11. Compaction is an event, applied before the model call, and rehydration
// applies it the same way.
#[tokio::test]
async fn compaction_is_emitted_and_applied() {
    let log = FakeLog::default();
    let first = FakeModel::new(vec![vec![text(&"long answer ".repeat(10))]]);
    let tools = FakeTools::new(vec![]);
    let mut ctx = log.start_turn("t0", "first question");
    assert_eq!(
        engine(&log, &first, &tools, budget())
            .run(&mut ctx, &CancellationToken::new())
            .await,
        Ok(Exit::Done)
    );
    let before = log.len();

    let model = FakeModel::new(vec![vec![text("short")]]);
    let engine = Engine::new(
        log.clone(),
        model.clone(),
        tools.clone(),
        FakeEffects::default(),
        Lexicon::default(),
        budget(),
    )
    .with_compactor(Threshold::new(100, 1, FakeSummarizer));
    let mut ctx = log.start_turn("t1", "second question");

    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(
        log.shapes_after(before),
        strings(&[
            "user:second question",
            "compaction:summary of 2 entries",
            "step:1",
            "delta:short",
            "completed:short:[]",
            "final:short",
        ])
    );
    assert_eq!(
        view(&model.seen()[0]),
        strings(&["summary:summary of 2 entries", "user:second question"])
    );
    assert_eq!(log.rehydrate(), ctx);
}

// 12. The sanitizer replaces internal identifiers in deltas, including ones
// split across chunks; the stored text equals the streamed text.
#[tokio::test]
async fn sanitizer_replaces_internal_tool_identifiers() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![vec![
        text("Using line"),
        text("ar.search_issues from Maes"),
        text("tro now"),
    ]]);
    let tools = FakeTools::new(vec![]);
    let engine = Engine::new(
        log.clone(),
        model.clone(),
        tools.clone(),
        FakeEffects::default(),
        Lexicon::new([
            ("linear.search_issues", "Linear search"),
            ("maestro", "Dex"),
        ]),
        budget(),
    );
    let mut ctx = log.start_turn("t1", "status?");

    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(
        log.text_writes(),
        strings(&["Using ", "Linear search from ", "Dex now"])
    );
    assert_eq!(
        log.shapes_after(1),
        strings(&[
            "step:1",
            "delta:Using Linear search from Dex now",
            "completed:Using Linear search from Dex now:[]",
            "final:Using Linear search from Dex now",
        ])
    );
    let serialized = serde_json::to_string(&log.events()).expect("serialize events");
    assert!(!serialized.contains("search_issues"));
    assert!(!serialized.to_lowercase().contains("maestro"));
}

// 13. tools.search exposes a non-core tool; its schema is offered from the
// next step, and the exposure is an event.
#[tokio::test]
async fn tools_search_exposes_schemas_for_the_next_step() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![call("tools.search", json!({"query": "crm"}))],
        vec![call("crm.lookup", json!({"key": "acme"}))],
        vec![text("found acme")],
    ]);
    let tools = FakeTools::new(vec![read_tool("search"), hidden_read_tool("crm.lookup")])
        .search_result("crm", &["crm.lookup"]);
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "find acme in the crm");

    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(
        log.shapes_after(1),
        strings(&[
            "step:1",
            "completed::[t1-1-0]",
            "started:t1-1-0",
            "exposed:[crm.lookup]",
            "finished:t1-1-0:ok",
            "step:2",
            "completed::[t1-2-0]",
            "started:t1-2-0",
            "finished:t1-2-0:ok",
            "step:3",
            "delta:found acme",
            "completed:found acme:[]",
            "final:found acme",
        ])
    );
    let offered = model.offered();
    assert_eq!(offered[0], strings(&["tools.search", "search"]));
    assert_eq!(
        offered[1],
        strings(&["tools.search", "search", "crm.lookup"])
    );
    assert_eq!(tools.run_ids(), strings(&["t1-2-0"]));
    assert_eq!(
        view(&model.seen()[1])[2..],
        strings(&["tool:t1-1-0:ok:crm.lookup: Label for crm.lookup"])
    );
    assert_eq!(log.rehydrate(), ctx);
}

// A question parks the turn; the answer is the call's result.
#[tokio::test]
async fn question_parks_and_answer_resumes() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![call("ask_user", json!({"question": "Which region?"}))],
        vec![text("using eu")],
    ]);
    let tools = FakeTools::new(vec![ask_tool("ask_user")]);
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "deploy");
    let cancel = CancellationToken::new();

    assert_eq!(
        engine.run(&mut ctx, &cancel).await,
        Ok(Exit::Asked(call_id("t1", 1, 0)))
    );
    log.host_append(Event::Answer {
        call: call_id("t1", 1, 0),
        principal: alice(),
        text: "eu".into(),
    });
    assert_eq!(engine.run(&mut ctx, &cancel).await, Ok(Exit::Done));
    assert_eq!(
        log.shapes_after(1),
        strings(&[
            "step:1",
            "completed::[t1-1-0]",
            "question:t1-1-0:Which region?",
            "answer:t1-1-0:eu",
            "finished:t1-1-0:ok",
            "step:2",
            "delta:using eu",
            "completed:using eu:[]",
            "final:using eu",
        ])
    );
    assert_eq!(view(&model.seen()[1])[2..], strings(&["tool:t1-1-0:ok:eu"]));
    assert!(tools.runs().is_empty());
    assert_eq!(log.rehydrate(), ctx);
}

// `Context` stores a turn's client-declared tools exactly as logged, for a
// host to compose into `Tools::catalog()` on rehydrate.
#[tokio::test]
async fn context_carries_the_declared_client_tools_unmodified() {
    let log = FakeLog::default();
    let declared = vec![client_tool("browser.read_tab", true)];
    let ctx = log.start_turn_with_client_tools("t1", "hi", declared.clone());
    assert_eq!(ctx.client_tools(), declared.as_slice());
    assert_eq!(log.rehydrate().client_tools(), declared.as_slice());
}

// A read-only client tool parks the turn; SubmitToolResult resumes it.
#[tokio::test]
async fn client_tool_is_requested_and_result_resumes() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![call("browser.read_tab", json!({}))],
        vec![text("Acme's pricing page")],
    ]);
    let tools = FakeTools::new(vec![client_executed_tool("browser.read_tab", true)]);
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "what's on my tab?");
    let cancel = CancellationToken::new();

    let call = call_id("t1", 1, 0);
    assert_eq!(
        engine.run(&mut ctx, &cancel).await,
        Ok(Exit::AwaitingClientTool(call.clone()))
    );
    log.submit_tool_result(&call, dex_loop::Outcome::Succeeded, "Acme pricing");
    assert_eq!(engine.run(&mut ctx, &cancel).await, Ok(Exit::Done));
    assert_eq!(
        log.shapes_after(1),
        strings(&[
            "step:1",
            "completed::[t1-1-0]",
            "client_tool:t1-1-0:browser.read_tab",
            "client_tool_result:t1-1-0:ok",
            "finished:t1-1-0:ok",
            "step:2",
            "delta:Acme's pricing page",
            "completed:Acme's pricing page:[]",
            "final:Acme's pricing page",
        ])
    );
    // The client's raw text is routed through `Tools::wrap_client_result`
    // before the model sees it -- the same shaping a live dispatch of any
    // other executor gets -- not appended to history unwrapped.
    assert_eq!(
        view(&model.seen()[1])[2..],
        strings(&["tool:t1-1-0:ok:wrapped:Acme pricing"])
    );
    assert_eq!(tools.wrapped_calls(), vec![call.clone()]);
    assert!(
        tools.runs().is_empty(),
        "the client, not Tools::run, executes it"
    );
    assert_eq!(log.rehydrate(), ctx);
}

// A mutating client tool parks for approval before it is requested; denying
// it never reaches the client.
#[tokio::test]
async fn a_mutating_client_tool_needs_approval_first() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![vec![call(
        "browser.click",
        json!({"selector": "#buy"}),
    )]]);
    let tools = FakeTools::new(vec![client_executed_tool("browser.click", false)]);
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "click buy");
    let cancel = CancellationToken::new();

    let call = call_id("t1", 1, 0);
    assert_eq!(
        engine.run(&mut ctx, &cancel).await,
        Ok(Exit::Parked(ApprovalId::new(format!("client-{call}"))))
    );
    log.decide(&call, &format!("client-{call}"), false);
    assert_eq!(engine.run(&mut ctx, &cancel).await, Ok(Exit::Failed));
    assert_eq!(
        log.shapes_after(1),
        strings(&[
            "step:1",
            "completed::[t1-1-0]",
            "approval:t1-1-0",
            "decided:t1-1-0:false",
            "finished:t1-1-0:err",
            "step:2",
            "abandoned:2",
            "error:model_failed:no script left",
        ])
    );
    assert_eq!(log.rehydrate(), ctx);
}

// A model failure abandons the attempt and ends the turn with model_failed.
#[tokio::test]
async fn model_failure_abandons_the_attempt_and_fails() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![]);
    let tools = FakeTools::new(vec![]);
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "hi");
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Failed)
    );
    assert_eq!(
        log.shapes_after(1),
        strings(&["step:1", "abandoned:1", "error:model_failed:no script left",])
    );
    assert_eq!(log.rehydrate(), ctx);
}

// Kernel contract: rehydrating a log suffix must never let a control event
// from before the suffix's start be (re-)applied against whatever turn is
// current once the replay catches up -- neither directly, nor indirectly
// via the engine's own `Log::control_since(ctx.control_cursor())` call
// re-fetching it.

// 12a. A suffix that excludes an old, already-resolved `Interrupt` (but
// includes its `Interrupted` and everything after) must leave
// `control_cursor()` high enough that `control_since` never re-fetches that
// interrupt -- otherwise it is observed again once the new turn is
// `Running` and kills it before it ever gets to run.
#[tokio::test]
async fn a_stale_interrupt_excluded_from_the_rehydrated_suffix_must_not_kill_the_new_turn() {
    let log = FakeLog::default();
    log.host_append(Event::UserMessage {
        turn: TurnId::new("t1"),
        message_id: None,
        principal: alice(),
        text: "long task".into(),
        attachments: Vec::new(),
        client_tools: Vec::new(),
        authorized_tools: Vec::new(),
        approval_mode: dex_loop::ApprovalMode::Interactive,
    }); // cursor 1
    log.host_append(Event::Interrupt { principal: alice() }); // cursor 2 -- excluded from the suffix below
    log.host_append(Event::Interrupted); // cursor 3
    log.host_append(Event::UserMessage {
        turn: TurnId::new("t2"),
        message_id: None,
        principal: alice(),
        text: "fresh task".into(),
        attachments: Vec::new(),
        client_tools: Vec::new(),
        authorized_tools: Vec::new(),
        approval_mode: dex_loop::ApprovalMode::Interactive,
    }); // cursor 4

    // Exactly what `read_for_rehydrate` returns when a compaction boundary
    // lands between an old turn's `Interrupt` and its `Interrupted`: the
    // suffix contains zero control-kind events even though a real one (the
    // `Interrupt` at cursor 2) exists earlier in the full log.
    let suffix: Vec<_> = log
        .entries()
        .into_iter()
        .filter(|(cursor, _)| cursor.0 >= 3)
        .collect();
    assert!(
        suffix.iter().all(|(_, event)| !event.is_control()),
        "this suffix must contain no control-kind event for the scenario to be meaningful"
    );
    let mut ctx = dex_loop::rehydrate(thread(), &suffix);
    assert_eq!(ctx.turn(), Some(&TurnId::new("t2")));

    let model = FakeModel::new(vec![vec![text("hi again")]]);
    let tools = FakeTools::new(vec![]);
    let engine = engine(&log, &model, &tools, budget());
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done),
        "an Interrupt excluded from the rehydrated suffix must not be re-fetched and \
         applied against the new turn"
    );
}

// 12b. A `Steer` queued for a turn whose own `UserMessage` is before the
// rehydrate point (excluded from the suffix) must not survive to be flushed
// into a later, unrelated turn's history.
#[tokio::test]
async fn a_steer_from_before_the_rehydrate_point_is_not_carried_into_a_later_turn() {
    let log = FakeLog::default();
    log.host_append(Event::UserMessage {
        turn: TurnId::new("t1"),
        message_id: None,
        principal: alice(),
        text: "long task".into(),
        attachments: Vec::new(),
        client_tools: Vec::new(),
        authorized_tools: Vec::new(),
        approval_mode: dex_loop::ApprovalMode::Interactive,
    }); // cursor 1 -- excluded from the suffix below
    log.host_append(Event::Steer {
        principal: bob(),
        text: "focus on the renewal date".into(),
    }); // cursor 2 -- never flushed within t1
    log.host_append(Event::Final {
        text: "done".into(),
    }); // cursor 3
    log.host_append(Event::UserMessage {
        turn: TurnId::new("t2"),
        message_id: None,
        principal: alice(),
        text: "a completely different question".into(),
        attachments: Vec::new(),
        client_tools: Vec::new(),
        authorized_tools: Vec::new(),
        approval_mode: dex_loop::ApprovalMode::Interactive,
    }); // cursor 4

    // Excludes t1's own `UserMessage` (cursor 1) but includes its orphaned
    // `Steer` (cursor 2) and everything after -- exactly what a compaction
    // boundary landing mid-turn, before the steer is flushed, produces.
    let suffix: Vec<_> = log
        .entries()
        .into_iter()
        .filter(|(cursor, _)| cursor.0 >= 2)
        .collect();
    let ctx = dex_loop::rehydrate(thread(), &suffix);
    assert_eq!(
        history(&ctx),
        strings(&["user:a completely different question"]),
        "bob's steer for a turn this replay never saw start must not appear in t2's history"
    );
}

// Provider reasoning is committed with its step and survives a park and a
// crash: after a fresh engine rehydrates the log and the approval resumes
// the step, the next model call sees it on the assistant message.
#[tokio::test]
async fn reasoning_is_committed_with_its_step_and_returned_after_park_and_crash() {
    let reasoning = dex_loop::ProviderReasoning {
        format: "google.gemini.v1".into(),
        model: "gemini-3.6-flash".into(),
        payload: json!({"calls": [{"extra_content": {"google": {"thought_signature": "c2ln"}}}]}),
    };
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![
            text("On it."),
            call("send_email", json!({"key": "w"})),
            Ok(dex_loop::ModelChunk::Reasoning(reasoning.clone())),
            usage(1, 1, 0),
        ],
        vec![text("sent")],
    ]);
    let tools =
        FakeTools::new(vec![write_tool("send_email")]).verdict("send_email", approval("ap-1"));
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "email them");
    let cancel = CancellationToken::new();
    assert_eq!(
        engine.run(&mut ctx, &cancel).await,
        Ok(Exit::Parked(ApprovalId::new("ap-1")))
    );
    let committed: Vec<_> = log
        .events()
        .into_iter()
        .filter_map(|event| match event {
            Event::ModelStepCompleted { reasoning, .. } => Some(reasoning),
            _ => None,
        })
        .collect();
    assert_eq!(committed, vec![Some(reasoning.clone())]);

    // -- crash: a fresh engine and context from the log, then the approval --
    let engine = support::engine(&log, &model, &tools, budget());
    log.decide(&call_id("t1", 1, 0), "ap-1", true);
    let mut ctx = log.rehydrate();
    assert_eq!(engine.run(&mut ctx, &cancel).await, Ok(Exit::Done));

    let seen = model.seen();
    let returned: Vec<_> = seen[1]
        .iter()
        .filter_map(|message| match message {
            dex_loop::Message::Assistant { reasoning, .. } => Some(reasoning.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(returned, vec![Some(reasoning)]);
    assert_eq!(log.rehydrate(), ctx);
}
#[tokio::test]
async fn fresh_call_ids_cannot_repeat_an_unknown_mutation() {
    let log = FakeLog::default();
    let write = ProposedCall::new(
        call_id("t1", 1, 0),
        ToolName::new("update"),
        json!({"key":"w"}),
        alice(),
    );
    crashed_after_start(&log, &write);
    let effects = FakeEffects::default().seed(
        write.id.clone(),
        Some(ToolResult::unknown("executor lost the result")),
    );
    let model = FakeModel::new(vec![
        vec![call("update", json!({"key":"w"}))],
        vec![call("update", json!({"key":"w"}))],
        vec![text("check the original")],
    ]);
    let tools = FakeTools::new(vec![write_tool("update")]);
    let engine = engine_with(&log, &model, &tools, &effects, budget());
    let mut ctx = log.rehydrate();
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert!(
        tools.runs().is_empty(),
        "a fresh ID repeated an unknown mutation"
    );
    assert_eq!(
        tools.policy_checks().len(),
        2,
        "current policy still applies"
    );
    for step in [2, 3] {
        assert_eq!(
            effects.recorded(&call_id("t1", step, 0)),
            None,
            "refusal must not claim a second effect"
        );
    }
    assert_eq!(
        effects.recorded(&write.id).unwrap().unwrap().outcome,
        dex_loop::Outcome::Unknown
    );
    assert_eq!(ctx, log.rehydrate());
}

#[tokio::test]
async fn uncertain_repeat_guard_preserves_reads_distinct_operations_and_known_retries() {
    for (prior_outcome, next_tool, next_args) in [
        (
            ToolResult::unknown("lost"),
            "update",
            json!({"key":"different"}),
        ),
        (ToolResult::unknown("lost"), "other", json!({"key":"w"})),
        (ToolResult::unknown("lost"), "search", json!({"key":"w"})),
        (ToolResult::error("not run"), "update", json!({"key":"w"})),
    ] {
        let log = FakeLog::default();
        let write = ProposedCall::new(
            call_id("t1", 1, 0),
            ToolName::new("update"),
            json!({"key":"w"}),
            alice(),
        );
        crashed_after_start(&log, &write);
        let effects = FakeEffects::default().seed(write.id.clone(), Some(prior_outcome));
        let model = FakeModel::new(vec![vec![call(next_tool, next_args)], vec![text("done")]]);
        let tools = FakeTools::new(vec![
            write_tool("update"),
            write_tool("other"),
            read_tool("search"),
        ]);
        let engine = engine_with(&log, &model, &tools, &effects, budget());
        let mut ctx = log.rehydrate();
        assert_eq!(
            engine.run(&mut ctx, &CancellationToken::new()).await,
            Ok(Exit::Done)
        );
        assert_eq!(tools.runs().len(), 1);
        assert_eq!(ctx, log.rehydrate());
    }
}

#[tokio::test]
async fn uncertain_mutation_guard_survives_compaction_and_a_new_turn_is_explicit() {
    let log = FakeLog::default();
    let write = ProposedCall::new(
        call_id("t1", 1, 0),
        ToolName::new("update"),
        json!({"key":"w"}),
        alice(),
    );
    crashed_after_start(&log, &write);
    let cursor = log.host_append(Event::ToolFinished {
        call: write.id.clone(),
        outcome: dex_loop::Outcome::Unknown,
        output: dex_loop::Output::Text("lost result".into()),
        receipt: None,
    });
    log.host_append(Event::Compaction {
        covers_to_cursor: cursor,
        summary: "Earlier work requires reconciliation".into(),
    });
    let effects =
        FakeEffects::default().seed(write.id.clone(), Some(ToolResult::unknown("lost result")));
    let model = FakeModel::new(vec![
        vec![call("update", json!({"key":"w"}))],
        vec![text("await a decision")],
        vec![call("update", json!({"key":"w"}))],
        vec![text("done")],
    ]);
    let tools = FakeTools::new(vec![write_tool("update")]);
    let engine = engine_with(&log, &model, &tools, &effects, budget());
    let mut ctx = log.rehydrate();
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert!(tools.runs().is_empty());
    assert_eq!(ctx, log.rehydrate());
    let mut ctx = log.start_turn("t2", "I checked; try it again");
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(tools.runs().len(), 1);
    assert_eq!(tools.policy_checks().len(), 2);
    assert_eq!(ctx, log.rehydrate());
}

#[tokio::test]
async fn unknown_client_mutations_are_not_requested_again_and_unknown_reads_can_retry() {
    for read_only in [false, true] {
        let log = FakeLog::default();
        let prior = ProposedCall::new(
            call_id("t1", 1, 0),
            ToolName::new("client.operation"),
            json!({"key":"w"}),
            alice(),
        );
        crashed_after_start(&log, &prior);
        log.host_append(Event::ToolFinished {
            call: prior.id.clone(),
            outcome: dex_loop::Outcome::Unknown,
            output: dex_loop::Output::Text("lost".into()),
            receipt: None,
        });
        let model = FakeModel::new(vec![
            vec![call("client.operation", json!({"key":"w"}))],
            vec![text("check before retry")],
        ]);
        let tools = FakeTools::new(vec![if read_only {
            read_tool("client.operation")
        } else {
            client_executed_tool("client.operation", false)
        }]);
        let engine = engine(&log, &model, &tools, budget());
        let mut ctx = log.rehydrate();
        assert_eq!(
            engine.run(&mut ctx, &CancellationToken::new()).await,
            Ok(Exit::Done)
        );
        assert_eq!(tools.runs().len(), usize::from(read_only));
        assert!(!log.events().iter().any(|event| matches!(
            event,
            Event::ClientToolRequested { .. } | Event::ApprovalRequested { .. }
        )));
        assert_eq!(ctx, log.rehydrate());
    }
}

#[tokio::test]
async fn uncertain_mutations_keep_their_principal_identity() {
    let log = FakeLog::default();
    let prior = ProposedCall::new(
        call_id("t1", 1, 0),
        ToolName::new("update"),
        json!({"key":"w"}),
        alice(),
    );
    crashed_after_start(&log, &prior);
    log.host_append(Event::ToolFinished {
        call: prior.id.clone(),
        outcome: dex_loop::Outcome::Unknown,
        output: dex_loop::Output::Text("lost".into()),
        receipt: None,
    });
    log.host_append(Event::Steer {
        principal: bob(),
        text: "I checked my operation".into(),
    });
    let model = FakeModel::new(vec![
        vec![call("update", json!({"key":"w"}))],
        vec![text("done")],
    ]);
    let tools = FakeTools::new(vec![write_tool("update")]);
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.rehydrate();
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(tools.runs().len(), 1);
    assert_eq!(tools.policy_checks()[0].1, "bob");
    assert_eq!(ctx, log.rehydrate());
}
