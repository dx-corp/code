//! # Application State Module
//!
//! This module manages the central state of the Maestro TUI application.
//! It contains all mutable data that changes during program execution,
//! including messages, UI state, and agent status.
//!
//! ## Rust Concept: Centralized State
//!
//! Unlike component-based frameworks (React, Vue), this uses a single state
//! struct pattern. All state lives in `AppState`, which is passed around
//! by mutable reference (`&mut AppState`). This makes state changes explicit
//! and avoids the complexity of distributed state management.
//!
//! ## Why This Design?
//!
//! 1. **Single Source of Truth**: All state is in one place
//! 2. **No Hidden State**: Every field is visible in the struct
//! 3. **Easy Serialization**: Can easily save/restore entire state
//! 4. **Predictable Updates**: All mutations go through methods

// ─────────────────────────────────────────────────────────────────────────────
// IMPORTS
// ─────────────────────────────────────────────────────────────────────────────

pub use maestro_local_host::state::{ApprovalMode, OutputDetail, QueueMode};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::time::{Instant, SystemTime};
// `Instant` is for measuring elapsed time (monotonic clock - always goes forward)
// `SystemTime` is wall-clock time (can go backwards if system time changes)
// We use `Instant` for UI timers (elapsed seconds) and `SystemTime` for timestamps

use crate::agent::{ExecutionStatus, FromAgent, TokenUsage};
use crate::kill_ring::{KillRing, next_word_start, previous_word_start};
use crate::session::ThinkingLevel;

// Import from our own crate using `crate::` prefix
// `FromAgent` is an enum of all messages the agent can send us

use crate::components::message_layout::{MessageLayout, MessageLayoutCache};
use crate::components::textarea::{PASTE_FOLD_MIN_CHARS, PASTE_FOLD_MIN_LINES, TextArea};
// Our multi-line text input component

// ─────────────────────────────────────────────────────────────────────────────
// MESSAGE TYPES
// ─────────────────────────────────────────────────────────────────────────────

/// A chat message in the conversation.
///
/// # Rust Concepts Used
///
/// - **`#[derive(...)]`**: Automatically implements traits:
///   - `Debug`: Enables `{:?}` formatting for printing
///   - `Clone`: Enables `.clone()` to create copies
///
/// - **Struct Fields**: All fields are `pub` (public), meaning code outside
///   this module can read and write them directly. This is a design choice -
///   for more encapsulation, we could use private fields with getter methods.
///
/// - **`String` vs `&str`**: We use `String` (owned) here because:
///   1. Messages live for an unknown duration
///   2. Content can be modified (appended to during streaming)
///   3. We need to store them in a `Vec`
///
///   If we used `&str` (borrowed), we'd need lifetime annotations and
///   couldn't modify the content.
#[derive(Debug, Clone)]
pub struct Message {
    /// Unique ID for this message.
    /// Used to find and update specific messages during streaming.
    pub id: String,

    /// Who sent this message (User or Assistant).
    pub role: MessageRole,

    /// Visual/message semantics used by the TUI.
    pub kind: MessageKind,

    /// The main message content.
    /// For streaming messages, this is appended to chunk by chunk.
    pub content: String,

    /// Thinking/reasoning content (for models that support extended thinking).
    /// Claude's extended thinking shows the model's reasoning process.
    pub thinking: String,

    /// Whether this message is still being streamed.
    /// When true, we show a cursor/animation. When false, the message is complete.
    pub streaming: bool,

    /// Tool calls associated with this message.
    /// An assistant message can request multiple tool executions.
    pub tool_calls: Vec<ToolCallState>,

    /// Token usage statistics (for assistant messages only).
    /// Shows input/output token counts for cost tracking.
    /// `Option<T>` because user messages don't have usage.
    pub usage: Option<TokenUsage>,

    /// When this message was created.
    /// Used for display and session recording.
    pub timestamp: SystemTime,

    /// Whether the thinking section is expanded in the UI.
    /// Users can toggle to show/hide extended thinking.
    pub thinking_expanded: bool,
}

impl Message {
    #[must_use]
    pub fn is_assistant_reply(&self) -> bool {
        self.role == MessageRole::Assistant && self.kind == MessageKind::Regular
    }

    #[must_use]
    pub fn counts_toward_compaction_index(&self) -> bool {
        self.kind == MessageKind::Regular
    }

    #[must_use]
    pub fn is_compaction_boundary(&self) -> bool {
        self.kind == MessageKind::CompactionBoundary
    }
}

#[cfg(test)]
mod codex_status_tests {
    use super::*;

    #[test]
    fn compact_codex_status_keeps_active_turn_over_metadata() {
        let mut state = AppState::new();

        state.handle_agent_message(FromAgent::CodexSessionState {
            state: "resumed".to_owned(),
            thread_id: "thread-abcdef1234567890".to_owned(),
            profile: "work".to_owned(),
        });
        assert_eq!(
            state.status.as_deref(),
            Some("Codex resumed work thread-abcd")
        );

        state.handle_agent_message(FromAgent::CodexTurnState {
            state: "streaming".to_owned(),
            thread_id: "thread-abcdef1234567890".to_owned(),
            turn_id: Some("turn-1234567890".to_owned()),
        });
        assert_eq!(
            state.status.as_deref(),
            Some("Codex streaming thread-abcd turn-123456")
        );

        state.handle_agent_message(FromAgent::CodexCompatibility {
            protocol_version: "2025-01-01".to_owned(),
            resume: false,
            steering: false,
        });
        assert_eq!(
            state.status.as_deref(),
            Some("Codex streaming thread-abcd turn-123456")
        );
    }

    #[test]
    fn interrupted_codex_status_does_not_lock_out_later_session_state() {
        let mut state = AppState::new();

        state.handle_agent_message(FromAgent::CodexTurnState {
            state: "interrupted".to_owned(),
            thread_id: "thread-abcdef1234567890".to_owned(),
            turn_id: Some("turn-1234567890".to_owned()),
        });
        state.handle_agent_message(FromAgent::CodexSessionState {
            state: "reconnecting".to_owned(),
            thread_id: "thread-fedcba0987654321".to_owned(),
            profile: "work".to_owned(),
        });

        assert_eq!(
            state.status.as_deref(),
            Some("Codex reconnecting work thread-fedc")
        );
    }
}

/// Who sent the message.
///
/// # Rust Concepts Used
///
/// - **Enums**: Rust enums are "sum types" - a value can be ONE of the variants.
///   This is more powerful than enums in languages like Java/TypeScript because
///   each variant can hold different data (though this one doesn't).
///
/// - **`Copy` trait**: Small types that can be copied bit-for-bit (like integers)
///   can derive `Copy`. This means `=` creates a copy instead of a move.
///   Without `Copy`, assigning would transfer ownership.
///
/// - **`PartialEq` and `Eq`**: Enable `==` comparison.
///   `Eq` is a marker trait for types with true equality (no NaN issues).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    /// Message from the user
    User,
    /// Message from the AI assistant
    Assistant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    Regular,
    System,
    CompactionBoundary,
    SideQuestion,
    SideAnswer,
}

/// State of a tool call.
///
/// Represents a request from the AI to execute a tool (like running a
/// bash command or reading a file). We track its lifecycle from creation
/// through approval, execution, and completion.
///
/// # Rust Concept: Nested Data
///
/// This struct contains `serde_json::Value`, which is a dynamically-typed
/// JSON value. We use this because tool arguments vary by tool - bash takes
/// a command string, read takes a file path, etc. Rather than defining a
/// type for each tool's args, we keep them as JSON.
#[derive(Debug, Clone)]
pub struct ToolCallState {
    /// Unique identifier for this tool call.
    /// Used to match responses to requests.
    pub call_id: String,

    /// Name of the tool (e.g., "bash", "read", "write").
    pub tool: String,

    /// Tool arguments as JSON.
    /// Structure depends on the tool (e.g., {"command": "ls"} for bash).
    pub args: serde_json::Value,

    /// Current status of this tool call.
    pub status: ToolCallStatus,

    /// Output from the tool (stdout, file content, error messages).
    /// Appended to as the tool runs (for streaming output).
    pub output: String,
}

