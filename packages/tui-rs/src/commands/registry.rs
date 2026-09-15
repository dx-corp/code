//! Command registry and execution engine
//!
//! This module implements the central command storage and dispatch system. The `CommandRegistry`
//! maintains a collection of all available slash commands and provides efficient lookup by name
//! or alias, argument parsing, and command execution.
//!
//! # Key Concepts
//!
//! ## Arc-based Shared Ownership
//!
//! Commands are stored as `Arc<Command>` to enable safe sharing across threads without copying:
//! - Multiple components can hold references to the same command
//! - Commands are immutable after registration (internal mutability via handler closures)
//! - Cheap cloning via reference counting instead of deep copies
//!
//! ## HashMap-based Lookup
//!
//! Two `HashMaps` provide O(1) lookup performance:
//! - `commands`: Primary name to Command mapping
//! - `aliases`: Alias to primary name mapping (double indirection)
//!
//! ## Command Execution Pipeline
//!
//! When `execute()` is called with input like `/help theme`:
//!
//! 1. **Parse**: Strip `/`, split command name from arguments
//! 2. **Lookup**: Find command by name or alias in registry
//! 3. **Parse Arguments**: Convert raw string into typed arguments based on command definition
//! 4. **Build Context**: Package inputs (cwd, session, model, args) into `CommandContext`
//! 5. **Execute Handler**: Call the command's handler function with the context
//! 6. **Return Output**: Handler returns `CommandOutput` enum (Message, Action, Modal, etc.)
//!
//! # Example
//!
//! ```rust,ignore
//! use maestro_tui::commands::{CommandRegistry, Command, CommandCategory, CommandOutput};
//!
//! let mut registry = CommandRegistry::new();
//!
//! // Register a simple command
//! registry.register(
//!     Command::new(
//!         "greet",
//!         "Greet the user",
//!         CommandCategory::Ui,
//!         Box::new(|ctx| {
//!             let name = ctx.get_string("name").unwrap_or("stranger");
//!             Ok(CommandOutput::Message(format!("Hello, {}!", name)))
//!         }),
//!     )
//!     .alias("hi")
//!     .arg(CommandArgument::string("name", "Your name")),
//! );
//!
//! // Execute by primary name
//! let result = registry.execute("/greet Alice", "/home", None, None);
//!
//! // Execute by alias
//! let result = registry.execute("/hi Bob", "/home", None, None);
//! ```
//!
//! # Argument Parsing
//!
//! The `parse_arguments()` function converts raw input strings into typed values:
//! - Positional parsing: Arguments are matched to definitions in order
//! - Type validation: Strings, integers, booleans, and choices are validated
//! - Required vs. optional: Missing required arguments return an error
//! - Default values: Applied before positional parsing
//!
//! See `CommandArgument` in `types.rs` for argument definition details.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use super::types::{
    A2aAction, A2aComputerHandoffSelection, ArgumentValue, AttachAction, BackgroundMonitorAction,
    Command, CommandAction, CommandArgument, CommandCategory, CommandContext, CommandError,
    CommandErrorKind, CommandOutput, CommandResult, ControlPanel, ExportAction, FooterStyle,
    GoalAction, HarnessAction, HistoryAction, HooksAction, LoopAction, MailboxAction, McpAction,
    ModalType, OrbAction, PlanReviewAction, PluginsAction, QueueAction, QueueModeKind,
    QueueMoveDirection, RlmAction, SessionAction, SkillsAction, ToolHistoryAction, UsageAction,
};
use crate::git;
use crate::keybindings::{
    format_keybindings_config_report, initialize_keybindings_file, keybindings_config_path,
};
use crate::lsp::max_diagnostics_per_file;
use crate::state::QueueMode;
use crate::tool_output::tool_output_limits;

/// Registry of all available commands with efficient lookup and execution
///
/// The `CommandRegistry` is the central storage for slash commands in the TUI. It provides:
/// - Fast name-based and alias-based lookup using `HashMaps`
/// - Thread-safe command sharing via Arc (atomic reference counting)
/// - Argument parsing and validation
/// - Command execution with runtime context
///
/// # Thread Safety
///
/// While the registry itself requires `&mut self` for registration (expected to happen
/// at initialization), command lookup and execution only require `&self`. Commands are
/// stored as `Arc<Command>`, allowing cheap cloning for concurrent access.
///
/// # Examples
///
/// ```rust,ignore
/// use maestro_tui::commands::{CommandRegistry, build_command_registry};
/// use std::sync::Arc;
///
/// // Build the default registry
/// let registry = build_command_registry();
///
/// // Get a command by name
/// let help_cmd = registry.get("help");
///
/// // Get a command by alias
/// let help_by_alias = registry.get("h");  // Same as "help"
///
/// // Execute a command
/// let result = registry.execute("/help theme", "/home/user", None, None);
/// ```
pub struct CommandRegistry {
    /// Commands indexed by primary name for O(1) lookup
    commands: HashMap<String, Arc<Command>>,
    /// Alias to primary command name mapping (double indirection for lookup)
    aliases: HashMap<String, String>,
}

impl CommandRegistry {
    /// Create a new empty registry
    #[must_use]
    pub fn new() -> Self {
        Self {
            commands: HashMap::new(),
            aliases: HashMap::new(),
        }
    }

    /// Register a command in the registry
    ///
    /// Adds a command to the registry, making it available for lookup and execution.
    /// Also registers all aliases defined in the command.
    ///
    /// # Arc Wrapping
    ///
    /// The command is wrapped in an `Arc` (atomic reference counted pointer) to enable:
    /// - Cheap cloning for concurrent access (only increments a counter)
    /// - Shared ownership across multiple matcher and UI components
    /// - Thread-safe distribution without locks
    ///
    /// # Alias Registration
    ///
    /// All aliases in `command.aliases` are registered in the `aliases` `HashMap`,
    /// pointing to the primary command name. This allows lookup by either name or alias.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use maestro_tui::commands::{CommandRegistry, Command, CommandCategory, CommandOutput};
    ///
    /// let mut registry = CommandRegistry::new();
    ///
    /// registry.register(
    ///     Command::new(
    ///         "help",
    ///         "Show help",
    ///         CommandCategory::Navigation,
    ///         Box::new(|_| Ok(CommandOutput::Silent)),
    ///     )
    ///     .alias("h")
    ///     .alias("?"),
    /// );
    ///
    /// assert!(registry.get("help").is_some());
    /// assert!(registry.get("h").is_some());
    /// assert!(registry.get("?").is_some());
    /// ```
    pub fn register(&mut self, command: Command) {
        let name = command.name.clone();
        assert!(
            self.get(&name).is_none(),
            "duplicate command registration: {name}"
        );
        for alias in &command.aliases {
            assert!(
                self.get(alias).is_none(),
                "duplicate command alias registration: {alias} (on {name})"
            );
        }
        let cmd = Arc::new(command);

        // Register aliases pointing to primary name
        for alias in &cmd.aliases {
            self.aliases.insert(alias.clone(), name.clone());
        }

        self.commands.insert(name, cmd);
    }

    /// Register a command only if its primary name and aliases are free.
    /// Built-ins always win when this is used after `build_command_registry`.
    pub fn register_if_absent(&mut self, command: Command) -> bool {
        if self.get(&command.name).is_some() {
            return false;
        }
        for alias in &command.aliases {
            if self.get(alias).is_some() {
                return false;
            }
        }
        self.register(command);
        true
    }

    /// Get a command by name or alias
    ///
    /// Performs a two-stage lookup:
    /// 1. Direct lookup in the `commands` `HashMap`
    /// 2. If not found, lookup in the `aliases` `HashMap` to get the primary name,
    ///    then lookup the primary name in `commands`
    ///
    /// Returns `Arc<Command>` for cheap cloning. The Arc is cloned (incrementing
    /// the reference count) rather than the entire Command structure.
    ///
    /// # Time Complexity
    ///
    /// O(1) average case for both direct and alias lookup (two `HashMap` lookups max).
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use maestro_tui::commands::build_command_registry;
    ///
    /// let registry = build_command_registry();
    ///
    /// // Get by primary name
    /// let help = registry.get("help");
    /// assert!(help.is_some());
    ///
    /// // Get by alias
    /// let help_alias = registry.get("h");
    /// assert!(help_alias.is_some());
    ///
    /// // Both return the same command
    /// assert_eq!(help.unwrap().name, help_alias.unwrap().name);
    /// ```
    pub fn get(&self, name: &str) -> Option<Arc<Command>> {
        // Try direct lookup first (primary name)
        if let Some(cmd) = self.commands.get(name) {
            return Some(Arc::clone(cmd));
        }

        // Try alias lookup (double indirection: alias -> name -> command)
        if let Some(real_name) = self.aliases.get(name) {
            return self.commands.get(real_name).map(Arc::clone);
        }

        None
    }

    /// Resolve a mistyped command name when it is within edit distance 1 of
    /// exactly one command name or alias (e.g. `quti` -> `quit`).
    ///
    /// This is the last-resort rescue before an unknown slash command falls
    /// through to the agent as a prompt: a single-character typo should not
    /// turn into a paid LLM call.
    ///
    /// Inputs shorter than 3 characters never match — at that length a
    /// distance-1 match is more likely to be a different intent than a typo.
    ///
    /// # Returns
    ///
    /// Same contract as [`Self::resolve_unique_prefix`]: `Ok(Some(cmd))` for a
    /// unique rescue, `Ok(None)` when nothing is close enough, and
    /// `Err(candidates)` when more than one command is within edit distance 1.
    pub fn resolve_typo(&self, typed: &str) -> Result<Option<Arc<Command>>, Vec<String>> {
        let typed = typed.to_lowercase();
        if typed.chars().count() < 3 {
            return Ok(None);
        }

        let mut matches: Vec<Arc<Command>> = Vec::new();
        for cmd in self.commands.values() {
            let name_hit = edit_distance(&typed, &cmd.name.to_lowercase()) <= 1;
            let alias_hit = cmd
                .aliases
                .iter()
                .any(|alias| edit_distance(&typed, &alias.to_lowercase()) <= 1);
            if name_hit || alias_hit {
                matches.push(Arc::clone(cmd));
            }
        }

        match matches.len() {
            0 => Ok(None),
            1 => Ok(matches.pop()),
            _ => {
                let mut names: Vec<String> = matches.iter().map(|cmd| cmd.name.clone()).collect();
                names.sort_unstable();
                Err(names)
            }
        }
    }

    /// Get all commands
    #[must_use]
    pub fn all(&self) -> Vec<Arc<Command>> {
        self.commands.values().cloned().collect()
    }

    /// Resolve a partial command name when it is an unambiguous prefix of
    /// exactly one command name or alias (e.g. `qui` -> `quit`).
    ///
    /// Used to make bare `Enter` on a partial slash command do what the user
    /// means instead of erroring (or leaking the partial command to the agent
    /// as a prompt).
    ///
    /// # Returns
    ///
    /// - `Ok(Some(cmd))`: exactly one command matches the prefix
    /// - `Ok(None)`: no command name or alias starts with `partial`
    /// - `Err(candidates)`: the prefix is ambiguous; `candidates` is the
    ///   sorted list of matching canonical command names for display
    ///
    /// Exact matches should be resolved via [`Self::get`] first; this method
    /// only considers proper prefixes and is case-insensitive.
    pub fn resolve_unique_prefix(
        &self,
        partial: &str,
    ) -> Result<Option<Arc<Command>>, Vec<String>> {
        let partial = partial.to_lowercase();
        if partial.is_empty() {
            return Ok(None);
        }

        let mut matches: Vec<Arc<Command>> = Vec::new();
        for cmd in self.commands.values() {
            let name_hit = cmd.name.to_lowercase().starts_with(&partial);
            let alias_hit = cmd
                .aliases
                .iter()
                .any(|alias| alias.to_lowercase().starts_with(&partial));
            if name_hit || alias_hit {
                matches.push(Arc::clone(cmd));
            }
        }

        match matches.len() {
            0 => Ok(None),
            1 => Ok(matches.pop()),
            _ => {
                let mut names: Vec<String> = matches.iter().map(|cmd| cmd.name.clone()).collect();
                names.sort_unstable();
                Err(names)
            }
        }
    }

    /// Primary commands in the order shared by slash discovery, search, and help.
    pub fn primary_commands(&self) -> Vec<Arc<Command>> {
        let mut commands = self.all();
        commands.retain(|command| command.browse_order.is_some());
        commands.sort_by_key(|command| (command.browse_order, command.name.clone()));
        commands
    }

    /// Get all command names (including aliases)
    #[must_use]
    pub fn all_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self
            .commands
            .keys()
            .map(std::string::String::as_str)
            .collect();
        names.extend(self.aliases.keys().map(std::string::String::as_str));
        names.sort_unstable();
        names
    }

    /// Get commands by category
    #[must_use]
    pub fn by_category(&self, category: CommandCategory) -> Vec<Arc<Command>> {
        self.commands
            .values()
            .filter(|cmd| cmd.category == category)
            .cloned()
            .collect()
    }

    /// Execute a command from input text
    ///
    /// Parses the input string, looks up the command, validates arguments,
    /// builds a context, and executes the command handler.
    ///
    /// # Arguments
    ///
    /// * `input` - The full command string (must start with `/`)
    /// * `cwd` - Current working directory (passed to handler context)
    /// * `session_id` - Optional current session ID (passed to handler context)
    /// * `model` - Optional current AI model (passed to handler context)
    ///
    /// # Execution Pipeline
    ///
    /// 1. **Validation**: Ensure input starts with `/`
    /// 2. **Parsing**: Split input into command name and raw arguments
    /// 3. **Lookup**: Find the command by name or alias
    /// 4. **Argument Parsing**: Convert raw args to typed values using `parse_arguments()`
    /// 5. **Context Building**: Create `CommandContext` with all inputs
    /// 6. **Execution**: Call the command's handler function
    ///
    /// # Returns
    ///
    /// Returns `CommandResult` which is `Result<CommandOutput, CommandError>`:
    /// - `Ok(CommandOutput)`: Handler executed successfully (Message, Action, Modal, etc.)
    /// - `Err(CommandError)`: Parsing failed, unknown command, or handler returned error
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use maestro_tui::commands::build_command_registry;
    ///
    /// let registry = build_command_registry();
    ///
    /// // Execute a simple command
    /// let result = registry.execute("/help", "/home/user", None, None);
    /// assert!(result.is_ok());
    ///
    /// // Execute with arguments
    /// let result = registry.execute("/theme dark", "/home/user", None, None);
    ///
    /// // Invalid command returns error
    /// let result = registry.execute("/notacommand", "/home/user", None, None);
    /// assert!(result.is_err());
    /// ```
    pub fn execute(
        &self,
        input: &str,
        cwd: &str,
        session_id: Option<&str>,
        model: Option<&str>,
    ) -> CommandResult {
        let input = input.trim();

        // Must start with /
        if !input.starts_with('/') {
            return Err(CommandError::new(maestro_ui::localization::tr(
                "Commands must start with /",
            )));
        }

        // Tolerate accidental double-slash from completion bugs (`//help`) or paste.
        let input_without_slash = input.trim_start_matches('/');

        // Split into command and args
        let mut parts = input_without_slash.splitn(2, char::is_whitespace);
        let command_name = parts.next().unwrap_or("").to_lowercase();
        let raw_args = parts.next().unwrap_or("").trim().to_string();

        // Find the command
        let command = self.get(&command_name).ok_or_else(|| {
            CommandError::new(maestro_ui::localization::format(
                "Unknown command: /{0}",
                std::slice::from_ref(&(command_name)),
            ))
            .with_kind(CommandErrorKind::UnknownCommand)
            .with_hint(maestro_ui::localization::tr(
                "Type /help to see available commands",
            ))
        })?;

        if command.name == "help" && raw_args == "commands" {
            return Ok(CommandOutput::OpenModal(ModalType::Help));
        }
        if command.name == "help" && !raw_args.is_empty() {
            return self.help_for_command(&raw_args);
        }

        // Parse arguments
        let args = parse_arguments(&raw_args, &command.arguments)?;

        // Build context
        let ctx = CommandContext {
            input: input.to_string(),
            command_name: command.name.clone(),
            args,
            raw_args,
            cwd: cwd.to_string(),
            session_id: session_id.map(String::from),
            model: model.map(String::from),
        };

        // Execute
        command.execute(&ctx)
    }

    fn help_for_command(&self, query: &str) -> CommandResult {
        let name = query
            .split_whitespace()
            .next()
            .unwrap_or(query)
            .trim_start_matches('/');
        let command = self.get(name).ok_or_else(|| {
            CommandError::new(maestro_ui::localization::format(
                "No help available for /{0}",
                &[(name).to_string()],
            ))
            .with_hint(maestro_ui::localization::tr(
                "Type /help to see available commands",
            ))
        })?;
        let mut lines = vec![format!(
            "/{} — {}",
            command.name,
            command.display_description()
        )];
        if !command.aliases.is_empty() {
            let aliases = command
                .aliases
                .iter()
                .map(|alias| format!("/{alias}"))
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(maestro_ui::localization::format(
                "Aliases: {0}",
                std::slice::from_ref(&(aliases)),
            ));
        }
        if !command.usage.is_empty() {
            lines.push(maestro_ui::localization::format(
                "Usage: {0}",
                std::slice::from_ref(&(command.usage)),
            ));
        }
        if !command.subcommands.is_empty() {
            lines.push(maestro_ui::localization::format(
                "Subcommands: {0}",
                &[(command.subcommands.join(", ")).clone()],
            ));
        }
        Ok(CommandOutput::Message(lines.join("\n")))
    }

    /// Get the number of commands
    #[must_use]
    pub fn len(&self) -> usize {
        self.commands.len()
    }

    /// Check if empty
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }
}

impl Default for CommandRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Edit distance between two strings, counted in characters.
///
/// Optimal string alignment distance: Levenshtein plus adjacent-character
/// transpositions at cost 1, since swapped neighbors ("quti") are the most
/// common typo on a keyboard. Used for typo rescue of slash commands; inputs
/// are command names, so strings stay short and the O(n*m) cost is negligible.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());

    let mut d = vec![vec![usize::MAX; m + 1]; n + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }

    for i in 1..=n {
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            d[i][j] = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
            }
        }
    }

    d[n][m]
}

/// Parse a `/loop` interval: `30s`, `5m`, `1h`, or a bare number of minutes.
/// Returns seconds; minimum 10s to avoid tight re-fire loops.
fn parse_loop_interval(text: &str) -> Option<u64> {
    let (digits, unit_secs) = match text.chars().last() {
        Some('s') => (&text[..text.len() - 1], 1),
        Some('m') => (&text[..text.len() - 1], 60),
        Some('h') => (&text[..text.len() - 1], 3600),
        _ => (text, 60),
    };
    let value: u64 = digits.parse().ok()?;
    if value == 0 {
        return None;
    }
    Some((value * unit_secs).max(10))
}

