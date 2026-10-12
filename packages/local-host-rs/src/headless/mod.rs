//! Headless protocol for communicating with the Node.js agent.
//!
//! This module implements the client side of the headless protocol, spawning the Node.js
//! agent process and communicating via JSON-over-stdio. It provides a multi-layered
//! architecture for inter-process communication (IPC) between Rust and Node.js.
//!
//! # Architecture Overview
//!
//! The headless module provides several layers of abstraction:
//!
//! 1. **Messages** - Protocol message types (ToAgentMessage, FromAgentMessage)
//! 2. **Framing** - Reliable message framing (newline-delimited or length-prefixed)
//! 3. **Transport** - Low-level subprocess communication (sync and async)
//! 4. **Session** - Session persistence (JSONL recording and replay)
//! 5. **Supervisor** - High-level wrapper with reconnection and health monitoring
//!
//! # Protocol Design
//!
//! ## Message-Based Communication
//!
//! The protocol uses a message-passing model where the Rust TUI and Node.js agent exchange
//! structured messages serialized as JSON. This design provides:
//!
//! - **Type safety** via Rust's type system and serde serialization
//! - **Language interoperability** through a language-agnostic JSON format
//! - **Structured data** with well-defined message schemas
//! - **Versioning support** through the `type` discriminator field
//!
//! ## JSON Message Framing
//!
//! Messages are framed using one of two protocols:
//!
//! ### Newline-Delimited JSON (Default)
//!
//! Each message is a single line of JSON terminated by a newline character:
//!
//! ```text
//! {"type":"prompt","content":"Hello"}\n
//! {"type":"response_chunk","response_id":"abc","content":"Hi","is_thinking":false}\n
//! ```
//!
//! This format is:
//! - **Human-readable** for debugging
//! - **Simple to implement** in any language
//! - **Compatible** with line-buffered I/O
//! - **Self-synchronizing** - lost frames don't corrupt subsequent messages
//!
//! ### Length-Prefixed Binary (Optional)
//!
//! For high-performance scenarios, messages can be framed with a 4-byte length prefix:
//!
//! ```text
//! [4-byte big-endian length][JSON bytes]
//! ```
//!
//! This format:
//! - **Avoids scanning** for newlines in large messages
//! - **Supports binary data** within JSON (base64-encoded)
//! - **Predictable performance** - no worst-case linear scanning
//!
//! ## Subprocess Communication Model
//!
//! The transport layer spawns the Node.js agent as a child process and uses:
//!
//! - **stdin** for sending messages (Rust -> Node.js)
//! - **stdout** for receiving messages (Node.js -> Rust)
//! - **stderr** for diagnostic output (inherited from parent)
//!
//! This design allows the agent to run independently while the TUI maintains control
//! over its lifecycle.
//!
//! # Rust Concepts Demonstrated
//!
//! ## Serde Serialization
//!
//! The protocol leverages [serde](https://serde.rs/) for zero-copy, type-safe serialization:
//!
//! ```rust,ignore
//! #[derive(Serialize, Deserialize)]
//! #[serde(tag = "type", rename_all = "snake_case")]
//! enum ToAgentMessage {
//!     Prompt { content: String },
//!     // ... other variants
//! }
//! ```
//!
//! Key features:
//! - **Tagged enums** - `#[serde(tag = "type")]` adds a discriminator field
//! - **Field renaming** - `rename_all = "snake_case"` converts Rust names to JSON convention
//! - **Optional fields** - `#[serde(skip_serializing_if = "Option::is_none")]` omits None values
//!
//! ## Async I/O with Tokio
//!
//! The async transport uses [tokio](https://tokio.rs/) for non-blocking I/O:
//!
//! ```rust,ignore
//! let mut reader = AsyncFrameReader::new(stdout);
//! let event = reader.read_message::<FromAgentMessage>().await?;
//! ```
//!
//! Benefits:
//! - **Concurrent operations** - read and write simultaneously without threads
//! - **Resource efficiency** - single thread handles multiple I/O operations
//! - **Cancellation support** - async tasks can be cancelled cleanly
//!
//! ## Buffered I/O
//!
//! Both sync and async implementations use buffered readers/writers:
//!
//! ```rust,ignore
//! let reader = BufReader::with_capacity(64 * 1024, stdout);
//! ```
//!
//! This reduces system call overhead by:
//! - **Batching reads** - fetch data in larger chunks
//! - **Reducing context switches** - fewer transitions between user/kernel mode
//! - **Improving throughput** - especially important for high-frequency messages
//!
//! ## Thread-Based Concurrency (Sync Transport)
//!
//! The synchronous transport uses OS threads for concurrent reading and writing:
//!
//! ```rust,ignore
//! thread::spawn(move || {
//!     // Reader thread processes stdout
//! });
//! thread::spawn(move || {
//!     // Writer thread processes stdin
//! });
//! ```
//!
//! This pattern:
//! - **Decouples I/O** - read and write operate independently
//! - **Handles blocking** - one blocked operation doesn't stall the other
//! - **Communicates via channels** - thread-safe message passing with mpsc
//!
//! # Usage Examples
//!
//! ## Simple Async Transport
//!
//! For basic use cases, use `AsyncAgentTransport`:
//!
//! ```ignore
//! use maestro_local_host::headless::{AsyncAgentTransportBuilder, ToAgentMessage};
//!
//! let mut transport = AsyncAgentTransportBuilder::new()
//!     .cli_path("maestro")
//!     .cwd("/path/to/project")
//!     .spawn()
//!     .await?;
//!
//! // Send a prompt
//! transport.prompt("Hello!")?;
//!
//! // Process response stream
//! while let Ok(event) = transport.recv().await {
//!     match event {
//!         AgentEvent::ResponseChunk { content, .. } => print!("{}", content),
//!         AgentEvent::ResponseEnd { .. } => { /* model-call boundary */ }
//!         AgentEvent::TurnCompleted { .. } => break,
//!         AgentEvent::TurnInterrupted { reason, .. } => return Err(reason.into()),
//!         AgentEvent::ProviderError { message, .. } => return Err(message.into()),
//!         _ => {}
//!     }
//! }
//! ```
//!
//! ## Production Supervisor with Session Recording
//!
//! For production use with reconnection and session recording, use `AgentSupervisor`:
//!
//! ```ignore
//! use maestro_local_host::headless::{SupervisorBuilder, SessionRecorder};
//!
//! // Create session recorder for persistence
//! let recorder = SessionRecorder::new("~/.composer/sessions")?;
//!
//! // Build supervisor with reconnection
//! let mut supervisor = SupervisorBuilder::new()
//!     .cli_path("maestro")
//!     .session_recorder(recorder)
//!     .max_reconnect_attempts(5)
//!     .build();
//!
//! // Connect and use
//! supervisor.connect().await?;
//! supervisor.prompt("Hello!")?;
//! ```
//!
//! # Thread Safety
//!
//! The transport types are designed for different concurrency models:
//!
//! - **AgentTransport** - Uses `mpsc::Sender` which is `Send` but not `Sync`
//! - **AsyncAgentTransport** - Futures are `Send` if used from a single task
//! - **SessionRecorder** - Not thread-safe; use one per transport
//!
//! For multi-threaded scenarios, wrap transports in `Arc<Mutex<_>>` or use the
//! async transport from a single task.

