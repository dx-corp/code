//! Runs Maestro's real local execution host (`ToolExecutor`, hooks, sandbox,
//! action firewall) on the dex-loop kernel through `HostTools`, with a
//! scripted model.
//!
//! A command the native actor would have prompted for does not run: it
//! returns a `needs_confirmation` preview, the model asks with `user.ask`
//! bound to it, and only the person's Confirm lets the identical call run.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use dex_loop::{
    ApprovalMode as TurnMode, Budget, CallId, CancellationToken, ConfirmationDecision, Context,
    Engine, Event, Exit, Lexicon, Log, Model, ModelChunk, ModelError, Outcome, PrincipalId,
    ProposedCall, ThreadId, ToolName, ToolSpec, Tools, TurnId, Verdict, args_digest,
};
use futures_util::{Stream, stream};
use maestro_dex_host::{
    CONFIRMATION_FIELD, HEADLESS_GATED, HostTools, LocalEffects, LocalLog, USER_ASK,
};
use maestro_local_host::agent::{NativeAgentConfig, dex_loop_execution_host};
use maestro_runtime::agent::CredentialVault;
use maestro_runtime::agent::native_host::ApprovalMode;
use serde_json::{Value, json};
use tempfile::TempDir;

type Script = Vec<Result<ModelChunk, ModelError>>;
type Step = Box<dyn Fn(&Context) -> Script + Send + Sync>;

/// One scripted response per model call; a step reads the context so it can
/// name a call id the engine assigned.
#[derive(Clone, Default)]
struct ScriptedModel {
    steps: Arc<Mutex<VecDeque<Step>>>,
}

impl ScriptedModel {
    fn new(steps: Vec<Step>) -> Self {
        Self {
            steps: Arc::new(Mutex::new(steps.into())),
        }
    }
}

impl Model for ScriptedModel {
    fn stream<'a>(
        &'a self,
        ctx: &'a Context,
        _tools: &'a [&'a ToolSpec],
    ) -> impl Stream<Item = Result<ModelChunk, ModelError>> + Send + 'a {
        let step = self
            .steps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front();
        let script = match step {
            Some(step) => step(ctx),
            None => vec![Err(ModelError {
                class: dex_loop::ErrorClass::Unknown,
                message: "no script left".into(),
            })],
        };
        stream::iter(script)
    }
}

fn call(name: &str, args: Value) -> Step {
    let name = name.to_owned();
    Box::new(move |_| {
        vec![Ok(ModelChunk::ToolCall {
            name: ToolName::new(&name),
            args: args.clone(),
        })]
    })
}

fn answer(text: &str) -> Step {
    let text = text.to_owned();
    Box::new(move |_| vec![Ok(ModelChunk::Text(text.clone()))])
}

/// The newest unexecuted preview's call id.
fn preview_id(ctx: &Context) -> String {
    ctx.tool_evidence()
        .iter()
        .rev()
        .find(|record| ctx.is_unexecuted_preview(&record.call.id))
        .map(|record| record.call.id.as_str().to_owned())
        .expect("a needs_confirmation preview")
}

fn ask_about_preview() -> Step {
    Box::new(|ctx| {
        vec![Ok(ModelChunk::ToolCall {
            name: ToolName::new(USER_ASK),
            args: json!({
                "question": "Run this command?",
                CONFIRMATION_FIELD: {"proposal_call_id": preview_id(ctx)},
            }),
        })]
    })
}

fn confirmed(name: &str, args: Value) -> Step {
    let name = name.to_owned();
    Box::new(move |ctx| {
        let mut args = args.clone();
        args[CONFIRMATION_FIELD] = Value::String(confirmed_preview(ctx));
        vec![Ok(ModelChunk::ToolCall {
            name: ToolName::new(&name),
            args,
        })]
    })
}

/// The preview the last `user.ask` was bound to.
fn confirmed_preview(ctx: &Context) -> String {
    ctx.history()
        .iter()
        .rev()
        .find_map(|entry| match &entry.message {
            dex_loop::Message::Assistant { calls, .. } => calls
                .iter()
                .find(|call| call.tool.as_str() == USER_ASK)
                .and_then(|call| call.args[CONFIRMATION_FIELD]["proposal_call_id"].as_str())
                .map(str::to_owned),
            _ => None,
        })
        .expect("a bound user.ask")
}

fn thread() -> ThreadId {
    ThreadId {
        org: "local".into(),
        workspace: "local".into(),
        thread: "thread-1".into(),
    }
}

fn alice() -> PrincipalId {
    PrincipalId::new("alice")
}

fn host_tools(workspace: &Path, mode: ApprovalMode) -> HostTools {
    let config = NativeAgentConfig {
        cwd: workspace.to_string_lossy().into_owned(),
        ..NativeAgentConfig::default()
    };
    let host = dex_loop_execution_host(&config, CredentialVault::new()).expect("compose host");
    HostTools::new(host, mode)
}