/// Parse argument string into typed values
///
/// Converts a raw argument string (everything after the command name) into a `HashMap`
/// of typed argument values based on the command's argument definitions.
///
/// # Parsing Strategy
///
/// 1. **Apply Defaults**: Start with default values for all arguments that have them
/// 2. **Positional Parsing**: Match space-separated tokens to argument definitions in order
/// 3. **Type Conversion**: Convert string tokens to the appropriate type (String, Bool, Int, Choice)
/// 4. **Validation**: Ensure required arguments are present and choices are valid
///
/// # Type Conversion Rules
///
/// - **String**: No conversion, stored as-is
/// - **Bool**: "true", "yes", "on", "1" (case-insensitive) -> true; everything else -> false
/// - **Int**: Parsed as i64; returns error if parsing fails
/// - **Choice**: Must match one of the allowed values; returns error if not
/// - **FilePath/SessionId**: Stored as strings (type hints for UI completion)
///
/// # Example
///
/// ```rust,ignore
/// // Command definition
/// let args = vec![
///     CommandArgument::string("name", "Name").required(),
///     CommandArgument::choice("mode", "Mode", vec!["fast", "slow"]),
/// ];
///
/// // Parse "/cmd Alice fast"
/// let parsed = parse_arguments("Alice fast", &args)?;
/// assert_eq!(parsed.get("name"), Some(&ArgumentValue::String("Alice".into())));
/// assert_eq!(parsed.get("mode"), Some(&ArgumentValue::String("fast".into())));
/// ```
fn parse_arguments(
    raw: &str,
    definitions: &[CommandArgument],
) -> Result<HashMap<String, ArgumentValue>, CommandError> {
    let mut result = HashMap::new();
    let parts: Vec<&str> = raw.split_whitespace().collect();

    // Apply defaults first
    for def in definitions {
        if let Some(ref default) = def.default {
            result.insert(def.name.clone(), default.clone());
        }
    }

    // Simple positional argument parsing
    for (i, def) in definitions.iter().enumerate() {
        if let Some(value) = parts.get(i) {
            let parsed = match &def.arg_type {
                super::types::CommandArgumentType::String => {
                    ArgumentValue::String((*value).to_string())
                }
                super::types::CommandArgumentType::Bool => {
                    let b = matches!(value.to_lowercase().as_str(), "true" | "yes" | "on" | "1");
                    ArgumentValue::Bool(b)
                }
                super::types::CommandArgumentType::Int => {
                    let i = value.parse::<i64>().map_err(|_| {
                        CommandError::new(maestro_ui::localization::format(
                            "Expected integer for '{0}'",
                            std::slice::from_ref(&(def.name)),
                        ))
                    })?;
                    ArgumentValue::Int(i)
                }
                super::types::CommandArgumentType::Choice(choices) => {
                    if !choices.contains(&(*value).to_string()) {
                        return Err(CommandError::new(maestro_ui::localization::format(
                            "Invalid value '{0}' for '{1}'. Expected one of: {2}",
                            &[
                                (value).to_string(),
                                (def.name).clone(),
                                (choices.join(", ")).clone(),
                            ],
                        )));
                    }
                    ArgumentValue::String((*value).to_string())
                }
                super::types::CommandArgumentType::FilePath
                | super::types::CommandArgumentType::SessionId => {
                    ArgumentValue::String((*value).to_string())
                }
            };
            result.insert(def.name.clone(), parsed);
        } else if def.required {
            return Err(CommandError::new(maestro_ui::localization::format(
                "Missing required argument: {0}",
                std::slice::from_ref(&(def.name)),
            )));
        }
    }

    Ok(result)
}

fn tokenize_command_args(raw: &str) -> Vec<String> {
    shlex::split(raw).unwrap_or_else(|| raw.split_whitespace().map(str::to_string).collect())
}

fn parse_orb_action(raw: &str) -> Result<OrbAction, CommandError> {
    let tokens = tokenize_command_args(raw);
    let subcommand = tokens
        .first()
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_else(|| "list".to_string());
    match subcommand.as_str() {
        "list" | "ls" => {
            if tokens.len() > 1 {
                return Err(CommandError::new(maestro_ui::localization::tr(
                    "Usage: /computer list",
                )));
            }
            Ok(OrbAction::List)
        }
        "status" => parse_orb_id(
            &tokens,
            OrbAction::Status,
            maestro_ui::localization::tr("/computer status <task-id>"),
        ),
        "pause" => parse_orb_id(
            &tokens,
            OrbAction::Pause,
            maestro_ui::localization::tr("/computer pause <task-id>"),
        ),
        "resume" => parse_orb_id(
            &tokens,
            OrbAction::Resume,
            maestro_ui::localization::tr("/computer resume <task-id>"),
        ),
        "cancel" => parse_orb_id(
            &tokens,
            OrbAction::Cancel,
            maestro_ui::localization::tr("/computer cancel <task-id>"),
        ),
        "collect" => parse_orb_id(
            &tokens,
            OrbAction::Collect,
            maestro_ui::localization::tr("/computer collect <task-id>"),
        ),
        "followup" | "follow-up" => {
            if tokens.len() < 3 {
                return Err(CommandError::new(maestro_ui::localization::tr(
                    "Usage: /computer followup <task-id> <prompt>",
                )));
            }
            Ok(OrbAction::Followup {
                id: tokens[1].clone(),
                prompt: tokens[2..].join(" "),
            })
        }
        "handoff" => parse_orb_handoff_action(&tokens[1..]),
        "help" | "?" => Err(CommandError::new(maestro_ui::localization::tr(
            "Usage: /computer [list|status <task-id>|followup <task-id> <prompt>|pause <task-id>|resume <task-id>|cancel <task-id>|collect <task-id>|handoff create|list|read ...]",
        ))),
        other => Err(CommandError::new(maestro_ui::localization::format(
            "Unknown Computer subcommand: {0}",
            &[(other).to_string()],
        ))),
    }
}

fn parse_orb_id<T>(
    tokens: &[String],
    constructor: fn(String) -> T,
    usage: &str,
) -> Result<T, CommandError> {
    if tokens.len() != 2 || tokens[1].trim().is_empty() {
        return Err(CommandError::new(maestro_ui::localization::format(
            "Usage: {0}",
            &[(usage).to_string()],
        )));
    }
    Ok(constructor(tokens[1].clone()))
}

fn parse_orb_handoff_action(tokens: &[String]) -> Result<OrbAction, CommandError> {
    let Some(operation) = tokens.first().map(|value| value.to_ascii_lowercase()) else {
        return Err(CommandError::new(maestro_ui::localization::tr(
            "Usage: /computer handoff create|list|read ...",
        )));
    };
    match operation.as_str() {
        "list" | "ls" => {
            if tokens.len() != 2 || tokens[1].trim().is_empty() {
                return Err(CommandError::new(maestro_ui::localization::tr(
                    "Usage: /computer handoff list <target-thread-id>",
                )));
            }
            Ok(OrbAction::HandoffList {
                target_thread_id: tokens[1].clone(),
            })
        }
        "read" => {
            if tokens.len() != 3 || tokens[1].trim().is_empty() || tokens[2].trim().is_empty() {
                return Err(CommandError::new(maestro_ui::localization::tr(
                    "Usage: /computer handoff read <target-thread-id> <package-id>",
                )));
            }
            Ok(OrbAction::HandoffRead {
                target_thread_id: tokens[1].clone(),
                package_id: tokens[2].clone(),
            })
        }
        "create" | "capture" => parse_orb_handoff_create(&tokens[1..]),
        other => Err(CommandError::new(maestro_ui::localization::format(
            "Unknown handoff subcommand: {0}",
            &[(other).to_string()],
        ))),
    }
}

fn parse_orb_handoff_create(tokens: &[String]) -> Result<OrbAction, CommandError> {
    let usage = maestro_ui::localization::tr(
        "Usage: /computer handoff create <source-task-id> <target-thread-id> [--file path] [--artifact id] [--include-diff]",
    );
    if tokens.len() < 2 || tokens[0].trim().is_empty() || tokens[1].trim().is_empty() {
        return Err(CommandError::new(usage));
    }
    let source_id = tokens[0].clone();
    let target_thread_id = tokens[1].clone();
    let mut files = Vec::new();
    let mut artifact_ids = Vec::new();
    let mut include_diff = false;
    let mut index = 2;
    while index < tokens.len() {
        let token = &tokens[index];
        if token == "--include-diff" {
            include_diff = true;
        } else if token == "--file" || token == "--artifact" {
            let Some(value) = tokens
                .get(index + 1)
                .filter(|value| !value.trim().is_empty())
            else {
                return Err(CommandError::new(maestro_ui::localization::format(
                    "{0} requires a value",
                    std::slice::from_ref(token),
                )));
            };
            if token == "--file" {
                files.push(value.clone());
            } else {
                artifact_ids.push(value.clone());
            }
            index += 1;
        } else if let Some(value) = token.strip_prefix("--file=") {
            if value.trim().is_empty() {
                return Err(CommandError::new(maestro_ui::localization::tr(
                    "--file requires a value",
                )));
            }
            files.push(value.to_string());
        } else if let Some(value) = token.strip_prefix("--artifact=") {
            if value.trim().is_empty() {
                return Err(CommandError::new(maestro_ui::localization::tr(
                    "--artifact requires a value",
                )));
            }
            artifact_ids.push(value.to_string());
        } else {
            return Err(CommandError::new(maestro_ui::localization::format(
                "Unknown handoff create argument '{0}'",
                std::slice::from_ref(token),
            )));
        }
        index += 1;
    }
    if files.is_empty() && artifact_ids.is_empty() && !include_diff {
        return Err(CommandError::new(maestro_ui::localization::tr(
            "handoff create requires --file, --artifact, or --include-diff",
        )));
    }
    Ok(OrbAction::HandoffCreate {
        source_id,
        target_thread_id,
        files,
        artifact_ids,
        include_diff,
    })
}

fn parse_mcp_prompts_action(raw: &str) -> Result<McpAction, CommandError> {
    let tokens = tokenize_command_args(raw);
    let server = tokens.get(1).cloned();
    let name = tokens.get(2).cloned();

    let mut arguments = HashMap::new();
    if server.is_none() || name.is_none() {
        return Ok(McpAction::Prompts {
            server,
            name,
            arguments,
        });
    }

    for token in tokens.iter().skip(3) {
        let Some((key, value)) = token.split_once('=') else {
            return Err(CommandError::new(maestro_ui::localization::tr(
                "Invalid MCP prompt argument. Use KEY=value after the prompt name.",
            )));
        };
        if key.trim().is_empty() {
            return Err(CommandError::new(maestro_ui::localization::tr(
                "Invalid MCP prompt argument. Use KEY=value after the prompt name.",
            )));
        }
        arguments.insert(key.trim().to_string(), value.to_string());
    }

    Ok(McpAction::Prompts {
        server,
        name,
        arguments,
    })
}

fn parse_a2a_action(raw: &str) -> Result<A2aAction, CommandError> {
    let tokens = tokenize_command_args(raw);
    let subcommand = tokens
        .first()
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_default();
    match subcommand.as_str() {
        "" | "help" => Ok(A2aAction::Help),
        "fleet" => Ok(A2aAction::Fleet),
        "peers" | "list" => Ok(A2aAction::Peers),
        "tasks" => Ok(A2aAction::Tasks {
            peer: first_a2a_positional(&tokens, 1),
            include_work_graph: has_a2a_flag(&tokens, "--work-graph"),
        }),
        "coordinate" => {
            let reply_index = tokens.iter().position(|value| value == "--reply");
            let peer = first_a2a_positional(
                reply_index
                    .map(|index| &tokens[..index])
                    .unwrap_or_else(|| tokens.as_slice()),
                1,
            );
            let include_work_graph = has_a2a_flag(&tokens, "--work-graph");
            let reply = reply_index.map(|index| {
                tokens
                    .get(index + 1..)
                    .unwrap_or(&[])
                    .iter()
                    .take_while(|value| !value.starts_with("--"))
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" ")
            });
            let reply = reply.and_then(|value| {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            });
            Ok(A2aAction::Coordinate {
                peer,
                reply,
                include_work_graph,
            })
        }
        "accept" => {
            let code = tokens
                .get(1)
                .ok_or_else(|| CommandError::new(maestro_ui::localization::tr("Usage: /a2a accept <pairing-code>")))?;
            Ok(A2aAction::Accept { code: code.clone() })
        }
        "register" | "publish" => Ok(A2aAction::Register {
            agent_id: a2a_flag_value(&tokens, "--agent-id"),
            public_url: a2a_flag_value(&tokens, "--url")
                .or_else(|| a2a_flag_value(&tokens, "--public-url")),
            heartbeat_only: has_a2a_flag(&tokens, "--heartbeat-only"),
        }),
        "delegate" => {
            let peer = tokens
                .get(1)
                .ok_or_else(|| CommandError::new(maestro_ui::localization::tr("Usage: /a2a delegate <peer> <text>")))?;
            let text = tokens.get(2..).unwrap_or(&[]).join(" ");
            if text.trim().is_empty() {
                return Err(CommandError::new(maestro_ui::localization::tr("Usage: /a2a delegate <peer> <text>")));
            }
            Ok(A2aAction::Delegate {
                peer: peer.clone(),
                text,
            })
        }
        "reply" | "continue" => {
            let peer = tokens
                .get(1)
                .ok_or_else(|| CommandError::new(maestro_ui::localization::tr("Usage: /a2a reply <peer> <task-id> <text>")))?;
            let task_id = tokens
                .get(2)
                .ok_or_else(|| CommandError::new(maestro_ui::localization::tr("Usage: /a2a reply <peer> <task-id> <text>")))?;
            let text = tokens.get(3..).unwrap_or(&[]).join(" ");
            if text.trim().is_empty() {
                return Err(CommandError::new(maestro_ui::localization::tr("Usage: /a2a reply <peer> <task-id> <text>")));
            }
            Ok(A2aAction::Reply {
                peer: peer.clone(),
                task_id: task_id.clone(),
                text,
            })
        }
        "send" => {
            let peer = tokens
                .get(1)
                .ok_or_else(|| CommandError::new(maestro_ui::localization::tr("Usage: /a2a send <peer> <text>")))?;
            let text = tokens.get(2..).unwrap_or(&[]).join(" ");
            if text.trim().is_empty() {
                return Err(CommandError::new(maestro_ui::localization::tr("Usage: /a2a send <peer> <text>")));
            }
            Ok(A2aAction::Send {
                peer: peer.clone(),
                text,
            })
        }
        _ => Err(CommandError::new(maestro_ui::localization::format("Unknown A2A subcommand: {0}", std::slice::from_ref(&(subcommand)))).with_hint(
            maestro_ui::localization::tr("Usage: /a2a [fleet|peers|tasks [--work-graph]|coordinate [--work-graph]|accept <code>|register --url <base-url>|delegate <peer> <text>|reply <peer> <task-id> <text>|send <peer> <text>]"),
        )),
    }
}

fn parse_handoff_action(raw: &str) -> Result<A2aAction, CommandError> {
    const USAGE: &str = "Usage: /handoff <prompt> | /handoff [--peer <name>] [Computer package options] -- <prompt>";

    let tokens = tokenize_command_args(raw);
    let mut peer = None;
    let mut source_task_id = None;
    let mut target_thread_id = None;
    let mut files = Vec::new();
    let mut artifact_ids = Vec::new();
    let mut include_diff = false;
    let mut prompt_start = None;
    let mut index = 0;
    while index < tokens.len() {
        let token = &tokens[index];
        if token == "--" {
            prompt_start = Some(index + 1);
            break;
        }
        if !token.starts_with("--") {
            prompt_start = Some(index);
            break;
        }
        if token == "--include-diff" {
            include_diff = true;
            index += 1;
            continue;
        }

        let (flag, inline_value) = token
            .split_once('=')
            .map_or((token.as_str(), None), |(flag, value)| (flag, Some(value)));
        if !matches!(
            flag,
            "--peer" | "--source-task" | "--target-thread" | "--file" | "--artifact"
        ) {
            return Err(CommandError::new(maestro_ui::localization::format(
                "Unknown /handoff argument '{0}'",
                std::slice::from_ref(token),
            ))
            .with_hint(USAGE));
        }
        let value = match inline_value {
            Some(value) if !value.trim().is_empty() => value.to_string(),
            Some(_) => {
                return Err(CommandError::new(maestro_ui::localization::format(
                    "{0} requires a value",
                    &[(flag).to_string()],
                )));
            }
            None => {
                let Some(value) = tokens
                    .get(index + 1)
                    .filter(|value| !value.trim().is_empty() && !value.starts_with("--"))
                else {
                    return Err(CommandError::new(maestro_ui::localization::format(
                        "{0} requires a value",
                        &[(flag).to_string()],
                    )));
                };
                index += 1;
                value.clone()
            }
        };
        match flag {
            "--peer" => peer = Some(value),
            "--source-task" => source_task_id = Some(value),
            "--target-thread" => target_thread_id = Some(value),
            "--file" => files.push(value),
            "--artifact" => artifact_ids.push(value),
            _ => unreachable!("validated handoff flag"),
        }
        index += 1;
    }

    let text = prompt_start
        .and_then(|start| tokens.get(start..))
        .unwrap_or(&[])
        .join(" ");
    if text.trim().is_empty() {
        return Err(CommandError::new(USAGE));
    }

    let has_package_argument = source_task_id.is_some()
        || target_thread_id.is_some()
        || !files.is_empty()
        || !artifact_ids.is_empty()
        || include_diff;
    let computer_package = if has_package_argument {
        let Some(source_task_id) = source_task_id else {
            return Err(CommandError::new(maestro_ui::localization::tr(
                "Computer package handoff requires --source-task",
            ))
            .with_hint(USAGE));
        };
        let Some(target_thread_id) = target_thread_id else {
            return Err(CommandError::new(maestro_ui::localization::tr(
                "Computer package handoff requires --target-thread",
            ))
            .with_hint(USAGE));
        };
        if files.is_empty() && artifact_ids.is_empty() && !include_diff {
            return Err(CommandError::new(maestro_ui::localization::tr(
                "Computer package handoff requires --file, --artifact, or --include-diff",
            ))
            .with_hint(USAGE));
        }
        Some(A2aComputerHandoffSelection {
            source_task_id,
            target_thread_id,
            files,
            artifact_ids,
            include_diff,
        })
    } else {
        None
    };

    Ok(A2aAction::Handoff {
        peer,
        text,
        computer_package,
    })
}

fn has_a2a_flag(tokens: &[String], flag: &str) -> bool {
    tokens.iter().any(|value| value == flag)
}

