//! The host ports. Each is implemented once, by the service that runs the
//! engine; tests implement them in memory.

use std::future::Future;

use futures_util::Stream;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::context::Context;
use crate::event::{
    ApprovalId, CallId, Cursor, Event, PrincipalId, ProposedCall, ProviderReasoning, ServedBy,
    ThreadId, ToolName, ToolResult, Usage,
};

/// The log or the effect ledger refused a write. The engine stops at once and
/// appends nothing more. Returned for a lost lease and for any write that
/// cannot be made durable; the next lease holder rehydrates from the log.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("write fenced: {reason}")]
pub struct Fenced {
    pub reason: String,
}

impl Fenced {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

/// The thread's event log, the only source of truth.
pub trait Log: Send + Sync {
    /// Appends `events` in order and returns one cursor per event. Everything
    /// buffered by `append_text` is written first.
    fn append(&self, events: &[Event]) -> impl Future<Output = Result<Vec<Cursor>, Fenced>> + Send;

    /// Customer-visible model text. The log coalesces it into `TextDelta`
    /// rows of 25-100 ms or a few KB each, so the stored text is what the
    /// customer saw; the engine never assumes one row per call.
    fn append_text(&self, text: String) -> impl Future<Output = Result<(), Fenced>> + Send;

    /// Control events (`Event::is_control`) with a cursor greater than
    /// `after`, in log order.
    fn control_since(
        &self,
        after: Cursor,
    ) -> impl Future<Output = Result<Vec<(Cursor, Event)>, Fenced>> + Send;
}

/// One chunk of a streaming model response.
#[derive(Clone, Debug, PartialEq)]
pub enum ModelChunk {
    Text(String),
    /// A summary of the model's thinking, streamed while it thinks. Shown as
    /// progress only: never answer text, never model history, not usage.
    Thinking(String),
    /// The engine assigns the `CallId`; provider call ids are not used.
    ToolCall {
        name: ToolName,
        args: serde_json::Value,
    },
    Usage(Usage),
    /// The step's opaque provider continuation state. At most one per step,
    /// sent only after a clean terminal (the same commit rule as `ToolCall`
    /// and `Usage`), after the last `ToolCall` and before `Usage`. The engine
    /// stores it on `ModelStepCompleted`; history returns it on
    /// `Message::Assistant`.
    Reasoning(ProviderReasoning),
    /// Which provider route and model served this response. Sent at most
    /// once, before the first other chunk; the engine stores it on
    /// `ModelStepCompleted`.
    Served(ServedBy),
}

/// The model call failed after the `Model` port's own retries.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("model call failed: {message}")]
pub struct ModelError {
    pub message: String,
}

/// The model, streaming.
pub trait Model: Send + Sync {
    /// Streams one response to `ctx.history()` with `tools` offered. The
    /// engine drops the stream to cancel it.
    fn stream<'a>(
        &'a self,
        ctx: &'a Context,
        tools: &'a [&'a ToolSpec],
    ) -> impl Stream<Item = Result<ModelChunk, ModelError>> + Send + 'a;
}

/// Which governance a tool falls under. Read by `Tools::policy`, not by the
/// engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GovernanceClass {
    Plain,
    Guardrails,
    Guardian,
    Approval,
}

/// Where a call runs. The engine only distinguishes `User`: those calls are
/// answered by a person, so the engine asks instead of running them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutorKind {
    InProcess,
    ToolExecutor,
    Computer,
    /// The call's `args.question` string is shown to the user; the answer is
    /// the result.
    User,
    /// Runs on the declaring client session (a browser tab, a desktop
    /// client), not on the host. The engine parks the call as
    /// `ClientToolRequested` and resumes on that session's
    /// `ClientToolResult`, exactly as `User` parks on `Answer`.
    Client,
}

