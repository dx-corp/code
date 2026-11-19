//! Production Dex turns replayed against the real engine with a scripted
//! model, so a prompt or loop change cannot bring a failure back unnoticed.
//!
//! Each case names the production turn shape it encodes and asserts the
//! turn's step count, tool calls, exit and the key text of the results, never
//! exact prose. The model is a function of what the engine shows it, so a
//! case states how a model that ignores every hint is still bounded. No
//! provider is contacted and no model output is recorded: a case is Rust, not
//! a stored transcript.
//!
//! A case that needs a fix not yet on `main` asserts the behavior expected
//! after it and is `#[ignore]`d with what it waits for. Remove the attribute
//! when that change merges.

use crate::support;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use dex_loop::{
    Budget, CODEMODE, CancellationToken, Context, Engine, Event, Exit, Lexicon, Model, ModelChunk,
    ModelError, Outcome, Output, PrincipalId, ProposedCall, TOOLS_CALL, TOOLS_SEARCH, ThreadId,
    ToolName, ToolResult, ToolSpec, ToolWrapper, Tools, Verdict, WrapperRoute, model_tool_name,
};
use futures_util::{Stream, stream};
use serde_json::{Value, json};
use support::{FakeEffects, FakeLog, FakeTools, hidden_read_tool, read_tool, write_tool};

/// The step cap the production turns ran under (dex-runtime, 2026-11-18).
const PROD_STEP_CAP: u32 = 40;
/// Model calls a turn takes when one identical call stalls: three calls that
/// run, the one the guard refuses, and the answer-only call after it.
const STALLED_TURN_MODEL_CALLS: usize = 5;
/// "Within a few steps": the most model calls a turn may take once a guard
/// that is not on `main` yet ends it. Far under `PROD_STEP_CAP`.
const A_FEW_MODEL_CALLS: usize = 8;

const BAG_VENTURES: &str =
    "I am meeting with BAG Ventures to discuss you in a bit. What should I focus on?";

// ------------------------------------------------------------------ harness

/// What the engine shows the model before one model call.
struct Seen {
    /// This model call's number in the turn, from 1.
    call: usize,
    /// No tools are offered: the model must answer from what it has.
    answer_only: bool,
    /// The stall guard's guidance for this attempt, when it has any.
    recovery: Option<String>,
}

/// What the scripted model does on one call that may use tools.
type Plan = fn(&Seen) -> Vec<ModelChunk>;

/// A model that follows `plan` on every call that offers tools and writes
/// `ANSWER` on the answer-only call. It reads nothing else, so a plan that
/// ignores `Seen` is a model that ignores every hint it is given.
#[derive(Clone)]
struct ScriptedModel {
    plan: Plan,
    seen: Arc<Mutex<Vec<Seen>>>,
}

const ANSWER: &str = "Here is what I could establish.";

impl Model for ScriptedModel {
    fn stream<'a>(
        &'a self,
        ctx: &'a Context,
        _: &'a [&'a ToolSpec],
    ) -> impl Stream<Item = Result<ModelChunk, ModelError>> + Send + 'a {
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Seen {
            call: seen.len() + 1,
            answer_only: ctx
                .remaining_budget()
                .is_some_and(|remaining| remaining.answer_only),
            recovery: ctx.recovery_guidance(),
        };
        let chunks = if now.answer_only {
            vec![ModelChunk::Text(ANSWER.into())]
        } else {
            (self.plan)(&now)
        };
        seen.push(now);
        stream::iter(chunks.into_iter().map(Ok))
    }
}

fn answer(text: &str) -> Vec<ModelChunk> {
    vec![ModelChunk::Text(text.into())]
}

fn tool_call(name: &str, args: Value) -> ModelChunk {
    ModelChunk::ToolCall {
        name: ToolName::new(name),
        args,
    }
}

/// One `run` script, as the model emits it.
fn script(code: &str) -> ModelChunk {
    tool_call(CODEMODE, json!({"code": code}))
}

/// The host's tools: `FakeTools` for catalog, search, policy and the record
/// of what ran, with JSON results a script can read and owner failures for
/// the tools named in `failing`.
#[derive(Clone)]
struct ReplayTools {
    inner: FakeTools,
    failing: Arc<HashMap<&'static str, &'static str>>,
}

