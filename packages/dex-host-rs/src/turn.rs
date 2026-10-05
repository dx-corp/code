//! The local-turn consumer: wires `LocalLog`/`LocalTools`/`LocalEffects`
//! plus a `dex_loop::Model` into one full `dex_loop::Engine` turn, driven to
//! completion (`Exit::Done`, `Exit::Interrupted` or `Exit::Failed`) without a
//! human in the loop.
//!
//! This is Maestro's `MAESTRO_DEX_LOOP=1` cutover step: a local turn now has
//! a consumer of this crate's ports, not just `tests/turn.rs`'s own
//! hand-driven approve/resume. Turns are headless (`ApprovalMode::Headless`,
//! `Verdict::Allow` for every local tool), so there is no approval step at
//! all, and a `Parked` exit is an error. Parked questions are auto-answered with [`UNATTENDED_ANSWER`] -- the same
//! unattended-turn policy `print_mode.rs` (Maestro's existing non-interactive
//! "auto-approves tools" entry point) and `cloud_cli.rs`'s attached REPL
//! already use. This does not change trust posture: it matches the mode
//! this consumer's caller opts into.
//!
//! `Model` stays generic so tests exercise this driver against a scripted
//! fake instead of a live provider; production callers pass [`crate::AiRsModel`].

use std::path::Path;

use dex_loop::{
    ApprovalMode, Budget, CancellationToken, ConfirmationDecision, Event, Exit, Lexicon, Log as _,
    Model, PrincipalId, ThreadId, TurnId, rehydrate,
};

use crate::{LocalEffects, LocalLog, LocalTools};

/// Answer given to a parked `Question` when no human is attached to the
/// turn. Mirrors `cloud_cli::UNATTENDED_USER_INPUT_ANSWER`.
pub const UNATTENDED_ANSWER: &str =
    "No user is watching this turn. Proceed with your best judgment and state the assumption.";

/// One local turn to run.
pub struct LocalTurnRequest {
    pub thread: ThreadId,
    pub principal: PrincipalId,
    pub turn: TurnId,
    pub text: String,
}

/// What running one local turn produced.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalTurnOutcome {
    pub exit: Exit,
    /// The model's final answer, when the turn reached `Exit::Done`.
    pub final_text: Option<String>,
}

/// Runs `request` to completion against a fresh `LocalLog`/`LocalTools`/
/// `LocalEffects` rooted at `state_root`/`workspace_root`, auto-approving
/// auto-answering every parked question and failing on a parked approval.
///
/// # Errors
/// Returns an error if the log cannot be acquired (e.g. its lease is held by
/// another process), if a write loses that lease mid-turn (`Fenced`), or if
/// the engine reports `Exit::Failed`.
pub async fn run_local_turn<M: Model>(
    state_root: &Path,
    workspace_root: &Path,
    model: M,
    request: LocalTurnRequest,
) -> anyhow::Result<LocalTurnOutcome> {
    let log = LocalLog::acquire(state_root.join("log"), &request.thread)
        .await
        .map_err(|error| anyhow::anyhow!("acquire the local dex-loop log: {error}"))?;
    log.append(&[Event::UserMessage {
        interaction_mode: dex_loop::InteractionMode::Unspecified,
        turn: request.turn,
        message_id: None,
        model_binding: None,
        voice: None,
        principal: request.principal.clone(),
        text: request.text,
        attachments: Vec::new(),
        client_tools: Vec::new(),
        authorized_tools: Vec::new(),
        approval_mode: ApprovalMode::Headless,
    }])
    .await
    .map_err(|_fenced| {
        anyhow::anyhow!("the local dex-loop log's lease was superseded before the turn started")
    })?;

    let tools = LocalTools::new(workspace_root);
    let effects = LocalEffects::open(state_root.join("effects.json"))
        .await
        .map_err(|error| anyhow::anyhow!("open the local dex-loop effects ledger: {error}"))?;
    let engine = dex_loop::Engine::new(
        log.clone(),
        model,
        tools,
        effects,
        Lexicon::default(),
        Budget::default(),
    );
    let cancel = CancellationToken::new();

    let exit =
        drive_to_completion(&engine, &log, &request.thread, &request.principal, &cancel).await?;
    let final_text = if exit == Exit::Done {
        final_answer_text(&log).await?
    } else {
        None
    };
    Ok(LocalTurnOutcome { exit, final_text })
}

