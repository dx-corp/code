//! Policy, schema and original deadline boundaries for speculative reads.
#[allow(dead_code)]
mod support;
use dex_loop::{
    Budget, CancellationToken, Context, Engine, Event, Exit, Lexicon, PrincipalId, ProposedCall,
    ThreadId, ToolName, ToolResult, ToolSpec, Tools, Verdict,
};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use support::*;

#[derive(Clone)]
struct PolicyTools {
    inner: FakeTools,
    checks: Arc<AtomicUsize>,
    allow_checks: usize,
    delay: Duration,
}
impl Tools for PolicyTools {
    fn catalog(&self) -> &[ToolSpec] {
        self.inner.catalog()
    }
    async fn search(&self, p: &PrincipalId, q: &str) -> Vec<ToolName> {
        self.inner.search(p, q).await
    }
    async fn policy(&self, _: &Context, _: &ProposedCall) -> Verdict {
        let index = self.checks.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        if index < self.allow_checks {
            Verdict::Allow
        } else {
            Verdict::Deny("grant revoked".into())
        }
    }
    async fn run(&self, t: &ThreadId, c: &ProposedCall, cancel: &CancellationToken) -> ToolResult {
        self.inner.run(t, c, cancel).await
    }
}
fn limits(wall: Duration) -> Budget {
    Budget {
        max_steps: 10,
        max_tokens: 1_000_000,
        max_cost_micros: 1_000_000,
        wall,
    }
}
fn governed(
    log: &FakeLog,
    model: &FakeModel,
    tools: PolicyTools,
    wall: Duration,
) -> Engine<FakeLog, FakeModel, PolicyTools, FakeEffects, Lexicon> {
    Engine::new(
        log.clone(),
        model.clone(),
        tools,
        FakeEffects::default(),
        Lexicon::default(),
        limits(wall),
    )
}
fn tools(inner: FakeTools, allow_checks: usize, delay: Duration) -> PolicyTools {
    PolicyTools {
        inner,
        checks: Arc::default(),
        allow_checks,
        delay,
    }
}
fn finishes(log: &FakeLog) -> Vec<dex_loop::Output> {
    log.events()
        .into_iter()
        .filter_map(|e| match e {
            Event::ToolFinished { output, .. } => Some(output),
            _ => None,
        })
        .collect()
}
#[tokio::test]
async fn completed_prefetch_is_discarded_when_current_policy_revokes_it() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![call("read", json!({})), text("tail")],
        vec![text("done")],
    ])
    .with_chunk_delay(Duration::from_millis(30));
    let inner = FakeTools::new(vec![read_tool("read")]);
    let tools = tools(inner.clone(), 1, Duration::ZERO);
    let checks = tools.checks.clone();
    let engine = governed(&log, &model, tools, Duration::from_secs(2));
    let mut ctx = log.start_turn("t1", "read it");
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(checks.load(Ordering::SeqCst), 2);
    assert_eq!(inner.run_ids(), strings(&["t1-1-0"]));
    assert_eq!(
        finishes(&log),
        vec![dex_loop::Output::Text("denied: grant revoked".into())]
    );
    assert!(!history(&ctx).join("\n").contains("out/t1-1-0"));
    assert_eq!(log.rehydrate(), ctx);
}
#[tokio::test]
async fn a_restarted_started_read_rechecks_authority_before_execution() {
    let log = FakeLog::default();
    log.start_turn("t1", "read it");
    let proposal = dex_loop::ProposedCall::new(
        call_id("t1", 1, 0),
        ToolName::new("read"),
        json!({}),
        alice(),
    );
    log.host_append(Event::StepStarted {
        step: 1,
        control_through: dex_loop::Cursor::START,
    });
    log.host_append(Event::ModelStepCompleted {
        served: None,
        step: 1,
        text: String::new(),
        calls: vec![proposal.clone()],
        reasoning: None,
        timing: None,
    });
    log.host_append(Event::ToolStarted {
        call: proposal.id,
        tool: proposal.tool,
        label: "read".into(),
        principal: alice(),
    });
    let model = FakeModel::new(vec![vec![text("done")]]);
    let inner = FakeTools::new(vec![read_tool("read")]);
    let engine = governed(
        &log,
        &model,
        tools(inner.clone(), 0, Duration::ZERO),
        Duration::from_secs(2),
    );
    let mut ctx = log.rehydrate();
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert!(inner.run_ids().is_empty());
    assert_eq!(
        finishes(&log),
        vec![dex_loop::Output::Text("denied: grant revoked".into())]
    );
}
#[tokio::test]
async fn malformed_and_false_schemas_never_reach_a_read_or_mutation_executor() {
    for schema in [
        json!(false),
        json!(null),
        json!({"type":7}),
        json!({"$ref":"#/$defs/missing"}),
    ] {
        for read_only in [true, false] {
            let mut spec = if read_only {
                read_tool("check")
            } else {
                write_tool("check")
            };
            spec.schema = schema.clone();
            let log = FakeLog::default();
            let model = FakeModel::new(vec![vec![call("check", json!({}))], vec![text("done")]]);
            let inner = FakeTools::new(vec![spec]);
            let engine = engine(&log, &model, &inner, limits(Duration::from_secs(2)));
            let mut ctx = log.start_turn("t1", "check");
            assert_eq!(
                engine.run(&mut ctx, &CancellationToken::new()).await,
                Ok(Exit::Done)
            );
            assert!(inner.run_ids().is_empty(), "{schema}");
            assert!(
                !log.events()
                    .iter()
                    .any(|e| matches!(e, Event::ToolStarted { .. })),
                "{schema}"
            );
            assert_eq!(finishes(&log).len(), 1);
        }
    }
}
#[tokio::test]
async fn boolean_true_and_valid_object_schemas_remain_callable() {
    for schema in [json!(true), json!({"type":"object"})] {
        let mut spec = read_tool("check");
        spec.schema = schema;
        let log = FakeLog::default();
        let model = FakeModel::new(vec![vec![call("check", json!({}))], vec![text("done")]]);
        let inner = FakeTools::new(vec![spec]);
        let engine = engine(&log, &model, &inner, limits(Duration::from_secs(2)));
        let mut ctx = log.start_turn("t1", "check");
        assert_eq!(
            engine.run(&mut ctx, &CancellationToken::new()).await,
            Ok(Exit::Done)
        );
        assert_eq!(inner.run_ids(), strings(&["t1-1-0"]));
    }
}
#[tokio::test]
async fn a_pending_prefetch_policy_cannot_outlive_the_wall_or_interrupt() {
    for interrupt in [false, true] {
        let log = FakeLog::default();
        let model = FakeModel::new(vec![vec![call("read", json!({}))]]);
        let inner = FakeTools::new(vec![read_tool("read")]);
        let engine = governed(
            &log,
            &model,
            tools(inner.clone(), usize::MAX, Duration::from_secs(60)),
            Duration::from_millis(150),
        );
        let mut ctx = log.start_turn("t1", "read");
        let cancel = CancellationToken::new();
        let host = async {
            if interrupt {
                tokio::time::sleep(Duration::from_millis(30)).await;
                cancel.cancel();
            }
        };
        let (exit, ()) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(engine.run(&mut ctx, &cancel), host)
        })
        .await
        .expect("policy must be bounded");
        assert_eq!(
            exit,
            Ok(if interrupt {
                Exit::Interrupted
            } else {
                Exit::Failed
            })
        );
        assert!(inner.run_ids().is_empty());
        assert!(
            !log.events()
                .iter()
                .any(|e| matches!(e, Event::ToolStarted { .. }))
        );
    }
}

