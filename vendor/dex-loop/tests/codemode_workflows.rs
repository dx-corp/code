//! Script scratch data survives successful wrappers through the thread journal,
//! while failed or uncertain scripts cannot publish replacement state.
#[allow(dead_code)]
mod support;

use dex_loop::{
    Budget, CancellationToken, Context, Event, Exit, Outcome, Output, PrincipalId, ProposedCall,
    ThreadId, ToolName, ToolResult, ToolSpec, Tools, TurnId, Verdict,
};
use serde_json::json;
use support::*;

fn script(code: &str) -> Result<dex_loop::ModelChunk, dex_loop::ModelError> {
    call("codemode", json!({"code": code}))
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

#[tokio::test]
async fn successful_pagination_state_survives_the_next_script_and_rehydrate() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![script(
            "await store('page', {next:'page-2', selected:['invoice-1']}); text('saved');",
        )],
        vec![script(
            "const page = await load('page'); text(page.next); text(page.selected);",
        )],
        vec![text("done")],
    ]);
    let tools = FakeTools::new(vec![]);
    let mut ctx = log.start_turn("t1", "paginate");
    assert_eq!(
        engine(&log, &model, &tools, Budget::default())
            .run(&mut ctx, &CancellationToken::new())
            .await,
        Ok(Exit::Done)
    );
    assert_eq!(
        outer_result(&log, "t1-1-0"),
        (Outcome::Succeeded, "saved".into())
    );
    assert_eq!(
        outer_result(&log, "t1-2-0"),
        (Outcome::Succeeded, "page-2\n[\"invoice-1\"]".into())
    );
    assert_eq!(log.rehydrate(), ctx);
    let history = model.seen();
    assert!(
        !history[1].iter().any(|message| matches!(message, dex_loop::Message::Tool { output, .. } if format!("{output:?}").contains("invoice-1"))),
        "scratch state must stay outside provider history"
    );
}

#[tokio::test]
async fn failed_script_keeps_the_last_successful_store_value() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![script("await store('page', 'accepted'); text('saved');")],
        vec![script(
            "await store('page', 'rejected'); throw new Error('cannot finish');",
        )],
        vec![script("text(await load('page'));")],
        vec![text("done")],
    ]);
    let tools = FakeTools::new(vec![]);
    let mut ctx = log.start_turn("t1", "paginate");
    assert_eq!(
        engine(&log, &model, &tools, Budget::default())
            .run(&mut ctx, &CancellationToken::new())
            .await,
        Ok(Exit::Done)
    );
    assert_eq!(outer_result(&log, "t1-1-0").0, Outcome::Succeeded);
    assert_eq!(outer_result(&log, "t1-2-0").0, Outcome::Failed);
    assert_eq!(
        outer_result(&log, "t1-3-0"),
        (Outcome::Succeeded, "accepted".into())
    );
    assert_eq!(log.rehydrate(), ctx);
}

#[tokio::test]
async fn recorded_success_recovers_prepared_store_without_replaying_its_effect() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![script(
            "await tools.update({key:'effect'}); await store('page', 'page-2'); text('saved');",
        )],
        vec![text("done")],
    ]);
    let tools = FakeTools::new(vec![write_tool("update")]);
    let effects = FakeEffects::default();
    let mut ctx = log.start_turn("t1", "paginate");
    assert_eq!(
        engine_with(&log, &model, &tools, &effects, Budget::default())
            .run(&mut ctx, &CancellationToken::new())
            .await,
        Ok(Exit::Done)
    );
    assert_eq!(outer_result(&log, "t1-1-0").0, Outcome::Succeeded);
    let entries = log.entries();
    let finish = entries.iter().position(|(_, event)| matches!(event, Event::ToolFinished { call, .. } if call.as_str() == "t1-1-0")).unwrap();
    let recovery = FakeLog::default();
    for (_, event) in &entries[..finish] {
        recovery.host_append(event.clone());
    }
    let restarted = FakeModel::new(vec![
        vec![script("text(await load('page'));")],
        vec![text("recovered")],
    ]);
    let mut replay = recovery.rehydrate();
    assert_eq!(
        engine_with(&recovery, &restarted, &tools, &effects, Budget::default())
            .run(&mut replay, &CancellationToken::new())
            .await,
        Ok(Exit::Done)
    );
    assert_eq!(
        tools.runs().len(),
        1,
        "the accepted mutation must never repeat"
    );
    assert_eq!(
        outer_result(&recovery, "t1-2-0"),
        (Outcome::Succeeded, "page-2".into())
    );
    assert_eq!(recovery.rehydrate(), replay);
}

