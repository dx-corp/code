//! Core types and enums for the command system
//!
//! This module defines the fundamental data structures used throughout the command
//! system, including command definitions, execution context, outputs, and errors.
//!
//! # Key Types
//!
//! ## Command Definition
//!
//! - **Command**: Complete command specification with name, handler, arguments, category
//! - **`CommandHandler`**: Type alias for handler function pointer/closure
//! - **`CommandArgument`**: Argument definition with name, type, defaults, validation
//!
//! ## Execution
//!
//! - **`CommandContext`**: Runtime context passed to handlers (cwd, session, args, etc.)
//! - **`CommandResult`**: Result type for handler execution (Ok/Err)
//!
//! ## Output Types
//!
//! - **`CommandOutput`**: Enum of possible handler return values (Message, Action, Modal, etc.)
//! - **`CommandAction`**: Enum of state-modifying actions (Quit, `ClearMessages`, etc.)
//! - **`ModalType`**: Types of modals that can be opened
//!
//! ## Errors
//!
//! - **`CommandError`**: Rich error type with message and optional hint
//!
//! # Enum-based Dispatch
//!
//! The command system uses enum-based dispatch for outputs and actions rather than
//! trait objects or callbacks. This provides:
//!
//! - **Type Safety**: All possible outputs are known at compile time
//! - **Pattern Matching**: Exhaustive matching ensures all cases are handled
//! - **No Virtual Dispatch**: Compiler can inline and optimize enum matching
//! - **Easy Serialization**: Enums can be easily serialized for IPC/logging
//!
//! # Example Command Flow
//!
//! ```rust,ignore
//! use maestro_tui::commands::{Command, CommandCategory, CommandContext, CommandOutput, CommandAction};
//!
//! // 1. Define a command with handler
//! let cmd = Command::new(
//!     "quit",
//!     "Exit the application",
//!     CommandCategory::Navigation,
//!     Box::new(|_ctx: &CommandContext| {
//!         // 2. Handler returns an enum variant
//!         Ok(CommandOutput::Action(CommandAction::Quit))
//!     }),
//! );
//!
//! // 3. Execute command (in real code)
//! // let context = CommandContext { ... };
//! // match cmd.execute(&context)? {
//! //     CommandOutput::Action(CommandAction::Quit) => app.quit(),
//! //     CommandOutput::Message(msg) => display_message(msg),
//! //     _ => {}
//! // }
//! ```

pub use maestro_local_host::ui_prefs::FooterStyle;

use std::collections::HashMap;

/// Result of executing a command
///
/// Type alias for the result returned by command handlers. Either:
/// - `Ok(CommandOutput)`: Command succeeded with some output (Message, Action, etc.)
/// - `Err(CommandError)`: Command failed with an error (shown to user)
pub type CommandResult = Result<CommandOutput, CommandError>;

/// Handler function for a command
///
/// Type alias for the command handler function pointer. This is a boxed closure with:
///
/// # Traits
///
/// - **Fn(&CommandContext)**: Immutable function (can be called multiple times)
/// - **Send**: Can be safely sent between threads
/// - **Sync**: Can be safely shared between threads via references
///
/// # Box vs Arc
///
/// Handlers are `Box`ed rather than `Arc`ed because:
/// - Commands themselves are wrapped in Arc (in the registry)
/// - Handlers don't need to be shared independently of their command
/// - Box provides simple heap allocation without reference counting overhead
///
/// # Example
///
/// ```rust,ignore
/// let handler: CommandHandler = Box::new(|ctx| {
///     let name = ctx.get_string("name").unwrap_or("World");
///     Ok(CommandOutput::Message(format!("Hello, {}!", name)))
/// });
/// ```
pub type CommandHandler = Box<dyn Fn(&CommandContext) -> CommandResult + Send + Sync>;

/// Output from a command execution
///
/// Enum representing all possible outcomes of executing a command handler.
/// This uses enum-based dispatch rather than callbacks for type safety and
/// exhaustive pattern matching.
///
/// # Variants
///
/// - **Message**: Display informational text to the user (e.g., status, results)
/// - **Help**: Display help documentation (may be rendered specially)
/// - **Warning**: Display a warning (not an error, execution succeeded)
/// - **`OpenModal`**: Open a UI modal/selector (theme picker, file browser, etc.)
/// - **Action**: Execute a state-modifying action (quit, clear, refresh, etc.)
/// - **Silent**: Command succeeded but has no visible output
/// - **Multi**: Multiple outputs to be processed in sequence
///
/// # Design Rationale
///
/// Using an enum instead of callbacks or trait objects provides:
/// - Compile-time exhaustiveness checking (all variants must be handled)
/// - Clear contract between command handlers and UI layer
/// - Easy serialization for IPC or logging
/// - No need for dynamic dispatch (faster)
///
/// # Example
///
/// ```rust,ignore
/// use maestro_tui::commands::{CommandOutput, CommandAction, ModalType};
///
/// // Handler can return different output types
/// fn handle_command(cmd: &str) -> CommandOutput {
///     match cmd {
///         "quit" => CommandOutput::Action(CommandAction::Quit),
///         "help" => CommandOutput::OpenModal(ModalType::Help),
///         "status" => CommandOutput::Message("System OK".into()),
///         _ => CommandOutput::Silent,
///     }
/// }
/// ```
#[derive(Debug, Clone)]
pub enum CommandOutput {
    /// Display a message to the user (informational)
    Message(String),
    /// Display help text (may be specially formatted)
    Help(String),
    /// Display a warning (not an error, but noteworthy)
    Warning(String),
    /// Open a modal/selector (theme picker, file browser, etc.)
    OpenModal(ModalType),
    /// Execute an action that modifies application state
    Action(CommandAction),
    /// No visible output (command succeeded silently)
    Silent,
    /// Multiple outputs to be processed in sequence
    Multi(Vec<CommandOutput>),
}

