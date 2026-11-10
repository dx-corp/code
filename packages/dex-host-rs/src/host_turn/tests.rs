use super::*;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use dex_loop::{Context, ExecutorKind, GovernanceClass, ModelChunk, ModelError, ToolSpec};
use futures_util::{Stream, stream};
use maestro_local_host::agent::{NativeAgentConfig, dex_loop_execution_host};
use maestro_runtime::agent::CredentialVault;
use maestro_runtime::agent::native_host::ApprovalMode as NativeApprovalMode;
use serde_json::json;
use tempfile::TempDir;

type Script = Box<dyn Fn(&Context) -> Vec<Result<ModelChunk, ModelError>> + Send + Sync>;

struct ScriptedModel(Mutex<VecDeque<Script>>);

impl Model for ScriptedModel {
    fn stream<'a>(
        &'a self,
        ctx: &'a Context,
        _tools: &'a [&'a ToolSpec],
    ) -> impl Stream<Item = Result<ModelChunk, ModelError>> + Send + 'a {
        let script = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
            .expect("scripted model response");
        stream::iter(script(ctx))
    }
}

fn scripted(chunks: Vec<ModelChunk>) -> Script {
    Box::new(move |_| chunks.iter().cloned().map(Ok).collect())
}

fn text_chunks() -> Vec<ModelChunk> {
    (0..OBSERVED_PAGE_SIZE * 3)
        .map(|index| ModelChunk::Text(format!("chunk-{index} ")))
        .collect()
}

fn tools(workspace: &Path, mode: NativeApprovalMode) -> HostTools {
    let config = NativeAgentConfig {
        cwd: workspace.to_string_lossy().into_owned(),
        ..NativeAgentConfig::default()
    };
    let host = dex_loop_execution_host(
        std::sync::Arc::new(maestro_local_host::agent::tool_host::BuiltInToolHost),
        &config,
        CredentialVault::new(),
    )
    .expect("compose native host");
    HostTools::new(host, mode)
}

async fn start(dir: &Path, tools: HostTools, scripts: Vec<Script>) -> HostTurnRun<ScriptedModel> {
    HostTurnRun::start(
        dir,
        ScriptedModel(Mutex::new(scripts.into())),
        tools,
        HostTurn {
            thread: ThreadId {
                org: "observer-org".into(),
                workspace: "observer-workspace".into(),
                thread: "observer-thread".into(),
            },
            principal: PrincipalId::new("alice"),
            turn: TurnId::new("observer-turn"),
            prompt: "run the scripted turn".into(),
            attachments: Vec::new(),
            approval: ApprovalMode::Interactive,
        },
    )
    .await
    .expect("start turn")
}

const PRODUCER_STALL_WINDOW: Duration = Duration::from_secs(10);

/// Require accepted log growth in each watchdog window until the producer completes.
async fn finish_producer<F, P, PF>(producer: F, mut progress: P) -> F::Output
where
    F: Future,
    P: FnMut() -> PF,
    PF: Future<Output = u64>,
{
    let mut accepted = tokio::time::timeout(PRODUCER_STALL_WINDOW, progress())
        .await
        .expect("read durable producer progress");
    tokio::pin!(producer);
    loop {
        tokio::select! {
            biased;
            finished = &mut producer => return finished,
            () = tokio::time::sleep(PRODUCER_STALL_WINDOW) => {
                let current = tokio::time::timeout(PRODUCER_STALL_WINDOW, progress())
                    .await
                    .expect("read durable producer progress");
                assert!(
                    current > accepted,
                    "producer must finish independently of observer speed (durable offset {accepted} -> {current})"
                );
                accepted = current;
            }
        }
    }
}

