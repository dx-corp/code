//! Scripts compress provider history, while every nested boundary remains
//! authorized, journaled and reconciled through normal Dex ports.
#[allow(dead_code)]
mod support;

use std::time::Duration;

use dex_loop::{
    Budget, CancellationToken, Context, Engine, Event, Exit, Lexicon, Message, Outcome, Output,
    ProposedCall, ThreadId, ToolName, ToolResult, ToolSpec, Tools, Verdict,
};
use serde_json::json;
use support::*;

#[derive(Clone)]
struct JsonTools(FakeTools);

#[derive(Clone)]
struct OutcomeTools(FakeTools, Outcome);
impl Tools for OutcomeTools {
    fn catalog(&self) -> &[ToolSpec] {
        self.0.catalog()
    }
    async fn search(&self, principal: &dex_loop::PrincipalId, query: &str) -> Vec<ToolName> {
        self.0.search(principal, query).await
    }
    async fn policy(&self, ctx: &Context, call: &ProposedCall) -> Verdict {
        self.0.policy(ctx, call).await
    }
    async fn run(
        &self,
        thread: &ThreadId,
        call: &ProposedCall,
        cancel: &CancellationToken,
    ) -> ToolResult {
        let _ = self.0.run(thread, call, cancel).await;
        match self.1 {
            Outcome::Unknown => ToolResult::unknown("owner could not establish the effect outcome"),
            _ => ToolResult::error("owner rejected unchanged input"),
        }
    }
}
impl Tools for JsonTools {
    fn catalog(&self) -> &[ToolSpec] {
        self.0.catalog()
    }
    async fn search(&self, principal: &dex_loop::PrincipalId, query: &str) -> Vec<ToolName> {
        self.0.search(principal, query).await
    }
    async fn policy(&self, ctx: &Context, call: &ProposedCall) -> Verdict {
        self.0.policy(ctx, call).await
    }
    async fn run(
        &self,
        thread: &ThreadId,
        call: &ProposedCall,
        cancel: &CancellationToken,
    ) -> ToolResult {
        let result = self.0.run(thread, call, cancel).await;
        if result.outcome != Outcome::Succeeded {
            return result;
        }
        ToolResult::text(
            json!({"key":call.args["key"],"bulk":"raw detail retained only as owner evidence"})
                .to_string(),
        )
    }
}

fn outer_result(log: &FakeLog, id: &str) -> (Outcome, String) {
    log.events()
        .into_iter()
        .find_map(|event| match event {
            Event::ToolFinished {
                call,
                outcome,
                output: Output::Text(output),
                ..
            } if call.as_str() == id => Some((outcome, output)),
            _ => None,
        })
        .expect("wrapper result")
}

fn script(code: &str) -> Result<dex_loop::ModelChunk, dex_loop::ModelError> {
    call("codemode", json!({"code": code}))
}