fn first_a2a_positional(tokens: &[String], start: usize) -> Option<String> {
    let mut index = start;
    while index < tokens.len() {
        let token = &tokens[index];
        if token.starts_with("--") {
            if a2a_value_flag(token) && !token.contains('=') {
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        return Some(token.clone());
    }
    None
}

fn a2a_flag_value(tokens: &[String], flag: &str) -> Option<String> {
    for (index, token) in tokens.iter().enumerate() {
        if let Some((token_flag, value)) = token.split_once('=') {
            if token_flag == flag {
                let value = value.trim();
                return (!value.is_empty()).then(|| value.to_string());
            }
        }
        if token == flag {
            let value = tokens.get(index + 1)?.trim();
            return (!value.is_empty() && !value.starts_with("--")).then(|| value.to_string());
        }
    }
    None
}

fn a2a_value_flag(token: &str) -> bool {
    matches!(
        token,
        "--registry"
            | "--tasks"
            | "--timeout-ms"
            | "--interval-ms"
            | "--max-wait-ms"
            | "--role"
            | "--cwd"
            | "--agent-card-url"
            | "--agent-id"
            | "--capabilities"
            | "--description"
            | "--internal-url"
            | "--name"
            | "--owner-id"
            | "--protocol-version"
            | "--public-url"
            | "--security-schemes"
            | "--status"
            | "--surface"
            | "--surface-types"
            | "--type"
            | "--url"
            | "--workspace-id"
    )
}

fn parse_rewind_args(raw: &str, usage: &str) -> Result<SessionAction, CommandError> {
    let tokens: Vec<&str> = raw.split_whitespace().collect();
    if let Some(&first) = tokens.first() {
        if first == "files" || first == "checkpoints" {
            if tokens.len() > 1 {
                return Err(CommandError::new(usage));
            }
            return Ok(if first == "files" {
                SessionAction::RewindFiles
            } else {
                SessionAction::ListCheckpoints
            });
        }
    }
    let mut turns = None;
    let mut dry_run = false;
    let mut both = false;
    for arg in tokens {
        match arg {
            "--dry-run" => dry_run = true,
            "--files" => both = true,
            _ if turns.is_none() => {
                turns = Some(arg.parse::<usize>().map_err(|_| CommandError::new(usage))?);
            }
            _ => return Err(CommandError::new(usage)),
        }
    }
    let turns = turns.unwrap_or(1);
    if turns == 0 {
        return Err(CommandError::new(maestro_ui::localization::tr(
            "Rewind count must be >= 1",
        )));
    }
    Ok(if both {
        SessionAction::RewindBoth { turns, dry_run }
    } else {
        SessionAction::Rewind { turns, dry_run }
    })
}

fn parse_plan_range(raw: &str) -> Result<(usize, usize), CommandError> {
    let (start, end) = raw
        .split_once('-')
        .or_else(|| raw.split_once(':'))
        .unwrap_or((raw, raw));
    let start = start.parse::<usize>().map_err(|_| {
        CommandError::new(maestro_ui::localization::tr(
            "Plan comment range must be LINE or START-END",
        ))
    })?;
    let end = end.parse::<usize>().map_err(|_| {
        CommandError::new(maestro_ui::localization::tr(
            "Plan comment range must be LINE or START-END",
        ))
    })?;
    if start == 0 || end < start {
        return Err(CommandError::new(maestro_ui::localization::tr(
            "Plan comment range must be positive and ordered",
        )));
    }
    Ok((start, end))
}

/// Build the default command registry with all built-in commands
///
/// Constructs and returns a fully populated `CommandRegistry` containing all
/// standard slash commands for the TUI application.
///
/// # Command Categories
///
/// The registry includes commands across multiple categories:
///
/// - **Navigation**: help, quit, refresh
/// - **UI**: clear, theme, zen, copy, footer
/// - **Session**: session, sessions, continue, resume
/// - **Config**: model, thinking, approvals
/// - **Context**: compact, context, memory, plan
/// - **Tools**: tools, mcp
/// - **Diagnostics**: status, diag, version
/// - **Safety**: approvals
///
/// # Function Pointers and Closures
///
/// Each command handler is a boxed closure with the signature:
/// ```rust,ignore
/// Box<dyn Fn(&CommandContext) -> CommandResult + Send + Sync>
/// ```
///
/// This allows:
/// - **Fn trait**: Handler can be called multiple times without consuming itself
/// - **Send + Sync**: Handler can be safely shared across threads
/// - **Box**: Dynamic dispatch - handlers can have different implementations
/// - **Closure**: Handlers can capture environment if needed (though most don't)
///
/// # Example
///
/// ```rust,ignore
/// use maestro_tui::commands::build_command_registry;
///
/// let registry = build_command_registry();
///
/// // Registry includes all standard commands
/// assert!(registry.get("help").is_some());
/// assert!(registry.get("quit").is_some());
/// assert!(registry.get("theme").is_some());
///
/// // Aliases are also registered
/// assert!(registry.get("h").is_some());  // alias for help
/// assert!(registry.get("q").is_some());  // alias for quit
/// ```
#[must_use]
pub fn build_command_registry() -> CommandRegistry {
    crate::localization::with_locale(crate::localization::Locale::English, build_builtin_registry)
}

fn build_builtin_registry() -> CommandRegistry {
    let mut registry = CommandRegistry::new();

    registry.register(Command::new("settings", maestro_ui::localization::tr("Account, permissions, connections, and appearance"), CommandCategory::Config,
        Box::new(|ctx| {
            let panel = match ctx.raw_args.trim() {
                "" => ControlPanel::Settings,
                "account" => ControlPanel::Account,
                "permissions" => ControlPanel::Permissions,
                "appearance" => ControlPanel::Appearance,
                "output" => ControlPanel::Output,
                "footer" => ControlPanel::Footer,
                "capabilities" => ControlPanel::Capabilities,
                "advanced" => ControlPanel::Advanced,
                "session" => ControlPanel::Session,
                _ => return Err(CommandError::new(maestro_ui::localization::tr("Usage: /settings [account|permissions|appearance|output|footer|capabilities|advanced|session]"))),
            };
            Ok(CommandOutput::Action(CommandAction::OpenPanel(panel)))
        })).localized().primary(8));
    registry.register(
        Command::new(
            "tasks",
            maestro_ui::localization::tr("Work, workers, queues, decisions, and schedules"),
            CommandCategory::Tools,
            Box::new(|_| {
                Ok(CommandOutput::Action(CommandAction::OpenPanel(
                    ControlPanel::Tasks,
                )))
            }),
        )
        .localized()
        .primary(7),
    );
    registry.register(
        Command::new(
            "output",
            maestro_ui::localization::tr("Choose summary, compact, or expanded tool output"),
            CommandCategory::Ui,
            Box::new(|ctx| {
                if ctx.raw_args.trim().is_empty() {
                    return Ok(CommandOutput::Action(CommandAction::OpenPanel(
                        ControlPanel::Output,
                    )));
                }
                let detail =
                    crate::state::OutputDetail::parse(ctx.raw_args.trim()).ok_or_else(|| {
                        CommandError::new(maestro_ui::localization::tr(
                            "Usage: /output [summary|compact|expanded]",
                        ))
                    })?;
                Ok(CommandOutput::Action(CommandAction::SetOutputDetail(
                    detail,
                )))
            }),
        )
        .localized()
        .usage("/output [summary|compact|expanded]"),
    );

    // Help command
    registry.register(
        Command::new(
            "help",
            maestro_ui::localization::tr("Show available commands"),
            CommandCategory::Navigation,
            Box::new(|_| {
                // `/help [command]` is handled in `CommandRegistry::execute` so
                // the handler can look up sibling commands.
                Ok(CommandOutput::Action(CommandAction::OpenPanel(
                    ControlPanel::Help,
                )))
            }),
        )
        .localized()
        .alias("h")
        .alias("?")
        .arg(CommandArgument::string(
            "command",
            maestro_ui::localization::tr("Command to get help for"),
        ))
        .usage("/help [command]")
        .primary(10),
    );

    // Hotkeys command
    registry.register(
        Command::new(
            "hotkeys",
            maestro_ui::localization::tr("Show or manage keyboard shortcuts"),
            CommandCategory::Config,
            Box::new(|ctx| {
                let args = ctx.raw_args.trim();
                let parts: Vec<&str> = args.split_whitespace().collect();
                let subcommand = parts
                    .first()
                    .map(|value| value.to_ascii_lowercase())
                    .unwrap_or_default();

                match subcommand.as_str() {
                    "" | "show" | "list" | "help" => {
                        Ok(CommandOutput::OpenModal(ModalType::ShortcutsHelp))
                    }
                    "path" | "where" | "file" => {
                        let path = keybindings_config_path();
                        Ok(CommandOutput::Message(maestro_ui::localization::format("Keyboard shortcuts config:\n  Path: {0}\n  Status: {1}", &[(path.display()).to_string(), (if path.exists() { "present" } else { "missing" }).to_string()])))
                    }
                    "init" | "create" | "setup" => {
                        let force = parts.iter().skip(1).any(|arg| *arg == "--force");
                        match initialize_keybindings_file(force) {
                            Ok(result) if result.created => Ok(CommandOutput::Message(maestro_ui::localization::format("Created keyboard shortcuts config at {0}\nRun /hotkeys validate to verify the file after editing.", &[(result.path.display()).to_string()]))),
                            Ok(result) => Err(
                                CommandError::new(maestro_ui::localization::format("Keybindings config already exists at {0}.", &[(result.path.display()).to_string()]))
                                .with_hint(
                                    maestro_ui::localization::tr("Re-run with /hotkeys init --force to overwrite it."),
                                ),
                            ),
                            Err(err) => Err(CommandError::new(maestro_ui::localization::format("Failed to create keybindings config: {0}", std::slice::from_ref(&(err))))),
                        }
                    }
                    "validate" | "check" | "doctor" | "status" => {
                        Ok(CommandOutput::Message(format_keybindings_config_report()))
                    }
                    _ => Err(
                        CommandError::new(maestro_ui::localization::format("Unknown hotkeys subcommand: {0}", std::slice::from_ref(&(subcommand))))
                        .with_hint(maestro_ui::localization::tr("Usage: /hotkeys [show|path|init|validate]")),
                    ),
                }
            }),
        ).localized()
        .alias("keys")
        .alias("shortcuts")
        .usage("/hotkeys [show|path|init|validate]"),
    );

    // Clear / new session (Grok-style: /new and /clear start fresh)
    registry.register(
        Command::new(
            "new",
            maestro_ui::localization::tr("Start a new conversation"),
            CommandCategory::Session,
            Box::new(|_| {
                Ok(CommandOutput::Action(CommandAction::Session(
                    SessionAction::New,
                )))
            }),
        )
        .localized()
        .alias("cls")
        .alias("clear")
        .primary(0),
    );

    // Fork session
    registry.register(
        Command::new(
            "fork",
            maestro_ui::localization::tr("Fork the conversation into a new session branch"),
            CommandCategory::Session,
            Box::new(|_| {
                Ok(CommandOutput::Action(CommandAction::Session(
                    SessionAction::Fork,
                )))
            }),
        )
        .localized()
        .usage("/fork")
        .primary(2),
    );

    // Rewind turns
    registry.register(
        Command::new(
            "rewind",
            maestro_ui::localization::tr("Remove the last N user turns, or restore files from a checkpoint"),
            CommandCategory::Session,
            Box::new(|ctx| {
                Ok(CommandOutput::Action(CommandAction::Session(
                    parse_rewind_args(
                        &ctx.raw_args,
                        maestro_ui::localization::tr("Usage: /rewind [n] [--dry-run] [--files] | /rewind files | /rewind checkpoints"),
                    )?,
                )))
            }),
        ).localized()
        .alias("undo")
        .usage("/rewind [n] [--dry-run] [--files] | /rewind files | /rewind checkpoints").primary(3)
    );

    registry.register(
        Command::new(
            "btw",
            maestro_ui::localization::tr("Ask a tool-free side question outside main history"),
            CommandCategory::Context,
            Box::new(|ctx| {
                let question = ctx.raw_args.trim();
                if question.is_empty() {
                    return Err(CommandError::new(maestro_ui::localization::tr(
                        "Usage: /btw <question>",
                    )));
                }
                Ok(CommandOutput::Action(CommandAction::SideQuestion(
                    question.to_string(),
                )))
            }),
        )
        .localized()
        .usage("/btw <question>"),
    );

    registry.register(
        Command::new(
            "workflow",
            maestro_ui::localization::tr("Run and control durable budgeted workflows"),
            CommandCategory::Diagnostics,
            Box::new(|ctx| {
                use crate::workflow_runtime::{WorkflowRun, WorkflowSpec, WorkflowStore};

                let store = WorkflowStore::for_workspace(std::path::Path::new(&ctx.cwd));
                let mut parts = ctx.raw_args.split_whitespace();
                let action = parts.next().unwrap_or("list");
                let render = |runs: Vec<WorkflowRun>| {
                    if runs.is_empty() {
                        return maestro_ui::localization::tr("No workflow runs.").to_string();
                    }
                    runs.into_iter()
                        .map(|run| {
                            format!(
                                "{}  {:?}  {}@{}  agents {}/{}  tokens {}/{}",
                                &run.id[..8.min(run.id.len())],
                                run.status,
                                run.spec.name,
                                run.spec.version,
                                run.agents_started,
                                run.spec.max_agents,
                                run.tokens_used,
                                run.spec.token_budget,
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                match action {
                    "list" | "status" => store
                        .list()
                        .map(render)
                        .map(CommandOutput::Message)
                        .map_err(CommandError::new),
                    "run" | "start" => {
                        let path = parts.next().ok_or_else(|| {
                            CommandError::new(maestro_ui::localization::tr(
                                "Usage: /workflow run <spec.json>",
                            ))
                        })?;
                        let bytes = std::fs::read(path).map_err(|error| {
                            CommandError::new(maestro_ui::localization::format(
                                "Failed to read workflow spec: {0}",
                                &[(error).to_string()],
                            ))
                        })?;
                        let spec: WorkflowSpec =
                            serde_json::from_slice(&bytes).map_err(|error| {
                                CommandError::new(maestro_ui::localization::format(
                                    "Invalid workflow spec: {0}",
                                    &[(error).to_string()],
                                ))
                            })?;
                        let run = WorkflowRun::start(spec, serde_json::json!({}))
                            .map_err(CommandError::new)?;
                        store.append(&run).map_err(CommandError::new)?;
                        Ok(CommandOutput::Message(maestro_ui::localization::format(
                            "Started workflow {0} ({1})",
                            &[(run.spec.name).clone(), (run.id).clone()],
                        )))
                    }
                    "pause" | "resume" | "stop" => {
                        let id = parts.next().ok_or_else(|| {
                            CommandError::new(maestro_ui::localization::format(
                                "Usage: /workflow {0} <run-id>",
                                &[(action).to_string()],
                            ))
                        })?;
                        let mut run = store.get(id).map_err(CommandError::new)?;
                        match action {
                            "pause" => run.pause(),
                            "resume" => {
                                let sha = run.spec_sha.clone();
                                let args = run.args.clone();
                                run.resume(&sha, &args)
                            }
                            "stop" => run.stop(Some(
                                maestro_ui::localization::tr("stopped from TUI").to_string(),
                            )),
                            _ => unreachable!(),
                        }
                        .map_err(CommandError::new)?;
                        store.append(&run).map_err(CommandError::new)?;
                        Ok(CommandOutput::Message(maestro_ui::localization::format(
                            "Workflow {0} is {1}",
                            &[(run.id).clone(), format!("{:?}", run.status)],
                        )))
                    }
                    _ => Err(CommandError::new(maestro_ui::localization::tr(
                        "Usage: /workflow [list|run <spec.json>|pause <id>|resume <id>|stop <id>]",
                    ))),
                }
            }),
        )
        .localized()
        .alias("workflows")
        .usage("/workflow [list|run <spec.json>|pause <id>|resume <id>|stop <id>]"),
    );

    registry.register(
        Command::new(
            "decision",
            maestro_ui::localization::tr("List, answer, or cancel background decisions"),
            CommandCategory::Context,
            Box::new(|ctx| {
                use crate::pending_decisions::PendingDecisionStore;

                let store = PendingDecisionStore::default_store();
                let mut fields = ctx.raw_args.trim().splitn(3, char::is_whitespace);
                let action = fields
                    .next()
                    .filter(|value| !value.is_empty())
                    .unwrap_or("list");
                match action {
                    "list" | "status" => {
                        let decisions = store.list().map_err(CommandError::new)?;
                        if decisions.is_empty() {
                            return Ok(CommandOutput::Message(
                                maestro_ui::localization::tr("No background decisions.")
                                    .to_string(),
                            ));
                        }
                        Ok(CommandOutput::Message(
                            decisions
                                .into_iter()
                                .map(|decision| {
                                    format!(
                                        "{}  {:?}{}",
                                        &decision.id[..8.min(decision.id.len())],
                                        decision.effective_status(),
                                        decision
                                            .answer
                                            .as_deref()
                                            .map(|answer| format!("  {answer}"))
                                            .unwrap_or_default(),
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join("\n"),
                        ))
                    }
                    "answer" => {
                        let id = fields.next().ok_or_else(|| {
                            CommandError::new(maestro_ui::localization::tr(
                                "Usage: /decision answer <id> <answer>",
                            ))
                        })?;
                        let answer = fields.next().ok_or_else(|| {
                            CommandError::new(maestro_ui::localization::tr(
                                "Usage: /decision answer <id> <answer>",
                            ))
                        })?;
                        let mut decision = store.get(id).map_err(CommandError::new)?;
                        decision
                            .answer(answer.to_string())
                            .map_err(CommandError::new)?;
                        store.append(&decision).map_err(CommandError::new)?;
                        Ok(CommandOutput::Action(CommandAction::Steer(
                            maestro_ui::localization::format(
                                "Background decision {0} was answered: {1}",
                                &[(decision.id).clone(), (answer).to_string()],
                            ),
                        )))
                    }
                    "cancel" => {
                        let id = fields.next().ok_or_else(|| {
                            CommandError::new(maestro_ui::localization::tr(
                                "Usage: /decision cancel <id>",
                            ))
                        })?;
                        let mut decision = store.get(id).map_err(CommandError::new)?;
                        decision.cancel().map_err(CommandError::new)?;
                        store.append(&decision).map_err(CommandError::new)?;
                        Ok(CommandOutput::Message(maestro_ui::localization::format(
                            "Cancelled background decision {0}",
                            &[(decision.id).clone()],
                        )))
                    }
                    _ => Err(CommandError::new(maestro_ui::localization::tr(
                        "Usage: /decision [list|answer <id> <answer>|cancel <id>]",
                    ))),
                }
            }),
        )
        .localized()
        .alias("decisions")
        .usage("/decision [list|answer <id> <answer>|cancel <id>]"),
    );

    // Quit command
    registry.register(
        Command::new(
            "quit",
            maestro_ui::localization::tr("Quit the application"),
            CommandCategory::Navigation,
            Box::new(|_| Ok(CommandOutput::Action(CommandAction::Quit))),
        )
        .localized()
        .alias("exit")
        .alias("q")
        .primary(11),
    );

    // Zen mode command
    registry.register(
        Command::new(
            "zen",
            maestro_ui::localization::tr("Toggle zen mode (minimal UI)"),
            CommandCategory::Ui,
            Box::new(|_| Ok(CommandOutput::Action(CommandAction::ToggleZenMode))),
        )
        .localized(),
    );

    // Tool output compact mode
    registry.register(
        Command::new(
            "compact-tools",
            maestro_ui::localization::tr("Toggle tool output folding"),
            CommandCategory::Ui,
            Box::new(|ctx| {
                let arg = ctx.raw_args.trim().to_lowercase();
                let mode = if arg.is_empty() || arg == "toggle" {
                    None
                } else if arg == "on" || arg == "true" {
                    Some(true)
                } else if arg == "off" || arg == "false" {
                    Some(false)
                } else {
                    return Err(CommandError::new(maestro_ui::localization::tr(
                        "Usage: /compact-tools [on|off|toggle]",
                    )));
                };
                Ok(CommandOutput::Action(CommandAction::SetCompactTools(mode)))
            }),
        )
        .localized()
        .usage("/compact-tools [on|off|toggle]"),
    );

    // Refresh command
    registry.register(
        Command::new(
            "refresh",
            maestro_ui::localization::tr("Refresh workspace files"),
            CommandCategory::Navigation,
            Box::new(|_| Ok(CommandOutput::Action(CommandAction::RefreshWorkspace))),
        )
        .localized(),
    );

    registry.register(
        Command::new(
            "language",
            maestro_ui::localization::tr("Choose the display language"),
            CommandCategory::Ui,
            Box::new(|ctx| {
                if ctx.raw_args.trim().is_empty() {
                    return Ok(CommandOutput::Action(CommandAction::OpenPanel(
                        ControlPanel::Language,
                    )));
                }
                let locale =
                    crate::localization::Locale::parse(ctx.raw_args.trim()).ok_or_else(|| {
                        CommandError::new(
                            "Supported display languages: en, es, fr, de, ja, ko, zh-CN",
                        )
                    })?;
                Ok(CommandOutput::Action(CommandAction::SetLanguage(locale)))
            }),
        )
        .localized()
        .usage("/language [en|es|fr|de|ja|ko|zh-CN]"),
    );

    // Copy command
    registry.register(
        Command::new(
            "copy",
            maestro_ui::localization::tr("Copy a response, prompts, turns, or the session ID"),
            CommandCategory::Ui,
            Box::new(|ctx| {
                let target = crate::transcript_copy::CopyTarget::parse(ctx.raw_args.trim())
                    .map_err(CommandError::new)?;
                Ok(CommandOutput::Action(CommandAction::CopyTranscript(target)))
            }),
        )
        .localized(),
    );

    // A2A peer pairing command
    registry.register(
        Command::new(
            "a2a",
            maestro_ui::localization::tr("Pair, inspect, and delegate to A2A peer agents"),
            CommandCategory::Tools,
            Box::new(|ctx| {
                Ok(CommandOutput::Action(CommandAction::A2a(parse_a2a_action(
                    &ctx.raw_args,
                )?)))
            }),
        ).localized()
        .usage("/a2a [fleet|peers|tasks [--work-graph]|coordinate [--work-graph]|accept <code>|register --url <base-url>|delegate <peer> <text>|reply <peer> <task-id> <text>|send <peer> <text>]"),
    );

    registry.register(
        Command::new(
            "handoff",
            maestro_ui::localization::tr("Hand work to the default peer and follow its response"),
            CommandCategory::Tools,
            Box::new(|ctx| {
                Ok(CommandOutput::Action(CommandAction::A2a(
                    parse_handoff_action(&ctx.raw_args)?,
                )))
            }),
        )
        .localized()
        .usage("/handoff <prompt> (use --peer <name> to override the default)"),
    );

    // Queue command
    registry.register(
        Command::new(
            "queue",
            maestro_ui::localization::tr("Manage queued prompts"),
            CommandCategory::Ui,
            Box::new(|ctx| {
                let args = ctx.raw_args.trim();
                if args.is_empty() || args.eq_ignore_ascii_case("list") {
                    return Ok(CommandOutput::Action(CommandAction::Queue(
                        QueueAction::Show,
                    )));
                }

                let mut parts = args.split_whitespace();
                let action = parts.next().unwrap_or("");
                if action.eq_ignore_ascii_case("cancel") {
                    let raw_id = parts
                        .next()
                        .ok_or_else(|| CommandError::new(maestro_ui::localization::tr("Usage: /queue cancel <id>")))?;
                    let trimmed = raw_id.trim_start_matches('#');
                    let id = trimmed.parse::<u64>().map_err(|_| {
                        CommandError::new(maestro_ui::localization::tr("Queue id must be a number (e.g. /queue cancel 12)"))
                    })?;
                    return Ok(CommandOutput::Action(CommandAction::Queue(
                        QueueAction::Cancel { id },
                    )));
                }

                if action.eq_ignore_ascii_case("move") || action.eq_ignore_ascii_case("send") {
                    let raw_id = parts.next().ok_or_else(|| {
                        if action.eq_ignore_ascii_case("send") {
                            CommandError::new(maestro_ui::localization::tr("Usage: /queue send <id>"))
                        } else {
                            CommandError::new(maestro_ui::localization::tr("Usage: /queue move <id> <up|down>"))
                        }
                    })?;
                    let trimmed = raw_id.trim_start_matches('#');
                    let id = trimmed.parse::<u64>().map_err(|_| {
                        CommandError::new(maestro_ui::localization::tr("Queue id must be a number (e.g. /queue send 12)"))
                    })?;
                    let direction = if action.eq_ignore_ascii_case("send") {
                        QueueMoveDirection::Now
                    } else {
                        match parts.next().map(str::to_ascii_lowercase).as_deref() {
                            Some("up") => QueueMoveDirection::Up,
                            Some("down") => QueueMoveDirection::Down,
                            _ => {
                                return Err(CommandError::new(
                                    maestro_ui::localization::tr("Usage: /queue move <id> <up|down>"),
                                ));
                            }
                        }
                    };
                    return Ok(CommandOutput::Action(CommandAction::Queue(
                        QueueAction::Move { id, direction },
                    )));
                }

                if action != "mode" {
                    return Err(CommandError::new(
                        maestro_ui::localization::tr("Usage: /queue [list|cancel <id>|move <id> <up|down>|send <id>|mode [steer|followup] <one|all>]"),
                    ));
                }

                let scope = parts.next();
                let value = parts.next();
                let (kind, mode) = match (scope, value) {
                    (None, _) => {
                        return Err(CommandError::new(
                            maestro_ui::localization::tr("Usage: /queue mode [steer|followup] <one|all>"),
                        ));
                    }
                    (Some(scope), None) => {
                        if let Some(mode) = QueueMode::parse(scope) {
                            (QueueModeKind::FollowUp, mode)
                        } else {
                            return Err(CommandError::new(
                                maestro_ui::localization::tr("Usage: /queue mode [steer|followup] <one|all>"),
                            ));
                        }
                    }
                    (Some(scope), Some(value)) => {
                        let kind = match scope.to_lowercase().as_str() {
                            "steer" | "steering" => QueueModeKind::Steering,
                            "followup" | "follow-up" => QueueModeKind::FollowUp,
                            _ => {
                                return Err(CommandError::new(
                                    maestro_ui::localization::tr("Usage: /queue mode [steer|followup] <one|all>"),
                                ));
                            }
                        };
                        let Some(mode) = QueueMode::parse(value) else {
                            return Err(CommandError::new(maestro_ui::localization::tr("Mode must be \"one\" or \"all\".")));
                        };
                        (kind, mode)
                    }
                };

                Ok(CommandOutput::Action(CommandAction::Queue(
                    QueueAction::Mode { kind, mode },
                )))
            }),
        ).localized()
        .usage("/queue [list|cancel <id>|move <id> <up|down>|send <id>|mode [steer|followup] <one|all>]"),
    );

    // Steer command
    registry.register(
        Command::new(
            "steer",
            maestro_ui::localization::tr("Send a steering message"),
            CommandCategory::Ui,
            Box::new(|ctx| {
                let text = ctx.raw_args.trim();
                if text.is_empty() {
                    return Err(CommandError::new(maestro_ui::localization::tr(
                        "Usage: /steer <message>",
                    )));
                }
                Ok(CommandOutput::Action(CommandAction::Steer(
                    text.to_string(),
                )))
            }),
        )
        .localized()
        .usage("/steer <message>"),
    );

    // Theme command
    registry.register(
        Command::new(
            "theme",
            maestro_ui::localization::tr("Change color theme"),
            CommandCategory::Ui,
            Box::new(|ctx| {
                if ctx.raw_args.is_empty() {
                    Ok(CommandOutput::OpenModal(ModalType::ThemeSelector))
                } else {
                    Ok(CommandOutput::Action(CommandAction::SetTheme(
                        ctx.raw_args.clone(),
                    )))
                }
            }),
        )
        .localized()
        .arg(CommandArgument::string(
            "name",
            maestro_ui::localization::tr("Theme name"),
        ))
        .usage("/theme [name]"),
    );

    // Model command
    registry.register(
        Command::new(
            "model",
            maestro_ui::localization::tr("Change AI model"),
            CommandCategory::Config,
            Box::new(|ctx| {
                let mut parts = ctx.raw_args.split_whitespace();
                match (parts.next(), parts.next()) {
                    (None, _) => Ok(CommandOutput::Action(CommandAction::OpenPanel(
                        ControlPanel::Model,
                    ))),
                    (Some("select"), None) => {
                        Ok(CommandOutput::OpenModal(ModalType::ModelSelector))
                    }
                    (Some("default"), Some(model)) => Ok(CommandOutput::Action(
                        CommandAction::SetDefaultModel(model.to_string()),
                    )),
                    (Some("default"), None) => Err(CommandError::new(
                        maestro_ui::localization::tr("Usage: /model default <name>"),
                    )),
                    _ => Ok(CommandOutput::Action(CommandAction::SetModel(
                        ctx.raw_args.clone(),
                    ))),
                }
            }),
        )
        .localized()
        .alias("m")
        .arg(CommandArgument::string(
            "name",
            maestro_ui::localization::tr("Model name"),
        ))
        .usage("/model [select | name | default <name>]")
        .primary(4),
    );

    // Rubber duck review command
    registry.register(
        Command::new(
            "rubber-duck",
            maestro_ui::localization::tr(
                "Review uncommitted changes with a different model (second opinion)",
            ),
            CommandCategory::Tools,
            Box::new(|ctx| {
                let model = ctx.raw_args.trim();
                Ok(CommandOutput::Action(CommandAction::RubberDuck {
                    model: if model.is_empty() {
                        None
                    } else {
                        Some(model.to_string())
                    },
                }))
            }),
        )
        .localized()
        .alias("duck")
        .arg(CommandArgument::string(
            "model",
            maestro_ui::localization::tr(
                "Model to review with (defaults to another provider's model)",
            ),
        ))
        .usage("/rubber-duck [model]"),
    );

    // Session commands
    registry.register(
        Command::new(
            "session",
            maestro_ui::localization::tr("Session information"),
            CommandCategory::Session,
            Box::new(|ctx| {
                let sub = ctx
                    .raw_args
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_lowercase();
                match sub.as_str() {
                    "cleanup" | "prune" => Ok(CommandOutput::Action(CommandAction::Session(
                        SessionAction::Cleanup,
                    ))),
                    "new" | "clear" => Ok(CommandOutput::Action(CommandAction::Session(
                        SessionAction::New,
                    ))),
                    "fork" => Ok(CommandOutput::Action(CommandAction::Session(
                        SessionAction::Fork,
                    ))),
                    "rewind" | "undo" => Ok(CommandOutput::Action(CommandAction::Session(
                        parse_rewind_args(
                            ctx.raw_args
                                .strip_prefix(ctx.raw_args.split_whitespace().next().unwrap_or(""))
                                .unwrap_or("")
                                .trim(),
                            maestro_ui::localization::tr("Usage: /session rewind [n] [--dry-run] [--files] | /session rewind files | /session rewind checkpoints"),
                        )?,
                    ))),
                    "info" | "status" | "" => {
                        Ok(CommandOutput::Action(CommandAction::Session(
                            SessionAction::Status,
                        )))
                    }
                    _ => Ok(CommandOutput::Message(
                        maestro_ui::localization::tr("Usage: /session [status|info|new|clear|fork|rewind|cleanup]").to_string(),
                    )),
                }
            }),
        ).localized()
        .alias("ss")
        .usage("/session [status|info|new|clear|list|load|export|cleanup|fork|rewind]")
        .group(vec![
            "status", "info", "new", "clear", "list", "load", "export", "cleanup", "fork",
            "rewind",
        ]),
    );

    registry.register(
        Command::new(
            "experiments",
            "Show or change participation in tool experiments",
            CommandCategory::Config,
            Box::new(|ctx| {
                maestro_local_host::experiments::command(ctx.raw_args.trim())
                    .map(CommandOutput::Message)
                    .map_err(|error| CommandError::new(error.to_string()))
            }),
        )
        .usage("/experiments [status|on|off]")
        .group(vec!["status", "on", "off"]),
    );
    registry.register(
        Command::new(
            "preferences",
            "Open Experiments preferences",
            CommandCategory::Config,
            Box::new(|_| Ok(CommandOutput::Action(CommandAction::ShowPreferences))),
        )
        .usage("/preferences"),
    );

    // Workspace trust (global config only)
    registry.register(
        Command::new(
            "trust",
            maestro_ui::localization::tr(
                "Grant or revoke trust so project skills/plugins/hooks can load",
            ),
            CommandCategory::Safety,
            Box::new(|ctx| {
                let sub = ctx
                    .raw_args
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_lowercase();
                let action = match sub.as_str() {
                    "" | "status" | "show" => crate::commands::TrustAction::Status,
                    "grant" | "on" | "yes" | "true" => crate::commands::TrustAction::Grant,
                    "revoke" | "off" | "no" | "false" => crate::commands::TrustAction::Revoke,
                    _ => {
                        return Ok(CommandOutput::Message(
                            maestro_ui::localization::tr("Usage: /trust [status|grant|revoke]")
                                .to_string(),
                        ));
                    }
                };
                Ok(CommandOutput::Action(CommandAction::Trust(action)))
            }),
        )
        .localized()
        .usage("/trust [status|grant|revoke]"),
    );

    // Sandbox status
    registry.register(
        Command::new(
            "sandbox",
            maestro_ui::localization::tr("Show the interactive sandbox policy for this session"),
            CommandCategory::Safety,
            Box::new(|_| Ok(CommandOutput::Action(CommandAction::ShowSandbox))),
        )
        .localized()
        .usage("/sandbox"),
    );

    registry.register(
        Command::new(
            "sessions",
            maestro_ui::localization::tr("List and manage sessions"),
            CommandCategory::Session,
            Box::new(|_| Ok(CommandOutput::OpenModal(ModalType::SessionList))),
        )
        .localized(),
    );

    registry.register(
        Command::new(
            "operations",
            maestro_ui::localization::tr("Inspect recent persisted tool executions"),
            CommandCategory::Diagnostics,
            Box::new(|_| Ok(CommandOutput::OpenModal(ModalType::Operations))),
        )
        .localized(),
    );

    registry.register(
        Command::new(
            "monitor",
            maestro_ui::localization::tr("Monitor output from an existing background task"),
            CommandCategory::Diagnostics,
            Box::new(|ctx| {
                let raw = ctx.raw_args.trim();
                let (action, rest) = raw
                    .split_once(char::is_whitespace)
                    .map_or((raw, ""), |(action, rest)| (action, rest.trim_start()));
                match action {
                    "" | "list" => Ok(CommandOutput::Action(CommandAction::BackgroundMonitor(
                        BackgroundMonitorAction::List,
                    ))),
                    "add" => {
                        let (task_id, pattern) =
                            rest.split_once(char::is_whitespace).ok_or_else(|| {
                                CommandError::new(maestro_ui::localization::tr(
                                    "Usage: /monitor add <task-id> <regex>",
                                ))
                            })?;
                        Ok(CommandOutput::Action(CommandAction::BackgroundMonitor(
                            BackgroundMonitorAction::Add {
                                task_id: task_id.to_string(),
                                pattern: pattern.trim_start().to_string(),
                            },
                        )))
                    }
                    "remove" | "rm" => {
                        if rest.is_empty() {
                            return Err(CommandError::new(maestro_ui::localization::tr(
                                "Usage: /monitor remove <monitor-id>",
                            )));
                        }
                        Ok(CommandOutput::Action(CommandAction::BackgroundMonitor(
                            BackgroundMonitorAction::Remove {
                                monitor_id: rest.to_string(),
                            },
                        )))
                    }
                    _ => Err(CommandError::new(maestro_ui::localization::tr(
                        "Usage: /monitor [list|add <task-id> <regex>|remove <monitor-id>]",
                    ))),
                }
            }),
        )
        .localized()
        .usage("/monitor [list|add <task-id> <regex>|remove <monitor-id>]")
        .group(vec!["list", "add", maestro_ui::localization::tr("remove")]),
    );

    registry.register(
        Command::new(
            "loop",
            maestro_ui::localization::tr("Re-run a prompt on an interval"),
            CommandCategory::Session,
            Box::new(|ctx| {
                let raw = ctx.raw_args.trim();
                if raw.is_empty() {
                    return Ok(CommandOutput::Action(CommandAction::Loop(
                        LoopAction::Status,
                    )));
                }
                if raw == "stop" {
                    return Ok(CommandOutput::Action(CommandAction::Loop(LoopAction::Stop)));
                }
                let (interval_text, prompt) =
                    raw.split_once(char::is_whitespace).ok_or_else(|| {
                        CommandError::new(maestro_ui::localization::tr(
                            "Usage: /loop [stop|<interval> <prompt>]",
                        ))
                    })?;
                let prompt = prompt.trim();
                if prompt.is_empty() {
                    return Err(CommandError::new(maestro_ui::localization::tr(
                        "Usage: /loop [stop|<interval> <prompt>]",
                    )));
                }
                let interval_secs = parse_loop_interval(interval_text).ok_or_else(|| {
                    CommandError::new(maestro_ui::localization::format(
                        "Invalid interval '{0}' (try 30s, 5m, 1h, or minutes as a bare number)",
                        &[(interval_text).to_string()],
                    ))
                })?;
                Ok(CommandOutput::Action(CommandAction::Loop(
                    LoopAction::Start {
                        interval_secs,
                        prompt: prompt.to_string(),
                    },
                )))
            }),
        )
        .localized()
        .usage("/loop [stop|<interval> <prompt>]")
        .group(vec!["stop"]),
    );

    registry.register(
        Command::new(
            "files",
            maestro_ui::localization::tr("Search workspace files"),
            CommandCategory::Navigation,
            Box::new(|_| Ok(CommandOutput::OpenModal(ModalType::FileSearch))),
        )
        .localized(),
    );

    registry.register(
        Command::new(
            "commands",
            maestro_ui::localization::tr("Open command palette"),
            CommandCategory::Navigation,
            Box::new(|_| Ok(CommandOutput::OpenModal(ModalType::CommandPalette))),
        )
        .localized(),
    );

    registry.register(
        Command::new(
            "summarize",
            maestro_ui::localization::tr(
                "Summarize from or through a chosen turn into a saved conversation",
            ),
            CommandCategory::Context,
            Box::new(|_| Ok(CommandOutput::Action(CommandAction::SummarizeConversation))),
        )
        .localized(),
    );

    // Compact command
    registry.register(
        Command::new(
            "compact",
            maestro_ui::localization::tr("Compact conversation history to reduce context size"),
            CommandCategory::Context,
            Box::new(|ctx| {
                let instructions = if ctx.raw_args.is_empty() {
                    None
                } else {
                    Some(ctx.raw_args.clone())
                };
                Ok(CommandOutput::Action(CommandAction::CompactConversation(
                    instructions,
                )))
            }),
        )
        .localized()
        .arg(CommandArgument::string(
            "instructions",
            maestro_ui::localization::tr("Custom compaction instructions"),
        ))
        .usage("/compact [instructions]"),
    );

    // Approval mode command
    registry.register(
        Command::new(
            "approvals",
            maestro_ui::localization::tr("Set approval mode"),
            CommandCategory::Safety,
            Box::new(|ctx| {
                let mode = ctx.raw_args.trim().to_string();
                if mode.is_empty() {
                    Ok(CommandOutput::Action(CommandAction::OpenPanel(
                        ControlPanel::Permissions,
                    )))
                } else {
                    Ok(CommandOutput::Action(CommandAction::SetApprovalMode(mode)))
                }
            }),
        )
        .localized()
        .arg(CommandArgument::choice(
            "mode",
            maestro_ui::localization::tr("Approval mode"),
            vec!["yolo", "selective", "safe"],
        ))
        .usage("/approvals [yolo|selective|safe]"),
    );

    registry.register(
        Command::new(
            "boost",
            maestro_ui::localization::tr("Give this task more intelligence"),
            CommandCategory::Config,
            Box::new(|_| Ok(CommandOutput::Action(CommandAction::Boost))),
        )
        .localized()
        .alias("b")
        .usage("/boost"),
    );

    // Thinking level command
    registry.register(
        Command::new(
            "thinking",
            maestro_ui::localization::tr("Set extended thinking level"),
            CommandCategory::Config,
            Box::new(|ctx| {
                let level = ctx.raw_args.trim().to_string();
                if level.is_empty() {
                    Ok(CommandOutput::Action(CommandAction::OpenPanel(
                        ControlPanel::Effort,
                    )))
                } else {
                    Ok(CommandOutput::Action(CommandAction::SetThinkingLevel(
                        level,
                    )))
                }
            }),
        )
        .localized()
        .arg(CommandArgument::choice(
            "level",
            maestro_ui::localization::tr("Thinking level"),
            vec!["off", "minimal", "low", "medium", "high", "max"],
        ))
        .usage("/thinking <level>"),
    );

    // About command
    registry.register(
        Command::new(
            "about",
            maestro_ui::localization::tr("Show build and environment info"),
            CommandCategory::Diagnostics,
            Box::new(|ctx| Ok(CommandOutput::Message(build_diag_about(ctx)))),
        )
        .localized()
        .usage("/about"),
    );

    // Context command
    registry.register(
        Command::new(
            "context",
            maestro_ui::localization::tr(
                "Show context usage or audit the effective prompt surface",
            ),
            CommandCategory::Context,
            Box::new(|ctx| {
                let raw = ctx.raw_args.trim();
                if raw.is_empty() {
                    return Ok(CommandOutput::Action(CommandAction::OpenPanel(
                        ControlPanel::Context,
                    )));
                }
                if raw == "usage" {
                    return Ok(CommandOutput::Action(CommandAction::ShowContext));
                }
                let args = raw.split_whitespace().collect::<Vec<_>>();
                if let [operation @ ("exclude" | "include"), name] = args.as_slice() {
                    return Ok(CommandOutput::Action(CommandAction::SetContextTool {
                        name: (*name).to_string(),
                        excluded: *operation == "exclude",
                    }));
                }
                match raw {
                    "audit" => Ok(CommandOutput::Action(CommandAction::ShowPromptAudit {
                        json: false,
                    })),
                    "audit --json" | "audit -j" => {
                        Ok(CommandOutput::Action(CommandAction::ShowPromptAudit {
                            json: true,
                        }))
                    }
                    _ => Err(CommandError::new(maestro_ui::localization::tr(
                        "Usage: /context [audit [--json] | exclude TOOL | include TOOL]",
                    ))),
                }
            }),
        )
        .localized()
        .usage("/context [audit [--json] | exclude TOOL | include TOOL]")
        .primary(9),
    );

    registry.register(
        Command::new(
            "focus",
            maestro_ui::localization::tr("Collapse tool-heavy turns into one live summary"),
            CommandCategory::Ui,
            Box::new(|ctx| {
                let value = match ctx.raw_args.trim().to_ascii_lowercase().as_str() {
                    "" | "toggle" => None,
                    "on" => Some(true),
                    "off" => Some(false),
                    _ => {
                        return Err(CommandError::new(maestro_ui::localization::tr(
                            "Usage: /focus [on|off|toggle]",
                        )));
                    }
                };
                Ok(CommandOutput::Action(CommandAction::SetFocus(value)))
            }),
        )
        .localized()
        .usage("/focus [on|off|toggle]"),
    );

    registry.register(
        Command::new(
            "prompt-audit",
            maestro_ui::localization::tr("Audit prompt provenance without exposing prompt content"),
            CommandCategory::Diagnostics,
            Box::new(|ctx| {
                let json = match ctx.raw_args.trim() {
                    "" => false,
                    "--json" | "-j" => true,
                    _ => {
                        return Err(CommandError::new(maestro_ui::localization::tr(
                            "Usage: /prompt-audit [--json]",
                        )));
                    }
                };
                Ok(CommandOutput::Action(CommandAction::ShowPromptAudit {
                    json,
                }))
            }),
        )
        .localized()
        .usage("/prompt-audit [--json]"),
    );

    // Limits command
    registry.register(
        Command::new(
            "limits",
            maestro_ui::localization::tr("Show configurable runtime limits"),
            CommandCategory::Config,
            Box::new(|ctx| {
                let subcommand = ctx
                    .raw_args
                    .split_whitespace()
                    .next()
                    .unwrap_or("all")
                    .to_lowercase();
                if matches!(subcommand.as_str(), "help" | "?" | "-h" | "--help") {
                    return Ok(CommandOutput::Message(
                        maestro_ui::localization::tr("Usage: /limits [all|tool|lsp|help]")
                            .to_string(),
                    ));
                }

                let tool_limits = tool_output_limits();
                let lsp_limit = max_diagnostics_per_file();

                let mut sections: Vec<(&str, Vec<String>)> = Vec::new();
                sections.push((
                    maestro_ui::localization::tr("Tool output (TUI):"),
                    vec![
                        format!(
                            "  TUI_TOOL_MAX_CHARS: {} (env: MAESTRO_TUI_TOOL_MAX_CHARS)",
                            tool_limits.max_chars
                        ),
                        format!(
                            "  TUI_TOOL_MAX_LINES: {} (env: MAESTRO_TUI_TOOL_MAX_LINES)",
                            tool_limits.max_lines
                        ),
                    ],
                ));
                sections.push((
                    maestro_ui::localization::tr("LSP diagnostics:"),
                    vec![format!(
                        "  MAX_DIAGNOSTICS_PER_FILE: {} (env: MAESTRO_LSP_MAX_DIAGNOSTICS)",
                        lsp_limit
                    )],
                ));

                let selected: Vec<(&str, Vec<String>)> = match subcommand.as_str() {
                    "all" | "" => sections.clone(),
                    "tool" | "tui" => sections
                        .first()
                        .map(|(title, lines)| (*title, lines.clone()))
                        .into_iter()
                        .collect(),
                    "lsp" => sections
                        .get(1)
                        .map(|(title, lines)| (*title, lines.clone()))
                        .into_iter()
                        .collect(),
                    _ => {
                        return Err(CommandError::new(maestro_ui::localization::tr(
                            "Usage: /limits [all|tool|lsp|help]",
                        )));
                    }
                };

                let mut lines = vec![
                    maestro_ui::localization::tr("Limits (restart after changing env vars):")
                        .to_string(),
                ];
                for (title, entries) in selected {
                    lines.push(String::new());
                    lines.push(title.to_string());
                    lines.extend(entries);
                }

                Ok(CommandOutput::Message(lines.join("\n")))
            }),
        )
        .localized()
        .usage("/limits [all|tool|lsp|help]"),
    );

    // Git diff command
    registry.register(
        Command::new(
            "diff",
            maestro_ui::localization::tr("Show git diff for working tree or a path"),
            CommandCategory::Diagnostics,
            Box::new(|ctx| {
                let path = ctx.raw_args.trim();
                Ok(CommandOutput::Message(build_git_diff_message(
                    &ctx.cwd,
                    if path.is_empty() { None } else { Some(path) },
                )))
            }),
        )
        .localized()
        .arg(CommandArgument::string(
            "path",
            maestro_ui::localization::tr("Optional path to diff"),
        ))
        .usage("/diff [path]"),
    );

    // Git review command
    registry.register(
        Command::new(
            "review",
            maestro_ui::localization::tr("Inspect changes, diffs, or request a second opinion"),
            CommandCategory::Diagnostics,
            Box::new(|_| {
                Ok(CommandOutput::Action(CommandAction::OpenPanel(
                    ControlPanel::Review,
                )))
            }),
        )
        .localized()
        .usage("/review")
        .primary(6),
    );

    // Git command (grouped)
    registry.register(
        Command::new(
            "git",
            maestro_ui::localization::tr("Git operations: status, diff, review"),
            CommandCategory::Diagnostics,
            Box::new(|ctx| {
                let mut parts = ctx.raw_args.split_whitespace();
                let sub = parts.next().unwrap_or("").to_lowercase();
                let rest_joined = parts.collect::<Vec<_>>().join(" ");
                let rest = if rest_joined.trim().is_empty() {
                    None
                } else {
                    Some(rest_joined.trim())
                };

                let message = match sub.as_str() {
                    "" | "status" | "st" => build_git_status_message(&ctx.cwd),
                    "diff" | "d" => build_git_diff_message(&ctx.cwd, rest),
                    "review" | "summary" => build_git_review_message(&ctx.cwd),
                    "help" | "?" | "-h" | "--help" => git_help_message(),
                    _ => {
                        let mut msg = String::new();
                        msg.push_str(maestro_ui::localization::tr("Unknown git subcommand.\n\n"));
                        msg.push_str(&git_help_message());
                        msg
                    }
                };

                Ok(CommandOutput::Message(message))
            }),
        )
        .localized()
        .usage("/git [status|diff <path>|review]"),
    );

    registry.register(
        Command::new(
            "setup",
            maestro_ui::localization::tr("Sign in to EvalOps or add a local API key"),
            CommandCategory::Config,
            Box::new(|_| Ok(CommandOutput::OpenModal(ModalType::Setup))),
        )
        .localized()
        .usage("/setup"),
    );

    registry.register(
        Command::new(
            "init",
            maestro_ui::localization::tr("Scaffold AGENTS.md for this project"),
            CommandCategory::Config,
            Box::new(|ctx| {
                let tokens: Vec<&str> = ctx.raw_args.split_whitespace().collect();
                let force = tokens.iter().any(|arg| *arg == "--force" || *arg == "-f");
                if !tokens.iter().all(|arg| *arg == "--force" || *arg == "-f") {
                    return Err(CommandError::new(maestro_ui::localization::tr(
                        "Usage: /init [--force]",
                    )));
                }
                Ok(CommandOutput::Action(CommandAction::Init { force }))
            }),
        )
        .localized()
        .usage("/init [--force]"),
    );

    // Status command
    registry.register(
        Command::new(
            "status",
            maestro_ui::localization::tr("Show system health overview"),
            CommandCategory::Diagnostics,
            Box::new(|_| Ok(CommandOutput::Action(CommandAction::ShowDiagnostics))),
        )
        .localized()
        .alias("health"),
    );

    // Alerts command
    registry.register(
        Command::new(
            "alerts",
            maestro_ui::localization::tr("List recorded alerts (agent/API errors)"),
            CommandCategory::Diagnostics,
            Box::new(|_| Ok(CommandOutput::Action(CommandAction::ShowAlerts))),
        )
        .localized(),
    );

    // Stats command
    registry.register(
        Command::new(
            "stats",
            maestro_ui::localization::tr("Show combined status and usage summary"),
            CommandCategory::Diagnostics,
            Box::new(|_| {
                Ok(CommandOutput::Multi(vec![
                    CommandOutput::Action(CommandAction::ShowDiagnostics),
                    CommandOutput::Action(CommandAction::ShowUsage(UsageAction::Summary)),
                ]))
            }),
        )
        .localized(),
    );

    // Diagnostics command
    registry.register(
        Command::new(
            "diag",
            maestro_ui::localization::tr("System diagnostics"),
            CommandCategory::Diagnostics,
            Box::new(|ctx| {
                let subcommand = ctx
                    .raw_args
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_lowercase();
                match subcommand.as_str() {
                    "" | "status" | "health" => {
                        Ok(CommandOutput::Action(CommandAction::ShowDiagnostics))
                    }
                    "stats" | "overview" => Ok(CommandOutput::Multi(vec![
                        CommandOutput::Action(CommandAction::ShowDiagnostics),
                        CommandOutput::Action(CommandAction::ShowUsage(UsageAction::Summary)),
                    ])),
                    "mcp" => Ok(CommandOutput::Action(CommandAction::Mcp(McpAction::Status))),
                    "help" | "?" | "-h" | "--help" => Ok(CommandOutput::Message(
                        maestro_ui::localization::tr(
                            "Usage: /diag [status|stats|about|context|mcp|help]",
                        )
                        .to_string(),
                    )),
                    "about" => Ok(CommandOutput::Message(build_diag_about(ctx))),
                    "context" => Ok(CommandOutput::Message(build_diag_context(ctx))),
                    "lsp" => Ok(CommandOutput::Message(
                        maestro_ui::localization::tr(
                            "LSP diagnostics are not supported in the Rust TUI yet.",
                        )
                        .to_string(),
                    )),
                    _ => Ok(CommandOutput::Action(CommandAction::ShowDiagnostics)),
                }
            }),
        )
        .localized()
        .group(vec!["status", "stats", "about", "context", "mcp"]),
    );

    // Jane Street magic-trace (Linux/Intel PT) — https://github.com/janestreet/magic-trace
    registry.register(
        Command::new(
            "magic-trace",
            maestro_ui::localization::tr(
                "Fire magic-trace stop indicator or toggle slow-frame snapshots",
            ),
            CommandCategory::Diagnostics,
            Box::new(|ctx| {
                let sub = ctx
                    .raw_args
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_lowercase();
                let action = match sub.as_str() {
                    "" | "stop" | "snap" | "snapshot" => crate::commands::MagicTraceAction::Stop,
                    "on" | "enable" | "slow" => crate::commands::MagicTraceAction::EnableSlowFrame,
                    "off" | "disable" => crate::commands::MagicTraceAction::DisableSlowFrame,
                    "status" | "help" | "?" => crate::commands::MagicTraceAction::Status,
                    _ => {
                        return Err(CommandError::new(maestro_ui::localization::tr(
                            "Usage: /magic-trace [stop|on|off|status]",
                        )));
                    }
                };
                Ok(CommandOutput::Action(CommandAction::MagicTrace(action)))
            }),
        )
        .localized()
        .alias("mt")
        .usage("/magic-trace [stop|on|off|status]"),
    );

    // Tools command
    registry.register(
        Command::new(
            "tools",
            maestro_ui::localization::tr("List built-in tools (and MCP via /mcp)"),
            CommandCategory::Tools,
            Box::new(|ctx| {
                let sub = ctx
                    .raw_args
                    .split_whitespace()
                    .next()
                    .unwrap_or("list")
                    .to_lowercase();
                match sub.as_str() {
                    "list" | "" => Ok(CommandOutput::Action(CommandAction::ShowTools)),
                    "mcp" => Ok(CommandOutput::Action(CommandAction::Mcp(McpAction::Status))),
                    "lsp" => Ok(CommandOutput::Message(
                        maestro_ui::localization::tr("LSP: set lsp.enabled in config; diagnostics can surface on write tools.")
                            .to_string(),
                    )),
                    _ => Err(CommandError::new(maestro_ui::localization::tr("Usage: /tools [list|mcp|lsp]"))),
                }
            }),
        ).localized()
        .group(vec!["list", "mcp", "lsp"])
        .usage("/tools [list|mcp|lsp]"),
    );

    // Hosted Computer command. `/orb` remains a compatibility alias.
    registry.register(Command::new(
        "computer",
        maestro_ui::localization::tr("Control durable hosted Computer tasks without exposing MCP internals"),
        CommandCategory::Tools,
        Box::new(|ctx| {
            Ok(CommandOutput::Action(CommandAction::Orb(parse_orb_action(
                &ctx.raw_args,
            )?)))
        }),
    ).localized().alias("orb").group(vec![
        "list", "status", "followup", "pause", "resume", maestro_ui::localization::tr("cancel"), "collect",
    ]).usage(
        "/computer [list|status <task-id>|followup <task-id> <prompt>|pause <task-id>|resume <task-id>|cancel <task-id>|collect <task-id>]",
    ));

    // MCP command
    registry.register(
        Command::new(
            "mcp",
            maestro_ui::localization::tr("Open the MCP server manager"),
            CommandCategory::Tools,
            Box::new(|ctx| {
                let raw = ctx.raw_args.trim();
                let tokens = tokenize_command_args(raw);
                let subcommand = tokens
                    .first()
                    .map(|token| token.to_lowercase())
                    .unwrap_or_default();

                let action = match subcommand.as_str() {
                    "" => McpAction::Status,
                    "config" => McpAction::Configure {
                        args: tokens.into_iter().skip(1).collect(),
                    },
                    "resources" => {
                        let server = tokens.get(1).cloned();
                        let uri = if server.is_some() {
                            let rest = tokens.iter().skip(2).cloned().collect::<Vec<_>>().join(" ");
                            if rest.is_empty() { None } else { Some(rest) }
                        } else {
                            None
                        };
                        McpAction::Resources { server, uri }
                    }
                    "prompts" => parse_mcp_prompts_action(raw)?,
                    other => {
                        return Err(CommandError::new(maestro_ui::localization::format(
                            "Unknown mcp subcommand: {0}",
                            &[(other).to_string()],
                        ))
                        .with_hint(maestro_ui::localization::tr(
                            "Available: config, resources, prompts",
                        )));
                    }
                };

                Ok(CommandOutput::Action(CommandAction::Mcp(action)))
            }),
        )
        .localized(),
    );

    registry.register(
        Command::new(
            "mcp-config",
            maestro_ui::localization::tr("Open or script the MCP server manager"),
            CommandCategory::Tools,
            Box::new(|ctx| {
                let raw = ctx.raw_args.trim();
                if raw.is_empty() || raw.eq_ignore_ascii_case("wizard") {
                    return Ok(CommandOutput::Action(CommandAction::Mcp(McpAction::Status)));
                }
                Ok(CommandOutput::Action(CommandAction::Mcp(
                    McpAction::Configure {
                        args: tokenize_command_args(raw),
                    },
                )))
            }),
        )
        .localized(),
    );

    // Hooks command
    registry.register(
        Command::new(
            "hooks",
            maestro_ui::localization::tr("Manage the hook system (list, toggle, reload, metrics)"),
            CommandCategory::Tools,
            Box::new(|ctx| {
                let subcommand = ctx.raw_args.trim().to_lowercase();
                let action = match subcommand.as_str() {
                    "" | "list" => HooksAction::List,
                    "toggle" => HooksAction::Toggle,
                    "reload" => HooksAction::Reload,
                    "metrics" | "stats" => HooksAction::Metrics,
                    "enable" | "on" => HooksAction::Enable,
                    "disable" | "off" => HooksAction::Disable,
                    other => {
                        return Err(CommandError::new(maestro_ui::localization::format(
                            "Unknown hooks subcommand: {0}",
                            &[(other).to_string()],
                        ))
                        .with_hint(maestro_ui::localization::tr(
                            "Available: list, toggle, reload, metrics, enable, disable",
                        )));
                    }
                };
                Ok(CommandOutput::Action(CommandAction::HooksManage(action)))
            }),
        )
        .localized()
        .alias("hook")
        .arg(CommandArgument::choice(
            "action",
            maestro_ui::localization::tr("Hook management action"),
            vec![
                "list",
                maestro_ui::localization::tr("toggle"),
                "reload",
                "metrics",
                "enable",
                "disable",
            ],
        ))
        .usage("/hooks [list|toggle|reload|metrics|enable|disable]")
        .group(vec![
            "list",
            maestro_ui::localization::tr("toggle"),
            "reload",
            "metrics",
            "enable",
            "disable",
        ]),
    );

    // Version command
    registry.register(
        Command::new(
            "version",
            maestro_ui::localization::tr("Show version information"),
            CommandCategory::Diagnostics,
            Box::new(|_| {
                Ok(CommandOutput::Message(maestro_ui::localization::format(
                    "Deixic Code v{0}",
                    &[env!("CARGO_PKG_VERSION").to_string()],
                )))
            }),
        )
        .localized()
        .alias("v"),
    );

    registry.register(
        Command::new(
            "dex",
            maestro_ui::localization::tr("Dex appearance, reactions, recap, and preferences"),
            CommandCategory::Ui,
            Box::new(|ctx| {
                let setting = ctx.get_string("setting").unwrap_or(ctx.raw_args.trim());
                if crate::dex_actions::contains(setting) {
                    Ok(CommandOutput::Action(CommandAction::SetDexPresentation(
                        setting.to_owned(),
                    )))
                } else {
                    Err(CommandError::new(maestro_ui::localization::tr(
                        "Unknown Dex action; use /dex for help",
                    )))
                }
            }),
        )
        .localized()
        .arg(CommandArgument::choice(
            "setting",
            crate::dex_actions::help(),
            crate::dex_actions::command_ids(),
        ))
        .usage(format!(
            "/dex [{}]",
            crate::dex_actions::command_ids().join("|")
        )),
    );

    // Footer command
    registry.register(
        Command::new(
            "footer",
            maestro_ui::localization::tr(
                "Change status-bar footer style (rich|solo|history|clear)",
            ),
            CommandCategory::Ui,
            Box::new(|ctx| {
                if ctx.raw_args.trim().is_empty() {
                    return Ok(CommandOutput::Action(CommandAction::OpenPanel(
                        ControlPanel::Footer,
                    )));
                }
                let raw = ctx
                    .get_string("style")
                    .map(str::to_owned)
                    .or_else(|| {
                        let t = ctx.raw_args.trim();
                        (!t.is_empty()).then(|| t.to_string())
                    })
                    .unwrap_or_else(|| "rich".to_string());
                let style = FooterStyle::parse(&raw).ok_or_else(|| {
                    CommandError::new(maestro_ui::localization::format(
                        "Unknown footer style '{0}'. Use: rich, solo, history, clear",
                        std::slice::from_ref(&(raw)),
                    ))
                })?;
                Ok(CommandOutput::Action(CommandAction::SetFooterStyle(style)))
            }),
        )
        .localized()
        .arg(CommandArgument::choice(
            "style",
            maestro_ui::localization::tr("Footer style"),
            vec!["rich", "solo", "history", "clear"],
        ))
        .usage("/footer [rich|solo|history|clear]"),
    );

    // Goal mode (Kimi-inspired structured objective + auto-continue)
    registry.register(
        Command::new(
            "goal",
            maestro_ui::localization::tr("Structured goal mode: create, pause, block, complete, auto-continue"),
            CommandCategory::Context,
            Box::new(|ctx| {
                Ok(CommandOutput::Action(CommandAction::Goal(parse_goal_action(
                    &ctx.raw_args,
                )?)))
            }),
        ).localized()
        .usage(
            "/goal [status|create [--max-turns N] [--token-budget N] [--max-duration-secs N]|replace|pause|resume|block|complete|clear|auto on|auto off] [text]",
        ),
    );

    // Continual harness (Prime Agent-inspired durable refinement records).
    registry.register(
        Command::new(
            "harness",
            maestro_ui::localization::tr("Manage durable prompt, memory, skill, and subagent context"),
            CommandCategory::Context,
            Box::new(|ctx| {
                Ok(CommandOutput::Action(CommandAction::Harness(
                    parse_harness_action(&ctx.raw_args)?,
                )))
            }),
        ).localized()
        .alias("refine")
        .usage(
            "/harness [status|list|review|propose <scope> <kind> <name> <content> --evidence <text>|add <scope> <kind> <name> <content> [--evidence <text>]|update <id> <content>|delete <id>|apply <proposal-id>|reject <proposal-id> [note]|rollback <revision>]",
        ),
    );

    // RLM-style named context variables.
    registry.register(
        Command::new(
            "rlm",
            maestro_ui::localization::tr("Compose prompts from persistent named context variables"),
            CommandCategory::Context,
            Box::new(|ctx| {
                Ok(CommandOutput::Action(CommandAction::Rlm(parse_rlm_action(
                    &ctx.raw_args,
                )?)))
            }),
        ).localized()
        .usage("/rlm [list|set <name> <value> [--description <text>]|append <name> <value>|render <template>|clear <name>]"),
    );

    // Durable messages for parent and delegated agent sessions.
    registry.register(
        Command::new(
            "mailbox",
            maestro_ui::localization::tr(
                "Send and acknowledge durable messages between agent sessions",
            ),
            CommandCategory::Session,
            Box::new(|ctx| {
                Ok(CommandOutput::Action(CommandAction::Mailbox(
                    parse_mailbox_action(&ctx.raw_args)?,
                )))
            }),
        )
        .localized()
        .usage(
            "/mailbox [list|send <recipient> <message>|read <id>|ack <id>|approve <id>|compact]",
        ),
    );

    // Attach path (image/video) for multimodal next prompt
    registry.register(
        Command::new(
            "attach",
            maestro_ui::localization::tr(
                "Queue local files for the next prompt (add|list|clear|remove)",
            ),
            CommandCategory::Ui,
            Box::new(|ctx| {
                let raw = ctx.raw_args.trim();
                if raw.is_empty()
                    || raw.eq_ignore_ascii_case("list")
                    || raw.eq_ignore_ascii_case("ls")
                {
                    return Ok(CommandOutput::Action(CommandAction::Attach(
                        AttachAction::List,
                    )));
                }
                if raw.eq_ignore_ascii_case("clear") || raw.eq_ignore_ascii_case("reset") {
                    return Ok(CommandOutput::Action(CommandAction::Attach(
                        AttachAction::Clear,
                    )));
                }
                let mut parts = raw.split_whitespace();
                let first = parts.next().unwrap_or("");
                if first.eq_ignore_ascii_case("remove")
                    || first.eq_ignore_ascii_case("rm")
                    || first.eq_ignore_ascii_case("drop")
                    || first.eq_ignore_ascii_case("detach")
                {
                    let index = parts
                        .next()
                        .ok_or_else(|| {
                            CommandError::new(maestro_ui::localization::tr(
                                "Usage: /attach remove <1-based-index>",
                            ))
                        })?
                        .parse::<usize>()
                        .map_err(|_| {
                            CommandError::new(maestro_ui::localization::tr(
                                "Usage: /attach remove <1-based-index>",
                            ))
                        })?;
                    return Ok(CommandOutput::Action(CommandAction::Attach(
                        AttachAction::Remove { index },
                    )));
                }
                let path = raw
                    .strip_prefix("add ")
                    .or_else(|| raw.strip_prefix("add\t"))
                    .unwrap_or(raw)
                    .trim();
                if path.is_empty() {
                    return Err(CommandError::new(maestro_ui::localization::tr(
                        "Usage: /attach <path> | /attach list | /attach clear | /attach remove <n>",
                    )));
                }
                Ok(CommandOutput::Action(CommandAction::Attach(
                    AttachAction::Add(path.to_string()),
                )))
            }),
        )
        .localized()
        .usage("/attach <path|list|clear|remove <n>>"),
    );

    registry.register(Command::new(
        "workers", maestro_ui::localization::tr("Inspect, redirect, cancel, or resume existing workers"), CommandCategory::Tools,
        Box::new(|ctx| {
            use super::types::WorkerAction;
            let raw = ctx.raw_args.trim();
            let (verb, rest) = raw.split_once(char::is_whitespace).unwrap_or((raw, ""));
            let rest = rest.trim();
            let action = match verb {
                "" | "list" if rest.is_empty() => WorkerAction::List,
                "inspect" | "cancel" if !rest.is_empty() && !rest.contains(char::is_whitespace) => {
                    if verb == "inspect" { WorkerAction::Inspect(rest.into()) } else { WorkerAction::Cancel(rest.into()) }
                }
                "steer" | "resume" => {
                    let (id, message) = rest.split_once(char::is_whitespace)
                        .filter(|(id, message)| !id.is_empty() && !message.trim().is_empty())
                        .ok_or_else(|| CommandError::new(maestro_ui::localization::tr("Provide a worker id and a message.")))?;
                    if verb == "steer" { WorkerAction::Steer { agent_ref: id.into(), message: message.trim().into() } }
                    else { WorkerAction::Resume { id: id.into(), message: message.trim().into() } }
                }
                _ => return Err(CommandError::new(maestro_ui::localization::tr("Usage: /workers [list|inspect <id>|steer <agent-ref> <message>|cancel <id>|resume <id> <message>]"))),
            };
            Ok(CommandOutput::Action(CommandAction::Worker(action)))
        }),
    ).localized().usage("/workers [list|inspect <id>|steer <agent-ref> <message>|cancel <id>|resume <id> <message>]"));

    // Memory commands
    registry.register(
        Command::new(
            "memory",
            maestro_ui::localization::tr("Review, save, edit, or forget scoped memory; show account status"),
            CommandCategory::Context,
            Box::new(|ctx| {
                let raw = ctx.raw_args.trim();
                if raw.is_empty() {
                    return Ok(CommandOutput::Action(CommandAction::ShowMemory));
                }
                let (action, rest) = raw.split_once(char::is_whitespace).unwrap_or((raw, ""));
                let mapped = match action {
                    "list" => "list".to_string(),
                    "review" => "review".to_string(),
                    "save" => format!("apply {rest}"),
                    "edit" => format!("update {rest}"),
                    "forget" => format!("delete {rest}"),
                    "reject" => format!("reject {rest}"),
                    _ => return Err(CommandError::new(maestro_ui::localization::tr("Usage: /memory [list|review|save <proposal-id>|edit <entry-id> <text>|forget <entry-id>|reject <proposal-id>]"))),
                };
                Ok(CommandOutput::Action(CommandAction::Harness(parse_harness_action(&mapped)?)))
            }),
        ).localized()
        .group(vec!["list", "review", "save", "edit", "forget", "reject"])
        .usage("/memory [list|review|save <proposal-id>|edit <entry-id> <text>|forget <entry-id>|reject <proposal-id>]"),
    );

    // Plan mode (Grok-style: plan.md + approve)
    registry.register(
        Command::new(
            "plan",
            maestro_ui::localization::tr("Plan mode: explore + write plan.md only until approved"),
            CommandCategory::Context,
            Box::new(|ctx| {
                let raw = ctx.raw_args.trim();
                let mut parts = raw.split_whitespace();
                let subcommand = parts.next().unwrap_or("").to_lowercase();
                match subcommand.as_str() {
                    "" | "on" | "true" | "1" => {
                        Ok(CommandOutput::Action(CommandAction::SetPlanMode(true)))
                    }
                    "off" | "false" | "0" => {
                        Ok(CommandOutput::Action(CommandAction::SetPlanMode(false)))
                    }
                    "approve" | "accept" | "done" => {
                        Ok(CommandOutput::Action(CommandAction::ApprovePlan))
                    }
                    "view" | "show" => Ok(CommandOutput::Action(CommandAction::ViewPlan)),
                    "comments" | "list" => Ok(CommandOutput::Action(CommandAction::PlanReview(
                        PlanReviewAction::List,
                    ))),
                    "comment" => {
                        let range = parts.next().ok_or_else(|| {
                            CommandError::new(maestro_ui::localization::tr("Usage: /plan comment <line|start-end> <text>"))
                        })?;
                        let (start_line, end_line) = parse_plan_range(range)?;
                        let text = parts.collect::<Vec<_>>().join(" ");
                        if text.is_empty() {
                            return Err(CommandError::new(
                                maestro_ui::localization::tr("Usage: /plan comment <line|start-end> <text>"),
                            ));
                        }
                        Ok(CommandOutput::Action(CommandAction::PlanReview(
                            PlanReviewAction::Comment {
                                start_line,
                                end_line,
                                text,
                            },
                        )))
                    }
                    "resolve" | "reopen" => {
                        let id = parts
                            .next()
                            .ok_or_else(|| CommandError::new(maestro_ui::localization::tr("Usage: /plan resolve|reopen <id>")))?
                            .trim_start_matches('#')
                            .parse::<u64>()
                            .map_err(|_| CommandError::new(maestro_ui::localization::tr("Plan comment id must be a number")))?;
                        let action = if subcommand == "resolve" {
                            PlanReviewAction::Resolve { id }
                        } else {
                            PlanReviewAction::Reopen { id }
                        };
                        Ok(CommandOutput::Action(CommandAction::PlanReview(action)))
                    }
                    _ => Err(CommandError::new(maestro_ui::localization::tr("Usage: /plan [on|off|approve|view|comments|comment <range> <text>|resolve <id>|reopen <id>]"))),
                }
            }),
        ).localized()
        .usage("/plan [on|off|approve|view|comments|comment <range> <text>|resolve <id>|reopen <id>]").primary(5)
    );

    registry.register(
        Command::new(
            "view-plan",
            maestro_ui::localization::tr("Show the current session plan.md"),
            CommandCategory::Context,
            Box::new(|_| Ok(CommandOutput::Action(CommandAction::ViewPlan))),
        )
        .localized()
        .alias("show-plan")
        .alias("plan-view")
        .usage("/view-plan"),
    );

    // Grok-style permission shortcuts
    registry.register(
        Command::new(
            "always-approve",
            maestro_ui::localization::tr("Auto-approve all tool executions (YOLO)"),
            CommandCategory::Safety,
            Box::new(|_| {
                Ok(CommandOutput::Action(CommandAction::SetApprovalMode(
                    "yolo".to_string(),
                )))
            }),
        )
        .localized()
        .alias("yolo"),
    );
    registry.register(
        Command::new(
            "auto",
            maestro_ui::localization::tr("Selective approvals (safe tools free, risky prompt)"),
            CommandCategory::Safety,
            Box::new(|_| {
                Ok(CommandOutput::Action(CommandAction::SetApprovalMode(
                    "selective".to_string(),
                )))
            }),
        )
        .localized(),
    );
    registry.register(
        Command::new(
            "ask",
            maestro_ui::localization::tr("Require approval for all tools"),
            CommandCategory::Safety,
            Box::new(|_| {
                Ok(CommandOutput::Action(CommandAction::SetApprovalMode(
                    "safe".to_string(),
                )))
            }),
        )
        .localized(),
    );

    // Continue command
    registry.register(
        Command::new(
            "continue",
            maestro_ui::localization::tr("Continue the most recent session for this workspace"),
            CommandCategory::Session,
            Box::new(|_| {
                Ok(CommandOutput::Action(CommandAction::Session(
                    SessionAction::Continue,
                )))
            }),
        )
        .localized()
        .alias("c")
        .usage("/continue"),
    );

    // Resume command
    registry.register(
        Command::new(
            "resume",
            maestro_ui::localization::tr("Browse and resume saved conversations"),
            CommandCategory::Session,
            Box::new(|_| Ok(CommandOutput::OpenModal(ModalType::SessionList))),
        )
        .localized()
        .alias("r")
        .primary(1),
    );

    // Cost/usage command
    registry.register(
        Command::new(
            "cost",
            maestro_ui::localization::tr("Show token usage and cost statistics"),
            CommandCategory::Diagnostics,
            Box::new(|ctx| {
                let subcommand = ctx.raw_args.trim().to_lowercase();
                let action = match subcommand.as_str() {
                    "" | "summary" => UsageAction::Summary,
                    "detailed" | "detail" | "full" => UsageAction::Detailed,
                    "reset" | "clear" => UsageAction::Reset,
                    other => {
                        return Err(CommandError::new(maestro_ui::localization::format(
                            "Unknown cost subcommand: {0}",
                            &[(other).to_string()],
                        ))
                        .with_hint(maestro_ui::localization::tr(
                            "Available: summary, detailed, reset",
                        )));
                    }
                };
                Ok(CommandOutput::Action(CommandAction::ShowUsage(action)))
            }),
        )
        .localized()
        .alias("usage")
        .alias("tokens")
        .arg(CommandArgument::choice(
            "action",
            maestro_ui::localization::tr("What to show"),
            vec!["summary", "detailed", "reset"],
        ))
        .usage("/cost [summary|detailed|reset]"),
    );

    registry.register(
        Command::new(
            "bug",
            maestro_ui::localization::tr("Draft, review, or send a product bug report"),
            CommandCategory::Session,
            Box::new(|ctx| Ok(CommandOutput::Action(CommandAction::BugReport(
                if ctx.raw_args.trim().is_empty() && ctx.command_name == "bug" { "compose".into() } else { ctx.raw_args.clone() }
            )))),
        ).localized()
        .alias("feedback")
        .usage("/bug [description|queue|draft <text>|expected <text>|repro <steps>|review|send|export|dismiss|diagnostics on|off|rating useful|partly_useful|not_useful]"),
    );

    // Export command
    registry.register(
        Command::new(
            "export",
            maestro_ui::localization::tr("Export current session to file"),
            CommandCategory::Session,
            Box::new(|ctx| {
                let parts: Vec<&str> = ctx.raw_args.split_whitespace().collect();
                let format = parts.first().map(|s| s.to_lowercase());
                let path = parts.get(1).map(|s| (*s).to_string());

                let action = match format.as_deref() {
                    None | Some("") => ExportAction::ShowOptions,
                    Some("md" | "markdown") => ExportAction::Markdown(path),
                    Some("html") => ExportAction::Html(path),
                    Some("json") => ExportAction::Json(path),
                    Some("txt" | "text") => ExportAction::PlainText(path),
                    Some(other) => {
                        return Err(CommandError::new(maestro_ui::localization::format(
                            "Unknown export format: {0}",
                            &[(other).to_string()],
                        ))
                        .with_hint(maestro_ui::localization::tr(
                            "Available: markdown, html, json, text",
                        )));
                    }
                };
                Ok(CommandOutput::Action(CommandAction::ExportSession(action)))
            }),
        )
        .localized()
        .arg(CommandArgument::choice(
            "format",
            maestro_ui::localization::tr("Export format"),
            vec!["markdown", "html", "json", "text"],
        ))
        .arg(CommandArgument::string(
            "path",
            maestro_ui::localization::tr("Output file path"),
        ))
        .usage("/export [format] [path]"),
    );

    // History command
    registry.register(
        Command::new(
            "history",
            maestro_ui::localization::tr("Show or search prompt history"),
            CommandCategory::Session,
            Box::new(|ctx| {
                let args = ctx.raw_args.trim();
                let args_lower = args.to_lowercase();

                let action = if args.is_empty() {
                    HistoryAction::Recent(20)
                } else if args_lower == "clear" {
                    HistoryAction::Clear
                } else if let Ok(n) = args.parse::<usize>() {
                    HistoryAction::Recent(n)
                } else {
                    HistoryAction::Search(args.to_string())
                };

                Ok(CommandOutput::Action(CommandAction::ShowHistory(action)))
            }),
        )
        .localized()
        .alias("hist")
        .arg(CommandArgument::string(
            "query",
            maestro_ui::localization::tr("Number of entries or search query"),
        ))
        .usage("/history [count|search query|clear]"),
    );

    // Tool history command
    registry.register(
        Command::new(
            "toolhistory",
            maestro_ui::localization::tr("Show tool execution history and statistics"),
            CommandCategory::Tools,
            Box::new(|ctx| {
                let raw_args = ctx.raw_args.trim();
                let raw_parts: Vec<&str> = raw_args.split_whitespace().collect();
                let raw_sub = raw_parts.first().copied().unwrap_or("");
                let sub = raw_sub.to_lowercase();
                let rest = raw_parts
                    .iter()
                    .skip(1)
                    .copied()
                    .collect::<Vec<_>>()
                    .join(" ");
                let rest_trimmed = rest.trim();

                let action = match sub.as_str() {
                    "" => ToolHistoryAction::Recent(10),
                    "stats" | "statistics" => ToolHistoryAction::Stats,
                    "clear" => ToolHistoryAction::Clear,
                    "tool" => {
                        let tool_name = rest_trimmed.to_string();
                        if tool_name.is_empty() {
                            return Err(CommandError::new(maestro_ui::localization::tr(
                                "Tool name required",
                            ))
                            .with_hint(maestro_ui::localization::tr(
                                "Usage: /toolhistory tool <name>",
                            )));
                        }
                        ToolHistoryAction::ForTool(tool_name)
                    }
                    _ => {
                        if let Ok(n) = raw_sub.parse::<usize>() {
                            ToolHistoryAction::Recent(n)
                        } else {
                            // Assume it's a tool name
                            ToolHistoryAction::ForTool(raw_sub.to_string())
                        }
                    }
                };

                Ok(CommandOutput::Action(CommandAction::ShowToolHistory(
                    action,
                )))
            }),
        )
        .localized()
        .alias("th")
        .arg(CommandArgument::string(
            "action",
            maestro_ui::localization::tr("Action or tool name"),
        ))
        .usage("/toolhistory [count|stats|clear|tool <name>]"),
    );

    // Skills command
    registry.register(
        Command::new(
            "skills",
            maestro_ui::localization::tr(
                "Manage skills (specialized behaviors from SKILL.md files)",
            ),
            CommandCategory::Tools,
            Box::new(|ctx| {
                let raw_args = ctx.raw_args.trim();
                let raw_parts: Vec<&str> = raw_args.split_whitespace().collect();
                let raw_sub = raw_parts.first().copied().unwrap_or("");
                let sub = raw_sub.to_lowercase();
                let rest = raw_parts
                    .iter()
                    .skip(1)
                    .copied()
                    .collect::<Vec<_>>()
                    .join(" ");
                let rest_trimmed = rest.trim();

                let action = match sub.as_str() {
                    "" | "list" => SkillsAction::List,
                    "reload" | "refresh" => SkillsAction::Reload,
                    "activate" | "enable" | "on" => {
                        let name = rest_trimmed.to_string();
                        if name.is_empty() {
                            return Err(CommandError::new(maestro_ui::localization::tr(
                                "Skill name required",
                            ))
                            .with_hint(maestro_ui::localization::tr(
                                "Usage: /skills activate <skill-name>",
                            )));
                        }
                        SkillsAction::Activate(name)
                    }
                    "deactivate" | "disable" | "off" => {
                        let name = rest_trimmed.to_string();
                        if name.is_empty() {
                            return Err(CommandError::new(maestro_ui::localization::tr(
                                "Skill name required",
                            ))
                            .with_hint(maestro_ui::localization::tr(
                                "Usage: /skills deactivate <skill-name>",
                            )));
                        }
                        SkillsAction::Deactivate(name)
                    }
                    "info" | "show" => {
                        let name = rest_trimmed.to_string();
                        if name.is_empty() {
                            return Err(CommandError::new(maestro_ui::localization::tr(
                                "Skill name required",
                            ))
                            .with_hint(maestro_ui::localization::tr(
                                "Usage: /skills info <skill-name>",
                            )));
                        }
                        SkillsAction::Info(name)
                    }
                    _ => {
                        // Treat unknown as skill name for info
                        if raw_args.is_empty() {
                            SkillsAction::List
                        } else {
                            SkillsAction::Info(raw_sub.to_string())
                        }
                    }
                };

                Ok(CommandOutput::Action(CommandAction::Skills(action)))
            }),
        )
        .localized()
        .alias("skill")
        .arg(CommandArgument::string(
            "action",
            "list|activate|deactivate|reload|info",
        ))
        .arg(CommandArgument::string(
            "name",
            maestro_ui::localization::tr("Skill name"),
        ))
        .usage("/skills [list|activate|deactivate|reload|info] [skill-name]"),
    );

    // Plugins command
    registry.register(
        Command::new(
            "plugins",
            maestro_ui::localization::tr("List plugins, marketplace catalog, install/reload"),
            CommandCategory::Tools,
            Box::new(|ctx| {
                let raw_args = ctx.raw_args.trim();
                let raw_parts: Vec<&str> = raw_args.split_whitespace().collect();
                let raw_sub = raw_parts.first().copied().unwrap_or("");
                let sub = raw_sub.to_lowercase();

                let action = match sub.as_str() {
                    "" | "list" => PluginsAction::List,
                    "reload" | "refresh" => PluginsAction::Reload,
                    "marketplace" | "market" | "catalog" => {
                        let rest: Vec<&str> = raw_parts.iter().skip(1).copied().collect();
                        match rest.first().map(|s| s.to_ascii_lowercase()).as_deref() {
                            None | Some("list" | "ls") => PluginsAction::MarketplaceList,
                            Some("install") => {
                                let id = rest.get(1).copied().unwrap_or("").trim();
                                if id.is_empty() {
                                    return Err(CommandError::new(maestro_ui::localization::tr(
                                        "Usage: /plugins marketplace install <id> [--trust]",
                                    )));
                                }
                                let trust = rest
                                    .iter()
                                    .any(|a| a.eq_ignore_ascii_case("--trust") || *a == "-t");
                                PluginsAction::MarketplaceInstall {
                                    id: id.to_string(),
                                    trust,
                                }
                            }
                            Some(other) => {
                                return Err(CommandError::new(maestro_ui::localization::format(
                                    "Unknown marketplace subcommand: {0}",
                                    &[(other).to_string()],
                                ))
                                .with_hint(
                                    maestro_ui::localization::tr(
                                        "Usage: /plugins marketplace [list|install <id> [--trust]]",
                                    ),
                                ));
                            }
                        }
                    }
                    "info" | "show" => {
                        let name = raw_parts
                            .iter()
                            .skip(1)
                            .copied()
                            .collect::<Vec<_>>()
                            .join(" ");
                        let name = name.trim();
                        if name.is_empty() {
                            return Err(CommandError::new(maestro_ui::localization::tr(
                                "Plugin name required",
                            ))
                            .with_hint(maestro_ui::localization::tr(
                                "Usage: /plugins info <plugin-name>",
                            )));
                        }
                        PluginsAction::Info(name.to_string())
                    }
                    _ => {
                        // Treat bare name as info lookup: `/plugins team-tools`
                        PluginsAction::Info(raw_sub.to_string())
                    }
                };

                Ok(CommandOutput::Action(CommandAction::Plugins(action)))
            }),
        )
        .localized()
        .alias("plugin")
        .arg(CommandArgument::string(
            "action",
            maestro_ui::localization::tr("list|info|reload|marketplace or plugin name"),
        ))
        .usage("/plugins [list|info|reload|marketplace [list|install <id> [--trust]]]"),
    );

    registry
}

fn parse_goal_action(raw: &str) -> Result<GoalAction, CommandError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("status") {
        return Ok(GoalAction::Status);
    }
    let mut parts = trimmed.split_whitespace();
    let sub = parts.next().unwrap_or("").to_ascii_lowercase();
    let rest = parts.collect::<Vec<_>>().join(" ");
    match sub.as_str() {
        "create" | "set" | "start" => {
            let (text, max_turns, token_budget, max_duration_secs) =
                crate::goal::strip_goal_flags_with_duration(&rest).map_err(CommandError::new)?;
            if text.is_empty() {
                return Err(CommandError::new(maestro_ui::localization::tr(
                    "Usage: /goal create [--max-turns N] [--token-budget N] [--max-duration-secs N] <text>",
                )));
            }
            Ok(GoalAction::Create {
                text,
                replace: false,
                criteria: None,
                max_turns,
                token_budget,
                max_duration_secs,
            })
        }
        "replace" => {
            let (text, max_turns, token_budget, max_duration_secs) =
                crate::goal::strip_goal_flags_with_duration(&rest).map_err(CommandError::new)?;
            if text.is_empty() {
                return Err(CommandError::new(maestro_ui::localization::tr(
                    "Usage: /goal replace [--max-turns N] [--token-budget N] [--max-duration-secs N] <text>",
                )));
            }
            Ok(GoalAction::Create {
                text,
                replace: true,
                criteria: None,
                max_turns,
                token_budget,
                max_duration_secs,
            })
        }
        "pause" => Ok(GoalAction::Pause),
        "resume" | "continue" => Ok(GoalAction::Resume),
        "block" => Ok(GoalAction::Block {
            reason: (!rest.is_empty()).then_some(rest),
        }),
        "complete" | "done" => Ok(GoalAction::Complete),
        "clear" | "cancel" => Ok(GoalAction::Clear),
        "auto" => {
            let flag = rest.trim().to_ascii_lowercase();
            let enabled = match flag.as_str() {
                "on" | "true" | "1" | "enable" | "enabled" => true,
                "off" | "false" | "0" | "disable" | "disabled" => false,
                _ => {
                    return Err(CommandError::new(maestro_ui::localization::tr(
                        "Usage: /goal auto on|off",
                    )));
                }
            };
            Ok(GoalAction::AutoContinue { enabled })
        }
        // Bare text → create without replace (may include flags).
        _ => {
            let (text, max_turns, token_budget, max_duration_secs) =
                crate::goal::strip_goal_flags_with_duration(trimmed).map_err(CommandError::new)?;
            if text.is_empty() {
                return Err(CommandError::new(maestro_ui::localization::tr(
                    "Usage: /goal create [--max-turns N] [--token-budget N] [--max-duration-secs N] <text>",
                )));
            }
            Ok(GoalAction::Create {
                text,
                replace: false,
                criteria: None,
                token_budget,
                max_turns,
                max_duration_secs,
            })
        }
    }
}

fn parse_harness_action(raw: &str) -> Result<HarnessAction, CommandError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("status") {
        return Ok(HarnessAction::Status);
    }
    if trimmed.eq_ignore_ascii_case("list") {
        return Ok(HarnessAction::List);
    }

    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let subcommand = parts.next().unwrap_or_default().to_ascii_lowercase();
    let rest = parts.next().unwrap_or_default().trim();
    match subcommand.as_str() {
        "review" | "proposals" => Ok(HarnessAction::Review),
        "propose" | "suggest" => {
            let mut fields = rest.splitn(4, char::is_whitespace);
            let scope = fields.next().unwrap_or_default();
            let kind = fields.next().unwrap_or_default();
            let name = fields.next().unwrap_or_default();
            let content = fields.next().unwrap_or_default().trim();
            if scope.is_empty() || kind.is_empty() || name.is_empty() || content.is_empty() {
                return Err(CommandError::new(maestro_ui::localization::tr(
                    "Usage: /refine propose <scope> <kind> <name> <content> --evidence <text>",
                )));
            }
            let (content, evidence) = split_harness_evidence(content);
            let Some(evidence) = evidence else {
                return Err(CommandError::new(maestro_ui::localization::tr(
                    "Refinement proposals require --evidence <text>.",
                )));
            };
            if content.is_empty() {
                return Err(CommandError::new(maestro_ui::localization::tr(
                    "Refinement content is required before --evidence.",
                )));
            }
            Ok(HarnessAction::Propose {
                scope: scope.to_string(),
                kind: kind.to_string(),
                name: name.to_string(),
                content,
                evidence,
            })
        }
        "add" | "create" => {
            let mut fields = rest.splitn(4, char::is_whitespace);
            let scope = fields.next().unwrap_or_default();
            let kind = fields.next().unwrap_or_default();
            let name = fields.next().unwrap_or_default();
            let content = fields.next().unwrap_or_default().trim();
            if scope.is_empty() || kind.is_empty() || name.is_empty() || content.is_empty() {
                return Err(CommandError::new(maestro_ui::localization::tr(
                    "Usage: /harness add <scope> <kind> <name> <content> [--evidence <text>]",
                )));
            }
            let (content, evidence) = split_harness_evidence(content);
            if content.is_empty() {
                return Err(CommandError::new(maestro_ui::localization::tr(
                    "Harness content is required before --evidence",
                )));
            }
            Ok(HarnessAction::Add {
                scope: scope.to_string(),
                kind: kind.to_string(),
                name: name.to_string(),
                content,
                evidence,
            })
        }
        "update" | "set" => {
            let mut fields = rest.splitn(2, char::is_whitespace);
            let id = fields.next().unwrap_or_default();
            let content = fields.next().unwrap_or_default().trim();
            if id.is_empty() || content.is_empty() {
                return Err(CommandError::new(maestro_ui::localization::tr(
                    "Usage: /harness update <id> <content> [--evidence <text>]",
                )));
            }
            let (content, evidence) = split_harness_evidence(content);
            if content.is_empty() {
                return Err(CommandError::new(maestro_ui::localization::tr(
                    "Harness content is required before --evidence",
                )));
            }
            Ok(HarnessAction::Update {
                id: id.to_string(),
                content,
                evidence,
            })
        }
        "delete" | "remove" => {
            if rest.is_empty() {
                return Err(CommandError::new(maestro_ui::localization::tr(
                    "Usage: /harness delete <id>",
                )));
            }
            Ok(HarnessAction::Delete(rest.to_string()))
        }
        "apply" | "accept" => {
            if rest.is_empty() {
                return Err(CommandError::new(maestro_ui::localization::tr(
                    "Usage: /refine apply <proposal-id>",
                )));
            }
            Ok(HarnessAction::Apply(rest.to_string()))
        }
        "reject" | "decline" => {
            let mut fields = rest.splitn(2, char::is_whitespace);
            let id = fields.next().unwrap_or_default();
            let note = fields
                .next()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned);
            if id.is_empty() {
                return Err(CommandError::new(maestro_ui::localization::tr(
                    "Usage: /refine reject <proposal-id> [note]",
                )));
            }
            Ok(HarnessAction::Reject {
                id: id.to_string(),
                note,
            })
        }
        "rollback" | "restore" => {
            let revision = rest.parse::<u64>().map_err(|_| {
                CommandError::new(maestro_ui::localization::tr(
                    "Usage: /harness rollback <revision> (revision must be numeric)",
                ))
            })?;
            Ok(HarnessAction::Rollback(revision))
        }
        _ => Err(CommandError::new(maestro_ui::localization::format(
            "Unknown harness action '{0}'. Use status, list, review, propose, add, update, apply, reject, delete, or rollback.",
            std::slice::from_ref(&(subcommand)),
        ))),
    }
}

fn split_harness_evidence(content: &str) -> (String, Option<String>) {
    let Some((content, evidence)) = content.split_once(" --evidence ") else {
        return (content.trim().to_string(), None);
    };
    let evidence = evidence.trim();
    (
        content.trim().to_string(),
        (!evidence.is_empty()).then(|| evidence.to_string()),
    )
}

fn parse_rlm_action(raw: &str) -> Result<RlmAction, CommandError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("list") {
        return Ok(RlmAction::List);
    }
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let subcommand = parts.next().unwrap_or_default().to_ascii_lowercase();
    let rest = parts.next().unwrap_or_default().trim();
    match subcommand.as_str() {
        "set" => {
            let mut fields = rest.splitn(2, char::is_whitespace);
            let name = fields.next().unwrap_or_default();
            let value = fields.next().unwrap_or_default().trim();
            let (value, description) = split_rlm_description(value);
            if name.is_empty() || value.is_empty() {
                return Err(CommandError::new(maestro_ui::localization::tr(
                    "Usage: /rlm set <name> <value> [--description <text>]",
                )));
            }
            Ok(RlmAction::Set {
                name: name.to_string(),
                value,
                description,
            })
        }
        "append" => {
            let mut fields = rest.splitn(2, char::is_whitespace);
            let name = fields.next().unwrap_or_default();
            let value = fields.next().unwrap_or_default().trim();
            if name.is_empty() || value.is_empty() {
                return Err(CommandError::new(maestro_ui::localization::tr(
                    "Usage: /rlm append <name> <value>",
                )));
            }
            Ok(RlmAction::Append {
                name: name.to_string(),
                value: value.to_string(),
            })
        }
        "render" | "expand" => {
            if rest.is_empty() {
                return Err(CommandError::new(maestro_ui::localization::tr(
                    "Usage: /rlm render <template>",
                )));
            }
            Ok(RlmAction::Render(rest.to_string()))
        }
        "clear" | "delete" | "remove" => {
            if rest.is_empty() {
                return Err(CommandError::new(maestro_ui::localization::tr(
                    "Usage: /rlm clear <name>",
                )));
            }
            Ok(RlmAction::Clear(rest.to_string()))
        }
        _ => Err(CommandError::new(maestro_ui::localization::tr(
            "Usage: /rlm list|set|append|render|clear",
        ))),
    }
}

fn split_rlm_description(value: &str) -> (String, Option<String>) {
    let Some((value, description)) = value.split_once(" --description ") else {
        return (value.trim().to_string(), None);
    };
    let description = description.trim();
    (
        value.trim().to_string(),
        (!description.is_empty()).then(|| description.to_string()),
    )
}

fn parse_mailbox_action(raw: &str) -> Result<MailboxAction, CommandError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("list") {
        return Ok(MailboxAction::List);
    }
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let subcommand = parts.next().unwrap_or_default().to_ascii_lowercase();
    let rest = parts.next().unwrap_or_default().trim();
    match subcommand.as_str() {
        "send" => {
            let mut fields = rest.splitn(2, char::is_whitespace);
            let recipient = fields.next().unwrap_or_default();
            let body = fields.next().unwrap_or_default().trim();
            if recipient.is_empty() || body.is_empty() {
                return Err(CommandError::new(maestro_ui::localization::tr(
                    "Usage: /mailbox send <recipient> <message>",
                )));
            }
            Ok(MailboxAction::Send {
                recipient: recipient.to_string(),
                body: body.to_string(),
            })
        }
        "read" => {
            if rest.is_empty() {
                return Err(CommandError::new(maestro_ui::localization::tr(
                    "Usage: /mailbox read <id>",
                )));
            }
            Ok(MailboxAction::Read(rest.to_string()))
        }
        "inspect" => {
            if rest.is_empty() {
                return Err(CommandError::new(maestro_ui::localization::tr(
                    "Usage: /mailbox inspect <id>",
                )));
            }
            Ok(MailboxAction::Inspect(rest.to_string()))
        }
        "ack" | "acknowledge" => {
            if rest.is_empty() {
                return Err(CommandError::new(maestro_ui::localization::tr(
                    "Usage: /mailbox ack <id>",
                )));
            }
            Ok(MailboxAction::Acknowledge(rest.to_string()))
        }
        "approve" => {
            if rest.is_empty() {
                return Err(CommandError::new(maestro_ui::localization::tr(
                    "Usage: /mailbox approve <id>",
                )));
            }
            Ok(MailboxAction::Approve(rest.to_string()))
        }
        "compact" | "clear" => Ok(MailboxAction::Compact),
        _ => Err(CommandError::new(maestro_ui::localization::tr(
            "Usage: /mailbox list|send|read|inspect|ack|approve|compact",
        ))),
    }
}

/// Built-ins + Grok-style skill/prompt slash extensions. Built-ins always win.
#[must_use]
pub fn build_command_registry_with_extensions(
    skills: &[crate::skills::LoadedSkill],
    prompts: &[crate::prompts::PromptDefinition],
) -> CommandRegistry {
    let mut registry = build_command_registry();

    for prompt in prompts {
        let name = prompt.name.clone();
        let desc = prompt.description.clone().unwrap_or_else(|| {
            maestro_ui::localization::format(
                "Prompt template ({0})",
                &[(prompt.source_type.as_str()).to_string()],
            )
        });
        let usage = crate::prompts::get_usage_hint(prompt).replace("/prompts:", "/");
        let name_for_handler = name.clone();
        registry.register_if_absent(
            Command::new(
                name.clone(),
                desc,
                CommandCategory::Tools,
                Box::new(move |ctx| {
                    Ok(CommandOutput::Action(CommandAction::InvokePromptTemplate {
                        name: name_for_handler.clone(),
                        args: ctx.raw_args.clone(),
                    }))
                }),
            )
            .usage(usage),
        );
    }

    for skill in skills {
        if !skill.definition.enabled || !skill.definition.user_invocable {
            continue;
        }
        let name = skill.definition.name.clone();
        let desc = if skill.definition.description.is_empty() {
            maestro_ui::localization::format("Skill: {0}", std::slice::from_ref(&(name)))
        } else {
            skill.definition.description.clone()
        };
        let hint = skill
            .definition
            .metadata
            .get("argument-hint")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let usage = match hint {
            Some(h) if !h.is_empty() => format!("/{name} {h}"),
            _ => format!("/{name} [args...]"),
        };
        let name_for_handler = name.clone();
        registry.register_if_absent(
            Command::new(
                name.clone(),
                desc,
                CommandCategory::Tools,
                Box::new(move |ctx| {
                    Ok(CommandOutput::Action(CommandAction::InvokeSkill {
                        name: name_for_handler.clone(),
                        args: ctx.raw_args.clone(),
                    }))
                }),
            )
            .usage(usage),
        );
    }

    registry
}

/// Register Droid-style executable script commands as slash commands.
///
/// Scripts are registered with `register_if_absent`, so built-in commands and
/// already-registered skill/prompt extensions always win name collisions.
/// Returns the names that were skipped due to a collision so the caller can
/// warn the user once.
pub fn register_exec_commands(
    registry: &mut CommandRegistry,
    exec_commands: &[crate::exec_commands::ExecCommand],
) -> Vec<String> {
    let mut skipped = Vec::new();
    for exec in exec_commands {
        let name = exec.name.clone();
        let description = maestro_ui::localization::format(
            "Executable command ({0}, `{1}`)",
            &[
                (exec.source.as_str()).to_string(),
                (exec.path.display()).to_string(),
            ],
        );
        let usage = format!("/{name} [args...]");
        let name_for_handler = name.clone();
        let registered = registry.register_if_absent(
            Command::new(
                name.clone(),
                description,
                CommandCategory::Tools,
                Box::new(move |ctx| {
                    Ok(CommandOutput::Action(CommandAction::InvokeExecCommand {
                        name: name_for_handler.clone(),
                        args: ctx.raw_args.clone(),
                    }))
                }),
            )
            .usage(usage),
        );
        if !registered {
            skipped.push(name);
        }
    }
    skipped
}

fn build_diag_about(ctx: &CommandContext) -> String {
    let version = env!("CARGO_PKG_VERSION");
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let cwd = ctx.cwd.clone();
    let branch = git::current_branch(Path::new(&ctx.cwd))
        .unwrap_or_else(|| maestro_ui::localization::tr("(not a repo)").to_string());
    let session = ctx
        .session_id
        .clone()
        .unwrap_or_else(|| "(ephemeral)".to_string());
    let model = ctx
        .model
        .clone()
        .unwrap_or_else(|| maestro_ui::localization::tr("(unknown)").to_string());

    let mut lines = Vec::new();
    lines.push("## About".to_string());
    lines.push(String::new());
    lines.push(format!("**Version:** {version}"));
    lines.push(format!("**OS:** {os}/{arch}"));
    lines.push(format!("**CWD:** {cwd}"));
    lines.push(maestro_ui::localization::format(
        "**Session:** {0}",
        std::slice::from_ref(&(session)),
    ));
    lines.push(maestro_ui::localization::format(
        "**Model:** {0}",
        std::slice::from_ref(&(model)),
    ));
    lines.push(format!("**Git:** {branch}"));
    lines.join("\n")
}

fn build_diag_context(ctx: &CommandContext) -> String {
    let session = ctx
        .session_id
        .clone()
        .unwrap_or_else(|| "(ephemeral)".to_string());
    let model = ctx
        .model
        .clone()
        .unwrap_or_else(|| maestro_ui::localization::tr("(unknown)").to_string());

    let mut lines = Vec::new();
    lines.push(maestro_ui::localization::tr("## Context").to_string());
    lines.push(String::new());
    lines.push(maestro_ui::localization::format(
        "**Model:** {0}",
        std::slice::from_ref(&(model)),
    ));
    lines.push(maestro_ui::localization::format(
        "**Session:** {0}",
        std::slice::from_ref(&(session)),
    ));
    lines.push(format!("**CWD:** {}", ctx.cwd));
    lines.push(String::new());
    lines.push(
        maestro_ui::localization::tr("Use /context for a token breakdown of the current session.")
            .to_string(),
    );
    lines.join("\n")
}

fn git_help_message() -> String {
    let mut msg = String::new();
    msg.push_str(maestro_ui::localization::tr("Git Commands:\n"));
    msg.push_str(maestro_ui::localization::tr(
        "  /git                 Show git status summary\n",
    ));
    msg.push_str(maestro_ui::localization::tr(
        "  /git status          Show git status\n",
    ));
    msg.push_str(maestro_ui::localization::tr(
        "  /git diff [path]     Show diff for file\n",
    ));
    msg.push_str(maestro_ui::localization::tr(
        "  /git review          Summarize status and diff stats\n\n",
    ));
    msg.push_str(maestro_ui::localization::tr(
        "Direct shortcuts still work: /diff, /review",
    ));
    msg
}

fn build_git_status_message(cwd: &str) -> String {
    let cwd_path = Path::new(cwd);
    if !git::is_git_repo(cwd_path) {
        return maestro_ui::localization::tr("Not a git repository.").to_string();
    }
    match git::status_short(cwd_path) {
        Ok(status) => {
            if status.is_empty() {
                return maestro_ui::localization::tr("Working tree clean.").to_string();
            }
            if is_clean_status(&status) {
                if let Some(branch_line) = status.lines().next() {
                    return maestro_ui::localization::format(
                        "## Git Status\n\n```\n{0}\n```\n\nWorking tree clean.",
                        &[(branch_line).to_string()],
                    );
                }
                return maestro_ui::localization::tr("Working tree clean.").to_string();
            }
            maestro_ui::localization::format(
                "## Git Status\n\n```\n{0}\n```",
                std::slice::from_ref(&(status)),
            )
        }
        Err(err) => {
            maestro_ui::localization::format("Git status failed: {0}", std::slice::from_ref(&(err)))
        }
    }
}

fn build_git_review_message(cwd: &str) -> String {
    let cwd_path = Path::new(cwd);
    if !git::is_git_repo(cwd_path) {
        return maestro_ui::localization::tr("Not a git repository.").to_string();
    }

    let status = git::status_short(cwd_path).unwrap_or_else(|err| {
        maestro_ui::localization::format("git status failed: {0}", std::slice::from_ref(&(err)))
    });
    let staged = git::diff_stat(cwd_path, true).unwrap_or_else(|err| {
        maestro_ui::localization::format(
            "git diff --cached --stat failed: {0}",
            std::slice::from_ref(&(err)),
        )
    });
    let worktree = git::diff_stat(cwd_path, false).unwrap_or_else(|err| {
        maestro_ui::localization::format(
            "git diff --stat failed: {0}",
            std::slice::from_ref(&(err)),
        )
    });

    let status_display = if status.is_empty() {
        maestro_ui::localization::tr("Working tree clean.").to_string()
    } else if is_clean_status(&status) {
        let mut display = String::new();
        if let Some(branch_line) = status.lines().next() {
            display.push_str(branch_line);
            display.push('\n');
        }
        display.push_str(maestro_ui::localization::tr("Working tree clean."));
        display
    } else {
        status.clone()
    };

    let mut msg = String::from(maestro_ui::localization::tr("## Git Review\n\n"));
    msg.push_str("**Status:**\n```\n");
    msg.push_str(&status_display);
    msg.push_str("\n```\n\n");

    msg.push_str(maestro_ui::localization::tr("**Staged diff stats:**\n"));
    if staged.is_empty() {
        msg.push_str(maestro_ui::localization::tr("No staged changes.\n\n"));
    } else {
        msg.push_str("```\n");
        msg.push_str(&staged);
        msg.push_str("\n```\n\n");
    }

    msg.push_str(maestro_ui::localization::tr("**Worktree diff stats:**\n"));
    if worktree.is_empty() {
        msg.push_str(maestro_ui::localization::tr("No unstaged changes."));
    } else {
        msg.push_str("```\n");
        msg.push_str(&worktree);
        msg.push_str("\n```");
    }

    msg
}

fn is_clean_status(status: &str) -> bool {
    let mut lines = status.lines();
    let _ = lines.next();
    lines.all(|line| line.trim().is_empty())
}

fn build_git_diff_message(cwd: &str, path: Option<&str>) -> String {
    let cwd_path = Path::new(cwd);
    if !git::is_git_repo(cwd_path) {
        return maestro_ui::localization::tr("Not a git repository.").to_string();
    }

    match git::diff(cwd_path, path) {
        Ok(diff) => {
            if diff.is_empty() {
                return maestro_ui::localization::tr("No unstaged changes.").to_string();
            }
            let (truncated, was_truncated) = truncate_text(&diff, 200, 20_000);
            let mut msg = String::from(maestro_ui::localization::tr("## Git Diff\n\n```diff\n"));
            msg.push_str(&truncated);
            msg.push_str("\n```");
            if was_truncated {
                msg.push_str(maestro_ui::localization::tr(
                    "\n\n(Truncated. Run git diff in your shell for full output.)",
                ));
            }
            msg
        }
        Err(err) => {
            maestro_ui::localization::format("Git diff failed: {0}", std::slice::from_ref(&(err)))
        }
    }
}

fn truncate_text(text: &str, max_lines: usize, max_chars: usize) -> (String, bool) {
    if text.is_empty() {
        return (String::new(), false);
    }

    let mut out = String::new();
    let mut total_chars = 0usize;
    let mut lines_used = 0usize;
    let mut truncated = false;

    for line in text.lines() {
        if lines_used >= max_lines {
            truncated = true;
            break;
        }

        if !out.is_empty() {
            out.push('\n');
            total_chars += 1;
        }

        let line_len = line.chars().count();
        if total_chars + line_len > max_chars {
            let remaining = max_chars.saturating_sub(total_chars);
            if remaining > 0 {
                out.push_str(&line.chars().take(remaining).collect::<String>());
            }
            truncated = true;
            break;
        }

        out.push_str(line);
        total_chars += line_len;
        lines_used += 1;
    }

    if !truncated {
        let total_lines = text.lines().count();
        let total_chars_all = text.chars().count();
        if total_lines > lines_used || total_chars_all > total_chars {
            truncated = true;
        }
    }

    (out, truncated)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod menu_contract_tests {
    use super::*;
    #[test]
    fn primary_menu_and_compatibility_commands_share_one_registry() {
        let registry = build_command_registry();
        let names: Vec<_> = registry
            .primary_commands()
            .iter()
            .map(|c| c.name.clone())
            .collect();
        assert_eq!(
            names,
            [
                "new", "resume", "fork", "rewind", "model", "plan", "review", "tasks", "settings",
                "context", "help", "quit"
            ]
        );
        assert_eq!(registry.get("clear").unwrap().name, "new");
        for name in [
            "always-approve",
            "auto",
            "ask",
            "prompt-audit",
            "mcp-config",
            "magic-trace",
            "view-plan",
            "stats",
            "toolhistory",
            "limits",
            "harness",
            "rlm",
        ] {
            assert!(registry.get(name).unwrap().browse_order.is_none(), "{name}");
        }
    }
    #[test]
    fn bare_settings_open_without_mutating() {
        let registry = build_command_registry();
        for input in [
            "/approvals",
            "/footer",
            "/thinking",
            "/model",
            "/settings",
            "/context",
        ] {
            assert!(
                matches!(
                    registry.execute(input, "/tmp", None, None).unwrap(),
                    CommandOutput::Action(CommandAction::OpenPanel(_))
                ),
                "{input}"
            );
        }
        assert!(
            matches!(registry.execute("/approvals safe", "/tmp", None, None).unwrap(), CommandOutput::Action(CommandAction::SetApprovalMode(mode)) if mode == "safe")
        );
        assert!(matches!(
            registry
                .execute("/footer solo", "/tmp", None, None)
                .unwrap(),
            CommandOutput::Action(CommandAction::SetFooterStyle(FooterStyle::Solo))
        ));
        assert!(
            registry
                .execute("/settings typo", "/tmp", None, None)
                .is_err()
        );
        assert!(
            registry
                .execute("/output typo", "/tmp", None, None)
                .is_err()
        );
    }
}

#[cfg(test)]
mod localization_regression_tests {
    use super::*;

    #[test]
    fn localization_keeps_command_identity_and_error_kind() {
        use crate::localization::{Locale, with_locale};
        for locale in Locale::ALL {
            with_locale(locale, || {
                let registry = build_command_registry();
                assert_eq!(registry.get("help").unwrap().name, "help");
                assert_eq!(
                    registry.get("help").unwrap().description,
                    "Show available commands"
                );
                let error = registry
                    .execute("/no-such-command-xyz", ".", None, None)
                    .unwrap_err();
                assert_eq!(error.kind, CommandErrorKind::UnknownCommand);
                assert!(error.message.contains("/no-such-command-xyz"));
                assert_eq!(
                    CommandError::new("Unknown command in custom validation").kind,
                    CommandErrorKind::InvalidInput
                );
            });
        }
    }
}
