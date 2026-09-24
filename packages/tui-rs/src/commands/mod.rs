//! Slash command system for TUI interactions
//!
//! This module provides a complete command infrastructure for slash commands (e.g., `/help`, `/quit`)
//! in the terminal user interface. It includes:
//!
//! - **Command registration and dispatch**: Define commands with handlers, arguments, and metadata
//! - **Fuzzy matching**: Intelligent command matching with prefix, substring, and alias support
//! - **Tab completion**: Cycle through command suggestions with Tab/Shift+Tab
//! - **Organized categories**: Group commands by function (UI, Session, Tools, etc.)
//!
//! # Architecture
//!
//! The command system is built on four core components:
//!
//! ## 1. Command Registry (`registry.rs`)
//!
//! Central storage for all available commands. Provides:
//! - Name and alias-based lookup using HashMap for O(1) access
//! - Arc-based shared ownership for thread-safe command distribution
//! - Argument parsing and validation
//! - Command execution with context injection
//!
//! ## 2. Command Types (`types.rs`)
//!
//! Core data structures and enums:
//! - `Command`: Full command definition with name, handler, arguments, category
//! - `CommandAction`: Enum representing state-modifying actions (e.g., Quit, ClearMessages)
//! - `CommandContext`: Runtime context passed to handlers (cwd, session, model, args)
//! - `CommandOutput`: Enum for handler return values (Message, OpenModal, Action, etc.)
//!
//! ## 3. Fuzzy Matcher (`matcher.rs`)
//!
//! Intelligent command matching with scoring:
//! - Exact match (score: 100)
//! - Prefix match for names (score: 70) and aliases (score: 55)
//! - Substring match for names (score: 25) and aliases (score: 15)
//! - Bonus points for favorites and recently used commands
//!
//! ## 4. Tab Completion State (`matcher.rs`)
//!
//! Maintains state for cycling through command suggestions:
//! - Query caching to avoid recomputation
//! - Forward and backward cycling through matches
//! - Automatic reset when query changes
//!
//! # Usage Example
//!
//! ```rust,ignore
//! use maestro_tui::commands::{build_command_registry, SlashCommandMatcher};
//! use std::sync::Arc;
//!
//! // Build the default command registry
//! let registry = Arc::new(build_command_registry());
//!
//! // Create a matcher for fuzzy search and tab completion
//! let matcher = SlashCommandMatcher::new(Arc::clone(&registry));
//!
//! // Execute a command
//! let result = registry.execute("/help", "/home/user", Some("session-123"), Some("claude-3-5-sonnet"));
//!
//! // Get fuzzy matches for autocomplete
//! let matches = matcher.get_matches("/hel");  // Returns "help" with high score
//! ```
//!
//! # Extending the System
//!
//! To add a new command:
//!
//! 1. Register it in `build_command_registry()` in `registry.rs`
//! 2. Define the handler as a boxed closure: `Box::new(|ctx| { ... })`
//! 3. Optionally add aliases, arguments, and usage examples
//! 4. Choose an appropriate category for organization
//!
//! Example:
//!
//! ```rust,ignore
//! registry.register(
//!     Command::new(
//!         "mycommand",
//!         "Description of my command",
//!         CommandCategory::Ui,
//!         Box::new(|ctx| {
//!             Ok(CommandOutput::Message(format!("Hello from {}", ctx.cwd)))
//!         }),
//!     )
//!     .alias("mc")
//!     .arg(CommandArgument::string("name", "Your name"))
//!     .usage("/mycommand [name]"),
//! );
//! ```

mod matcher;
mod registry;
mod types;

pub use matcher::{
    ArgCompletion, CommandMatch, InlineCompletion, RichCompletion, SlashCommandMatcher,
    SlashCycleState,
};
pub use registry::{
    CommandRegistry, build_command_registry, build_command_registry_with_extensions,
    register_exec_commands,
};
pub use types::{
    A2aAction, A2aComputerHandoffSelection, AttachAction, BackgroundMonitorAction, Command,
    CommandAction, CommandArgument, CommandArgumentType, CommandCategory, CommandContext,
    CommandError, CommandErrorKind, CommandHandler, CommandOutput, CommandResult, ControlPanel,
    ExportAction, FooterStyle, GoalAction, HarnessAction, HistoryAction, HooksAction, LoopAction,
    MagicTraceAction, MailboxAction, McpAction, ModalType, OrbAction, PluginsAction, QueueAction,
    QueueModeKind, QueueMoveDirection, RlmAction, SessionAction, SkillsAction, ToolHistoryAction,
    TrustAction, UsageAction, WorkerAction,
};
