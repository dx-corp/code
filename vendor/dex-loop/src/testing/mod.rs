//! Recovery contracts shared by real hosts. Enable the `testing` feature in
//! dev-dependencies. Each scenario needs a fresh, explicitly tenant-scoped
//! thread. Only model/tool dispatch is scripted; log and effect storage must
//! be the production adapters. No provider is contacted.
//!
//! `restart` must recreate adapters from durable storage and acquire a new
//! log generation, fencing the previous handle. This suite covers recovery
//! at the port boundary, not process kill/fsync or downstream reconciliation.

use std::future::Future;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use crate::{
    ApprovalMode, Budget, CallId, CancellationToken, Claim, Context, Cursor, Effects, Engine,
    Event, ExecutorKind, Exit, GovernanceClass, InteractionMode, Lexicon, Log, Model, ModelChunk,
    ModelError, Outcome, PrincipalId, ProposedCall, ReceiptId, ThreadId, ToolName, ToolResult,
    ToolSpec, Tools, TurnId, Verdict, rehydrate,
};
use futures_util::{Stream, stream};

/// Adapter lifecycle supplied by the host's own database/filesystem fixture.
pub trait RecoveryHost {
    type Log: Log;
    type Effects: Effects;
    fn thread(&self) -> ThreadId;
    fn log(&self) -> Self::Log;
    fn effects(&self) -> Self::Effects;
    fn events(&self) -> impl Future<Output = Vec<(Cursor, Event)>> + Send;
    fn restart(&mut self) -> impl Future<Output = ()> + Send;
}

/// Independently runnable cases so host test failures retain scenario names.
#[derive(Clone, Copy, Debug)]
pub enum RecoveryScenario {
    LostLease,
    ClaimWithoutResult,
    RecordedResultReplay,
    DuplicateClaim,
    Cancellation,
}