#[tokio::test]
async fn parallel_reads_then_serial_effects_project_only_selected_output() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![script(
            r#"
        const reads = await Promise.all([tools.lookup({key:"a"}), tools.lookup({key:"b"})]);
        await tools.update({key:"updated-"+reads[0].key});
        await tools.update({key:"updated-"+reads[1].key});
        text(reads.map(value => value.key));
    "#,
        )],
        vec![text("done")],
    ]);
    let tools = JsonTools(
        FakeTools::new(vec![strict_read_tool("lookup"), write_tool("update")])
            .barrier(&["a", "b"])
            // The owner registry has no synthetic wrapper entry. Only nested
            // calls may reach its tool policy.
            .verdict("codemode", Verdict::Deny("unknown owner tool".into())),
    );
    let effects = FakeEffects::default();
    let engine = Engine::new(
        log.clone(),
        model.clone(),
        tools.clone(),
        effects.clone(),
        Lexicon::default(),
        Budget::default(),
    );
    let mut ctx = log.start_turn("t1", "compose");
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(
        outer_result(&log, "t1-1-0"),
        (Outcome::Succeeded, "[\"a\",\"b\"]".into())
    );
    let runs = tools.0.runs();
    assert_eq!(runs.len(), 4);
    assert!(
        runs[..2]
            .iter()
            .all(|run| matches!(run.call.as_str(), "t1-1-0:codemode:0" | "t1-1-0:codemode:1"))
    );
    assert_eq!(runs[2].call.as_str(), "t1-1-0:codemode:2");
    assert_eq!(runs[3].call.as_str(), "t1-1-0:codemode:3");
    assert!(runs.iter().all(|run| run.thread == thread()));
    assert!(
        tools
            .0
            .policy_checks()
            .iter()
            .all(|(_, principal)| principal == "alice")
    );
    assert_eq!(tools.0.policy_checks().len(), 4);
    for index in 2..4 {
        assert!(
            effects
                .recorded(&dex_loop::CallId::new(format!("t1-1-0:codemode:{index}")))
                .flatten()
                .is_some()
        );
    }
    let messages = &model.seen()[1];
    assert_eq!(
        messages
            .iter()
            .filter(|message| matches!(message, Message::Tool { .. }))
            .count(),
        1
    );
    assert!(!format!("{messages:?}").contains("raw detail"));
    assert_eq!(
        ctx.tool_evidence().len(),
        5,
        "raw nested evidence plus projection"
    );
    assert_eq!(log.rehydrate(), ctx);
    let evidence = ctx.tool_evidence().to_vec();
    let compaction = Event::Compaction {
        covers_to_cursor: ctx.cursor(),
        summary: "summary is not owner evidence".into(),
    };
    let cursor = log.host_append(compaction.clone());
    ctx.observe(cursor, &compaction);
    assert_eq!(ctx.tool_evidence(), evidence);
    assert_eq!(log.rehydrate(), ctx);
    assert!(log.events().iter().any(|event| matches!(event, Event::CodeModeCallsProposed { parent, calls } if parent.as_str() == "t1-1-0" && calls.len() == 2)));
}

#[tokio::test]
async fn denied_invalid_unexposed_and_conversational_calls_do_not_execute() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![script(
            r#"
        const results = await Promise.allSettled([
          tools.lookup({key:4}), tools.denied({}), tools.confirm({})
        ]);
        text(results.map(result => result.status));
        text(ALL_TOOLS.map(tool => tool.name));
        text(typeof tools.hidden);
        text(typeof tools.ask);
        text(typeof tools.client);
        text(typeof tools.codemode);
    "#,
        )],
        vec![text("done")],
    ]);
    let tools = FakeTools::new(vec![
        strict_read_tool("lookup"),
        write_tool("denied"),
        write_tool("confirm"),
        hidden_read_tool("hidden"),
        ask_tool("ask"),
        client_executed_tool("client", true),
    ])
    .verdict("denied", Verdict::Deny("revoked".into()))
    .verdict(
        "confirm",
        Verdict::NeedsConfirmation {
            preview: "owner confirmation".into(),
        },
    );
    let mut ctx = log.start_turn("t1", "compose");
    assert_eq!(
        engine(&log, &model, &tools, Budget::default())
            .run(&mut ctx, &CancellationToken::new())
            .await,
        Ok(Exit::Done)
    );
    assert!(tools.runs().is_empty());
    let (outcome, output) = outer_result(&log, "t1-1-0");
    assert_eq!(outcome, Outcome::Succeeded);
    assert!(output.starts_with("[\"rejected\",\"rejected\",\"rejected\"]"));
    assert!(output.ends_with("undefined\nundefined\nundefined\nundefined"));
    assert_eq!(
        tools.policy_checks().len(),
        2,
        "invalid arguments never reach policy"
    );
    assert_eq!(ctx.tool_evidence().len(), 4);
    assert_eq!(log.rehydrate(), ctx);
}

