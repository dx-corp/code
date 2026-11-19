//! Budget visibility and recovery exercise the real Engine and model boundary.
// Shared fixtures include helpers used by other integration-test binaries.
use crate::support;

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
    /// Repeats the failed call despite recovery guidance, but answers when
    /// no tools are offered.
    ignores_recovery: bool,
    /// The tool it calls; `read` when empty.
    tool: &'static str,
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
        let heeds = remaining.answer_only || (recovery.is_some() && !self.ignores_recovery);
        let chunk = if heeds && !self.stubborn {
            ModelChunk::Text("Blocked: the read failed; I could not verify completion.".into())
        } else {
            ModelChunk::ToolCall {
                name: ToolName::new(if self.tool.is_empty() {
                    "read"
                } else {
                    self.tool
                }),
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
        Self::named("read")
    }
    fn named(name: &str) -> Self {
        Self {
            spec: vec![read_tool(name)],
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
    // Three failures, one refusal, then the answer-only call: the step
    // budget is never the thing that stops it.
    assert_eq!(seen.len(), 5);
    assert!(!seen[3].0.answer_only);
    assert!(seen[4].0.answer_only);
    assert_eq!(seen[4].0.tool_steps, 0);
    assert_eq!(ctx, log.rehydrate());
}

#[tokio::test]
async fn a_refused_stalled_call_withholds_tools_so_the_model_reports_the_blocker() {
    // Production shape (turn operating_turn_3af07df2): the model repeated an
    // identical failing `dex.describe` call through the recovery guidance and
    // 27 stall-guard refusals, one model call each, and never answered. With
    // a 30-step budget the old loop made 31 model calls; the turn must instead
    // answer on the call after the first refusal.
    let log = FakeLog::default();
    let model = ObservingModel {
        ignores_recovery: true,
        tool: "dex.describe",
        ..Default::default()
    };
    let tools = FailingTools::named("dex.describe");
    let engine = Engine::new(
        log.clone(),
        model.clone(),
        tools.clone(),
        FakeEffects::default(),
        Lexicon::default(),
        budget(30),
    );
    let mut ctx = log.start_turn("t1", "describe what you can do");
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(*tools.calls.lock().expect("calls"), 3);
    let seen = model.seen.lock().expect("seen");
    assert_eq!(seen.len(), 5, "three failures, one refusal, one answer");
    assert!(
        seen[..4]
            .iter()
            .all(|(remaining, _)| !remaining.answer_only)
    );
    assert!(seen[4].0.answer_only);
    assert_eq!(seen[4].0.tool_steps, 0);
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
                interruption: None,
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
                summary: None,
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

/// Calls `run` with a script on every step until no tools are offered, then
/// answers. `code` receives the model call's number, from 1.
#[derive(Clone)]
struct ScriptingModel {
    seen: Arc<Mutex<Vec<ModelObservation>>>,
    code: fn(usize) -> String,
}
impl ScriptingModel {
    fn new(code: fn(usize) -> String) -> Self {
        Self {
            seen: Arc::default(),
            code,
        }
    }
}
impl Model for ScriptingModel {
    fn stream<'a>(
        &'a self,
        ctx: &'a Context,
        _: &'a [&'a ToolSpec],
    ) -> impl Stream<Item = Result<ModelChunk, ModelError>> + Send + 'a {
        let remaining = ctx
            .remaining_budget()
            .expect("every engine attempt has fresh capacity");
        let mut seen = self.seen.lock().expect("seen");
        seen.push((remaining, ctx.recovery_guidance()));
        let chunk = if remaining.answer_only {
            ModelChunk::Text("I could not find a record of that investor.".into())
        } else {
            ModelChunk::ToolCall {
                name: ToolName::new(dex_loop::CODEMODE),
                args: json!({"code": (self.code)(seen.len())}),
            }
        };
        stream::iter([Ok(chunk)])
    }
}

/// Every finished call's outcome and model-facing text, in log order.
fn finished(log: &FakeLog) -> Vec<(dex_loop::Outcome, String)> {
    log.events()
        .into_iter()
        .filter_map(|event| match event {
            Event::ToolFinished {
                outcome,
                output: dex_loop::Output::Text(text),
                ..
            } => Some((outcome, text)),
            _ => None,
        })
        .collect()
}