/// Actions that commands can trigger to modify application state
///
/// Enum representing all possible state-modifying actions that command handlers
/// can request. The application layer pattern-matches on these to perform the
/// actual state modifications.
///
/// # Design Pattern: Command Pattern
///
/// This enum implements a variation of the Command pattern where:
/// - Handlers return action requests rather than directly modifying state
/// - The application layer owns mutable state and performs actions
/// - Clear separation between command parsing and state modification
///
/// # Why Enum Instead of Direct Mutation?
///
/// Using an enum for actions provides:
/// - **Testability**: Handlers can be tested by checking returned actions
/// - **Decoupling**: Handlers don't need references to application state
/// - **Serialization**: Actions can be logged, replayed, or sent over IPC
/// - **Type Safety**: All possible actions are known at compile time
///
/// # Variants
///
/// - **`ClearMessages`**: Remove all chat messages from the display
/// - **`ToggleZenMode`**: Switch between normal and minimal UI
/// - **`SetApprovalMode`**: Change safety/approval settings (yolo/selective/safe)
/// - **`SetThinkingLevel`**: Configure extended thinking level
/// - **Quit**: Exit the application
/// - **`RefreshWorkspace`**: Reload workspace file listing
/// - **`CopyLastMessage`**: Copy the last message to clipboard
/// - **`CompactConversation`**: Compress conversation history, optionally with custom instructions
/// - **`Mcp`**: Display Model Context Protocol status or resources/prompts
/// - **`HooksManage`**: Hook system management action (list, toggle, reload, metrics)
///
/// # Example
///
/// ```rust,ignore
/// use maestro_tui::commands::{CommandAction, CommandOutput};
///
/// // Handler returns action request
/// let output = CommandOutput::Action(CommandAction::Quit);
///
/// // Application layer handles action
/// match output {
///     CommandOutput::Action(CommandAction::Quit) => {
///         // Application performs cleanup and exits
///         app.cleanup();
///         std::process::exit(0);
///     }
///     _ => {}
/// }
/// ```
#[derive(Debug, Clone)]
pub enum CommandAction {
    /// Open a read-only settings or task navigation view.
    OpenPanel(ControlPanel),
    /// Apply one coherent tool output presentation preference.
    SetOutputDetail(crate::state::OutputDetail),
    /// Clear all messages from the chat display
    ClearMessages,
    /// Toggle zen mode (minimal UI)
    ToggleZenMode,
    /// Toggle tool output compact mode (collapse by default)
    SetCompactTools(Option<bool>),
    /// Set approval mode (yolo, selective, safe)
    SetApprovalMode(String),
    /// Cycle between normal and auto-approve interaction modes.
    CycleInteractionMode,
    /// Ask a tool-free question outside the main conversation history
    SideQuestion(String),
    /// Set extended thinking level (off, low, medium, high, max)
    SetThinkingLevel(String),
    /// Quit the application
    Quit,
    /// Refresh workspace file listing
    RefreshWorkspace,
    /// Copy the last message to system clipboard
    CopyLastMessage,
    CopyTranscript(crate::transcript_copy::CopyTarget),
    SetLanguage(crate::localization::Locale),
    /// Set the current UI theme
    SetTheme(String),
    /// Set the current model
    SetModel(String),
    /// Increase intelligence for the current task at a safe request boundary.
    Boost,
    /// Review uncommitted changes with a different model (second opinion).
    /// `None` lets the app pick a model from a different provider.
    RubberDuck {
        model: Option<String>,
    },
    /// Set the current model and persist it as the user default
    SetDefaultModel(String),
    /// Compact conversation history (with optional custom instructions)
    CompactConversation(Option<String>),
    /// Summarize a selected span into a saved child conversation.
    SummarizeConversation,
    /// MCP (Model Context Protocol) actions
    Mcp(McpAction),
    /// Native hosted Computer task console actions.
    Orb(OrbAction),
    /// Agent-to-Agent peer pairing actions
    A2a(A2aAction),
    /// Hook system management action
    HooksManage(HooksAction),
    /// Show usage and cost statistics
    ShowUsage(UsageAction),
    /// Show a token breakdown of the current session's context by category
    ShowContext,
    /// Exclude or include a registered tool schema for this session.
    SetContextTool {
        name: String,
        excluded: bool,
    },
    /// Audit the prompt and tool surface that will be sent to the model.
    ShowPromptAudit {
        json: bool,
    },
    /// Change the transcript's turn-level Focus projection.
    SetFocus(Option<bool>),
    /// Export current session
    ExportSession(ExportAction),
    /// Manage a local product issue draft; only Send performs network I/O.
    BugReport(String),
    /// Show or search prompt history
    ShowHistory(HistoryAction),
    /// Show tool execution history
    ShowToolHistory(ToolHistoryAction),
    /// Skills system action
    Skills(SkillsAction),
    /// Plugin discovery / listing action
    Plugins(PluginsAction),
    /// Queue management action
    Queue(QueueAction),
    /// Submit a steering prompt
    Steer(String),
    /// Show diagnostics/status summary
    ShowDiagnostics,
    /// List recorded alerts (agent/API errors) in the transcript
    ShowAlerts,
    /// List built-in tools
    ShowTools,
    /// Show local/cross-session memory status
    ShowMemory,
    /// Session management actions
    Session(SessionAction),
    /// Workspace trust (global config grant/revoke for project skills/plugins)
    Trust(TrustAction),
    /// Show the active interactive sandbox policy
    ShowSandbox,
    /// Open durable experiment preferences.
    ShowPreferences,
    /// Invoke a skill as a slash command (Grok-style `/skillname args`)
    InvokeSkill {
        name: String,
        args: String,
    },
    /// Invoke a flat markdown prompt/command template as a slash command
    InvokePromptTemplate {
        name: String,
        args: String,
    },
    /// Invoke an executable script (Droid-style `.composer/commands/`) as a slash command
    InvokeExecCommand {
        name: String,
        args: String,
    },
    /// Fire Jane Street magic-trace stop indicator (or toggle slow-frame mode)
    MagicTrace(MagicTraceAction),
    /// Direct user controls for existing workers.
    Worker(WorkerAction),
    /// Observe output from an existing background task.
    BackgroundMonitor(BackgroundMonitorAction),
    /// Re-run a prompt on an interval (Grok-style `/loop`).
    Loop(LoopAction),
    /// Structured goal mode (create / pause / block / complete).
    Goal(GoalAction),
    /// Durable continual-harness refinement actions.
    Harness(HarnessAction),
    /// RLM-style context variable actions.
    Rlm(RlmAction),
    /// Durable parent/subagent mailbox actions.
    Mailbox(MailboxAction),
    /// Change status-bar footer density.
    SetFooterStyle(FooterStyle),
    /// Change Dex presentation through the existing UI preferences.
    SetDexPresentation(String),
    /// Manage files queued for the next prompt (`/attach`).
    Attach(AttachAction),
    /// Scaffold or refresh AGENTS.md for the current workspace (`/init`).
    Init {
        force: bool,
    },
}