/// Status of a tool call in its lifecycle.
///
/// Tool calls go through this state machine:
/// ```text
/// Pending -> Running -> Completed
///    |          |
///    v          v
///  (rejected)  Failed
///               Cancelled (Ctrl+C / SIGINT, exit 130)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallStatus {
    /// Waiting for user approval.
    /// The tool won't run until the user approves it.
    Pending,

    /// Tool is currently executing.
    /// For bash commands, this means the process is running.
    Running,

    /// Tool completed successfully.
    /// Output contains the result.
    Completed,

    /// Tool execution failed.
    /// Output contains error information.
    Failed,

    /// Tool was cancelled mid-execution (Ctrl+C interrupt).
    /// Bash reports this as exit code 130 (SIGINT).
    Cancelled,

    /// Tool was blocked by a hook.
    /// A `PreToolUse` hook prevented execution (e.g., safety check).
    Blocked,
}

// ─────────────────────────────────────────────────────────────────────────────
// APPROVAL MODE
// ─────────────────────────────────────────────────────────────────────────────

// ─────────────────────────────────────────────────────────────────────────────
// INTERACTION MODE
// ─────────────────────────────────────────────────────────────────────────────

/// High-level interaction mode selected with explicit commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InteractionMode {
    /// Normal agenting with selective approvals
    #[default]
    Normal,
    /// Always-approve (YOLO) — skip tool approval prompts
    AlwaysApprove,
}