use tokio::sync::{Notify, mpsc};

mod async_transport;
pub mod controller_binding;
#[cfg(test)]
mod controller_binding_test;
mod framing;
mod generated_protocol;
pub mod messages;
mod proto;
mod remote_transport;
mod session;
mod supervisor;
mod transport;
pub mod workspace_capabilities;
#[cfg(test)]
mod workspace_capabilities_test;

fn local_controller_capabilities() -> messages::ClientCapabilities {
    messages::ClientCapabilities {
        server_requests: Some(vec![
            messages::ServerRequestType::Approval,
            messages::ServerRequestType::UserInput,
            messages::ServerRequestType::ToolRetry,
        ]),
        utility_operations: Some(vec![messages::UtilityOperation::CommandExec]),
        raw_agent_events: None,
        transcript_grade: Some(crate::transcript::TranscriptGrade::Delta),
        governed_code_mode: None,
    }
}

/// Build the runtime-owned server capability surface for headless clients.
///
/// The native registry is the only source of the tool names and baseline
/// approval metadata. A client must not recreate this list or infer a
/// per-call decision from it; the native tool event remains authoritative for
/// argument-aware approval and action-firewall outcomes.
pub fn native_server_capabilities() -> messages::ServerCapabilities {
    let mut native_tools = crate::agent::tool_host::tool_host()
        .registry_tool_definitions()
        .into_iter()
        .map(|definition| {
            let name = definition.tool.name.clone();
            messages::NativeToolCapability {
                requires_approval: definition.requires_approval,
                version: crate::tools::versions::is_version_managed(&name)
                    .then(|| "current".to_string()),
                name,
            }
        })
        .collect::<Vec<_>>();
    native_tools.sort_unstable_by(|left, right| left.name.cmp(&right.name));

    messages::ServerCapabilities {
        server_requests: vec![
            messages::ServerRequestType::Approval,
            messages::ServerRequestType::ClientTool,
            messages::ServerRequestType::UserInput,
            messages::ServerRequestType::ToolRetry,
        ],
        utility_operations: vec![
            messages::UtilityOperation::CommandExec,
            messages::UtilityOperation::FileSearch,
            messages::UtilityOperation::FileRead,
            messages::UtilityOperation::FileWatch,
        ],
        raw_agent_events: true,
        connection_roles: vec![
            messages::ConnectionRole::Viewer,
            messages::ConnectionRole::Controller,
        ],
        native_tools,
        workspace_prompt_capability_activation: true,
        governed_tool_grant_algorithms: crate::headless_server::governed_grant_verifier_algorithms(
        )
        .into_iter()
        .map(str::to_string)
        .collect(),
    }
}