/// Product-level hosted Computer controls exposed by `/computer` in the TUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrbAction {
    List,
    Status(String),
    Followup {
        id: String,
        prompt: String,
    },
    Pause(String),
    Resume(String),
    Cancel(String),
    Collect(String),
    HandoffCreate {
        source_id: String,
        target_thread_id: String,
        files: Vec<String>,
        artifact_ids: Vec<String>,
        include_diff: bool,
    },
    HandoffList {
        target_thread_id: String,
    },
    HandoffRead {
        target_thread_id: String,
        package_id: String,
    },
}

/// `/attach` subcommands.
#[derive(Debug, Clone)]
pub enum AttachAction {
    /// Queue a local path for the next prompt.
    Add(String),
    /// List pending attachments.
    List,
    /// Clear all pending attachments.
    Clear,
    /// Drop one pending attachment by 1-based index.
    Remove { index: usize },
}

/// Goal-mode slash actions.
#[derive(Debug, Clone)]
pub enum GoalAction {
    Create {
        text: String,
        replace: bool,
        criteria: Option<String>,
        /// Safety cap on auto-continue submissions; `None` uses default (50).
        max_turns: Option<u32>,
        /// Optional Codex-style token budget for the goal.
        token_budget: Option<u64>,
        /// Optional wall-clock budget for autonomous continuation.
        max_duration_secs: Option<u64>,
    },
    Status,
    Pause,
    Resume,
    Block {
        reason: Option<String>,
    },
    Complete,
    Clear,
    AutoContinue {
        enabled: bool,
    },
}

#[derive(Debug, Clone)]
pub enum LoopAction {
    /// Start re-running `prompt` every `interval_secs` seconds.
    Start { interval_secs: u64, prompt: String },
    /// Stop the active loop.
    Stop,
    /// Show the active loop, if any.
    Status,
}

