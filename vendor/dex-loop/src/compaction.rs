//! Keeping the model's context bounded.

use std::future::Future;

use crate::context::{Context, Entry, Message};
use crate::event::{Cursor, Usage};

/// A planned compaction: history up to and including `covers_to` becomes
/// `summary`. The engine appends it as `Event::Compaction`, so rehydration
/// applies it the same way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Compaction {
    pub covers_to: Cursor,
    pub summary: String,
}

/// Decides, before each model call, whether to compact.
pub trait Compactor: Send + Sync {
    fn plan(&self, ctx: &Context) -> impl Future<Output = CompactionPlan> + Send;
}

/// An attempted summary may consume model usage even when no summary commits.
#[derive(Clone, Debug, Default)]
pub struct CompactionPlan {
    pub compaction: Option<Compaction>,
    pub usage: Usage,
}

#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub text: Option<String>,
    pub usage: Usage,
}

/// Never compacts.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoCompaction;

impl Compactor for NoCompaction {
    async fn plan(&self, _ctx: &Context) -> CompactionPlan {
        CompactionPlan::default()
    }
}

/// Writes the summary for a prefix of history (usually one model call).
pub trait Summarize: Send + Sync {
    /// `None` skips this compaction; the engine continues uncompacted.
    fn summarize(&self, ctx: &Context, entries: &[Entry]) -> impl Future<Output = Summary> + Send;
}

/// Compacts when history exceeds `max_bytes`. `new` keeps recent complete
/// call/result pairs; `for_turns` covers all completed assistant steps while
/// Context retains the current request, steers and attachments verbatim.
#[derive(Clone, Debug)]
pub struct Threshold<S> {
    max_bytes: usize,
    cut_policy: CutPolicy,
    summarizer: S,
}

#[derive(Clone, Copy, Debug)]
enum CutPolicy {
    KeepRecent(usize),
    CompletedTurnSteps,
}

impl<S: Summarize> Threshold<S> {
    pub fn new(max_bytes: usize, keep_recent: usize, summarizer: S) -> Self {
        Self {
            max_bytes,
            cut_policy: CutPolicy::KeepRecent(keep_recent),
            summarizer,
        }
    }

    /// Production turn compaction covers all completed assistant steps so
    /// signed thinking never replays beside a prefix it was not produced with.
    /// Current request, applied steers and attachments remain exact in Context.
    pub fn for_turns(max_bytes: usize, summarizer: S) -> Self {
        Self {
            max_bytes,
            cut_policy: CutPolicy::CompletedTurnSteps,
            summarizer,
        }
    }
}

impl<S: Summarize> Threshold<S> {
    /// Plans at 70% of the limit after a finished turn; running turns are untouched.
    pub async fn plan_after_turn(&self, ctx: &Context) -> CompactionPlan {
        if ctx.turn_running() || self.max_bytes == usize::MAX {
            return CompactionPlan::default();
        }
        // Do not repeatedly summarize a large prior summary after tiny turns.
        let new_bytes: usize = ctx
            .history()
            .iter()
            .filter(|entry| !matches!(entry.message, Message::Summary { .. }))
            .map(|entry| entry.message.size())
            .sum();
        if new_bytes < self.max_bytes / 10 {
            return CompactionPlan::default();
        }
        self.plan_at(ctx, self.max_bytes.saturating_mul(70) / 100)
            .await
    }

    async fn plan_at(&self, ctx: &Context, max_bytes: usize) -> CompactionPlan {
        // An unresolved call/attempt retains its exact request, arguments and
        // provider continuation. The engine also dispatches these before planning.
        if ctx.open_step().is_some() || ctx.open_attempt().is_some() {
            return CompactionPlan::default();
        }
        let history = ctx.history();
        let size: usize = history.iter().map(|entry| entry.message.size()).sum();
        if size <= max_bytes {
            return CompactionPlan::default();
        }
        let cut = match self.cut_policy {
            CutPolicy::KeepRecent(keep_recent) => cut_point(ctx, keep_recent),
            CutPolicy::CompletedTurnSteps => plan_cut(ctx, max_bytes).map(|cut| cut.covered.len()),
        };
        let Some(cut) = cut else {
            return CompactionPlan::default();
        };
        // Current user input (including steers) stays verbatim in Context;
        // summarize only the historical/completed material being replaced.
        let entries: Vec<_> = history[..cut].iter().filter(|entry| {
            !matches!(&entry.message, Message::User { turn, .. } if ctx.turn_running() && Some(turn) == ctx.turn())
        }).cloned().collect();
        if entries.is_empty() {
            return CompactionPlan::default();
        }
        let summary = self.summarizer.summarize(ctx, &entries).await;
        CompactionPlan {
            compaction: summary.text.map(|summary| Compaction {
                covers_to: history[cut - 1].cursor,
                summary,
            }),
            usage: summary.usage,
        }
    }
}

