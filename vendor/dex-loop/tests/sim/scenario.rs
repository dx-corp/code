//! The host-action driver: a sequence of `Action`s interpreted against one
//! (or two, for the lease-fencing property) simulated replicas, checked with
//! `invariants::check_log` after every `Tick` and once more at quiescence.

use std::collections::HashSet;
use std::time::Duration;

use dex_loop::{
    ApprovalId, Budget, CallId, CancellationToken, Engine, Event, ExecutorKind, Exit,
    GovernanceClass, Lexicon, Outcome, PrincipalId, ThreadId, ToolName, ToolSpec, TurnId,
    rehydrate,
};
use proptest::prelude::*;

use super::fakes::{CrashBudget, SimEffects, SimLog, SimModel, SimTools, adversarial_payload};
use super::invariants::{self, Violation};

type SimEngine = Engine<SimLog, SimModel, SimTools, SimEffects, Lexicon>;

fn thread() -> ThreadId {
    ThreadId {
        org: "org-sim".into(),
        workspace: "ws-sim".into(),
        thread: "thread-sim".into(),
    }
}

fn budget() -> Budget {
    Budget {
        max_steps: 20,
        max_tokens: u64::MAX,
        max_cost_micros: u64::MAX,
        wall: Duration::from_secs(5),
    }
}

/// alice and bob may decide approvals in these scenarios; mallory may not.
/// Kernel-level approvals carry no authorization check of their own (only
/// `call` + `approval` + `args_digest` are verified, against the durable
/// `ApprovalRequested` row -- see `dex_runtime::ingress::Ingress::approve`):
/// authorizing the *principal* is a host responsibility performed before an
/// `ApprovalDecided` is ever appended. This driver plays that host role, so
/// `ApprovalChoice::FromUnauthorized` exercises "the host correctly refuses
/// to forward it", not "the kernel independently checks and refuses it".
fn principal(index: u8) -> PrincipalId {
    match index % 3 {
        0 => PrincipalId::new("alice"),
        1 => PrincipalId::new("bob"),
        _ => PrincipalId::new("mallory"),
    }
}

fn authorized(principal: &PrincipalId) -> bool {
    principal.as_str() != "mallory"
}

fn catalog() -> Vec<ToolSpec> {
    let spec =
        |name: &str, read_only: bool, governance: GovernanceClass, executor: ExecutorKind| {
            ToolSpec {
                description: String::new(),
                name: ToolName::new(name),
                label: format!("Label for {name}"),
                schema: serde_json::json!({"type": "object"}),
                read_only,
                core: true,
                governance,
                executor,
            }
        };
    vec![
        spec(
            "reader",
            true,
            GovernanceClass::Plain,
            ExecutorKind::InProcess,
        ),
        spec(
            "mutator",
            false,
            GovernanceClass::Approval,
            ExecutorKind::ToolExecutor,
        ),
        spec(
            "client.read",
            true,
            GovernanceClass::Plain,
            ExecutorKind::Client,
        ),
        spec(
            "client.write",
            false,
            GovernanceClass::Approval,
            ExecutorKind::Client,
        ),
    ]
}

fn mutation_names() -> HashSet<String> {
    ["mutator", "client.write"]
        .into_iter()
        .map(String::from)
        .collect()
}

fn client_names() -> HashSet<String> {
    ["client.read", "client.write"]
        .into_iter()
        .map(String::from)
        .collect()
}

// ---------------------------------------------------------------- Action

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Payload {
    Plain,
    Nul,
    Unicode,
    Huge,
}