#[derive(Clone)]
struct SlowStartLog(FakeLog);
impl dex_loop::Log for SlowStartLog {
    async fn append(&self, events: &[Event]) -> Result<Vec<dex_loop::Cursor>, dex_loop::Fenced> {
        let cursors = self.0.append(events).await?;
        if events
            .iter()
            .any(|e| matches!(e, Event::ToolStarted { .. }))
        {
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
        Ok(cursors)
    }
    async fn append_text(&self, text: String) -> Result<(), dex_loop::Fenced> {
        self.0.append_text(text).await
    }
    async fn control_since(
        &self,
        c: dex_loop::Cursor,
    ) -> Result<Vec<(dex_loop::Cursor, Event)>, dex_loop::Fenced> {
        self.0.control_since(c).await
    }
}

#[tokio::test]
async fn an_expired_queued_read_is_never_polled_into_the_executor() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![vec![call("read", json!({}))], vec![text("done")]]);
    let first_polls = Arc::new(AtomicUsize::new(0));
    let count = first_polls.clone();
    let inner = FakeTools::new(vec![read_tool("read")]).on_run(move |_| {
        count.fetch_add(1, Ordering::SeqCst);
    });
    let engine = Engine::new(
        SlowStartLog(log.clone()),
        model,
        inner,
        FakeEffects::default(),
        Lexicon::default(),
        limits(Duration::from_secs(2)),
    )
    .with_tool_call_deadline(Duration::from_millis(40));
    let mut ctx = log.start_turn("t1", "read it");
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(first_polls.load(Ordering::SeqCst), 0);
    assert_eq!(finishes(&log).len(), 1);
    assert!(
        matches!(&finishes(&log)[0], dex_loop::Output::Text(text) if text.contains("time limit"))
    );
    assert_eq!(log.rehydrate(), ctx);
}