/// Continual-harness slash actions.
#[derive(Debug, Clone)]
pub enum HarnessAction {
    /// Show harness metadata and entries visible to the current scope.
    Status,
    /// List entries visible to the current scope.
    List,
    /// Show pending and reviewed evidence-backed proposals.
    Review,
    /// Stage an evidence-backed refinement proposal.
    Propose {
        scope: String,
        kind: String,
        name: String,
        content: String,
        evidence: String,
    },
    /// Add a scoped record. Evidence may be supplied after `--evidence`.
    Add {
        scope: String,
        kind: String,
        name: String,
        content: String,
        evidence: Option<String>,
    },
    /// Update a record's content. Evidence may be supplied after `--evidence`.
    Update {
        id: String,
        content: String,
        evidence: Option<String>,
    },
    /// Delete a record by id.
    Delete(String),
    /// Restore the entries captured at a prior revision.
    Rollback(u64),
    /// Apply a pending proposal.
    Apply(String),
    /// Reject a pending proposal with an optional operator note.
    Reject { id: String, note: Option<String> },
}

/// RLM context variable actions.
#[derive(Debug, Clone)]
pub enum RlmAction {
    /// Show the current variable inventory.
    List,
    /// Set or replace a variable.
    Set {
        name: String,
        value: String,
        description: Option<String>,
    },
    /// Append text to a variable.
    Append { name: String, value: String },
    /// Render `{{name}}` references in a template.
    Render(String),
    /// Remove a variable.
    Clear(String),
}

/// Durable local mailbox actions.
#[derive(Debug, Clone)]
pub enum MailboxAction {
    /// Show pending messages.
    List,
    /// Send a message.
    Send { recipient: String, body: String },
    /// Mark a message read and display it.
    Read(String),
    /// Display a control addressed to a locally owned subagent.
    Inspect(String),
    /// Acknowledge a message.
    Acknowledge(String),
    /// Release a held coordination message for delivery.
    Approve(String),
    /// Remove acknowledged messages.
    Compact,
}

#[derive(Debug, Clone)]
pub enum BackgroundMonitorAction {
    Add { task_id: String, pattern: String },
    List,
    Remove { monitor_id: String },
}

/// magic-trace control from the TUI slash menu.
#[derive(Debug, Clone)]
pub enum MagicTraceAction {
    /// Call `magic_trace_stop_indicator` once (snapshot if attached).
    Stop,
    /// Enable slow-frame auto snapshots.
    EnableSlowFrame,
    /// Disable slow-frame auto snapshots.
    DisableSlowFrame,
    /// Show status / how to attach.
    Status,
}

/// Session management actions.
#[derive(Debug, Clone)]
pub enum SessionAction {
    /// Prune old sessions by count/age limits
    Cleanup,
    /// Start a new session (clear transcript + new session file)
    New,
    /// Fork current conversation into a new session branch
    Fork,
    /// Browse and resume persisted forks.
    BrowseBranches,
    /// Rewind last N user turns (default 1)
    Rewind { turns: usize, dry_run: bool },
    /// Rewind saved conversation and corresponding file changes.
    RewindBoth { turns: usize, dry_run: bool },
    /// Restore files from the most recent turn checkpoint
    RewindFiles,
    /// List file checkpoints recorded for this session
    ListCheckpoints,
    /// Continue the most recent session for this workspace
    Continue,
    /// Show session id, path, model, sandbox, trust
    Status,
}

/// Workspace trust actions (writes only global `~/.composer/config.toml`).
#[derive(Debug, Clone)]
pub enum TrustAction {
    /// Show whether this workspace is trusted
    Status,
    /// Grant trust so project skills/plugins/hooks load
    Grant,
    /// Revoke trust
    Revoke,
}

/// Queue mode target for queue commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueModeKind {
    Steering,
    FollowUp,
}

/// Direction for moving a queued prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueMoveDirection {
    Up,
    Down,
    Now,
}

/// Queue-related actions.
#[derive(Debug, Clone)]
pub enum QueueAction {
    /// Show queue status
    Show,
    /// Cancel a queued prompt by id
    Cancel { id: u64 },
    /// Move a queued prompt relative to the other pending prompts.
    Move {
        id: u64,
        direction: QueueMoveDirection,
    },
    /// Set queue mode
    Mode {
        kind: QueueModeKind,
        mode: crate::state::QueueMode,
    },
}

/// Actions for usage/cost display
#[derive(Debug, Clone)]
pub enum UsageAction {
    /// Show summary
    Summary,
    /// Show detailed breakdown
    Detailed,
    /// Reset tracking
    Reset,
}

/// Actions for session export
#[derive(Debug, Clone)]
pub enum ExportAction {
    /// Export to markdown
    Markdown(Option<String>),
    /// Export to HTML
    Html(Option<String>),
    /// Export to JSON
    Json(Option<String>),
    /// Export to plain text
    PlainText(Option<String>),
    /// Show export options modal
    ShowOptions,
}

/// Actions for prompt history
#[derive(Debug, Clone)]
pub enum HistoryAction {
    /// Show recent history
    Recent(usize),
    /// Search history
    Search(String),
    /// Clear history
    Clear,
}