impl RecoveryScenario {
    /// Panics on a contract violation, as a normal Rust test assertion does.
    pub async fn run(self, host: &mut impl RecoveryHost) {
        assert!(!host.thread().org.is_empty());
        assert!(!host.thread().workspace.is_empty());
        let call = proposal();
        let initial = host
            .log()
            .append(&pending_events(&call))
            .await
            .expect("persist proposal");
        assert_eq!(initial.len(), 3);
        assert!(initial.windows(2).all(|pair| pair[0] < pair[1]));
        match self {
            Self::LostLease => {
                let stale = host.log();
                let before = host.events().await;
                host.restart().await;
                assert!(
                    stale.append(&[Event::Interrupted]).await.is_err(),
                    "old generation must be fenced"
                );
                assert!(stale.append_text("stale text".into()).await.is_err());
                let tools = ProbeTools::new(host.thread(), None);
                let runs = tools.runs.clone();
                let mut ctx = rehydrate(host.thread(), &before);
                assert!(
                    engine(stale, host.effects(), tools)
                        .run(&mut ctx, &CancellationToken::new())
                        .await
                        .is_err()
                );
                assert_eq!(runs.load(Ordering::SeqCst), 0);
                assert_eq!(
                    host.events().await,
                    before,
                    "stale actor cannot publish a finish"
                );
                host.log()
                    .append(&[Event::Interrupted])
                    .await
                    .expect("successor can append");
            }
            Self::ClaimWithoutResult | Self::RecordedResultReplay | Self::DuplicateClaim => {
                assert_eq!(
                    host.effects().claim(&call).await.expect("first claim"),
                    Claim::Granted
                );
                if matches!(self, Self::DuplicateClaim) {
                    assert!(matches!(
                        host.effects().claim(&call).await.expect("duplicate claim"),
                        Claim::Existing(_)
                    ));
                }
                if !matches!(self, Self::DuplicateClaim) {
                    host.log()
                        .append(&[Event::ToolStarted {
                            call: call.id.clone(),
                            tool: call.tool.clone(),
                            label: "Recovery mutation".into(),
                            principal: call.principal.clone(),
                        }])
                        .await
                        .expect("persist dispatch boundary before crash");
                }
                let recorded = ToolResult {
                    receipt: Some(ReceiptId::new("recovery-receipt")),
                    ..ToolResult::text("exact result\nwith preserved bytes: λ")
                };
                if matches!(self, Self::RecordedResultReplay) {
                    host.effects()
                        .record(&call.id, &recorded)
                        .await
                        .expect("record outcome before crash");
                }
                // ModelStepCompleted is durable, but no ToolFinished exists:
                // the process may have crashed before or after dispatch.
                host.restart().await;
                let prior = host
                    .effects()
                    .claim(&call)
                    .await
                    .expect("claim after reopen");
                match prior {
                    Claim::Existing(result) if matches!(self, Self::RecordedResultReplay) => {
                        assert_eq!(result, recorded)
                    }
                    Claim::Existing(result) => assert!(
                        matches!(result.outcome, Outcome::Running | Outcome::Unknown),
                        "missing evidence cannot become success or retry permission"
                    ),
                    Claim::Granted => panic!("restart granted a second dispatch"),
                }
                let tools = ProbeTools::new(host.thread(), None);
                let runs = tools.runs.clone();
                let mut ctx = rehydrate(host.thread(), &host.events().await);
                assert_eq!(
                    engine(host.log(), host.effects(), tools)
                        .run(&mut ctx, &CancellationToken::new())
                        .await,
                    Ok(Exit::Done)
                );
                assert_eq!(
                    runs.load(Ordering::SeqCst),
                    0,
                    "claimed mutation must never redispatch"
                );
                let durable = host.events().await;
                let results = finished(&durable, &call.id);
                assert_eq!(results.len(), 1);
                if matches!(self, Self::RecordedResultReplay) {
                    assert_eq!(results[0], recorded);
                } else {
                    assert_eq!(
                        results[0].outcome,
                        Outcome::Unknown,
                        "Running must settle to Unknown before model history"
                    );
                }
                assert_eq!(ctx, rehydrate(host.thread(), &durable));
                host.restart().await;
                let mut replay = rehydrate(host.thread(), &host.events().await);
                let tools = ProbeTools::new(host.thread(), None);
                let runs = tools.runs.clone();
                assert_eq!(
                    engine(host.log(), host.effects(), tools)
                        .run(&mut replay, &CancellationToken::new())
                        .await,
                    Ok(Exit::Done)
                );
                assert_eq!(runs.load(Ordering::SeqCst), 0);
                assert_eq!(
                    host.events().await,
                    durable,
                    "completed replay appends no duplicate result"
                );
            }
            Self::Cancellation => {
                let cancel = CancellationToken::new();
                let tools = ProbeTools::new(host.thread(), Some(cancel.clone()));
                let runs = tools.runs.clone();
                let mut ctx = rehydrate(host.thread(), &host.events().await);
                assert_eq!(
                    engine(host.log(), host.effects(), tools)
                        .run(&mut ctx, &cancel)
                        .await,
                    Ok(Exit::Interrupted)
                );
                assert_eq!(runs.load(Ordering::SeqCst), 1);
                let durable = host.events().await;
                let results = finished(&durable, &call.id);
                assert_eq!(
                    results,
                    vec![ToolResult::error("cancelled after partial work")]
                );
                assert!(
                    durable
                        .iter()
                        .any(|(_, event)| matches!(event, Event::Interrupted))
                );
                host.restart().await;
                assert_eq!(
                    host.effects()
                        .claim(&call)
                        .await
                        .expect("cancelled result survives restart"),
                    Claim::Existing(results[0].clone())
                );
                assert_eq!(ctx, rehydrate(host.thread(), &host.events().await));
            }
        }
    }
}

/// The same call/thread strings in separate tenant scopes must have independent
/// effect claims, results and log generations. Fixtures must share the same
/// underlying storage, with the local ledger path bound by its host to scope.
pub async fn tenant_isolation(left: &mut impl RecoveryHost, right: &mut impl RecoveryHost) {
    let a = left.thread();
    let b = right.thread();
    assert_eq!(
        a.thread, b.thread,
        "use matching thread ids to exercise the tenant key"
    );
    assert!(a.org != b.org || a.workspace != b.workspace);
    let call = proposal();
    left.log()
        .append(&pending_events(&call))
        .await
        .expect("left proposal");
    assert!(
        right.events().await.is_empty(),
        "foreign log cannot disclose left events"
    );
    assert_eq!(
        left.effects().claim(&call).await.expect("left claim"),
        Claim::Granted
    );
    left.effects()
        .record(&call.id, &ToolResult::text("left tenant result"))
        .await
        .expect("left result");
    assert_eq!(
        right.effects().claim(&call).await.expect("right claim"),
        Claim::Granted,
        "foreign result cannot satisfy a claim"
    );
    right
        .log()
        .append(&pending_events(&call))
        .await
        .expect("right proposal");
    let before = right.events().await;
    left.restart().await;
    right
        .log()
        .append(&[Event::Interrupted])
        .await
        .expect("foreign takeover cannot fence this scope");
    assert_eq!(right.events().await.len(), before.len() + 1);
    assert!(
        matches!(right.effects().claim(&call).await.expect("right duplicate"), Claim::Existing(result) if matches!(result.outcome, Outcome::Running | Outcome::Unknown))
    );
    assert_eq!(
        left.effects().claim(&call).await.expect("left replay"),
        Claim::Existing(ToolResult::text("left tenant result"))
    );
}