/// Start the producer without pulling any output, then wait on its actual
/// completion. A bounded channel with awaiting sends would stall here.
async fn finish_without_pulling(run: &mut HostTurnRun<ScriptedModel>) {
    run.spawn();
    let local = run.local.clone();
    let finished = finish_producer(run.running.as_mut().expect("producer started"), || {
        let local = local.clone();
        async move { local.observer_offset().await.expect("durable progress") }
    })
    .await
    .expect("turn task joined");
    run.running = None;
    run.finished = Some(finished);
    assert_eq!(run.observed.len(), 1, "only one wakeup may queue");
    assert!(
        run.buffered.is_empty(),
        "paused caller has no payload copies"
    );
}

#[tokio::test(start_paused = true)]
async fn observer_watchdog_accepts_durable_progress_beyond_one_window() {
    let progress = Arc::new(AtomicU64::new(0));
    let writer = Arc::clone(&progress);
    let producer = async move {
        for offset in 1..=3 {
            tokio::time::sleep(PRODUCER_STALL_WINDOW / 2).await;
            writer.store(offset, Ordering::SeqCst);
        }
        "finished"
    };
    let result = finish_producer(producer, || {
        std::future::ready(progress.load(Ordering::SeqCst))
    })
    .await;
    assert_eq!(result, "finished");
}

#[tokio::test(start_paused = true)]
async fn observer_watchdog_accepts_completion_at_the_deadline() {
    let producer = async {
        tokio::time::sleep(PRODUCER_STALL_WINDOW).await;
        "finished"
    };
    assert_eq!(
        finish_producer(producer, || std::future::ready(0)).await,
        "finished"
    );
}

#[tokio::test(start_paused = true)]
#[should_panic(expected = "producer must finish independently of observer speed")]
async fn observer_watchdog_rejects_a_producer_blocked_on_delivery() {
    let progress = AtomicU64::new(0);
    let (sender, _paused_observer) = tokio::sync::mpsc::channel(1);
    let producer = async {
        for offset in 1..=3 {
            progress.store(offset, Ordering::SeqCst);
            sender.send(()).await.expect("observer remains connected");
        }
    };
    finish_producer(producer, || {
        std::future::ready(progress.load(Ordering::SeqCst))
    })
    .await;
}

async fn pull_until_outcome(run: &mut HostTurnRun<ScriptedModel>) -> (Vec<Observed>, Step) {
    let mut seen = Vec::new();
    loop {
        let step = run.next().await.expect("pull turn");
        assert!(
            run.buffered.len() < OBSERVED_PAGE_SIZE,
            "catchup retains only one bounded page"
        );
        match step {
            Step::Observed(observed) => seen.push(observed),
            outcome => return (seen, outcome),
        }
    }
}

async fn accepted_engine_writes(run: &HostTurnRun<ScriptedModel>) -> Vec<Observed> {
    run.local
        .read_all()
        .await
        .expect("read accepted writes")
        .into_iter()
        .skip(1) // The caller's initial user message was never observed.
        .filter(|(_, event)| {
            !matches!(event, Event::Answer { .. } | Event::ClientToolResult { .. })
        })
        .map(|(cursor, event)| match event {
            Event::TextDelta { text } => Observed::Text(text),
            event => Observed::Event(cursor, Box::new(event)),
        })
        .collect()
}

#[tokio::test]
async fn paused_observer_is_bounded_while_mutation_finishes_and_replays_every_write() {
    let state = TempDir::new().expect("state");
    let workspace = TempDir::new().expect("workspace");
    let marker = workspace.path().join("marker");
    let mut chunks = text_chunks();
    chunks.push(ModelChunk::ToolCall {
        name: ToolName::new("bash"),
        args: json!({"command": format!("touch '{}'", marker.display())}),
    });
    let mut run = start(
        state.path(),
        tools(workspace.path(), NativeApprovalMode::Yolo),
        vec![
            scripted(chunks),
            scripted(vec![ModelChunk::Text("mutation complete".into())]),
        ],
    )
    .await;
    finish_without_pulling(&mut run).await;
    assert!(
        marker.exists(),
        "the mutation finishes while the observer is paused"
    );
    let expected = accepted_engine_writes(&run).await;
    assert!(expected.len() > OBSERVED_PAGE_SIZE * 2);
    let (seen, outcome) = pull_until_outcome(&mut run).await;
    assert_eq!(
        seen, expected,
        "every accepted write arrives once, in log order"
    );
    assert!(matches!(outcome, Step::Exit(Exit::Done)));
    assert!(matches!(run.next().await, Ok(Step::Exit(Exit::Done))));
}