/// Actions for tool history
#[derive(Debug, Clone)]
pub enum ToolHistoryAction {
    /// Show recent executions
    Recent(usize),
    /// Show stats
    Stats,
    /// Show for specific tool
    ForTool(String),
    /// Clear history
    Clear,
}

/// Actions for MCP commands
#[derive(Debug, Clone)]
pub enum McpAction {
    /// Show MCP server status
    Status,
    /// Mutate MCP configuration through the native safe configurator.
    Configure { args: Vec<String> },
    /// List or read MCP resources
    Resources {
        server: Option<String>,
        uri: Option<String>,
    },
    /// List or fetch MCP prompts
    Prompts {
        server: Option<String>,
        name: Option<String>,
        arguments: HashMap<String, String>,
    },
}

/// Actions for A2A peer pairing commands.
#[derive(Debug, Clone)]
pub enum A2aAction {
    /// Show native A2A pairing help.
    Help,
    /// Show A2A fleet health.
    Fleet,
    /// List paired peers.
    Peers,
    /// List delegated A2A tasks.
    Tasks {
        peer: Option<String>,
        include_work_graph: bool,
    },
    /// Coordinate actionable A2A tasks.
    Coordinate {
        peer: Option<String>,
        reply: Option<String>,
        include_work_graph: bool,
    },
    /// Accept a pairing code.
    Accept { code: String },
    /// Publish this Maestro instance as a Platform A2A peer.
    Register {
        agent_id: Option<String>,
        public_url: Option<String>,
        heartbeat_only: bool,
    },
    /// Delegate work to a peer.
    Delegate { peer: String, text: String },
    /// Continue an existing A2A task.
    Reply {
        peer: String,
        task_id: String,
        text: String,
    },
    /// Send a message to a peer.
    Send { peer: String, text: String },
    /// Hand work to the default or explicitly selected peer and follow the task.
    Handoff {
        peer: Option<String>,
        text: String,
        computer_package: Option<A2aComputerHandoffSelection>,
    },
}

/// Explicit selection used to freeze an existing hosted Computer task before
/// handing work to an A2A peer. Computer remains the package owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct A2aComputerHandoffSelection {
    pub source_task_id: String,
    pub target_thread_id: String,
    pub files: Vec<String>,
    pub artifact_ids: Vec<String>,
    pub include_diff: bool,
}

/// Actions for managing the hook system
#[derive(Debug, Clone)]
pub enum HooksAction {
    /// List all registered hooks with their status
    List,
    /// Toggle hooks on/off globally
    Toggle,
    /// Reload hooks from configuration files
    Reload,
    /// Show execution metrics
    Metrics,
    /// Enable hooks
    Enable,
    /// Disable hooks
    Disable,
}

/// Actions for the skills system
#[derive(Debug, Clone)]
pub enum SkillsAction {
    /// List all available skills
    List,
    /// Activate a skill by name
    Activate(String),
    /// Deactivate a skill by name
    Deactivate(String),
    /// Reload skills from filesystem
    Reload,
    /// Show detailed info about a skill
    Info(String),
}

/// Actions for the plugin discovery system
#[derive(Debug, Clone)]
pub enum PluginsAction {
    /// List discovered plugins
    List,
    /// Show details for one plugin (by name)
    Info(String),
    /// Rediscover plugins from the filesystem
    Reload,
    /// List curated marketplace catalog
    MarketplaceList,
    /// Install a marketplace entry by id
    MarketplaceInstall { id: String, trust: bool },
}

/// Types of modals that can be opened by commands
///
/// Enum representing all UI modals (overlays, selectors, dialogs) that commands
/// can trigger. Each variant corresponds to a specific modal component in the UI.
///
/// # Variants
///
/// - **`ThemeSelector`**: Color theme picker (dark, light, custom themes)
/// - **`ModelSelector`**: AI model chooser (Claude 3.5 Sonnet, etc.)
/// - **`SessionList`**: Session browser for loading/managing conversations
/// - **`FileSearch`**: File browser/search for attaching context
/// - **`CommandPalette`**: Command search and execution
/// - **Help**: Help documentation viewer
///
/// # Example
///
/// ```rust,ignore
/// use maestro_tui::commands::{CommandOutput, ModalType};
///
/// // Command handler opens theme selector
/// let output = CommandOutput::OpenModal(ModalType::ThemeSelector);
///
/// // UI layer shows the modal
/// match output {
///     CommandOutput::OpenModal(ModalType::ThemeSelector) => {
///         ui.show_theme_selector();
///     }
///     _ => {}
/// }
/// ```
#[derive(Debug, Clone)]
pub enum ModalType {
    /// Color theme selection modal
    ThemeSelector,
    /// AI model selection modal
    ModelSelector,
    /// Session list and management modal
    SessionList,
    /// Read-only recent persisted tool executions
    Operations,
    /// File search and browser modal
    FileSearch,
    /// Command palette (searchable command list)
    CommandPalette,
    /// Keyboard shortcuts help overlay
    ShortcutsHelp,
    /// Help documentation viewer
    Help,
    /// First-run EvalOps Identity and optional local API key setup
    Setup,
}

