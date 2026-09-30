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

/// Model spend reported by one model response.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_micros: u64,
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
    }
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

/// Who can answer a turn's approval requests.
///
/// `Headless` turns come from callers with no human to click Approve (service
/// accounts, workloads, agents, synthetic canaries, API automation). For them
/// the engine resolves a policy `NeedsApproval` verdict itself, recording the
/// request and the decision in the log. A `Deny` verdict is never affected.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    /// A human decides: the turn parks on `ApprovalRequested`.
    #[default]
    Interactive,
    /// No human: policy approves what would otherwise ask.
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
    },
    /// A model attempt with no `ModelStepCompleted` (a crash mid-stream or a
    /// model failure). Its streamed text is dropped from model context;
    /// renderers remove it. The call is issued again as a new step.
    ModelAttemptAbandoned {
        step: u32,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_round_trip_through_json() {
        let events = vec![
            Event::UserMessage {
                turn: TurnId::new("t1"),
                message_id: Some(MessageId::new("m1")),
                principal: PrincipalId::new("alice"),
                text: "hi".into(),
                attachments: vec![ArtifactRef::new("a1")],
                authorized_tools: Vec::new(),
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
            },
            Event::ToolFinished {
                call: CallId::new("t1-1-0"),
                outcome: Outcome::Unknown,
                output: Output::Text("outcome unknown".into()),
                receipt: None,
            },
            Event::Error {
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
    fn approval_mode_defaults_to_interactive_for_old_rows_and_round_trips() {
        let old_row = serde_json::json!({
            "type": "user_message",
            "turn": "t1",
            "principal": "alice",
            "text": "hi",
            "attachments": [],
        });
        match serde_json::from_value::<Event>(old_row).expect("old row deserializes") {
            Event::UserMessage { approval_mode, .. } => {
                assert_eq!(approval_mode, ApprovalMode::Interactive);
            }
            other => panic!("expected UserMessage, got {other:?}"),
        }
        assert_eq!(
            serde_json::to_value(ApprovalMode::Headless).expect("serialize"),
            serde_json::json!("headless")
        );
        assert_eq!(
            serde_json::from_value::<ApprovalMode>(serde_json::json!("interactive"))
                .expect("deserialize"),
            ApprovalMode::Interactive
        );
    }

    #[test]
    fn user_message_without_message_id_round_trips_to_none() {
        let event = Event::UserMessage {
            turn: TurnId::new("t1"),
            message_id: None,
            principal: PrincipalId::new("alice"),
            text: "hi".into(),
            attachments: vec![],
            client_tools: vec![],
            authorized_tools: Vec::new(),
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
                turn: TurnId::new("t1"),
                message_id: None,
                principal: PrincipalId::new("alice"),
                text: "hi".into(),
                attachments: vec![],
                client_tools: vec![],
                authorized_tools: Vec::new(),
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
}