#[tokio::test]
async fn client_tool_park_follows_all_writes_and_resume_hides_the_callers_result() {
    let state = TempDir::new().expect("state");
    let workspace = TempDir::new().expect("workspace");
    let client = ToolSpec {
        name: ToolName::new("client.read"),
        label: "Client read".into(),
        description: "Read on the caller".into(),
        schema: json!({"type": "object"}),
        read_only: true,
        core: true,
        governance: GovernanceClass::Plain,
        executor: ExecutorKind::Client,
    };
    let mut chunks = text_chunks();
    chunks.push(ModelChunk::ToolCall {
        name: client.name.clone(),
        args: json!({}),
    });
    let mut run = start(
        state.path(),
        tools(workspace.path(), NativeApprovalMode::Selective).with_client_tools([client]),
        vec![
            scripted(chunks),
            scripted(vec![ModelChunk::Text("done".into())]),
        ],
    )
    .await;
    finish_without_pulling(&mut run).await;
    let expected = accepted_engine_writes(&run).await;
    let (mut seen, outcome) = pull_until_outcome(&mut run).await;
    assert_eq!(seen, expected);
    let Step::Park(Park::ClientTool { call, .. }) = outcome else {
        panic!("client tool parks after its request is observed");
    };
    run.client_result(call, true, "caller result".into())
        .await
        .expect("append caller result");
    let (resumed, outcome) = pull_until_outcome(&mut run).await;
    seen.extend(resumed);
    assert_eq!(seen, accepted_engine_writes(&run).await);
    assert!(matches!(outcome, Step::Exit(Exit::Done)));
    assert!(
        run.caller_events.is_empty(),
        "skipped cursor is retired on catchup"
    );
}

#[tokio::test]
async fn approval_question_is_observed_before_park_and_decline_never_runs_mutation() {
    let state = TempDir::new().expect("state");
    let workspace = TempDir::new().expect("workspace");
    let marker = workspace.path().join("marker");
    let mut chunks = text_chunks();
    chunks.push(ModelChunk::ToolCall {
        name: ToolName::new("bash"),
        args: json!({"command": format!("touch '{}'", marker.display())}),
    });
    let ask: Script = Box::new(|ctx| {
        let preview = ctx
            .tool_evidence()
            .iter()
            .rev()
            .find(|record| ctx.is_unexecuted_preview(&record.call.id))
            .expect("previewed mutation");
        vec![Ok(ModelChunk::ToolCall {
            name: ToolName::new(crate::USER_ASK),
            args: json!({
                "question": "Run the mutation?",
                "confirmation": {"proposal_call_id": preview.call.id.as_str()},
            }),
        })]
    });
    let mut run = start(
        state.path(),
        tools(workspace.path(), NativeApprovalMode::Selective),
        vec![
            scripted(chunks),
            ask,
            scripted(vec![ModelChunk::Text("declined".into())]),
        ],
    )
    .await;
    finish_without_pulling(&mut run).await;
    let (mut seen, outcome) = pull_until_outcome(&mut run).await;
    assert_eq!(seen, accepted_engine_writes(&run).await);
    let Step::Park(Park::Confirm {
        question, binding, ..
    }) = outcome
    else {
        panic!("confirmation parks after its question is observed");
    };
    assert!(!marker.exists(), "the preview cannot execute");
    run.confirm(question, &binding, false)
        .await
        .expect("decline");
    let (resumed, outcome) = pull_until_outcome(&mut run).await;
    seen.extend(resumed);
    assert_eq!(seen, accepted_engine_writes(&run).await);
    assert!(matches!(outcome, Step::Exit(Exit::Done)));
    assert!(!marker.exists(), "the declined mutation cannot execute");
}