/// Navigation within existing runtime, session, and configuration owners.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlPanel {
    Language,
    Settings,
    Account,
    Permissions,
    Appearance,
    Output,
    Footer,
    Capabilities,
    Advanced,
    Tasks,
    Context,
    Model,
    Effort,
    Review,
    Help,
    Session,
}

/// Error from command execution
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandErrorKind {
    InvalidInput,
    UnknownCommand,
}

/// Rich error type for command failures, including a message and optional hint
/// for helping the user fix the issue.
///
/// # Fields
///
/// - `message`: Primary error description (required)
/// - `hint`: Optional suggestion for resolving the error
///
/// # Builder Pattern
///
/// The `with_hint()` method enables fluent error construction:
///
/// ```rust,ignore
/// use maestro_tui::commands::CommandError;
///
/// let error = CommandError::new("Unknown command: /foo")
///     .with_hint("Type /help to see available commands");
/// ```
///
/// # Display Format
///
/// When displayed via `Display` trait:
/// - Without hint: "Unknown command"
/// - With hint: "Unknown command (Type /help to see available commands)"
///
/// # Example
///
/// ```rust,ignore
/// use maestro_tui::commands::{CommandError, CommandResult, CommandOutput};
///
/// fn validate_arg(value: &str) -> CommandResult {
///     if value.is_empty() {
///         return Err(
///             CommandError::new("Argument cannot be empty")
///                 .with_hint("Provide a non-empty value")
///         );
///     }
///     Ok(CommandOutput::Silent)
/// }
/// ```
#[derive(Debug, Clone)]
pub struct CommandError {
    pub kind: CommandErrorKind,
    /// Primary error message
    pub message: String,
    /// Optional hint for resolving the error
    pub hint: Option<String>,
}

impl CommandError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            kind: CommandErrorKind::InvalidInput,
            message: message.into(),
            hint: None,
        }
    }

    pub fn with_kind(mut self, kind: CommandErrorKind) -> Self {
        self.kind = kind;
        self
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)?;
        if let Some(ref hint) = self.hint {
            write!(f, " ({hint})")?;
        }
        Ok(())
    }
}

impl std::error::Error for CommandError {}

/// Context passed to command handlers
///
/// Contains all runtime information needed by a command handler to execute,
/// including parsed arguments, current environment state, and metadata.
///
/// # Fields
///
/// ## Input Parsing
///
/// - `input`: Full command string as typed by user (e.g., "/help theme")
/// - `command_name`: The matched command name (could be primary name or alias)
/// - `args`: Parsed and typed arguments as a `HashMap`
/// - `raw_args`: Unparsed argument string (everything after command name)
///
/// ## Environment State
///
/// - `cwd`: Current working directory (for file-related commands)
/// - `session_id`: Optional current session ID (for session management)
/// - `model`: Optional current AI model name
///
/// # Argument Access Helpers
///
/// The context provides typed accessor methods:
/// - `get_string()`: Get a string argument by name
/// - `get_bool()`: Get a boolean argument by name
/// - `get_int()`: Get an integer argument by name
/// - `has_flag()`: Check if a boolean flag is true
///
/// # Example
///
/// ```rust,ignore
/// use maestro_tui::commands::{CommandContext, CommandResult, CommandOutput};
/// use std::collections::HashMap;
///
/// fn my_handler(ctx: &CommandContext) -> CommandResult {
///     // Access parsed arguments
///     let name = ctx.get_string("name").unwrap_or("default");
///     let verbose = ctx.has_flag("verbose");
///
///     // Access environment state
///     println!("Running in: {}", ctx.cwd);
///     if let Some(ref session) = ctx.session_id {
///         println!("Session: {}", session);
///     }
///
///     Ok(CommandOutput::Message(format!("Hello, {}!", name)))
/// }
/// ```
#[derive(Debug, Clone)]
pub struct CommandContext {
    /// The full input text (including slash) as typed by user
    pub input: String,
    /// The command name that was matched (could be alias)
    pub command_name: String,
    /// Parsed and typed arguments
    pub args: HashMap<String, ArgumentValue>,
    /// Raw argument string (everything after command name, unparsed)
    pub raw_args: String,
    /// Current working directory
    pub cwd: String,
    /// Current session ID (if in a session)
    pub session_id: Option<String>,
    /// Current AI model name (if set)
    pub model: Option<String>,
}

impl CommandContext {
    /// Get a string argument
    #[must_use]
    pub fn get_string(&self, name: &str) -> Option<&str> {
        match self.args.get(name)? {
            ArgumentValue::String(s) => Some(s),
            _ => None,
        }
    }