impl Tools for ReplayTools {
    fn catalog(&self) -> &[ToolSpec] {
        self.inner.catalog()
    }
    fn wrappers(&self) -> &[ToolWrapper] {
        Tools::wrappers(&self.inner)
    }
    async fn search(&self, principal: &PrincipalId, query: &str) -> Vec<ToolName> {
        self.inner.search(principal, query).await
    }
    async fn policy(&self, ctx: &Context, call: &ProposedCall) -> Verdict {
        self.inner.policy(ctx, call).await
    }
    async fn run(
        &self,
        thread: &ThreadId,
        call: &ProposedCall,
        cancel: &CancellationToken,
    ) -> ToolResult {
        let result = self.inner.run(thread, call, cancel).await;
        if result.outcome != Outcome::Succeeded {
            return result;
        }
        match self.failing.get(call.tool.as_str()) {
            Some(reason) => ToolResult::error(*reason),
            None => ToolResult::text(json!({"tool": call.tool, "args": call.args}).to_string()),
        }
    }
}

/// One replayed turn: what to ask, what the host offers and what the model
/// does.
struct Replay {
    ask: &'static str,
    tools: FakeTools,
    failing: &'static [(&'static str, &'static str)],
    plan: Plan,
    max_steps: u32,
}

impl Replay {
    fn new(ask: &'static str, tools: FakeTools, plan: Plan) -> Self {
        Self {
            ask,
            tools,
            failing: &[],
            plan,
            max_steps: PROD_STEP_CAP,
        }
    }

    /// Tools whose owner fails every call with the given reason.
    fn failing(mut self, failing: &'static [(&'static str, &'static str)]) -> Self {
        self.failing = failing;
        self
    }

    async fn run(self) -> Turn {
        let log = FakeLog::default();
        let model = ScriptedModel {
            plan: self.plan,
            seen: Arc::default(),
        };
        let engine = Engine::new(
            log.clone(),
            model.clone(),
            ReplayTools {
                inner: self.tools.clone(),
                failing: Arc::new(self.failing.iter().copied().collect()),
            },
            FakeEffects::default(),
            Lexicon::default(),
            Budget {
                max_steps: self.max_steps,
                wall: Duration::from_secs(60),
                ..Budget::default()
            },
        );
        let mut ctx = log.start_turn("t1", self.ask);
        let exit = engine
            .run(&mut ctx, &CancellationToken::new())
            .await
            .expect("the log is never fenced in a replay");
        assert_eq!(ctx, log.rehydrate(), "a restart rebuilds the same turn");
        let seen = std::mem::take(&mut *model.seen.lock().unwrap_or_else(PoisonError::into_inner));
        Turn {
            exit,
            seen,
            log,
            tools: self.tools,
        }
    }
}

/// One finished call as the model was shown it.
#[derive(Debug)]
struct Finished {
    id: String,
    tool: String,
    outcome: Outcome,
    text: String,
}

impl Finished {
    /// A call a `run` script made, not one the model made itself.
    fn nested(&self) -> bool {
        self.id.contains(":codemode:")
    }
}

/// What a replayed turn did.
struct Turn {
    exit: Exit,
    seen: Vec<Seen>,
    log: FakeLog,
    tools: FakeTools,
}

impl Turn {
    /// Model calls the turn made, the answer-only one included.
    fn model_calls(&self) -> usize {
        self.seen.len()
    }

    /// Steps the engine opened. One per model call in these replays: no
    /// attempt is retried.
    fn steps(&self) -> usize {
        self.log
            .events()
            .iter()
            .filter(|event| matches!(event, Event::StepStarted { .. }))
            .count()
    }

    /// Every finished call in log order, the model's own and a script's.
    fn finished(&self) -> Vec<Finished> {
        let events = self.log.events();
        let tools: HashMap<String, String> = events
            .iter()
            .flat_map(|event| match event {
                Event::ModelStepCompleted { calls, .. }
                | Event::CodeModeCallsProposed { calls, .. } => calls.as_slice(),
                _ => &[],
            })
            .map(|call| (call.id.to_string(), call.tool.to_string()))
            .collect();
        events
            .iter()
            .filter_map(|event| match event {
                Event::ToolFinished {
                    call,
                    outcome,
                    output,
                    ..
                } => Some(Finished {
                    id: call.to_string(),
                    tool: tools.get(call.as_str()).cloned().unwrap_or_default(),
                    outcome: *outcome,
                    text: match output {
                        Output::Text(text) => text.clone(),
                        Output::Ref(reference) => reference.to_string(),
                        Output::Blocks(_) => String::new(),
                    },
                }),
                _ => None,
            })
            .collect()
    }

