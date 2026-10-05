//! One kernel turn on a Maestro host, driven by its caller.
//!
//! `HostTurnRun` owns a per-turn local log and effect ledger and runs the
//! engine in its own task, so a started mutation always finishes even if the
//! caller stops listening. The caller pulls [`Step`]s: every accepted log
//! write, then either a park the caller resolves (a confirmation question or
//! a caller-run tool) or the turn's exit.
//!
//! This is the one place a Maestro surface drives the kernel; the gateway's
//! chat endpoints, automations and A2A turns are thin projections of it.

use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dex_loop::{
    ActionConfirmation, ApprovalMode, ArtifactRef, Budget, CallId, CancellationToken,
    ClientToolSpec, ConfirmationDecision, Cursor, Engine, Event, Exit, Lexicon, Log as _, Model,
    ModelSummarizer, Outcome, PrincipalId, ThreadId, Threshold, ToolName, TurnId, rehydrate,
};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::{HostTools, LocalEffects, LocalLog, Observed, ObservedLog};

/// One turn's request.
pub struct HostTurn {
    pub thread: ThreadId,
    pub principal: PrincipalId,
    pub turn: TurnId,
    pub prompt: String,
    /// Local files; images reach the model as image blocks.
    pub attachments: Vec<String>,
    pub approval: ApprovalMode,
}

/// Where the turn stopped for its caller.
#[derive(Clone, Debug, PartialEq)]
pub enum Park {
    /// The model asked the person to confirm one previewed call.
    Confirm {
        question: CallId,
        binding: ActionConfirmation,
        /// The previewed call's arguments, without `confirmation`.
        args: Value,
    },
    /// A caller-run tool (`ExecutorKind::Client`) was called.
    ClientTool {
        call: CallId,
        tool: ToolName,
        args: Value,
    },
}

/// What the caller pulls next.
#[derive(Debug)]
pub enum Step {
    Observed(Observed),
    Park(Park),
    Exit(Exit),
}

// A model need not be Clone to serve the primary step and its summary. Both
// calls use this same port, preserving provider ownership, routing and receipts.
struct SharedModel<M>(Arc<M>);

impl<M> Clone for SharedModel<M> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<M: Model> Model for SharedModel<M> {
    fn stream<'a>(
        &'a self,
        ctx: &'a dex_loop::Context,
        tools: &'a [&'a dex_loop::ToolSpec],
    ) -> impl futures_util::Stream<Item = Result<dex_loop::ModelChunk, dex_loop::ModelError>> + Send + 'a
    {
        self.0.stream(ctx, tools)
    }

    fn prepare_turn(&self, ctx: &dex_loop::Context) {
        self.0.prepare_turn(ctx);
    }
}

// Per-turn context projection only: compaction never replaces the raw LocalLog.
// Keep the hosted planner's bounded default, with explicit local policy rather
// than reading the hosted service's environment or selecting another credential.
const HISTORY_BYTES: usize = 48 * 1024;
type TurnEngine<M> = Engine<
    ObservedLog<LocalLog>,
    SharedModel<M>,
    HostTools,
    LocalEffects,
    Lexicon,
    Threshold<ModelSummarizer<SharedModel<M>>>,
>;

/// At most this many payloads are retained between pulls.
const OBSERVED_PAGE_SIZE: usize = 32;

type Finished = Result<(Exit, dex_loop::Context), String>;

/// A turn in progress. See the module docs.
pub struct HostTurnRun<M: Model + 'static> {
    dir: PathBuf,
    thread: ThreadId,
    principal: PrincipalId,
    local: LocalLog,
    engine: Arc<TurnEngine<M>>,
    observed: mpsc::Receiver<()>,
    observed_offset: u64,
    caller_events: BTreeSet<Cursor>,
    cancel: CancellationToken,
    running: Option<JoinHandle<Finished>>,
    buffered: VecDeque<Observed>,
    finished: Option<Finished>,
    exited: Option<Exit>,
    questions: Vec<(CallId, ActionConfirmation)>,
    client_calls: Vec<(CallId, ToolName, Value)>,
}

