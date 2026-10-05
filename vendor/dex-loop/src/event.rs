//! The event log vocabulary: every row a thread's log holds, and the values
//! those rows carry.

use std::fmt;
use std::ops::AddAssign;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

macro_rules! string_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

string_id!(
    /// One user turn. Host-assigned; must match `[A-Za-z0-9_-]+` because call
    /// ids are derived from it and model providers restrict tool-call ids.
    TurnId
);
string_id!(
    /// The source message a `UserMessage` was appended for. Host-constructed
    /// and validated by the host: an opaque, bounded string (1 to 256
    /// bytes) with no ASCII control characters or whitespace, but otherwise
    /// unrestricted, since real ids (e.g. platform-api's
    /// `message:human:<suffix>`) are not limited to `TurnId`'s narrower
    /// `[A-Za-z0-9_-]` charset. The value is carried byte for byte, never
    /// normalized. Optional because log rows written before this field
    /// existed carry none; `#[serde(default)]` on
    /// `Event::UserMessage::message_id` makes those rows deserialize to
    /// `None` rather than fail.
    MessageId
);
string_id!(
    /// One tool call. `"{turn}-{step}-{index}"`, assigned by the engine.
    /// Unique only within its thread: `TurnId` is caller-chosen and not
    /// guaranteed unique across threads, so the same `CallId` string can
    /// occur in two different threads. A downstream idempotency key built
    /// from this alone can collide; see `Tools::run`.
    CallId
);
string_id!(
    /// A registry tool name. Internal: surfaces show `ToolSpec::label`.
    ToolName
);
string_id!(
    /// One approval request, issued by `Tools::policy`.
    ApprovalId
);
string_id!(
    /// The principal who decided an approval.
    PrincipalId
);
string_id!(
    /// A reference into tenant-scoped storage holding a tool's output.
    OutputRef
);
string_id!(
    /// A governance receipt written for a tool call.
    ReceiptId
);
string_id!(
    /// A reference to a user-supplied attachment in tenant-scoped storage.
    ArtifactRef
);

/// Owner-bound action details attached to a chat question. No prose grants authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionConfirmation {
    pub proposal_call_id: CallId,
    pub tool: ToolName,
    pub args_digest: String,
    pub principal_id: PrincipalId,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmationDecision {
    #[default]
    Unspecified,
    Confirm,
    Decline,
}

/// The tenant scope of one thread. Every log read and write carries all three.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ThreadId {
    pub org: String,
    pub workspace: String,
    pub thread: String,
}

/// Position of an event in a thread's log. Strictly increasing per thread.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Cursor(pub i64);

impl Cursor {
    /// Before the first event.
    pub const START: Cursor = Cursor(0);
}

/// Opaque provider continuation state for one model step: what a provider
/// requires the client to send back unmodified with that step's calls on the
/// next request (Gemini function-call thought signatures, Anthropic signed
/// thinking blocks, OpenAI encrypted reasoning items).
///
/// dex-loop stores it with the step and hands it back in history; it never
/// reads `payload`. Only the `Model` port that wrote it interprets it.
///
/// `payload` can hold model-written text about tenant data (Anthropic
/// thinking summaries), so `Debug` prints its size, never its content; keep
/// it out of tracing, metrics and catch-all metadata.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderReasoning {
    /// The payload's shape, e.g. `google.gemini.v1`,
    /// `anthropic.messages.v1`, `openai.responses.v1`.
    pub format: String,
    /// The provider model id that produced it.
    pub model: String,
    /// Opaque provider payload. Never edited by dex-loop.
    pub payload: serde_json::Value,
}

impl fmt::Debug for ProviderReasoning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderReasoning")
            .field("format", &self.format)
            .field("model", &self.model)
            .field(
                "payload",
                &format_args!("<{} bytes>", self.payload.to_string().len()),
            )
            .finish()
    }
}

/// The provider route and model that actually served one model step. With
/// ordered failover the serving route can differ from the configured
/// primary, so surfaces and metering read it from the step, not config.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServedBy {
    /// The model-gateway provider id, e.g. `vertex-anthropic`, `vertex-ai`.
    pub provider: String,
    /// The provider model id, e.g. `claude-opus-5-5`.
    pub model: String,
}

/// What the model port did after one attempt failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptNext {
    /// Tried the same route again.
    Retry,
    /// Moved to the next route.
    Failover,
    /// Gave up; the step fails.
    Abandon,
}

/// Model spend reported by one model response.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// All prompt tokens, cached or not.
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_micros: u64,
    /// The part of `input_tokens` served from the prompt cache. Zero on rows
    /// written before the field existed, and for providers that do not
    /// report it.
    #[serde(default, alias = "cache_read_tokens", skip_serializing_if = "is_zero")]
    pub cache_read_input_tokens: u64,
    /// The part of `input_tokens` written to the prompt cache. Same default.
    #[serde(
        default,
        alias = "cache_creation_tokens",
        skip_serializing_if = "is_zero"
    )]
    pub cache_creation_input_tokens: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl Usage {
    pub fn tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

impl AddAssign for Usage {
    fn add_assign(&mut self, other: Usage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cost_micros = self.cost_micros.saturating_add(other.cost_micros);
        self.cache_read_input_tokens = self
            .cache_read_input_tokens
            .saturating_add(other.cache_read_input_tokens);
        self.cache_creation_input_tokens = self
            .cache_creation_input_tokens
            .saturating_add(other.cache_creation_input_tokens);
    }
}

/// Where one model step's wall clock went. Every span is measured from the
/// moment the step's request began. Nothing here is request or response
/// content, and it never enters the model request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepTiming {
    /// Building the request. Zero on a retry.
    pub prepare_ms: u64,
    /// Minting the gateway bearer.
    pub mint_ms: u64,
    /// Request start to the gateway's response headers.
    pub headers_ms: u64,
    /// Request start to the first bytes of the response body.
    pub first_event_ms: Option<u64>,
    /// Request start to the first text delta released to the loop.
    pub first_text_ms: Option<u64>,
    /// Request start to the end of the response.
    pub total_ms: u64,
}