    /// The calls the model made itself, in order.
    fn model_tool_calls(&self) -> Vec<Finished> {
        self.finished()
            .into_iter()
            .filter(|call| !call.nested())
            .collect()
    }

    /// Calls that reached a tool's owner, from the model or from a script.
    fn owner_runs(&self) -> usize {
        self.tools.runs().len()
    }

    /// The turn's final answer.
    fn answer(&self) -> String {
        self.log
            .events()
            .into_iter()
            .rev()
            .find_map(|event| match event {
                Event::Final { text, .. } => Some(text),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// The turn ended with the answer-only call, having been told why.
    fn assert_stalled_then_answered(&self, guidance: &str, refusal: &str) {
        assert_eq!(self.exit, Exit::Done);
        assert_eq!(self.model_calls(), STALLED_TURN_MODEL_CALLS);
        assert_eq!(self.steps(), STALLED_TURN_MODEL_CALLS);
        let (last, before) = self.seen.split_last().expect("the turn called the model");
        assert!(last.answer_only, "the turn ends on the answer-only call");
        assert!(before.iter().all(|seen| !seen.answer_only));
        let guided: Vec<usize> = self
            .seen
            .iter()
            .filter(|seen| {
                seen.recovery
                    .as_ref()
                    .is_some_and(|text| text.contains(guidance))
            })
            .map(|seen| seen.call)
            .collect();
        assert_eq!(
            guided.first(),
            Some(&(STALLED_TURN_MODEL_CALLS - 1)),
            "guidance arrives with the call after the third repeat: {guided:?}"
        );
        let calls = self.model_tool_calls();
        let (refused, ran) = calls.split_last().expect("the model called a tool");
        assert_eq!(calls.len(), STALLED_TURN_MODEL_CALLS - 1);
        assert_eq!(refused.outcome, Outcome::Failed);
        assert!(refused.text.contains(refusal), "{}", refused.text);
        assert!(
            ran.iter()
                .all(|call| call.outcome == ran[0].outcome && call.text == ran[0].text),
            "the calls before the refusal all returned the same thing: {ran:?}"
        );
        assert_eq!(self.answer(), ANSWER);
    }
}

/// Dex declares `search` in place of `tools.search`
/// (`dex_tools::wrappers::SEARCH`): the same route, so texts that name the
/// tool the model finds tools with name `search`.
fn dex_search_wrapper() -> ToolWrapper {
    ToolWrapper {
        spec: read_tool("search"),
        routes: vec![WrapperRoute {
            field: "query".into(),
            value: None,
            tool: ToolName::new(TOOLS_SEARCH),
        }],
    }
}

/// A catalog with one core read and one read only a search exposes, as an
/// organization with a CRM connected.
fn catalog() -> FakeTools {
    FakeTools::new(vec![read_tool("lookup"), hidden_read_tool("crm.lookup")])
}

// ------------------------------------------------------- 1. BAG Ventures

// Production 2026-11-18, turn operating_turn_b8642fdf: asked what to focus on
// for an investor meeting, Dex made 39 `run` calls that only probed the
// script catalog helpers, called no tool from any of them, reached the
// 40-step cap and gave a generic answer.

/// The probe calls no owner: after two idle scripts, the next model call
/// answers with no tools, before the identical-call guard needs to refuse one.
#[tokio::test]
async fn bag_ventures_repeated_catalog_probe_stalls_and_answers() {
    let turn = Replay::new(BAG_VENTURES, catalog(), |_| {
        vec![script("text(searchTools('investor crm'));")]
    })
    .run()
    .await;

    assert_eq!(turn.exit, Exit::Done);
    assert_eq!(turn.model_calls(), 3);
    assert_eq!(turn.steps(), 3);
    let (answer, scripts) = turn.seen.split_last().expect("the turn called the model");
    assert!(answer.answer_only);
    assert!(scripts.iter().all(|seen| !seen.answer_only));
    assert!(
        answer
            .recovery
            .as_ref()
            .is_some_and(|text| text.contains("The last 2 scripts called no tool"))
    );
    let calls = turn.model_tool_calls();
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|call| call.outcome == Outcome::Succeeded));
    assert!(!calls[0].text.is_empty());
    assert!(!calls[0].text.contains(dex_loop::IDLE_SCRIPTS_NOTICE));
    assert!(calls[1].text.ends_with(dex_loop::IDLE_SCRIPTS_NOTICE));
    assert!(calls[1].text.len() > dex_loop::IDLE_SCRIPTS_NOTICE.len());
    assert_eq!(turn.answer(), ANSWER);
    assert!(turn.model_calls() < PROD_STEP_CAP as usize);
    assert_eq!(turn.owner_runs(), 0, "no script called a tool");
    assert!(
        turn.model_tool_calls()
            .iter()
            .all(|call| call.tool == CODEMODE)
    );
}

/// The probe reworded every step, as production's was: each script is a new
/// call with a new result, so the identical-call guard never counts it. A
/// run of scripts that call nothing must end the turn all the same.
#[tokio::test]
#[ignore = "awaiting the zero-nested-call guard: scripts that differ but call no tool still run to the step cap"]
async fn bag_ventures_reworded_catalog_probes_end_within_a_few_steps() {
    let turn = Replay::new(BAG_VENTURES, catalog(), |seen| {
        vec![script(&format!(
            "text(searchTools('investor crm {}'));",
            seen.call
        ))]
    })
    .run()
    .await;

    assert_eq!(turn.exit, Exit::Done);
    assert!(
        turn.model_calls() <= A_FEW_MODEL_CALLS,
        "{} model calls",
        turn.model_calls()
    );
    assert_eq!(turn.owner_runs(), 0, "no script called a tool");
    assert!(
        turn.seen.last().is_some_and(|seen| seen.answer_only),
        "the turn ends on the answer-only call"
    );
    assert_eq!(turn.answer(), ANSWER);
}

/// The other acceptable turn: the model answers from what it already knows
/// and calls nothing.
#[tokio::test]
async fn bag_ventures_answered_directly_uses_no_tools() {
    let turn = Replay::new(BAG_VENTURES, catalog(), |_| {
        answer("Lead with what Deixic verifies for a customer.")
    })
    .run()
    .await;

    assert_eq!(turn.exit, Exit::Done);
    assert_eq!(turn.model_calls(), 1);
    assert_eq!(turn.steps(), 1);
    assert!(turn.finished().is_empty());
    assert_eq!(turn.owner_runs(), 0);
    assert!(turn.answer().contains("Deixic"));
}

// ------------------------------------------- 2. A script that emits nothing

/// A script that selects no output used to return an empty success, which
/// read as "nothing found". It says no output was selected and stays a
/// success, and the model's next call sees exactly that.
#[tokio::test]
async fn a_script_that_emits_nothing_says_no_output_was_selected() {
    let turn = Replay::new(BAG_VENTURES, catalog(), |seen| match seen.call {
        1 => vec![script("const found = searchTools('lookup');")],
        _ => answer("done"),
    })
    .run()
    .await;

    assert_eq!(turn.exit, Exit::Done);
    assert_eq!(turn.model_calls(), 2);
    let calls = turn.model_tool_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].tool, CODEMODE);
    assert_eq!(calls[0].outcome, Outcome::Succeeded);
    assert_eq!(calls[0].text, agent_codemode::NO_OUTPUT);
    assert_eq!(turn.owner_runs(), 0);
}