#[tokio::test]
async fn scratch_state_is_partitioned_by_the_accepted_principal() {
    let log = FakeLog::default();
    let tools = FakeTools::new(vec![]);
    let mut ctx = log.start_turn("alice-1", "save cursor");
    let first = FakeModel::new(vec![
        vec![script(
            "await store('page', 'alice-cursor'); text('saved');",
        )],
        vec![text("done")],
    ]);
    assert_eq!(
        engine(&log, &first, &tools, Budget::default())
            .run(&mut ctx, &CancellationToken::new())
            .await,
        Ok(Exit::Done)
    );
    assert_eq!(outer_result(&log, "alice-1-1-0").0, Outcome::Succeeded);
    log.host_append(Event::UserMessage {
        turn: TurnId::new("bob-1"),
        message_id: None,
        principal: PrincipalId::new("bob"),
        text: "read cursor".into(),
        attachments: vec![],
        client_tools: vec![],
        authorized_tools: vec![],
        model_binding: None,
        approval_mode: dex_loop::ApprovalMode::Interactive,
    });
    ctx = log.rehydrate();
    let second = FakeModel::new(vec![
        vec![script("text((await load('page')) ?? 'missing');")],
        vec![text("done")],
    ]);
    assert_eq!(
        engine(&log, &second, &tools, Budget::default())
            .run(&mut ctx, &CancellationToken::new())
            .await,
        Ok(Exit::Done)
    );
    assert_eq!(
        outer_result(&log, "bob-1-1-0"),
        (Outcome::Succeeded, "missing".into())
    );
    ctx = log.start_turn("alice-2", "read cursor");
    let third = FakeModel::new(vec![
        vec![script("text(await load('page'));")],
        vec![text("done")],
    ]);
    assert_eq!(
        engine(&log, &third, &tools, Budget::default())
            .run(&mut ctx, &CancellationToken::new())
            .await,
        Ok(Exit::Done)
    );
    assert_eq!(
        outer_result(&log, "alice-2-1-0"),
        (Outcome::Succeeded, "alice-cursor".into())
    );
}

#[derive(Clone)]
struct UnknownTools(FakeTools);
impl Tools for UnknownTools {
    fn catalog(&self) -> &[ToolSpec] {
        self.0.catalog()
    }
    async fn search(&self, principal: &PrincipalId, query: &str) -> Vec<ToolName> {
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
        ToolResult::unknown("owner outcome not established")
    }
}

#[tokio::test]
async fn a_caught_unknown_effect_never_publishes_store_writes() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![script("await store('page', 'accepted'); text('saved');")],
        vec![script(
            "try { await tools.update({key:'effect'}); } catch {} await store('page', 'uncertain'); text('caught');",
        )],
        vec![script("text(await load('page'));")],
        vec![text("done")],
    ]);
    let tools = UnknownTools(FakeTools::new(vec![write_tool("update")]));
    let mut ctx = log.start_turn("t1", "paginate");
    let engine = dex_loop::Engine::new(
        log.clone(),
        model,
        tools.clone(),
        FakeEffects::default(),
        dex_loop::Lexicon::default(),
        Budget::default(),
    );
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(outer_result(&log, "t1-1-0").0, Outcome::Succeeded);
    assert_eq!(outer_result(&log, "t1-2-0").0, Outcome::Unknown);
    let unknown = outer_result(&log, "t1-2-0").1;
    assert!(unknown.starts_with("Outcome unknown: a nested tool may have taken effect;"));
    assert!(unknown.contains("caught"), "retain selected partial output");
    assert!(log.events().iter().any(|event| matches!(event,
        Event::ToolFinished {call,outcome:Outcome::Unknown,summary:Some(summary),..}
        if call.as_str()=="t1-2-0" && summary=="A tool may have taken effect. Check the action history before retrying.")));

    assert_eq!(
        outer_result(&log, "t1-3-0"),
        (Outcome::Succeeded, "accepted".into())
    );
    assert_eq!(log.rehydrate(), ctx);
}

