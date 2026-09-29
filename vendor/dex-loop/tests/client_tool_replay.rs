//! Crash/restart behavior for `ExecutorKind::Client` calls.
//!
//! Two gaps in how the engine resumed these calls after a restart:
//! - the wait's deadline lived only in the async task that requested it, so
//!   a restart after `ClientToolRequested` rehydrated the call as
//!   `AwaitingClient` with nothing left to ever time it out;
//! - a restart after `ClientToolResult` (but before `ToolFinished`) finished
//!   the call with the client's raw, unwrapped text, skipping the wrap,
//!   output storage and ledger record a live finish gets.
//!
//! Every scenario here builds the log directly (as `Event`s a prior,
//! now-crashed process would have appended) and then rehydrates, exactly the
//! shape a real restart takes.

// This binary only exercises a slice of `support`'s shared helpers (each
// integration test file is its own compilation unit); the rest are real,
// used by `scenarios.rs`.
#[allow(dead_code)]
mod support;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dex_loop::{
    ApprovalId, Budget, CancellationToken, Cursor, Event, Exit, Outcome, Output, ProposedCall,
    ToolName, ToolResult, TurnId,
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

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_millis() as i64
}

/// `UserMessage`, `StepStarted`, `ModelStepCompleted` proposing `call` alone
/// -- the log a warm engine would have written before parking it.
fn log_up_to_model_step(log: &FakeLog, call: &ProposedCall) {
    log.host_append(Event::UserMessage {
        turn: TurnId::new("t1"),
        message_id: None,
        principal: alice(),
        text: "do it".into(),
        attachments: Vec::new(),
        client_tools: Vec::new(),
        authorized_tools: Vec::new(),
        approval_mode: dex_loop::ApprovalMode::Interactive,
    });
    log.host_append(Event::StepStarted {
        step: 1,
        control_through: Cursor::START,
    });
    log.host_append(Event::ModelStepCompleted {
        step: 1,
        text: String::new(),
        calls: vec![call.clone()],
        reasoning: None,
    });
}

/// The approval a mutating client tool parks for before it is requested,
/// already decided `approved`, as `dispatch_client_tool` requires before it
/// ever emits `ClientToolRequested`.
fn approved(log: &FakeLog, call: &ProposedCall) {
    let approval = ApprovalId::new(format!("client-{}", call.id));
    log.host_append(Event::ApprovalRequested {
        call: call.id.clone(),
        approval: approval.clone(),
        args_digest: call.args_digest.clone(),
        summary: "Run it in your browser".into(),
    });
    log.host_append(Event::ApprovalDecided {
        call: call.id.clone(),
        approval,
        args_digest: call.args_digest.clone(),
        approved: true,
        principal: alice(),
    });
}

fn requested(call: &ProposedCall, deadline_ms: i64) -> Event {
    Event::ClientToolRequested {
        call: call.id.clone(),
        tool: call.tool.clone(),
        args: call.args.clone(),
        label: format!("Label for {}", call.tool),
        principal: call.principal.clone(),
        target_session: call.principal.to_string(),
        deadline_ms,
    }
}

fn tool_finished(log: &FakeLog, call: &ProposedCall) -> Option<(Outcome, Output)> {
    log.events().into_iter().find_map(|event| match event {
        Event::ToolFinished {
            call: id,
            outcome,
            output,
            ..
        } if id == call.id => Some((outcome, output)),
        _ => None,
    })
}

// ---------------------------------------------------------------- Gap 1:
// the deadline must survive a restart.

// A restart landing right after `ClientToolRequested`, with time still left
// on the clock, must keep waiting -- not time the call out just because the
// process happened to restart.
#[tokio::test]
async fn a_restart_before_the_deadline_still_parks_on_the_client_tool() {
    let log = FakeLog::default();
    let call = ProposedCall::new(
        call_id("t1", 1, 0),
        ToolName::new("browser.read_tab"),
        json!({}),
        alice(),
    );
    log_up_to_model_step(&log, &call);
    log.host_append(requested(&call, now_ms() + 60_000));

    let model = FakeModel::new(vec![]);
    let tools = FakeTools::new(vec![client_executed_tool("browser.read_tab", true)]);
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.rehydrate();

    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::AwaitingClientTool(call.id.clone())),
        "a restart before the deadline must not time the call out early"
    );
    assert_eq!(log.rehydrate(), ctx);
}