// ------------------------------- 3. An in-script search that matches nothing

/// `searchTools` inspects only the tools admitted to the script. A search
/// that matches nothing says so and names the tool that searches the whole
/// catalog: `tools_search`, or `search` where the host declares that in its
/// place, as Dex does.
#[tokio::test]
async fn an_in_script_search_with_no_match_names_the_direct_search() {
    for (tools, searcher) in [
        (catalog(), model_tool_name(TOOLS_SEARCH)),
        (
            catalog().wrappers(vec![dex_search_wrapper()]),
            "search".to_owned(),
        ),
    ] {
        let turn = Replay::new(BAG_VENTURES, tools, |seen| match seen.call {
            1 => vec![script("text(searchTools('investor crm'));")],
            _ => answer("done"),
        })
        .run()
        .await;

        assert_eq!(turn.exit, Exit::Done);
        assert_eq!(turn.model_calls(), 2);
        let calls = turn.model_tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].outcome, Outcome::Succeeded);
        let text = &calls[0].text;
        assert!(text.starts_with("[]"), "{text}");
        assert!(text.contains("found nothing"), "{text}");
        assert!(text.contains("only the tools admitted"), "{text}");
        assert!(
            text.contains(&format!("call {searcher} directly")),
            "names {searcher}: {text}"
        );
        assert_eq!(turn.owner_runs(), 0);
    }
}