impl<S: Summarize> Compactor for Threshold<S> {
    async fn plan(&self, ctx: &Context) -> CompactionPlan {
        self.plan_at(ctx, self.max_bytes).await
    }
}

/// Where to cut history for a compaction, chosen by [`plan_cut`].
#[derive(Clone, Copy, Debug, PartialEq)]
struct Cut<'a> {
    /// The entries the summary replaces (a prefix of the history).
    covered: &'a [Entry],
    /// The cursor the `Compaction` event names: the last covered entry's.
    #[cfg(test)]
    covers_to: Cursor,
    /// Whether the cut fell inside the current turn (after a closed step)
    /// rather than at its start.
    #[cfg(test)]
    mid_turn: bool,
}

/// Picks a cut for a history over `max_bytes`, or `None` when nothing should
/// be compacted now.
///
/// - Never while a step's tool calls are unresolved (`open_step`): a cut
///   there would separate a tool call from its result.
/// - The kept part is only the user messages that follow the last assistant
///   or tool entry (the current turn's request and any steer), so the summary
///   is followed by what the model still has to answer. A cut at the start of
///   a turn keeps that turn's user message; a cut inside a turn (`mid_turn`)
///   falls right after a closed step and covers the turn's own steps so far.
/// - No assistant step is ever kept, so none is replayed under a summary it
///   was not produced beside. Pre-compaction continuation state belongs to
///   the old prefix. The next step starts a fresh assistant message after
///   the summary.
/// - Never leaves a `Message::Tool` as the first kept entry and never splits
///   entries that share a cursor.
fn plan_cut(ctx: &Context, max_bytes: usize) -> Option<Cut<'_>> {
    if ctx.open_step().is_some() || ctx.open_attempt().is_some() {
        return None;
    }
    let history = ctx.history();
    let total: usize = history.iter().map(|entry| entry.message.size()).sum();
    if total <= max_bytes {
        return None;
    }
    let cut = history
        .iter()
        .rposition(|entry| !matches!(entry.message, Message::User { .. }))?
        + 1;
    if !valid_cut(history, cut) {
        return None;
    }
    #[cfg(test)]
    let turn_start = ctx.turn().and_then(|turn| {
        history
            .iter()
            .position(|entry| matches!(&entry.message, Message::User { turn: t, .. } if t == turn))
    });
    Some(Cut {
        covered: &history[..cut],
        #[cfg(test)]
        covers_to: history[cut - 1].cursor,
        #[cfg(test)]
        mid_turn: turn_start.is_some_and(|start| start < cut),
    })
}

/// Whether history can be split before index `cut` (`cut == len` keeps
/// nothing): something precedes it that is more than a lone summary, the
/// last covered entry is a finished step (a tool result, or an assistant
/// message that made no calls), the first kept entry is not a tool result,
/// and the cut falls between two cursors.
fn valid_cut(history: &[Entry], cut: usize) -> bool {
    if cut == 0 || cut > history.len() {
        return false;
    }
    let only_summary = cut == 1 && matches!(history[0].message, Message::Summary { .. });
    let finished = match &history[cut - 1].message {
        Message::Tool { .. } => true,
        Message::Assistant { calls, .. } => calls.is_empty(),
        Message::Summary { .. } | Message::User { .. } => false,
    };
    let kept = history.get(cut);
    let splits_cursor = kept.is_some_and(|kept| kept.cursor == history[cut - 1].cursor);
    let orphans_result = kept.is_some_and(|kept| matches!(kept.message, Message::Tool { .. }));
    !only_summary && finished && !splits_cursor && !orphans_result
}