async fn run_scripts(
    code: fn(usize) -> String,
    steps: u32,
) -> (Exit, Vec<ModelObservation>, FakeLog) {
    let log = FakeLog::default();
    let model = ScriptingModel::new(code);
    let engine = Engine::new(
        log.clone(),
        model.clone(),
        support::FakeTools::new(vec![read_tool("read")]),
        FakeEffects::default(),
        Lexicon::default(),
        budget(steps),
    );
    let mut ctx = log.start_turn("t1", "I am meeting an investor. What should I focus on?");
    let exit = engine
        .run(&mut ctx, &CancellationToken::new())
        .await
        .expect("not fenced");
    assert_eq!(ctx, log.rehydrate());
    let seen = model.seen.lock().expect("seen").clone();
    (exit, seen, log)
}

#[tokio::test]
async fn scripts_that_call_no_tool_stall_after_two_and_answer() {
    // Production shape (2026-11-18, turn operating_turn_b8642fdf): 39 scripts
    // ran only in-script discovery, called no tool and all "succeeded", so
    // the turn ran to its 40-step cap in 92 s. The scripts differed, so no
    // identical-call count would have seen them. Whatever a script selects,
    // the second in a row that calls no tool ends tool use.
    for code in [
        (|_| "searchTools('investor crm');".to_owned()) as fn(usize) -> String,
        |step| format!("text(searchTools('investor crm {step}'));"),
        |step| format!("text(describeTool('crm.read_{step}'));"),
        |_| "const unused = 1;".to_owned(),
    ] {
        let (exit, seen, log) = run_scripts(code, 40).await;
        assert_eq!(exit, Exit::Done);
        assert_eq!(seen.len(), 3, "two scripts, one answer");
        assert!(
            seen[..2]
                .iter()
                .all(|(remaining, recovery)| { !remaining.answer_only && recovery.is_none() })
        );
        assert!(seen[2].0.answer_only);
        assert_eq!(seen[2].0.tool_steps, 0);
        assert!(
            seen[2]
                .1
                .as_ref()
                .is_some_and(|text| text.contains("The last 2 scripts called no tool")),
            "{:?}",
            seen[2].1
        );
        let results = finished(&log);
        assert_eq!(results.len(), 2);
        assert!(
            results
                .iter()
                .all(|(outcome, _)| *outcome == dex_loop::Outcome::Succeeded)
        );
        // The first result is what the script selected; the second keeps
        // what it selected and ends with the fixed notice.
        assert!(!results[0].1.contains(dex_loop::IDLE_SCRIPTS_NOTICE));
        assert!(
            results[1].1.ends_with(dex_loop::IDLE_SCRIPTS_NOTICE),
            "{}",
            results[1].1
        );
        assert!(results[1].1.len() > dex_loop::IDLE_SCRIPTS_NOTICE.len());
        assert!(matches!(log.events().last(), Some(Event::Final { .. })));
    }
}

/// Always succeeds with the same inline text.
#[derive(Clone)]
struct SameResultTools {
    spec: Vec<ToolSpec>,
    calls: Arc<Mutex<usize>>,
}
impl Tools for SameResultTools {
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
        ToolResult {
            outcome: dex_loop::Outcome::Succeeded,
            output: dex_loop::Output::Text("no rows".into()),
            receipt: None,
        }
    }
}

#[tokio::test]
async fn an_identical_call_returning_the_same_result_stalls_like_a_failing_one() {
    let log = FakeLog::default();
    let model = ObservingModel {
        ignores_recovery: true,
        ..Default::default()
    };
    let tools = SameResultTools {
        spec: vec![read_tool("read")],
        calls: Arc::default(),
    };
    let engine = Engine::new(
        log.clone(),
        model.clone(),
        tools.clone(),
        FakeEffects::default(),
        Lexicon::default(),
        budget(30),
    );
    let mut ctx = log.start_turn("t1", "find the investor record");
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(*tools.calls.lock().expect("calls"), 3);
    let seen = model.seen.lock().expect("seen");
    assert_eq!(seen.len(), 5, "three reads, one refusal, one answer");
    assert!(
        seen[3]
            .1
            .as_ref()
            .is_some_and(|text| text.contains("returned the same result 3 consecutive times")),
        "{:?}",
        seen[3].1
    );
    assert!(seen[4].0.answer_only);
    let results = finished(&log);
    assert_eq!(results[3].0, dex_loop::Outcome::Failed);
    assert!(
        results[3]
            .1
            .contains("returned the same result three times"),
        "{}",
        results[3].1
    );
    assert_eq!(ctx, log.rehydrate());
}