    /// Get a boolean argument
    #[must_use]
    pub fn get_bool(&self, name: &str) -> Option<bool> {
        match self.args.get(name)? {
            ArgumentValue::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Get an integer argument
    #[must_use]
    pub fn get_int(&self, name: &str) -> Option<i64> {
        match self.args.get(name)? {
            ArgumentValue::Int(i) => Some(*i),
            _ => None,
        }
    }

    /// Check if a flag is present
    #[must_use]
    pub fn has_flag(&self, name: &str) -> bool {
        self.get_bool(name).unwrap_or(false)
    }
}

/// A parsed argument value
#[derive(Debug, Clone)]
pub enum ArgumentValue {
    String(String),
    Bool(bool),
    Int(i64),
    List(Vec<String>),
}

/// Command category for organization
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommandCategory {
    /// UI-related commands (theme, clean, etc.)
    Ui,
    /// Session management
    Session,
    /// Tool management
    Tools,
    /// Safety and approval settings
    Safety,
    /// Diagnostic commands
    Diagnostics,
    /// Configuration
    Config,
    /// Navigation
    Navigation,
    /// Context management
    Context,
}

impl CommandCategory {
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            CommandCategory::Ui => "UI",
            CommandCategory::Session => maestro_ui::localization::tr("Session"),
            CommandCategory::Tools => maestro_ui::localization::tr("Tools"),
            CommandCategory::Safety => maestro_ui::localization::tr("Safety"),
            CommandCategory::Diagnostics => maestro_ui::localization::tr("Diagnostics"),
            CommandCategory::Config => maestro_ui::localization::tr("Config"),
            CommandCategory::Navigation => maestro_ui::localization::tr("Navigation"),
            CommandCategory::Context => maestro_ui::localization::tr("Context"),
        }
    }

    #[must_use]
    pub fn description(&self) -> &'static str {
        match self {
            CommandCategory::Ui => maestro_ui::localization::tr("User interface settings"),
            CommandCategory::Session => maestro_ui::localization::tr("Session management"),
            CommandCategory::Tools => maestro_ui::localization::tr("Tool and MCP management"),
            CommandCategory::Safety => maestro_ui::localization::tr("Safety and approval settings"),
            CommandCategory::Diagnostics => maestro_ui::localization::tr("System diagnostics"),
            CommandCategory::Config => maestro_ui::localization::tr("Configuration options"),
            CommandCategory::Navigation => maestro_ui::localization::tr("Navigation and search"),
            CommandCategory::Context => maestro_ui::localization::tr("Context management"),
        }
    }
}

/// Type of command argument
#[derive(Debug, Clone)]
pub enum CommandArgumentType {
    /// Free-form string
    String,
    /// Boolean flag
    Bool,
    /// Integer
    Int,
    /// One of several options
    Choice(Vec<String>),
    /// File path (enables file completion)
    FilePath,
    /// Session ID
    SessionId,
}

/// Definition of a command argument
#[derive(Debug, Clone)]
pub struct CommandArgument {
    /// Argument name
    pub name: String,
    /// Short description
    pub description: String,
    /// Argument type
    pub arg_type: CommandArgumentType,
    /// Whether the argument is required
    pub required: bool,
    /// Default value if not provided
    pub default: Option<ArgumentValue>,
}

impl CommandArgument {
    pub fn string(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            arg_type: CommandArgumentType::String,
            required: false,
            default: None,
        }
    }

    pub fn bool(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            arg_type: CommandArgumentType::Bool,
            required: false,
            default: Some(ArgumentValue::Bool(false)),
        }
    }

    pub fn choice(
        name: impl Into<String>,
        description: impl Into<String>,
        choices: Vec<&str>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            arg_type: CommandArgumentType::Choice(choices.into_iter().map(String::from).collect()),
            required: false,
            default: None,
        }
    }

    #[must_use]
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    #[must_use]
    pub fn with_default(mut self, value: ArgumentValue) -> Self {
        self.default = Some(value);
        self
    }
}

/// A command definition
///
/// Complete specification of a slash command including metadata, arguments,
/// and the handler function that executes the command logic.
///
/// # Fields
///
/// ## Metadata
///
/// - `name`: Primary command name without slash (e.g., "help", "quit")
/// - `description`: Short description shown in help/autocomplete
/// - `usage`: Usage example shown in help (e.g., "/help [command]")
/// - `category`: Organizational category (UI, Session, Tools, etc.)
///
/// ## Aliases and Arguments
///
/// - `aliases`: Alternative names for the command (e.g., "h" for "help")
/// - `arguments`: Argument definitions with types, defaults, validation
///
/// ## Handler
///
/// - `handler`: Boxed closure that implements the command logic
///
/// ## Subcommands (Optional)
///
/// - `is_group`: True if this command has subcommands
/// - `subcommands`: List of subcommand names (e.g., `["info", "new", "list"]`)
///
/// # Builder Pattern
///
/// The Command struct uses a builder pattern for fluent construction:
///
/// ```rust,ignore
/// use maestro_tui::commands::{Command, CommandCategory, CommandArgument, CommandOutput};
///
/// let cmd = Command::new(
///     "greet",
///     "Greet the user",
///     CommandCategory::Ui,
///     Box::new(|ctx| Ok(CommandOutput::Silent)),
/// )
/// .alias("hi")
/// .alias("hello")
/// .arg(CommandArgument::string("name", "User's name"))
/// .usage("/greet <name>");
/// ```
///
/// # Handler Function Signature
///
/// Handlers must match this signature:
/// ```rust,ignore
/// Box<dyn Fn(&CommandContext) -> CommandResult + Send + Sync>
/// ```
///
/// Where:
/// - `Fn`: Can be called multiple times without consuming
/// - `Send + Sync`: Thread-safe for concurrent access
/// - `&CommandContext`: Immutable reference to execution context
/// - `CommandResult`: Returns Ok(CommandOutput) or Err(CommandError)
pub struct Command {
    /// Only application-owned descriptions are translated.
    pub localized_description: bool,
    /// Primary browse order; None keeps compatibility/advanced commands searchable only.
    pub browse_order: Option<u16>,
    /// Primary command name (without slash)
    pub name: String,
    /// Short description for help and autocomplete
    pub description: String,
    /// Usage example (e.g., "/command [args]")
    pub usage: String,
    /// Command category for organization
    pub category: CommandCategory,
    /// Alternative names (aliases like "h" for "help")
    pub aliases: Vec<String>,
    /// Argument definitions
    pub arguments: Vec<CommandArgument>,
    /// The command handler function
    pub handler: CommandHandler,
    /// Whether this command has subcommands
    pub is_group: bool,
    /// Subcommand names (if `is_group` is true)
    pub subcommands: Vec<String>,
}