#[tokio::test]
async fn approval_receipt_and_script_failure_preserve_completed_effect_and_partial_output() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![script(
            "text('partial'); await tools.update({key:'a'}); throw new Error('after effect');",
        )],
        vec![text("done")],
    ]);
    let tools = FakeTools::new(vec![write_tool("update")]).verdict("update", approval("gate"));
    let effects = FakeEffects::default();
    let mut ctx = log.start_turn("t1", "compose");
    assert_eq!(
        engine_with(&log, &model, &tools, &effects, Budget::default())
            .run(&mut ctx, &CancellationToken::new())
            .await,
        Ok(Exit::Done)
    );
    assert_eq!(
        outer_result(&log, "t1-1-0"),
        (
            Outcome::Failed,
            "partial\nScript failed: Error: after effect\n    at <anonymous> (codemode.js:1:77)\nNested calls (completed effects are not undone): update (Ok)".into()
        )
    );
    assert_eq!(tools.runs().len(), 1);
    assert!(log.events().iter().any(|event| matches!(event, Event::AutoApproved { call, args_digest, principal, .. } if call.as_str() == "t1-1-0:codemode:0" && principal.as_str() == dex_loop::AUTO_APPROVER && !args_digest.is_empty())));
    assert!(
        effects
            .recorded(&dex_loop::CallId::new("t1-1-0:codemode:0"))
            .flatten()
            .is_some()
    );
    assert_eq!(log.rehydrate(), ctx);
}

#[tokio::test]
async fn wrapper_recovery_never_reexecutes_nested_effects() {
    let log = FakeLog::default();
    let code = "await tools.update({key:'a'}); text('done');";
    let model = FakeModel::new(vec![vec![script(code)], vec![text("finished")]]);
    let tools = FakeTools::new(vec![write_tool("update")]);
    let effects = FakeEffects::default();
    let mut ctx = log.start_turn("t1", "compose");
    let engine = engine_with(&log, &model, &tools, &effects, Budget::default());
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    let entries = log.entries();
    // Crash before the outer result append; nested effect and ledger survived.
    let end = entries.iter().position(|(_, event)| matches!(event, Event::ToolFinished { call, .. } if call.as_str() == "t1-1-0")).unwrap();
    let recovery = FakeLog::default();
    for (_, event) in &entries[..end] {
        recovery.host_append(event.clone());
    }
    let restarted_model = FakeModel::new(vec![vec![text("recovered")]]);
    let mut replay = recovery.rehydrate();
    assert_eq!(
        engine_with(
            &recovery,
            &restarted_model,
            &tools,
            &effects,
            Budget::default()
        )
        .run(&mut replay, &CancellationToken::new())
        .await,
        Ok(Exit::Done)
    );
    assert_eq!(tools.runs().len(), 1);
    assert_eq!(
        outer_result(&recovery, "t1-1-0"),
        (Outcome::Succeeded, "done".into())
    );
    assert_eq!(recovery.rehydrate(), replay);
}

#[tokio::test]
async fn unknown_mutation_remains_unknown_even_when_script_catches_it_and_retries() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![script(
            "try { await tools.update({key:'slow'}); } catch(e) { text('caught'); }",
        )],
        vec![script("await tools.update({key:'slow'});")],
        vec![text("done")],
    ]);
    // Supply a real owner Unknown immediately: the script must actually catch
    // it and produce output before its own VM deadline expires.
    let tools = OutcomeTools(FakeTools::new(vec![write_tool("update")]), Outcome::Unknown);
    let effects = FakeEffects::default();
    let mut ctx = log.start_turn("t1", "compose");
    let engine = Engine::new(
        log.clone(),
        model.clone(),
        tools.clone(),
        effects.clone(),
        Lexicon::default(),
        Budget::default(),
    );
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(
        outer_result(&log, "t1-1-0"),
        (Outcome::Unknown, "Outcome unknown: a nested tool may have taken effect; completed effects are not undone. Reconcile it before retrying.\ncaught".into())
    );
    assert_eq!(
        tools.0.runs().len(),
        1,
        "the admitted owner operation is never repeated"
    );
    let second = outer_result(&log, "t1-2-0");
    assert_eq!(second.0, Outcome::Failed);
    assert!(second.1.contains("unknown outcome"));
    let nested_claim = effects
        .recorded(&dex_loop::CallId::new("t1-1-0:codemode:0"))
        .flatten()
        .expect("nested effect ledger");
    assert_eq!(nested_claim.outcome, Outcome::Unknown);
    assert_eq!(log.rehydrate(), ctx);
}

