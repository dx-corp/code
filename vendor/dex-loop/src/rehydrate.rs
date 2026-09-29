//! Rebuilding a thread's context from its log.

use crate::context::Context;
use crate::event::{Cursor, Event, ThreadId};

/// Rebuilds the context a warm actor would hold after these events.
///
/// `events` is the thread's log in cursor order: all of it, or a suffix that
/// starts at or before both the current turn's `UserMessage` and the latest
/// `Compaction`'s `covers_to_cursor`.
///
/// What `Engine::run` then does with the result:
/// - a `StepStarted` with no `ModelStepCompleted` gets `ModelAttemptAbandoned`
///   and the model call is issued again as a new step;
/// - a read-only call with `ToolStarted` but no `ToolFinished` runs again; a
///   mutation goes to the `Effects` ledger, which returns its recorded outcome
///   (or `Running`/`Unknown`) instead of dispatching it again;
/// - a parked call waits for its `ApprovalDecided`, then policy runs again,
///   the approval digest is checked, and the calls after it in the same step
///   are dispatched in order, all read from `ModelStepCompleted`.
pub fn rehydrate(thread: ThreadId, events: &[(Cursor, Event)]) -> Context {
    let mut ctx = Context::new(thread);
    // Kernel contract: no control event with a cursor before this replay's
    // starting point is ever re-applied -- not by this replay (it only ever
    // observes `events`, which already excludes anything earlier) and not
    // afterward, by the engine's own `Log::control_since(ctx.control_cursor())`
    // re-fetching what this suffix deliberately left out. A suffix with no
    // control-kind event in it at all would otherwise leave
    // `control_cursor()` at its default, under-reporting where this replay
    // starts and letting an old, already-resolved control event (most
    // notably an `Interrupt` for a turn that ended before this one started)
    // be fetched again and observed against whatever turn is running now.
    // See `Context::advance_control_floor`.
    if let Some((first, _)) = events.first() {
        ctx.advance_control_floor(*first);
    }
    for (cursor, event) in events {
        ctx.observe(*cursor, event);
    }
    ctx
}
