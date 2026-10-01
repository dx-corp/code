//! Budget visibility and recovery exercise the real Engine and model boundary.
// Shared fixtures include helpers used by other integration-test binaries.
#[allow(dead_code)]
mod support;

use dex_loop::{
    Budget, CancellationToken, Context, Engine, Event, Exit, Lexicon, Model, ModelChunk,
    ModelError, PrincipalId, ProposedCall, RemainingBudget, ThreadId, ToolName, ToolResult,
    ToolSpec, Tools, Verdict,
};
use futures_util::{Stream, stream};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::{FakeEffects, FakeLog, read_tool};

type ModelObservation = (RemainingBudget, Option<String>);

#[derive(Clone, Default)]
struct ObservingModel {
    seen: Arc<Mutex<Vec<ModelObservation>>>,
    stubborn: bool,
}
impl Model for ObservingModel {
    fn stream<'a>(
        &'a self,
        ctx: &'a Context,
        _: &'a [&'a ToolSpec],
    ) -> impl Stream<Item = Result<ModelChunk, ModelError>> + Send + 'a {
        let remaining = ctx
            .remaining_budget()
            .expect("every engine attempt has fresh capacity");
        let recovery = ctx.recovery_guidance();
        self.seen
            .lock()
            .expect("seen")
            .push((remaining, recovery.clone()));
        let chunk = if (recovery.is_some() || remaining.answer_only) && !self.stubborn {
            ModelChunk::Text("Blocked: the read failed; I could not verify completion.".into())
        } else {
            ModelChunk::ToolCall {
                name: ToolName::new("read"),
                args: json!({"key":"same"}),
            }
        };
        stream::iter([Ok(chunk)])
    }
}
#[derive(Clone)]
struct FailingTools {
    spec: Vec<ToolSpec>,
    calls: Arc<Mutex<usize>>,
}
impl FailingTools {
    fn new() -> Self {
        Self {
            spec: vec![read_tool("read")],
            calls: Arc::default(),
        }
    }
}
impl Tools for FailingTools {
    fn catalog(&self) -> &[ToolSpec] {
        &self.spec
    }
    async fn search(&self, _: &PrincipalId, _: &str) -> Vec<ToolName> {
        vec![]
    }
    async fn policy(&self, _: &Context, _: &ProposedCall) -> Verdict {
        Verdict::Allow
    }
    async fn run(&self, _: &ThreadId, _: &ProposedCall, _: &CancellationToken) -> ToolResult {
        *self.calls.lock().expect("calls") += 1;
        ToolResult::error("recorded read failure")
    }
}
fn budget(steps: u32) -> Budget {
    Budget {
        max_steps: steps,
        max_tokens: 1234,
        max_cost_micros: 5678,
        wall: Duration::from_secs(10),
    }
}

#[tokio::test]
async fn three_failures_offer_recovery_before_spending_the_whole_budget() {
    let log = FakeLog::default();
    let model = ObservingModel::default();
    let tools = FailingTools::new();
    let engine = Engine::new(
        log.clone(),
        model.clone(),
        tools.clone(),
        FakeEffects::default(),
        Lexicon::default(),
        budget(20),
    );
    let mut ctx = log.start_turn("t1", "verify the record");
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(*tools.calls.lock().expect("calls"), 3);
    let seen = model.seen.lock().expect("seen");
    assert_eq!(seen.len(), 4);
    assert_eq!(seen[0].0.tool_steps, 20);
    assert_eq!(seen[3].0.tool_steps, 17);
    assert_eq!(seen[0].0.tokens, Some(1234));
    assert_eq!(seen[0].0.cost_micros, Some(5678));
    assert!(
        seen[3]
            .1
            .as_ref()
            .is_some_and(|text| text.contains("failed 3"))
    );
    assert_eq!(
        ctx,
        log.rehydrate(),
        "advisory attempt state must not pollute durable context"
    );
}