/// The default for `ClientToolRequested::deadline_ms` on a row written
/// before that field existed: never expires, rather than timing out every
/// call still parked from before this field shipped.
fn never_expires() -> i64 {
    i64::MAX
}

/// Hex SHA-256 of serialized tool arguments.
pub fn args_digest(args: &serde_json::Value) -> String {
    // Serializing a `serde_json::Value` into a Vec cannot fail: every key is
    // a string and there is no I/O.
    let bytes = serde_json::to_vec(args).unwrap_or_default();
    hex::encode(Sha256::digest(bytes))
}

/// A tool call the model proposed. Durable from `ModelStepCompleted` on:
/// park, resume and replay all read it from the log, never from memory.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProposedCall {
    pub id: CallId,
    pub tool: ToolName,
    pub args: serde_json::Value,
    /// An approval authorizes only a call with this digest.
    pub args_digest: String,
    /// The principal whose authority the call runs under: the author of the
    /// most recent user input the step acts on.
    pub principal: PrincipalId,
}

impl ProposedCall {
    pub fn new(
        id: CallId,
        tool: ToolName,
        args: serde_json::Value,
        principal: PrincipalId,
    ) -> Self {
        let args_digest = args_digest(&args);
        Self {
            id,
            tool,
            args,
            args_digest,
            principal,
        }
    }
}

/// One tool a client session declared it can execute locally, resolved by
/// the host against its per-surface allowlist before this reaches the log:
/// `read_only` here is the host's decision, never the client's own claim.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClientToolSpec {
    pub name: ToolName,
    pub schema: serde_json::Value,
    pub read_only: bool,
    /// The only tool text a surface may show.
    pub label: String,
}

/// The durable outcome of a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Succeeded,
    Failed,
    /// Dispatched; completion is not yet known.
    Running,
    /// The effect may or may not have happened. Never replayed automatically.
    Unknown,
}

/// What a tool call produced, as the model will see it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Output {
    /// Tool output stored in tenant-scoped storage. The `Model` port resolves
    /// it when it renders history; the log never holds the bytes.
    Ref(OutputRef),
    /// A short note: a denial, an unknown tool, a user's answer, an outcome
    /// the engine could not recover.
    Text(String),
    /// Bounded inline script projections, revalidated before provider transport.
    Blocks(Vec<agent_codemode::OutputBlock>),
}

/// The outcome of one tool call.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub outcome: Outcome,
    pub output: Output,
    pub receipt: Option<ReceiptId>,
}

impl ToolResult {
    /// A successful call whose output lives in storage.
    pub fn stored(output: OutputRef, receipt: Option<ReceiptId>) -> Self {
        Self {
            outcome: Outcome::Succeeded,
            output: Output::Ref(output),
            receipt,
        }
    }

    /// A successful call with a short inline result.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            outcome: Outcome::Succeeded,
            output: Output::Text(text.into()),
            receipt: None,
        }
    }

    /// A failed call. The model sees `message`.
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            outcome: Outcome::Failed,
            output: Output::Text(message.into()),
            receipt: None,
        }
    }

    /// A call whose effect may or may not have happened.
    pub fn unknown(message: impl Into<String>) -> Self {
        Self {
            outcome: Outcome::Unknown,
            output: Output::Text(message.into()),
            receipt: None,
        }
    }
}

/// Why a turn stopped with `Event::Error`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BudgetExhausted,
    ModelFailed,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::BudgetExhausted => "budget_exhausted",
            ErrorCode::ModelFailed => "model_failed",
        }
    }
}

/// Execution mode retained in durable ingress for compatibility and attribution.
/// Both modes now grant `NeedsApproval` through an exact `AutoApproved`
/// receipt. Neither mode overrides a hard policy `Deny`.
/// A content-free failure class supplied by the typed model adapter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    /// The turn hit its step, token, cost or wall budget.
    BudgetExhausted,
    /// The stream ended before the response completed.
    Truncated,
    /// The provider ended the response early (for example the output token
    /// limit).
    Incomplete,
    /// The model proposed a tool call whose arguments were not a JSON object.
    MalformedToolCall,
    /// The response held neither text nor tool calls.
    EmptyCompletion,
    /// The gateway could not be reached or the connection broke.
    Transport,
    /// The provider or gateway throttled the request.
    RateLimited,
    /// The provider or gateway reported itself unavailable or overloaded.
    Unavailable,
    /// The provider or a safety filter refused to answer.
    Refusal,
    /// The service token for the gateway could not be minted.
    Auth,
    /// The wire broke the SSE or Responses event contract.
    Protocol,
    /// Stored history could not be loaded for the request.
    Resolve,
    /// The host could not prepare the request.
    Host,
    /// Any other gateway or provider rejection.
    Rejected,
    /// An error the classifier does not recognise. Old and new codes land
    /// here rather than failing.
    #[default]
    #[serde(other)]
    Unknown,
}