struct Turn {
    _state: TempDir,
    log: LocalLog,
    engine: Engine<LocalLog, ScriptedModel, HostTools, LocalEffects, Lexicon>,
    cancel: CancellationToken,
}

impl Turn {
    async fn start(tools: HostTools, mode: TurnMode, steps: Vec<Step>) -> Self {
        let state = TempDir::new().expect("state tempdir");
        let log = LocalLog::acquire(state.path().join("log"), &thread())
            .await
            .expect("acquire log");
        log.append(&[Event::UserMessage {
            interaction_mode: dex_loop::InteractionMode::Unspecified,
            turn: TurnId::new("t1"),
            message_id: None,
            model_binding: None,
            voice: None,
            principal: alice(),
            text: "make the marker".into(),
            attachments: Vec::new(),
            client_tools: Vec::new(),
            authorized_tools: Vec::new(),
            approval_mode: mode,
        }])
        .await
        .expect("append user message");
        let effects = LocalEffects::open(state.path().join("effects.json"))
            .await
            .expect("open effects ledger");
        let engine = Engine::new(
            log.clone(),
            ScriptedModel::new(steps),
            tools,
            effects,
            Lexicon::default(),
            Budget::default(),
        );
        Self {
            _state: state,
            log,
            engine,
            cancel: CancellationToken::new(),
        }
    }

    async fn run(&self) -> Exit {
        let entries = self.log.read_all().await.expect("read for rehydrate");
        let mut ctx = dex_loop::rehydrate(thread(), &entries);
        self.engine.run(&mut ctx, &self.cancel).await.expect("run")
    }

    async fn events(&self) -> Vec<Event> {
        self.log
            .read_all()
            .await
            .expect("read log")
            .into_iter()
            .map(|(_, event)| event)
            .collect()
    }

    /// Answers the parked question the way the desktop's card does.
    async fn decide(&self, asked: &CallId, decision: ConfirmationDecision) {
        let binding = self
            .events()
            .await
            .into_iter()
            .find_map(|event| match event {
                Event::Question {
                    call,
                    confirmation: Some(binding),
                    ..
                } if &call == asked => Some(binding),
                _ => None,
            })
            .expect("the question carries the action binding");
        self.log
            .append(&[Event::Answer {
                call: asked.clone(),
                principal: alice(),
                text: format!("{decision:?}"),
                confirmation_decision: decision,
                args_digest: binding.args_digest,
            }])
            .await
            .expect("append answer");
    }
}

/// `touch` writes, so the native actor's bash classifier asks first.
fn touch(workspace: &Path) -> Value {
    json!({"command": format!("touch {}", workspace.join("marker").display())})
}

#[tokio::test]
async fn the_catalog_offers_the_host_tools_and_the_kernel_question() {
    let workspace = TempDir::new().expect("workspace");
    let tools = host_tools(workspace.path(), ApprovalMode::Selective);
    let spec = |name: &str| tools.spec(&ToolName::new(name)).cloned();
    assert!(spec("read").expect("read").read_only);
    assert!(!spec("bash").expect("bash").read_only);
    assert!(spec("bash").expect("bash").schema["properties"][CONFIRMATION_FIELD].is_object());
    assert_eq!(
        spec(USER_ASK).expect("user.ask").executor,
        dex_loop::ExecutorKind::User
    );
    assert!(
        spec("ask_user").is_some(),
        "open questions stay on Maestro's own ask_user"
    );
}

#[tokio::test]
async fn user_ask_only_confirms_a_preview() {
    let workspace = TempDir::new().expect("workspace");
    let tools = host_tools(workspace.path(), ApprovalMode::Selective);
    let ctx = dex_loop::rehydrate(thread(), &[]);
    let open = ProposedCall::new(
        CallId::new("q1"),
        ToolName::new(USER_ASK),
        json!({"question": "Which branch?"}),
        alice(),
    );
    assert!(matches!(tools.policy(&ctx, &open).await, Verdict::Deny(_)));
    let unbound = ProposedCall::new(
        CallId::new("q2"),
        ToolName::new(USER_ASK),
        json!({"question": "Run it?", CONFIRMATION_FIELD: {"proposal_call_id": "nope"}}),
        alice(),
    );
    assert!(matches!(
        tools.policy(&ctx, &unbound).await,
        Verdict::Deny(_)
    ));
}