#[test]
fn nested_journal_round_trip_rejects_tampered_arguments() {
    let event = Event::CodeModeCallsProposed {
        parent: dex_loop::CallId::new("p"),
        calls: vec![ProposedCall::new(
            dex_loop::CallId::new("p:codemode:0"),
            ToolName::new("update"),
            json!({"key":"a"}),
            alice(),
        )],
    };
    let mut payload = serde_json::to_value(&event).unwrap();
    assert_eq!(
        Event::from_stored_json(&payload, Some("code_mode_calls_proposed")).unwrap(),
        event
    );
    payload["calls"][0]["args"]["key"] = json!("changed");
    assert!(Event::from_stored_json(&payload, Some("code_mode_calls_proposed")).is_err());
}

#[tokio::test]
async fn crash_during_nested_effect_blocks_a_fresh_script_with_the_same_operation() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![script("await tools.update({key:'a'}); text('first');")],
        vec![text("done")],
    ]);
    let tools = FakeTools::new(vec![write_tool("update")]);
    let effects = FakeEffects::default();
    let mut ctx = log.start_turn("t1", "compose");
    assert_eq!(
        engine_with(&log, &model, &tools, &effects, Budget::default())
            .run(&mut ctx, &CancellationToken::new())
            .await,
        Ok(Exit::Done)
    );
    let entries = log.entries();
    let end = entries.iter().position(|(_, event)| matches!(event, Event::ToolFinished { call, .. } if call.as_str() == "t1-1-0:codemode:0")).unwrap();
    let recovery = FakeLog::default();
    for (_, event) in &entries[..end] {
        recovery.host_append(event.clone());
    }
    // Neither ledger can prove what the effect did before the crash.
    let effects = FakeEffects::default()
        .seed(dex_loop::CallId::new("t1-1-0"), None)
        .seed(dex_loop::CallId::new("t1-1-0:codemode:0"), None);
    let fresh = FakeModel::new(vec![
        vec![script("await tools.update({key:'a'}); text('retry');")],
        vec![text("check outcome")],
    ]);
    let mut replay = recovery.rehydrate();
    assert_eq!(
        engine_with(&recovery, &fresh, &tools, &effects, Budget::default())
            .run(&mut replay, &CancellationToken::new())
            .await,
        Ok(Exit::Done)
    );
    assert_eq!(outer_result(&recovery, "t1-1-0").0, Outcome::Unknown);
    assert!(
        outer_result(&recovery, "t1-2-0")
            .1
            .contains("unknown outcome")
    );
    assert_eq!(tools.runs().len(), 1, "only the pre-crash effect ran");
    assert_eq!(recovery.rehydrate(), replay);
}

#[tokio::test]
async fn cancellation_settles_admitted_mutation_and_stops_before_the_next_effect() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![vec![script(
        "await Promise.all([tools.update({key:'a'}),tools.update({key:'b'})]);",
    )]]);
    let cancel = CancellationToken::new();
    let signal = cancel.clone();
    let tools = FakeTools::new(vec![write_tool("update")]).on_run(move |_| signal.cancel());
    let effects = FakeEffects::default();
    let mut ctx = log.start_turn("t1", "compose");
    assert_eq!(
        engine_with(&log, &model, &tools, &effects, Budget::default())
            .run(&mut ctx, &cancel)
            .await,
        Ok(Exit::Interrupted)
    );
    assert_eq!(tools.runs().len(), 1);
    assert!(
        effects
            .recorded(&dex_loop::CallId::new("t1-1-0:codemode:0"))
            .flatten()
            .is_some()
    );
    assert!(
        effects
            .recorded(&dex_loop::CallId::new("t1-1-0:codemode:1"))
            .is_none()
    );
    assert_eq!(log.rehydrate(), ctx);
}

#[tokio::test]
async fn repeated_owner_failures_stop_after_three_across_script_ids() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![script(
            "for (let i=0; i<4; i++) { try { await tools.update({key:'a'}); } catch(e) {} } text('attempts done');",
        )],
        vec![script(
            "try { await tools.update({key:'a'}); } catch(e) { text(e.message); }",
        )],
        vec![text("changed approach")],
    ]);
    let tools = OutcomeTools(FakeTools::new(vec![write_tool("update")]), Outcome::Failed);
    let effects = FakeEffects::default();
    let engine = Engine::new(
        log.clone(),
        model.clone(),
        tools.clone(),
        effects.clone(),
        Lexicon::default(),
        Budget::default(),
    );
    let mut ctx = log.start_turn("t1", "compose");
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(
        tools.0.runs().len(),
        3,
        "the fourth owner call must never execute"
    );
    assert!(
        outer_result(&log, "t1-2-0")
            .1
            .contains("failed three times without progress")
    );
    assert!(
        effects
            .recorded(&dex_loop::CallId::new("t1-2-0:codemode:0"))
            .is_none()
    );
    assert_eq!(log.rehydrate(), ctx);
}