impl ErrorClass {
    /// The stable label stored in `dex_turn_outcomes.error_class`.
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorClass::BudgetExhausted => "budget_exhausted",
            ErrorClass::Truncated => "truncated",
            ErrorClass::Incomplete => "incomplete",
            ErrorClass::MalformedToolCall => "malformed_tool_call",
            ErrorClass::EmptyCompletion => "empty_completion",
            ErrorClass::Transport => "transport",
            ErrorClass::RateLimited => "rate_limited",
            ErrorClass::Unavailable => "unavailable",
            ErrorClass::Refusal => "refusal",
            ErrorClass::Auth => "auth",
            ErrorClass::Protocol => "protocol",
            ErrorClass::Resolve => "resolve",
            ErrorClass::Host => "host",
            ErrorClass::Rejected => "rejected",
            ErrorClass::Unknown => "unknown",
        }
    }

    /// Explicit wire-code boundary; human-readable details are never classified.
    pub fn of_gateway_code(code: &str) -> Self {
        match code {
            "rate_limit_error"
            | "rate_limit_exceeded"
            | "resource_exhausted"
            | "quota_exceeded"
            | "too_many_requests" => Self::RateLimited,
            "refusal"
            | "content_filter"
            | "content_filter_error"
            | "safety"
            | "policy_violation" => Self::Refusal,
            "upstream_unavailable"
            | "overloaded_error"
            | "unavailable"
            | "api_error"
            | "internal_error"
            | "server_error"
            | "timeout" => Self::Unavailable,
            "authentication_error" | "unauthorized" => Self::Auth,
            "invalid_request"
            | "invalid_request_error"
            | "bad_request"
            | "permission_error"
            | "forbidden"
            | "not_found" => Self::Rejected,
            _ => Self::Unknown,
        }
    }
}

/// Execution mode retained in durable ingress for compatibility and attribution.
/// Both modes now grant `NeedsApproval` through an exact `AutoApproved`
/// receipt. Neither mode overrides a hard policy `Deny`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    /// Interactive caller; approval-class calls receive a durable auto-approval.
    #[default]
    Interactive,
    /// Unattended caller; approval-class calls receive the same durable receipt.
    Headless,
}

impl ApprovalMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalMode::Interactive => "interactive",
            ApprovalMode::Headless => "headless",
        }
    }
}

/// The principal recorded on an `ApprovalDecided` the engine wrote itself
/// for a `Headless` turn. Kept so stored rows still decode; the engine no
/// longer writes it (see `AUTO_APPROVER`).
pub const HEADLESS_AUTO_APPROVER: &str = "policy:headless_auto_approve";

/// The principal recorded on every `AutoApproved` receipt. No human approves
/// a Dex tool call: a `NeedsApproval` verdict is granted by policy at once,
/// on every surface and for every principal, and the receipt is the audit
/// record of what ran, for whom, and under which argument digest.
pub const AUTO_APPROVER: &str = "policy:auto_approve";

/// Host-admitted turn policy, retained outside compactable model history.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionMode {
    #[default]
    Unspecified,
    Discuss,
    Implement,
}

impl InteractionMode {
    pub fn is_unspecified(&self) -> bool {
        *self == Self::Unspecified
    }
    pub(crate) fn tools_allowed(self) -> bool {
        self != Self::Discuss
    }
}