// Core message types
pub use maestro_runtime::{
    DelegationControlAction, DelegationControlProjection, DelegationControlState, DelegationEvent,
    DelegationEventKind, DelegationLifecycleState, TaggedMessageDecode, TaggedMessageDecodeError,
    UnknownWireMessage,
};
pub use messages::{
    ActiveTool, AgentEvent, AgentState, ApprovalMode, ClientCapabilities, ClientInfo,
    ClientToolExecutionOwner, ClientToolResultContent, CodeMode, ConnectionGrantBinding,
    ConnectionRole, ExternalToolDefinition, FromAgentMessage, GovernedToolGrant,
    HEADLESS_PROTOCOL_VERSION, HeadlessErrorType, HistoryMessage, HistoryRole, InitConfig,
    NativeToolCapability, PendingApproval, ServerCapabilities, ServerRequestResolutionStatus,
    ServerRequestResolvedBy, ServerRequestType, StreamingResponse, ThinkingLevel, ToAgentMessage,
    TokenUsage, ToolResult, UtilityCommandShellMode, UtilityCommandStream, UtilityOperation,
    decode_from_agent_message,
};
pub use proto::maestro::v1 as proto_types;

// Sync transport
pub use transport::{AgentTransport, AgentTransportBuilder, TransportConfig, TransportError};

// Async transport
pub use async_transport::{
    AsyncAgentTransport, AsyncAgentTransportBuilder, AsyncTransportConfig, AsyncTransportError,
};

// Remote transport
pub use remote_transport::{RemoteAgentTransport, RemoteIncoming, RemoteTransportConfig};

// Session persistence
pub use session::{
    SessionEntry, SessionMetadata, SessionReader, SessionRecorder, SessionReplay, delete_session,
    list_sessions,
};

// Supervisor with reconnection
pub use supervisor::{
    AgentSupervisor, HealthStatus, SupervisorBuilder, SupervisorConfig, SupervisorEvent,
    agent_event_to_message,
};

pub(crate) fn send_transport_event<T>(
    sender: &mpsc::UnboundedSender<T>,
    notification: &Notify,
    event: T,
) -> bool {
    if sender.send(event).is_ok() {
        notification.notify_one();
        true
    } else {
        false
    }
}
// Internal-only: a best-effort, off-the-hot-path stderr diagnostic helper
// shared with `tools::registry::execute` and `agent::native`, not part of
// this crate's public surface.
pub(crate) use supervisor::report_diagnostic_nonblocking;
pub use supervisor::{ResponseAcknowledgement, response_ack_request_id};

// Message framing
pub use framing::{
    AsyncFrameReader, AsyncFrameWriter, FrameReader, FrameWriter, FramingError, FramingMode,
    MAX_MESSAGE_SIZE,
};