/// A search that matches adds no note: the note is for the empty result.
#[tokio::test]
async fn an_in_script_search_that_matches_adds_no_note() {
    let turn = Replay::new(BAG_VENTURES, catalog(), |seen| match seen.call {
        1 => vec![script("text(searchTools('lookup').length);")],
        _ => answer("done"),
    })
    .run()
    .await;

    let calls = turn.model_tool_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].outcome, Outcome::Succeeded);
    assert!(
        !calls[0].text.contains("found nothing"),
        "{}",
        calls[0].text
    );
}

// --------------------------------------- 4. `tools.call` as a top-level call

// Production, week to 2026-11-18: all 312 top-level `tools.call` calls were
// refused. The model is steered away from them by the tool descriptions,
// which a scripted model cannot show. What these cases hold is the loop's
// half: a refused `tools.call` costs one step, says what to correct, reaches
// no owner, and a model that repeats it is stopped by the stall guard.

/// A script helper named as a tool, a script handed over as the arguments,
/// and a catalog tool no search has exposed.
const REFUSED_TOOLS_CALLS: [fn() -> Value; 3] = [
    || json!({"name": "searchTools", "arguments": {"query": "investor crm"}}),
    || json!({"code": "text(await tools.lookup({key: 'bag'}))"}),
    || json!({"name": "crm_lookup", "arguments": {"key": "bag"}}),
];

/// Each refusal costs one step: the model reads why, makes the direct call
/// and answers.
#[tokio::test]
async fn a_refused_top_level_tools_call_costs_one_step() {
    const PLANS: [Plan; 3] = [
        |seen| corrected_after(seen, REFUSED_TOOLS_CALLS[0]()),
        |seen| corrected_after(seen, REFUSED_TOOLS_CALLS[1]()),
        |seen| corrected_after(seen, REFUSED_TOOLS_CALLS[2]()),
    ];
    fn corrected_after(seen: &Seen, refused: Value) -> Vec<ModelChunk> {
        match seen.call {
            1 => vec![tool_call(TOOLS_CALL, refused)],
            2 => vec![tool_call("lookup", json!({"key": "bag"}))],
            _ => answer("done"),
        }
    }
    for plan in PLANS {
        let turn = Replay::new(BAG_VENTURES, catalog(), plan).run().await;

        assert_eq!(turn.exit, Exit::Done);
        assert_eq!(turn.model_calls(), 3);
        assert_eq!(turn.steps(), 3);
        let calls = turn.model_tool_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].outcome, Outcome::Failed);
        assert!(
            calls[0].text.contains(&model_tool_name(TOOLS_SEARCH))
                || calls[0].text.contains("required"),
            "the refusal says what to correct: {}",
            calls[0].text
        );
        assert_eq!(calls[1].tool, "lookup");
        assert_eq!(calls[1].outcome, Outcome::Succeeded);
        assert_eq!(turn.owner_runs(), 1, "only the corrected call ran");
        assert!(
            turn.seen.iter().all(|seen| seen.recovery.is_none()),
            "one refusal is not a stall"
        );
    }
}