impl<M: Model + 'static> HostTurnRun<M> {
    /// Opens the turn's log under `dir` (which it owns and removes on drop of
    /// the directory by the caller) and records the user's message.
    pub async fn start(
        dir: &Path,
        model: M,
        tools: HostTools,
        turn: HostTurn,
    ) -> Result<Self, String> {
        let local = LocalLog::acquire(dir.join("log"), &turn.thread)
            .await
            .map_err(|error| format!("open the turn log: {error}"))?;
        let client_tools = tools
            .client_specs()
            .map(|spec| ClientToolSpec {
                name: spec.name.clone(),
                schema: spec.schema.clone(),
                read_only: spec.read_only,
                label: spec.label.clone(),
            })
            .collect();
        local
            .append(&[Event::UserMessage {
                interaction_mode: dex_loop::InteractionMode::Unspecified,
                turn: turn.turn,
                message_id: None,
                model_binding: None,
                voice: None,
                principal: turn.principal.clone(),
                text: turn.prompt,
                attachments: turn
                    .attachments
                    .iter()
                    .map(|path| ArtifactRef::new(path.as_str()))
                    .collect(),
                client_tools,
                authorized_tools: Vec::new(),
                approval_mode: turn.approval,
            }])
            .await
            .map_err(|_| "the turn log was superseded before the turn started".to_owned())?;
        let effects = LocalEffects::open(dir.join("effects.json"))
            .await
            .map_err(|error| format!("open the effect ledger: {error}"))?;
        let observed_offset = local
            .observer_offset()
            .await
            .map_err(|error| format!("start observing the turn log: {error}"))?;
        let (log, observed) = ObservedLog::new(local.clone());
        let model = SharedModel(Arc::new(model));
        let compactor = Threshold::for_turns(HISTORY_BYTES, ModelSummarizer::new(model.clone()));
        let engine = Engine::new(
            log,
            model,
            tools,
            effects,
            Lexicon::default(),
            Budget::default(),
        )
        .with_compactor(compactor);
        Ok(Self {
            dir: dir.to_path_buf(),
            thread: turn.thread,
            principal: turn.principal,
            local,
            engine: Arc::new(engine),
            observed,
            observed_offset,
            caller_events: BTreeSet::new(),
            cancel: CancellationToken::new(),
            running: None,
            buffered: VecDeque::new(),
            finished: None,
            exited: None,
            questions: Vec::new(),
            client_calls: Vec::new(),
        })
    }

    /// The directory holding this turn's log and ledger.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Asks the engine to stop: the model stream ends, reads abandon, and a
    /// started mutation still finishes. Keep pulling until `Step::Exit`.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    fn spawn(&mut self) {
        let engine = Arc::clone(&self.engine);
        let local = self.local.clone();
        let thread = self.thread.clone();
        let cancel = self.cancel.clone();
        self.running = Some(tokio::spawn(async move {
            let entries = local
                .read_all()
                .await
                .map_err(|error| format!("read the turn log: {error}"))?;
            let mut ctx = rehydrate(thread, &entries);
            let exit = engine
                .run(&mut ctx, &cancel)
                .await
                .map_err(|fenced| fenced.to_string())?;
            Ok((exit, ctx))
        }));
    }

    fn note(&mut self, observed: &Observed) {
        if let Observed::Event(_, event) = observed {
            match event.as_ref() {
                Event::Question {
                    call,
                    confirmation: Some(binding),
                    ..
                } => self.questions.push((call.clone(), binding.clone())),
                Event::ClientToolRequested {
                    call, tool, args, ..
                } => self
                    .client_calls
                    .push((call.clone(), tool.clone(), args.clone())),
                _ => {}
            }
        }
    }

    /// The next accepted write, park, or the exit. After `Step::Exit` it
    /// keeps returning that exit.
    pub async fn next(&mut self) -> Result<Step, String> {
        loop {
            if let Some(observed) = self.buffered.pop_front() {
                self.note(&observed);
                return Ok(Step::Observed(observed));
            }
            if let Some(exit) = &self.exited {
                return Ok(Step::Exit(exit.clone()));
            }
            // Catch up even without a wakeup: notifications may coalesce,
            // and completion may win the select after the final write.
            let (offset, entries) = self
                .local
                .read_observed(self.observed_offset, OBSERVED_PAGE_SIZE)
                .await
                .map_err(|error| format!("read observed turn writes: {error}"))?;
            self.observed_offset = offset;
            let had_entries = !entries.is_empty();
            for (cursor, event) in entries {
                // The old observer wrapped only engine writes. Answers and
                // caller tool results still reach rehydrate, not the surface.
                if self.caller_events.remove(&cursor) {
                    continue;
                }
                self.buffered.push_back(match event {
                    Event::TextDelta { text } => Observed::Text(text),
                    event => Observed::Event(cursor, Box::new(event)),
                });
            }
            if had_entries {
                continue;
            }
            // Everything the run wrote reaches the caller before its outcome.
            if let Some(finished) = self.finished.take() {
                return self.settle(finished);
            }
            if self.running.is_none() {
                self.spawn();
            }
            let running = self.running.as_mut().expect("spawned above");
            let finished = tokio::select! {
                biased;
                Some(()) = self.observed.recv() => continue,
                finished = running => finished.map_err(|error| format!("the turn task failed: {error}"))?,
            };
            self.running = None;
            self.finished = Some(finished);
        }
    }

    fn settle(&mut self, finished: Finished) -> Result<Step, String> {
        let (exit, ctx) = finished?;
        match exit {
            Exit::Asked(call) => {
                let Some((_, binding)) = self.questions.iter().find(|(asked, _)| *asked == call)
                else {
                    return self.exit(Exit::Failed);
                };
                let binding = binding.clone();
                let args = ctx
                    .action_preview(&binding.proposal_call_id)
                    .map(|proposal| proposal.args.clone())
                    .unwrap_or(Value::Null);
                Ok(Step::Park(Park::Confirm {
                    question: call,
                    binding,
                    args,
                }))
            }
            Exit::AwaitingClientTool(call) => {
                let Some((_, tool, args)) =
                    self.client_calls.iter().find(|(asked, ..)| *asked == call)
                else {
                    return self.exit(Exit::Failed);
                };
                Ok(Step::Park(Park::ClientTool {
                    call,
                    tool: tool.clone(),
                    args: args.clone(),
                }))
            }
            exit => self.exit(exit),
        }
    }

    fn exit(&mut self, exit: Exit) -> Result<Step, String> {
        self.exited = Some(exit.clone());
        Ok(Step::Exit(exit))
    }

    /// Answers a `Park::Confirm`; the next pull resumes the turn.
    pub async fn confirm(
        &mut self,
        question: CallId,
        binding: &ActionConfirmation,
        approved: bool,
    ) -> Result<(), String> {
        let cursors = self
            .local
            .append(&[Event::Answer {
                call: question,
                principal: self.principal.clone(),
                text: if approved { "Confirm" } else { "Decline" }.to_owned(),
                confirmation_decision: if approved {
                    ConfirmationDecision::Confirm
                } else {
                    ConfirmationDecision::Decline
                },
                args_digest: binding.args_digest.clone(),
            }])
            .await
            .map_err(|fenced| fenced.to_string())?;
        self.caller_events.extend(cursors);
        Ok(())
    }

    /// Answers a `Park::ClientTool`; the next pull resumes the turn.
    pub async fn client_result(
        &mut self,
        call: CallId,
        succeeded: bool,
        output: String,
    ) -> Result<(), String> {
        let cursors = self
            .local
            .append(&[Event::ClientToolResult {
                call,
                principal: self.principal.clone(),
                outcome: if succeeded {
                    Outcome::Succeeded
                } else {
                    Outcome::Failed
                },
                output,
            }])
            .await
            .map_err(|fenced| fenced.to_string())?;
        self.caller_events.extend(cursors);
        Ok(())
    }
}

/// A fresh, private directory for one turn's log and effect ledger.
pub fn turn_dir(prefix: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
}

#[cfg(test)]
#[path = "host_turn/tests/compaction.rs"]
mod compaction_tests;

#[cfg(test)]
#[path = "host_turn/tests.rs"]
mod tests;