/// One row in a thread's log. Hosts append the ingress events (`UserMessage`,
/// `Steer`, `Interrupt`, `ApprovalDecided`, `Answer`, and optionally
/// `ToolProgress`); the engine appends everything else.
///
/// Surfaces render `TextDelta`, labels, summaries and questions. Tool names in
/// `ModelStepCompleted`, `ToolStarted` and `ToolsExposed` are internal and
/// for replay only.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// Starts a turn under `principal`.
    UserMessage {
        #[serde(default, skip_serializing_if = "InteractionMode::is_unspecified")]
        interaction_mode: InteractionMode,
        turn: TurnId,
        /// The source message this turn was sent for, when the host has one.
        /// `#[serde(default)] so log rows written before this field existed
        /// still deserialize, to `None`.
        #[serde(default)]
        message_id: Option<MessageId>,
        principal: PrincipalId,
        text: String,
        attachments: Vec<ArtifactRef>,
        /// Tools this turn's client session can execute locally, already
        /// resolved by the host against its allowlist. `#[serde(default)]`
        /// so log rows written before this field existed still deserialize.
        #[serde(default)]
        client_tools: Vec<ClientToolSpec>,
        /// Owner tools admitted by the authenticated host for this principal.
        /// Older log rows carry no authority. Public clients cannot set this.
        #[serde(default)]
        authorized_tools: Vec<ToolName>,
        /// Whether a human can answer this turn's approval requests. Older
        /// log rows carry none and replay as `Interactive`, the behaviour
        /// they were written under.
        #[serde(default)]
        approval_mode: ApprovalMode,
        /// Exact provider coordinates resolved by the authenticated host.
        /// Older turns retain the deployment default. This carries references,
        /// never credential values, and grants no Gateway authority.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_binding: Option<crate::ManagedInferenceProviderBinding>,
        /// The workspace writing policy and the sender's voice choice,
        /// resolved by the authenticated host. Prompt data only; grants no
        /// authority. Older turns carry none and render no voice.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        voice: Option<Box<crate::TurnVoice>>,
    },
    /// Control: becomes a user message from `principal` before the next model
    /// call. Calls the model then proposes act under `principal`.
    Steer {
        principal: PrincipalId,
        text: String,
    },
    /// Control: cancels the model stream and running read-only calls, and
    /// stops before the next effect. A started mutation completes.
    Interrupt {
        principal: PrincipalId,
    },
    /// Control: the decision on a parked call. Approval is necessary, not
    /// sufficient: policy runs again on resume, then `args_digest` must match.
    ApprovalDecided {
        call: CallId,
        approval: ApprovalId,
        args_digest: String,
        approved: bool,
        principal: PrincipalId,
    },
    /// Control: the user's reply to a `Question`.
    Answer {
        call: CallId,
        principal: PrincipalId,
        text: String,
        #[serde(default)]
        confirmation_decision: ConfirmationDecision,
        #[serde(default)]
        args_digest: String,
    },
    /// Control: a client session's outcome for one `ClientToolRequested`
    /// call. Only `Outcome::Succeeded` or `Outcome::Failed` are accepted
    /// from a client; the host validates that before appending this event.
    ClientToolResult {
        call: CallId,
        principal: PrincipalId,
        outcome: Outcome,
        output: String,
    },

    /// A model attempt begins. Steers at or before `control_through` were
    /// placed in history before this call.
    StepStarted {
        step: u32,
        control_through: Cursor,
    },
    /// Customer-safe model text (already through the `Sanitizer`). The `Log`
    /// coalesces deltas, so one row may hold many model chunks.
    TextDelta {
        text: String,
    },
    /// Customer-safe summary of the model's thinking in the current attempt
    /// (through the `Sanitizer`), for surfaces to show as live progress until
    /// the answer starts. Never part of the answer or of model history; the
    /// engine bounds how much one attempt emits.
    ThinkingDelta {
        text: String,
    },
    Usage(Usage),
    /// The commit point of a model attempt: its full text and every proposed
    /// call with full arguments, appended before any policy check or
    /// execution. An interrupted stream completes with the text so far and no
    /// calls.
    ModelStepCompleted {
        step: u32,
        text: String,
        calls: Vec<ProposedCall>,
        /// The step's provider continuation state (`ModelChunk::Reasoning`),
        /// returned in history on `Message::Assistant`. Internal: never
        /// rendered or sent to a surface. `#[serde(default)]` so log rows
        /// written before this field existed still deserialize, to `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning: Option<ProviderReasoning>,
        /// The provider and model that served the step
        /// (`ModelChunk::Served`); `None` for steps logged before the field
        /// existed or from a model port that does not report it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        served: Option<ServedBy>,
        /// Where the step's time went (`ModelChunk::Timing`); `None` for
        /// steps logged before the field existed, cut-off steps, or a model
        /// port that does not report it. Debug data: never part of model
        /// history.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timing: Option<StepTiming>,
    },
    /// A model attempt with no `ModelStepCompleted` (a crash mid-stream or a
    /// model failure). Its streamed text is dropped from model context;
    /// renderers remove it. The call is issued again as a new step.
    ModelAttemptAbandoned {
        step: u32,
    },
    /// A model attempt on one route ended without a `ModelStepCompleted`.
    /// Debug data for staff: never model history, never rendered. Carries
    /// only the route's configured provider/model, dex-model's fixed error
    /// code and timing; never the error message.
    ModelAttemptFailed {
        step: u32,
        provider: String,
        model: String,
        code: String,
        elapsed_ms: u64,
        then: AttemptNext,
    },

    /// Internal script journal. Nested calls retain exact principal, arguments
    /// and parent identity without becoming provider history. Written before
    /// any nested policy check or dispatch; normal tool rows carry outcomes.
    CodeModeCallsProposed {
        parent: CallId,
        calls: Vec<ProposedCall>,
    },
    /// Internal scratch write-ahead record. Published only by matching known success.
    CodeModeStorePrepared {
        parent: CallId,
        principal: PrincipalId,
        writes: agent_codemode::StoreWrites,
    },
    /// Internal marker for an exact specialist usage receipt in this atomic batch.
    ModelUsageResolved {
        call: CallId,
    },
    /// Owner usage was unavailable after a billable call. Finite budgets fail closed.
    ModelUsageUnresolved {
        call: CallId,
        reason: String,
    },
    ToolStarted {
        call: CallId,
        tool: ToolName,
        label: String,
        principal: PrincipalId,
    },
    /// Host-appended progress for a running call, e.g. "Starting computer".
    ToolProgress {
        call: CallId,
        label: String,
    },
    /// `tools.search` matched these tools; their schemas are offered to the
    /// model from the next step of this turn.
    ToolsExposed {
        call: CallId,
        tools: Vec<ToolName>,
    },
    ToolFinished {
        call: CallId,
        outcome: Outcome,
        output: Output,
        receipt: Option<ReceiptId>,
        /// Customer-safe owner template. Raw tool diagnostics never populate it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
    },
    /// Legacy: the turn was parked until an `ApprovalDecided` for this call
    /// arrived. Never emitted any more (policy grants at once and writes
    /// `AutoApproved`); still decoded from stored history. A call left in
    /// this state by an older deploy is auto-approved on its next run.
    ApprovalRequested {
        call: CallId,
        approval: ApprovalId,
        args_digest: String,
        summary: String,
    },
    /// The durable receipt for a call policy would once have parked for a
    /// human: granted at once by `AUTO_APPROVER`, never shown as a prompt.
    /// `summary` is what the approver would have read (a guardian flag is
    /// carried here too); `args_digest` binds the receipt to the exact
    /// arguments that ran. Not a control event: the engine writes it itself.
    AutoApproved {
        call: CallId,
        approval: ApprovalId,
        args_digest: String,
        summary: String,
        principal: PrincipalId,
    },
    /// The turn is parked until an `Answer` for this call arrives.
    Question {
        call: CallId,
        text: String,
        #[serde(default)]
        confirmation: Option<ActionConfirmation>,
    },
    /// The turn is parked until a `ClientToolResult` for this call arrives.
    /// The only event that carries tool arguments to a surface; the host
    /// delivers it only to the session that declared `tool`, never to every
    /// Watch subscriber on the thread.
    ClientToolRequested {
        call: CallId,
        /// The client-declared name from this session's `ClientToolSpec`,
        /// not an internal registry name.
        tool: ToolName,
        args: serde_json::Value,
        label: String,
        /// The principal whose authority this call runs under. A
        /// `ClientToolResult` for this call must come from the same
        /// principal.
        principal: PrincipalId,
        /// Currently the declaring principal's id: this API has no separate
        /// session identity yet, so it does not distinguish two concurrent
        /// sessions for the same principal. A real session id can replace
        /// this value without changing the event's shape.
        target_session: String,
        /// Unix milliseconds after which this call's client wait counts as
        /// lost. Set once, when the call is first requested, and never moved
        /// afterward: a restart rehydrates this same value from the log, so
        /// the wait's deadline survives the actor that started it. Rows
        /// written before this field existed have no deadline of their own;
        /// `#[serde(default)]` reads them back as never expiring rather than
        /// timing out every call still parked from before this field shipped.
        #[serde(default = "never_expires")]
        deadline_ms: i64,
    },
    /// History up to and including `covers_to_cursor` is replaced by `summary`.
    Compaction {
        covers_to_cursor: Cursor,
        summary: String,
    },

    /// The turn finished; `text` is the last step's text.
    Final {
        text: String,
    },
    Error {
        code: ErrorCode,
        message: String,
        /// Typed adapter classification; old rows and old readers remain compatible.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        class: Option<ErrorClass>,
    },
    Interrupted,
}

