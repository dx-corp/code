//! Keeping the model's context bounded.

use std::future::Future;

use crate::context::{Context, Entry, Message};
use crate::event::Cursor;

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
    fn plan(&self, ctx: &Context) -> impl Future<Output = Option<Compaction>> + Send;
}

/// Never compacts.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoCompaction;

impl Compactor for NoCompaction {
    async fn plan(&self, _ctx: &Context) -> Option<Compaction> {
        None
    }
}

/// Writes the summary for a prefix of history (usually one model call).
pub trait Summarize: Send + Sync {
    /// `None` skips this compaction; the engine continues uncompacted.
    fn summarize(&self, entries: &[Entry]) -> impl Future<Output = Option<String>> + Send;
}

/// Compacts when history exceeds `max_bytes`, keeping at least the last
/// `keep_recent` entries verbatim.
#[derive(Clone, Debug)]
pub struct Threshold<S> {
    max_bytes: usize,
    keep_recent: usize,
    summarizer: S,
}

impl<S: Summarize> Threshold<S> {
    pub fn new(max_bytes: usize, keep_recent: usize, summarizer: S) -> Self {
        Self {
            max_bytes,
            keep_recent,
            summarizer,
        }
    }
}

impl<S: Summarize> Compactor for Threshold<S> {
    async fn plan(&self, ctx: &Context) -> Option<Compaction> {
        let history = ctx.history();
        let size: usize = history.iter().map(|entry| entry.message.size()).sum();
        if size <= self.max_bytes {
            return None;
        }
        let cut = cut_point(history, self.keep_recent)?;
        let summary = self.summarizer.summarize(&history[..cut]).await?;
        Some(Compaction {
            covers_to: history[cut - 1].cursor,
            summary,
        })
    }
}

/// Where to cut history for a compaction, chosen by [`plan_cut`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cut<'a> {
    /// The entries the summary replaces (a prefix of the history).
    pub covered: &'a [Entry],
    /// The cursor the `Compaction` event names: the last covered entry's.
    pub covers_to: Cursor,
    /// Whether the cut fell inside the current turn (after a closed step)
    /// rather than at its start.
    pub mid_turn: bool,
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
///   was not produced beside. Claude requires the last assistant tool step to
///   carry its own unmodified thinking blocks (the request is rejected
///   otherwise), and a step produced before a compaction cannot replay them.
///   The next step starts a fresh assistant message after the summary.
/// - Never leaves a `Message::Tool` as the first kept entry and never splits
///   entries that share a cursor.
pub fn plan_cut(ctx: &Context, max_bytes: usize) -> Option<Cut<'_>> {
    if ctx.open_step().is_some() {
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
    let turn_start = ctx.turn().and_then(|turn| {
        history
            .iter()
            .position(|entry| matches!(&entry.message, Message::User { turn: t, .. } if t == turn))
    });
    Some(Cut {
        covered: &history[..cut],
        covers_to: history[cut - 1].cursor,
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
fn cut_point(history: &[Entry], keep_recent: usize) -> Option<usize> {
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
            },
        ]
    }

    fn result(call: &str) -> Event {
        Event::ToolFinished {
            call: CallId::new(call),
            outcome: Outcome::Succeeded,
            output: Output::Text("z".repeat(BIG)),
            receipt: None,
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
        let mut events = vec![user("t1")];
        events.extend(step(1, None));
        events.push(Event::Compaction {
            covers_to_cursor: Cursor(3),
            summary: "s".repeat(BIG),
        });
        events.push(user("t2"));
        let ctx = context(events);
        // History: Summary + the new user message; the turn starts at 1.
        assert_eq!(plan_cut(&ctx, BIG), None);
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