#[tokio::test]
async fn stubborn_model_cannot_redispatch_stalled_reads_or_cross_answer_only_cap() {
    let log = FakeLog::default();
    let model = ObservingModel {
        stubborn: true,
        ..Default::default()
    };
    let tools = FailingTools::new();
    let engine = Engine::new(
        log.clone(),
        model.clone(),
        tools.clone(),
        FakeEffects::default(),
        Lexicon::default(),
        budget(5),
    );
    let mut ctx = log.start_turn("t1", "read");
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Failed)
    );
    assert_eq!(*tools.calls.lock().expect("calls"), 3);
    let seen = model.seen.lock().expect("seen");
    assert_eq!(seen.len(), 6);
    assert!(seen[5].0.answer_only);
    assert_eq!(seen[5].0.tool_steps, 0);
    assert_eq!(ctx, log.rehydrate());
}

#[tokio::test]
async fn answer_only_remains_available_and_zero_tool_budget_is_visible() {
    let log = FakeLog::default();
    let model = ObservingModel::default();
    let tools = FailingTools::new();
    let engine = Engine::new(
        log.clone(),
        model.clone(),
        tools.clone(),
        FakeEffects::default(),
        Lexicon::default(),
        budget(0),
    );
    let mut ctx = log.start_turn("t1", "report existing evidence");
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(*tools.calls.lock().expect("calls"), 0);
    assert!(model.seen.lock().expect("seen")[0].0.answer_only);
}

#[test]
fn failure_recovery_survives_restart_and_compaction_but_new_input_resets_it() {
    let log = FakeLog::default();
    let mut ctx = log.start_turn("t1", "verify");
    for step in 1..=3 {
        let call = ProposedCall::new(
            dex_loop::CallId::new(format!("call{step}")),
            ToolName::new("read"),
            json!({"key":"same"}),
            support::alice(),
        );
        for event in [
            Event::StepStarted {
                step,
                control_through: ctx.control_cursor(),
            },
            Event::ModelStepCompleted {
                step,
                text: String::new(),
                calls: vec![call.clone()],
                reasoning: None,
                served: None,
                timing: None,
            },
            Event::ToolFinished {
                call: call.id,
                outcome: dex_loop::Outcome::Failed,
                output: dex_loop::Output::Text("failed".into()),
                receipt: None,
            },
        ] {
            let cursor = log.host_append(event.clone());
            ctx.observe(cursor, &event);
        }
    }
    let event = Event::Compaction {
        covers_to_cursor: ctx.cursor(),
        summary: "reads failed".into(),
    };
    let cursor = log.host_append(event.clone());
    ctx.observe(cursor, &event);
    assert_eq!(ctx, log.rehydrate());
    assert!(ctx.recovery_guidance().is_some());
    let event = Event::Steer {
        principal: support::alice(),
        text: "try this newly corrected key".into(),
    };
    let cursor = log.host_append(event.clone());
    ctx.observe(cursor, &event);
    let event = Event::StepStarted {
        step: 4,
        control_through: cursor,
    };
    let cursor = log.host_append(event.clone());
    ctx.observe(cursor, &event);
    assert!(ctx.recovery_guidance().is_none());
    assert_eq!(ctx, log.rehydrate());
}

#[tokio::test]
async fn recovery_never_discards_results_of_reads_that_already_started() {
    let log = FakeLog::default();
    let model = support::FakeModel::new(vec![
        (0..4)
            .map(|_| support::call("read", json!({"key":"same"})))
            .collect(),
        vec![support::text("Blocked: all four attempted reads failed.")],
    ]);
    let tools = FailingTools::new();
    let engine = Engine::new(
        log.clone(),
        model,
        tools.clone(),
        FakeEffects::default(),
        Lexicon::default(),
        budget(5),
    );
    let mut ctx = log.start_turn("t1", "read");
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(*tools.calls.lock().expect("calls"), 4);
    let finishes = log
        .events()
        .into_iter()
        .filter_map(|event| {
            if let Event::ToolFinished { output, .. } = event {
                Some(output)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(finishes.len(), 4);
    assert!(finishes.iter().all(
        |output| matches!(output, dex_loop::Output::Text(text) if text == "recorded read failure")
    ));
    assert_eq!(ctx, log.rehydrate());
}
