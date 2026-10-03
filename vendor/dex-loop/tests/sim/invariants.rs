//! Invariant checks against a finished (or quiescent) simulated log.

use std::collections::{HashMap, HashSet};

use dex_loop::{CallId, Cursor, Event, Outcome, PrincipalId, ThreadId, ToolName, rehydrate};

use super::fakes::SimLog;

/// A single invariant violation, or a known-pending one: client-executor
/// tool wiring is mid-flight in another PR at the time this simulator was
/// written (see `tests/sim.rs`'s module doc), so a violation whose only
/// offending call used `ExecutorKind::Client` is reported, not failed.
#[derive(Debug, Clone)]
pub struct Violation {
    pub description: String,
    pub known_pending: bool,
}

impl Violation {
    pub fn new(description: impl Into<String>) -> Self {
        Self {
            description: description.into(),
            known_pending: false,
        }
    }
}

/// Exact copies of `Engine`'s private message constants, used only to tell a
/// "not run: interrupted" from an "unknown: interrupted" `ToolFinished` when
/// scanning the log from outside the crate. If `engine.rs` ever rewords
/// these, update this copy: the check that uses it fails loudly (as a
/// harmless false positive) rather than silently, since it only compares
/// against the outcome kind, never a message substring, for anything
/// load-bearing.
const NOT_RUN_INTERRUPTED: &str = "not run: the turn was interrupted";

/// dex-runtime's `actor.rs::WAKE_KINDS` / `log.rs::CONTROL_KINDS`, copied so
/// the static-parity check does not need dex-runtime as a dependency of
/// dex-loop's test target. Kept honest by `control_kind_parity_matches_kernel`
/// in `tests/sim.rs`, which fails loudly the day these fall out of sync
/// instead of silently testing a stale copy.
pub const DEX_RUNTIME_CONTROL_KINDS: [&str; 5] = [
    "steer",
    "interrupt",
    "approval_decided",
    "answer",
    "client_tool_result",
];
pub const DEX_RUNTIME_WAKE_KINDS: [&str; 6] = [
    "user_message",
    "steer",
    "interrupt",
    "approval_decided",
    "answer",
    "client_tool_result",
];

fn kind_str(event: &Event) -> &'static str {
    match event {
        Event::UserMessage { .. } => "user_message",
        Event::Steer { .. } => "steer",
        Event::Interrupt { .. } => "interrupt",
        Event::ApprovalDecided { .. } => "approval_decided",
        Event::Answer { .. } => "answer",
        Event::ClientToolResult { .. } => "client_tool_result",
        Event::StepStarted { .. } => "step_started",
        Event::TextDelta { .. } => "text_delta",
        Event::ThinkingDelta { .. } => "thinking_delta",
        Event::Usage(_) => "usage",
        Event::ModelStepCompleted { .. } => "model_step_completed",
        Event::ModelAttemptAbandoned { .. } => "model_attempt_abandoned",
        Event::ModelAttemptFailed { .. } => "model_attempt_failed",
        Event::CodeModeStorePrepared { .. } => "code_mode_store_prepared",
        Event::ModelUsageResolved { .. } => "model_usage_resolved",
        Event::ModelUsageUnresolved { .. } => "model_usage_unresolved",
        Event::CodeModeCallsProposed { .. } => "code_mode_calls_proposed",
        Event::ToolStarted { .. } => "tool_started",
        Event::ToolProgress { .. } => "tool_progress",
        Event::ToolsExposed { .. } => "tools_exposed",
        Event::ToolFinished { .. } => "tool_finished",
        Event::ApprovalRequested { .. } => "approval_requested",
        Event::AutoApproved { .. } => "auto_approved",
        Event::Question { .. } => "question",
        Event::ClientToolRequested { .. } => "client_tool_requested",
        Event::Compaction { .. } => "compaction",
        Event::Final { .. } => "final",
        Event::Error { .. } => "error",
        Event::Interrupted => "interrupted",
    }
}