impl Payload {
    fn text(self, seed: u64) -> String {
        let bucket = match self {
            Payload::Plain => 0,
            Payload::Nul => 1,
            Payload::Unicode => 2,
            Payload::Huge => 3,
        };
        adversarial_payload(seed.wrapping_add(bucket))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalChoice {
    Correct,
    Denied,
    WrongDigest,
    WrongApprovalId,
    FromUnauthorized,
    /// Decides correctly, then submits the identical decision again --
    /// duplicate delivery of the same `ApprovalDecided`.
    Duplicate,
}

#[derive(Clone, Debug)]
pub enum Action {
    Send {
        principal: u8,
        payload: Payload,
        /// Reuse the most recent turn id instead of minting a new one:
        /// duplicate `Send` delivery, which a real host's ingress makes an
        /// idempotent no-op (see `run_actions`'s `used_turns`).
        reuse_turn: bool,
    },
    Steer {
        principal: u8,
        payload: Payload,
    },
    Interrupt {
        principal: u8,
    },
    Approve(ApprovalChoice),
    Answer {
        principal: u8,
        payload: Payload,
    },
    ClientResult {
        principal: u8,
        succeed: bool,
    },
    VanishMutator,
    RestoreMutator,
    DenyMutator,
    AllowMutator,
    /// Arms a crash at the `n`th port operation of the *next* `Tick` only
    /// (`n` taken mod a small cap so shrinking stays meaningful).
    CrashAt(u8),
    Tick,
}

fn payload_strategy() -> impl Strategy<Value = Payload> {
    prop_oneof![
        Just(Payload::Plain),
        Just(Payload::Nul),
        Just(Payload::Unicode),
        Just(Payload::Huge),
    ]
}

fn approval_choice_strategy() -> impl Strategy<Value = ApprovalChoice> {
    prop_oneof![
        Just(ApprovalChoice::Correct),
        Just(ApprovalChoice::Denied),
        Just(ApprovalChoice::WrongDigest),
        Just(ApprovalChoice::WrongApprovalId),
        Just(ApprovalChoice::FromUnauthorized),
        Just(ApprovalChoice::Duplicate),
    ]
}

/// One `Action`. `proptest`'s shrinker works on this directly: on failure it
/// drops and simplifies elements of the generated `Vec<Action>` until no
/// smaller trace still reproduces the violation.
pub fn action() -> impl Strategy<Value = Action> {
    prop_oneof![
        3 => (0u8..3, payload_strategy(), any::<bool>())
            .prop_map(|(principal, payload, reuse_turn)| Action::Send { principal, payload, reuse_turn }),
        1 => (0u8..3, payload_strategy()).prop_map(|(principal, payload)| Action::Steer { principal, payload }),
        1 => (0u8..3).prop_map(|principal| Action::Interrupt { principal }),
        2 => approval_choice_strategy().prop_map(Action::Approve),
        1 => (0u8..3, payload_strategy()).prop_map(|(principal, payload)| Action::Answer { principal, payload }),
        1 => (0u8..3, any::<bool>()).prop_map(|(principal, succeed)| Action::ClientResult { principal, succeed }),
        1 => Just(Action::VanishMutator),
        1 => Just(Action::RestoreMutator),
        1 => Just(Action::DenyMutator),
        1 => Just(Action::AllowMutator),
        2 => (0u8..12).prop_map(Action::CrashAt),
        6 => Just(Action::Tick),
    ]
}

pub fn action_sequence() -> impl Strategy<Value = Vec<Action>> {
    proptest::collection::vec(action(), 1..30)
}

// ---------------------------------------------------------------- Interpreter

fn find_approval_request(log: &SimLog, approval: &ApprovalId) -> Option<(CallId, String)> {
    log.entries()
        .into_iter()
        .rev()
        .find_map(|(_, event)| match event {
            Event::ApprovalRequested {
                call,
                approval: a,
                args_digest,
                ..
            } if &a == approval => Some((call, args_digest)),
            _ => None,
        })
}

/// `true` once every `UserMessage` the log holds has its own terminal
/// (`Final`, `Error` or `Interrupted`): the ground truth this driver uses for
/// "nothing more will ever happen on its own", computed from the log alone
/// (not from `Context`, whose `status()`/`open_step()` are crate-private) so
/// it also catches the case where finishing turn A silently promotes an
/// already-queued turn B (`Context::begin_next_pending_turn`) in the same
/// `emit()` that ended A: B's `UserMessage` was already in the log, so the
/// message/terminal counts stay unequal until B gets its own terminal too,
/// even though the `Engine::run` call that ended A already returned
/// `Exit::Done`.
fn log_settled(log: &SimLog) -> bool {
    let events = log.entries();
    let sent = events
        .iter()
        .filter(|(_, event)| matches!(event, Event::UserMessage { .. }))
        .count();
    let terminals = events
        .iter()
        .filter(|(_, event)| {
            matches!(
                event,
                Event::Final { .. } | Event::Error { .. } | Event::Interrupted
            )
        })
        .count();
    sent == terminals
}

/// One drive iteration: rehydrate fresh from the current log (never carry a
/// `Context` over between ticks -- `dex_runtime::actor::Runtime::drive` does
/// not either, since `Engine::run` only ever reads control-kind events
/// mid-flight, never `UserMessage`; a `Context` held across two separate
/// `run()` calls would silently miss a `Send` that landed in between), run
/// one attempt, and check invariant 8 (`rehydrate(log) == live state`)
/// against the resulting log.
async fn tick(
    thread: &ThreadId,
    log: &SimLog,
    model: &SimModel,
    tools: &SimTools,
    effects: &SimEffects,
    crash: CrashBudget,
) -> Option<Result<Exit, dex_loop::Fenced>> {
    let mut c = rehydrate(thread.clone(), &log.entries());
    let engine: SimEngine = Engine::new(
        log.for_replica(log.generation(), crash.clone()),
        model.clone(),
        tools.with_crash(crash.clone()),
        effects.for_replica(effects.fence().generation(), crash),
        Lexicon::default(),
        budget(),
    );
    let cancel = CancellationToken::new();
    match tokio::time::timeout(Duration::from_secs(60), engine.run(&mut c, &cancel)).await {
        Ok(result) => {
            if let Ok(exit) = &result {
                let full = rehydrate(thread.clone(), &log.entries());
                assert_eq!(
                    full, c,
                    "rehydrate(full log) must equal the live context right after a completed `Engine::run` \
                     (exit {exit:?})"
                );
            }
            Some(result)
        }
        Err(_timeout) => {
            // Simulated crash: whatever already landed in the log stays
            // durable; `c`'s in-memory state (possibly behind the log by
            // whatever this attempt never got to `observe`) is discarded.
            None
        }
    }
}

/// Interprets `actions` against one replica, then drains to quiescence
/// (bounded) and checks every invariant. `seed` drives the model's and
/// tools' adversarial choices (see `fakes.rs`); it is independent of the
/// `Action` sequence itself, so proptest shrinks the sequence without also
/// needing to shrink the seed.
pub async fn run_actions(seed: u64, actions: &[Action]) -> Vec<Violation> {
    let thread = thread();
    let crash_free = CrashBudget::none();
    let log = SimLog::new(crash_free.clone());
    let effects = SimEffects::new(crash_free.clone());
    let tools = SimTools::new(catalog(), seed, crash_free.clone());
    let model = SimModel::new(seed);

    let mut pending_exit: Option<Exit> = None;
    let mut used_turns: HashSet<String> = HashSet::new();
    let mut turn_counter: u32 = 0;
    let mut next_crash: Option<u8> = None;
    let mut gave_up = false;

    for action in actions {
        match action.clone() {
            Action::Send {
                principal: p,
                payload,
                reuse_turn,
            } => {
                let turn = if reuse_turn && turn_counter > 0 {
                    format!("t{turn_counter}")
                } else {
                    turn_counter += 1;
                    format!("t{turn_counter}")
                };
                if used_turns.insert(turn.clone()) {
                    log.host_append(Event::UserMessage {
                        turn: TurnId::new(turn),
                        message_id: None,
                        principal: principal(p),
                        text: payload.text(seed),
                        attachments: Vec::new(),
                        client_tools: Vec::new(),
                        authorized_tools: Vec::new(),
                        approval_mode: dex_loop::ApprovalMode::Interactive,
                    });
                }
                // else: a real ingress makes a repeated turn id a no-op.
            }
            Action::Steer {
                principal: p,
                payload,
            } => {
                log.host_append(Event::Steer {
                    principal: principal(p),
                    text: payload.text(seed),
                });
            }
            Action::Interrupt { principal: p } => {
                log.host_append(Event::Interrupt {
                    principal: principal(p),
                });
            }
            Action::Approve(choice) => {
                if let Some(Exit::Parked(approval)) = &pending_exit
                    && let Some((call, requested_digest)) = find_approval_request(&log, approval)
                {
                    let event =
                        |digest: String, approval: ApprovalId, who: PrincipalId, approved: bool| {
                            Event::ApprovalDecided {
                                call: call.clone(),
                                approval,
                                args_digest: digest,
                                approved,
                                principal: who,
                            }
                        };
                    match choice {
                        ApprovalChoice::Correct => {
                            log.host_append(event(
                                requested_digest,
                                approval.clone(),
                                principal(0),
                                true,
                            ));
                        }
                        ApprovalChoice::Denied => {
                            log.host_append(event(
                                requested_digest,
                                approval.clone(),
                                principal(0),
                                false,
                            ));
                        }
                        ApprovalChoice::WrongDigest => {
                            log.host_append(event(
                                format!("{requested_digest}00"),
                                approval.clone(),
                                principal(0),
                                true,
                            ));
                        }
                        ApprovalChoice::WrongApprovalId => {
                            log.host_append(event(
                                requested_digest,
                                ApprovalId::new(format!("{approval}-bogus")),
                                principal(0),
                                true,
                            ));
                        }
                        ApprovalChoice::FromUnauthorized => {
                            // The host refuses to forward this: nothing
                            // appended (see the module doc on `principal`).
                            assert!(
                                !authorized(&principal(2)),
                                "mallory must not be authorized in this scenario"
                            );
                        }
                        ApprovalChoice::Duplicate => {
                            log.host_append(event(
                                requested_digest.clone(),
                                approval.clone(),
                                principal(0),
                                true,
                            ));
                            log.host_append(event(
                                requested_digest,
                                approval.clone(),
                                principal(0),
                                true,
                            ));
                        }
                    }
                }
            }
            Action::Answer {
                principal: p,
                payload,
            } => {
                if let Some(Exit::Asked(call)) = &pending_exit {
                    log.host_append(Event::Answer {
                        call: call.clone(),
                        principal: principal(p),
                        text: payload.text(seed),
                        confirmation_decision: dex_loop::ConfirmationDecision::Unspecified,
                        args_digest: String::new(),
                    });
                }
            }
            Action::ClientResult {
                principal: p,
                succeed,
            } => {
                if let Some(Exit::AwaitingClientTool(call)) = &pending_exit {
                    let outcome = if succeed {
                        Outcome::Succeeded
                    } else {
                        Outcome::Failed
                    };
                    log.host_append(Event::ClientToolResult {
                        call: call.clone(),
                        principal: principal(p),
                        outcome,
                        output: "client result".into(),
                    });
                }
            }
            Action::VanishMutator => tools.vanish("mutator"),
            Action::RestoreMutator => tools.restore("mutator"),
            Action::DenyMutator => tools.force_deny("mutator"),
            Action::AllowMutator => tools.clear_deny("mutator"),
            Action::CrashAt(n) => next_crash = Some(n),
            Action::Tick => {
                let crash = next_crash
                    .take()
                    .map(|n| CrashBudget::at(n as u64))
                    .unwrap_or_else(CrashBudget::none);
                match tick(&thread, &log, &model, &tools, &effects, crash).await {
                    Some(Ok(exit)) => pending_exit = Some(exit),
                    Some(Err(_fenced)) => {
                        // Single replica in this property: the generation
                        // never moves, so a genuine `Fenced` would itself be
                        // a bug worth surfacing rather than swallowing.
                        pending_exit = None;
                    }
                    None => pending_exit = None, // crashed; rehydrate next time.
                }
            }
        }
    }

    // Drain to quiescence with no further host actions and no crash,
    // bounded so a real livelock fails the test instead of hanging it.
    let mut final_parked = false;
    for _ in 0..64 {
        if log_settled(&log) {
            break;
        }
        match tick(&thread, &log, &model, &tools, &effects, CrashBudget::none()).await {
            Some(Ok(exit)) => {
                let parked = matches!(
                    exit,
                    Exit::Parked(_) | Exit::Asked(_) | Exit::AwaitingClientTool(_)
                );
                pending_exit = Some(exit);
                if parked {
                    final_parked = true;
                    break;
                }
            }
            _ => {
                gave_up = true;
                break;
            }
        }
    }
    if !log_settled(&log) && !final_parked {
        gave_up = true;
    }

    let mut violations = invariants::check_log(
        &log.entries(),
        &tools.dispatches(),
        &mutation_names(),
        &client_names(),
    );
    violations.extend(invariants::check_principal_attribution(
        &thread,
        &log.entries(),
    ));
    if let Err(message) = invariants::check_watch_replay(&log) {
        violations.push(Violation::new(message));
    }
    if gave_up {
        violations.push(Violation::new(format!(
            "drain loop did not reach quiescence within 64 ticks (last exit: {pending_exit:?})"
        )));
    } else if !final_parked
        && let Err(message) = invariants::check_quiescent_turn_count(&log.entries())
    {
        violations.push(Violation::new(message));
    }
    violations
}

// ---------------------------------------------------------------- Bounded seeds

/// Generates a small, seed-derived action sequence for the plain
/// seed-sweep entry point (`dst_bounded_seeds` / `DEX_SIM_SEEDS` soak): every
/// seed is reproducible on its own, without going through `proptest`.
pub fn actions_for_seed(seed: u64) -> Vec<Action> {
    use rand::Rng;
    use rand::SeedableRng;
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let len = rng.gen_range(1..30);
    let mut actions = Vec::with_capacity(len);
    for _ in 0..len {
        actions.push(match rng.gen_range(0..12) {
            0..=2 => Action::Send {
                principal: rng.gen_range(0..3),
                payload: [
                    Payload::Plain,
                    Payload::Nul,
                    Payload::Unicode,
                    Payload::Huge,
                ][rng.gen_range(0..4)],
                reuse_turn: rng.gen_bool(0.2),
            },
            3 => Action::Steer {
                principal: rng.gen_range(0..3),
                payload: Payload::Plain,
            },
            4 => Action::Interrupt {
                principal: rng.gen_range(0..3),
            },
            5 | 6 => Action::Approve(
                [
                    ApprovalChoice::Correct,
                    ApprovalChoice::Denied,
                    ApprovalChoice::WrongDigest,
                    ApprovalChoice::WrongApprovalId,
                    ApprovalChoice::FromUnauthorized,
                    ApprovalChoice::Duplicate,
                ][rng.gen_range(0..6)],
            ),
            7 => Action::Answer {
                principal: rng.gen_range(0..3),
                payload: Payload::Plain,
            },
            8 => Action::ClientResult {
                principal: rng.gen_range(0..3),
                succeed: rng.gen_bool(0.5),
            },
            9 => {
                if rng.gen_bool(0.5) {
                    Action::VanishMutator
                } else {
                    Action::RestoreMutator
                }
            }
            10 => Action::CrashAt(rng.gen_range(0..12)),
            _ => Action::Tick,
        });
    }
    actions.push(Action::Tick);
    actions
}

pub async fn run_seed(seed: u64) -> Vec<Violation> {
    run_actions(seed, &actions_for_seed(seed)).await
}