/// A model that repeats the refused call is stopped after three, not at the
/// step cap, and no owner ever runs.
#[tokio::test]
async fn a_repeated_refused_tools_call_stalls_instead_of_spending_the_turn() {
    const PLANS: [Plan; 3] = [
        |_| vec![tool_call(TOOLS_CALL, REFUSED_TOOLS_CALLS[0]())],
        |_| vec![tool_call(TOOLS_CALL, REFUSED_TOOLS_CALLS[1]())],
        |_| vec![tool_call(TOOLS_CALL, REFUSED_TOOLS_CALLS[2]())],
    ];
    for plan in PLANS {
        let turn = Replay::new(BAG_VENTURES, catalog(), plan).run().await;

        turn.assert_stalled_then_answered(
            "failed 3 consecutive times",
            "identical call failed three times",
        );
        assert_eq!(turn.owner_runs(), 0);
    }
}

// ------------------------------------- 5. The same failed call, repeated

/// The owner fails the same read every time. Three run, the fourth is
/// refused without running, and the turn answers with what it has.
#[tokio::test]
async fn repeated_identical_failed_calls_stall_and_answer() {
    let turn = Replay::new(BAG_VENTURES, catalog(), |_| {
        vec![tool_call("lookup", json!({"key": "bag"}))]
    })
    .failing(&[("lookup", "the CRM connection is not authorized")])
    .run()
    .await;

    turn.assert_stalled_then_answered(
        "failed 3 consecutive times",
        "identical call failed three times",
    );
    let calls = turn.model_tool_calls();
    assert!(calls.iter().all(|call| call.tool == "lookup"));
    assert!(
        calls[0].text.contains("not authorized"),
        "{}",
        calls[0].text
    );
    assert_eq!(
        turn.owner_runs(),
        3,
        "the refused call never reached the owner"
    );
}

/// A different call between the failures is progress: the guard starts over
/// and the turn is not cut short.
#[tokio::test]
async fn a_different_call_between_failures_is_not_a_stall() {
    let turn = Replay::new(BAG_VENTURES, catalog(), |seen| match seen.call {
        3 => vec![tool_call("lookup", json!({"key": "other"}))],
        1..=5 => vec![tool_call("lookup", json!({"key": "bag"}))],
        _ => answer("done"),
    })
    .failing(&[("lookup", "the CRM connection is not authorized")])
    .run()
    .await;

    assert_eq!(turn.exit, Exit::Done);
    assert_eq!(turn.model_calls(), 6);
    assert_eq!(turn.owner_runs(), 5, "every call ran");
    assert!(turn.seen.iter().all(|seen| seen.recovery.is_none()));
    assert!(turn.seen.iter().all(|seen| !seen.answer_only));
}

// ------------------------------------------------------------ 6. Batching

const INDEPENDENT_READS: [&str; 3] = ["funding", "investors", "pipeline"];

fn reads() -> FakeTools {
    FakeTools::new(vec![support::strict_read_tool("lookup")])
}

/// The reads one per step: the baseline the batched turns are measured
/// against.
async fn reads_one_per_step() -> Turn {
    Replay::new(BAG_VENTURES, reads(), |seen| {
        match INDEPENDENT_READS.get(seen.call - 1) {
            Some(key) => vec![tool_call("lookup", json!({"key": key}))],
            None => answer("done"),
        }
    })
    .run()
    .await
}

/// Every read ran, succeeded and overlapped the others. The barrier makes a
/// read wait for its peers, so serial dispatch fails them instead.
fn assert_one_parallel_wave(turn: &Turn, reads: &[&Finished]) {
    assert_eq!(reads.len(), INDEPENDENT_READS.len());
    assert!(
        reads.iter().all(|read| read.outcome == Outcome::Succeeded),
        "{reads:?}"
    );
    let runs = turn.tools.runs();
    assert_eq!(runs.len(), INDEPENDENT_READS.len());
    let last_start = runs.iter().map(|run| run.started).max();
    let first_finish = runs.iter().map(|run| run.finished).min();
    assert!(
        last_start <= first_finish,
        "every read started before any finished"
    );
}