#[tokio::test]
async fn nested_mutation_deadline_records_unknown_after_dispatch() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![script("await tools.update({key:'slow'});")],
        vec![text("check effect")],
    ]);
    let tools = FakeTools::new(vec![write_tool("update")]).delay("slow", Duration::from_secs(5));
    let effects = FakeEffects::default();
    let mut ctx = log.start_turn("t1", "compose");
    // VM startup and schema validation use real native-thread time. The
    // deadline must leave room to admit the child whose timeout is tested.
    assert_eq!(
        engine_with(&log, &model, &tools, &effects, Budget::default())
            .with_tool_call_deadline(Duration::from_secs(1))
            .run(&mut ctx, &CancellationToken::new())
            .await,
        Ok(Exit::Done)
    );
    let nested = effects
        .recorded(&dex_loop::CallId::new("t1-1-0:codemode:0"))
        .flatten()
        .expect("the timeout test must first admit and claim its child mutation");
    assert_eq!(nested.outcome, Outcome::Unknown);
    assert!(log.events().iter().any(
        |event| matches!(event,Event::ToolStarted { call,.. } if call.as_str()=="t1-1-0:codemode:0")
    ));
    assert_eq!(outer_result(&log, "t1-1-0").0, Outcome::Unknown);
    assert_eq!(log.rehydrate(), ctx);
}

#[derive(Clone)]
struct FailedChildReceipt(FakeEffects);

impl dex_loop::Effects for FailedChildReceipt {
    async fn claim(&self, call: &ProposedCall) -> Result<dex_loop::Claim, dex_loop::Fenced> {
        dex_loop::Effects::claim(&self.0, call).await
    }
    async fn record(
        &self,
        call: &dex_loop::CallId,
        result: &ToolResult,
    ) -> Result<(), dex_loop::Fenced> {
        if call.as_str() == "t1-1-0:codemode:0" {
            return Err(dex_loop::Fenced::new(
                "injected child completion write failure",
            ));
        }
        dex_loop::Effects::record(&self.0, call, result).await
    }
}

