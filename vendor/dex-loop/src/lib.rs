//! `dex-loop`: the one Dex agent loop, shared by every surface.
//!
//! The loop appends the user's message, streams the model, commits the
//! proposed tool calls to the log, dispatches them (allowed read-only calls
//! in one parallel wave, mutations one at a time through the effect ledger,
//! all in the model's order), appends the results, and repeats until the
//! model answers without tool calls. Every step is an event in the thread's
//! log; the log is the only state.
//!
//! The crate holds no I/O. The host supplies the ports:
//! [`Log`], [`Model`], [`Tools`], [`Effects`], [`Sanitizer`], and optionally
//! a [`Compactor`]. It never spawns tasks; parallel tool calls are polled
//! concurrently inside [`Engine::run`].
//!
//! ```text
//! host: append UserMessage
//! ctx = rehydrate(thread, log)          // same code path warm or after a crash
//! engine.run(&mut ctx, &cancel) -> Exit // Done | Parked | Asked | Interrupted | Failed
//! host: on approve/answer, append the decision and call run again
//! ```

mod budget;
mod compaction;
mod context;
mod engine;
mod event;
mod ports;
mod rehydrate;
mod sanitize;

pub use budget::{Budget, BudgetAxis};
pub use compaction::{Compaction, Compactor, NoCompaction, Summarize, Threshold};
pub use context::{Context, Entry, Message};
pub use engine::{DEFAULT_TOOL_CALL_DEADLINE, Engine, Exit, TOOLS_SEARCH};
pub use event::{
    ApprovalId, ApprovalMode, ArtifactRef, CallId, ClientToolSpec, Cursor, ErrorCode, Event,
    HEADLESS_AUTO_APPROVER, MessageId, Outcome, Output, OutputRef, PrincipalId, ProposedCall,
    ProviderReasoning, ReceiptId, ThreadId, ToolName, ToolResult, TurnId, Usage, args_digest,
};
pub use ports::{
    Claim, Effects, ExecutorKind, Fenced, GovernanceClass, Log, Model, ModelChunk, ModelError,
    ToolSpec, Tools, Verdict,
};
pub use rehydrate::rehydrate;
pub use sanitize::{DeltaFilter, Lexicon, LexiconFilter, Sanitizer};
pub use tokio_util::sync::CancellationToken;