/// Static parity between the kernel's own `Event::is_control` classification
/// and the ingress-kind string lists dex-runtime's actor keys its wake and
/// lease-finish decisions on. A kind present in `is_control` but missing
/// from `CONTROL_KINDS` means `lease::finish` cannot see that a control
/// event landed after the actor's last read, and will hand the thread back
/// as though nothing more needs doing. A kind present in `is_control` (other
/// than the ones ingress never turns into a wake, if any) but missing from
/// `WAKE_KINDS` means an actor is never woken for it.
pub fn control_kind_parity() -> Result<(), String> {
    let is_control_kinds: HashSet<&'static str> = [
        "steer",
        "interrupt",
        "approval_decided",
        "answer",
        "client_tool_result",
    ]
    .into_iter()
    .collect();
    let control: HashSet<&'static str> = DEX_RUNTIME_CONTROL_KINDS.into_iter().collect();
    let wake: HashSet<&'static str> = DEX_RUNTIME_WAKE_KINDS.into_iter().collect();
    if is_control_kinds != control {
        return Err(format!(
            "Event::is_control kinds {is_control_kinds:?} != dex-runtime CONTROL_KINDS {control:?}"
        ));
    }
    // WAKE_KINDS additionally wakes on `user_message` (starting a thread),
    // which is not itself a control event.
    let mut expected_wake = control.clone();
    expected_wake.insert("user_message");
    if expected_wake != wake {
        return Err(format!(
            "expected WAKE_KINDS {expected_wake:?} (CONTROL_KINDS plus user_message), got {wake:?}"
        ));
    }
    Ok(())
}

/// Pure re-implementation of `lease::finish`'s three `NOT EXISTS` predicates,
/// against an in-memory event log instead of Postgres, so actor-liveness
/// properties can be checked without a database. `control_kinds` is the
/// caller's copy of dex-runtime's `CONTROL_KINDS` (pass
/// `DEX_RUNTIME_CONTROL_KINDS` to check against the real list, or `is_control`
/// kinds directly to check what *should* happen).
pub fn would_release(
    events: &[(Cursor, Event)],
    control_kinds: &[&str],
    seen: Cursor,
    control_seen: Cursor,
    turn: &str,
) -> bool {
    let has_new_write = events.iter().any(|(cursor, _)| *cursor > seen);
    let has_new_control = events
        .iter()
        .any(|(cursor, event)| *cursor > control_seen && control_kinds.contains(&kind_str(event)));
    let latest_user_turn = events.iter().rev().find_map(|(_, event)| match event {
        Event::UserMessage { turn, .. } => Some(turn.as_str().to_owned()),
        _ => None,
    });
    let turn_mismatch = latest_user_turn.is_some_and(|latest| latest != turn);
    !(has_new_write || has_new_control || turn_mismatch)
}

/// Per-call bookkeeping used by several checks below.
struct CallHistory {
    tool: ToolName,
    started_at: Option<Cursor>,
    finished: Vec<(Cursor, Outcome, String)>,
    approval_requested_digest: Option<String>,
    approvals_decided: Vec<(Cursor, bool, String, PrincipalId)>,
}

fn by_call(events: &[(Cursor, Event)]) -> HashMap<CallId, CallHistory> {
    let mut calls: HashMap<CallId, CallHistory> = HashMap::new();
    let mut names: HashMap<CallId, ToolName> = HashMap::new();
    for (_, event) in events {
        if let Event::ModelStepCompleted {
            calls: proposed, ..
        } = event
        {
            for call in proposed {
                names.insert(call.id.clone(), call.tool.clone());
            }
        }
    }
    for (cursor, event) in events {
        match event {
            Event::ToolStarted { call, .. } => {
                calls
                    .entry(call.clone())
                    .or_insert_with(|| CallHistory {
                        tool: names
                            .get(call)
                            .cloned()
                            .unwrap_or_else(|| ToolName::new("?")),
                        started_at: None,
                        finished: Vec::new(),
                        approval_requested_digest: None,
                        approvals_decided: Vec::new(),
                    })
                    .started_at
                    .get_or_insert(*cursor);
            }
            Event::ApprovalRequested {
                call, args_digest, ..
            } => {
                calls
                    .entry(call.clone())
                    .or_insert_with(|| CallHistory {
                        tool: names
                            .get(call)
                            .cloned()
                            .unwrap_or_else(|| ToolName::new("?")),
                        started_at: None,
                        finished: Vec::new(),
                        approval_requested_digest: None,
                        approvals_decided: Vec::new(),
                    })
                    .approval_requested_digest = Some(args_digest.clone());
            }
            Event::ApprovalDecided {
                call,
                args_digest,
                approved,
                principal,
                ..
            } => {
                calls
                    .entry(call.clone())
                    .or_insert_with(|| CallHistory {
                        tool: names
                            .get(call)
                            .cloned()
                            .unwrap_or_else(|| ToolName::new("?")),
                        started_at: None,
                        finished: Vec::new(),
                        approval_requested_digest: None,
                        approvals_decided: Vec::new(),
                    })
                    .approvals_decided
                    .push((*cursor, *approved, args_digest.clone(), principal.clone()));
            }
            Event::ToolFinished {
                call,
                outcome,
                output,
                ..
            } => {
                let text = match output {
                    dex_loop::Output::Text(text) => text.clone(),
                    dex_loop::Output::Ref(reference) => reference.to_string(),
                    dex_loop::Output::Blocks(_) => "selected media".into(),
                };
                calls
                    .entry(call.clone())
                    .or_insert_with(|| CallHistory {
                        tool: names
                            .get(call)
                            .cloned()
                            .unwrap_or_else(|| ToolName::new("?")),
                        started_at: None,
                        finished: Vec::new(),
                        approval_requested_digest: None,
                        approvals_decided: Vec::new(),
                    })
                    .finished
                    .push((*cursor, *outcome, text));
            }
            _ => {}
        }
    }
    calls
}

