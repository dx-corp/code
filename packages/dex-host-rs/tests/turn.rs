//! Runs one full `dex_loop::Engine` turn against `maestro_dex_host`'s local
//! ports and a scripted fake model: a read tool that runs immediately, a
//! mutation that runs straight through (headless: no approval step), and a
//! final answer.
//!
//! This proves a headless turn completes with no approval request against a
//! purely local host — no database, no HTTP — before any of
//! Maestro's real turn logic is touched. See
//! `docs/design/maestro-on-dex-loop.md`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use dex_loop::{
    Budget, CancellationToken, Context, Engine, Event, Exit, Lexicon, Log, Model, ModelChunk,
    ModelError, Outcome, PrincipalId, ThreadId, ToolName, ToolSpec, TurnId,
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
                    class: dex_loop::ErrorClass::Unknown,
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
async fn read_then_write_then_done_with_no_approval() {
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
        approval_mode: dex_loop::ApprovalMode::Headless,
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

    // One run: the mutation goes straight through, nothing parks.
    let exit = engine.run(&mut ctx, &cancel).await.expect("run");
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
    assert!(
        !events.iter().any(|event| matches!(
            event,
            Event::ApprovalRequested { .. } | Event::ApprovalDecided { .. }
        )),
        "a headless turn must not request or decide any approval"
    );
    let finished = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                Event::ToolFinished {
                    outcome: Outcome::Succeeded,
                    ..
                }
            )
        })
        .count();
    assert_eq!(finished, 2, "the read and the write both ran to completion");
    assert!(matches!(events.last(), Some(Event::Final { text }) if text == "done"));
}
