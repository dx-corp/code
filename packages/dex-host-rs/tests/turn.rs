//! Runs one full `dex_loop::Engine` turn against `maestro_dex_host`'s local
//! ports and a scripted fake model: a read tool that runs immediately, a
//! mutation that parks for approval, an approval, and a final answer.
//!
//! This proves the kernel's park/approve/resume semantics run correctly
//! against a purely local host — no database, no HTTP — before any of
//! Maestro's real turn logic is touched. See
//! `docs/design/maestro-on-dex-loop.md`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use dex_loop::{
    ApprovalId, Budget, CallId, CancellationToken, Context, Engine, Event, Exit, Lexicon, Log,
    Model, ModelChunk, ModelError, Outcome, PrincipalId, ThreadId, ToolName, ToolSpec, TurnId,
    args_digest,
};
use futures_util::Stream;
use futures_util::stream;
use maestro_dex_host::{LocalEffects, LocalLog, LocalTools, READ_FILE, WRITE_FILE};
use tempfile::TempDir;

/// One scripted model call: the chunks it streams, in order.
type Script = Vec<Result<ModelChunk, ModelError>>;

/// Replays one scripted model response per call, matching
/// `dex-loop`'s own test double (`dex-loop/tests/support/mod.rs`), which
/// this crate cannot import directly since it is not a public dependency
/// of another workspace's test binary.
#[derive(Clone, Default)]
struct ScriptedModel {
    scripts: Arc<Mutex<VecDeque<Script>>>,
}

impl ScriptedModel {
    fn new(scripts: Vec<Script>) -> Self {
        Self {
            scripts: Arc::new(Mutex::new(scripts.into())),
        }
    }
}

impl Model for ScriptedModel {
    fn stream<'a>(
        &'a self,
        _ctx: &'a Context,
        _tools: &'a [&'a ToolSpec],
    ) -> impl Stream<Item = Result<ModelChunk, ModelError>> + Send + 'a {
        let script = self
            .scripts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
            .unwrap_or_else(|| {
                vec![Err(ModelError {
                    message: "no script left".into(),
                })]
            });
        stream::iter(script)
    }
}

fn thread() -> ThreadId {
    ThreadId {
        org: "org-1".into(),
        workspace: "ws-1".into(),
        thread: "thread-1".into(),
    }
}

fn alice() -> PrincipalId {
    PrincipalId::new("alice")
}

#[tokio::test]
async fn read_then_approved_write_then_done() {
    let state_root = TempDir::new().expect("state tempdir");
    let workspace = TempDir::new().expect("workspace tempdir");
    std::fs::write(workspace.path().join("notes.txt"), "shopping list").expect("seed file");

    let log = LocalLog::acquire(state_root.path().join("log"), &thread())
        .await
        .expect("acquire log");
    log.append(&[Event::UserMessage {
        turn: TurnId::new("t1"),
        message_id: None,
        principal: alice(),
        text: "read notes.txt, then write out.txt".into(),
        attachments: Vec::new(),
        client_tools: Vec::new(),
        authorized_tools: Vec::new(),
        approval_mode: dex_loop::ApprovalMode::Interactive,
    }])
    .await
    .expect("append user message");

    let tools = LocalTools::new(workspace.path());
    let effects = LocalEffects::open(state_root.path().join("effects.json"))
        .await
        .expect("open effects ledger");
    let write_args = serde_json::json!({"path": "out.txt", "content": "hello from dex-loop"});
    let model = ScriptedModel::new(vec![
        vec![Ok(ModelChunk::ToolCall {
            name: ToolName::new(READ_FILE),
            args: serde_json::json!({"path": "notes.txt"}),
        })],
        vec![Ok(ModelChunk::ToolCall {
            name: ToolName::new(WRITE_FILE),
            args: write_args.clone(),
        })],
        vec![Ok(ModelChunk::Text("done".into()))],
    ]);
    let engine = Engine::new(
        log.clone(),
        model,
        tools,
        effects,
        Lexicon::default(),
        Budget::default(),
    );
    let cancel = CancellationToken::new();

    let entries = log.read_all().await.expect("read for rehydrate");
    let mut ctx = dex_loop::rehydrate(thread(), &entries);

    let exit = engine.run(&mut ctx, &cancel).await.expect("first run");
    // The read call ran with no approval; the write call parked on one.
    let expected_write_call = CallId::new("t1-2-0");
    let expected_approval = ApprovalId::new(format!("approve-{expected_write_call}"));
    assert_eq!(exit, Exit::Parked(expected_approval.clone()));

    // The mutation has not run yet: no file, no approval requested for the
    // read call.
    assert!(!workspace.path().join("out.txt").exists());
    let events: Vec<Event> = log
        .read_all()
        .await
        .expect("read log")
        .into_iter()
        .map(|(_, event)| event)
        .collect();
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::ToolFinished {
                outcome: Outcome::Succeeded,
                ..
            }
        )),
        "the read call must have finished before the write call parked"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::ApprovalRequested { call, .. } if call.as_str() != expected_write_call.as_str())),
        "only the mutation should have requested approval"
    );

    log.append(&[Event::ApprovalDecided {
        call: expected_write_call.clone(),
        approval: expected_approval,
        args_digest: args_digest(&write_args),
        approved: true,
        principal: alice(),
    }])
    .await
    .expect("append approval decision");

    let entries = log.read_all().await.expect("read for resume");
    let mut ctx = dex_loop::rehydrate(thread(), &entries);
    let exit = engine.run(&mut ctx, &cancel).await.expect("second run");
    assert_eq!(exit, Exit::Done);

    assert_eq!(
        std::fs::read_to_string(workspace.path().join("out.txt")).expect("read written file"),
        "hello from dex-loop"
    );
    let events: Vec<Event> = log
        .read_all()
        .await
        .expect("read final log")
        .into_iter()
        .map(|(_, event)| event)
        .collect();
    assert!(matches!(events.last(), Some(Event::Final { text }) if text == "done"));
}