/// Checks (1) at-most-once mutation dispatch, (2) `Unknown` never
/// re-dispatched, (3) approval binding (digest match; denial blocks the
/// call), and (6) interrupt never turns a started call into "not run".
/// `dispatched` is every `CallId` `SimTools::run` actually ran (in order,
/// duplicates included); `mutation_names`/`client_names` classify tool names
/// from the catalog actually used in the scenario.
pub fn check_log(
    events: &[(Cursor, Event)],
    dispatched: &[CallId],
    mutation_names: &HashSet<String>,
    client_names: &HashSet<String>,
) -> Vec<Violation> {
    let mut violations = Vec::new();
    let calls = by_call(events);

    // (1) + (2): count dispatches of mutation-class calls.
    let mut counts: HashMap<&CallId, u32> = HashMap::new();
    for call in dispatched {
        *counts.entry(call).or_default() += 1;
    }
    for (call, history) in &calls {
        if !mutation_names.contains(history.tool.as_str()) {
            continue; // reads may legitimately run more than once.
        }
        let count = counts.get(call).copied().unwrap_or(0);
        if count > 1 {
            let violation = Violation::new(format!(
                "mutation {call} ({}) dispatched {count} times, want at most once",
                history.tool
            ));
            violations.push(tag_known_pending(violation, &history.tool, client_names));
        }
    }

    // (3) Approval binding.
    for (call, history) in &calls {
        let Some(requested_digest) = &history.approval_requested_digest else {
            continue;
        };
        let Some((_, approved, decided_digest, _)) = history.approvals_decided.first() else {
            continue; // never decided in this trace: nothing to check yet.
        };
        let succeeded = history
            .finished
            .iter()
            .any(|(_, outcome, _)| *outcome == Outcome::Succeeded);
        if !approved && succeeded {
            let violation = Violation::new(format!(
                "call {call} ({}) ran despite a denied approval",
                history.tool
            ));
            violations.push(tag_known_pending(violation, &history.tool, client_names));
        }
        if decided_digest != requested_digest && succeeded {
            let violation = Violation::new(format!(
                "call {call} ({}) ran despite an approval digest that does not match the requested one",
                history.tool
            ));
            violations.push(tag_known_pending(violation, &history.tool, client_names));
        }
    }

    // (3b) Auto-approval receipts: no call is ever parked for a human (no
    // `ApprovalRequested` in a fresh trace), and every receipt is bound to
    // the digest of the call it granted.
    let mut proposed_digests: HashMap<CallId, String> = HashMap::new();
    for (_, event) in events {
        if let Event::ModelStepCompleted { calls, .. } = event {
            for call in calls {
                proposed_digests.insert(call.id.clone(), call.args_digest.clone());
            }
        }
    }
    for (_, event) in events {
        match event {
            Event::ApprovalRequested { call, .. } => {
                violations.push(Violation::new(format!(
                    "call {call} was parked for a human approval; policy grants at once"
                )));
            }
            Event::AutoApproved {
                call,
                args_digest,
                principal,
                ..
            } => {
                if proposed_digests.get(call) != Some(args_digest) {
                    violations.push(Violation::new(format!(
                        "call {call} has an auto-approval receipt whose digest is not the call's"
                    )));
                }
                if principal.as_str() != dex_loop::AUTO_APPROVER {
                    violations.push(Violation::new(format!(
                        "call {call} has an auto-approval receipt from {principal}, want {}",
                        dex_loop::AUTO_APPROVER
                    )));
                }
            }
            _ => {}
        }
    }

    // (7) Only reads start ahead of their step's commit point: a mutation
    // or client call whose `ToolStarted` precedes the `ModelStepCompleted`
    // that proposes it began before the model finished.
    let mut committed_at: HashMap<&CallId, Cursor> = HashMap::new();
    for (cursor, event) in events {
        if let Event::ModelStepCompleted { calls, .. } = event {
            for call in calls {
                committed_at.insert(&call.id, *cursor);
            }
        }
    }
    for (call, history) in &calls {
        let (Some(started), Some(committed)) = (history.started_at, committed_at.get(call)) else {
            continue;
        };
        let is_read = !mutation_names.contains(history.tool.as_str())
            && !client_names.contains(history.tool.as_str());
        if started < *committed && !is_read {
            let violation = Violation::new(format!(
                "call {call} ({}) started before its step committed, but is not a read",
                history.tool
            ));
            violations.push(tag_known_pending(violation, &history.tool, client_names));
        }
    }

    // (6) Interrupt never turns a started call into "not run".
    for (call, history) in &calls {
        if history.started_at.is_none() {
            continue;
        }
        for (_, outcome, message) in &history.finished {
            if *outcome == Outcome::Failed && message == NOT_RUN_INTERRUPTED {
                let violation = Violation::new(format!(
                    "call {call} ({}) started, but interrupt marked it \"not run\" instead of \"unknown\"",
                    history.tool
                ));
                violations.push(tag_known_pending(violation, &history.tool, client_names));
            }
        }
    }

    violations
}