#[tokio::test]
async fn image_projection_is_typed_and_oversize_never_commits_scratch_state() {
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a2uoAAAAASUVORK5CYII=";
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![script(&format!(
            "image({{mimeType:'image/png',data:'{PNG}'}}); text('selected');"
        ))],
        vec![script(
            "await store('bad', 'must-not-publish'); image({mimeType:'image/png',data:'iVBORw0KGgoA'+'A'.repeat(32768)});",
        )],
        vec![script("text((await load('bad')) ?? 'missing');")],
        vec![text("done")],
    ]);
    let tools = FakeTools::new(vec![]);
    let mut ctx = log.start_turn("images", "select images");
    assert_eq!(
        engine(&log, &model, &tools, Budget::default())
            .run(&mut ctx, &CancellationToken::new())
            .await,
        Ok(Exit::Done)
    );
    assert!(log.events().iter().any(|event| matches!(event, Event::ToolFinished { call, outcome: Outcome::Succeeded, output: Output::Blocks(blocks), .. } if call.as_str() == "images-1-0" && blocks.iter().any(|block| matches!(block, dex_loop::OutputBlock::Image { data, .. } if data == PNG)))));
    assert_eq!(outer_result(&log, "images-2-0").0, Outcome::Failed);
    assert_eq!(
        outer_result(&log, "images-3-0"),
        (Outcome::Succeeded, "missing".into())
    );
    assert_eq!(log.rehydrate(), ctx);
}

#[tokio::test]
async fn scratch_preparation_cannot_publish_under_a_foreign_principal() {
    let log = FakeLog::default();
    let mut ctx = log.start_turn("t1", "store");
    let parent = ProposedCall::new(
        dex_loop::CallId::new("parent"),
        ToolName::new("codemode"),
        json!({"code":""}),
        alice(),
    );
    for event in [
        Event::StepStarted {
            step: 1,
            control_through: dex_loop::Cursor::START,
        },
        Event::ModelStepCompleted {
            step: 1,
            text: String::new(),
            calls: vec![parent.clone()],
            reasoning: None,
            served: None,
            timing: None,
        },
        Event::CodeModeStorePrepared {
            parent: parent.id.clone(),
            principal: bob(),
            writes: agent_codemode::StoreWrites {
                set: [("cursor".into(), json!("foreign"))].into(),
                delete: vec![],
            },
        },
        Event::ToolFinished {
            call: parent.id,
            outcome: Outcome::Succeeded,
            output: Output::Text("done".into()),
            receipt: None,
            summary: None,
        },
    ] {
        let cursor = log.host_append(event.clone());
        ctx.observe(cursor, &event);
    }
    assert_eq!(log.rehydrate(), ctx);
    let encoded = serde_json::to_value(
        ctx.history()
            .iter()
            .filter_map(|entry| match &entry.message {
                dex_loop::Message::Tool { output, .. } => Some(output),
                _ => None,
            })
            .collect::<Vec<_>>(),
    )
    .unwrap();
    assert!(!encoded.to_string().contains("foreign"));
    let model = FakeModel::new(vec![
        vec![script("text((await load('cursor')) ?? 'missing');")],
        vec![text("done")],
    ]);
    let tools = FakeTools::new(vec![]);
    assert_eq!(
        engine(&log, &model, &tools, Budget::default())
            .run(&mut ctx, &CancellationToken::new())
            .await,
        Ok(Exit::Done)
    );
    assert_eq!(
        outer_result(&log, "t1-2-0"),
        (Outcome::Succeeded, "missing".into())
    );
    assert_eq!(log.rehydrate(), ctx);
}