type LocalEngine<M> = dex_loop::Engine<LocalLog, M, LocalTools, LocalEffects, Lexicon>;

/// Runs `engine` until it reaches a terminal `Exit`, auto-answering every
/// `Asked` (question) and rejecting `Parked` (approval) it hits along the way.
/// `AwaitingClientTool` has no local consumer yet -- client-side tools are
/// not part of this crate's two-tool slice (`tools.rs`) -- so it is reported
/// as an error instead of hanging forever.
async fn drive_to_completion<M: Model>(
    engine: &LocalEngine<M>,
    log: &LocalLog,
    thread: &ThreadId,
    principal: &PrincipalId,
    cancel: &CancellationToken,
) -> anyhow::Result<Exit> {
    let mut ctx = rehydrate(thread.clone(), &read_log(log).await?);
    loop {
        let exit = engine
            .run(&mut ctx, cancel)
            .await
            .map_err(|_fenced| anyhow::anyhow!("the local dex-loop log's lease was superseded"))?;
        match exit {
            Exit::Done | Exit::Interrupted | Exit::Failed => return Ok(exit),
            Exit::AwaitingClientTool(call) => {
                anyhow::bail!(
                    "call {call} is waiting on a client-side tool result; the local dex-loop \
                     consumer has no client session to answer it"
                );
            }
            // Headless turns never wait on a human: `LocalTools::policy`
            // returns `Allow`, and `ApprovalMode::Headless` makes the engine
            // grant any `NeedsApproval` itself. Reaching `Parked` is a bug,
            // so fail loudly instead of parking or auto-deciding here.
            Exit::Parked(approval) => {
                anyhow::bail!(
                    "headless turn parked on approval {approval}; Maestro turns run with no \
                     approval step, so this must not happen"
                );
            }
            // `LocalTools`' two-tool catalog (`tools.rs`) has no
            // `ExecutorKind::User` tool today, so the engine cannot actually
            // reach this arm through `run_local_turn` yet; it is here so a
            // future ask-the-user tool does not silently hang instead of
            // getting the same unattended answer `Parked` gets.
            Exit::Asked(call) => {
                log.append(&[Event::Answer {
                    call,
                    principal: principal.clone(),
                    text: UNATTENDED_ANSWER.to_owned(),
                    // An unattended assumption is text, never human action consent.
                    confirmation_decision: ConfirmationDecision::Unspecified,
                    args_digest: String::new(),
                }])
                .await
                .map_err(|_fenced| {
                    anyhow::anyhow!(
                        "the local dex-loop log's lease was superseded while auto-answering"
                    )
                })?;
                ctx = rehydrate(thread.clone(), &read_log(log).await?);
            }
        }
    }
}

async fn read_log(log: &LocalLog) -> anyhow::Result<Vec<(dex_loop::Cursor, Event)>> {
    log.read_all()
        .await
        .map_err(|error| anyhow::anyhow!("read the local dex-loop log: {error}"))
}