fn proposal() -> ProposedCall {
    ProposedCall::new(
        CallId::new("recovery-turn-1-0"),
        ToolName::new("recovery.mutate"),
        serde_json::json!({"value":"unchanged"}),
        PrincipalId::new("recovery-principal"),
    )
}

fn pending_events(call: &ProposedCall) -> Vec<Event> {
    vec![
        Event::UserMessage {
            interaction_mode: InteractionMode::Unspecified,
            turn: TurnId::new("recovery-turn"),
            message_id: None,
            principal: call.principal.clone(),
            text: "recover mutation".into(),
            attachments: vec![],
            client_tools: vec![],
            authorized_tools: vec![call.tool.clone()],
            approval_mode: ApprovalMode::Interactive,
            model_binding: None,
            voice: None,
        },
        Event::StepStarted {
            step: 1,
            control_through: Cursor::START,
        },
        Event::ModelStepCompleted {
            step: 1,
            text: String::new(),
            calls: vec![call.clone()],
            reasoning: None,
            served: None,
            timing: None,
        },
    ]
}

fn finished(events: &[(Cursor, Event)], id: &CallId) -> Vec<ToolResult> {
    events
        .iter()
        .filter_map(|(_, event)| match event {
            Event::ToolFinished {
                call,
                outcome,
                output,
                receipt,
                ..
            } if call == id => Some(ToolResult {
                outcome: *outcome,
                output: output.clone(),
                receipt: receipt.clone(),
            }),
            _ => None,
        })
        .collect()
}

fn engine<L: Log, E: Effects>(
    log: L,
    effects: E,
    tools: ProbeTools,
) -> Engine<L, Answer, ProbeTools, E, Lexicon> {
    Engine::new(
        log,
        Answer,
        tools,
        effects,
        Lexicon::default(),
        Budget {
            max_steps: 3,
            wall: Duration::from_secs(5),
            ..Budget::default()
        },
    )
}

struct Answer;
impl Model for Answer {
    fn stream<'a>(
        &'a self,
        ctx: &'a Context,
        _: &'a [&'a ToolSpec],
    ) -> impl Stream<Item = Result<ModelChunk, ModelError>> + Send + 'a {
        assert!(
            ctx.history()
                .iter()
                .any(|entry| matches!(&entry.message, crate::Message::Tool { .. })),
            "model must receive the recovered result"
        );
        stream::iter([Ok(ModelChunk::Text("recovery complete".into()))])
    }
}

struct ProbeTools {
    thread: ThreadId,
    catalog: Vec<ToolSpec>,
    runs: Arc<AtomicUsize>,
    cancel: Option<CancellationToken>,
}
impl ProbeTools {
    fn new(thread: ThreadId, cancel: Option<CancellationToken>) -> Self {
        Self {
            thread,
            cancel,
            runs: Arc::default(),
            catalog: vec![ToolSpec {
                name: proposal().tool,
                label: "Recovery mutation".into(),
                description: String::new(),
                schema: serde_json::json!({"type":"object","properties":{"value":{"type":"string"}},"required":["value"],"additionalProperties":false}),
                read_only: false,
                core: true,
                governance: GovernanceClass::Plain,
                executor: ExecutorKind::InProcess,
            }],
        }
    }
}
impl Tools for ProbeTools {
    fn catalog(&self) -> &[ToolSpec] {
        &self.catalog
    }
    async fn search(&self, _: &PrincipalId, _: &str) -> Vec<ToolName> {
        vec![]
    }
    async fn policy(&self, ctx: &Context, call: &ProposedCall) -> Verdict {
        assert_eq!(ctx.thread(), &self.thread);
        assert_eq!(
            call,
            &proposal(),
            "retry must retain principal, arguments and call identity"
        );
        Verdict::Allow
    }
    async fn run(
        &self,
        thread: &ThreadId,
        call: &ProposedCall,
        cancel: &CancellationToken,
    ) -> ToolResult {
        assert_eq!(thread, &self.thread);
        assert_eq!(call, &proposal());
        self.runs.fetch_add(1, Ordering::SeqCst);
        let root = self
            .cancel
            .as_ref()
            .expect("recovery should not redispatch this mutation");
        root.cancel();
        cancel.cancelled().await;
        ToolResult::error("cancelled after partial work")
    }
}