#[tokio::test]
async fn a_gated_command_runs_only_after_the_person_confirms_it() {
    let workspace = TempDir::new().expect("workspace");
    let marker = workspace.path().join("marker");
    let args = touch(workspace.path());
    let tools = host_tools(workspace.path(), ApprovalMode::Selective);
    let turn = Turn::start(
        tools,
        TurnMode::Interactive,
        vec![
            call("bash", args.clone()),
            ask_about_preview(),
            confirmed("bash", args.clone()),
            answer("created the marker"),
        ],
    )
    .await;

    let Exit::Asked(asked) = turn.run().await else {
        panic!("the turn parks on the confirmation question");
    };
    assert!(!marker.exists(), "nothing runs before the person decides");
    let events = turn.events().await;
    assert!(events.iter().any(|event| matches!(
        event,
        Event::ToolFinished { outcome: Outcome::Failed, output: dex_loop::Output::Text(text), .. }
            if text.contains("\"needs_confirmation\"")
    )));

    turn.decide(&asked, ConfirmationDecision::Confirm).await;
    assert_eq!(turn.run().await, Exit::Done);
    assert!(marker.exists(), "the confirmed command ran");
    assert!(matches!(
        turn.events().await.last(),
        Some(Event::Final { text }) if text == "created the marker"
    ));
}

#[tokio::test]
async fn a_declined_command_never_runs() {
    let workspace = TempDir::new().expect("workspace");
    let marker = workspace.path().join("marker");
    let args = touch(workspace.path());
    let tools = host_tools(workspace.path(), ApprovalMode::Selective);
    let turn = Turn::start(
        tools,
        TurnMode::Interactive,
        vec![
            call("bash", args.clone()),
            ask_about_preview(),
            // A model that ignores the decline and retries the bound call
            // gets another preview, not an execution.
            confirmed("bash", args.clone()),
            answer("left it alone"),
        ],
    )
    .await;

    let Exit::Asked(asked) = turn.run().await else {
        panic!("the turn parks on the confirmation question");
    };
    turn.decide(&asked, ConfirmationDecision::Decline).await;
    assert_eq!(turn.run().await, Exit::Done);
    assert!(!marker.exists(), "a declined command never runs");
}

#[tokio::test]
async fn an_ungated_read_runs_at_once() {
    let workspace = TempDir::new().expect("workspace");
    std::fs::write(workspace.path().join("notes.txt"), "shopping list").expect("seed");
    let tools = host_tools(workspace.path(), ApprovalMode::Selective);
    let turn = Turn::start(
        tools,
        TurnMode::Interactive,
        vec![
            call(
                "read",
                json!({"path": workspace.path().join("notes.txt").display().to_string()}),
            ),
            answer("read it"),
        ],
    )
    .await;
    assert_eq!(turn.run().await, Exit::Done);
    assert!(turn.events().await.iter().any(|event| matches!(
        event,
        Event::ToolFinished { outcome: Outcome::Succeeded, output: dex_loop::Output::Text(text), .. }
            if text.contains("shopping list")
    )));
}

#[tokio::test]
async fn policy_matches_the_native_actor() {
    let workspace = TempDir::new().expect("workspace");
    let selective = host_tools(workspace.path(), ApprovalMode::Selective);
    let yolo = host_tools(workspace.path(), ApprovalMode::Yolo);
    let interactive = dex_loop::rehydrate(thread(), &[]);
    let proposed =
        |args: Value| ProposedCall::new(CallId::new("c1"), ToolName::new("bash"), args, alice());

    // Gated under Selective; Yolo runs it, as the native actor does.
    let gated = proposed(touch(workspace.path()));
    assert!(matches!(
        selective.policy(&interactive, &gated).await,
        Verdict::NeedsConfirmation { .. }
    ));
    assert_eq!(yolo.policy(&interactive, &gated).await, Verdict::Allow);

    // A safe command is not gated.
    assert_eq!(
        selective
            .policy(&interactive, &proposed(json!({"command": "ls"})))
            .await,
        Verdict::Allow
    );

    // A preview's digest is the one `Context` binds a confirmation to.
    let Verdict::NeedsConfirmation { preview } = selective.policy(&interactive, &gated).await
    else {
        panic!("gated");
    };
    let document: Value = serde_json::from_str(&preview).expect("preview is JSON");
    assert_eq!(
        document["args_digest"],
        json!(args_digest(&touch(workspace.path())))
    );
}

#[tokio::test]
async fn a_headless_turn_refuses_a_gated_command() {
    let workspace = TempDir::new().expect("workspace");
    let marker = workspace.path().join("marker");
    let tools = host_tools(workspace.path(), ApprovalMode::Selective);
    let turn = Turn::start(
        tools,
        TurnMode::Headless,
        vec![call("bash", touch(workspace.path())), answer("could not")],
    )
    .await;
    assert_eq!(turn.run().await, Exit::Done);
    assert!(!marker.exists());
    assert!(turn.events().await.iter().any(|event| matches!(
        event,
        Event::ToolFinished { outcome: Outcome::Failed, output: dex_loop::Output::Text(text), .. }
            if text.contains(HEADLESS_GATED)
    )));
}

#[path = "host_tools/fuzz.rs"]
mod fuzz;