#[tokio::test]
async fn a_script_result_says_when_nothing_was_selected_or_found() {
    let (_, _, log) = run_scripts(|_| "const unused = 1;".to_owned(), 1).await;
    assert_eq!(finished(&log)[0].1, agent_codemode::NO_OUTPUT);

    let (_, _, log) = run_scripts(|_| "text(searchTools('investor crm'));".to_owned(), 1).await;
    assert_eq!(
        finished(&log)[0].1,
        "[]\nsearchTools(\"investor crm\") found nothing. It inspects only the tools admitted to this script, not every tool the host can reach.\nTo find tools beyond the admitted ones, call tools_search directly, not from a script. A later script can call what it finds."
    );

    let (_, _, log) = run_scripts(
        |_| "searchTools('investor crm'); describeTool('crm.read');".to_owned(),
        1,
    )
    .await;
    let text = &finished(&log)[0].1;
    assert!(text.starts_with(agent_codemode::NO_OUTPUT), "{text}");
    assert!(text.contains("describeTool(\"crm.read\") found nothing."));
    assert!(text.ends_with("A later script can call what it finds."));

    // A search that matches says nothing extra.
    let (_, _, log) = run_scripts(|_| "text(searchTools('read').length);".to_owned(), 1).await;
    assert_eq!(finished(&log)[0].1, "1");
}

#[tokio::test]
async fn scripts_that_call_tools_run_to_the_step_cap() {
    // A script that calls a tool is doing what scripts are for, whether the
    // script changes or the identical script reads something new each time.
    // Neither is cut short, and the step cap still ends both: every tool
    // step, then the answer-only call.
    let cap = 40;
    let tool_steps = cap as usize;
    for code in [
        (|step| format!("text(await tools.read({{key: 'page {step}'}}));")) as fn(usize) -> String,
        |_| "text(await tools.read({key: 'next'}));".to_owned(),
    ] {
        let (exit, seen, log) = run_scripts(code, cap).await;
        assert_eq!(exit, Exit::Done);
        assert_eq!(seen.len(), tool_steps + 1);
        assert!(seen.iter().all(|(_, recovery)| recovery.is_none()));
        assert!(
            seen[..tool_steps]
                .iter()
                .all(|(remaining, _)| !remaining.answer_only)
        );
        assert!(seen[tool_steps].0.answer_only);
        let scripts: Vec<_> = log
            .events()
            .into_iter()
            .filter_map(|event| match event {
                Event::ToolFinished {
                    call,
                    outcome,
                    output,
                    ..
                } if !call.as_str().contains(":codemode:") => {
                    Some((outcome, format!("{output:?}")))
                }
                _ => None,
            })
            .collect();
        assert_eq!(scripts.len(), tool_steps);
        assert!(scripts.iter().all(|(outcome, output)| {
            *outcome == dex_loop::Outcome::Succeeded
                && !output.contains(dex_loop::IDLE_SCRIPTS_NOTICE)
        }));
    }
}

#[tokio::test]
async fn a_script_that_calls_a_tool_starts_the_count_of_idle_scripts_over() {
    // Idle, working, idle, working, idle: never two idle scripts in a row,
    // so tools stay offered until the step budget ends the turn.
    let (exit, seen, log) = run_scripts(
        |step| {
            if step % 2 == 1 {
                "text(load('page') ?? 'nothing stored');".to_owned()
            } else {
                "text(await tools.read({key: 'next'}));".to_owned()
            }
        },
        5,
    )
    .await;
    assert_eq!(exit, Exit::Done);
    assert_eq!(seen.len(), 6);
    assert!(seen.iter().all(|(_, recovery)| recovery.is_none()));
    assert!(
        seen[..5]
            .iter()
            .all(|(remaining, _)| !remaining.answer_only)
    );
    assert!(
        finished(&log)
            .iter()
            .all(|(_, text)| !text.contains(dex_loop::IDLE_SCRIPTS_NOTICE))
    );
}