impl InteractionMode {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::AlwaysApprove => "always-approve",
        }
    }

    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Self::Normal => Self::AlwaysApprove,
            Self::AlwaysApprove => Self::Normal,
        }
    }

    #[must_use]
    pub fn approval_mode(self) -> ApprovalMode {
        match self {
            Self::Normal => ApprovalMode::Selective,
            Self::AlwaysApprove => ApprovalMode::Yolo,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// QUEUE MODES
// ─────────────────────────────────────────────────────────────────────────────

// ─────────────────────────────────────────────────────────────────────────────
// MAIN APPLICATION STATE
// ─────────────────────────────────────────────────────────────────────────────

/// Main application state.
///
/// Maximum number of alerts retained in [`AppState::alerts`].
/// Older alerts are dropped first so a flaky provider cannot grow memory
/// unbounded during a long session.
pub const MAX_ALERT_HISTORY: usize = 50;

/// This struct holds ALL mutable state for the application. Everything that
/// can change during program execution lives here. This centralized approach
/// makes it easy to understand what data exists and how it can change.
///
/// # Rust Concept: No Derive on Complex Structs
///
/// Notice we don't derive `Clone` or `Debug` here. That's because some fields
/// (like `HashSet`) have complex implementations, and we don't need to clone
/// the entire state (we pass it by reference instead).
///
/// # Rust Concept: Public Fields
///
/// All fields are `pub` for simplicity. In a library meant for external use,
/// we might make fields private and use methods for access. But for internal
/// use, direct field access is simpler and faster.
pub struct AppState {
    /// Display-only language, independent of model context.
    pub locale: crate::localization::Locale,
    /// All messages in the conversation.
    /// Ordered chronologically (oldest first).
    /// We use `Vec` because we frequently append and iterate, rarely remove.
    pub messages: Vec<Message>,

    /// Exact transcript heights reused across immutable Ratatui render passes.
    message_layout_cache: RefCell<MessageLayoutCache>,

    /// Input text area for composing messages.
    /// Supports multi-line editing with cursor movement.
    pub textarea: TextArea,

    /// Cached inner width for input cursor movement (in columns).
    pub input_width: u16,

    /// Preferred cursor column when moving up/down.
    input_preferred_col: Option<u16>,
    /// Emacs-style kill ring for yank/pop behavior.
    kill_ring: KillRing,
    /// Whether the last edit was a kill (for kill-ring append behavior).
    kill_chain_active: bool,

    /// Currently selected AI model name (e.g., "claude-3-opus").
    /// `Option` because it may not be known until the agent is ready.
    pub model: Option<String>,

    /// AI provider name (e.g., "anthropic", "openai").
    pub provider: Option<String>,

    /// Current working directory.
    /// Displayed in the UI and used for relative path resolution.
    pub cwd: Option<String>,

    /// Current git branch (if in a git repository).
    /// Displayed in the status bar for context.
    pub git_branch: Option<String>,

    /// Configured context-window capacity for the active model.
    /// Used by the session header; absent when the provider did not expose one.
    pub context_window: Option<u64>,

    /// Current session ID for persistence.
    /// Used to resume this conversation later.
    pub session_id: Option<String>,

    /// Whether the agent is currently processing a request.
    /// When true, we show a loading spinner and adjust input hints.
    pub busy: bool,

    /// When the agent became busy (for elapsed time display).
    /// Using `Instant` (monotonic clock) ensures accurate elapsed time
    /// even if system time changes.
    pub busy_since: Option<Instant>,

    /// Status message to display in the UI.
    /// Transient messages like "Copied to clipboard".
    pub status: Option<String>,

    /// Live terminal-theme state driven by typed OSC/DEC replies.
    pub theme_follower: Option<crate::themes::osc11::AutoThemeFollower>,
    /// Last time the app requested the terminal's current theme.
    pub last_theme_query: Option<Instant>,
    /// Whether DEC light/dark reporting is supported by this terminal.
    pub theme_reporting_available: bool,

    /// Scroll offset for the message list.
    /// Higher values scroll further up (show older messages).
    pub scroll_offset: usize,

    /// Set of expanded tool call IDs.
    /// Tool calls in this set show full details; others show summary.
    /// Using `HashSet` for O(1) contains/insert/remove operations.
    pub expanded_tool_calls: std::collections::HashSet<String>,

    /// Whether tool-bearing assistant turns render as a single summary row.
    pub focus_view: bool,

    /// Message IDs whose tool calls remain fully visible while Focus view is on.
    pub expanded_focus_turns: std::collections::HashSet<String>,

    /// Tool-bearing turn selected for keyboard inspection in Focus view.
    pub focus_selected_turn: Option<String>,

    /// Error message to display.
    /// Shown prominently in the UI when set.
    pub error: Option<String>,

    /// History of alerts (agent/API errors) recorded this session.
    /// Backs the status-bar `alerts:N` badge and the `/alerts` command so an
    /// alert is inspectable after it scrolls off or is replaced.
    /// Bounded to [`MAX_ALERT_HISTORY`] entries (oldest dropped first).
    pub alerts: VecDeque<String>,

    /// Number of recorded alerts not yet viewed via `/alerts`.
    /// Drives the status-bar `alerts:N` badge.
    pub unseen_alerts: usize,

    /// Current thinking header (extracted from **Header** in thinking text).
    /// Shown while the model is thinking to indicate what it's working on.
    pub thinking_header: Option<String>,

    /// Current thinking level for runtime badges and UI hints.
    pub thinking_level: ThinkingLevel,
    pub boost_status: crate::model_dynamics::BoostStatus,

    /// Full thinking buffer for the current response.
    /// Private because it's only used internally for header extraction.
    /// Public API uses the `thinking` field on individual messages.
    thinking_buffer: String,

    /// Zen mode - minimal UI.
    /// When enabled, hides status bar, hints, and other chrome.
    pub zen_mode: bool,

    /// Whether tool outputs should be collapsed by default.
    pub compact_tool_outputs: bool,

    /// Current approval mode for tool execution.
    /// Controls whether tools run automatically or require approval.
    pub approval_mode: ApprovalMode,
    /// Grok-style interaction mode (Normal / Plan / Always-approve)
    pub interaction_mode: InteractionMode,

    /// Queue mode for steering prompts while running.
    pub steering_mode: QueueMode,

    /// Queue mode for follow-up prompts while running.
    pub follow_up_mode: QueueMode,

    /// Number of prompts currently queued while running.
    pub queued_prompt_count: usize,

    /// Number of queued steering prompts.
    pub queued_steering_count: usize,

    /// Number of queued follow-up prompts.
    pub queued_follow_up_count: usize,

    /// Preview snippets for queued steering prompts.
    pub queued_steering_preview: Vec<String>,

    /// Preview snippets for queued follow-up prompts.
    pub queued_follow_up_preview: Vec<String>,

    /// Keyboard shortcut label for editing the last queued follow-up.
    pub queued_follow_up_edit_binding_label: String,

    /// Cached MCP connected server count for runtime badges.
    pub mcp_connected: usize,

    /// Cached MCP tool count for runtime badges.
    pub mcp_tool_count: usize,

    /// Cached MCP failed server count for runtime badges.
    pub mcp_failed: usize,

    /// Ghost-text suffix for the current slash-command inline completion.
    /// Rendered dimmed after the cursor; accepted with Right/End at end of input.
    /// `None` when there is nothing worth suggesting.
    pub ghost_completion: Option<String>,

    /// Whether an unknown slash command is forwarded to the agent as a prompt.
    /// Sourced from `tui.slash_command_fallback` in the config; defaults to true
    /// (the historical behavior).
    pub unknown_slash_command_fallback: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// DEFAULT IMPLEMENTATION
// ─────────────────────────────────────────────────────────────────────────────

/// Implement `Default` trait for `AppState`.
///
/// # Rust Concept: Traits
///
/// Traits are like interfaces in other languages. They define a set of
/// methods that a type must implement. `Default` is a standard library
/// trait that provides a `default()` method for creating a default value.
///
/// By implementing `Default`, we enable:
/// - `AppState::default()` to create a new state
/// - Generic code that requires `T: Default` to work with `AppState`
impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// APPSTATE IMPLEMENTATION
// ─────────────────────────────────────────────────────────────────────────────

/// Implementation block for `AppState`.
///
/// # Rust Concept: impl Blocks
///
/// In Rust, methods are defined in `impl` blocks separate from the struct
/// definition. A type can have multiple `impl` blocks, which is useful for
/// organizing code or conditional compilation.
impl AppState {
    /// Create a new `AppState` with default values.
    ///
    /// This is the primary constructor. All fields start empty/default.
    #[must_use]
    pub fn new() -> Self {
        Self {
            locale: crate::localization::Locale::default(),
            messages: Vec::new(), // Empty message list
            message_layout_cache: RefCell::new(MessageLayoutCache::default()),
            textarea: TextArea::new(), // Empty input area
            input_width: 1,            // Default width until first render
            input_preferred_col: None,
            kill_ring: KillRing::new(),
            kill_chain_active: false,
            model: None,      // No model selected yet
            provider: None,   // No provider yet
            cwd: None,        // No working directory
            git_branch: None, // Not in a git repo (yet)
            context_window: None,
            session_id: None, // No session yet
            busy: false,      // Not processing
            busy_since: None, // No timer running
            status: None,     // No status message
            theme_follower: None,
            last_theme_query: None,
            theme_reporting_available: false,
            scroll_offset: 0, // At bottom of messages
            expanded_tool_calls: std::collections::HashSet::new(),
            focus_view: false,
            expanded_focus_turns: std::collections::HashSet::new(),
            focus_selected_turn: None,
            error: None, // No error
            alerts: VecDeque::new(),
            unseen_alerts: 0,
            thinking_header: None, // No thinking in progress
            thinking_level: ThinkingLevel::Off,
            boost_status: crate::model_dynamics::BoostStatus::Idle,
            thinking_buffer: String::new(),
            zen_mode: false,                        // Full UI by default
            compact_tool_outputs: true, // Short previews; expand individual results on demand
            approval_mode: ApprovalMode::default(), // Selective mode
            interaction_mode: InteractionMode::default(),
            steering_mode: QueueMode::default(), // Queue steering by default
            follow_up_mode: QueueMode::default(), // Queue follow-ups by default
            queued_prompt_count: 0,              // No queued prompts
            queued_steering_count: 0,            // No queued steering prompts
            queued_follow_up_count: 0,           // No queued follow-up prompts
            queued_steering_preview: Vec::new(),
            queued_follow_up_preview: Vec::new(),
            queued_follow_up_edit_binding_label: "Alt+Up".to_string(),
            mcp_connected: 0,
            mcp_tool_count: 0,
            mcp_failed: 0,
            ghost_completion: None,
            unknown_slash_command_fallback: true,
        }
    }

    /// Update cached input width (inner width of the input box).
    pub fn set_input_width(&mut self, width: u16) {
        self.input_width = width.max(1);
    }

    /// Record an alert (e.g. an agent/API error) for later inspection.
    ///
    /// The full message is retained so `/alerts` can show what the status-bar
    /// `alerts:N` badge is counting. Bumps `unseen_alerts` until the user
    /// views the list.
    pub fn record_alert(&mut self, message: String) {
        if self.alerts.len() >= MAX_ALERT_HISTORY {
            self.alerts.pop_front();
        }
        self.alerts.push_back(message);
        self.unseen_alerts = self.unseen_alerts.saturating_add(1);
    }

    /// Mark all recorded alerts as seen (after `/alerts` lists them).
    pub fn mark_alerts_seen(&mut self) {
        self.unseen_alerts = 0;
    }

    pub(crate) fn prepare_message_layout<F>(
        &self,
        width: u16,
        settings_key: u64,
        messages: &[&Message],
        measure: F,
    ) -> MessageLayout
    where
        F: FnMut(usize) -> usize,
    {
        self.message_layout_cache.borrow_mut().prepare_messages(
            width,
            settings_key,
            messages,
            measure,
        )
    }

    #[cfg(test)]
    pub(crate) fn transcript_layout_measurements(&self) -> usize {
        self.message_layout_cache.borrow().measurements()
    }

    /// Get elapsed time since the agent became busy (in seconds).
    ///
    /// Returns 0 if not currently busy.
    ///
    /// # Rust Concept: Method Chaining with Option
    ///
    /// `.map()` transforms `Option<T>` to `Option<U>` by applying a function.
    /// `.unwrap_or()` extracts the value or returns a default if `None`.
    ///
    /// This pattern avoids explicit `if let Some(x) = ...` blocks.
    pub fn elapsed_busy_secs(&self) -> u64 {
        self.busy_since.map_or(0, |since| since.elapsed().as_secs())
    }

    #[must_use]
    pub fn can_queue_follow_up_shortcut(&self) -> bool {
        if !self.busy || !self.follow_up_mode.allows_queue() {
            return false;
        }
        let text = self.textarea.text();
        !text.trim().is_empty()
            && !text.trim_start().starts_with('/')
            && !text.trim_start().starts_with('!')
    }

    /// Handle a message from the agent.
    ///
    /// This is the main state machine that processes all agent events.
    /// Each event type triggers specific state updates.
    ///
    /// # Rust Concept: Pattern Matching with Enums
    ///
    /// `match` on an enum must handle all variants. This is exhaustive
    /// matching - the compiler ensures we don't miss any cases. If we
    /// add a new `FromAgent` variant, this code won't compile until
    /// we add a handler for it.
    pub fn handle_agent_message(&mut self, msg: FromAgent) {
        match msg {
            // Private durable provider state is not interactive UI state.
            FromAgent::LocalAssistantContent { .. } | FromAgent::ConversationSnapshot { .. } => {}
            // Managed receipts are forwarded to headless consumers but carry
            // no interactive UI content.
            FromAgent::ManagedGatewayReceipt { .. } => {}
            // The authenticated headless controller answers this transport request.
            FromAgent::ManagedAuthorizationRequest { .. } => {}
            // Accounting only: reported so a caller metering model output can
            // charge Codex-native operations, which never arrive as `ToolCall`.
            // It carries no text to display and needs no response.
            FromAgent::CodexNativeOperation { .. }
            | FromAgent::CodexNativeDecision { .. }
            | FromAgent::CodexTransportReceipt { .. } => {}
            FromAgent::CodexSessionState {
                state,
                thread_id,
                profile,
            } => {
                self.set_codex_status_if_useful(compact_codex_session_status(
                    &state, &thread_id, &profile,
                ));
            }
            FromAgent::CodexTurnState {
                state,
                thread_id,
                turn_id,
            } => {
                self.status = Some(compact_codex_turn_status(
                    &state,
                    &thread_id,
                    turn_id.as_deref(),
                ));
            }
            FromAgent::CodexUsageState { usage, .. } => {
                if let Some(usage) = usage {
                    if let Some(msg) = self.messages.iter_mut().rev().find(|m| m.streaming) {
                        msg.usage = Some(usage);
                    }
                }
            }
            FromAgent::CodexCompatibility {
                protocol_version,
                resume,
                steering,
            } => {
                self.set_codex_status_if_useful(compact_codex_compatibility_status(
                    &protocol_version,
                    resume,
                    steering,
                ));
            }
            // Agent is ready with its model info
            FromAgent::Ready { model, provider } => {
                self.model = Some(model);
                self.provider = Some(provider);
                self.busy = false;
                self.busy_since = None;
            }

            FromAgent::BoostChanged { status, thinking } => {
                self.boost_status = status;
                if let Some(thinking) = thinking {
                    self.thinking_level = thinking;
                }
            }
            FromAgent::ModelChanged { model, provider } => {
                self.status = Some(format!("Model: {model}"));
                self.model = Some(model);
                self.provider = Some(provider);
            }

            FromAgent::ModelChangeFailed { reason, .. } => {
                self.error = Some(reason);
            }

            // Agent started generating a response
            FromAgent::ResponseStart { response_id } => {
                self.busy = true;
                self.busy_since = Some(Instant::now());

                // Create a new empty message that will be filled in by chunks
                self.messages.push(Message {
                    id: response_id,
                    role: MessageRole::Assistant,
                    kind: MessageKind::Regular,
                    content: String::new(),
                    thinking: String::new(),
                    streaming: true, // Will receive more content
                    tool_calls: Vec::new(),
                    usage: None,
                    timestamp: SystemTime::now(),
                    thinking_expanded: false,
                });
            }

            // Received a chunk of the response (streaming)
            FromAgent::ResponseChunk {
                response_id,
                content,
                is_thinking,
            } => {
                // Find the message being streamed and append content
                // Rust Concept: `.iter_mut()` returns mutable references
                if let Some(msg) = self.messages.iter_mut().find(|m| m.id == response_id) {
                    if is_thinking {
                        // Append to thinking content
                        msg.thinking.push_str(&content);

                        // Also track in our buffer to extract headers
                        self.thinking_buffer.push_str(&content);
                        if let Some(header) = extract_thinking_header(&self.thinking_buffer) {
                            self.thinking_header = Some(header);
                        }
                    } else {
                        // Append to main content
                        msg.content.push_str(&content);
                    }
                }
            }

            // Response finished
            FromAgent::ResponseEnd { response_id, usage } => {
                if let Some(msg) = self.messages.iter_mut().find(|m| m.id == response_id) {
                    msg.streaming = false;
                    msg.usage = usage;
                }
                // Clear thinking state for next response
                self.thinking_header = None;
                self.thinking_buffer.clear();
            }

            FromAgent::TurnCompleted { .. } | FromAgent::TurnInterrupted { .. } => {
                self.busy = false;
                self.busy_since = None;
            }

            FromAgent::SideQuestionStart { standalone, .. } => {
                if standalone {
                    self.busy = true;
                    self.busy_since.get_or_insert_with(Instant::now);
                }
            }
            FromAgent::SideQuestionChunk { .. } => {}
            FromAgent::SideQuestionEnd { standalone, .. } => {
                if standalone {
                    self.busy = false;
                    self.busy_since = None;
                }
            }

            // Agent wants to call a tool
            FromAgent::ToolCall {
                call_id,
                tool,
                args,
                requires_approval,
                ..
            } => {
                // Add tool call to the most recent assistant message
                // `.rev()` iterates in reverse order (newest first)
                if let Some(msg) = self
                    .messages
                    .iter_mut()
                    .rev()
                    .find(|m| m.is_assistant_reply())
                {
                    msg.tool_calls.push(ToolCallState {
                        call_id,
                        tool,
                        args,
                        status: if requires_approval {
                            ToolCallStatus::Pending
                        } else {
                            ToolCallStatus::Running
                        },
                        output: String::new(),
                    });
                }
            }

            // Tool execution started
            FromAgent::ToolStart { call_id } => {
                self.update_tool_status(&call_id, ToolCallStatus::Running);
            }

            // Tool produced output (may be called multiple times for streaming)
            FromAgent::ToolOutput { call_id, content } => {
                // Find and append to the tool call's output
                for msg in self.messages.iter_mut().rev() {
                    for tc in &mut msg.tool_calls {
                        if tc.call_id == call_id {
                            tc.output.push_str(&content);
                            return; // Early return once found
                        }
                    }
                }
            }

            // Tool finished
            FromAgent::ToolEnd {
                call_id,
                success,
                receipt,
                ..
            } => {
                self.update_tool_status(
                    &call_id,
                    if matches!(
                        receipt.as_ref().map(|receipt| receipt.status),
                        Some(ExecutionStatus::Cancelled { .. })
                    ) {
                        ToolCallStatus::Cancelled
                    } else if success {
                        ToolCallStatus::Completed
                    } else {
                        ToolCallStatus::Failed
                    },
                );
            }

            // Batch execution events (informational, handled by individual tool events)
            FromAgent::BatchStart { total } => {
                self.status = Some(format!("Executing {total} tools in parallel..."));
            }
            FromAgent::BatchEnd {
                total,
                successes,
                failures,
            } => {
                if failures > 0 {
                    self.status = Some(format!(
                        "Batch complete: {successes}/{total} succeeded, {failures} failed"
                    ));
                } else {
                    self.status = Some(format!("Batch complete: {total} tools succeeded"));
                }
            }

            // Error occurred
            FromAgent::Error {
                message,
                fatal,
                terminal,
                ..
            } => {
                self.record_alert(message.clone());
                self.error = Some(message);
                if fatal || terminal {
                    self.busy = false;
                    self.busy_since = None;
                }
            }

            FromAgent::ProviderError { message, .. } => {
                self.record_alert(message.clone());
                self.error = Some(message);
                self.busy = false;
                self.busy_since = None;
            }

            // Status update (informational)
            FromAgent::Status { message } => {
                self.status = Some(message);
            }

            // Content-free observations are consumed by TurnTracker; they do not
            // create messages or change the interactive execution state.
            FromAgent::Compaction { .. }
            | FromAgent::StreamObservation { .. }
            | FromAgent::RequestRetryObservation
            | FromAgent::RequestContextPrepared { .. }
            | FromAgent::OperationObservation { .. }
            | FromAgent::TurnStarted
            | FromAgent::RequestRetryScheduled { .. }
            | FromAgent::ContextCalibration { .. }
            | FromAgent::CompactionMeasured { .. } => {}

            // Session info updated
            FromAgent::SessionInfo {
                session_id,
                cwd,
                git_branch,
            } => {
                self.session_id = session_id;
                self.cwd = Some(cwd);
                self.git_branch = git_branch;
            }

            // Tool blocked by hook
            FromAgent::HookBlocked {
                call_id,
                tool,
                reason,
            } => {
                // Update tool status to blocked
                self.update_tool_status(&call_id, ToolCallStatus::Blocked);

                // Log the blocking for debugging
                eprintln!("[hooks] Tool '{tool}' blocked: {reason} (call_id: {call_id})");
            }
        }
    }

    /// Update the status of a tool call.
    ///
    /// Helper method to find and update a tool call across all messages.
    /// Private because it's only used internally.
    fn update_tool_status(&mut self, call_id: &str, status: ToolCallStatus) {
        // Search in reverse (most recent messages first) for efficiency
        for msg in self.messages.iter_mut().rev() {
            for tc in &mut msg.tool_calls {
                if tc.call_id == call_id {
                    tc.status = status;
                    return; // Found it, done
                }
            }
        }
    }

    /// Mark a tool call as started (executing).
    ///
    /// Used when the TUI executes the tool itself on a spawned task: the
    /// agent never emits `ToolStart` for these calls, so without this the
    /// transcript row would sit at `Pending` for the whole execution.
    pub fn start_tool_call(&mut self, call_id: &str) {
        self.update_tool_status(call_id, ToolCallStatus::Running);
    }

    /// Mark a tool call as completed and attach its output.
    pub fn complete_tool_call(&mut self, call_id: &str, output: &str) {
        for msg in self.messages.iter_mut().rev() {
            for tc in &mut msg.tool_calls {
                if tc.call_id == call_id {
                    tc.status = ToolCallStatus::Completed;
                    if !output.is_empty() {
                        if !tc.output.is_empty() {
                            tc.output.push('\n');
                        }
                        tc.output.push_str(output);
                    }
                    return;
                }
            }
        }
    }

    /// Mark a tool call as cancelled (Ctrl+C interrupt, exit code 130).
    pub fn cancel_tool_call(&mut self, call_id: &str, note: &str) {
        for msg in self.messages.iter_mut().rev() {
            for tc in &mut msg.tool_calls {
                if tc.call_id == call_id {
                    tc.status = ToolCallStatus::Cancelled;
                    if !note.is_empty() {
                        if !tc.output.is_empty() {
                            tc.output.push('\n');
                        }
                        tc.output.push_str(note);
                    }
                    return;
                }
            }
        }
    }

    /// Mark a tool call as failed with an error note.
    ///
    /// Used when we reject a tool call locally (e.g., user declined approval).
    pub fn fail_tool_call(&mut self, call_id: &str, note: &str) {
        for msg in self.messages.iter_mut().rev() {
            for tc in &mut msg.tool_calls {
                if tc.call_id == call_id {
                    tc.status = ToolCallStatus::Failed;
                    if !note.is_empty() {
                        // Add separator if there's existing output
                        if !tc.output.is_empty() {
                            tc.output.push('\n');
                        }
                        tc.output.push_str(note);
                    }
                    return;
                }
            }
        }
    }

    /// Add a user message to the conversation.
    ///
    /// Creates a new message, marks us as busy, and returns the message ID.
    ///
    /// # Returns
    ///
    /// The ID of the new message (can be used for tracking).
    pub fn add_user_message(&mut self, content: String) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        self.messages.push(Message {
            id: id.clone(), // Clone because we return it and store it
            role: MessageRole::User,
            kind: MessageKind::Regular,
            content,
            thinking: String::new(),
            streaming: false, // User messages are complete immediately
            tool_calls: Vec::new(),
            usage: None,
            timestamp: SystemTime::now(),
            thinking_expanded: false,
        });
        self.busy = true;
        self.busy_since = Some(Instant::now());
        id
    }

    /// Add a visually distinct question that is not part of main history.
    pub fn add_side_question(&mut self, id: String, content: String) {
        self.messages.push(Message {
            id,
            role: MessageRole::User,
            kind: MessageKind::SideQuestion,
            content,
            thinking: String::new(),
            streaming: false,
            tool_calls: Vec::new(),
            usage: None,
            timestamp: SystemTime::now(),
            thinking_expanded: false,
        });
    }

    /// Add a visually distinct answer that is not part of main history.
    pub fn add_side_answer(&mut self, id: String, content: String, streaming: bool) {
        self.messages.push(Message {
            id,
            role: MessageRole::Assistant,
            kind: MessageKind::SideAnswer,
            content,
            thinking: String::new(),
            streaming,
            tool_calls: Vec::new(),
            usage: None,
            timestamp: SystemTime::now(),
            thinking_expanded: false,
        });
    }

    // ─────────────────────────────────────────────────────────────────────────
    // TEXT INPUT METHODS
    // ─────────────────────────────────────────────────────────────────────────
    //
    // These methods delegate to the TextArea component but provide a simpler
    // interface for common operations.

    /// Get current input text (read-only view).
    ///
    /// # Rust Concept: Returning References
    ///
    /// Returning `&str` instead of `String` avoids allocation. The caller
    /// gets a view into our data without copying it. The lifetime of the
    /// returned reference is tied to `&self`.
    pub fn input(&self) -> &str {
        self.textarea.text()
    }

    /// Get cursor position (byte offset into the text).
    pub fn cursor(&self) -> usize {
        self.textarea.cursor()
    }

    /// Insert a character at the cursor position.
    ///
    /// # Rust Concept: UTF-8 String Handling
    ///
    /// Rust strings are UTF-8. Characters can be 1-4 bytes. We use
    /// `char.len_utf8()` to get the byte length for cursor movement.
    /// This ensures we don't split multi-byte characters.
    pub fn insert_char(&mut self, c: char) {
        self.insert_char_internal(c, true);
    }

    /// Insert a string at the cursor position.
    pub fn insert_str(&mut self, s: &str) {
        self.insert_str_internal(s, true);
    }

    fn insert_char_internal(&mut self, c: char, reset_kill_ring: bool) {
        self.kill_chain_active = false;
        if reset_kill_ring {
            self.kill_ring.reset_rotation();
        }
        let cursor = self.textarea.cursor();
        let mut text = self.textarea.text().to_string();
        text.insert(cursor, c);
        self.textarea.set_text(&text);
        self.textarea.set_cursor(cursor + c.len_utf8());
        self.input_preferred_col = None;
    }

    fn insert_str_internal(&mut self, s: &str, reset_kill_ring: bool) {
        self.kill_chain_active = false;
        if reset_kill_ring {
            self.kill_ring.reset_rotation();
        }
        let cursor = self.textarea.cursor();
        let mut text = self.textarea.text().to_string();
        text.insert_str(cursor, s);
        self.textarea.set_text(&text);
        self.textarea.set_cursor(cursor + s.len());
        self.input_preferred_col = None;
    }

    /// Insert pasted text at the cursor position.
    ///
    /// Carriage returns are stripped to normalize line endings. The full
    /// pasted content is always stored (submission stays byte-identical);
    /// when the paste is large (more than `PASTE_FOLD_MIN_LINES` lines or
    /// `PASTE_FOLD_MIN_CHARS` bytes) it is folded into a `[Pasted: N lines]`
    /// chip in the display only.
    pub fn insert_paste(&mut self, raw: &str) {
        let pasted: String = raw.chars().filter(|c| *c != '\r').collect();
        if pasted.is_empty() {
            return;
        }
        self.kill_chain_active = false;
        self.kill_ring.reset_rotation();
        let start = self.textarea.cursor();
        let mut text = self.textarea.text().to_string();
        text.insert_str(start, &pasted);
        self.textarea.set_text(&text);
        let end = start + pasted.len();
        self.textarea.set_cursor(end);
        self.input_preferred_col = None;
        let lines = pasted.matches('\n').count() + 1;
        if lines > PASTE_FOLD_MIN_LINES || pasted.len() > PASTE_FOLD_MIN_CHARS {
            self.textarea.add_paste_fold(start..end, lines);
        }
    }

    /// Delete the character before the cursor (Backspace key).
    ///
    /// Handles multi-byte UTF-8 characters correctly. When the cursor is
    /// right at the end of a folded paste, the whole pasted block is
    /// deleted as a unit.
    pub fn backspace(&mut self) {
        self.reset_kill_state();
        let cursor = self.textarea.cursor();
        if cursor > 0 {
            // Unit-delete a folded paste sitting right before the cursor.
            if let Some(range) = self.textarea.fold_ending_at(cursor) {
                let mut new_text = self.textarea.text().to_string();
                new_text.replace_range(range.clone(), "");
                self.textarea.set_text(&new_text);
                self.textarea.set_cursor(range.start);
                self.input_preferred_col = None;
                return;
            }
            let text = self.textarea.text();
            // Find the byte length of the previous character
            let prev = text[..cursor]
                .chars()
                .last() // Get the last char before cursor
                .map_or(0, char::len_utf8);
            let mut new_text = text.to_string();
            new_text.remove(cursor - prev);
            self.textarea.set_text(&new_text);
            self.textarea.set_cursor(cursor - prev);
            self.input_preferred_col = None;
        }
    }

    /// Delete the character after the cursor (Delete key).
    pub fn delete(&mut self) {
        self.reset_kill_state();
        let cursor = self.textarea.cursor();
        let text = self.textarea.text();
        if cursor < text.len() {
            let mut new_text = text.to_string();
            new_text.remove(cursor);
            self.textarea.set_text(&new_text);
            self.input_preferred_col = None;
        }
    }

    /// Move cursor one character left.
    pub fn move_left(&mut self) {
        self.reset_kill_state();
        let cursor = self.textarea.cursor();
        if cursor > 0 {
            let text = self.textarea.text();
            // Find byte length of previous character for proper UTF-8 handling
            let prev = text[..cursor].chars().last().map_or(0, char::len_utf8);
            self.textarea.set_cursor(cursor - prev);
            self.input_preferred_col = None;
        }
    }

    /// Move cursor one character right.
    pub fn move_right(&mut self) {
        self.reset_kill_state();
        let cursor = self.textarea.cursor();
        let text = self.textarea.text();
        if cursor < text.len() {
            // Find byte length of next character
            let next = text[cursor..]
                .chars()
                .next() // Get the first char after cursor
                .map_or(0, char::len_utf8);
            self.textarea.set_cursor(cursor + next);
            self.input_preferred_col = None;
        }
    }

    /// Move cursor to the start of input (Home key).
    pub fn move_home(&mut self) {
        self.reset_kill_state();
        self.textarea.set_cursor(0);
        self.input_preferred_col = None;
    }

    /// Move cursor to the end of input (End key).
    pub fn move_end(&mut self) {
        self.reset_kill_state();
        let len = self.textarea.text().len();
        self.textarea.set_cursor(len);
        self.input_preferred_col = None;
    }

    /// Move cursor one wrapped line up.
    pub fn move_up(&mut self) {
        self.reset_kill_state();
        self.move_vertical(-1);
    }

    /// Move cursor one wrapped line down.
    pub fn move_down(&mut self) {
        self.reset_kill_state();
        self.move_vertical(1);
    }

    /// Move cursor to the start of the current line (smart Home).
    ///
    /// Toggles between the first non-whitespace character and column 0.
    pub fn move_home_smart(&mut self) {
        self.reset_kill_state();
        let text = self.textarea.text();
        let cursor = self.textarea.cursor();
        let (line_start, line_end, col) = Self::line_bounds(text, cursor);
        let line = &text[line_start..line_end];
        let first_non_blank = Self::first_non_whitespace_offset(line);
        let target = if col == first_non_blank {
            0
        } else {
            first_non_blank
        };
        self.textarea.set_cursor(line_start + target);
        self.input_preferred_col = None;
    }

    /// Move cursor to the start of the previous word.
    pub fn move_word_left(&mut self) {
        self.reset_kill_state();
        let text = self.textarea.text();
        let cursor = self.textarea.cursor();
        if cursor == 0 {
            return;
        }

        let (line_start, line_end, col) = Self::line_bounds(text, cursor);
        if col == 0 {
            if line_start == 0 {
                return;
            }
            let mut prev_end = line_start.saturating_sub(1);
            loop {
                let prev_start = text[..prev_end].rfind('\n').map_or(0, |i| i + 1);
                if prev_end > prev_start {
                    self.textarea.set_cursor(prev_end);
                    self.input_preferred_col = None;
                    return;
                }
                if prev_start == 0 {
                    self.textarea.set_cursor(0);
                    self.input_preferred_col = None;
                    return;
                }
                prev_end = prev_start.saturating_sub(1);
            }
        }

        let line = &text[line_start..line_end];
        let new_col = previous_word_start(line, col.min(line.len()));
        self.textarea.set_cursor(line_start + new_col);
        self.input_preferred_col = None;
    }

    /// Move cursor to the start of the next word.
    pub fn move_word_right(&mut self) {
        self.reset_kill_state();
        let text = self.textarea.text();
        let mut cursor = self.textarea.cursor();
        if cursor >= text.len() {
            return;
        }

        loop {
            let (line_start, line_end, col) = Self::line_bounds(text, cursor);
            let line = &text[line_start..line_end];
            let new_col = next_word_start(line, col.min(line.len()));
            if new_col >= line.len() {
                if line_end >= text.len() {
                    self.textarea.set_cursor(text.len());
                    self.input_preferred_col = None;
                    return;
                }
                cursor = line_end + 1;
                if cursor > text.len() {
                    self.textarea.set_cursor(text.len());
                    self.input_preferred_col = None;
                    return;
                }
                continue;
            }
            self.textarea.set_cursor(line_start + new_col);
            self.input_preferred_col = None;
            return;
        }
    }

    /// Delete the word before the cursor.
    pub fn delete_word_backward(&mut self) {
        let cursor = self.textarea.cursor();
        if cursor == 0 {
            return;
        }
        let text = self.textarea.text().to_string();
        let (line_start, line_end, col) = Self::line_bounds(&text, cursor);

        if col == 0 {
            if line_start == 0 {
                return;
            }
            let remove_at = line_start.saturating_sub(1);
            let mut new_text = text;
            new_text.remove(remove_at);
            self.textarea.set_text(&new_text);
            self.textarea.set_cursor(remove_at);
            self.input_preferred_col = None;
            self.record_kill("\n".to_string(), true);
            return;
        }

        let line = &text[line_start..line_end];
        let start = previous_word_start(line, col.min(line.len()));
        if start == col {
            return;
        }
        let killed = line[start..col].to_string();
        let mut new_text = text;
        new_text.replace_range(line_start + start..cursor, "");
        self.textarea.set_text(&new_text);
        self.textarea.set_cursor(line_start + start);
        self.input_preferred_col = None;
        self.record_kill(killed, true);
    }

    /// Delete from cursor to start of line (Ctrl+U).
    pub fn delete_to_start_of_line(&mut self) {
        let cursor = self.textarea.cursor();
        if cursor == 0 {
            return;
        }
        let text = self.textarea.text().to_string();
        let (line_start, _line_end, col) = Self::line_bounds(&text, cursor);
        if col == 0 {
            if line_start == 0 {
                return;
            }
            let remove_at = line_start.saturating_sub(1);
            let mut new_text = text;
            new_text.remove(remove_at);
            self.textarea.set_text(&new_text);
            self.textarea.set_cursor(remove_at);
            self.input_preferred_col = None;
            self.record_kill("\n".to_string(), true);
            return;
        }
        let killed = text[line_start..cursor].to_string();
        let mut new_text = text;
        new_text.replace_range(line_start..cursor, "");
        self.textarea.set_text(&new_text);
        self.textarea.set_cursor(line_start);
        self.input_preferred_col = None;
        self.record_kill(killed, true);
    }

    /// Delete from cursor to end of line (Ctrl+K).
    pub fn delete_to_end_of_line(&mut self) {
        let cursor = self.textarea.cursor();
        let text = self.textarea.text().to_string();
        let (_line_start, line_end, _col) = Self::line_bounds(&text, cursor);
        if cursor < line_end {
            let killed = text[cursor..line_end].to_string();
            let mut new_text = text;
            new_text.replace_range(cursor..line_end, "");
            self.textarea.set_text(&new_text);
            self.textarea.set_cursor(cursor);
            self.input_preferred_col = None;
            self.record_kill(killed, false);
            return;
        }
        if line_end < text.len() {
            let mut new_text = text;
            new_text.remove(line_end);
            self.textarea.set_text(&new_text);
            self.textarea.set_cursor(cursor);
            self.input_preferred_col = None;
            self.record_kill("\n".to_string(), false);
        }
    }

    /// Yank the most recent kill (Alt+Y), cycling on repeated presses.
    pub fn yank_kill_ring(&mut self) {
        let cursor = self.textarea.cursor();
        self.kill_chain_active = false;
        if self.kill_ring.is_rotating() {
            let info = self.kill_ring.last_yank_info();
            let next = self.kill_ring.yank_pop().map(str::to_string);
            if let (Some(info), Some(next)) = (info, next) {
                self.replace_range_internal(info.start, info.start + info.length, &next, false);
            }
            return;
        }

        if let Some((text, _info)) = self.kill_ring.yank_with_info(cursor) {
            self.insert_str_internal(&text, false);
        }
    }

    fn move_vertical(&mut self, delta: i32) {
        let width = self.input_width.max(1);
        let Some((line_idx, col)) = self.textarea.cursor_line_col(width) else {
            return;
        };
        let mut line_idx = line_idx;
        let target_col = self.input_preferred_col.unwrap_or(col);
        if self.input_preferred_col.is_none() {
            self.input_preferred_col = Some(target_col);
        }

        if delta.is_positive() && col == 0 && self.input_preferred_col.is_some() && line_idx > 0 {
            line_idx = line_idx.saturating_sub(1);
        }

        let new_line = if delta.is_negative() {
            if line_idx == 0 {
                return;
            }
            line_idx.saturating_sub(delta.wrapping_abs() as usize)
        } else {
            line_idx.saturating_add(delta as usize)
        };

        if let Some(new_pos) = self
            .textarea
            .byte_pos_for_line_col(width, new_line, target_col)
        {
            self.textarea.set_cursor(new_pos);
        }
    }

    /// Take the current input and clear it.
    ///
    /// Used when submitting input - we extract the text and reset.
    ///
    /// # Rust Concept: Ownership Transfer
    ///
    /// "Taking" implies ownership transfer. We return the String (owned)
    /// and clear the internal state. The caller now owns the string.
    pub fn take_input(&mut self) -> String {
        let input = self.textarea.text().to_string();
        self.textarea.set_text("");
        self.textarea.set_cursor(0);
        self.input_preferred_col = None;
        self.reset_kill_state();
        input
    }

    /// Exchange the complete editor, preserving cursor and folded paste content.
    pub fn swap_input_editor(&mut self, editor: &mut TextArea) {
        std::mem::swap(&mut self.textarea, editor);
        self.input_preferred_col = None;
        self.reset_kill_state();
    }

    /// Set the input text directly (e.g., for command history).
    pub fn set_input(&mut self, text: &str) {
        self.textarea.set_text(text);
        self.textarea.set_cursor(text.len()); // Cursor at end
        self.input_preferred_col = None;
        self.reset_kill_state();
    }

    fn replace_range_internal(
        &mut self,
        start: usize,
        end: usize,
        replacement: &str,
        reset_kill_ring: bool,
    ) {
        self.kill_chain_active = false;
        if reset_kill_ring {
            self.kill_ring.reset_rotation();
        }
        let mut text = self.textarea.text().to_string();
        let start = start.min(text.len());
        let end = end.min(text.len()).max(start);
        text.replace_range(start..end, replacement);
        self.textarea.set_text(&text);
        self.textarea.set_cursor(start + replacement.len());
        self.input_preferred_col = None;
    }

    fn line_bounds(text: &str, cursor: usize) -> (usize, usize, usize) {
        let cursor = cursor.min(text.len());
        let line_start = text[..cursor].rfind('\n').map_or(0, |i| i + 1);
        let line_end = text[cursor..].find('\n').map_or(text.len(), |i| cursor + i);
        let col = cursor.saturating_sub(line_start);
        (line_start, line_end, col)
    }

    fn first_non_whitespace_offset(line: &str) -> usize {
        line.char_indices()
            .find(|(_, c)| !c.is_whitespace())
            .map(|(idx, _)| idx)
            .unwrap_or(line.len())
    }

    fn reset_kill_state(&mut self) {
        self.kill_chain_active = false;
        self.kill_ring.reset_rotation();
    }

    fn record_kill(&mut self, text: String, prepend: bool) {
        if text.is_empty() {
            return;
        }
        if self.kill_chain_active {
            self.kill_ring.kill_append(text, prepend);
        } else {
            self.kill_ring.kill(text);
            self.kill_chain_active = true;
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // MESSAGE MANIPULATION METHODS
    // ─────────────────────────────────────────────────────────────────────────

    /// Add a system message (for help, status, errors, etc.).
    ///
    /// System messages appear as assistant messages but are locally generated,
    /// not from the AI.
    pub fn add_system_message(&mut self, content: String) {
        let id = uuid::Uuid::new_v4().to_string();
        self.messages.push(Message {
            id,
            role: MessageRole::Assistant, // Display as assistant
            kind: MessageKind::System,
            content,
            thinking: String::new(),
            streaming: false,
            tool_calls: Vec::new(),
            usage: None,
            timestamp: SystemTime::now(),
            thinking_expanded: false,
        });
    }

    fn transcript_insert_position(&self, first_kept_entry_index: usize) -> usize {
        if first_kept_entry_index == 0 {
            return 0;
        }

        let mut transcript_count = 0usize;
        for (index, message) in self.messages.iter().enumerate() {
            if !message.counts_toward_compaction_index() {
                continue;
            }
            if transcript_count == first_kept_entry_index {
                return index;
            }
            transcript_count += 1;
        }

        self.messages.len()
    }

    pub fn apply_compaction(
        &mut self,
        summary: String,
        first_kept_entry_index: usize,
        timestamp: SystemTime,
    ) {
        let insert_pos = self.transcript_insert_position(first_kept_entry_index);
        self.messages.drain(..insert_pos);
        self.messages.insert(
            0,
            Message {
                id: uuid::Uuid::new_v4().to_string(),
                role: MessageRole::Assistant,
                kind: MessageKind::CompactionBoundary,
                content: summary,
                thinking: String::new(),
                streaming: false,
                tool_calls: Vec::new(),
                usage: None,
                timestamp,
                thinking_expanded: false,
            },
        );
        self.prune_focus_turn_state();
    }

    /// Toggle whether thinking is expanded for a message.
    pub fn toggle_thinking(&mut self, message_id: &str) {
        if let Some(msg) = self.messages.iter_mut().find(|m| m.id == message_id) {
            msg.thinking_expanded = !msg.thinking_expanded;
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // SCROLL METHODS
    // ─────────────────────────────────────────────────────────────────────────

    /// Scroll up in the message list.
    ///
    /// # Rust Concept: `saturating_sub`
    ///
    /// `saturating_sub` prevents underflow - if the result would be negative,
    /// it returns 0 instead. This is safer than wrapping subtraction which
    /// could give a huge number on underflow.
    pub fn scroll_up(&mut self, amount: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(amount);
    }

    /// Scroll down in the message list.
    ///
    /// Uses `saturating_add` to prevent overflow (though unlikely for scroll).
    pub fn scroll_down(&mut self, amount: usize) {
        self.scroll_offset = self.scroll_offset.saturating_add(amount);
    }

    // ─────────────────────────────────────────────────────────────────────────
    // TOOL CALL EXPANSION METHODS
    // ─────────────────────────────────────────────────────────────────────────

    /// Toggle whether a tool call is expanded.
    ///
    /// Expanded tool calls show full output; collapsed show a summary.
    pub fn toggle_tool_call(&mut self, call_id: &str) {
        if self.expanded_tool_calls.contains(call_id) {
            self.expanded_tool_calls.remove(call_id);
        } else {
            self.expanded_tool_calls.insert(call_id.to_string());
        }
    }

    /// Check if a tool call is expanded.
    pub fn is_tool_call_expanded(&self, call_id: &str) -> bool {
        let toggled = self.expanded_tool_calls.contains(call_id);
        if self.compact_tool_outputs {
            toggled
        } else {
            !toggled
        }
    }

    /// Apply an output detail preset, clearing overrides from the previous view.
    /// This changes presentation only; approval and tool execution state are untouched.
    pub fn set_output_detail(&mut self, detail: OutputDetail) {
        self.expanded_tool_calls.clear();
        self.clear_focus_turn_state();
        self.compact_tool_outputs = detail != OutputDetail::Expanded;
        self.set_focus_view(detail == OutputDetail::Summary);
    }

    /// Return the detail preset represented by the current presentation flags.
    pub fn output_detail(&self) -> OutputDetail {
        if self.focus_view {
            OutputDetail::Summary
        } else if self.compact_tool_outputs {
            OutputDetail::Compact
        } else {
            OutputDetail::Expanded
        }
    }

    /// Set Focus view explicitly and return the resulting state.
    pub fn set_focus_view(&mut self, enabled: bool) -> bool {
        self.focus_view = enabled;
        self.prune_focus_turn_state();
        if enabled && self.focus_selected_turn.is_none() {
            self.focus_selected_turn = self
                .messages
                .iter()
                .rev()
                .find(|message| !message.tool_calls.is_empty())
                .map(|message| message.id.clone());
        }
        self.focus_view
    }

    pub fn clear_focus_turn_state(&mut self) {
        self.expanded_focus_turns.clear();
        self.focus_selected_turn = None;
    }

    pub fn prune_focus_turn_state(&mut self) {
        let available = self
            .messages
            .iter()
            .filter(|message| !message.tool_calls.is_empty())
            .map(|message| message.id.as_str())
            .collect::<std::collections::HashSet<_>>();
        self.expanded_focus_turns
            .retain(|message_id| available.contains(message_id.as_str()));
        if self
            .focus_selected_turn
            .as_deref()
            .is_some_and(|message_id| !available.contains(message_id))
        {
            self.focus_selected_turn = None;
        }
    }

    pub fn select_previous_focus_turn(&mut self) -> bool {
        self.move_focus_selection(-1)
    }

    pub fn select_next_focus_turn(&mut self) -> bool {
        self.move_focus_selection(1)
    }

    fn move_focus_selection(&mut self, direction: isize) -> bool {
        let ids = self
            .messages
            .iter()
            .filter(|message| !message.tool_calls.is_empty())
            .map(|message| message.id.clone())
            .collect::<Vec<_>>();
        if ids.is_empty() {
            self.focus_selected_turn = None;
            return false;
        }
        let current = self
            .focus_selected_turn
            .as_ref()
            .and_then(|selected| ids.iter().position(|id| id == selected))
            .unwrap_or(ids.len() - 1);
        let next = current
            .saturating_add_signed(direction)
            .min(ids.len().saturating_sub(1));
        self.focus_selected_turn = Some(ids[next].clone());
        true
    }

    pub fn toggle_selected_focus_turn(&mut self) -> bool {
        let Some(message_id) = self.focus_selected_turn.clone() else {
            return self.toggle_latest_focus_turn();
        };
        self.toggle_focus_turn(&message_id);
        true
    }

    /// Toggle Focus view and return the resulting state.
    pub fn toggle_focus_view(&mut self) -> bool {
        self.set_focus_view(!self.focus_view)
    }

    /// Toggle full tool rendering for one message while Focus view is enabled.
    pub fn toggle_focus_turn(&mut self, message_id: &str) {
        if !self.expanded_focus_turns.remove(message_id) {
            self.expanded_focus_turns.insert(message_id.to_string());
        }
    }

    /// Toggle the most recent turn containing tool calls.
    pub fn toggle_latest_focus_turn(&mut self) -> bool {
        let Some(message_id) = self
            .messages
            .iter()
            .rev()
            .find(|message| !message.tool_calls.is_empty())
            .map(|message| message.id.clone())
        else {
            return false;
        };

        self.toggle_focus_turn(&message_id);
        true
    }

    fn set_codex_status_if_useful(&mut self, status: String) {
        if self
            .status
            .as_deref()
            .is_some_and(is_active_codex_turn_status)
        {
            return;
        }
        self.status = Some(status);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// HELPER FUNCTIONS
// ─────────────────────────────────────────────────────────────────────────────

/// Extract the first bold header from thinking text.
///
/// Looks for `**Header**` patterns in the text and returns the header text.
/// Used to show a summary of what the model is thinking about.
///
/// # Arguments
///
/// * `text` - The thinking text to search
///
/// # Returns
///
/// `Some(header)` if a header was found, `None` otherwise.
fn extract_thinking_header(text: &str) -> Option<String> {
    // Look for **Header** pattern by finding the last closing **
    // then looking backward for the opening **
    if let Some(start) = text.rfind("**") {
        let before_start = &text[..start];
        if let Some(open) = before_start.rfind("**") {
            let header = &text[open + 2..start];
            // Take only the first line and validate length
            let header = header.lines().next().unwrap_or(header);
            if !header.is_empty() && header.len() < 100 {
                return Some(header.to_string());
            }
        }
    }
    None
}

fn compact_codex_session_status(state: &str, thread_id: &str, profile: &str) -> String {
    format!("Codex {state} {profile} {}", short_codex_id(thread_id))
}

fn compact_codex_turn_status(state: &str, thread_id: &str, turn_id: Option<&str>) -> String {
    match turn_id {
        Some(turn_id) => format!(
            "Codex {state} {} {}",
            short_codex_id(thread_id),
            short_codex_id(turn_id)
        ),
        None => format!("Codex {state} {}", short_codex_id(thread_id)),
    }
}

fn compact_codex_compatibility_status(
    protocol_version: &str,
    resume: bool,
    steering: bool,
) -> String {
    let optional = match (resume, steering) {
        (true, true) => "full",
        (false, true) => "no-resume",
        (true, false) => "no-steer",
        (false, false) => "no-resume/no-steer",
    };
    format!("Codex protocol {protocol_version} {optional}")
}

fn is_active_codex_turn_status(status: &str) -> bool {
    status.starts_with("Codex accepted ")
        || status.starts_with("Codex streaming ")
        || status.starts_with("Codex steering ")
}

fn short_codex_id(value: &str) -> String {
    value.chars().take(11).collect()
}

#[cfg(test)]
mod focus_view_tests {
    use super::*;

    fn tool_message(id: &str) -> Message {
        Message {
            id: id.to_string(),
            role: MessageRole::Assistant,
            kind: MessageKind::Regular,
            content: String::new(),
            thinking: String::new(),
            streaming: false,
            tool_calls: vec![ToolCallState {
                call_id: format!("call-{id}"),
                tool: "read".to_string(),
                args: serde_json::json!({ "file_path": "Cargo.toml" }),
                status: ToolCallStatus::Completed,
                output: String::new(),
            }],
            usage: None,
            timestamp: SystemTime::UNIX_EPOCH,
            thinking_expanded: false,
        }
    }

    #[test]
    fn focus_view_defaults_off_and_toggles_explicitly() {
        let mut state = AppState::new();

        assert!(!state.focus_view);
        assert!(state.toggle_focus_view());
        assert!(state.focus_view);
        assert!(!state.set_focus_view(false));
    }

    #[test]
    fn output_detail_expanded_exits_focus_and_clears_collapsed_tool_overrides() {
        let mut state = AppState::new();
        state.messages.push(tool_message("first"));
        assert_eq!(state.output_detail(), OutputDetail::Compact);

        state.set_output_detail(OutputDetail::Summary);
        assert!(state.focus_view);
        assert_eq!(state.focus_selected_turn.as_deref(), Some("first"));
        state.toggle_focus_turn("first");
        state.toggle_tool_call("call-first");

        state.set_output_detail(OutputDetail::Expanded);
        assert_eq!(state.output_detail(), OutputDetail::Expanded);
        assert!(!state.focus_view);
        assert!(!state.compact_tool_outputs);
        assert!(state.expanded_focus_turns.is_empty());
        assert!(state.focus_selected_turn.is_none());
        assert!(state.expanded_tool_calls.is_empty());
        assert!(state.is_tool_call_expanded("call-first"));
        assert_eq!(
            state.messages[0].tool_calls[0].status,
            ToolCallStatus::Completed
        );

        state.toggle_tool_call("call-first");
        assert!(!state.is_tool_call_expanded("call-first"));
        state.set_output_detail(OutputDetail::Compact);
        assert_eq!(state.output_detail(), OutputDetail::Compact);
        assert!(!state.focus_view);
        assert!(state.compact_tool_outputs);
        assert!(state.expanded_tool_calls.is_empty());
        assert!(!state.is_tool_call_expanded("call-first"));
    }

    #[test]
    fn output_detail_summary_clears_previous_turn_expansions() {
        let mut state = AppState::new();
        state.messages.push(tool_message("first"));
        state.set_focus_view(true);
        state.toggle_focus_turn("first");
        state.set_output_detail(OutputDetail::Summary);
        assert_eq!(state.output_detail(), OutputDetail::Summary);
        assert!(state.expanded_focus_turns.is_empty());
        assert_eq!(state.focus_selected_turn.as_deref(), Some("first"));
    }

    #[test]
    fn latest_focus_turn_toggle_uses_the_latest_tool_bearing_message() {
        let mut state = AppState::new();
        state.messages.push(tool_message("first"));
        state.messages.push(Message {
            id: "text-only".to_string(),
            role: MessageRole::Assistant,
            kind: MessageKind::Regular,
            content: "Done".to_string(),
            thinking: String::new(),
            streaming: false,
            tool_calls: Vec::new(),
            usage: None,
            timestamp: SystemTime::UNIX_EPOCH,
            thinking_expanded: false,
        });
        state.messages.push(tool_message("latest"));

        assert!(state.toggle_latest_focus_turn());
        assert_eq!(
            state.expanded_focus_turns,
            std::collections::HashSet::from(["latest".to_string()])
        );
        assert!(state.toggle_latest_focus_turn());
        assert!(state.expanded_focus_turns.is_empty());
    }

    #[test]
    fn focus_selection_moves_across_older_turns_and_prunes_removed_messages() {
        let mut state = AppState::new();
        state.messages = vec![
            tool_message("first"),
            tool_message("second"),
            tool_message("third"),
        ];

        state.set_focus_view(true);
        assert_eq!(state.focus_selected_turn.as_deref(), Some("third"));
        assert!(state.select_previous_focus_turn());
        assert_eq!(state.focus_selected_turn.as_deref(), Some("second"));
        assert!(state.toggle_selected_focus_turn());
        assert!(state.expanded_focus_turns.contains("second"));

        state.messages.retain(|message| message.id != "second");
        state.prune_focus_turn_state();
        assert!(state.focus_selected_turn.is_none());
        assert!(!state.expanded_focus_turns.contains("second"));
    }
}

#[cfg(test)]
mod tests;