#[tokio::test]
async fn failed_child_receipt_stops_script_and_recovery_never_repeats_its_effect() {
    let log = FakeLog::default();
    let effects = FakeEffects::default();
    let tools = FakeTools::new(vec![write_tool("update")]).verdict("update", approval("gate"));
    let model = FakeModel::new(vec![vec![script(
        "await tools.update({key:'a'}); await tools.update({key:'b'});",
    )]]);
    let mut ctx = log.start_turn("t1", "compose");
    let failed = Engine::new(
        log.clone(),
        model,
        tools.clone(),
        FailedChildReceipt(effects.clone()),
        Lexicon::default(),
        Budget::default(),
    );
    assert!(
        failed
            .run(&mut ctx, &CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(
        tools.runs().len(),
        1,
        "the next effect never starts after a failed receipt"
    );
    assert_eq!(
        effects.recorded(&dex_loop::CallId::new("t1-1-0:codemode:0")),
        Some(None),
        "the effect ran but its completion was not recorded"
    );
    assert!(!log.events().iter().any(|event| matches!(event, Event::ToolFinished { call, .. } if call.as_str() == "t1-1-0:codemode:0")));
    assert!(log.events().iter().any(|event| matches!(event, Event::AutoApproved { call, .. } if call.as_str() == "t1-1-0:codemode:0")));
    assert_eq!(ctx, log.rehydrate());

    let restarted = FakeModel::new(vec![
        vec![script("await tools.update({key:'a'});")],
        vec![text("check outcome")],
    ]);
    let mut replay = log.rehydrate();
    assert_eq!(
        engine_with(&log, &restarted, &tools, &effects, Budget::default())
            .run(&mut replay, &CancellationToken::new())
            .await,
        Ok(Exit::Done)
    );
    assert_eq!(outer_result(&log, "t1-1-0").0, Outcome::Unknown);
    assert!(outer_result(&log, "t1-2-0").1.contains("unknown outcome"));
    assert_eq!(
        tools.runs().len(),
        1,
        "neither replay nor a fresh script retries the unknown child"
    );
    assert_eq!(replay, log.rehydrate());
}

#[derive(Clone)]
struct GatedReads {
    inner: FakeTools,
    slow_entered: CancellationToken,
    slow_released: CancellationToken,
}
impl GatedReads {
    fn new(inner: FakeTools) -> Self {
        Self {
            inner,
            slow_entered: CancellationToken::new(),
            slow_released: CancellationToken::new(),
        }
    }
}
impl Tools for GatedReads {
    fn catalog(&self) -> &[ToolSpec] {
        self.inner.catalog()
    }
    async fn search(&self, principal: &dex_loop::PrincipalId, query: &str) -> Vec<ToolName> {
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
        if call.args["key"] == "slow" {
            self.slow_entered.cancel();
            tokio::select! {
                () = self.slow_released.cancelled() => {},
                () = cancel.cancelled() => return ToolResult::error("cancelled"),
            }
        } else {
            self.slow_entered.cancelled().await;
            assert!(!self.slow_released.is_cancelled());
        }
        self.inner.run(thread, call, cancel).await
    }
    async fn resolve_codemode_result(
        &self,
        ctx: &Context,
        call: &ProposedCall,
        result: &ToolResult,
        max_bytes: usize,
    ) -> Result<ToolResult, String> {
        self.inner
            .resolve_codemode_result(ctx, call, result, max_bytes)
            .await
    }
}

#[tokio::test]
async fn fastest_read_settles_without_waiting_for_unrelated_read() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![script(
            "text(await Promise.race([tools.lookup({key:'slow'}),tools.lookup({key:'fast'})]));",
        )],
        vec![text("done")],
    ]);
    let tools = GatedReads::new(FakeTools::new(vec![strict_read_tool("lookup")]));
    let mut ctx = log.start_turn("t1", "race");
    let result = Engine::new(
        log.clone(),
        model,
        tools.clone(),
        FakeEffects::default(),
        Lexicon::default(),
        Budget::default(),
    )
    .run(&mut ctx, &CancellationToken::new())
    .await;
    assert_eq!(result, Ok(Exit::Done));
    assert!(tools.slow_entered.is_cancelled());
    assert!(
        !tools.slow_released.is_cancelled(),
        "the unrelated read cannot finish before the script"
    );
    assert_eq!(outer_result(&log, "t1-1-0").0, Outcome::Succeeded);
    assert_eq!(log.rehydrate(), ctx);
}

#[tokio::test]
async fn dependent_read_starts_before_an_unrelated_read_finishes() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![script(
            "const slow=tools.lookup({key:'slow'}); await tools.lookup({key:'fast'}); await tools.lookup({key:'dependent'}); text('pipeline');",
        )],
        vec![text("done")],
    ]);
    let dependent = Arc::new(AtomicBool::new(false));
    let signal = dependent.clone();
    let tools = GatedReads::new(FakeTools::new(vec![strict_read_tool("lookup")]).on_run(
        move |call| {
            if call.args["key"] == "dependent" {
                signal.store(true, Ordering::SeqCst);
            }
        },
    ));
    let mut ctx = log.start_turn("t1", "pipeline");
    let result = Engine::new(
        log.clone(),
        model,
        tools.clone(),
        FakeEffects::default(),
        Lexicon::default(),
        Budget::default(),
    )
    .run(&mut ctx, &CancellationToken::new())
    .await;
    assert_eq!(result, Ok(Exit::Done));
    assert!(tools.slow_entered.is_cancelled());
    assert!(
        !tools.slow_released.is_cancelled(),
        "the unrelated read cannot finish before the script"
    );
    assert!(dependent.load(Ordering::SeqCst));
    assert_eq!(
        outer_result(&log, "t1-1-0"),
        (Outcome::Succeeded, "pipeline".into())
    );
    assert_eq!(log.rehydrate(), ctx);
}