impl Event {
    /// Control events are the ones the engine reads back with
    /// `Log::control_since` while it runs.
    pub fn is_control(&self) -> bool {
        matches!(
            self,
            Event::Steer { .. }
                | Event::Interrupt { .. }
                | Event::ApprovalDecided { .. }
                | Event::Answer { .. }
                | Event::ClientToolResult { .. }
        )
    }
}

impl Event {
    /// Storage-adapter metadata for a lossless JSON-string event. JSONB
    /// cannot represent NUL codepoints directly, but it can store escaped
    /// JSON text inside a string. This key is outside the event vocabulary.
    pub const STORED_JSON_V1_KEY: &'static str = "_dex_event_json_v1";

    /// Intentionally unsupported by ordinary `Event` deserialization: an
    /// older reader must refuse an encoded row instead of executing its
    /// non-authoritative, sanitized projection.
    pub const STORED_JSON_V1_TYPE: &'static str = "_dex_event_json_v1";

    /// Explicit turn authority requires a reader that understands this mode.
    /// Older readers reject the outer marker before replaying the user message.
    pub const STORED_INTERACTION_MODE_V1_TYPE: &'static str = "_dex_event_interaction_mode_v1";

    /// Decode legacy event JSON or a lossless storage-adapter envelope.
    /// Durable readers pass the independently stored row kind. A malformed
    /// envelope never falls back to the projection, and its original tool
    /// argument digests are checked without rewriting any accepted value.
    pub fn from_stored_json(
        payload: &serde_json::Value,
        expected_kind: Option<&str>,
    ) -> Result<Self, serde_json::Error> {
        let encoded = payload.get(Self::STORED_JSON_V1_KEY);
        let event: Self = match encoded {
            Some(exact) => {
                let marker = payload.get("type").and_then(serde_json::Value::as_str);
                if !matches!(
                    marker,
                    Some(Self::STORED_JSON_V1_TYPE | Self::STORED_INTERACTION_MODE_V1_TYPE)
                ) {
                    return Err(serde::de::Error::custom(
                        "encoded event has no storage type marker",
                    ));
                }
                let exact: String = serde_json::from_value(exact.clone())?;
                let event: Self = serde_json::from_str(&exact)?;
                if marker == Some(Self::STORED_INTERACTION_MODE_V1_TYPE)
                    && !matches!(
                        &event,
                        Self::UserMessage {
                            interaction_mode: InteractionMode::Discuss | InteractionMode::Implement,
                            ..
                        }
                    )
                {
                    return Err(serde::de::Error::custom(
                        "mode storage marker requires an explicit-mode user message",
                    ));
                }
                event
            }
            None => serde_json::from_value(payload.clone())?,
        };
        if let Some(expected_kind) = expected_kind
            && serde_json::to_value(&event)?
                .get("type")
                .and_then(serde_json::Value::as_str)
                != Some(expected_kind)
        {
            return Err(serde::de::Error::custom(
                "event type does not match its stored row kind",
            ));
        }
        if let Self::ModelStepCompleted { calls, .. } | Self::CodeModeCallsProposed { calls, .. } =
            &event
            && calls
                .iter()
                .any(|call| call.args_digest != args_digest(&call.args))
        {
            return Err(serde::de::Error::custom(
                "event tool arguments do not match their digest",
            ));
        }
        Ok(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_storage_marker_rejects_missing_authority_or_wrong_event() {
        let envelope = |exact: serde_json::Value| {
            serde_json::json!({
                "type": Event::STORED_INTERACTION_MODE_V1_TYPE,
                Event::STORED_JSON_V1_KEY: serde_json::to_string(&exact).unwrap()
            })
        };
        let user = serde_json::json!({"type": "user_message", "turn": "t",
            "principal": "alice", "text": "plan", "attachments": []});
        for mode in [None, Some("unspecified")] {
            let mut exact = user.clone();
            if let Some(mode) = mode {
                exact["interaction_mode"] = mode.into();
            }
            assert!(Event::from_stored_json(&envelope(exact), Some("user_message")).is_err());
        }
        assert!(
            Event::from_stored_json(&envelope(serde_json::json!({"type":"interrupted"})), None)
                .is_err()
        );
        let mut exact = user;
        exact["interaction_mode"] = "discuss".into();
        let valid = envelope(exact);
        assert!(Event::from_stored_json(&valid, Some("user_message")).is_ok());
        assert!(Event::from_stored_json(&valid, Some("interrupted")).is_err());
        let mut malformed = valid.clone();
        malformed[Event::STORED_JSON_V1_KEY] = "invalid JSON".into();
        assert!(Event::from_stored_json(&malformed, None).is_err());
        malformed
            .as_object_mut()
            .unwrap()
            .remove(Event::STORED_JSON_V1_KEY);
        assert!(Event::from_stored_json(&malformed, None).is_err());
    }

    #[test]
    fn accepted_model_binding_survives_event_round_trip() {
        let input = serde_json::json!({
            "type": "user_message", "turn": "turn-1", "principal": "user-1",
            "text": "hello", "attachments": [],
            "model_binding": {
                "provider": "vertex-ai", "model": "gemini-selected",
                "provider_environment": "production", "credential_name": "gemini-ref",
                "team_id": "team-1"
            }
        });
        let event: Event = serde_json::from_value(input.clone()).expect("accepted message");
        let replay = serde_json::to_value(event).expect("durable event");
        assert_eq!(replay["model_binding"], input["model_binding"]);
        assert!(
            replay.get("voice").is_none(),
            "older rows stay byte-identical"
        );
    }

    #[test]
    fn accepted_turn_voice_survives_event_round_trip() {
        let input = serde_json::json!({
            "type": "user_message", "turn": "turn-1", "principal": "user-1",
            "text": "hello", "attachments": [],
            "voice": {
                "policy": {
                    "guide_version": 3,
                    "response_guidance": "Use observed execution evidence",
                    "required_terms": ["Deixic"],
                    "voice": {"kind": "neutral"}
                },
                "tone": ["formal"]
            }
        });
        let event: Event = serde_json::from_value(input.clone()).expect("accepted message");
        let Event::UserMessage { voice, .. } = &event else {
            panic!("user message");
        };
        let voice = voice.as_ref().expect("voice");
        assert_eq!(voice.tone, vec![crate::ToneAdjustment::Formal]);
        assert_eq!(
            voice.policy.as_ref().map(|policy| &policy.voice),
            Some(&crate::TurnVoiceChoice::Neutral)
        );
        let replay = serde_json::to_value(event).expect("durable event");
        assert_eq!(replay["voice"]["tone"], input["voice"]["tone"]);
        assert_eq!(
            replay["voice"]["policy"]["response_guidance"],
            input["voice"]["policy"]["response_guidance"]
        );
        assert_eq!(
            replay["voice"]["policy"]["required_terms"],
            input["voice"]["policy"]["required_terms"]
        );
        assert_eq!(
            replay["voice"]["policy"]["voice"],
            input["voice"]["policy"]["voice"]
        );
    }

    #[test]
    fn accepted_voice_blend_survives_durable_event_replay() {
        let input = serde_json::json!({
            "type": "user_message", "turn": "turn-1", "principal": "user-1",
            "text": "hello", "attachments": [],
            "voice": { "policy": { "guide_version": 9, "voice": {
                "kind": "blend", "voices": [
                    {"voice_id": "exec", "name": "Executive", "guidance": "Decision first", "version": 2, "explicit": true},
                    {"voice_id": "care", "name": "Care", "guidance": "Warm and patient", "version": 4, "explicit": true}
                ]
            } } }
        });
        let event: Event = serde_json::from_value(input.clone()).unwrap();
        let encoded = serde_json::to_string(&event).unwrap();
        let replay: Event = serde_json::from_str(&encoded).unwrap();
        assert_eq!(
            serde_json::to_value(replay).unwrap()["voice"]["policy"]["voice"],
            input["voice"]["policy"]["voice"]
        );
    }

    #[test]
    fn model_attempt_failed_round_trips_as_snake_case_json() {
        let event = Event::ModelAttemptFailed {
            step: 2,
            provider: "vertex-ai".into(),
            model: "gemini-3.6-flash".into(),
            code: "provider_unavailable".into(),
            elapsed_ms: 1500,
            then: AttemptNext::Failover,
        };
        let json = serde_json::to_value(&event).expect("serialize");
        assert_eq!(json["type"], "model_attempt_failed");
        assert_eq!(json["then"], "failover");
        assert_eq!(
            serde_json::from_value::<Event>(json).expect("decode"),
            event
        );
        assert!(!event.is_control());
        for (then, name) in [
            (AttemptNext::Retry, "retry"),
            (AttemptNext::Abandon, "abandon"),
        ] {
            assert_eq!(serde_json::to_value(then).expect("serialize"), name);
        }
    }

    #[test]
    fn events_round_trip_through_json() {
        let events = vec![
            Event::UserMessage {
                interaction_mode: crate::InteractionMode::Unspecified,
                turn: TurnId::new("t1"),
                message_id: Some(MessageId::new("m1")),
                principal: PrincipalId::new("alice"),
                text: "hi".into(),
                attachments: vec![ArtifactRef::new("a1")],
                authorized_tools: Vec::new(),
                model_binding: None,
                voice: None,
                approval_mode: ApprovalMode::Interactive,
                client_tools: vec![ClientToolSpec {
                    name: ToolName::new("browser.read_tab"),
                    schema: serde_json::json!({"type": "object"}),
                    read_only: true,
                    label: "Reading your tab".into(),
                }],
            },
            Event::ClientToolRequested {
                call: CallId::new("t1-1-0"),
                tool: ToolName::new("browser.read_tab"),
                args: serde_json::json!({}),
                label: "Reading your tab".into(),
                principal: PrincipalId::new("alice"),
                target_session: "alice".into(),
                deadline_ms: 1_700_000_000_000,
            },
            Event::ClientToolResult {
                call: CallId::new("t1-1-0"),
                principal: PrincipalId::new("alice"),
                outcome: Outcome::Succeeded,
                output: "ok".into(),
            },
            Event::Usage(Usage {
                input_tokens: 1,
                output_tokens: 2,
                cost_micros: 3,
                ..Usage::default()
            }),
            Event::ModelStepCompleted {
                step: 1,
                text: "ok".into(),
                calls: vec![ProposedCall::new(
                    CallId::new("t1-1-0"),
                    ToolName::new("search"),
                    serde_json::json!({"q": "x"}),
                    PrincipalId::new("alice"),
                )],
                reasoning: None,
                served: None,
                timing: None,
            },
            Event::ModelStepCompleted {
                step: 2,
                text: String::new(),
                calls: Vec::new(),
                reasoning: Some(ProviderReasoning {
                    format: "google.gemini.v1".into(),
                    model: "gemini-3.6-flash".into(),
                    payload: serde_json::json!({"calls": [{"thought_signature": "c2ln"}]}),
                }),
                served: Some(ServedBy {
                    provider: "vertex-ai".into(),
                    model: "gemini-3.6-flash".into(),
                }),
                timing: None,
            },
            Event::ToolFinished {
                call: CallId::new("t1-1-0"),
                outcome: Outcome::Unknown,
                output: Output::Text("outcome unknown".into()),
                receipt: None,
                summary: None,
            },
            Event::Error {
                class: None,
                code: ErrorCode::BudgetExhausted,
                message: "steps".into(),
            },
            Event::Interrupt {
                principal: PrincipalId::new("bob"),
            },
            Event::Interrupted,
        ];
        for event in events {
            let json = serde_json::to_string(&event).expect("serialize");
            let back: Event = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, event, "{json}");
        }
    }

    #[test]
    fn a_client_tool_requested_row_written_before_deadline_ms_existed_never_expires() {
        let json = serde_json::json!({
            "type": "client_tool_requested",
            "call": "t1-1-0",
            "tool": "browser.read_tab",
            "args": {},
            "label": "Reading your tab",
            "principal": "alice",
            "target_session": "alice",
        })
        .to_string();
        let event: Event = serde_json::from_str(&json).expect("deserialize");
        match event {
            Event::ClientToolRequested { deadline_ms, .. } => {
                assert_eq!(deadline_ms, i64::MAX);
            }
            other => panic!("expected ClientToolRequested, got {other:?}"),
        }
    }

    #[test]
    fn user_message_without_message_id_round_trips_to_none() {
        let event = Event::UserMessage {
            interaction_mode: crate::InteractionMode::Unspecified,
            turn: TurnId::new("t1"),
            message_id: None,
            principal: PrincipalId::new("alice"),
            text: "hi".into(),
            attachments: vec![],
            client_tools: vec![],
            authorized_tools: Vec::new(),
            model_binding: None,
            voice: None,
            approval_mode: ApprovalMode::Interactive,
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let back: Event = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, event);

        // A log row written before `message_id` existed has no such key at
        // all: `#[serde(default)]` must still deserialize it, to `None`,
        // rather than fail.
        let old_row = serde_json::json!({
            "type": "user_message",
            "turn": "t1",
            "principal": "alice",
            "text": "hi",
            "attachments": [],
        });
        let back: Event =
            serde_json::from_value(old_row).expect("old row without message_id deserializes");
        assert_eq!(
            back,
            Event::UserMessage {
                interaction_mode: crate::InteractionMode::Unspecified,
                turn: TurnId::new("t1"),
                message_id: None,
                principal: PrincipalId::new("alice"),
                text: "hi".into(),
                attachments: vec![],
                client_tools: vec![],
                authorized_tools: Vec::new(),
                model_binding: None,
                voice: None,
                approval_mode: ApprovalMode::Interactive,
            }
        );
    }

    /// A step logged before `reasoning` existed has no such key: it still
    /// deserializes, to `None`, and a step without reasoning is written
    /// without the key, so old readers see the row they always did.
    #[test]
    fn a_model_step_row_without_reasoning_replays_to_none() {
        let old_row = serde_json::json!({
            "type": "model_step_completed",
            "step": 1,
            "text": "Let me check.",
            "calls": [{
                "id": "t1-1-0",
                "tool": "search",
                "args": {"q": "x"},
                "args_digest": args_digest(&serde_json::json!({"q": "x"})),
                "principal": "alice",
            }],
        });
        let event: Event =
            serde_json::from_value(old_row.clone()).expect("old step row deserializes");
        let Event::ModelStepCompleted { reasoning, .. } = &event else {
            panic!("expected ModelStepCompleted, got {event:?}");
        };
        assert_eq!(*reasoning, None);
        assert_eq!(serde_json::to_value(&event).expect("serialize"), old_row);
    }

    /// `Debug` (and so any `{:?}` of an event) never prints the payload.
    #[test]
    fn reasoning_debug_omits_the_payload() {
        let reasoning = ProviderReasoning {
            format: "anthropic.messages.v1".into(),
            model: "claude-opus-5-5".into(),
            payload: serde_json::json!({"content": [{"type": "thinking", "thinking": "tenant secret"}]}),
        };
        let printed = format!("{reasoning:?}");
        assert!(!printed.contains("tenant secret"), "{printed}");
        assert!(printed.contains("anthropic.messages.v1"), "{printed}");
    }

    #[test]
    fn digest_depends_only_on_args() {
        let a = ProposedCall::new(
            CallId::new("c"),
            ToolName::new("t"),
            serde_json::json!({"q": "x"}),
            PrincipalId::new("alice"),
        );
        let b = ProposedCall::new(
            CallId::new("other"),
            ToolName::new("t"),
            serde_json::json!({"q": "x"}),
            PrincipalId::new("bob"),
        );
        let c = ProposedCall::new(
            CallId::new("c"),
            ToolName::new("t"),
            serde_json::json!({"q": "y"}),
            PrincipalId::new("alice"),
        );
        assert_eq!(a.args_digest, b.args_digest);
        assert_ne!(a.args_digest, c.args_digest);
        assert_eq!(a.args_digest.len(), 64);
    }

    /// Rows written before `timing` and the cache token counts existed still
    /// decode, to `None` and zero; new rows round-trip.
    #[test]
    fn old_rows_without_timing_or_cache_counts_decode() {
        let old_step = serde_json::json!({
            "type": "model_step_completed", "step": 1, "text": "hi", "calls": []
        });
        let Event::ModelStepCompleted { timing, .. } =
            serde_json::from_value::<Event>(old_step).expect("old step row")
        else {
            panic!("expected ModelStepCompleted");
        };
        assert_eq!(timing, None);

        let old_usage = serde_json::json!({
            "type": "usage", "input_tokens": 3, "output_tokens": 4, "cost_micros": 5
        });
        assert_eq!(
            serde_json::from_value::<Event>(old_usage).expect("old usage row"),
            Event::Usage(Usage {
                input_tokens: 3,
                output_tokens: 4,
                cost_micros: 5,
                ..Usage::default()
            })
        );
    }

    #[test]
    fn step_timing_and_cache_counts_round_trip_and_none_is_omitted() {
        let step = Event::ModelStepCompleted {
            step: 2,
            text: String::new(),
            calls: Vec::new(),
            reasoning: None,
            served: None,
            timing: Some(StepTiming {
                prepare_ms: 1,
                mint_ms: 2,
                headers_ms: 3,
                first_event_ms: Some(4),
                first_text_ms: None,
                total_ms: 5,
            }),
        };
        let value = serde_json::to_value(&step).expect("encode");
        assert_eq!(
            serde_json::from_value::<Event>(value).expect("decode"),
            step
        );
        let untimed = Event::ModelStepCompleted {
            step: 2,
            text: String::new(),
            calls: Vec::new(),
            reasoning: None,
            served: None,
            timing: None,
        };
        let text = serde_json::to_string(&untimed).expect("encode");
        assert!(!text.contains("timing"), "{text}");

        let usage = Event::Usage(Usage {
            input_tokens: 10,
            output_tokens: 2,
            cost_micros: 1,
            cache_read_input_tokens: 7,
            cache_creation_input_tokens: 1,
        });
        let value = serde_json::to_value(&usage).expect("encode");
        assert_eq!(
            serde_json::from_value::<Event>(value).expect("decode"),
            usage
        );
    }
    #[test]
    fn usage_rows_written_before_the_cache_split_still_decode() {
        let old: Usage =
            serde_json::from_str(r#"{"input_tokens":5,"output_tokens":2,"cost_micros":9}"#)
                .expect("old row decodes");
        assert_eq!(
            old,
            Usage {
                input_tokens: 5,
                output_tokens: 2,
                cost_micros: 9,
                ..Usage::default()
            }
        );
        // A zero split is not written, so unsplit providers keep their shape.
        let json = serde_json::to_value(old).expect("encodes");
        assert_eq!(
            json,
            serde_json::json!({"input_tokens":5,"output_tokens":2,"cost_micros":9})
        );
        let split = Usage {
            cache_read_input_tokens: 3,
            ..old
        };
        let back: Usage =
            serde_json::from_value(serde_json::to_value(split).expect("encodes")).expect("decodes");
        assert_eq!(back, split);
    }

    #[test]
    fn old_error_rows_do_not_classify_human_readable_prose() {
        let old = serde_json::json!({"type":"error", "code":"model_failed", "message":"rate_limit_error: arbitrary prose"});
        assert!(matches!(
            serde_json::from_value::<Event>(old).unwrap(),
            Event::Error { class: None, .. }
        ));
        assert_eq!(
            serde_json::to_string(&ErrorCode::ModelFailed).unwrap(),
            r#""model_failed""#
        );
        assert_eq!(
            ErrorClass::of_gateway_code("not_a_rate_limit_error"),
            ErrorClass::Unknown
        );
        assert_eq!(
            ErrorClass::of_gateway_code("rate_limit_error"),
            ErrorClass::RateLimited
        );
    }
}