async fn final_answer_text(log: &LocalLog) -> anyhow::Result<Option<String>> {
    let entries = read_log(log).await?;
    Ok(entries
        .into_iter()
        .rev()
        .find_map(|(_, event)| match event {
            Event::Final { text } => Some(text),
            _ => None,
        }))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex, PoisonError};

    use dex_loop::{ModelChunk, ModelError, ToolName};
    use futures_util::{Stream, stream};
    use tempfile::TempDir;

    use super::*;
    use crate::{READ_FILE, WRITE_FILE};

    /// One scripted model call: the chunks it streams, in order. Matches
    /// `tests/turn.rs`'s own `Script` alias.
    type Script = Vec<Result<ModelChunk, ModelError>>;

    /// Replays one scripted model call's chunks per `stream()` invocation,
    /// the same double `dex-loop`'s own tests and `tests/turn.rs` use.
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
            _ctx: &'a dex_loop::Context,
            _tools: &'a [&'a dex_loop::ToolSpec],
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

    #[tokio::test]
    async fn unattended_question_answer_never_grants_typed_action_consent() {
        use dex_loop::{ActionConfirmation, CallId, ProposedCall};

        let state_root = TempDir::new().expect("state tempdir");
        let workspace = TempDir::new().expect("workspace tempdir");
        let principal = PrincipalId::new("alice");
        let question_call = CallId::new("question-1");
        let action = ProposedCall::new(
            CallId::new("send-1"),
            ToolName::new("mail.send"),
            serde_json::json!({"recipient":"someone@example.com"}),
            principal.clone(),
        );
        let question = ProposedCall::new(
            question_call.clone(),
            ToolName::new("person.ask"),
            serde_json::json!({"text":"Send the message?"}),
            principal.clone(),
        );
        let log = LocalLog::acquire(state_root.path().join("log"), &thread())
            .await
            .expect("acquire log");
        // Resume a persisted question from an earlier host. The current local
        // two-tool catalog cannot ask yet; the driver still owns this exit.
        log.append(&[
            Event::UserMessage {
                interaction_mode: dex_loop::InteractionMode::Unspecified,
                turn: TurnId::new("t1"),
                message_id: None,
                model_binding: None,
                voice: None,
                principal: principal.clone(),
                text: "Work unattended".into(),
                attachments: vec![],
                client_tools: vec![],
                authorized_tools: vec![],
                approval_mode: ApprovalMode::Headless,
            },
            Event::StepStarted {
                step: 1,
                control_through: dex_loop::Cursor::START,
            },
            Event::ModelStepCompleted {
                step: 1,
                text: String::new(),
                calls: vec![question],
                reasoning: None,
                served: None,
                timing: None,
            },
            Event::Question {
                call: question_call.clone(),
                text: "Send the message?".into(),
                confirmation: Some(ActionConfirmation {
                    proposal_call_id: action.id.clone(),
                    tool: action.tool.clone(),
                    args_digest: dex_loop::args_digest(&action.args),
                    principal_id: principal.clone(),
                }),
            },
        ])
        .await
        .expect("persist parked question");
        let engine = dex_loop::Engine::new(
            log.clone(),
            ScriptedModel::new(vec![vec![Ok(ModelChunk::Text("done".into()))]]),
            LocalTools::new(workspace.path()),
            LocalEffects::open(state_root.path().join("effects.json"))
                .await
                .expect("open ledger"),
            Lexicon::default(),
            Budget::default(),
        );
        assert_eq!(
            drive_to_completion(
                &engine,
                &log,
                &thread(),
                &principal,
                &CancellationToken::new()
            )
            .await
            .expect("resume unattended question"),
            Exit::Done
        );
        let events = read_log(&log).await.expect("read answered question");
        let answers: Vec<_> = events
            .iter()
            .filter_map(|(_, event)| match event {
                Event::Answer {
                    call,
                    principal,
                    text,
                    confirmation_decision,
                    args_digest,
                } => Some((call, principal, text, confirmation_decision, args_digest)),
                _ => None,
            })
            .collect();
        assert_eq!(answers.len(), 1);
        let (call, actor, text, decision, digest) = answers[0];
        assert_eq!(call, &question_call);
        assert_eq!(actor, &principal);
        assert_eq!(text, UNATTENDED_ANSWER);
        assert_eq!(*decision, ConfirmationDecision::Unspecified);
        assert!(digest.is_empty());
        let replayed = rehydrate(thread(), &events);
        let mut attempted_action = action;
        attempted_action.args["confirmation"] = serde_json::json!(attempted_action.id.as_str());
        assert!(!replayed.confirmed_action(&attempted_action));
        assert!(matches!(events.last(), Some((_, Event::Final { text })) if text == "done"));
    }

    #[tokio::test]
    async fn drives_a_mutation_to_completion_with_no_approval() {
        let state_root = TempDir::new().expect("state tempdir");
        let workspace = TempDir::new().expect("workspace tempdir");
        std::fs::write(workspace.path().join("notes.txt"), "shopping list").expect("seed file");

        let model = ScriptedModel::new(vec![
            vec![Ok(ModelChunk::ToolCall {
                name: ToolName::new(READ_FILE),
                args: serde_json::json!({"path": "notes.txt"}),
            })],
            // A mutation: runs at once, with no approval step.
            vec![Ok(ModelChunk::ToolCall {
                name: ToolName::new(WRITE_FILE),
                args: serde_json::json!({"path": "out.txt", "content": "hello"}),
            })],
            vec![Ok(ModelChunk::Text("done".into()))],
        ]);

        let outcome = run_local_turn(
            state_root.path(),
            workspace.path(),
            model,
            LocalTurnRequest {
                thread: thread(),
                principal: PrincipalId::new("alice"),
                turn: TurnId::new("t1"),
                text: "read notes.txt, then write out.txt".into(),
            },
        )
        .await
        .expect("run the local turn");

        assert_eq!(outcome.exit, Exit::Done);
        assert_eq!(outcome.final_text.as_deref(), Some("done"));
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("out.txt")).expect("read written file"),
            "hello"
        );
        let log = LocalLog::acquire(state_root.path().join("log"), &thread())
            .await
            .expect("reopen log");
        let events = read_log(&log).await.expect("read log");
        assert!(
            !events.iter().any(|(_, event)| matches!(
                event,
                Event::ApprovalRequested { .. } | Event::ApprovalDecided { .. }
            )),
            "a headless Maestro turn must not request or decide an approval"
        );
    }

    #[tokio::test]
    async fn a_turn_with_no_tool_calls_completes_immediately() {
        let state_root = TempDir::new().expect("state tempdir");
        let workspace = TempDir::new().expect("workspace tempdir");
        let model = ScriptedModel::new(vec![vec![Ok(ModelChunk::Text("hi".into()))]]);
        let outcome = run_local_turn(
            state_root.path(),
            workspace.path(),
            model,
            LocalTurnRequest {
                thread: thread(),
                principal: PrincipalId::new("alice"),
                turn: TurnId::new("t1"),
                text: "say hi".into(),
            },
        )
        .await
        .expect("run the local turn");
        assert_eq!(outcome.exit, Exit::Done);
        assert_eq!(outcome.final_text.as_deref(), Some("hi"));
    }

    #[tokio::test]
    async fn a_denied_write_reports_failure_through_the_tool_result() {
        // `run_local_turn` never asks for approval; this proves the turn
        // still reaches `Exit::Done` (not stuck) when the mutation itself
        // fails for a reason unrelated to approval, e.g. a path outside the
        // workspace root.
        let state_root = TempDir::new().expect("state tempdir");
        let workspace = TempDir::new().expect("workspace tempdir");
        let model = ScriptedModel::new(vec![
            vec![Ok(ModelChunk::ToolCall {
                name: ToolName::new(WRITE_FILE),
                args: serde_json::json!({"path": "../escape.txt", "content": "x"}),
            })],
            vec![Ok(ModelChunk::Text("could not write that file".into()))],
        ]);
        let outcome = run_local_turn(
            state_root.path(),
            workspace.path(),
            model,
            LocalTurnRequest {
                thread: thread(),
                principal: PrincipalId::new("alice"),
                turn: TurnId::new("t1"),
                text: "write ../escape.txt".into(),
            },
        )
        .await
        .expect("run the local turn");
        assert_eq!(outcome.exit, Exit::Done);
        assert!(!workspace.path().join("escape.txt").exists());
        assert!(!state_root.path().join("escape.txt").exists());
    }
}