#[tokio::test]
async fn a_running_revoked_prefetch_is_dropped_without_waiting_or_exposing_output() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![call("read", json!({"key":"slow"})), text("tail")],
        vec![text("done")],
    ])
    .with_chunk_delay(Duration::from_millis(20));
    let first_polls = Arc::new(AtomicUsize::new(0));
    let count = first_polls.clone();
    let inner = FakeTools::new(vec![read_tool("read")])
        .delay("slow", Duration::from_secs(60))
        .on_run(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
        });
    let engine = governed(
        &log,
        &model,
        tools(inner, 1, Duration::ZERO),
        Duration::from_secs(2),
    );
    let mut ctx = log.start_turn("t1", "read");
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(1),
            engine.run(&mut ctx, &CancellationToken::new())
        )
        .await
        .expect("revoked future must be dropped"),
        Ok(Exit::Done)
    );
    assert_eq!(first_polls.load(Ordering::SeqCst), 1);
    assert_eq!(
        finishes(&log),
        vec![dex_loop::Output::Text("denied: grant revoked".into())]
    );
}

#[tokio::test]
async fn an_in_process_client_bridge_waits_for_the_committed_model_step() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![call("page.read", json!({})), text("tail")],
        vec![text("done")],
    ])
    .with_chunk_delay(Duration::from_millis(20));
    let committed = log.clone();
    let inner = FakeTools::new(vec![read_tool("page.read")]).on_run(move |_| {
        assert!(
            committed
                .events()
                .iter()
                .any(|event| matches!(event, Event::ModelStepCompleted { step: 1, .. })),
            "a client bridge must not wait for a response before its step commits"
        );
    });
    let engine = engine(&log, &model, &inner, limits(Duration::from_secs(2)));
    let mut ctx = log.start_turn_with_client_tools(
        "t1",
        "read my page",
        vec![dex_loop::ClientToolSpec {
            name: ToolName::new("page.read"),
            schema: json!({}),
            read_only: true,
            label: "Read page".into(),
        }],
    );
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(inner.run_ids(), strings(&["t1-1-0"]));
    assert_eq!(log.rehydrate(), ctx);
}

/// A host read may suspend while owning the same row lock as a progress/log
/// write. The engine must keep polling it through commit and wave completion.
#[tokio::test]
async fn host_read_log_contention_does_not_deadlock_commit_or_wave_completion() {
    use dex_loop::{Cursor, Fenced, Log};
    #[derive(Clone)]
    struct ContendedLog {
        inner: FakeLog,
        row: Arc<tokio::sync::Mutex<()>>,
        waits: Arc<AtomicUsize>,
    }
    impl Log for ContendedLog {
        async fn append(&self, events: &[Event]) -> Result<Vec<Cursor>, Fenced> {
            if self.row.try_lock().is_err() {
                self.waits.fetch_add(1, Ordering::SeqCst);
            }
            let _row = self.row.lock().await;
            self.inner.append(events).await
        }
        async fn append_text(&self, text: String) -> Result<(), Fenced> {
            let _row = self.row.lock().await;
            self.inner.append_text(text).await
        }
        async fn control_since(&self, after: Cursor) -> Result<Vec<(Cursor, Event)>, Fenced> {
            self.inner.control_since(after).await
        }
    }
    #[derive(Clone)]
    struct ContendedTools {
        inner: FakeTools,
        row: Arc<tokio::sync::Mutex<()>>,
    }
    impl Tools for ContendedTools {
        fn catalog(&self) -> &[ToolSpec] {
            self.inner.catalog()
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
            let _row = self.row.lock().await;
            tokio::time::sleep(Duration::from_millis(30)).await;
            self.inner.run(thread, call, cancel).await
        }
    }
    let inner = FakeLog::default();
    let row = Arc::new(tokio::sync::Mutex::new(()));
    let waits = Arc::new(AtomicUsize::new(0));
    let log = ContendedLog {
        inner: inner.clone(),
        row: row.clone(),
        waits: waits.clone(),
    };
    let reads = FakeTools::new(vec![read_tool("read")]);
    let tools = ContendedTools {
        inner: reads.clone(),
        row,
    };
    let model = FakeModel::new(vec![
        vec![
            call("read", json!({})),
            call("read", json!({})),
            text("tail"),
        ],
        vec![text("done")],
    ])
    .with_chunk_delay(Duration::from_millis(1));
    let engine = Engine::new(
        log,
        model,
        tools,
        FakeEffects::default(),
        Lexicon::default(),
        limits(Duration::from_secs(2)),
    );
    let mut ctx = inner.start_turn("t1", "read both");
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(2),
            engine.run(&mut ctx, &CancellationToken::new())
        )
        .await
        .expect("row lock released while its host read is polled"),
        Ok(Exit::Done)
    );
    assert!(
        waits.load(Ordering::SeqCst) > 0,
        "fixture must contend on the real log path"
    );
    let mut runs = reads.run_ids();
    runs.sort();
    assert_eq!(runs, strings(&["t1-1-0", "t1-1-1"]));
    assert_eq!(inner.rehydrate(), ctx);
}