#[derive(Clone)]
struct MeteredTools {
    inner: FakeTools,
    known_usage: bool,
}
impl Tools for MeteredTools {
    fn catalog(&self) -> &[ToolSpec] {
        self.inner.catalog()
    }
    fn codemode_model_operation(&self, name: &ToolName) -> Option<dex_loop::ModelOperation> {
        (name.as_str() == "classify").then_some(dex_loop::ModelOperation::Classify)
    }
    fn codemode_model_binding(&self, name: &ToolName) -> Option<dex_loop::ModelBinding> {
        (name.as_str() == "classify").then(|| dex_loop::ModelBinding {
            owner: "fixture-owner".into(),
            provider: "fixture-provider".into(),
            model: "fixture-classifier".into(),
        })
    }
    fn model_cost_bound(&self, _name: &ToolName) -> Option<u64> {
        Some(10)
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
    async fn model_usage(
        &self,
        _ctx: &Context,
        call: &ProposedCall,
        result: &ToolResult,
    ) -> Result<Option<dex_loop::Usage>, String> {
        if call.tool.as_str() != "classify" || result.outcome != Outcome::Succeeded {
            return Ok(None);
        }
        if !self.known_usage {
            return Err("owner cost unavailable".into());
        }
        Ok(Some(dex_loop::Usage {
            input_tokens: 3,
            output_tokens: 1,
            cost_micros: 10,
            ..Default::default()
        }))
    }
}

#[tokio::test]
async fn model_calls_in_a_promise_wave_recheck_actual_cost_before_the_next_admission() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![script(
            "await Promise.all([tools.classify({key:'one'}),tools.classify({key:'two'})]).catch(e => text(String(e))); ",
        )],
        vec![text("done")],
    ]);
    let tools = MeteredTools {
        inner: FakeTools::new(vec![read_tool("classify")]),
        known_usage: true,
    };
    let mut ctx = log.start_turn("t1", "classify two");
    let engine = dex_loop::Engine::new(
        log.clone(),
        model,
        tools.clone(),
        FakeEffects::default(),
        dex_loop::Lexicon::default(),
        Budget {
            max_cost_micros: 15,
            ..Default::default()
        },
    );
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(
        tools.inner.runs().len(),
        1,
        "only the reserved first model call may dispatch"
    );
    assert_eq!(ctx.usage().cost_micros, 10);
    assert!(log.events().iter().any(|event| matches!(event, Event::ToolFinished { call, outcome: Outcome::Failed, .. } if call.as_str() == "t1-1-0:codemode:1")));
    assert_eq!(
        log.events()
            .iter()
            .filter(|event| matches!(event, Event::Usage(_)))
            .count(),
        1
    );
    assert_eq!(log.rehydrate(), ctx);
}

#[tokio::test]
async fn unresolved_owner_cost_survives_replay_and_stops_finite_budget_calls() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![script(
            "await tools.classify({key:'one'}); await tools.classify({key:'two'}).catch(e => text(String(e))); ",
        )],
        vec![text("cannot be admitted")],
    ]);
    let tools = MeteredTools {
        inner: FakeTools::new(vec![read_tool("classify")]),
        known_usage: false,
    };
    let mut ctx = log.start_turn("t1", "classify two");
    let engine = dex_loop::Engine::new(
        log.clone(),
        model.clone(),
        tools.clone(),
        FakeEffects::default(),
        dex_loop::Lexicon::default(),
        Budget {
            max_cost_micros: 50,
            ..Default::default()
        },
    );
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Failed)
    );
    assert_eq!(tools.inner.runs().len(), 1);
    assert_eq!(
        model.seen().len(),
        1,
        "later ordinary inference must also stop"
    );
    assert!(
        log.events()
            .iter()
            .any(|event| matches!(event, Event::ModelUsageUnresolved { .. }))
    );
    assert!(log.events().iter().any(|event| matches!(event, Event::ToolFinished { call, outcome: Outcome::Failed, .. } if call.as_str() == "t1-1-0:codemode:1")));
    assert_eq!(log.rehydrate(), ctx);
}

#[tokio::test]
async fn specialist_spend_blocks_a_later_ordinary_mutation_in_the_same_step() {
    for known_usage in [true, false] {
        let log = FakeLog::default();
        let model = FakeModel::new(vec![
            vec![
                call("classify", json!({"key":"classify"})),
                call("update", json!({"key":"must-not-dispatch"})),
            ],
            vec![text("not admitted")],
        ]);
        let tools = MeteredTools {
            inner: FakeTools::new(vec![read_tool("classify"), write_tool("update")]),
            known_usage,
        };
        let mut ctx = log.start_turn("t1", "classify before updating");
        let engine = dex_loop::Engine::new(
            log.clone(),
            model,
            tools.clone(),
            FakeEffects::default(),
            dex_loop::Lexicon::default(),
            Budget {
                max_cost_micros: if known_usage { 10 } else { 50 },
                ..Default::default()
            },
        );
        assert_eq!(
            engine.run(&mut ctx, &CancellationToken::new()).await,
            Ok(Exit::Failed)
        );
        assert_eq!(
            tools.inner.runs().len(),
            1,
            "later ordinary mutation must not dispatch after specialist usage exhausted or unresolved"
        );
        assert!(log.events().iter().any(|event| matches!(event, Event::ToolFinished { call, outcome: Outcome::Failed, .. } if call.as_str() == "t1-1-1")));
        assert_eq!(log.rehydrate(), ctx);
    }
}