/// The latest index `cut` with at least `keep_recent` entries after it where
/// history can be split: the kept part must not start with a tool result
/// (it belongs to the assistant message before it), and the cut must fall
/// between two cursors so the compaction event can name it.
fn cut_point(ctx: &Context, keep_recent: usize) -> Option<usize> {
    let history = ctx.history();
    let latest = history.len().checked_sub(keep_recent)?;
    (1..=latest).rev().find(|&cut| {
        let first_kept = history.get(cut);
        let only_summary = cut == 1 && matches!(history[0].message, Message::Summary { .. });
        let splits_cursor = first_kept.is_some_and(|kept| kept.cursor == history[cut - 1].cursor);
        let orphans_result =
            first_kept.is_some_and(|kept| matches!(kept.message, Message::Tool { .. }));
        !only_summary && !splits_cursor && !orphans_result
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ApprovalId, ApprovalMode, ArtifactRef, CallId, Event, Outcome, Output, PrincipalId,
        ProposedCall, ThreadId, ToolName, TurnId, rehydrate,
    };
    use serde_json::json;

    fn thread() -> ThreadId {
        ThreadId {
            org: "org".into(),
            workspace: "ws".into(),
            thread: "thread".into(),
        }
    }
    fn user(turn: &str, text: &str) -> Event {
        Event::UserMessage {
            turn: TurnId::new(turn),
            message_id: None,
            principal: PrincipalId::new("alice"),
            text: text.into(),
            attachments: vec![ArtifactRef::new("doc@v1")],
            client_tools: vec![],
            authorized_tools: vec![],
            model_binding: None,
            approval_mode: ApprovalMode::Interactive,
        }
    }
    fn append(events: &mut Vec<(Cursor, Event)>, event: Event) {
        events.push((Cursor(events.len() as i64 + 1), event));
    }
    fn completed(events: &mut Vec<(Cursor, Event)>, id: &str, step: u32) {
        let control_through = Cursor(events.len() as i64);
        append(
            events,
            Event::StepStarted {
                step,
                control_through,
            },
        );
        append(
            events,
            Event::ModelStepCompleted {
                timing: None,
                served: None,
                step,
                text: "completed tool request".into(),
                calls: vec![ProposedCall::new(
                    CallId::new(id),
                    ToolName::new("read"),
                    json!({"exact": "args"}),
                    PrincipalId::new("alice"),
                )],
                reasoning: None,
            },
        );
        append(
            events,
            Event::ToolFinished {
                call: CallId::new(id),
                outcome: Outcome::Succeeded,
                output: Output::Text("result".into()),
                receipt: None,
                summary: None,
            },
        );
    }
    fn inputs(ctx: &Context) -> Vec<Message> {
        ctx.history()
            .iter()
            .filter(|entry| matches!(entry.message, Message::User { .. }))
            .map(|entry| entry.message.clone())
            .collect()
    }
    struct Fake;
    impl Summarize for Fake {
        async fn summarize(&self, _ctx: &Context, entries: &[Entry]) -> Summary {
            assert!(entries.iter().all(|entry| !matches!(&entry.message, Message::User { turn, .. } if turn.as_str() == "current")));
            Summary {
                text: Some("untrusted old completed work".into()),
                usage: Usage::default(),
            }
        }
    }

    #[test]
    fn repeated_compaction_retains_current_input_steers_and_suffix_replay() {
        let mut events = vec![];
        append(&mut events, user("old", "old constraint"));
        append(
            &mut events,
            Event::Final {
                text: "old done".into(),
            },
        );
        append(&mut events, user("current", "do not publish"));
        append(
            &mut events,
            Event::Steer {
                principal: PrincipalId::new("bob"),
                text: "keep within $10".into(),
            },
        );
        completed(&mut events, "one", 1);
        let mut warm = rehydrate(thread(), &events);
        let before = inputs(&warm);
        for (id, step) in [("two", 2), ("three", 3)] {
            let covers = events.last().expect("event").0;
            let event = Event::Compaction {
                covers_to_cursor: covers,
                summary: format!("summary through {}", covers.0),
            };
            append(&mut events, event.clone());
            warm.observe(events.last().expect("event").0, &event);
            assert_eq!(inputs(&warm), before.iter().filter(|message| matches!(message, Message::User { turn, .. } if turn.as_str() == "current")).cloned().collect::<Vec<_>>());
            assert!(
                warm.history()
                    .windows(2)
                    .all(|pair| pair[0].cursor <= pair[1].cursor)
            );
            assert_eq!(warm, rehydrate(thread(), &events));
            // Same suffix floor PgLog uses: current UserMessage precedes covers.
            assert_eq!(warm, rehydrate(thread(), &events[2..]));
            completed(&mut events, id, step);
            warm = rehydrate(thread(), &events);
        }
        append(&mut events, user("next", "new task"));
        append(
            &mut events,
            Event::Final {
                text: "current done".into(),
            },
        );
        let mut next = rehydrate(thread(), &events);
        assert_eq!(next.turn(), Some(&TurnId::new("next")));
        let covers = events[events.len() - 3].0;
        let event = Event::Compaction {
            covers_to_cursor: covers,
            summary: "all previous work".into(),
        };
        append(&mut events, event.clone());
        next.observe(events.last().expect("event").0, &event);
        assert_eq!(inputs(&next).len(), 1);
        assert!(matches!(&inputs(&next)[0], Message::User { text, .. } if text == "new task"));
        assert_eq!(next, rehydrate(thread(), &events));
    }

    #[test]
    fn queued_turn_after_compaction_requires_full_replay_and_keeps_exact_turn_state() {
        let mut events = vec![];
        append(&mut events, user("current", "do not publish"));
        let mut warm = rehydrate(thread(), &events);
        {
            let mut push = |event: Event| {
                append(&mut events, event.clone());
                warm.observe(events.last().expect("event").0, &event);
            };
            push(Event::ModelStepCompleted {
                timing: None,
                served: None,
                step: 1,
                text: "completed work".into(),
                calls: vec![],
                reasoning: None,
            });
            push(Event::Compaction {
                covers_to_cursor: Cursor(2),
                summary: "historical work".into(),
            });
            push(user("queued-one", "first queued request"));
            push(user("queued-two", "second queued request"));
            push(Event::Final {
                text: "current done".into(),
            });
            push(Event::Compaction {
                covers_to_cursor: Cursor(2),
                summary: "earlier inputs and work".into(),
            });
        }
        assert_eq!(warm.turn(), Some(&TurnId::new("queued-one")));
        assert_eq!(warm.status(), crate::context::Status::Running);
        assert_eq!(warm, rehydrate(thread(), &events));
        // The retired suffix starts at covers=2 and omits the current
        // UserMessage. Its Final then wrongly terminates queued-one.
        assert_ne!(warm, rehydrate(thread(), &events[1..]));
        let finish = Event::Final {
            text: "first queued done".into(),
        };
        append(&mut events, finish.clone());
        warm.observe(events.last().expect("event").0, &finish);
        assert_eq!(warm.turn(), Some(&TurnId::new("queued-two")));
        assert_eq!(warm.status(), crate::context::Status::Running);
        assert_eq!(warm, rehydrate(thread(), &events));
    }

    #[tokio::test]
    async fn cut_keeps_complete_pairs_and_parked_approval_state_unchanged() {
        let mut events = vec![];
        append(&mut events, user("current", "original request"));
        completed(&mut events, "one", 1);
        completed(&mut events, "two", 2);
        let mut ctx = rehydrate(thread(), &events);
        let compactor = Threshold::new(1, 1, Fake);
        let plan = compactor.plan(&ctx).await;
        let compaction = plan.compaction.expect("completed prefix compacts");
        // Keeping one result retains its assistant request too.
        assert_eq!(compaction.covers_to, Cursor(4));
        let event = Event::Compaction {
            covers_to_cursor: compaction.covers_to,
            summary: compaction.summary,
        };
        append(&mut events, event.clone());
        ctx.observe(events.last().expect("event").0, &event);
        assert!(
            matches!(&ctx.history()[2].message, Message::Assistant { calls, .. } if calls[0].id.as_str() == "two")
        );
        assert!(
            matches!(&ctx.history()[3].message, Message::Tool { call, .. } if call.as_str() == "two")
        );
        let call = ProposedCall::new(
            CallId::new("write"),
            ToolName::new("write"),
            json!({"unchanged": true}),
            PrincipalId::new("alice"),
        );
        append(
            &mut events,
            Event::ModelStepCompleted {
                timing: None,
                served: None,
                step: 3,
                text: String::new(),
                calls: vec![call.clone()],
                reasoning: None,
            },
        );
        append(
            &mut events,
            Event::ApprovalRequested {
                call: call.id.clone(),
                approval: ApprovalId::new("approval"),
                args_digest: call.args_digest.clone(),
                summary: "approve".into(),
            },
        );
        ctx = rehydrate(thread(), &events);
        let before = ctx.clone();
        assert!(compactor.plan(&ctx).await.compaction.is_none());
        assert_eq!(ctx, before);
        assert_eq!(ctx, rehydrate(thread(), &events));
    }
    #[tokio::test]
    async fn boundary_summary_covers_finished_inputs_preserves_usage_and_replays() {
        struct FinishedSummary;
        impl Summarize for FinishedSummary {
            async fn summarize(&self, _ctx: &Context, entries: &[Entry]) -> Summary {
                assert!(entries.iter().any(|entry| matches!(&entry.message,
                    Message::User { text, .. } if text == "exact completed request")));
                Summary {
                    text: Some("finished history".into()),
                    usage: Usage {
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
                        input_tokens: 4,
                        output_tokens: 2,
                        cost_micros: 3,
                    },
                }
            }
        }
        let mut events = vec![];
        append(&mut events, user("finished", "exact completed request"));
        completed(&mut events, "one", 1);
        append(
            &mut events,
            Event::Final {
                text: "done".into(),
            },
        );
        let mut ctx = rehydrate(thread(), &events);
        let size = ctx
            .history()
            .iter()
            .map(|entry| entry.message.size())
            .sum::<usize>();
        let compactor = Threshold::for_turns(size + 1, FinishedSummary);
        assert!(compactor.plan(&ctx).await.compaction.is_none());
        let plan = compactor.plan_after_turn(&ctx).await;
        assert_eq!(plan.usage.cost_micros, 3);
        let plan = plan.compaction.expect("proactive boundary");
        append(
            &mut events,
            Event::Usage(Usage {
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                input_tokens: 4,
                output_tokens: 2,
                cost_micros: 3,
            }),
        );
        append(
            &mut events,
            Event::Compaction {
                covers_to_cursor: plan.covers_to,
                summary: plan.summary,
            },
        );
        ctx.observe(events[events.len() - 2].0, &events[events.len() - 2].1);
        ctx.observe(events[events.len() - 1].0, &events[events.len() - 1].1);
        assert_eq!(ctx, rehydrate(thread(), &events));
        assert_eq!(
            ctx.history().len(),
            1,
            "finished request is included in the summary"
        );
        append(&mut events, user("next", "next request"));
        let ctx = rehydrate(thread(), &events);
        assert!(
            compactor.plan_after_turn(&ctx).await.compaction.is_none(),
            "queued work is never compacted between turns"
        );
        assert!(compactor.plan(&ctx).await.compaction.is_none());
    }

    struct RecordedSummary(std::sync::Arc<std::sync::Mutex<Vec<Entry>>>);
    impl Summarize for RecordedSummary {
        async fn summarize(&self, ctx: &Context, entries: &[Entry]) -> Summary {
            assert!(entries.iter().all(|entry| !matches!(&entry.message,
                Message::User { turn, .. } if Some(turn) == ctx.turn())));
            *self.0.lock().expect("seen") = entries.to_vec();
            Summary {
                text: Some("completed non-authoritative history".into()),
                usage: Usage {
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                    input_tokens: 11,
                    output_tokens: 3,
                    cost_micros: 7,
                },
            }
        }
    }

    #[tokio::test]
    async fn configured_turn_compactor_activates_mid_turn_and_preserves_exact_inputs_and_usage() {
        let mut events = vec![];
        append(&mut events, user("current", "exact current request"));
        completed(&mut events, "one", 1);
        append(
            &mut events,
            Event::Steer {
                principal: PrincipalId::new("bob"),
                text: "exact applied steer".into(),
            },
        );
        completed(&mut events, "two", 2);
        // Control accepted during planning must survive alongside applied inputs.
        append(
            &mut events,
            Event::Steer {
                principal: PrincipalId::new("carol"),
                text: "exact queued steer".into(),
            },
        );
        let mut ctx = rehydrate(thread(), &events);
        assert!(ctx.has_queued_steers());
        let before_inputs = inputs(&ctx);
        assert_eq!(before_inputs.len(), 2, "request and applied steer");
        let expected_cursor = ctx.history().last().expect("last completed result").cursor;
        let seen = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
        // This is the constructor Wired uses, rather than an isolated helper test.
        let compactor = Threshold::for_turns(1, RecordedSummary(seen.clone()));
        let plan = compactor.plan(&ctx).await;
        assert_eq!(
            plan.usage.cost_micros, 7,
            "summary usage reaches engine admission"
        );
        let compaction = plan.compaction.expect("configured mid-turn compaction");
        assert_eq!(
            compaction.covers_to, expected_cursor,
            "latest completed step is covered"
        );
        let covered = seen.lock().expect("seen");
        for id in ["one", "two"] {
            assert!(covered.iter().any(
                |entry| matches!(&entry.message, Message::Assistant { calls, .. }
                if calls.iter().any(|call| call.id.as_str() == id))
            ));
            assert!(covered.iter().any(
                |entry| matches!(&entry.message, Message::Tool { call, .. } if call.as_str() == id)
            ));
        }
        drop(covered);
        let event = Event::Compaction {
            covers_to_cursor: compaction.covers_to,
            summary: compaction.summary,
        };
        append(&mut events, event.clone());
        ctx.observe(events.last().expect("compaction cursor").0, &event);
        assert_eq!(
            inputs(&ctx),
            before_inputs,
            "principal, text and attachment identity remain exact"
        );
        assert!(ctx.has_queued_steers(), "queued control remains pending");
        assert!(
            ctx.history().iter().all(|entry| matches!(
                entry.message,
                Message::User { .. } | Message::Summary { .. }
            )),
            "no assistant/tool suffix is replayed under the changed prefix"
        );
        assert_eq!(
            ctx,
            rehydrate(thread(), &events),
            "warm and full replay agree"
        );
    }
}

#[cfg(test)]
mod cut_tests {
    use super::*;
    use crate::event::{
        ApprovalMode, CallId, Event, Outcome, Output, PrincipalId, ProposedCall, ThreadId,
        ToolName, TurnId,
    };
    use crate::rehydrate::rehydrate;

    const BIG: usize = 1_000;

    fn thread() -> ThreadId {
        ThreadId {
            org: "o".into(),
            workspace: "w".into(),
            thread: "t".into(),
        }
    }

    fn user(turn: &str) -> Event {
        Event::UserMessage {
            turn: TurnId::new(turn),
            message_id: None,
            principal: PrincipalId::new("alice"),
            text: "x".repeat(BIG),
            attachments: Vec::new(),
            client_tools: Vec::new(),
            authorized_tools: Vec::new(),
            model_binding: None,
            approval_mode: ApprovalMode::Interactive,
        }
    }

    fn step(n: u32, call: Option<&str>) -> Vec<Event> {
        let calls = call
            .map(|id| {
                vec![ProposedCall::new(
                    CallId::new(id),
                    ToolName::new("tool"),
                    serde_json::json!({}),
                    PrincipalId::new("alice"),
                )]
            })
            .unwrap_or_default();
        vec![
            Event::StepStarted {
                step: n,
                control_through: Cursor::START,
            },
            Event::ModelStepCompleted {
                step: n,
                text: "y".repeat(BIG),
                calls,
                reasoning: None,
                served: None,
                timing: None,
            },
        ]
    }

    fn result(call: &str) -> Event {
        Event::ToolFinished {
            call: CallId::new(call),
            outcome: Outcome::Succeeded,
            output: Output::Text("z".repeat(BIG)),
            receipt: None,
            summary: None,
        }
    }

    fn context(events: Vec<Event>) -> Context {
        let numbered: Vec<_> = events
            .into_iter()
            .enumerate()
            .map(|(index, event)| (Cursor(index as i64 + 1), event))
            .collect();
        rehydrate(thread(), &numbered)
    }

    /// Turn 1 answered; turn 2 with `rounds` closed tool rounds and, when
    /// `open`, a final step whose call has no result yet.
    fn two_turns(rounds: u32, open: bool) -> Context {
        let mut events = vec![user("t1")];
        events.extend(step(1, None));
        events.push(Event::Final {
            text: "done".into(),
        });
        events.push(user("t2"));
        for round in 1..=rounds {
            let call = format!("c{round}");
            events.extend(step(round, Some(&call)));
            events.push(result(&call));
        }
        if open {
            events.extend(step(rounds + 1, Some("open")));
        }
        context(events)
    }

    #[test]
    fn under_the_limit_nothing_is_cut() {
        assert_eq!(plan_cut(&two_turns(1, false), 1_000_000), None);
    }

    #[test]
    fn cuts_at_the_start_of_the_current_turn() {
        // Turn 2 has only its user message so far: the summary covers turn 1
        // and turn 2's request stays verbatim.
        let ctx = two_turns(0, false);
        let cut = plan_cut(&ctx, 2 * BIG).expect("cut");
        // Turn 1: user + assistant. Turn 2 starts at index 2.
        assert_eq!(cut.covered.len(), 2);
        assert!(!cut.mid_turn);
        assert_eq!(cut.covers_to, ctx.history()[1].cursor);
        assert!(matches!(ctx.history()[2].message, Message::User { .. }));
        assert_eq!(ctx.history().len(), 3);
    }

    #[test]
    fn cuts_inside_the_turn_after_the_latest_closed_step() {
        let ctx = two_turns(3, false);
        let cut = plan_cut(&ctx, 4 * BIG).expect("cut");
        assert!(cut.mid_turn);
        // Everything up to the last tool result is covered: the summary
        // stands alone and no assistant step is replayed under it.
        assert_eq!(cut.covered.len(), ctx.history().len());
        assert!(matches!(
            cut.covered.last().expect("covered").message,
            Message::Tool { .. }
        ));
        assert_eq!(cut.covers_to, ctx.history().last().expect("last").cursor);
    }

    #[test]
    fn never_keeps_an_assistant_step_after_the_cut() {
        // Tool rounds inside a turn small enough to fit on its own: the
        // turn-boundary cut is not taken because it would keep steps.
        for rounds in 0..=4 {
            let ctx = two_turns(rounds, false);
            let Some(cut) = plan_cut(&ctx, BIG) else {
                continue;
            };
            let kept = &ctx.history()[cut.covered.len()..];
            assert!(
                kept.iter()
                    .all(|entry| matches!(entry.message, Message::User { .. })),
                "rounds={rounds}: only user messages are kept"
            );
        }
    }

    #[test]
    fn refuses_while_a_tool_round_is_open() {
        assert_eq!(plan_cut(&two_turns(3, true), BIG), None);
        assert_eq!(plan_cut(&two_turns(0, true), BIG), None);
    }

    #[test]
    fn a_tool_result_never_leads_the_kept_part() {
        let ctx = two_turns(3, false);
        let history = ctx.history();
        for cut in 1..=history.len() {
            if matches!(
                history.get(cut).map(|e| &e.message),
                Some(Message::Tool { .. })
            ) {
                assert!(!valid_cut(history, cut), "cut {cut} would orphan a result");
            }
        }
    }

    #[test]
    fn never_cuts_after_an_unfinished_assistant_message() {
        let ctx = two_turns(2, true);
        let history = ctx.history();
        let last = history.len() - 1;
        assert!(matches!(history[last].message, Message::Assistant { .. }));
        assert!(!valid_cut(history, last + 1));
    }

    #[test]
    fn never_cuts_between_entries_of_one_cursor() {
        let mut history = two_turns(2, false).history().to_vec();
        // Force the assistant after the first Tool group to share its cursor.
        let tool_cursor = history[4].cursor;
        history[5].cursor = tool_cursor;
        assert!(!valid_cut(&history, 5));
        assert!(!valid_cut(&history, 0));
        assert!(!valid_cut(&history, history.len() + 1));
    }

    #[test]
    fn a_lone_summary_is_not_worth_recompacting() {
        let history = [Entry {
            cursor: Cursor(1),
            message: Message::Summary { text: "s".into() },
        }];
        assert!(!valid_cut(&history, 1));
    }

    #[test]
    fn the_compaction_event_cursor_is_recorded() {
        let mut events = vec![user("t1")];
        events.extend(step(1, None));
        events.push(Event::Compaction {
            covers_to_cursor: Cursor(3),
            summary: "s".into(),
        });
        let ctx = context(events);
        assert_eq!(ctx.last_compaction_cursor(), Some(Cursor(4)));
        assert_eq!(context(vec![user("t1")]).last_compaction_cursor(), None);
    }
}