/// One registry entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: ToolName,
    /// The only tool text a surface may show.
    pub label: String,
    pub schema: serde_json::Value,
    /// Read-only calls that policy allows run together in one parallel wave,
    /// and may run again after a restart. Every other call is a mutation and
    /// goes through `Effects`.
    pub read_only: bool,
    /// Offered to the model on every step. Other tools are offered only after
    /// `tools.search` exposes them in the turn.
    pub core: bool,
    pub governance: GovernanceClass,
    pub executor: ExecutorKind,
}

/// The policy decision for one call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    /// The model sees `"denied: {reason}"` as the call's result.
    Deny(String),
    /// Park the turn. `summary` is customer-safe and shown on the approval card.
    NeedsApproval {
        approval: ApprovalId,
        summary: String,
    },
}

/// The tool registry, policy, and executors.
pub trait Tools: Send + Sync {
    /// Every tool this turn may use, core or discoverable.
    fn catalog(&self) -> &[ToolSpec];

    fn spec(&self, name: &ToolName) -> Option<&ToolSpec> {
        self.catalog().iter().find(|spec| &spec.name == name)
    }

    /// Catalog tools matching `query` that `principal` may use, best first.
    fn search(
        &self,
        principal: &PrincipalId,
        query: &str,
    ) -> impl Future<Output = Vec<ToolName>> + Send;

    /// Current authorization for one call under `call.principal`: governance,
    /// grants, guardrails and guardian. Called in the model's call order just
    /// before the call would run, and again when a parked call resumes.
    fn policy(&self, ctx: &Context, call: &ProposedCall) -> impl Future<Output = Verdict> + Send;

    /// Runs one call. `call.id` is unique only within `thread`: a turn id is
    /// caller-chosen, so the same `CallId` string can occur in two different
    /// threads. Where the downstream system needs a globally unique
    /// idempotency key, build it from `thread` and `call.id` together, not
    /// `call.id` alone. `cancel` fires on interrupt for reads and mutations
    /// alike, and the engine keeps awaiting the call to return either way:
    /// it never drops a mutation's future, and it records whatever result
    /// comes back in the effect ledger. What cancel means is the tool's to
    /// decide. A read abandons its work; a mutation that can stop cleanly
    /// (a background process, say) stops it and returns the partial outcome,
    /// and one that cannot simply finishes.
    fn run(
        &self,
        thread: &ThreadId,
        call: &ProposedCall,
        cancel: &CancellationToken,
    ) -> impl Future<Output = ToolResult> + Send;

    /// Finishes a call an `ExecutorKind::Client` session already reported an
    /// outcome for. The engine never dispatches these through `run` (the
    /// client, not this port, already ran the call); it calls this instead,
    /// exactly once per call id, to let the host wrap untrusted client
    /// content and store a large output before the result reaches history --
    /// the same shaping a live dispatch of any other executor gets. Called
    /// identically whether `raw` just arrived or is being adopted on replay
    /// after a restart, so the two produce the same wrapped result. The
    /// default passes `raw` through unchanged.
    fn wrap_client_result(
        &self,
        thread: &ThreadId,
        call: &ProposedCall,
        raw: ToolResult,
    ) -> impl Future<Output = ToolResult> + Send {
        let _ = thread;
        let _ = call;
        std::future::ready(raw)
    }
}

/// The answer to `Effects::claim`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Claim {
    /// First claim for this call: dispatch it, then `record` the outcome.
    Granted,
    /// Claimed before. The recorded result, reconciled with downstream where
    /// possible; `Outcome::Running` or `Outcome::Unknown` if not. The engine
    /// never reports `Running` to the model as a call's final outcome —
    /// nothing revisits it — so it settles `Running` to `Unknown` first.
    Existing(ToolResult),
}

/// The durable effect ledger, keyed by `CallId`. Every mutation is claimed
/// before dispatch and recorded after, so a mutation is dispatched at most
/// once per call id, across restarts and replicas.
pub trait Effects: Send + Sync {
    fn claim(&self, call: &ProposedCall) -> impl Future<Output = Result<Claim, Fenced>> + Send;

    fn record(
        &self,
        call: &CallId,
        result: &ToolResult,
    ) -> impl Future<Output = Result<(), Fenced>> + Send;
}