#[tokio::test]
async fn scratch_keys_and_values_are_sanitized_before_durable_preparation() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![script(
            "const k='private'+'-secret'; await store(k,{[k]:{[k]:k}}); text('saved');",
        )],
        vec![script(
            "text(await load('private'+'-secret')); text(await load('[redacted]'));",
        )],
        vec![text("done")],
    ]);
    let mut ctx = log.start_turn("t1", "save selected state");
    let engine = dex_loop::Engine::new(
        log.clone(),
        model,
        FakeTools::new(vec![]),
        FakeEffects::default(),
        dex_loop::Lexicon::new([("private-secret", "[redacted]")]),
        Budget::default(),
    );
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    let prepared = log
        .events()
        .into_iter()
        .find_map(|event| match event {
            Event::CodeModeStorePrepared { writes, .. } => Some(writes),
            _ => None,
        })
        .expect("known success prepares sanitized scratch state");
    assert_eq!(
        prepared.set,
        std::collections::BTreeMap::from([(
            "[redacted]".into(),
            json!({"[redacted]":{"[redacted]":"[redacted]"}})
        ),])
    );
    assert!(
        !serde_json::to_string(&log.events())
            .unwrap()
            .contains("private-secret")
    );
    assert_eq!(
        outer_result(&log, "t1-2-0"),
        (
            Outcome::Succeeded,
            "undefined\n{\"[redacted]\":{\"[redacted]\":\"[redacted]\"}}".into()
        )
    );
    assert!(
        !serde_json::to_string(&log.events())
            .unwrap()
            .contains("private-secret")
    );
    assert_eq!(log.rehydrate(), ctx);
}

#[tokio::test]
async fn sanitized_scratch_key_collisions_fail_without_replacing_accepted_state() {
    for conflicting in [
        "await store('private'+'-secret',1); await store('[redacted]',2);",
        "await store('state',{['private'+'-secret']:1,'[redacted]':2});",
        "await store('private'+'-secret',1);",
    ] {
        let log = FakeLog::default();
        let model = FakeModel::new(vec![
            vec![script(
                "await store('state','accepted'); await store('[redacted]','original');",
            )],
            vec![script(conflicting)],
            vec![script(
                "text(await load('state')); text(await load('[redacted]'));",
            )],
            vec![text("done")],
        ]);
        let mut ctx = log.start_turn("t1", "save selected state");
        let engine = dex_loop::Engine::new(
            log.clone(),
            model,
            FakeTools::new(vec![]),
            FakeEffects::default(),
            dex_loop::Lexicon::new([("private-secret", "[redacted]")]),
            Budget::default(),
        );
        assert_eq!(
            engine.run(&mut ctx, &CancellationToken::new()).await,
            Ok(Exit::Done)
        );
        assert_eq!(outer_result(&log, "t1-2-0").0, Outcome::Failed);
        assert!(
            outer_result(&log, "t1-2-0")
                .1
                .contains("collide after sanitization")
        );
        assert_eq!(
            outer_result(&log, "t1-3-0"),
            (Outcome::Succeeded, "accepted\noriginal".into())
        );
        assert!(
            !serde_json::to_string(&log.events())
                .unwrap()
                .contains("private-secret")
        );
        assert_eq!(log.rehydrate(), ctx);
    }
}

#[tokio::test]
async fn failed_wrapper_summary_is_host_owned_and_excludes_script_diagnostics() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![script("throw new Error('private-diagnostic');")],
        vec![text("done")],
    ]);
    let tools = FakeTools::new(vec![]);
    let mut ctx = log.start_turn("t1", "compose");
    assert_eq!(
        engine(&log, &model, &tools, Budget::default())
            .run(&mut ctx, &CancellationToken::new())
            .await,
        Ok(Exit::Done)
    );
    let events = log.events();
    let summary = events
        .iter()
        .find_map(|event| match event {
            Event::ToolFinished {
                call,
                outcome: Outcome::Failed,
                summary: Some(summary),
                ..
            } if call.as_str() == "t1-1-0" => Some(summary),
            _ => None,
        })
        .expect("failed wrapper has a customer-safe host summary");
    assert_eq!(
        summary,
        "The script stopped before completing. Review completed steps before changing its inputs and retrying."
    );
    assert!(
        outer_result(&log, "t1-1-0")
            .1
            .contains("private-diagnostic")
    );
    assert!(!summary.contains("private-diagnostic"));
    assert_eq!(log.rehydrate(), ctx);
}