// A restart landing after the deadline has already passed must finish the
// call as timed out on this very run, not park it forever: nothing else is
// ever going to re-arm a timer for it.
#[tokio::test]
async fn a_restart_after_the_deadline_has_passed_times_the_call_out() {
    let log = FakeLog::default();
    let call = ProposedCall::new(
        call_id("t1", 1, 0),
        ToolName::new("browser.read_tab"),
        json!({}),
        alice(),
    );
    log_up_to_model_step(&log, &call);
    log.host_append(requested(&call, now_ms() - 1));

    let model = FakeModel::new(vec![vec![text("moving on")]]);
    let tools = FakeTools::new(vec![client_executed_tool("browser.read_tab", true)]);
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.rehydrate();

    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    let (outcome, _) = tool_finished(&log, &call).expect("the call must finish, not park forever");
    assert_eq!(outcome, Outcome::Failed);
    assert!(
        !log.events()
            .iter()
            .any(|event| matches!(event, Event::ClientToolResult { .. })),
        "no client ever answered; the engine's own deadline, not a late result, must have finished this call"
    );
    assert_eq!(log.rehydrate(), ctx);
}

// The same expiry for a mutation: it must go through the effect ledger, and
// a later restart landing on the same expired deadline must adopt the
// recorded outcome rather than manufacture (and ledger) a second one.
#[tokio::test]
async fn a_timed_out_mutation_is_ledgered_once() {
    let log = FakeLog::default();
    let call = ProposedCall::new(
        call_id("t1", 1, 0),
        ToolName::new("browser.click"),
        json!({"selector": "#buy"}),
        alice(),
    );
    log_up_to_model_step(&log, &call);
    approved(&log, &call);
    log.host_append(requested(&call, now_ms() - 1));

    let effects = FakeEffects::default().seed(
        call.id.clone(),
        Some(ToolResult::unknown("previously recorded timeout")),
    );
    let model = FakeModel::new(vec![vec![text("done")]]);
    let tools = FakeTools::new(vec![client_executed_tool("browser.click", false)]);
    let engine = engine_with(&log, &model, &tools, &effects, budget());
    let mut ctx = log.rehydrate();

    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    // The ledger already had an entry (as if a still-earlier crash had
    // recorded one): this run must adopt it, not overwrite it with a fresh
    // timeout message.
    let (_, output) = tool_finished(&log, &call).expect("finished");
    assert_eq!(output, Output::Text("previously recorded timeout".into()));
    assert_eq!(
        effects.recorded(&call.id),
        Some(Some(ToolResult::unknown("previously recorded timeout"))),
        "the seeded ledger entry must be unchanged, not replaced by a second recording"
    );
}

// ---------------------------------------------------------------- Gap 2:
// a replayed `ClientToolResult` must go through the same shaping as live.

// A crash after `ClientToolResult` but before `ToolFinished`: the reported
// mutation must be wrapped, stored and ledgered on replay, not finished with
// the client's raw text.
#[tokio::test]
async fn a_restart_after_client_tool_result_wraps_stores_and_ledgers_the_reported_mutation() {
    let log = FakeLog::default();
    let call = ProposedCall::new(
        call_id("t1", 1, 0),
        ToolName::new("browser.click"),
        json!({"selector": "#buy"}),
        alice(),
    );
    log_up_to_model_step(&log, &call);
    approved(&log, &call);
    log.host_append(requested(&call, now_ms() + 60_000));
    // The client answered, but the process crashed before `ToolFinished`.
    log.host_append(Event::ClientToolResult {
        call: call.id.clone(),
        principal: alice(),
        outcome: Outcome::Succeeded,
        output: "clicked".into(),
    });

    let effects = FakeEffects::default();
    let model = FakeModel::new(vec![vec![text("done")]]);
    let tools = FakeTools::new(vec![client_executed_tool("browser.click", false)]);
    let engine = engine_with(&log, &model, &tools, &effects, budget());
    let mut ctx = log.rehydrate();

    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(
        tools.wrapped_calls(),
        vec![call.id.clone()],
        "the replayed result must be routed through the host's wrap exactly once"
    );
    let (outcome, output) = tool_finished(&log, &call).expect("finished");
    assert_eq!(outcome, Outcome::Succeeded);
    assert_eq!(output, Output::Text("wrapped:clicked".into()));
    assert_eq!(
        effects.recorded(&call.id),
        Some(Some(ToolResult::text("wrapped:clicked"))),
        "the wrapped result, not the client's raw text, must reach the ledger"
    );
    assert_eq!(log.rehydrate(), ctx);
}