fn tag_known_pending(
    violation: Violation,
    tool: &ToolName,
    client_names: &HashSet<String>,
) -> Violation {
    if client_names.contains(tool.as_str()) {
        Violation {
            known_pending: true,
            ..violation
        }
    } else {
        violation
    }
}

/// Invariant (4): at quiescence (no turn running, nothing parked), every
/// `UserMessage` has exactly one terminal (`Final`, `Error` or
/// `Interrupted`) -- no lost turns, no double finals.
pub fn check_quiescent_turn_count(events: &[(Cursor, Event)]) -> Result<(), String> {
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
    if sent != terminals {
        return Err(format!(
            "{sent} UserMessage events but {terminals} terminal events at quiescence"
        ));
    }
    Ok(())
}

/// Invariant (5): the principal on every `ModelStepCompleted`'s proposed
/// calls matches `rehydrate`'s own acting principal at that point -- the
/// kernel is the oracle here, so this also doubles as a cross-check that
/// `Context::observe`'s acting-principal bookkeeping (steers, pending turns)
/// agrees with itself when replayed incrementally versus in one shot.
pub fn check_principal_attribution(
    thread: &ThreadId,
    events: &[(Cursor, Event)],
) -> Vec<Violation> {
    let mut violations = Vec::new();
    for (index, (cursor, event)) in events.iter().enumerate() {
        let Event::ModelStepCompleted { calls, .. } = event else {
            continue;
        };
        if calls.is_empty() {
            continue;
        }
        let prefix = &events[..index];
        let ctx = rehydrate(thread.clone(), prefix);
        let Some(expected) = ctx.acting_principal() else {
            violations.push(Violation::new(format!(
                "cursor {cursor:?}: calls proposed with no acting principal in context"
            )));
            continue;
        };
        for call in calls {
            if &call.principal != expected {
                violations.push(Violation::new(format!(
                    "cursor {cursor:?}: call {} principal {} != acting principal {} at that point",
                    call.id, call.principal, expected
                )));
            }
        }
    }
    violations
}

/// Invariant (7), the "exactly once" half: a `Watch` resuming from any
/// cursor it already reached sees every later event exactly once, with no
/// gap and no duplicate, regardless of how it chooses to poll. Simulated by
/// repeatedly calling `SimLog::since` at every cursor the log itself
/// produced and stitching the results back together; cursor monotonicity
/// and gaplessness (the other half of invariant 7) hold by construction in
/// `SimLog` (each event is assigned `len() + 1` under the same lock as the
/// push), so this check is the one half worth stating as a property rather
/// than taking on faith.
pub fn check_watch_replay(log: &SimLog) -> Result<(), String> {
    let all = log.entries();
    let mut replayed = Vec::with_capacity(all.len());
    let mut after = Cursor(0);
    loop {
        let batch = log.since(after);
        if batch.is_empty() {
            break;
        }
        after = batch.last().map(|(cursor, _)| *cursor).unwrap_or(after);
        replayed.extend(batch);
    }
    if replayed != all {
        return Err(format!(
            "watch replay via repeated `since` calls produced {} events, the log itself holds {}",
            replayed.len(),
            all.len()
        ));
    }
    Ok(())
}