/// Several independent reads in one step run as one parallel wave, and the
/// turn takes two model calls where one read per step takes four.
#[tokio::test]
async fn independent_reads_in_one_step_run_as_one_wave_in_fewer_steps() {
    let serial = reads_one_per_step().await;
    assert_eq!(serial.exit, Exit::Done);
    assert_eq!(serial.model_calls(), INDEPENDENT_READS.len() + 1);

    let batched = Replay::new(
        BAG_VENTURES,
        reads().barrier(&INDEPENDENT_READS),
        |seen| match seen.call {
            1 => INDEPENDENT_READS
                .iter()
                .map(|key| tool_call("lookup", json!({"key": key})))
                .collect(),
            _ => answer("done"),
        },
    )
    .run()
    .await;

    assert_eq!(batched.exit, Exit::Done);
    assert_eq!(batched.model_calls(), 2);
    assert_eq!(batched.steps(), 2);
    assert!(batched.model_calls() < serial.model_calls());
    let calls = batched.model_tool_calls();
    assert!(calls.iter().all(|call| call.tool == "lookup"));
    assert_one_parallel_wave(&batched, &calls.iter().collect::<Vec<_>>());
    assert_eq!(batched.owner_runs(), serial.owner_runs(), "the same reads");
}

/// The same reads from one `run` script: one model step, one parallel wave,
/// and one result holding what the script selected.
#[tokio::test]
async fn independent_reads_in_one_script_run_as_one_wave_in_fewer_steps() {
    let serial = reads_one_per_step().await;

    let batched = Replay::new(
        BAG_VENTURES,
        reads().barrier(&INDEPENDENT_READS),
        |seen| match seen.call {
            1 => vec![script(
                r#"
                const found = await Promise.all(
                    ["funding", "investors", "pipeline"].map(key => tools.lookup({key}))
                );
                text(found.map(read => read.args.key));
            "#,
            )],
            _ => answer("done"),
        },
    )
    .run()
    .await;

    assert_eq!(batched.exit, Exit::Done);
    assert_eq!(batched.model_calls(), 2);
    assert!(batched.model_calls() < serial.model_calls());
    let finished = batched.finished();
    let nested: Vec<&Finished> = finished.iter().filter(|call| call.nested()).collect();
    assert!(nested.iter().all(|call| call.tool == "lookup"));
    assert_one_parallel_wave(&batched, &nested);
    let calls = batched.model_tool_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].tool, CODEMODE);
    assert_eq!(calls[0].outcome, Outcome::Succeeded);
    for key in INDEPENDENT_READS {
        assert!(calls[0].text.contains(key), "{}", calls[0].text);
    }
}

// --------------------------- 7. Business-object writes that need a person

/// Several business-object writes in one step that each need a person's
/// approval come back today as that many failures, which the model retries
/// or reports as errors. Expected once the feature lands: one result asking
/// the person to confirm the changes together, and no failure.
///
/// The owner's refusal here stands in for the platform's `HUMAN_APPROVAL`
/// governance answer. If the feature lands in the host rather than the loop,
/// keep the assertions and move the stand-in to what the host then returns.
#[tokio::test]
#[ignore = "awaiting the batched HUMAN_APPROVAL confirmation for business-object writes (not on main)"]
async fn business_object_writes_needing_approval_return_one_confirmation() {
    const CHANGES: [&str; 3] = ["vendor-1", "vendor-2", "vendor-3"];
    let turn = Replay::new(
        "Mark these three vendors as reviewed.",
        FakeTools::new(vec![write_tool("records.update")]),
        |seen| match seen.call {
            1 => CHANGES
                .iter()
                .map(|record| {
                    tool_call(
                        "records.update",
                        json!({"key": record, "status": "reviewed"}),
                    )
                })
                .collect(),
            _ => answer("I have asked you to confirm the three changes."),
        },
    )
    .failing(&[(
        "records.update",
        "HUMAN_APPROVAL: this change needs a person's approval",
    )])
    .run()
    .await;

    assert_eq!(turn.exit, Exit::Done);
    assert_eq!(turn.model_calls(), 2);
    let calls = turn.model_tool_calls();
    assert!(
        calls.iter().all(|call| call.outcome != Outcome::Failed),
        "a change waiting for a person is not a failure: {calls:?}"
    );
    let confirmations: Vec<&Finished> = calls
        .iter()
        .filter(|call| call.text.to_lowercase().contains("confirm"))
        .collect();
    assert_eq!(confirmations.len(), 1, "one request to confirm: {calls:?}");
    assert!(
        confirmations[0]
            .text
            .contains(&format!("{} changes", CHANGES.len())),
        "{}",
        confirmations[0].text
    );
    assert!(turn.seen.iter().all(|seen| seen.recovery.is_none()));
}