impl Command {
    /// Create a new command
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        category: CommandCategory,
        handler: CommandHandler,
    ) -> Self {
        let name = name.into();
        Self {
            localized_description: false,
            browse_order: None,
            usage: format!("/{name}"),
            name,
            description: description.into(),
            category,
            aliases: Vec::new(),
            arguments: Vec::new(),
            handler,
            is_group: false,
            subcommands: Vec::new(),
        }
    }

    pub fn localized(mut self) -> Self {
        self.localized_description = true;
        self
    }

    pub fn display_description(&self) -> &str {
        if self.localized_description {
            maestro_ui::localization::tr(&self.description)
        } else {
            &self.description
        }
    }

    /// Include this command in normal discovery at a stable position.
    pub fn primary(mut self, order: u16) -> Self {
        self.browse_order = Some(order);
        self
    }

    /// Add an alias
    pub fn alias(mut self, alias: impl Into<String>) -> Self {
        self.aliases.push(alias.into());
        self
    }

    /// Add an argument
    #[must_use]
    pub fn arg(mut self, arg: CommandArgument) -> Self {
        self.arguments.push(arg);
        self
    }

    /// Set usage string
    pub fn usage(mut self, usage: impl Into<String>) -> Self {
        self.usage = usage.into();
        self
    }

    /// Make this a command group
    pub fn group(mut self, subcommands: Vec<&str>) -> Self {
        self.is_group = true;
        self.subcommands = subcommands.into_iter().map(String::from).collect();
        self
    }

    /// Execute the command
    pub fn execute(&self, ctx: &CommandContext) -> CommandResult {
        (self.handler)(ctx)
    }
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Command")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("category", &self.category)
            .field("aliases", &self.aliases)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_context_get_string() {
        let mut args = HashMap::new();
        args.insert(
            "name".to_string(),
            ArgumentValue::String("test".to_string()),
        );

        let ctx = CommandContext {
            input: "/test name".to_string(),
            command_name: "test".to_string(),
            args,
            raw_args: "name".to_string(),
            cwd: "/tmp".to_string(),
            session_id: None,
            model: None,
        };

        assert_eq!(ctx.get_string("name"), Some("test"));
        assert_eq!(ctx.get_string("missing"), None);
    }

    #[test]
    fn command_builder() {
        let cmd = Command::new(
            "test",
            "A test command",
            CommandCategory::Diagnostics,
            Box::new(|_| Ok(CommandOutput::Silent)),
        )
        .alias("t")
        .arg(CommandArgument::string("name", "The name"))
        .usage("/test [name]");

        assert_eq!(cmd.name, "test");
        assert_eq!(cmd.aliases, vec!["t"]);
        assert_eq!(cmd.arguments.len(), 1);
    }
}

/// User-selected worker operation; execution remains in the subagent owner.
#[derive(Debug, Clone)]
pub enum WorkerAction {
    List,
    Inspect(String),
    Steer { agent_ref: String, message: String },
    Cancel(String),
    Resume { id: String, message: String },
}
impl WorkerAction {
    pub(crate) fn tool_call(&self) -> (&'static str, serde_json::Value) {
        use serde_json::json;
        match self {
            Self::List => ("list_subagents", json!({})),
            Self::Inspect(id) => ("get_subagent", json!({"subagent_id":id})),
            Self::Steer { agent_ref, message } => (
                "control_subagent",
                json!({"agent_ref":agent_ref,"mode":"steer","message":message}),
            ),
            Self::Cancel(id) => ("cancel_subagent", json!({"subagent_id":id})),
            Self::Resume { id, message } => (
                "resume_subagent",
                json!({"subagent_id":id,"follow_up":message}),
            ),
        }
    }
}