// A read-only client tool's replayed result is wrapped too, but never
// touches the effect ledger -- reads carry no ledger entry anywhere else in
// this engine either.
#[tokio::test]
async fn a_restart_after_client_tool_result_wraps_a_reported_read_without_ledgering_it() {
    let log = FakeLog::default();
    let call = ProposedCall::new(
        call_id("t1", 1, 0),
        ToolName::new("browser.read_tab"),
        json!({}),
        alice(),
    );
    log_up_to_model_step(&log, &call);
    log.host_append(requested(&call, now_ms() + 60_000));
    log.host_append(Event::ClientToolResult {
        call: call.id.clone(),
        principal: alice(),
        outcome: Outcome::Succeeded,
        output: "Acme pricing".into(),
    });

    let effects = FakeEffects::default();
    let model = FakeModel::new(vec![vec![text("done")]]);
    let tools = FakeTools::new(vec![client_executed_tool("browser.read_tab", true)]);
    let engine = engine_with(&log, &model, &tools, &effects, budget());
    let mut ctx = log.rehydrate();

    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(tools.wrapped_calls(), vec![call.id.clone()]);
    let (_, output) = tool_finished(&log, &call).expect("finished");
    assert_eq!(output, Output::Text("wrapped:Acme pricing".into()));
    assert_eq!(
        effects.recorded(&call.id),
        None,
        "a read never touches the ledger"
    );
    assert_eq!(log.rehydrate(), ctx);
}

// A still-earlier crash already wrapped, stored and ledgered this exact
// call (it just never got to append `ToolFinished`): this restart must
// adopt that recorded result rather than ask the host to wrap it again.
#[tokio::test]
async fn a_second_restart_adopts_the_ledgered_wrap_instead_of_wrapping_twice() {
    let log = FakeLog::default();
    let call = ProposedCall::new(
        call_id("t1", 1, 0),
        ToolName::new("browser.click"),
        json!({"selector": "#buy"}),
        alice(),
    );
    log_up_to_model_step(&log, &call);
    approved(&log, &call);
    log.host_append(requested(&call, now_ms() + 60_000));
    log.host_append(Event::ClientToolResult {
        call: call.id.clone(),
        principal: alice(),
        outcome: Outcome::Succeeded,
        output: "clicked".into(),
    });

    let effects = FakeEffects::default().seed(
        call.id.clone(),
        Some(ToolResult::text("previously recorded wrap")),
    );
    let model = FakeModel::new(vec![vec![text("done")]]);
    let tools = FakeTools::new(vec![client_executed_tool("browser.click", false)]);
    let engine = engine_with(&log, &model, &tools, &effects, budget());
    let mut ctx = log.rehydrate();

    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert!(
        tools.wrapped_calls().is_empty(),
        "an existing ledger claim must be adopted, not wrapped a second time"
    );
    let (_, output) = tool_finished(&log, &call).expect("finished");
    assert_eq!(output, Output::Text("previously recorded wrap".into()));
}

// ---------------------------------------------------------------- P2:
// interrupt must still be able to end a client-tool wait.

// A turn parked on `Exit::AwaitingClientTool` must still end at the next
// `Interrupt`, exactly like every other parked wait.
#[tokio::test]
async fn interrupting_a_turn_parked_on_a_client_tool_ends_it_promptly() {
    let log = FakeLog::default();
    let call = ProposedCall::new(
        call_id("t1", 1, 0),
        ToolName::new("browser.read_tab"),
        json!({}),
        alice(),
    );
    log_up_to_model_step(&log, &call);
    log.host_append(requested(&call, now_ms() + 60_000));
    log.host_append(Event::Interrupt { principal: alice() });

    let model = FakeModel::new(vec![]);
    let tools = FakeTools::new(vec![client_executed_tool("browser.read_tab", true)]);
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.rehydrate();

    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Interrupted)
    );
    assert_eq!(log.rehydrate(), ctx);
}
