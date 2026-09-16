//! # Native Maestro TUI Application
//!
//! This is the main entry point for the native Rust TUI. It coordinates all
//! the major subsystems: terminal rendering, input handling, agent communication,
//! and tool execution.
//!
//! ## Architecture Overview
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                           App                                   │
//! │  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────────────┐ │
//! │  │ AppState │  │ Terminal │  │  Agent   │  │ Tool Executor    │ │
//! │  │ (state)  │  │(ratatui) │  │ (async)  │  │ (bash, read, ..) │ │
//! │  └──────────┘  └──────────┘  └──────────┘  └──────────────────┘ │
//! │  ┌──────────────────────────────────────────────────────────────┐│
//! │  │                    Modals / Components                       ││
//! │  │  FileSearch, SessionSwitcher, CommandPalette, Approval, etc. ││
//! │  └──────────────────────────────────────────────────────────────┘│
//! └─────────────────────────────────────────────────────────────────┘
//! ```
//!
//! ## Rust Concepts Demonstrated
//!
//! - **Async Event Loop**: The `run()` method shows how to combine sync (terminal)
//!   and async (agent) operations using tokio.
//!
//! - **Message Passing**: Uses `mpsc` channels for agent communication, avoiding
//!   shared mutable state between async tasks.
//!
//! - **Ownership with Option**: Uses `Option<T>` for resources that may or may
//!   not be initialized (agent, channels).
//!
//! - **Pattern Matching for Input**: Handles keyboard input with exhaustive matching.

// ─────────────────────────────────────────────────────────────────────────────
// IMPORTS
// ─────────────────────────────────────────────────────────────────────────────

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
// `Arc` (Atomic Reference Counted) is a thread-safe reference-counted pointer.
// Multiple owners can share the same data. The data is freed when the last
// Arc is dropped. Unlike `Rc`, `Arc` is safe to use across threads.

use anyhow::{Context, Result, bail};
// `anyhow` provides ergonomic error handling:
// - `Result` is shorthand for `Result<T, anyhow::Error>`
// - `.context("msg")` adds context to errors for better debugging

use crossterm::event::{
    self, KeyCode, KeyEventKind, KeyModifiers as CrosstermModifiers, MouseEventKind,
};
// `crossterm` is a cross-platform terminal manipulation library.
// It handles raw mode, events, and cursor control across Windows/Mac/Linux.

use ratatui::prelude::*;
// `ratatui` is the terminal UI framework (fork of `tui-rs`).
// It provides widgets (Paragraph, Block, List) and layout primitives.

use tokio::sync::mpsc;
// `mpsc` = Multi-Producer, Single-Consumer channel.
// Used for async message passing between tasks.
// - `mpsc::unbounded_channel()` creates a channel with no size limit
// - Sender can be cloned (multiple producers)
// - Receiver cannot be cloned (single consumer)

use crate::agent::{
    CredentialVault, ExecutionSource, FromAgent, MaxTokensSource, NativeAgent, NativeAgentConfig,
    PromptKind, QueuePlacement, ToolExecution, ToolResponseMessage, ToolResult,
};
use crate::agent::{ExternalToolSchemaPolicy, MAX_PENDING_MESSAGES};
use crate::ai::AiProvider;
use crate::clipboard::ClipboardManager;
use crate::commands::{
    BackgroundMonitorAction, CommandAction, CommandOutput, CommandRegistry, FooterStyle,
    LoopAction, ModalType, PlanReviewAction, QueueAction, QueueModeKind, QueueMoveDirection,
    SlashCommandMatcher, SlashCycleState, build_command_registry_with_extensions,
};
use crate::components::{
    ApprovalController, ApprovalDecision, ApprovalModal, ApprovalModalKind, ApprovalRequest,
    BatchedApprovalModal, ChatInputWidget, ChatInputWidgetOptions, ChatView, CommandPalette,
    DetailView, FileSearchModal, McpManager, ModelSelector, OperationsModal, RewindPicker,
    SessionSwitcher, SetupAdvance, SetupModal, ShortcutsHelp, ThemeSelector, approval_modal_kind,
    calculate_input_height,
};
use crate::config_watcher::{ConfigEvent, ConfigWatcher, ConfigWatcherBuilder};
use crate::files::{WorkspaceFile, get_workspace_files};
use crate::git;
use crate::goal::GoalStore;
use crate::harness::HarnessStore;
use crate::keybindings::load_rust_tui_keybindings;
use crate::keybindings::{is_keybindings_config_path, summarize_keybindings_config_issues};
use crate::mailbox::MailboxStore;
#[cfg(test)]
use crate::mcp::{McpConfigScope, McpTransport};
use crate::mcp::{McpPrompt, McpRuntimeEvent, append_mcp_prompt_summary};
use crate::palette_resource::{PaletteResource, PaletteResourceKind};
use crate::plugins::PluginRegistry;
use crate::prompts::{PromptDefinition, parse_args, render_prompt};
use crate::rlm::RlmStore;
use crate::safety::{
    FirewallVerdict, check_model_allowed, check_path_allowed, check_session_limits,
    guardian::{GuardianError, GuardianVerdict},
};
use crate::session::{
    AppMessage, CompactionEntry, ContentBlock as SessionContentBlock, CustomEntry, MessageContent,
    MessageEntry, ModelChange, ParsedSession, PlanReviewComment, PlanReviewEntry, PlanReviewEvent,
    SessionEntry, SessionExporter, SessionHeader, SessionManager, SideQuestionEntry, ThinkingLevel,
    ThinkingLevelChange, TokenCost, TokenUsage as SessionTokenUsage, ToolInfo,
};
use crate::skills::{LoadedSkill, SkillLoadError, SkillLoader, SkillRegistry};
use crate::state::{AppState, Message, MessageKind, MessageRole, QueueMode};
use crate::sync_output::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use crate::terminal::{self, AppTerminalEvent, TerminalCapabilities, TerminalEventReader};
use crate::tools::{ToolExecutor, ToolRegistry};
use chrono::{Datelike, Utc};

use self::prompt_audit::{PromptAssembly, PromptAuditReport, PromptFragment};

// ─────────────────────────────────────────────────────────────────────────────
// TYPES
// ─────────────────────────────────────────────────────────────────────────────

/// A finished guardian review: the pending approval request plus the verdict
/// (or the failure that must fall back to the human modal).
type GuardianReviewOutcome = (ApprovalRequest, Result<GuardianVerdict, GuardianError>);

/// Active modal in the UI.
///
/// Only one modal can be active at a time. This enum tracks which one.
/// Modals are overlays that capture input (like dialogs in web apps).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveModal {
    /// No modal active - normal chat input
    None,
    /// File search modal (Ctrl+P style)
    FileSearch,
    /// Session history browser
    SessionSwitcher,
    /// Read-only persisted tool execution browser
    Operations,
    /// Interactive MCP server lifecycle manager
    McpManager,
    /// Command palette (Ctrl+Shift+P style)
    CommandPalette,
    /// Tool execution approval dialog
    Approval,
    /// AI model selector
    ModelSelector,
    /// Color theme selector
    ThemeSelector,
    /// Durable experiment preferences.
    Preferences,
    /// Persistent Dex cosmetics.
    DexAppearance,
    /// First-run EvalOps Identity and optional local API key setup
    Setup,
    /// Keyboard shortcuts help overlay
    ShortcutsHelp,
    /// File checkpoint restore picker (double-Esc on empty input)
    RewindPicker,
    /// Select and review a conversation summary.
    SelectiveSummary,
    /// Full-output detail view (Ctrl+E)
    DetailView,
    /// Review local feedback without interrupting the running agent.
    Feedback,
}

#[derive(Debug, Clone)]
struct QueuedPrompt {
    id: u64,
    content: String,
    kind: PromptKind,
}

#[derive(Debug, Clone)]
struct PendingModelChange {
    model: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingAgentToolNote {
    application_id: Option<String>,
    content: String,
}

/// Skills catalog fragment plus the provenance recorded on it.
///
/// `source` is stamped with the catalog-budget rung so `/prompt-audit` shows
/// degradation as provenance, not only inside the rendered block.
struct SkillsPromptSection {
    content: Option<String>,
    source: String,
    audit_only: bool,
}

struct PendingAgentNoteConsumption {
    application_id: String,
    content: String,
    consumed: tokio::sync::oneshot::Receiver<()>,
}

struct PendingAgentNoteApplication {
    note: PendingAgentToolNote,
    applied: tokio::sync::oneshot::Receiver<()>,
    consumed: Option<tokio::sync::oneshot::Receiver<()>>,
}

#[derive(Debug, Clone, Copy)]
struct QueuedPromptCursor {
    id: u64,
}

struct SystemPromptAssemblyContext<'a> {
    cwd: &'a str,
    current_year: i32,
    skills: SkillsPromptSection,
    harness_section: Option<String>,
    rlm_section: Option<String>,
    mailbox_section: Option<String>,
    managed_rules_section: Option<String>,
    active_prompt: &'a str,
}

fn build_mcp_config_watcher() -> ConfigWatcher {
    let mut builder = ConfigWatcherBuilder::new().debounce(Duration::from_millis(250));

    if let Some(home) = dirs::home_dir() {
        builder = builder
            .watch(home.join(".composer").join("mcp.json"))
            .watch(home.join(".composer").join("enterprise").join("mcp.json"))
            .watch(home.join(".maestro").join("mcp.json"))
            .watch(home.join(".maestro").join("enterprise").join("mcp.json"))
            .watch(home.join(".maestro").join("keybindings.json"));
    }

    builder
        .watch(".composer/mcp.json")
        .watch(".composer/mcp.local.json")
        .watch(".maestro/mcp.json")
        .watch(".maestro/mcp.local.json")
        .watch(crate::keybindings::keybindings_config_path())
        .build()
        .unwrap_or_default()
}

fn is_mcp_config_path(path: &std::path::Path) -> bool {
    path.ends_with(std::path::Path::new(".composer").join("mcp.json"))
        || path.ends_with(std::path::Path::new(".composer").join("mcp.local.json"))
        || path.ends_with(std::path::Path::new(".maestro").join("mcp.json"))
        || path.ends_with(std::path::Path::new(".maestro").join("mcp.local.json"))
        || path.ends_with(
            std::path::Path::new(".composer")
                .join("enterprise")
                .join("mcp.json"),
        )
        || path.ends_with(
            std::path::Path::new(".maestro")
                .join("enterprise")
                .join("mcp.json"),
        )
}

#[cfg(test)]
fn format_mcp_scope_label(scope: McpConfigScope) -> &'static str {
    match scope {
        McpConfigScope::Enterprise => "Enterprise config",
        McpConfigScope::Managed => "Managed connection",
        McpConfigScope::Local => "Local config",
        McpConfigScope::Project => "Project config",
        McpConfigScope::User => "User config",
    }
}

#[cfg(test)]
fn format_mcp_transport_label(transport: McpTransport) -> &'static str {
    match transport {
        McpTransport::Http => "HTTP",
        McpTransport::Sse => "SSE",
        McpTransport::Stdio => "stdio",
    }
}

fn format_mcp_error_label(error: Option<&str>) -> Option<String> {
    error.map(|message| {
        let trimmed = message.trim();
        if trimmed.is_empty() {
            maestro_ui::localization::tr("Connection failed.").to_string()
        } else {
            trimmed.to_string()
        }
    })
}

/// Turn persistence failures into an actionable live-session error. Disk-full
/// errors otherwise look like generic I/O failures, even though the recovery
/// is different: the user must free space before Maestro can safely continue
/// recording the transcript.
pub(super) fn format_session_persistence_error(
    action: &str,
    error: impl std::fmt::Display,
) -> String {
    let error = error.to_string();
    let normalized = error.to_ascii_lowercase();
    if normalized.contains("enospc")
        || normalized.contains("disk full")
        || normalized.contains("storage full")
        || normalized.contains("no space")
    {
        maestro_ui::localization::format(
            "Disk full: Deixic Code could not {0}. Free disk space, then retry; the transcript may be missing the latest event. ({1})",
            &[(action).to_string(), (error).clone()],
        )
    } else {
        maestro_ui::localization::format(
            "Failed to {0}: {1}",
            &[(action).to_string(), (error).clone()],
        )
    }
}

fn trim_optional_message(message: Option<&str>) -> Option<&str> {
    message.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    })
}

fn format_mcp_progress_status(
    server: &str,
    progress: f64,
    total: Option<f64>,
    message: Option<&str>,
) -> String {
    let message = trim_optional_message(message);
    if let Some(total) = total.filter(|value| value.is_finite() && *value > 0.0) {
        let percent = ((progress / total) * 100.0).round().clamp(0.0, 100.0) as i64;
        if let Some(message) = message {
            format!("MCP {server}: {message} ({percent}%)")
        } else {
            format!("MCP {server}: {percent}%")
        }
    } else if let Some(message) = message {
        format!("MCP {server}: {message}")
    } else {
        maestro_ui::localization::format("MCP {0}: in progress", &[(server).to_string()])
    }
}

fn format_mcp_log_data(data: &serde_json::Value) -> String {
    let text = match data {
        serde_json::Value::String(message) => message.clone(),
        serde_json::Value::Null => "null".to_string(),
        other => serde_json::to_string(other)
            .unwrap_or_else(|_| maestro_ui::localization::tr("[Unserializable data]").to_string()),
    };

    text.chars().take(100).collect()
}

fn format_mcp_runtime_event_status(event: &McpRuntimeEvent) -> Option<String> {
    match event {
        McpRuntimeEvent::ToolsListChanged { server } => Some(maestro_ui::localization::format(
            "MCP server \"{0}\" tools updated",
            std::slice::from_ref(server),
        )),
        McpRuntimeEvent::ResourcesListChanged { server } => Some(maestro_ui::localization::format(
            "MCP server \"{0}\" resources updated",
            std::slice::from_ref(server),
        )),
        McpRuntimeEvent::PromptsListChanged { server } => Some(maestro_ui::localization::format(
            "MCP server \"{0}\" prompts updated",
            std::slice::from_ref(server),
        )),
        McpRuntimeEvent::Progress {
            server,
            progress,
            total,
            message,
        } => Some(format_mcp_progress_status(
            server,
            *progress,
            *total,
            message.as_deref(),
        )),
        McpRuntimeEvent::ToolRevoked {
            server,
            tool,
            reason,
        } => Some(maestro_ui::localization::format(
            "MCP tool \"{0}/{1}\" withdrawn: {2}",
            &[(server).clone(), (tool).clone(), (reason).clone()],
        )),
        McpRuntimeEvent::Log {
            server,
            level,
            data,
            ..
        } => match level.as_str() {
            "warning" | "error" | "critical" | "alert" | "emergency" => {
                Some(format!("[{server}] {}", format_mcp_log_data(data)))
            }
            _ => None,
        },
    }
}

fn format_mcp_connection_status(name: &str, tools: usize) -> String {
    let label = if tools == 1 {
        maestro_ui::localization::tr("tool")
    } else {
        maestro_ui::localization::tr("tools")
    };
    maestro_ui::localization::format(
        "MCP server \"{0}\" connected ({1} {2})",
        &[(name).to_string(), (tools).to_string(), (label).to_string()],
    )
}

fn format_mcp_disconnection_status(name: &str) -> String {
    maestro_ui::localization::format("MCP server \"{0}\" disconnected", &[(name).to_string()])
}

fn format_mcp_connection_error_status(name: &str, error: Option<&str>) -> String {
    let error_label = format_mcp_error_label(error)
        .unwrap_or_else(|| maestro_ui::localization::tr("Connection failed.").to_string());
    maestro_ui::localization::format(
        "MCP server \"{0}\" error: {1}",
        &[(name).to_string(), (error_label).clone()],
    )
}

fn format_mcp_server_transition_status(
    previous: Option<&crate::tools::McpServerStatus>,
    current: Option<&crate::tools::McpServerStatus>,
) -> Option<String> {
    match (previous, current) {
        (_, Some(server)) if server.connected => {
            if previous.is_none_or(|status| !status.connected) {
                Some(format_mcp_connection_status(
                    &server.name,
                    server.tools.len(),
                ))
            } else {
                None
            }
        }
        (Some(previous), Some(server)) => {
            let previous_error = format_mcp_error_label(previous.error.as_deref());
            let current_error = format_mcp_error_label(server.error.as_deref());

            if current_error != previous_error {
                current_error.as_deref().map(|_| {
                    format_mcp_connection_error_status(&server.name, server.error.as_deref())
                })
            } else if previous.connected && !server.connected {
                Some(format_mcp_disconnection_status(&server.name))
            } else {
                None
            }
        }
        (None, Some(server)) => server
            .error
            .as_deref()
            .map(|_| format_mcp_connection_error_status(&server.name, server.error.as_deref())),
        (Some(previous), None) if previous.connected => {
            Some(format_mcp_disconnection_status(&previous.name))
        }
        _ => None,
    }
}

fn snapshot_mcp_server_statuses(
    servers: &[crate::tools::McpServerStatus],
) -> HashMap<String, crate::tools::McpServerStatus> {
    servers
        .iter()
        .cloned()
        .map(|server| (server.name.clone(), server))
        .collect()
}

#[cfg(test)]
fn render_mcp_status_lines(servers: &[crate::tools::McpServerStatus]) -> Vec<String> {
    let mut lines = vec!["Model Context Protocol".to_string(), String::new()];

    if servers.is_empty() {
        lines.push("No MCP servers configured.".to_string());
        lines.push(String::new());
        lines.push(
            "Use `/mcp-config help`, or edit ~/.maestro/mcp.json or .maestro/mcp.json:".to_string(),
        );
        lines.push(String::new());
        lines.push(
            "{ \"mcpServers\": { \"my-server\": { \"command\": \"npx\", \"args\": [\"-y\", \"@example/mcp-server\"] } } }"
                .to_string(),
        );
        return lines;
    }

    for server in servers {
        let status = if server.connected {
            "connected"
        } else {
            "disconnected"
        };
        lines.push(format!("- {} ({status})", server.name));
        lines.push(format!(
            "  Source: {}",
            format_mcp_scope_label(server.scope)
        ));
        lines.push(format!(
            "  Transport: {}",
            format_mcp_transport_label(server.transport)
        ));

        if server.connected {
            if !server.tools.is_empty() {
                lines.push(format!("  Tools: {}", server.tools.join(", ")));
            }
            if !server.resources.is_empty() {
                lines.push(format!("  Resources: {}", server.resources.len()));
            }
            if !server.prompts.is_empty() {
                lines.push(format!("  Prompts: {}", server.prompts.join(", ")));
            }
        } else {
            lines.push("  Not connected".to_string());
            if let Some(error_label) = format_mcp_error_label(server.error.as_deref()) {
                lines.push(format!("  Error: {error_label}"));
            }
        }
    }

    lines.push(String::new());
    lines.push("Subcommands: /mcp resources, /mcp prompts".to_string());
    lines
}

fn render_mcp_prompt_lines(
    prompt_servers: &[(String, Vec<McpPrompt>)],
    server_name: Option<&str>,
) -> Vec<String> {
    let mut lines = vec![
        maestro_ui::localization::tr("MCP Prompts").to_string(),
        String::new(),
    ];

    if prompt_servers.is_empty() {
        lines.push(match server_name {
            Some(name) => maestro_ui::localization::format(
                "Server '{0}' does not expose prompts.",
                &[(name).to_string()],
            ),
            None => maestro_ui::localization::tr("No prompts available from connected servers.")
                .to_string(),
        });
    } else {
        for (server_name, prompts) in prompt_servers {
            lines.push(format!("{server_name}:"));
            for prompt in prompts {
                append_mcp_prompt_summary(&mut lines, prompt, "  ", "    ");
            }
            lines.push(String::new());
        }
    }

    lines.push(String::new());
    lines.push(
        maestro_ui::localization::tr("Usage: /mcp prompts <server> <name> [KEY=value ...]")
            .to_string(),
    );
    lines
}

/// Main application struct - the central coordinator.
///
/// # Rust Concept: Struct with Many Fields
///
/// This struct owns many resources. In Rust, this is fine - there's no
/// overhead for having many fields. The struct size is the sum of its
/// field sizes, laid out contiguously in memory.
///
/// # Rust Concept: Option for Optional Resources
///
/// Fields like `native_agent: Option<NativeAgent>` use `Option` because
/// the agent may not be spawned yet. This is more explicit than null -
/// you must handle the None case.
///
/// # Rust Concept: Arc for Shared Ownership
///
/// `command_registry: Arc<CommandRegistry>` is wrapped in `Arc` because
/// multiple components need read access to the registry. Arc provides
/// thread-safe shared ownership through reference counting.
/// A `/loop` schedule: re-fires a prompt on an interval.
///
/// The loop only fires while the app is idle; a due prompt waits for the
/// current turn to finish rather than interrupting it.
struct LoopSchedule {
    /// Seconds between firings.
    interval: Duration,
    /// The prompt to re-submit.
    prompt: String,
    /// Next time the loop should fire.
    next_fire: Instant,
}

pub struct App {
    /// Central application state (messages, input, status).
    /// See `state.rs` for details.
    state: AppState,

    /// The AI agent that processes prompts and generates responses.
    /// `Option` because it's spawned asynchronously after app creation.
    native_agent: Option<NativeAgent>,

    /// Channel receiver for messages from the agent.
    /// The agent sends streaming responses, tool calls, etc. through this.
    /// `mpsc::UnboundedReceiver` = async channel with unlimited buffer.
    native_event_rx: Option<mpsc::UnboundedReceiver<FromAgent>>,

    /// Channel sender for approval decisions back to the agent.
    /// The native agent owns ordered execution after approval.
    /// [`ToolResponseMessage`]: (`call_id`, approved, `optional_external_result`,
    /// provenance source).
    tool_response_tx: Option<mpsc::UnboundedSender<ToolResponseMessage>>,

    /// Executes tools (bash commands, file reads, etc.) requested by the agent.
    ///
    /// Shared via `Arc` so tool executions can run on spawned tasks instead of
    /// blocking the event loop (a bash command may run for minutes).
    tool_executor: Arc<ToolExecutor>,

    /// Shared credential vault for the active application session.
    credential_vault: CredentialVault,

    /// The ratatui terminal handle for rendering.
    terminal: terminal::Terminal,

    /// Whether the terminal backend is a real TTY that supports cursor-aware clears.
    terminal_clear_supported: bool,

    /// Cached terminal dimensions, refreshed only at startup and on resize.
    terminal_size: Option<(u16, u16)>,

    /// Protocol-aware terminal input. Falls back to crossterm when unavailable.
    terminal_events: Option<TerminalEventReader>,

    /// Flag to exit the main loop.
    should_quit: bool,
    /// Relaunch in the saved workspace after this agent and writer shut down.
    pub(crate) resume_target: Option<(std::path::PathBuf, String)>,

    /// Terminal capabilities (color support, viewport position, etc.).
    capabilities: TerminalCapabilities,

    /// Registry of all available slash commands.
    /// Wrapped in Arc for shared access from command palette.
    command_registry: Arc<CommandRegistry>,

    /// Fuzzy matcher for slash command completion.
    slash_matcher: SlashCommandMatcher,

    /// State for Tab-cycling through slash command completions.
    slash_state: SlashCycleState,

    /// Which modal (if any) is currently shown.
    active_modal: ActiveModal,
    feedback_ui: bug_reports::FeedbackUi,
    feedback_rating_rx: Option<tokio::sync::oneshot::Receiver<visibility::FeedbackSubmission>>,

    /// File search modal component (like VS Code's Ctrl+P).
    file_search: FileSearchModal,

    /// Workspace files shared by file search and the unified palette.
    workspace_files: Vec<WorkspaceFile>,

    /// Receiver for the background workspace file scan (startup and
    /// `/refresh-workspace`); polled by the event loop.
    workspace_scan_rx: Option<std::sync::mpsc::Receiver<Vec<WorkspaceFile>>>,

    /// Set by `/refresh-workspace` so scan completion confirms in the status
    /// line (the startup scan stays silent).
    workspace_refresh_pending: bool,

    /// Session history browser modal.
    session_switcher: SessionSwitcher,

    /// Recent persisted tool executions.
    operations: OperationsModal,

    /// Unified MCP server manager.
    mcp_manager: McpManager,

    /// Command palette modal (like VS Code's Ctrl+Shift+P).
    command_palette: CommandPalette,

    /// Handles tool execution approval flow.
    approval_controller: ApprovalController,

    /// Native OS sandbox policy resolved at startup (see
    /// `config::resolve_interactive_sandbox_policy`).
    ///
    /// Stored so `spawn_agent` can pass the *same* policy into
    /// `NativeAgentConfig`: the native agent runner's own tool executor
    /// (which actually runs every Yolo-mode call and every allowlisted
    /// Selective-mode call) is entirely separate from `self.tool_executor`
    /// below, which only ever runs approval-gated calls. Without this,
    /// resolving a policy here and applying it only to `self.tool_executor`
    /// sandboxes nothing for the common case (review finding on #3144).
    sandbox_policy: Option<crate::sandbox::SandboxPolicy>,

    /// Optional guardian: an independent LLM reviewer that auto-approves
    /// routine tool calls silently and fails closed to the human modal.
    /// Enabled via `MAESTRO_GUARDIAN=1`; see `safety::guardian`.
    guardian: Option<crate::safety::guardian::Guardian>,

    /// Completion channel for spawned guardian reviews; drained by the event
    /// loop via `poll_guardian_verdicts`.
    guardian_tx: mpsc::UnboundedSender<GuardianReviewOutcome>,
    guardian_rx: mpsc::UnboundedReceiver<GuardianReviewOutcome>,

    /// Call IDs with a guardian review in flight. Cleared on interrupt so a
    /// review that finishes after Ctrl+C cannot auto-execute the tool.
    pending_guardian_reviews: HashSet<String>,

    /// MCP connection work runs outside the input/render loop. Results are
    /// drained here so a slow server cannot freeze keystrokes or painting.
    mcp_status_tx: mpsc::UnboundedSender<Result<Vec<crate::tools::McpServerStatus>, String>>,
    mcp_status_rx: mpsc::UnboundedReceiver<Result<Vec<crate::tools::McpServerStatus>, String>>,
    mcp_status_refresh_in_flight: bool,
    /// Long-running MCP configuration work (notably browser OAuth) also stays
    /// off the input/render loop.
    mcp_config_tx: mpsc::UnboundedSender<Result<String, String>>,
    mcp_config_rx: mpsc::UnboundedReceiver<Result<String, String>>,
    mcp_config_in_flight: bool,

    /// Manages session persistence (save/load conversations).
    session_manager: SessionManager,

    /// System clipboard integration.
    clipboard: ClipboardManager,

    /// AI model selection modal.
    model_selector: ModelSelector,

    /// Color theme selection modal.
    theme_selector: ThemeSelector,
    config_selector: crate::components::ConfigSelector,

    /// `/setup` modal for mandatory EvalOps Identity and optional local API keys.
    setup_modal: SetupModal,
    setup_login_rx: Option<tokio::sync::oneshot::Receiver<Result<(), String>>>,
    onboarding: onboarding::OnboardingSession,
    pending_agent_spawn: bool,
    /// True when `current_model` was chosen by `/setup` or `/model`, so a
    /// later respawn should not fall back to `resolve_default_model`.
    current_model_user_set: bool,

    /// Keyboard shortcuts help overlay.
    shortcuts_help: ShortcutsHelp,

    /// File checkpoint restore picker modal.
    rewind_picker: RewindPicker,
    selective_summary: Option<selective_summary::SummaryDialog>,

    /// Token usage and cost tracker.
    usage_tracker: crate::usage::UsageTracker,

    /// Aggregated facts for the currently running turn. The recap is only
    /// published after the terminal response sentinel makes the app idle.
    active_turn_summary: Option<crate::turn_summary::TurnSummary>,
    active_turn_start_message_index: Option<usize>,
    active_turn_started_at: Option<Instant>,

    /// Prompt history for recall and search.
    prompt_history: crate::history::PromptHistory,

    /// Tool execution history.
    tool_history: crate::tools::ToolHistory,

    /// Loaded skill definitions (with paths/resources).
    loaded_skills: Vec<LoadedSkill>,

    /// Skill load errors from last scan.
    skill_load_errors: Vec<SkillLoadError>,

    /// Runtime skill registry (activation state).
    skill_registry: SkillRegistry,

    /// Discovered filesystem plugins (skills/commands/hooks/MCP packages).
    plugin_registry: PluginRegistry,

    /// Flat markdown prompt/command templates.
    custom_prompts: Vec<PromptDefinition>,

    /// Droid-style executable slash commands from `.composer/commands/`.
    exec_commands: Vec<crate::exec_commands::ExecCommand>,

    /// Sender side of the exec-command completion channel (cloned into workers).
    exec_command_tx: std::sync::mpsc::Sender<exec_commands::ExecCommandOutcome>,

    /// Receiver polled each frame for finished executable command runs.
    exec_command_rx: std::sync::mpsc::Receiver<exec_commands::ExecCommandOutcome>,

    /// Background progress for `/handoff` A2A tasks.
    a2a_handoff_tx: mpsc::UnboundedSender<a2a_handoff::A2aHandoffEvent>,
    a2a_handoff_rx: mpsc::UnboundedReceiver<a2a_handoff::A2aHandoffEvent>,

    /// Optional initial prompt from CLI (Grok-style trailing args).
    initial_prompt: Option<String>,

    /// Prompts submitted while running (queued in the agent).
    queued_prompts: VecDeque<QueuedPrompt>,

    /// Presentation of the last explicit terminal event; cleared on session changes.
    dex_terminal: Option<crate::components::dex_companion::DexCompanionState>,
    dex_pose_started: Instant,
    dex_delight: dex_presentation::DexPresentation,

    /// Queued prompt reserved by the agent (between `ResponseEnd` and `ResponseStart`).
    queued_prompt_inflight: Option<QueuedPromptCursor>,

    /// Queued prompt currently being processed.
    queued_prompt_active: Option<QueuedPrompt>,

    /// Next id for queued prompts.
    next_queue_id: u64,

    /// Structured comments on the current plan, rebuilt from session events.
    plan_review_comments: Vec<PlanReviewComment>,

    /// When the current session started (for policy limits).
    session_started_at: SystemTime,

    /// True when a session was loaded but the writer failed to resume.
    session_resume_failed: bool,

    /// Current model in use (for session headers and usage tracking).
    current_model: String,

    /// Current thinking level (for session headers/changes).
    current_thinking_level: ThinkingLevel,
    pending_assistant_content: Option<(String, Vec<crate::ai::ContentBlock>)>,

    /// Last time we refreshed MCP status for runtime badges.
    last_mcp_status_refresh: Option<Instant>,

    /// Last Esc press used for double-Esc "clear input" detection.
    last_esc_at: Option<Instant>,

    /// Active `/loop` schedule: re-fires a prompt on an interval while the
    /// app is running.
    loop_schedule: Option<LoopSchedule>,

    /// Structured goal mode store (`/goal`).
    goal_store: GoalStore,

    /// Durable supplemental context used by `/harness` and `/refine`.
    harness_store: HarnessStore,

    /// Persistent named context variables used by `/rlm`.
    rlm_store: RlmStore,

    /// Durable messages left by or addressed to delegated sessions.
    mailbox_store: MailboxStore,

    /// The organization's setup document, resolved once at session start. It
    /// supplies the system-prompt rule block, the MCP server policy installed
    /// on the tool executor, and the team sandbox policy source.
    managed_setup: crate::managed_setup::ManagedSetupClient,
    managed_setup_identity_scope: Option<crate::telemetry::TelemetryIdentityScope>,

    /// When true and the agent is idle, fire one goal continuation prompt.
    /// Armed on create/resume/auto-on, and after a worker turn if the goal is
    /// still active (worker did not call `update_goal` complete|blocked).
    goal_auto_continue_armed: bool,

    /// Status-bar density (`/footer rich|solo|history|clear`).
    footer_style: FooterStyle,
    worker_badge: Option<String>,
    worker_badge_refreshed: std::time::Instant,
    ui_prefs: crate::ui_prefs::UiPrefs,
    configured_animations: bool,

    /// Local paths attached via `/attach` or clipboard image paste for the
    /// next `submit_prompt` (cleared after send).
    pending_attachments: Vec<String>,
    draft_stash: Option<composer_recall::Draft>,
    history_search: Option<composer_recall::HistorySearch>,

    /// Last observed MCP server status snapshots for transition messages.
    last_mcp_server_statuses: HashMap<String, crate::tools::McpServerStatus>,

    /// Watches MCP config files so status badges refresh immediately after edits.
    config_watcher: ConfigWatcher,

    /// Pending model change awaiting agent confirmation.
    pending_model_change: Option<PendingModelChange>,

    /// Bounded actor and result channel for selected-model verification.
    model_monitor: crate::model_monitor::ModelMonitor,
    model_verification_rx: std::sync::mpsc::Receiver<crate::model_monitor::ModelVerificationEvent>,

    /// Nonblocking discovery actor for local OpenAI-compatible runtimes.
    local_model_discovery: crate::local_models::LocalDiscoveryHandle,
    local_model_discovery_rx: std::sync::mpsc::Receiver<crate::local_models::LocalDiscoveryBatch>,

    /// Receiver for a background `/rubber-duck` review; polled by the event loop.
    rubber_duck_rx: Option<std::sync::mpsc::Receiver<crate::rubber_duck::RubberDuckEvent>>,

    /// True while a rubber-duck review task is running.
    rubber_duck_running: bool,

    /// Cached git branch for session info updates.
    current_git_branch: Option<String>,

    /// Keyboard shortcuts used to cycle configured models.
    cycle_model_binding: crate::key_hints::KeyBinding,
    cycle_model_backward_binding: crate::key_hints::KeyBinding,
    /// Keyboard shortcut used to open the command palette.
    command_palette_binding: crate::key_hints::KeyBinding,

    /// Keyboard shortcut used to open file search.
    file_search_binding: crate::key_hints::KeyBinding,

    /// Keyboard shortcut used to toggle tool output expansion.
    toggle_tool_outputs_binding: crate::key_hints::KeyBinding,

    /// Keyboard shortcut used to restore the last queued follow-up.
    queued_follow_up_edit_binding: crate::key_hints::KeyBinding,

    /// Last surfaced keybinding config issue summary.
    last_keybinding_issue_summary: Option<String>,

    /// Follow-up currently being edited out of the queue.
    editing_queued_follow_up: Option<QueuedPrompt>,

    /// Interrupt should immediately submit the next queued steering batch.
    submit_queued_steering_after_interrupt: bool,

    /// Interrupt should restore queued prompts into the composer after the run ends.
    restore_queued_prompts_after_interrupt: bool,

    /// Pre-turn file checkpoint awaiting turn completion (git worktrees only).
    pending_checkpoint: Option<crate::checkpoints::PendingTurn>,

    /// Background-task lifecycle notes waiting for agent idle before injection
    /// into conversation history (provider-safe user tool notes).
    pending_agent_tool_notes: Vec<PendingAgentToolNote>,

    /// Notes sent to the runner but not yet confirmed in native history.
    pending_agent_note_applications: Vec<PendingAgentNoteApplication>,

    /// Applied lifecycle notes waiting for a successful native model turn.
    pending_agent_note_consumptions: Vec<PendingAgentNoteConsumption>,

    /// Consumed lifecycle-note markers waiting for durable persistence.
    pending_consumed_agent_tool_notes: Vec<String>,

    /// Consumed lifecycle notes waiting for the app to persist the terminal
    /// assistant response that proves the model turn completed.
    ready_consumed_agent_tool_notes: Vec<String>,

    /// Whether every assistant response in the active model turn reached the
    /// session transcript. Consumption markers cannot precede that boundary.
    active_turn_assistant_messages_persisted: bool,

    /// Process-local lifecycle dedupe for intentionally ephemeral sessions.
    ephemeral_lifecycle_applications: HashSet<String>,

    /// Terminal-native turn notifications (OSC 9;4 tab progress, OSC 0 title,
    /// focus-gated desktop notifications).
    terminal_notifier: crate::notifications::TerminalStateNotifier,

    /// Whether `run_inner` started a terminal notification session.
    terminal_session_started: bool,

    /// Full-output detail view overlay (Ctrl+E), when open.
    detail_view: Option<DetailView>,

    /// Modal to restore when the detail view closes (e.g. back to Approval).
    detail_return_modal: ActiveModal,
}

/// Build a single, user-facing notice explaining why repo-controlled
/// project config (inline tools, hooks, skills, plugins) was skipped
/// because this workspace isn't trusted.
///
/// Silently dropping a repository's tools/hooks/skills reads as a bug to
/// users, so every one of the four project-scoped load paths is checked
/// here (in addition to logging via `eprintln!` at the point of skipping,
/// for headless/log-based consumption) and folded into one system message
/// shown once at startup / on `/skills reload`. Returns `None` when the
/// workspace is trusted, or when it's untrusted but none of the
/// project-scoped config files/directories exist (nothing to explain).
fn untrusted_workspace_notice(
    workspace_dir: &std::path::Path,
    plugin_registry: &PluginRegistry,
) -> Option<String> {
    if crate::config::workspace_trusted_in_global_config(workspace_dir) {
        return None;
    }

    let mut skipped = Vec::new();
    if crate::tools::inline::has_project_tools_config(workspace_dir) {
        skipped.push(maestro_ui::localization::tr(
            ".composer/tools.json (custom tools)",
        ));
    }
    if crate::hooks::has_project_hook_config(workspace_dir) {
        skipped.push(maestro_ui::localization::tr(
            ".composer/hooks.toml or .json (hooks)",
        ));
    }
    if SkillLoader::has_project_skill_dirs(workspace_dir) {
        skipped.push(".agents|.composer|.maestro/skills (skills)");
    }
    let has_skipped_plugins = plugin_registry.untrusted_skip_notice().is_some();
    if has_skipped_plugins {
        skipped.push(".maestro|.composer/plugins (plugins)");
    }

    if skipped.is_empty() {
        return None;
    }

    Some(maestro_ui::localization::format(
        "Workspace untrusted — skipped project config: {0}. Run `/trust` (or `deixic-code trust`) to load them for {1}.",
        &[
            (skipped.join(", ")).clone(),
            (workspace_dir.display()).to_string(),
        ],
    ))
}

// ─────────────────────────────────────────────────────────────────────────────
// IMPLEMENTATION
// ─────────────────────────────────────────────────────────────────────────────

enum PlatformSessionResolution {
    Detect,
    #[cfg(test)]
    UseNoPlatformSession,
}

/// Bind configuration and its receipt origin to the same live Identity result.
/// An unverified local selector can only select a deny-all policy;
/// it cannot fetch a document or authorize a receipt.
fn resolve_verified_managed_setup(
    verified: anyhow::Result<crate::credential_mode::PlatformSession>,
    unverified: impl FnOnce() -> Option<crate::credential_mode::PlatformSession>,
) -> (
    crate::managed_setup::ManagedSetupClient,
    Option<crate::telemetry::TelemetryIdentityScope>,
) {
    match verified {
        Ok(session) => {
            let origin = crate::telemetry::TelemetryIdentityScope::new(
                &session.organization_id,
                session.workspace_id.as_deref(),
            );
            (resolve_session_managed_setup(&session), origin)
        }
        Err(_) => (
            crate::managed_setup::ManagedSetupClient::resolve_with(
                unverified().as_ref(),
                None,
                0,
                std::time::Duration::ZERO,
                |_| {
                    Err(crate::managed_setup::ManagedSetupError::Request(
                        maestro_ui::localization::tr("Identity session could not be verified")
                            .to_owned(),
                    ))
                },
            ),
            None,
        ),
    }
}

fn resolve_session_managed_setup(
    session: &crate::credential_mode::PlatformSession,
) -> crate::managed_setup::ManagedSetupClient {
    let worker_session = session.clone();
    // App construction also runs inside Tokio. Keep the blocking HTTP client's
    // private runtime and its destruction on this worker thread.
    std::thread::spawn(move || {
        crate::managed_setup::ManagedSetupClient::resolve(Some(&worker_session))
    })
    .join()
    .unwrap_or_else(|_| {
        crate::managed_setup::ManagedSetupClient::resolve_with(
            Some(session),
            None,
            0,
            std::time::Duration::ZERO,
            |_| {
                Err(crate::managed_setup::ManagedSetupError::Request(
                    maestro_ui::localization::tr("managed setup worker failed").to_owned(),
                ))
            },
        )
    })
}

impl App {
    /// Create a new application instance.
    ///
    /// # Rust Concept: Constructor Pattern
    ///
    /// Rust doesn't have constructors like OOP languages. Instead, we use
    /// associated functions (functions in `impl` blocks without `self`).
    /// By convention, `new()` creates a new instance.
    ///
    /// # Rust Concept: Error Propagation with `?`
    ///
    /// The `?` operator is syntactic sugar for error handling:
    /// - If the expression is `Ok(value)`, extract `value`
    /// - If the expression is `Err(e)`, return `Err(e)` from the function
    ///
    /// `.context("msg")` from anyhow wraps the error with additional context.
    ///
    /// # Returns
    ///
    /// `Result<Self>` - either a new App instance or an initialization error.
    pub fn new() -> Result<Self> {
        Self::new_with_initial_prompt(None)
    }

    /// Create an app, optionally submitting `initial_prompt` after the agent is ready.
    pub fn new_with_initial_prompt(initial_prompt: Option<String>) -> Result<Self> {
        let (mut terminal, mut capabilities) =
            terminal::init().context("Failed to initialize terminal")?;
        let mut terminal_events =
            if uncurses_input_enabled(std::env::var_os("MAESTRO_UNCURSES_INPUT").as_deref()) {
                TerminalEventReader::open().ok()
            } else {
                None
            };
        let (prepared, draft) = match startup::prepare_with_composer(
            &mut terminal,
            &mut capabilities,
            &mut terminal_events,
        ) {
            Ok(prepared) => prepared,
            Err(error) => {
                let _ = terminal::restore();
                return Err(error);
            }
        };
        let mut app =
            Self::new_with_prepared_startup(terminal, capabilities, initial_prompt, true, prepared);
        app.state.textarea = draft;
        // Keep the reader and its buffered input across the startup handoff.
        app.terminal_events = terminal_events;
        app.initialize_terminal_events();
        app.note_goal_paused_on_restart_if_needed();
        app.note_orphan_background_tasks_if_any();
        Ok(app)
    }

    /// Surface a system message when process-start demoted an active goal.
    pub(crate) fn note_goal_paused_on_restart_if_needed(&mut self) {
        let Some(goal) = self.goal_store.current.as_ref() else {
            return;
        };
        if goal.status != crate::goal::GoalStatus::Paused {
            return;
        }
        let Some(reason) = goal.last_judge_reason.as_deref() else {
            return;
        };
        if !reason.contains("process restart") {
            return;
        }
        self.state.add_system_message(self.state.locale.format("Goal {0} was active when Maestro last exited and is now **paused**. Use `/goal resume` to continue, or `/goal clear` to drop it.", std::slice::from_ref(&(goal.id))));
    }

    /// Clear the process-global goal for a forked session. Returns the cleared id.
    pub(crate) fn clear_goal_for_fork(&mut self) -> Result<Option<String>> {
        self.goal_auto_continue_armed = false;
        let previous_tools_visible = self.goal_store.tools_visible();
        let result = self.goal_store.clear_for_session_fork();
        if previous_tools_visible != self.goal_store.tools_visible() {
            self.sync_agent_goal_tools_visibility();
        }
        result
    }

    pub(crate) fn note_system_message(&mut self, message: impl Into<String>) {
        self.state.add_system_message(message.into());
    }

    /// Push buffered host tool notes into the agent transcript (no model turn).
    fn flush_pending_agent_tool_notes(&mut self) {
        if self.pending_agent_tool_notes.is_empty() {
            return;
        }
        if self.native_agent.is_none() {
            return;
        }
        let mut notes = std::mem::take(&mut self.pending_agent_tool_notes).into_iter();
        while let Some(note) = notes.next() {
            let (applied, consumed) = match self
                .native_agent
                .as_ref()
                .expect("native agent checked above")
                .inject_user_note(note.content.clone())
            {
                Ok(receivers) => receivers,
                Err(error) => {
                    self.pending_agent_tool_notes.push(note);
                    self.pending_agent_tool_notes.extend(notes);
                    self.state.error = Some(error.to_string());
                    return;
                }
            };
            self.pending_agent_note_applications
                .push(PendingAgentNoteApplication {
                    note,
                    applied,
                    consumed: Some(consumed),
                });
        }
    }

    fn record_agent_tool_note_progress(&mut self, commit_ready_consumptions: bool) {
        let mut applied_notes = Vec::new();
        let mut retry_notes = Vec::new();
        self.pending_agent_note_applications.retain_mut(|pending| {
            match pending.applied.try_recv() {
                Ok(()) => {
                    applied_notes.push((
                        pending.note.application_id.clone(),
                        pending.note.content.clone(),
                        pending
                            .consumed
                            .take()
                            .expect("pending note consumption receiver"),
                    ));
                    false
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => true,
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    retry_notes.push(pending.note.clone());
                    false
                }
            }
        });
        self.pending_agent_tool_notes.extend(retry_notes);
        for (application_id, content, consumed) in applied_notes {
            if let Some(application_id) = application_id {
                self.record_subagent_lifecycle_agent_note_delivered(&application_id);
                self.pending_agent_note_consumptions
                    .push(PendingAgentNoteConsumption {
                        application_id,
                        content,
                        consumed,
                    });
            }
        }

        let mut consumed_ids = Vec::new();
        let mut retry_consumed_notes = Vec::new();
        self.pending_agent_note_consumptions.retain_mut(|pending| {
            match pending.consumed.try_recv() {
                Ok(()) => {
                    consumed_ids.push(pending.application_id.clone());
                    false
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => true,
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    retry_consumed_notes.push(PendingAgentToolNote {
                        application_id: Some(pending.application_id.clone()),
                        content: pending.content.clone(),
                    });
                    false
                }
            }
        });
        self.pending_agent_tool_notes.extend(retry_consumed_notes);
        self.ready_consumed_agent_tool_notes.extend(consumed_ids);
        if commit_ready_consumptions {
            self.pending_consumed_agent_tool_notes
                .append(&mut self.ready_consumed_agent_tool_notes);
        }
        let pending_consumed = std::mem::take(&mut self.pending_consumed_agent_tool_notes);
        for application_id in pending_consumed {
            if !self.record_subagent_lifecycle_agent_note_consumed(&application_id) {
                self.pending_consumed_agent_tool_notes.push(application_id);
            }
        }
    }

    fn restore_pending_lifecycle_agent_notes(&mut self, session: &ParsedSession) {
        self.pending_agent_note_applications.clear();
        self.pending_agent_note_consumptions.clear();
        self.pending_consumed_agent_tool_notes.clear();
        self.ready_consumed_agent_tool_notes.clear();
        self.ephemeral_lifecycle_applications.clear();
        self.pending_agent_tool_notes = session
            .pending_lifecycle_agent_notes
            .iter()
            .map(|note| PendingAgentToolNote {
                application_id: Some(note.application_id.clone()),
                content: note.content.clone(),
            })
            .collect();
    }

    fn sync_agent_goal_tools_visibility(&self) {
        if let Some(agent) = self.native_agent.as_ref() {
            agent.set_goal_tools_visible(self.goal_store.tools_visible());
        }
    }

    /// Warn if a prior process left running background-task records.
    pub(crate) fn note_orphan_background_tasks_if_any(&mut self) {
        let orphans = crate::tools::background_tasks::load_persisted_running_snapshot();
        if orphans.is_empty() {
            return;
        }
        let preview: Vec<String> = orphans
            .iter()
            .take(5)
            .map(|task| {
                format!(
                    "{} pid={} `{}`",
                    task.id,
                    task.pid
                        .map(|p| p.to_string())
                        .unwrap_or_else(|| "?".into()),
                    task.command.chars().take(48).collect::<String>()
                )
            })
            .collect();
        let extra = if orphans.len() > 5 {
            format!(" (+{} more)", orphans.len() - 5)
        } else {
            String::new()
        };
        self.state.add_system_message(self.state.locale.format("Previous Maestro process left {0} background task(s) marked running{1}. PIDs may still be alive outside this session: {2}", &[(orphans.len()).to_string(), (extra).clone(), (preview.join("; ")).clone()]));
    }

    fn new_with_terminal(
        terminal: terminal::Terminal,
        capabilities: TerminalCapabilities,
        initial_prompt: Option<String>,
        terminal_clear_supported: bool,
    ) -> Self {
        Self::new_with_prepared_startup(
            terminal,
            capabilities,
            initial_prompt,
            terminal_clear_supported,
            startup::PreparedStartup::load(PlatformSessionResolution::Detect),
        )
    }

    fn new_with_prepared_startup(
        terminal: terminal::Terminal,
        capabilities: TerminalCapabilities,
        initial_prompt: Option<String>,
        terminal_clear_supported: bool,
        prepared: startup::PreparedStartup,
    ) -> Self {
        let config = &prepared.config;
        let context_window = config.model_context_window.map(|value| value as u64);
        let mut history_config = crate::history::HistoryConfig::default();
        if let Some(history_settings) = &config.history {
            if let Some(max_bytes) = history_settings.max_bytes {
                history_config = history_config.with_max_bytes(max_bytes);
            }
            if let Some(persistence) = history_settings.persistence {
                history_config = history_config.with_persistence(persistence);
            }
        }
        let prompt_history =
            crate::history::PromptHistory::load_with_config(history_config.clone())
                .unwrap_or_else(|_| crate::history::PromptHistory::new(history_config));
        let slash_command_fallback = config
            .tui
            .as_ref()
            .and_then(|tui| tui.slash_command_fallback)
            .unwrap_or(true);
        let theme_follow = config
            .tui
            .as_ref()
            .and_then(|tui| tui.theme_follow)
            .unwrap_or(false);
        let mut app = Self::new_with_terminal_with_history_and_prepared(
            terminal,
            capabilities,
            prompt_history,
            initial_prompt,
            context_window,
            terminal_clear_supported,
            prepared,
        );
        app.state.unknown_slash_command_fallback = slash_command_fallback;
        if theme_follow {
            let current = if crate::themes::current_theme_name() == "light" {
                "light"
            } else {
                "dark"
            };
            app.state.theme_follower = Some(crate::themes::osc11::AutoThemeFollower::new(current));
        }
        app
    }

    #[cfg(test)]
    fn new_with_terminal_with_history_and_platform_session(
        terminal: terminal::Terminal,
        capabilities: TerminalCapabilities,
        prompt_history: crate::history::PromptHistory,
        initial_prompt: Option<String>,
        context_window: Option<u64>,
        terminal_clear_supported: bool,
        platform_session_resolution: PlatformSessionResolution,
    ) -> Self {
        Self::new_with_terminal_with_history_and_prepared(
            terminal,
            capabilities,
            prompt_history,
            initial_prompt,
            context_window,
            terminal_clear_supported,
            startup::PreparedStartup::load(platform_session_resolution),
        )
    }

    fn new_with_terminal_with_history_and_prepared(
        terminal: terminal::Terminal,
        capabilities: TerminalCapabilities,
        prompt_history: crate::history::PromptHistory,
        initial_prompt: Option<String>,
        context_window: Option<u64>,
        terminal_clear_supported: bool,
        prepared: startup::PreparedStartup,
    ) -> Self {
        let startup::PreparedStartup {
            config: app_config,
            plugin_registry,
            loaded_skills,
            skill_load_errors,
            custom_prompts,
            exec_commands,
            managed_setup,
            managed_setup_identity_scope,
        } = prepared;
        let cwd = std::env::current_dir()
            .map_or_else(|_| ".".to_string(), |p| p.to_string_lossy().to_string());
        let workspace_dir = std::path::PathBuf::from(&cwd);
        let credential_vault = CredentialVault::new();

        let ui_prefs = crate::ui_prefs::UiPrefs::load_default();
        let mut state = AppState::new();
        state.locale = ui_prefs.locale();
        state.context_window = context_window;
        for error in plugin_registry.admission_errors() {
            state.add_system_message(
                state
                    .locale
                    .format("Plugin admission failed: {0}", std::slice::from_ref(error)),
            );
        }
        let queue_modes = crate::ui_state::load_queue_modes();
        if let Some(mode) = queue_modes.steering_mode {
            state.steering_mode = mode;
        }
        if let Some(mode) = queue_modes.follow_up_mode {
            state.follow_up_mode = mode;
        }
        let terminal_info = crate::terminal_info::TerminalInfo::get();
        let keybindings =
            load_rust_tui_keybindings(&terminal_info.name, std::env::var_os("TMUX").is_some());
        let keybinding_labels = keybindings.labels();
        let keybinding_issue_summary = summarize_keybindings_config_issues();
        state.queued_follow_up_edit_binding_label =
            keybinding_labels.edit_last_queued_follow_up.clone();
        if let Some(summary) = &keybinding_issue_summary {
            state.add_system_message(summary.clone());
        }

        let mut skill_registry = SkillRegistry::new();
        for loaded in &loaded_skills {
            skill_registry.register(loaded.definition.clone());
        }
        if let Some(notice) = crate::localization::with_locale(state.locale, || {
            untrusted_workspace_notice(&workspace_dir, &plugin_registry)
        }) {
            state.add_system_message(notice);
        }
        let (model_monitor, model_verification_rx) = crate::model_monitor::spawn_model_monitor();
        let (local_model_discovery, local_model_discovery_rx) =
            crate::local_models::spawn_local_model_discovery();
        local_model_discovery.refresh();

        let initial_thinking = crate::model_dynamics::configured_thinking(
            &app_config,
            &crate::codex_auth::resolve_default_model(),
            &crate::config::model_dynamics_config(),
        );
        state.thinking_level = initial_thinking;
        let tui_settings = app_config.tui.clone();

        // Resolve the session's configured native OS sandbox. Managed setup is
        // applied below before either executor is constructed, so a workspace
        // policy can only narrow this baseline.
        let requested_sandbox_policy =
            crate::config::resolve_interactive_sandbox_policy(&app_config);

        let terminal_notifier = crate::notifications::TerminalStateNotifier::from_config(
            tui_settings.as_ref().and_then(|tui| tui.tab_progress),
            tui_settings.as_ref().and_then(|tui| tui.title_updates),
            tui_settings
                .as_ref()
                .and_then(|tui| tui.focus_gated_notifications),
            std::env::var("TERM_PROGRAM").ok().as_deref(),
        );

        let mut command_registry =
            build_command_registry_with_extensions(&loaded_skills, &custom_prompts);
        let skipped_exec =
            crate::commands::register_exec_commands(&mut command_registry, &exec_commands);
        if !skipped_exec.is_empty() {
            state.add_system_message(exec_commands::exec_collision_warning(&skipped_exec));
        }
        let command_registry = Arc::new(command_registry);
        let slash_matcher = SlashCommandMatcher::new(Arc::clone(&command_registry));
        let (guardian_tx, guardian_rx) = mpsc::unbounded_channel();
        let (mcp_status_tx, mcp_status_rx) = mpsc::unbounded_channel();
        let (mcp_config_tx, mcp_config_rx) = mpsc::unbounded_channel();
        let (exec_command_tx, exec_command_rx) = std::sync::mpsc::channel();
        let (a2a_handoff_tx, a2a_handoff_rx) = mpsc::unbounded_channel();

        // The organization's setup document is resolved once, at session
        // start, before any MCP server can be dialed. A session bound to a
        // platform workspace with no reachable platform and no cache starts
        // with every MCP server refused; it never starts open.
        for notice in managed_setup.notices() {
            state.add_system_message(notice.clone());
        }
        if let Some(notice) = managed_setup.missing_required_skills_notice(
            loaded_skills
                .iter()
                .map(|skill| skill.definition.id.as_str()),
        ) {
            state.add_system_message(notice);
        }
        let managed_sandbox_required = managed_setup.is_managed()
            && !managed_setup.setup().sandbox_policy_toml.trim().is_empty();
        let (sandbox_policy, policy_resolution_failed) = match managed_setup
            .native_sandbox_policy(&workspace_dir, requested_sandbox_policy)
        {
            Ok(policy) => (policy, false),
            Err(error) => {
                state.add_system_message(state.locale.format("Sandbox policy could not be loaded ({0}); command execution is read-only for this session.", &[(error).to_string()]));
                (Some(crate::sandbox::SandboxPolicy::ReadOnly), true)
            }
        };
        let sandbox_policy = match sandbox_policy {
            Some(policy) if !crate::sandbox::is_sandbox_available() => {
                let reason = crate::sandbox::sandbox_unavailable_reason().unwrap_or_else(|| {
                    state
                        .locale
                        .translate("the native sandbox is unavailable")
                        .to_string()
                });
                if managed_sandbox_required || policy_resolution_failed {
                    state.add_system_message(state.locale.format("Required sandboxing is unavailable here: {0} Commands will fail closed until the environment is fixed and the session is restarted.", std::slice::from_ref(&(reason))));
                    Some(policy)
                } else {
                    state.add_system_message(state.locale.format("Native sandboxing was requested for this session but is not available here: {0} Running without the sandbox for this session. Fix the environment and restart to try again.", std::slice::from_ref(&(reason))));
                    None
                }
            }
            other => other,
        };
        // `spawn_agent` needs the same policy as the interactive tool executor.
        let stored_sandbox_policy = sandbox_policy.clone();
        let managed_mcp_policy = managed_setup
            .is_managed()
            .then(|| crate::mcp::ManagedMcpPolicy {
                version: managed_setup.version(),
                policy: managed_setup.mcp_policy().clone(),
            });

        let tool_executor = {
            let executor = ToolExecutor::with_credential_vault(&cwd, credential_vault.clone())
                .with_managed_mcp_policy(managed_mcp_policy)
                .with_code_authority();
            match sandbox_policy {
                Some(policy) => executor.with_sandbox_policy(policy),
                None => executor,
            }
        };

        let terminal_size = terminal.size().ok().map(|area| (area.width, area.height));

        let harness_store = match HarnessStore::load_default() {
            Ok(store) => store,
            Err(error) => {
                state.add_system_message(state.locale.format(
                    "Failed to load the continual harness; starting with an empty store: {0}",
                    &[format!("{:#}", error)],
                ));
                HarnessStore::default()
            }
        };
        let rlm_path = crate::rlm::default_path();
        let rlm_store = match RlmStore::load_from_path(&rlm_path) {
            Ok(store) => store,
            Err(error) => {
                state.add_system_message(state.locale.format(
                    "Failed to load RLM context; starting with an empty store: {0}",
                    &[format!("{:#}", error)],
                ));
                RlmStore::with_path(rlm_path)
            }
        };
        let mailbox_path = crate::mailbox::default_path();
        let mailbox_store = match MailboxStore::load_from_path(&mailbox_path) {
            Ok(store) => store,
            Err(error) => {
                state.add_system_message(state.locale.format(
                    "Failed to load the agent mailbox; starting with an empty store: {0}",
                    &[format!("{:#}", error)],
                ));
                MailboxStore::with_path(mailbox_path)
            }
        };

        state.set_output_detail(ui_prefs.output_detail());
        let configured_animations = app_config
            .tui
            .as_ref()
            .and_then(|tui| tui.animations)
            .unwrap_or(true);
        Self {
            state,
            native_agent: None,
            native_event_rx: None,
            tool_response_tx: None,
            tool_executor: Arc::new(tool_executor),
            credential_vault,
            terminal,
            terminal_clear_supported,
            terminal_size,
            terminal_events: None,
            should_quit: false,
            resume_target: None,
            capabilities,
            command_palette: CommandPalette::new(Arc::clone(&command_registry)),
            command_registry,
            slash_matcher,
            slash_state: SlashCycleState::new(),
            active_modal: ActiveModal::None,
            feedback_ui: bug_reports::FeedbackUi::default(),
            feedback_rating_rx: None,
            file_search: FileSearchModal::new(),
            workspace_files: Vec::new(),
            workspace_scan_rx: None,
            workspace_refresh_pending: false,
            session_switcher: SessionSwitcher::new(&cwd),
            operations: OperationsModal::new(&cwd),
            mcp_manager: McpManager::new(),
            approval_controller: ApprovalController::new(),
            sandbox_policy: stored_sandbox_policy,
            guardian: crate::safety::guardian::Guardian::from_env(app_config.model),
            guardian_tx,
            guardian_rx,
            pending_guardian_reviews: HashSet::new(),
            mcp_status_tx,
            mcp_status_rx,
            mcp_status_refresh_in_flight: false,
            mcp_config_tx,
            mcp_config_rx,
            mcp_config_in_flight: false,
            session_manager: SessionManager::new(&cwd),
            clipboard: ClipboardManager::new(),
            model_selector: ModelSelector::new(),
            theme_selector: ThemeSelector::new(),
            config_selector: crate::components::ConfigSelector::experiments(),
            setup_modal: SetupModal::new(),
            setup_login_rx: None,
            onboarding: onboarding::OnboardingSession::default(),
            pending_agent_spawn: false,
            current_model_user_set: false,
            shortcuts_help: ShortcutsHelp::new_with_binding_labels(keybinding_labels),
            rewind_picker: RewindPicker::new(),
            selective_summary: None,
            usage_tracker: crate::usage::UsageTracker::new(),
            active_turn_summary: None,
            active_turn_start_message_index: None,
            active_turn_started_at: None,
            prompt_history,
            tool_history: crate::tools::ToolHistory::default(),
            loaded_skills,
            skill_load_errors,
            skill_registry,
            plugin_registry,
            custom_prompts,
            exec_commands,
            exec_command_tx,
            exec_command_rx,
            a2a_handoff_tx,
            a2a_handoff_rx,
            initial_prompt: initial_prompt.filter(|p| !p.trim().is_empty()),
            queued_prompts: VecDeque::new(),
            dex_terminal: None,
            dex_pose_started: Instant::now(),
            dex_delight: Default::default(),
            queued_prompt_inflight: None,
            queued_prompt_active: None,
            next_queue_id: 1,
            plan_review_comments: Vec::new(),
            session_started_at: SystemTime::now(),
            session_resume_failed: false,
            current_model: String::new(),
            current_thinking_level: initial_thinking,
            pending_assistant_content: None,
            last_mcp_status_refresh: None,
            last_esc_at: None,
            loop_schedule: None,
            goal_store: GoalStore::load_for_process_start(),
            harness_store,
            rlm_store,
            mailbox_store,
            managed_setup_identity_scope,
            managed_setup,
            goal_auto_continue_armed: false,
            footer_style: ui_prefs.footer_style(),
            worker_badge: None,
            worker_badge_refreshed: std::time::Instant::now(),
            ui_prefs,
            configured_animations,
            pending_attachments: Vec::new(),
            draft_stash: None,
            history_search: None,
            last_mcp_server_statuses: HashMap::new(),
            config_watcher: build_mcp_config_watcher(),
            pending_model_change: None,
            model_monitor,
            model_verification_rx,
            local_model_discovery,
            local_model_discovery_rx,
            rubber_duck_rx: None,
            rubber_duck_running: false,
            current_git_branch: None,
            cycle_model_binding: keybindings.cycle_model,
            cycle_model_backward_binding: keybindings.cycle_model_backward,
            command_palette_binding: keybindings.command_palette,
            file_search_binding: keybindings.file_search,
            toggle_tool_outputs_binding: keybindings.toggle_tool_outputs,
            queued_follow_up_edit_binding: keybindings.edit_last_queued_follow_up,
            last_keybinding_issue_summary: keybinding_issue_summary,
            editing_queued_follow_up: None,
            submit_queued_steering_after_interrupt: false,
            restore_queued_prompts_after_interrupt: false,
            pending_checkpoint: None,
            pending_agent_tool_notes: Vec::new(),
            pending_agent_note_applications: Vec::new(),
            pending_agent_note_consumptions: Vec::new(),
            pending_consumed_agent_tool_notes: Vec::new(),
            ready_consumed_agent_tool_notes: Vec::new(),
            active_turn_assistant_messages_persisted: true,
            ephemeral_lifecycle_applications: HashSet::new(),
            terminal_notifier,
            terminal_session_started: false,
            detail_view: None,
            detail_return_modal: ActiveModal::None,
        }
    }

    /// Get the current viewport top position (for history push).
    pub fn viewport_top(&self) -> u16 {
        self.capabilities.viewport_top
    }

    fn build_base_system_prompt(cwd: &str) -> String {
        // The untrusted-content clause lives in
        // `agent::protocol::UNTRUSTED_CONTENT_POLICY` (single source of
        // truth, embedded here verbatim) so every native runtime sends the
        // same policy — see `ensure_untrusted_content_policy`.
        let policy = crate::agent::UNTRUSTED_CONTENT_POLICY;
        format!(
            r#"You are an AI assistant helping with software development tasks.

Current working directory: {cwd}

You have access to the following tools:
- bash: Execute shell commands. REQUIRED arg: {{\"command\":\"<cmd>\"}}. Do not send empty commands.
- read: Read file contents. REQUIRED: {{\"file_path\":\"/abs/path\"}}.
- write: Write to files. REQUIRED: {{\"file_path\":\"/abs/path\",\"content\":\"...\"}}.
- glob: Find files by pattern. REQUIRED: {{\"pattern\":\"*.rs\"}}. Optional: {{\"path\":\"/abs/dir\"}}.
- grep: Search file contents. REQUIRED: {{\"pattern\":\"regex or text\"}}. Optional: {{\"path\":\"/abs/dir\"}}.

Remembering corrections:
- When the user repeats a correction, offer to remember it. If `propose_harness_refinement` is available, stage a memory proposal in the narrowest applicable scope, with the exact correction and its session/turn sources as evidence. Use only actual user messages as evidence; do not invent source references. Tell the user to review it with `/memory review` and save it with `/memory save <proposal-id>`. A proposal stays inactive until the user explicitly saves it.

Hosted Computer delegation:
- When the user explicitly asks to "work on this in a Computer", "use a Computer", "send this to Computer", or equivalent, treat that as a request to delegate the whole task to the managed hosted Computer.
- Call `spawn_subagent` once with `backend: "computer"` and the user's complete task. Infer the role from the request: implementation work uses `code`, planning uses `plan`, review uses `review`, and read-only investigation uses `explore`.
- The runtime resolves the active workspace project and remote repository when the request omits them. Do not make the user manually provide those values unless the workspace has no safe remote origin.
- Do not call raw `mcp__...` Computer lifecycle tools, manually orchestrate create/start/wait/message, or silently fall back to the native backend. If Computer is unavailable or policy denies the launch, explain that clearly and stop.

Tool-calling rules:
- Always prefer read/write/glob/grep for filesystem; use bash only for commands that are not pure file ops.
- Never emit a tool call without all required fields.
- If a tool call is denied, immediately retry with corrected arguments instead of responding without action.

{policy}

Always use tools when they would be helpful. Be concise and direct in your responses."#
        )
    }

    #[cfg(test)]
    fn build_system_prompt_with_context(
        cwd: &str,
        current_year: i32,
        skills_section: Option<String>,
        harness_section: Option<String>,
        rlm_section: Option<String>,
        mailbox_section: Option<String>,
        active_prompt: &str,
    ) -> String {
        Self::build_system_prompt_assembly_with_context(SystemPromptAssemblyContext {
            cwd,
            current_year,
            skills: SkillsPromptSection {
                content: skills_section,
                source: "skills.loader.available_skills".to_string(),
                audit_only: false,
            },
            harness_section,
            rlm_section,
            mailbox_section,
            managed_rules_section: None,
            active_prompt,
        })
        .render()
    }

    fn build_system_prompt_assembly_with_context(
        ctx: SystemPromptAssemblyContext<'_>,
    ) -> PromptAssembly {
        let mut fragments = vec![PromptFragment::new(
            "base",
            "maestro.base_system_prompt",
            Self::build_base_system_prompt(ctx.cwd),
        )];

        if ctx.skills.audit_only {
            fragments.push(PromptFragment::audit_only(
                "loaded_skills",
                ctx.skills.source,
            ));
        } else if let Some(content) = ctx
            .skills
            .content
            .filter(|content| !content.trim().is_empty())
        {
            fragments.push(PromptFragment::new(
                "loaded_skills",
                ctx.skills.source,
                content,
            ));
        }

        let mut push_section = |name: &'static str, source: &str, section: Option<String>| {
            if let Some(content) = section.filter(|content| !content.trim().is_empty()) {
                fragments.push(PromptFragment::new(name, source, content));
            }
        };

        push_section("harness", "harness.store", ctx.harness_section);
        push_section("rlm", "rlm.store", ctx.rlm_section);
        push_section("mailbox", "mailbox.store", ctx.mailbox_section);
        // The organization's rules are a named fragment like any other, so the
        // prompt audit reports them and nothing has to match on prompt text.
        push_section(
            "managed_setup_rules",
            "managed_setup.rules",
            ctx.managed_rules_section,
        );

        fragments.push(PromptFragment::new(
            "current_year_hint",
            "maestro.current_year_hint",
            format!(
                "When using websearch/codesearch for up-to-date information, include the current year ({}) in the query unless the user specifies a different year or a historical range.",
                ctx.current_year
            ),
        ));
        let active_prompt = ctx.active_prompt.trim();
        if !active_prompt.is_empty() {
            fragments.push(PromptFragment::new(
                "active_skill_additions",
                "skills.registry.active_prompt_additions",
                active_prompt,
            ));
        }

        PromptAssembly::new(fragments)
    }

    fn initialize_terminal_events(&mut self) {
        if self.state.theme_follower.is_some() {
            if self.terminal_events.is_some() {
                // Discover whether mode 2031 is already active before
                // changing it, so cleanup preserves a parent process's mode.
                let _ = terminal::initialize_theme_reporting();
                self.state.last_theme_query = Some(Instant::now());
            } else if self.capabilities.enhanced_keys {
                // Compatibility path for terminals where uncurses cannot open
                // the controlling tty.
                crate::themes::osc11::apply_auto_theme_from_terminal();
            }
        }
    }

    fn poll_terminal_event(&mut self, timeout: Duration) -> Result<Option<AppTerminalEvent>> {
        if let Some(reader) = &mut self.terminal_events {
            return reader.poll(timeout).map_err(Into::into);
        }
        if event::poll(timeout)? {
            return Ok(AppTerminalEvent::from_crossterm(event::read()?));
        }
        Ok(None)
    }

    fn poll_terminal_theme(&mut self) {
        if !terminal_theme_query_due(
            self.terminal_events.is_some(),
            self.state.theme_follower.is_some(),
            self.state.theme_reporting_available,
            self.state.last_theme_query.map(|last| last.elapsed()),
        ) {
            return;
        }
        let _ = terminal::query_theme();
        self.state.last_theme_query = Some(Instant::now());
    }

    fn apply_terminal_theme_event(&mut self, event: &AppTerminalEvent) -> bool {
        // Theme replies may be unsolicited or left in the input queue from a
        // previous query. They must never override an explicit opt-out.
        if self.state.theme_follower.is_none() {
            return false;
        }
        let next = match event {
            AppTerminalEvent::ColorScheme(scheme) => {
                let next = match scheme {
                    uncurses::event::ColorScheme::Light => "light",
                    uncurses::event::ColorScheme::Dark => "dark",
                };
                self.state.theme_follower =
                    Some(crate::themes::osc11::AutoThemeFollower::new(next));
                Some(next)
            }
            AppTerminalEvent::BackgroundColor { red, green, blue } => {
                let luminance = crate::themes::osc11::relative_luminance(*red, *green, *blue);
                self.state
                    .theme_follower
                    .as_mut()
                    .and_then(|follower| follower.observe_luminance(luminance))
            }
            _ => None,
        };
        let Some(next) = next else {
            return false;
        };
        if crate::themes::current_theme_name() == next {
            return false;
        }
        if crate::themes::set_theme_by_name(next).is_ok() {
            return true;
        }
        false
    }

    /// Run the main event loop.
    ///
    /// # Rust Concept: Async Main Loop
    ///
    /// This function is `async` because the agent communication is async.
    /// The pattern here combines sync operations (terminal rendering, input)
    /// with async operations (agent polling) using a polling approach.
    ///
    /// # Rust Concept: `&mut self`
    ///
    /// Borrowing the App mutably lets the signal wrapper cancel this future,
    /// then retain the App long enough to finish orderly shutdown cleanup.
    ///
    /// # Returns
    ///
    /// Exit code for the process (0 = success, non-zero = error).
    pub async fn run(&mut self) -> Result<i32> {
        let result = self.run_inner().await;
        let disable_theme_reporting = self.prepare_terminal_restore();
        if disable_theme_reporting {
            let _ = terminal::disable_theme_reporting();
        }
        // Restore the terminal unconditionally: an error propagated out of
        // the loop (event poll/read, render, agent poll, ...) must not leave
        // raw mode, bracketed paste, and mouse capture enabled. The panic
        // hook only covers panics, not `?` propagation.
        let restore_result = terminal::restore();
        match (result, restore_result) {
            (Ok(code), Ok(())) => Ok(code),
            (Err(e), _) => Err(e),
            (Ok(_), Err(e)) => Err(e).context("Failed to restore terminal"),
        }
    }

    async fn run_inner(&mut self) -> Result<i32> {
        self.record_configuration_visibility();
        // Optional Jane Street magic-trace slow-frame snapshots (Linux/Intel PT).
        if crate::magic_trace::init_from_env() {
            eprintln!(
                "[magic-trace] slow-frame stop indicator enabled (budget {}µs). Attach with scripts/magic-trace-tui.sh",
                std::env::var("MAESTRO_MAGIC_TRACE_FRAME_BUDGET_MS")
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(16)
                    * 1000
            );
        }

        // Paint the chrome immediately. Workspace file indexing used to run
        // *before* the first frame and blocked on `rg --files --follow` of the
        // entire cwd (often `$HOME`), so typing `maestro` looked hung/broken.
        self.render()?;

        // Save the terminal title and show the idle state (OSC title stack).
        self.terminal_session_started = true;
        let startup_seqs = self.terminal_notifier.session_started();
        Self::write_terminal_sequences(&startup_seqs);

        // Index @-mention files with a bounded, killable scan (see workspace.rs).
        // Kick it off on a background thread so agent spawn is not gated on it.
        self.spawn_workspace_scan();

        // Spawn the agent (async operation).
        // This creates the channels and starts the agent task.
        self.spawn_agent().await?;
        if self.maybe_open_first_run_setup() {
            self.render()?;
        }

        // Grok-style trailing prompt: submit after the agent is ready and any
        // initial local-runtime metadata has refreshed its request budgets.
        if self.active_modal != ActiveModal::Setup {
            if let Some(prompt) = self.take_initial_prompt_after_discovery().await {
                let _ = self.submit_prompt(prompt).await;
            }
        }

        // Main event loop - runs until should_quit is set to true.
        // Only repaint when something changed (or while busy for spinners).
        // Idle 20Hz full-buffer diffs were the top cost after FS badge work
        // (perf on developer@dev-desktop → ratatui Buffer::diff / unicode_width).
        let mut needs_redraw = true;
        loop {
            // Apply workspace scan results as soon as they arrive (non-blocking).
            if self.poll_workspace_scan() {
                needs_redraw = true;
            }

            // Apply finished executable slash command runs (non-blocking).
            if self.poll_exec_commands() {
                needs_redraw = true;
            }

            if self.poll_a2a_handoffs() {
                needs_redraw = true;
            }

            // Drain agent output before entering the blocking terminal poll so
            // an already-queued first token is not held for the 33ms busy tick.
            let mut agent_activity = self.poll_agent().await?;
            if agent_activity {
                needs_redraw = true;
            }
            if self.poll_selective_summary() {
                needs_redraw = true;
            }
            if self.poll_setup_login() {
                needs_redraw = true;
            }
            if self.poll_onboarding() {
                needs_redraw = true;
            }
            if self.pending_agent_spawn {
                self.pending_agent_spawn = false;
                self.spawn_agent().await?;
                needs_redraw = true;
                if self.native_agent.is_some() && self.active_modal != ActiveModal::Setup {
                    if let Some(prompt) = self.take_initial_prompt_after_discovery().await {
                        let _ = self.submit_prompt(prompt).await;
                    }
                }
            }

            // A startup prompt held by setup resumes after any exit path, including feedback.
            if self.initial_prompt.is_some()
                && self.native_agent.is_some()
                && self.active_modal == ActiveModal::None
                && !self.state.busy
            {
                if let Some(prompt) = self.take_initial_prompt_after_discovery().await {
                    let _ = self.submit_prompt(prompt).await;
                    needs_redraw = true;
                }
            }

            // Poll faster while busy so the working-state sheen and spinners
            // advance. Idle welcome screens stay still on the normal cadence.
            let poll_timeout = terminal_poll_timeout(self.state.busy, agent_activity);
            self.poll_terminal_theme();
            if let Some(event) = self.poll_terminal_event(poll_timeout)? {
                match event {
                    AppTerminalEvent::Key(key) if should_handle_key_event(key.kind) => {
                        self.handle_key(key.code, key.modifiers).await?;
                        needs_redraw = true;
                    }
                    AppTerminalEvent::Mouse(mouse) => {
                        if mouse.kind == MouseEventKind::Down(crossterm::event::MouseButton::Left)
                            && self.active_modal == ActiveModal::None
                            && self.dex_hit(mouse.column, mouse.row)
                        {
                            self.pet_dex();
                            needs_redraw = true;
                            continue;
                        }
                        // Handle mouse scroll wheel
                        match mouse.kind {
                            MouseEventKind::ScrollUp => {
                                self.state.scroll_up(3);
                                needs_redraw = true;
                            }
                            MouseEventKind::ScrollDown => {
                                self.state.scroll_down(3);
                                needs_redraw = true;
                            }
                            MouseEventKind::Down(crossterm::event::MouseButton::Left)
                                if self.active_modal == ActiveModal::None
                                    && self.history_search.is_none()
                                    && self.slash_state.has_completions() =>
                            {
                                // Click-to-select on the slash completion popup.
                                if let Ok(size) = self.terminal.size() {
                                    let area = Rect::new(0, 0, size.width, size.height);
                                    let popup = Self::slash_popup_area(
                                        area,
                                        self.slash_state.completions().len(),
                                    );
                                    let in_x = mouse.column >= popup.x
                                        && mouse.column < popup.x + popup.width;
                                    let in_y = mouse.row > popup.y
                                        && mouse.row < popup.y + popup.height - 1;
                                    if in_x && in_y {
                                        let offset = self.slash_state.list_state_mut().offset();
                                        let index = offset + (mouse.row - popup.y - 1) as usize;
                                        self.slash_state.select(index);
                                        self.apply_slash_completion();
                                        self.update_slash_state();
                                        needs_redraw = true;
                                    }
                                }
                            }
                            _ => {} // Ignore other mouse events
                        }
                    }
                    AppTerminalEvent::Resize { width, height } => {
                        self.handle_resize(width, height)?;
                        needs_redraw = true;
                    }
                    // Bracketed paste: route to the open modal's text input,
                    // or to the main input (large pastes fold into a
                    // `[Pasted: N lines]` display chip).
                    AppTerminalEvent::Paste(text) => {
                        self.handle_paste(&text);
                        needs_redraw = true;
                    }
                    AppTerminalEvent::FocusGained => {
                        self.terminal_notifier.record_focus(true);
                        self.dex_focus_returned();
                        needs_redraw = true;
                    }
                    AppTerminalEvent::FocusLost => {
                        self.terminal_notifier.record_focus(false);
                        self.dex_focus_lost();
                    }
                    AppTerminalEvent::ThemeReportingStatus(setting) => {
                        self.state.theme_reporting_available = setting.is_available();
                        if setting == uncurses::ansi::mode::ModeSetting::Reset {
                            let _ = terminal::enable_theme_reporting();
                        }
                    }
                    theme_event @ (AppTerminalEvent::BackgroundColor { .. }
                    | AppTerminalEvent::ColorScheme(_)) => {
                        needs_redraw |= self.apply_terminal_theme_event(&theme_event);
                    }
                    _ => {}
                }
            }

            // Also drain messages that arrived while the terminal poll was in
            // progress; the pre-poll drain handles messages already queued.
            agent_activity |= self.poll_agent().await?;
            if agent_activity {
                needs_redraw = true;
            }
            self.record_agent_tool_note_progress(false);

            // Apply finished guardian reviews (spawned per approval request).
            if self.poll_guardian_verdicts().await? {
                needs_redraw = true;
            }

            self.sync_dex_attention();

            if self.poll_model_verification() {
                needs_redraw = true;
            }

            if self.poll_local_model_discovery() {
                needs_redraw = true;
            }

            if self.poll_rubber_duck() {
                needs_redraw = true;
            }

            // Drain live MCP notifications so list changes refresh the UI
            // without waiting for a reconnect or a manual status check.
            if self.poll_mcp_updates().await {
                needs_redraw = true;
            }

            // Apply MCP config changes before the periodic refresh so edits
            // show up in the footer as soon as the watcher delivers them.
            if self.poll_config_watcher().await {
                needs_redraw = true;
            }

            if self.poll_mcp_status_refresh() {
                needs_redraw = true;
            }

            if self.poll_mcp_config() {
                needs_redraw = true;
            }

            // Refresh MCP badge counts periodically without blocking the UI.
            if self.refresh_mcp_badges().await {
                needs_redraw = true;
            }

            if self.worker_badge_refreshed.elapsed() >= Duration::from_secs(1) {
                self.worker_badge_refreshed = std::time::Instant::now();
                let next = match self.tool_executor.worker_activity() {
                    Ok((0, 0)) => None,
                    Ok((running, waiting)) => Some(self.state.locale.format(
                        "↗ {0} running · {1} need input /workers",
                        &[(running).to_string(), (waiting).to_string()],
                    )),
                    Err(_) => Some(
                        self.state
                            .locale
                            .translate("↗ workers unavailable /workers")
                            .into(),
                    ),
                };
                if next != self.worker_badge {
                    self.worker_badge = next;
                    needs_redraw = true;
                }
            }

            if self.operations.poll_load() {
                needs_redraw = true;
            }

            for event in crate::tools::background_tasks::poll_monitor_events() {
                self.operations.add_monitor_event(&event);
                needs_redraw = true;
            }

            for event in crate::tools::background_tasks::poll_task_lifecycle_events() {
                let code = event
                    .exit_code
                    .map(|c| format!(" exit={c}"))
                    .unwrap_or_default();
                let cmd: String = event.command.chars().take(64).collect();
                self.state.add_system_message(self.state.locale.format(
                    "Background task {0} **{1}**{2}: `{3}`",
                    &[
                        (event.task_id).clone(),
                        (event.status).clone(),
                        (code).clone(),
                        (cmd).clone(),
                    ],
                ));
                self.pending_agent_tool_notes.push(PendingAgentToolNote {
                    application_id: None,
                    content: format_background_task_tool_note(&event),
                });
                needs_redraw = true;
            }
            for event in self.tool_executor.poll_subagent_lifecycle_events() {
                let status = format!("{:?}", event.status).to_ascii_lowercase();
                let outcome = event
                    .error
                    .as_deref()
                    .or(event.summary.as_deref())
                    .unwrap_or(self.state.locale.translate("no summary"));
                let projection = maestro_runtime::DelegationEvent::from_subagent_lifecycle(
                    &event.mailbox_message_id,
                    &event.subagent_id,
                    event.attempt,
                    &status,
                    Some(outcome),
                    None,
                );
                let lifecycle_message =
                    projection.native_summary(self.state.locale.translate("Subagent"));
                let agent_note =
                    projection.native_agent_note(self.state.locale.translate("Subagent"));
                let Some(already_applied) = self.subagent_lifecycle_application_exists(&event)
                else {
                    continue;
                };
                if !already_applied {
                    if !self.record_subagent_lifecycle_application(
                        &event,
                        lifecycle_message.clone(),
                        agent_note.clone(),
                    ) {
                        continue;
                    }
                    self.state.add_system_message(lifecycle_message);
                    self.pending_agent_tool_notes.push(PendingAgentToolNote {
                        application_id: Some(event.mailbox_message_id.clone()),
                        content: agent_note,
                    });
                }
                if let Err(error) = self
                    .tool_executor
                    .acknowledge_subagent_lifecycle_event(&event)
                {
                    self.state.error = Some(error);
                }
                needs_redraw = true;
            }
            if !self.state.busy {
                self.flush_pending_agent_tool_notes();
            }

            // Fire a due /loop prompt. Loops never interrupt a running turn:
            // a due prompt waits for idle and then submits.
            let loop_prompt = if self.state.busy {
                None
            } else {
                self.loop_schedule.as_mut().and_then(|schedule| {
                    if Instant::now() >= schedule.next_fire {
                        schedule.next_fire = Instant::now() + schedule.interval;
                        Some(schedule.prompt.clone())
                    } else {
                        None
                    }
                })
            };
            if let Some(prompt) = loop_prompt {
                self.state.status.replace(
                    self.state
                        .locale
                        .format("Loop: \"{0}\"", std::slice::from_ref(&prompt)),
                );
                self.submit_prompt(prompt).await?;
                needs_redraw = true;
            }

            // Goal auto-continue (Codex-style): after idle, if the goal is still
            // active (worker did not call update_goal complete|blocked), inject
            // a continuation prompt. No second model. Do not burn the safety
            // counter or spin when no agent is available (e.g. missing API key).
            let queue_or_loop_busy = self.loop_schedule.is_some()
                || !self.queued_prompts.is_empty()
                || self.queued_prompt_inflight.is_some()
                || self.queued_prompt_active.is_some();
            // Reload before the idle decision so a worker `update_goal` that
            // completed while we still hold a stale Active snapshot cannot
            // re-arm another continuation (and rewrite goals.json as Active).
            if !self.state.busy && !queue_or_loop_busy && self.goal_auto_continue_armed {
                self.goal_store.reload_from_disk();
                match self.goal_store.enforce_limits() {
                    Ok(Some(reason)) => {
                        self.goal_auto_continue_armed = false;
                        self.state.status.replace(
                            self.state
                                .locale
                                .translate("Goal auto-continue stopped (wall-clock budget)")
                                .to_string(),
                        );
                        self.state.add_system_message(self.state.locale.format("Goal auto-continue stopped: {0}. Use `/goal resume` to start a new time window.", std::slice::from_ref(&(reason))));
                        needs_redraw = true;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        self.goal_auto_continue_armed = false;
                        self.state.error = Some(self.state.locale.format(
                            "Goal wall-clock budget could not be persisted: {0}",
                            &[format!("{:#}", error)],
                        ));
                        needs_redraw = true;
                    }
                }
                if !self.goal_store.should_auto_continue() {
                    self.goal_auto_continue_armed = false;
                }
            }
            if !self.state.busy
                && !queue_or_loop_busy
                && self.goal_auto_continue_armed
                && self.goal_store.should_auto_continue()
            {
                if self.native_agent.is_none() {
                    // Stay armed so a later successful agent start can continue,
                    // but do not increment auto_continue_count or call submit.
                    self.goal_auto_continue_armed = false;
                    self.state.status.replace(
                        self.state.locale.translate("Goal auto-continue waiting for agent (sign in to EvalOps Identity, then configure a provider)").to_string(),
                    );
                    needs_redraw = true;
                } else if let Some(prompt) = self.goal_store.continuation_prompt() {
                    self.goal_auto_continue_armed = false;
                    // Check the safety cap *before* submitting so a failed send
                    // never consumes a turn.
                    let at_cap =
                        self.goal_store.current.as_ref().is_some_and(|g| {
                            g.auto_continue_count.saturating_add(1) >= g.max_turns
                        });
                    if at_cap {
                        let max = self
                            .goal_store
                            .current
                            .as_ref()
                            .map(|g| g.max_turns)
                            .unwrap_or(0);
                        let _ = self.goal_store.note_auto_continue_submitted();
                        self.state.status.replace(self.state.locale.format(
                            "Goal auto-continue stopped (safety max {0})",
                            &[(max).to_string()],
                        ));
                        self.state.add_system_message(self.state.locale.format("Goal auto-continue hit safety max_turns={0}. Mark goals done with the `update_goal` tool (or `/goal complete`). Use `/goal resume` or `/goal auto on` to re-arm.", &[(max).to_string()]));
                        needs_redraw = true;
                    } else {
                        self.state.status.replace(
                            self.state
                                .locale
                                .translate("Goal auto-continue")
                                .to_string(),
                        );
                        match self
                            .submit_prompt_with_kind(prompt, crate::agent::PromptKind::Prompt)
                            .await
                        {
                            Ok(true) => {
                                if let Err(e) = self.goal_store.note_auto_continue_submitted() {
                                    self.state.error = Some(self.state.locale.format(
                                        "Goal auto-continue bookkeeping failed: {0}",
                                        &[(e).to_string()],
                                    ));
                                }
                                needs_redraw = true;
                            }
                            Ok(false) => {
                                // Agent missing or session blocked — do not burn
                                // the safety counter. Leave disarmed.
                                self.state.status.replace(
                                    self.state
                                        .locale
                                        .translate("Goal auto-continue skipped (agent unavailable)")
                                        .to_string(),
                                );
                                needs_redraw = true;
                            }
                            Err(e) => {
                                self.state.error = Some(self.state.locale.format(
                                    "Goal auto-continue failed to submit: {0}",
                                    &[(e).to_string()],
                                ));
                                // Leave disarmed so we do not tight-loop on a
                                // permanent submit failure.
                                needs_redraw = true;
                            }
                        }
                    }
                } else {
                    self.goal_auto_continue_armed = false;
                }
            }

            // Paint when dirty, or continuously while busy (thinking/spinner).
            let dex_hop_active = self.dex_terminal
                == Some(crate::components::dex_companion::DexCompanionState::Finished)
                && self.dex_pose_started.elapsed() < Duration::from_millis(800)
                && self
                    .ui_prefs
                    .animations
                    .unwrap_or(self.configured_animations)
                && self.ui_prefs.dex_personality()
                    != crate::components::dex_companion::DexPersonality::Quiet;
            if needs_redraw
                || self.state.busy
                || dex_hop_active
                || self.dex_pet_active()
                || self.onboarding_animation_active()
            {
                self.render()?;
                if !self.state.busy {
                    needs_redraw = false;
                }
            }

            // Check exit condition.
            if self.should_quit {
                break;
            }
        }

        // Close the active session for hooks before teardown. This is the
        // orderly exit; a crash or a kill cannot run it, which is stated in
        // docs/design/HOOKS_SYSTEM.md so nobody builds cleanup that depends on
        // `SessionEnd` always arriving.
        if self.state.session_id.is_some() {
            if let Some(event) = self.new_session_event(
                maestro_runtime_contracts::SessionEventLane::Runtime,
                "session.closed",
                maestro_runtime_contracts::SessionEventPhase::Completed,
            ) {
                self.record_session_event(event);
                self.flush_session();
            }
            self.adopt_session_context(None, "exit");
        }

        // Cleanup background processes before exit
        let process_count = crate::tools::cleanup_background_processes();
        if process_count > 0 {
            eprintln!("[app] Cleaned up {process_count} background process(es)");
        }

        // Clear tab progress and restore the original terminal title.
        let shutdown_seqs = self.terminal_session_ended_sequences();
        Self::write_terminal_sequences(&shutdown_seqs);

        // Terminal restore happens in `run`, which wraps this inner loop so
        // the terminal is restored even when the loop exits with an error.
        Ok(0)
    }

    /// Handle a terminal resize: recompute viewport capabilities (PageUp/
    /// PageDown step size) and resize the inline viewport itself.
    fn handle_resize(&mut self, width: u16, height: u16) -> Result<()> {
        self.terminal_size = Some((width, height));
        self.state
            .set_input_width(crate::components::composer_editor_width(width));
        if self.update_viewport_capabilities(height) {
            // ratatui's inline viewport height is fixed at construction, so
            // growing/shrinking it requires rebuilding the Terminal around
            // the same device handle. Width-only changes are handled by
            // ratatui's autoresize on the next draw.
            self.terminal = terminal::recreate_with_viewport(self.capabilities.viewport_height)?;
        }
        Ok(())
    }

    /// Recompute viewport capabilities for a new terminal height.
    /// Returns true when they changed.
    fn update_viewport_capabilities(&mut self, height: u16) -> bool {
        let (viewport_top, viewport_height) = terminal::calculate_viewport(height);
        let changed = viewport_top != self.capabilities.viewport_top
            || viewport_height != self.capabilities.viewport_height;
        self.capabilities.viewport_top = viewport_top;
        self.capabilities.viewport_height = viewport_height;
        changed
    }

    /// Scan workspace files on a background thread.
    ///
    /// The `rg --files --follow` scan of the whole cwd can take seconds, so
    /// it must never run on the UI thread; the event loop polls the receiver
    /// via `poll_workspace_scan` and applies the result when it arrives.
    fn spawn_workspace_scan(&mut self) {
        let (workspace_tx, workspace_rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("maestro-workspace-scan".into())
            .spawn(move || {
                let cwd = std::env::current_dir().unwrap_or_default();
                let files = get_workspace_files(&cwd, 10_000);
                let _ = workspace_tx.send(files);
            })
            .ok();
        self.workspace_scan_rx = Some(workspace_rx);
    }

    /// Apply background workspace scan results as soon as they arrive
    /// (non-blocking). Returns true when the file list changed.
    fn poll_workspace_scan(&mut self) -> bool {
        let Some(rx) = self.workspace_scan_rx.take() else {
            return false;
        };
        match rx.try_recv() {
            Ok(files) => {
                self.workspace_files.clone_from(&files);
                self.file_search.set_files(files);
                if std::mem::take(&mut self.workspace_refresh_pending) {
                    self.state.status = Some(
                        self.state
                            .locale
                            .translate("Workspace files refreshed")
                            .to_string(),
                    );
                }
                true
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.workspace_refresh_pending = false;
                false
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                self.workspace_scan_rx = Some(rx);
                false
            }
        }
    }

    /// Spawn the native Rust agent
    async fn spawn_agent(&mut self) -> Result<()> {
        let cwd_path = std::env::current_dir().unwrap_or_default();
        let cwd = cwd_path.to_string_lossy().to_string();

        // Detect git branch
        let git_branch = git::current_branch(&cwd_path);
        self.current_git_branch = git_branch.clone();

        // Prefer a model the user just picked in `/setup` or `/model`.
        // Otherwise: MAESTRO_MODEL, then ~/.maestro (and project) model
        // settings, then Codex ChatGPT login, then the platform default.
        let model = if self.current_model_user_set && !self.current_model.trim().is_empty() {
            self.current_model.clone()
        } else {
            crate::codex_auth::resolve_default_model()
        };

        let (history, session_id, thinking_level) = self.agent_context_for_spawn()?;
        let thinking_level = crate::model_dynamics::normalize_thinking(&model, thinking_level);
        self.current_thinking_level = thinking_level;
        self.state.thinking_level = thinking_level;
        let (thinking_enabled, thinking_budget) = thinking_level.to_config();
        let config = NativeAgentConfig {
            model_capabilities: None,
            model_dynamics: crate::config::model_dynamics_config(),
            model: model.clone(),
            max_tokens: crate::model_catalog::default_max_output_tokens(&model),
            max_tokens_source: MaxTokensSource::Catalog,
            system_prompt: Some(self.build_system_prompt()),
            thinking_enabled,
            thinking_budget,
            cwd: cwd.clone(),
            approval_mode: self.state.approval_mode,
            context_window: self.state.context_window,
            // See the `sandbox_policy` field doc on `App`: without this,
            // only calls reaching the human approval modal via `self.tool_executor` were ever
            // sandboxed. Yolo mode and Selective mode's allowlisted calls
            // run through the native agent runner's own executor instead.
            sandbox_policy: self.sandbox_policy.clone(),
            managed_mcp_policy: self.managed_setup.is_managed().then(|| {
                crate::mcp::ManagedMcpPolicy {
                    version: self.managed_setup.version(),
                    policy: self.managed_setup.mcp_policy().clone(),
                }
            }),
            external_tool_schema_policy: ExternalToolSchemaPolicy::Eager,
            max_turn_steps: crate::agent::DEFAULT_MAX_TURN_STEPS,
            allow_unbounded_turn: false,
            retry_config: crate::agent::retry::RetryConfig::default(),
        };

        let policy_model = policy_model_id(&model);
        if let Some(reason) = check_model_allowed(&policy_model) {
            self.state.error = Some(reason);
            return Ok(());
        }

        self.current_model = model.clone();
        self.state.thinking_level = thinking_level;
        self.usage_tracker.set_model(model.clone());

        self.state.status = Some(self.state.locale.format(
            "Initializing agent ({0})...",
            std::slice::from_ref(&(model)),
        ));

        let subagent_parent_scope_id = self.tool_executor.subagent_parent_scope_id();
        match NativeAgent::new_with_credential_vault_and_subagent_scope(
            config,
            self.credential_vault.clone(),
            subagent_parent_scope_id,
        ) {
            Ok((agent, event_rx)) => {
                self.restore_request_cache(&agent);
                let tool_tx = agent.tool_response_sender();
                self.native_agent = Some(agent);
                self.native_event_rx = Some(event_rx);
                self.tool_response_tx = Some(tool_tx);

                // A session restored at startup exists before the runner does,
                // so it never passed through any of the transitions that adopt
                // a session context. Capture it here and adopt it below.
                let restored_session_id = session_id.clone();

                // Send ready event
                if let Some(agent) = &self.native_agent {
                    agent.set_goal_tools_visible(self.goal_store.tools_visible());
                    // Startup session restore runs before the agent exists. Queue
                    // the copied conversation before any initial prompt so a
                    // fork continues with the same model context.
                    let continuation = match self.session_manager.current_session_path() {
                        Some(path) => crate::session::SessionReader::read_file(&path)?
                            .compactions
                            .last()
                            .and_then(|entry| entry.continuation.clone()),
                        None => None,
                    };
                    agent.replace_history_with_continuation(history, continuation);
                    agent.send_ready();
                    // Send session info with git branch
                    agent.send_session_info(&cwd, session_id, git_branch);
                    let _ = agent.set_steering_mode(self.state.steering_mode);
                    let _ = agent.set_follow_up_mode(self.state.follow_up_mode);
                }

                // `send_session_info` is an outbound event to the UI and sets
                // nothing on the runner. Without this the runner kept the tool
                // executor's initial random subagent scope and a hook
                // `session_id` of `None`, so completions parked for this
                // session's own children were never drained and every hook
                // payload published a null session.
                if let Some(restored_session_id) = restored_session_id {
                    self.adopt_session_context(Some(&restored_session_id), "restore");
                }

                // Ensure busy is false so user can type
                self.state.busy = false;
                self.state.model = Some(model.clone());
                self.state.status = Some(
                    self.state
                        .locale
                        .format("Ready: {0}", std::slice::from_ref(&(model))),
                );
                self.flush_pending_agent_tool_notes();
            }
            Err(e) => {
                if should_open_first_run_setup(false, default_model_credentials_ready()) {
                    self.state.status = Some(
                        self.state
                            .locale
                            .translate("Complete setup to start.")
                            .to_string(),
                    );
                } else {
                    let mut message = self
                        .state
                        .locale
                        .format("Failed to create agent: {0}", &[(e).to_string()]);
                    if crate::codex_auth::read_codex_auth().is_none()
                        && std::env::var_os("OPENAI_API_KEY").is_none()
                        && std::env::var_os("OPENAI_CODEX_TOKEN").is_none()
                    {
                        message.push_str(
                            self.state.locale.translate(" — run `deixic-code evalops login` (required), then configure a provider if needed."),
                        );
                    }
                    self.state.error = Some(message);
                }
            }
        }

        Ok(())
    }

    fn agent_context_for_spawn(
        &mut self,
    ) -> Result<(Vec<crate::ai::Message>, Option<String>, ThinkingLevel)> {
        self.session_manager.flush()?;
        if let Some(path) = self.session_manager.current_session_path() {
            let session = crate::session::SessionReader::read_file(&path)?;
            return Ok((
                crate::session::model_history(&session),
                self.state.session_id.clone(),
                self.current_thinking_level,
            ));
        }
        let history = self
            .state
            .messages
            .iter()
            .filter(|message| message.kind == MessageKind::Regular)
            .filter_map(|message| match message.role {
                MessageRole::User => Some(crate::ai::Message {
                    role: crate::ai::Role::User,
                    content: crate::ai::MessageContent::text(message.content.clone()),
                }),
                MessageRole::Assistant if message.is_assistant_reply() => {
                    Some(crate::ai::Message {
                        role: crate::ai::Role::Assistant,
                        content: crate::ai::MessageContent::text(message.content.clone()),
                    })
                }
                _ => None,
            })
            .collect();
        Ok((
            history,
            self.state.session_id.clone(),
            self.current_thinking_level,
        ))
    }

    fn build_system_prompt_assembly(&self) -> PromptAssembly {
        let cwd = std::env::current_dir()
            .map_or_else(|_| ".".to_string(), |p| p.to_string_lossy().to_string());
        let current_year = Utc::now().year();
        let mut skills_source = "skills.loader.available_skills".to_string();
        let mut skills_audit_only = false;
        let skills_section = if self.loaded_skills.is_empty() {
            None
        } else {
            // The catalog is bounded at a share of the model's context window
            // rather than rendered whole: a large plugin set would otherwise
            // spend an unbounded part of the prompt listing skills. The rung
            // the ladder reached is stamped on the rendered block as
            // `budget_strategy`, so `/prompt-audit` shows the degradation.
            let entries: Vec<crate::skills::SkillCatalogEntry> = self
                .loaded_skills
                .iter()
                .map(crate::skills::SkillCatalogEntry::from_loaded)
                .collect();
            let budget = crate::skills::skill_catalog_budget_tokens(&self.current_model);
            match crate::skills::apply_skill_catalog_budget(&entries, budget) {
                Ok((rendered, strategy)) => {
                    skills_source = format!("skills.loader.available_skills#{}", strategy.as_str());
                    Some(rendered)
                }
                Err(error) => {
                    skills_source = format!(
                        "skills.loader.available_skills#omitted_budget_exceeded?budget_tokens={}&required_tokens={}&protected_count={}",
                        error.budget_tokens, error.required_tokens, error.protected_count
                    );
                    skills_audit_only = true;
                    tracing::warn!(
                        budget_tokens = error.budget_tokens,
                        required_tokens = error.required_tokens,
                        protected_count = error.protected_count,
                        "omitting skills catalog because its minimum representation exceeds the budget"
                    );
                    None
                }
            }
        };
        let harness_section = self
            .harness_store
            .prompt_section(std::path::Path::new(&cwd), self.state.session_id.as_deref());
        let rlm_section = self.rlm_store.prompt_section();
        let mailbox_section = self
            .mailbox_store
            .prompt_section_for(&crate::mailbox::local_identity());
        let active_prompt = self.skill_registry.active_system_prompt_additions();
        let managed_rules_section = self.managed_setup.rules_prompt_section();

        Self::build_system_prompt_assembly_with_context(SystemPromptAssemblyContext {
            cwd: &cwd,
            current_year,
            skills: SkillsPromptSection {
                content: skills_section,
                source: skills_source,
                audit_only: skills_audit_only,
            },
            harness_section,
            rlm_section,
            mailbox_section,
            managed_rules_section,
            active_prompt: &active_prompt,
        })
    }

    /// Build the system prompt for the agent.
    fn build_system_prompt(&self) -> String {
        self.build_system_prompt_assembly().render()
    }

    /// The team sandbox policy source for this session.
    ///
    /// `sandbox_policy::resolve_effective_policy` takes a
    /// `&dyn TeamPolicyProvider` for the administrator's document; for a
    /// platform-bound session that document arrives inside the managed setup.
    #[must_use]
    pub fn team_policy_provider(&self) -> &dyn crate::sandbox_policy::TeamPolicyProvider {
        &self.managed_setup
    }

    fn build_prompt_audit_report(&self) -> PromptAuditReport {
        let model = (!self.current_model.trim().is_empty()).then_some(self.current_model.as_str());
        let active_skill_ids = self
            .skill_registry
            .active_skills()
            .into_iter()
            .map(|skill| skill.definition.id.clone());
        let tools = self.tool_executor.tool_definitions().map(|definition| {
            (
                definition.tool.name.clone(),
                definition.requires_approval,
                definition.tool.description.clone(),
                definition.tool.input_schema.clone(),
            )
        });
        let mut report = self
            .build_system_prompt_assembly()
            .audit(model, active_skill_ids)
            .with_registered_tools(tools);
        if let Some(agent) = &self.native_agent {
            let effective = agent.runtime_audit_snapshot();
            report = report.with_effective_runtime(
                effective.prompt_revision,
                effective.system_prompt.as_deref(),
                effective.tools.into_iter().map(|definition| {
                    (
                        definition.tool.name,
                        definition.requires_approval,
                        definition.tool.description,
                        definition.tool.input_schema,
                    )
                }),
            );
        }
        report
    }

    fn refresh_skills(&mut self, preserve_active: bool) {
        let active_ids: HashSet<String> = if preserve_active {
            self.skill_registry
                .active_skills()
                .iter()
                .map(|skill| skill.definition.id.clone())
                .collect()
        } else {
            HashSet::new()
        };

        self.plugin_registry = PluginRegistry::discover();
        let loader = SkillLoader::with_plugins(&self.plugin_registry);
        let (loaded_skills, skill_load_errors) = loader.load_all_with_paths();
        let mut registry = SkillRegistry::new();
        for loaded in &loaded_skills {
            registry.register(loaded.definition.clone());
        }
        if preserve_active {
            for id in active_ids {
                let _ = registry.activate(&id);
            }
        }

        self.loaded_skills = loaded_skills;
        self.skill_load_errors = skill_load_errors;
        self.skill_registry = registry;

        let workspace_dir =
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        if let Some(notice) = crate::localization::with_locale(self.state.locale, || {
            untrusted_workspace_notice(&workspace_dir, &self.plugin_registry)
        }) {
            self.state.add_system_message(notice);
        }
        let plugin_command_dirs = self.plugin_registry.command_paths();
        self.custom_prompts =
            crate::prompts::load_prompts_with_plugin_dirs(&workspace_dir, &plugin_command_dirs);
        self.exec_commands =
            crate::exec_commands::discover_with_plugin_dirs(&workspace_dir, &plugin_command_dirs);
        self.rebuild_slash_registry();
    }

    fn rebuild_slash_registry(&mut self) {
        let mut registry =
            build_command_registry_with_extensions(&self.loaded_skills, &self.custom_prompts);
        let skipped_exec =
            crate::commands::register_exec_commands(&mut registry, &self.exec_commands);
        if !skipped_exec.is_empty() {
            self.state
                .add_system_message(exec_commands::exec_collision_warning(&skipped_exec));
        }
        let registry = Arc::new(registry);
        self.command_registry = Arc::clone(&registry);
        self.slash_matcher = SlashCommandMatcher::new(Arc::clone(&registry));
        self.command_palette.update_registry(registry);
    }

    fn format_skill_invoke(skill: &LoadedSkill, raw_args: &str) -> String {
        let body = skill
            .definition
            .system_prompt_additions
            .as_deref()
            .unwrap_or("")
            .trim();
        let args = raw_args.trim();

        if body.contains('$') {
            let parsed = parse_args(raw_args);
            let pseudo = PromptDefinition {
                name: skill.definition.name.clone(),
                description: None,
                argument_hint: None,
                body: body.to_string(),
                source_path: skill.source_path.clone(),
                source_type: crate::prompts::PromptSource::User,
                named_placeholders: Vec::new(),
                has_positional_placeholders: true,
            };
            return render_prompt(&pseudo, &parsed);
        }

        if args.is_empty() {
            if body.is_empty() {
                format!("Use the \"{}\" skill.", skill.definition.name)
            } else {
                body.to_string()
            }
        } else if body.is_empty() {
            args.to_string()
        } else {
            format!("{body}\n\n---\n\n{args}")
        }
    }

    fn resolve_skill_id(&self, query: &str) -> Result<String, String> {
        let normalized = query.trim().to_lowercase();
        if normalized.is_empty() {
            return Err(self
                .state
                .locale
                .translate("Skill name required")
                .to_string());
        }

        let mut partial_matches: Vec<String> = Vec::new();
        for loaded in &self.loaded_skills {
            let def = &loaded.definition;
            let id = def.id.clone();
            let id_lower = id.to_lowercase();
            let name_lower = def.name.to_lowercase();

            if id_lower == normalized || name_lower == normalized {
                return Ok(id);
            }
            if id_lower.contains(&normalized) || name_lower.contains(&normalized) {
                partial_matches.push(id);
            }
        }

        partial_matches.sort();
        partial_matches.dedup();
        match partial_matches.len() {
            1 => Ok(partial_matches[0].clone()),
            0 => Err(self
                .state
                .locale
                .format("Skill \"{0}\" not found.", &[(query).to_string()])),
            _ => Err(self.state.locale.format(
                "Multiple skills match \"{0}\": {1}",
                &[(query).to_string(), (partial_matches.join(", ")).clone()],
            )),
        }
    }

    fn find_loaded_skill(&self, id: &str) -> Option<&LoadedSkill> {
        self.loaded_skills
            .iter()
            .find(|skill| skill.definition.id == id || skill.definition.name == id)
    }

    fn update_agent_system_prompt(&mut self) {
        let prompt = self.build_system_prompt();
        if let Some(agent) = &self.native_agent {
            if let Err(e) = agent.set_system_prompt(prompt) {
                self.state.error = Some(format!("Failed to update system prompt: {e}"));
            }
        }
    }

    /// Push `state.approval_mode` to the native agent.
    ///
    /// The agent is the sole owner of the approve/auto-execute decision (see
    /// the `FromAgent::ToolCall` handler in `poll_agent`); call this
    /// wherever `state.approval_mode` changes so the agent's inline gate
    /// never runs a stale mode against the one shown in the UI.
    pub(super) fn sync_agent_approval_mode(&mut self) {
        if let Some(agent) = &self.native_agent {
            let _ = agent.set_approval_mode(self.state.approval_mode);
        }
    }

    /// Point the subagent scope at `session_id`'s conversation.
    ///
    /// Call this wherever the active conversation changes. The tool executor
    /// lives behind an `Arc` for the whole process, so without a rotation a
    /// child started by an earlier conversation reported its completion into
    /// whichever conversation happened to be active when it finished, and that
    /// summary went on to the next model request.
    ///
    /// The scope is derived from the session id rather than random, so resuming
    /// a session re-adopts the scope its own children were stamped with and
    /// their parked completions surface then. `None` means no session id exists
    /// yet; the placeholder is unique so it can never collide with another
    /// conversation, and it is replaced when the session file is created.
    /// Point both the subagent scope and the hook system at `session_id`.
    ///
    /// `reason` is published to `SessionStart` / `SessionEnd` hooks as their
    /// `source` / `reason` field. The hook system lives in the runner, so the
    /// session id and the lifecycle dispatch both travel as a command; the
    /// runner compares against the session it holds and fires the transition.
    pub(super) fn adopt_session_context(&mut self, session_id: Option<&str>, reason: &str) {
        self.adopt_session_context_inner(session_id, reason, false);
    }

    pub(super) fn adopt_compacted_session_context(&mut self, session_id: &str) {
        self.adopt_session_context_inner(Some(session_id), "summarize", true);
    }

    fn adopt_session_context_inner(
        &mut self,
        session_id: Option<&str>,
        reason: &str,
        preserve_compacted_checkpoint: bool,
    ) {
        let scope = subagent_scope_for_session(session_id);
        self.tool_executor.set_subagent_parent_scope(scope.clone());
        let Some(agent) = &self.native_agent else {
            return;
        };
        if let Err(e) = agent.set_subagent_parent_scope(scope) {
            self.state.error = Some(
                self.state
                    .locale
                    .format("Failed to rotate subagent scope: {0}", &[(e).to_string()]),
            );
        }
        let owns_persistent_tool_spills =
            session_id.is_some() && self.session_manager.writer().is_some();
        let transcript_path = session_id.and_then(|_| {
            self.session_manager
                .current_session_path()
                .map(|path| path.to_string_lossy().into_owned())
        });
        let transition =
            if let Some(session_id) = session_id.filter(|_| preserve_compacted_checkpoint) {
                agent.set_compacted_session_context_with_transcript(
                    session_id.to_owned(),
                    transcript_path,
                    owns_persistent_tool_spills,
                )
            } else {
                agent.set_session_context_with_transcript(
                    session_id.map(str::to_owned),
                    transcript_path,
                    reason,
                    owns_persistent_tool_spills,
                )
            };
        if let Err(e) = transition {
            self.state.error = Some(
                self.state
                    .locale
                    .format("Failed to update session context: {0}", &[e.to_string()]),
            );
        }
    }

    fn clear_active_skills(&mut self) {
        let active_ids: Vec<String> = self
            .skill_registry
            .active_skills()
            .iter()
            .map(|skill| skill.definition.id.clone())
            .collect();
        if active_ids.is_empty() {
            return;
        }
        for id in active_ids {
            let _ = self.skill_registry.deactivate(&id);
        }
        self.update_agent_system_prompt();
    }

    /// Poll for messages from the agent.
    /// Returns true if any messages were processed (UI should redraw).
    async fn poll_agent(&mut self) -> Result<bool> {
        // Collect messages first to avoid borrow issues
        let mut messages = Vec::new();
        if let Some(rx) = &mut self.native_event_rx {
            while let Ok(msg) = rx.try_recv() {
                messages.push(msg);
            }
        }
        let had_messages = !messages.is_empty();
        // Process messages
        for msg in messages {
            self.handle_agent_message(msg).await?;
        }
        Ok(had_messages)
    }

    fn maybe_open_first_run_setup(&mut self) -> bool {
        let credentials_missing = should_open_first_run_setup(
            self.native_agent.is_some(),
            default_model_credentials_ready(),
        );
        let introductory = !self.ui_prefs.onboarding_seen && self.initial_prompt.is_none();
        if !credentials_missing && !introductory {
            return false;
        }
        if self.active_modal != ActiveModal::None {
            return false;
        }
        self.open_onboarding();
        true
    }

    fn poll_setup_login(&mut self) -> bool {
        let Some(rx) = self.setup_login_rx.as_mut() else {
            return false;
        };
        match rx.try_recv() {
            Ok(Ok(())) => {
                self.setup_login_rx = None;
                if let Err(error) = self.refresh_managed_setup_after_identity_login() {
                    self.setup_modal.set_status(error.clone());
                    self.state.add_system_message(error);
                    return true;
                }
                if self.setup_modal.continue_to_byok_after_identity() {
                    self.state.add_system_message(
                        self.state.locale.translate("EvalOps Identity saved. Choose a provider key to finish BYOK setup.")
                            .to_string(),
                    );
                } else {
                    self.setup_modal.set_connection_ready();
                    self.record_onboarding(crate::telemetry::OnboardingStage::ConnectionSaved);
                    if self.native_agent.is_none() {
                        self.pending_agent_spawn = true;
                    }
                }
                true
            }
            Ok(Err(error)) => {
                self.setup_login_rx = None;
                self.setup_modal.set_status(error);
                true
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => false,
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                self.setup_login_rx = None;
                self.setup_modal.set_status(
                    self.state
                        .locale
                        .translate("EvalOps login ended without a result."),
                );
                true
            }
        }
    }

    fn refresh_managed_setup_after_identity_login(&mut self) -> Result<(), String> {
        // Login may have changed accounts. Never keep the previous account's
        // receipt origin if verification or policy refresh fails.
        self.managed_setup_identity_scope = None;
        let platform_session = crate::credential_mode::current_verified_identity_session()
            .map_err(|error| {
                self.state.locale.format(
                    "EvalOps Identity could not be verified after login: {0}",
                    &[(error).to_string()],
                )
            })?;
        let managed_setup = resolve_session_managed_setup(&platform_session);
        for notice in managed_setup.notices() {
            self.state.add_system_message(notice.clone());
        }

        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let sandbox_policy = match managed_setup
            .native_sandbox_policy(&cwd, self.sandbox_policy.clone())
        {
            Ok(policy) => policy,
            Err(error) => {
                self.state.add_system_message(self.state.locale.format("Managed sandbox policy could not be loaded after login ({0}); command execution is read-only.", &[(error).to_string()]));
                Some(crate::sandbox::SandboxPolicy::ReadOnly)
            }
        };
        let managed_mcp_policy = Some(crate::mcp::ManagedMcpPolicy {
            version: managed_setup.version(),
            policy: managed_setup.mcp_policy().clone(),
        });
        let cwd_text = cwd.to_string_lossy();
        let executor =
            ToolExecutor::with_credential_vault(cwd_text.as_ref(), self.credential_vault.clone())
                .with_managed_mcp_policy(managed_mcp_policy)
                .with_code_authority();
        self.tool_executor = Arc::new(match sandbox_policy.clone() {
            Some(policy) => executor.with_sandbox_policy(policy),
            None => executor,
        });
        self.sandbox_policy = sandbox_policy;
        self.managed_setup = managed_setup;
        self.managed_setup_identity_scope = crate::telemetry::TelemetryIdentityScope::new(
            &platform_session.organization_id,
            platform_session.workspace_id.as_deref(),
        );
        self.record_configuration_visibility();
        Ok(())
    }

    fn poll_model_verification(&mut self) -> bool {
        let mut changed = false;
        while let Ok(event) = self.model_verification_rx.try_recv() {
            if !event.is_for_model(&self.current_model) {
                continue;
            }
            changed |= self
                .model_selector
                .set_verification(&event.model, event.verification.clone());
            if event.verification.state == crate::model_catalog::VerificationState::Unavailable {
                self.state.status = event.verification.detail;
                changed = true;
            }
        }
        changed
    }

    fn poll_local_model_discovery(&mut self) -> bool {
        let mut changed = false;
        while let Ok(batch) = self.local_model_discovery_rx.try_recv() {
            let accepted = self
                .model_selector
                .replace_discovered_models(batch.generation, batch.models.clone());
            if accepted {
                crate::local_models::replace_discovered_models(
                    batch.generation,
                    &batch.models,
                    Some(&self.current_model),
                );
                if crate::local_models::find_discovered_model(&self.current_model).is_some() {
                    if let Some(agent) = &self.native_agent {
                        if let Err(error) = agent.refresh_model_budgets() {
                            self.state.status = Some(error.to_string());
                        }
                    }
                }
                changed = true;
            }
        }
        changed
    }

    async fn take_initial_prompt_after_discovery(&mut self) -> Option<String> {
        let has_initial_prompt = self.initial_prompt.is_some();
        if has_initial_prompt {
            self.apply_startup_local_model_discovery().await;
        }
        self.initial_prompt.take()
    }

    async fn apply_startup_local_model_discovery(&mut self) {
        if self.poll_local_model_discovery()
            || !crate::local_models::is_local_model_route(&self.current_model)
            || crate::local_models::find_discovered_model(&self.current_model).is_some()
        {
            return;
        }

        // Discovery probes all local runtimes concurrently with a 750ms HTTP
        // timeout. Give that first batch a small scheduling margin so its
        // RefreshModelBudgets command is queued before the startup prompt.
        let deadline = Instant::now() + Duration::from_millis(800);
        while Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if self.poll_local_model_discovery() {
                break;
            }
        }
    }

    /// Poll the background `/rubber-duck` review channel and post the finished
    /// review (or failure) into the chat as a system message.
    fn poll_rubber_duck(&mut self) -> bool {
        let mut events = Vec::new();
        let mut disconnected = false;
        if let Some(rx) = &self.rubber_duck_rx {
            loop {
                match rx.try_recv() {
                    Ok(event) => events.push(event),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        if events.is_empty() && !disconnected {
            return false;
        }
        self.rubber_duck_rx = None;
        self.rubber_duck_running = false;
        self.state.status = None;
        if events.is_empty() {
            // Sender dropped without a result: the review task died early.
            self.state.add_system_message(
                self.state.locale.translate("Rubber duck review failed: the review task exited without reporting a result.")
                    .to_string(),
            );
            return true;
        }
        for event in events {
            match event {
                crate::rubber_duck::RubberDuckEvent::Completed { model, review } => {
                    self.state.add_system_message(self.state.locale.format(
                        "## Rubber duck review (model: {0})\n\n{1}",
                        &[(model).clone(), (review).clone()],
                    ));
                }
                crate::rubber_duck::RubberDuckEvent::Failed { model, message } => {
                    self.state.add_system_message(self.state.locale.format(
                        "Rubber duck review failed (model: {0}): {1}",
                        &[(model).clone(), (message).clone()],
                    ));
                }
            }
        }
        true
    }

    /// After a worker turn: reload goal from disk (agent may have called
    /// `update_goal`) and arm auto-continue only if still active.
    fn sync_goal_store_after_turn(&mut self) {
        let prev_id = self.goal_store.current.as_ref().map(|g| g.id.clone());
        let prev_status = self.goal_store.current.as_ref().map(|g| g.status);
        let prev_tools_visible = self.goal_store.tools_visible();
        self.goal_store.reload_from_disk();
        if prev_tools_visible != self.goal_store.tools_visible() {
            self.sync_agent_goal_tools_visibility();
        }
        let cur = self.goal_store.current.as_ref();
        match cur {
            Some(g) if g.status == crate::goal::GoalStatus::Active && g.auto_continue => {
                self.goal_auto_continue_armed = true;
            }
            Some(g) if matches!(g.status, crate::goal::GoalStatus::Complete) => {
                self.goal_auto_continue_armed = false;
                if prev_status != Some(crate::goal::GoalStatus::Complete) {
                    self.state.add_system_message(self.state.locale.format(
                        "Goal {0} marked **complete** (via `update_goal`). Auto-continue stopped.",
                        std::slice::from_ref(&(g.id)),
                    ));
                    self.state.status.replace(
                        self.state
                            .locale
                            .format("Goal {0} complete", std::slice::from_ref(&g.id)),
                    );
                }
            }
            Some(g) if matches!(g.status, crate::goal::GoalStatus::Blocked) => {
                self.goal_auto_continue_armed = false;
                if prev_status != Some(crate::goal::GoalStatus::Blocked) {
                    let reason = g
                        .block_reason
                        .as_deref()
                        .unwrap_or(self.state.locale.translate("blocked"));
                    self.state.add_system_message(self.state.locale.format(
                        "Goal {0} marked **blocked** (via `update_goal`): {1}",
                        &[(g.id).clone(), (reason).to_string()],
                    ));
                    self.state.status.replace(
                        self.state
                            .locale
                            .format("Goal {0} blocked", std::slice::from_ref(&g.id)),
                    );
                }
            }
            None => {
                self.goal_auto_continue_armed = false;
                // Older builds cleared `current` on complete; keep the message
                // for that disk shape. Current `complete()` keeps status complete.
                if prev_id.is_some()
                    && matches!(
                        prev_status,
                        Some(
                            crate::goal::GoalStatus::Active
                                | crate::goal::GoalStatus::Paused
                                | crate::goal::GoalStatus::Blocked
                        )
                    )
                {
                    self.state.add_system_message(self.state.locale.format("Goal {0} finished (`update_goal` complete or `/goal complete`). Auto-continue stopped.", &[prev_id.as_deref().unwrap_or("?").to_string()]));
                    self.state
                        .status
                        .replace(self.state.locale.translate("Goal complete").into());
                }
            }
            _ => {
                self.goal_auto_continue_armed = false;
            }
        }
    }

    fn update_mcp_badge_counts(&mut self, servers: &[crate::tools::McpServerStatus]) {
        let connected = servers.iter().filter(|server| server.connected).count();
        let tool_count: usize = servers.iter().map(|server| server.tools.len()).sum();
        let failed = servers
            .iter()
            .filter(|server| !server.connected && server.error.is_some())
            .count();
        self.state.mcp_connected = connected;
        self.state.mcp_tool_count = tool_count;
        self.state.mcp_failed = failed;
    }

    /// Returns true if badge counts or status text changed.
    async fn refresh_mcp_badges(&mut self) -> bool {
        self.refresh_mcp_badges_with_force(false).await
    }

    /// Returns true if badge counts or status text changed.
    async fn refresh_mcp_badges_with_force(&mut self, force: bool) -> bool {
        let now = Instant::now();
        if !force
            && self
                .last_mcp_status_refresh
                .is_some_and(|last| now.duration_since(last) < Duration::from_secs(5))
        {
            return false;
        }
        self.last_mcp_status_refresh = Some(now);
        if self.mcp_status_refresh_in_flight {
            return false;
        }
        self.mcp_status_refresh_in_flight = true;
        let executor = Arc::clone(&self.tool_executor);
        let tx = self.mcp_status_tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(executor.mcp_status().await);
        });
        false
    }

    fn poll_mcp_status_refresh(&mut self) -> bool {
        let mut dirty = false;
        while let Ok(result) = self.mcp_status_rx.try_recv() {
            self.mcp_status_refresh_in_flight = false;
            let servers = match result {
                Ok(servers) => servers,
                Err(error) => {
                    self.state.status = Some(self.state.locale.format(
                        "MCP status refresh failed: {0}",
                        std::slice::from_ref(&(error)),
                    ));
                    dirty = true;
                    continue;
                }
            };
            let mut status_message = None;
            let current_statuses = snapshot_mcp_server_statuses(&servers);
            for server in &servers {
                if let Some(message) = crate::localization::with_locale(self.state.locale, || {
                    format_mcp_server_transition_status(
                        self.last_mcp_server_statuses.get(&server.name),
                        Some(server),
                    )
                }) {
                    status_message = Some(message);
                }
            }
            let mut removed_servers = self
                .last_mcp_server_statuses
                .keys()
                .filter(|name| !current_statuses.contains_key(*name))
                .cloned()
                .collect::<Vec<_>>();
            removed_servers.sort();
            for name in removed_servers {
                if let Some(message) = crate::localization::with_locale(self.state.locale, || {
                    format_mcp_server_transition_status(
                        self.last_mcp_server_statuses.get(&name),
                        None,
                    )
                }) {
                    status_message = Some(message);
                }
            }
            let previous = (
                self.state.mcp_connected,
                self.state.mcp_tool_count,
                self.state.mcp_failed,
            );
            self.update_mcp_badge_counts(&servers);
            self.mcp_manager.set_statuses(servers.clone());
            self.last_mcp_server_statuses = current_statuses;
            dirty |= previous
                != (
                    self.state.mcp_connected,
                    self.state.mcp_tool_count,
                    self.state.mcp_failed,
                );
            if let Some(message) = status_message {
                self.state.status = Some(message);
                dirty = true;
            }
        }
        dirty
    }

    fn poll_mcp_config(&mut self) -> bool {
        let mut dirty = false;
        while let Ok(result) = self.mcp_config_rx.try_recv() {
            self.mcp_config_in_flight = false;
            match result {
                Ok(message) => self.state.add_system_message(message),
                Err(error) => self.state.add_system_message(self.state.locale.format(
                    "MCP configuration failed: {0}",
                    std::slice::from_ref(&(error)),
                )),
            }
            self.last_mcp_status_refresh = None;
            dirty = true;
        }
        dirty
    }

    /// Returns true if MCP events updated UI state.
    async fn poll_mcp_updates(&mut self) -> bool {
        let mut dirty = false;
        match self.tool_executor.poll_mcp_updates().await {
            Ok(events) => {
                if events.iter().any(McpRuntimeEvent::affects_badges) {
                    dirty |= self.refresh_mcp_badges_with_force(true).await;
                }

                if let Some(status) = events
                    .iter()
                    .rev()
                    .find_map(format_mcp_runtime_event_status)
                {
                    self.state.status = Some(status);
                    dirty = true;
                }
            }
            Err(err) => {
                self.state.status = Some(
                    self.state
                        .locale
                        .format("MCP update error: {0}", std::slice::from_ref(&(err))),
                );
                dirty = true;
            }
        }
        dirty
    }

    /// Returns true if any config events were applied.
    async fn poll_config_watcher(&mut self) -> bool {
        let mut dirty = false;
        while let Some(event) = self.config_watcher.poll() {
            self.handle_config_event(event).await;
            dirty = true;
        }
        dirty
    }

    async fn handle_config_event(&mut self, event: ConfigEvent) {
        match event {
            ConfigEvent::Changed(path)
            | ConfigEvent::Created(path)
            | ConfigEvent::Deleted(path)
                if is_mcp_config_path(&path) =>
            {
                self.refresh_mcp_badges_with_force(true).await;
            }
            ConfigEvent::Changed(path)
            | ConfigEvent::Created(path)
            | ConfigEvent::Deleted(path)
                if is_keybindings_config_path(&path) =>
            {
                self.reload_keybindings_from_config();
            }
            ConfigEvent::Error(message) => {
                self.state.status = Some(self.state.locale.format(
                    "Config watcher error: {0}",
                    std::slice::from_ref(&(message)),
                ));
            }
            _ => {}
        }
    }

    fn apply_runtime_keybindings(&mut self, keybindings: crate::keybindings::RustTuiKeybindings) {
        let labels = keybindings.labels();
        self.state.queued_follow_up_edit_binding_label = labels.edit_last_queued_follow_up.clone();
        self.shortcuts_help.set_binding_labels(labels);
        self.cycle_model_binding = keybindings.cycle_model;
        self.cycle_model_backward_binding = keybindings.cycle_model_backward;
        self.command_palette_binding = keybindings.command_palette;
        self.file_search_binding = keybindings.file_search;
        self.toggle_tool_outputs_binding = keybindings.toggle_tool_outputs;
        self.queued_follow_up_edit_binding = keybindings.edit_last_queued_follow_up;
    }

    fn reload_keybindings_from_config(&mut self) {
        let terminal_info = crate::terminal_info::TerminalInfo::get();
        let keybindings =
            load_rust_tui_keybindings(&terminal_info.name, std::env::var_os("TMUX").is_some());
        self.apply_runtime_keybindings(keybindings);

        let next_issue_summary = summarize_keybindings_config_issues();
        if let Some(summary) = &next_issue_summary {
            if self.last_keybinding_issue_summary.as_ref() != Some(summary) {
                self.state.status = Some(summary.clone());
            }
        } else if self.last_keybinding_issue_summary.is_some() {
            self.state.status = Some(
                self.state
                    .locale
                    .translate("Keyboard shortcuts config reloaded cleanly.")
                    .to_string(),
            );
        }
        self.last_keybinding_issue_summary = next_issue_summary;
    }

    /// Cancel the current native-agent operation and wait until its request,
    /// approval, and foreground-tool cleanup are all quiescent. The
    /// repeat-signal monitor remains the escape hatch if cleanup wedges.
    pub(crate) async fn stop_agent_for_resume(&mut self) {
        if let Some(agent) = self.native_agent.take() {
            agent.shutdown().await;
        }
        self.drain_agent_events_after_shutdown().await;
    }

    pub(crate) async fn signal_shutdown_teardown(&mut self) -> (Vec<String>, bool) {
        self.stop_agent_for_resume().await;
        let disable_theme_reporting = self.prepare_terminal_restore();
        (
            self.terminal_session_ended_sequences(),
            disable_theme_reporting,
        )
    }

    /// Apply every event the agent published before its shutdown barrier
    /// completed. The runner owns the last event sender, so after
    /// `NativeAgent::shutdown` returns this channel is closed and `try_recv`
    /// drains a stable, finite suffix.
    ///
    /// Post-interrupt queue submission and immediate terminal notifications
    /// are intentionally disabled: signal teardown must persist terminal
    /// events without starting another request or blocking the async
    /// repeat-signal monitor on terminal I/O. The caller writes the final
    /// terminal-session sequences from its blocking cleanup.
    async fn drain_agent_events_after_shutdown(&mut self) {
        let mut messages = Vec::new();
        if let Some(rx) = &mut self.native_event_rx {
            while let Ok(message) = rx.try_recv() {
                messages.push(message);
            }
        }
        for message in messages {
            if let Err(error) = self
                .handle_agent_message_with_options(message, false, false)
                .await
            {
                self.state.error = Some(self.state.locale.format(
                    "Failed to finalize an agent event during shutdown: {0}",
                    &[(error).to_string()],
                ));
            }
        }
    }

    /// Stop protocol-aware reads before terminal restoration and report
    /// whether mode 2031 must be disabled by the caller's blocking cleanup.
    ///
    /// Signal cleanup invokes this after cancelling the run future; app
    /// construction cleanup also invokes it on an app completed after the
    /// signal won. Keeping this synchronous lets both paths drop the tty
    /// reader before any restore while moving terminal writes off the async
    /// worker.
    pub(crate) fn prepare_terminal_restore(&mut self) -> bool {
        let had_terminal_events = self.terminal_events.take().is_some();
        terminal_reporting_shutdown_needed(had_terminal_events, self.state.theme_follower.is_some())
    }

    fn terminal_session_ended_sequences(&mut self) -> Vec<String> {
        if !self.terminal_session_started {
            return Vec::new();
        }
        self.terminal_session_started = false;
        self.terminal_notifier.session_ended()
    }

    /// Write terminal-notification escape sequences (OSC progress/title) to
    /// the terminal device. No-ops when the terminal is not initialized.
    pub(crate) fn write_terminal_sequences(seqs: &[String]) {
        if !seqs.is_empty() {
            let _ = terminal::write_raw(&seqs.concat());
        }
    }

    /// Emit the turn-start terminal state: indeterminate tab progress and the
    /// working title.
    fn notify_terminal_turn_started(&mut self) {
        let seqs = self.terminal_notifier.turn_started();
        Self::write_terminal_sequences(&seqs);
    }

    /// Emit the turn-end terminal state: clear tab progress, restore the idle
    /// title, and send the desktop notification unless the focus gate
    /// suppresses it (terminal focused).
    fn notify_terminal_turn_finished(&mut self) {
        let seqs = self.terminal_notifier.turn_finished();
        Self::write_terminal_sequences(&seqs);
        if self.terminal_notifier.should_send_desktop_notification() {
            crate::notifications::notify_turn_complete();
        }
    }

    fn set_codex_status_if_useful(&mut self, status: String) {
        if self
            .state
            .status
            .as_deref()
            .is_some_and(is_active_codex_turn_status)
        {
            return;
        }
        self.state.status = Some(status);
    }

    fn finish_active_turn_summary(&mut self) {
        let Some(mut summary) = self.active_turn_summary.take() else {
            self.active_turn_start_message_index = None;
            self.active_turn_started_at = None;
            return;
        };

        let start = self
            .active_turn_start_message_index
            .take()
            .unwrap_or(self.state.messages.len());
        let mut tool_calls = 0usize;
        let mut successful_tools = 0usize;
        let mut failed_tools = 0usize;
        for message in self.state.messages.iter().skip(start) {
            for call in &message.tool_calls {
                tool_calls += 1;
                match call.status {
                    crate::state::ToolCallStatus::Completed => successful_tools += 1,
                    crate::state::ToolCallStatus::Failed
                    | crate::state::ToolCallStatus::Cancelled
                    | crate::state::ToolCallStatus::Blocked => failed_tools += 1,
                    crate::state::ToolCallStatus::Pending
                    | crate::state::ToolCallStatus::Running => {}
                }
            }
        }
        summary.set_tool_counts(tool_calls, successful_tools, failed_tools);
        if let Some(started_at) = self.active_turn_started_at.take() {
            summary.set_duration(started_at.elapsed());
        }

        // A queued prompt means the runner is about to start another turn. Do
        // not overwrite its queue status with a recap that would be painted
        // only during the handoff gap; summaries belong to an actually idle UI.
        if !self.state.busy && self.queued_prompts.is_empty() {
            self.state.status = Some(summary.status_line());
        }
    }

    fn abandon_active_turn_summary(&mut self) {
        self.active_turn_summary = None;
        self.active_turn_start_message_index = None;
        self.active_turn_started_at = None;
    }

    async fn handle_agent_message(&mut self, msg: FromAgent) -> Result<()> {
        let result = self
            .handle_agent_message_with_options(msg, true, true)
            .await;
        self.sync_dex_attention();
        result
    }

    async fn handle_agent_message_with_options(
        &mut self,
        msg: FromAgent,
        allow_post_interrupt_queue: bool,
        allow_terminal_notifications: bool,
    ) -> Result<()> {
        if allow_terminal_notifications {
            if let FromAgent::Error {
                fatal,
                terminal,
                retryable,
                ..
            } = &msg
            {
                if (*fatal || *terminal) && !retryable {
                    self.suggest_bug_report();
                }
            }
        }
        if let FromAgent::LocalAssistantContent {
            response_id,
            content,
        } = &msg
        {
            self.pending_assistant_content = Some((response_id.clone(), content.clone()));
        }
        self.record_agent_session_events(&msg);
        let response_end_info = match &msg {
            FromAgent::ResponseEnd { response_id, usage } => {
                self.persist_request_cache();
                Some((response_id.clone(), usage.clone()))
            }
            _ => None,
        };
        let native_tool_completion = match &msg {
            FromAgent::ToolEnd {
                call_id,
                success,
                result,
                receipt,
            } => self.tool_history.get(call_id).map(|execution| {
                (
                    call_id.clone(),
                    execution.tool_name.clone(),
                    *success,
                    result.clone(),
                    receipt.clone(),
                )
            }),
            _ => None,
        };
        let mut needs_post_interrupt_queue = false;
        use crate::components::dex_companion::DexCompanionState;
        let previous_dex_terminal = self.dex_terminal;
        if matches!(
            &msg,
            FromAgent::Ready { .. }
                | FromAgent::ResponseStart { .. }
                | FromAgent::SideQuestionStart {
                    standalone: true,
                    ..
                }
        ) {
            self.dex_begin_turn();
        }
        match &msg {
            FromAgent::Ready { .. } | FromAgent::ResponseStart { .. } => self.dex_terminal = None,
            FromAgent::SideQuestionStart {
                standalone: true, ..
            } => self.dex_terminal = None,
            FromAgent::SideQuestionEnd {
                standalone: true,
                error,
                ..
            } => {
                self.dex_terminal = Some(if error.is_some() {
                    DexCompanionState::Failed
                } else {
                    DexCompanionState::Finished
                });
            }
            FromAgent::TurnCompleted { .. } => {
                self.dex_terminal = Some(DexCompanionState::Finished);
            }
            FromAgent::TurnInterrupted { .. } => self.dex_terminal = Some(DexCompanionState::Ready),
            FromAgent::Error {
                fatal, terminal, ..
            } if *fatal || *terminal => {
                self.dex_terminal = Some(DexCompanionState::Failed);
            }
            FromAgent::ProviderError { .. } => self.dex_terminal = Some(DexCompanionState::Failed),
            _ => {}
        }

        if self.dex_terminal != previous_dex_terminal {
            self.dex_pose_started = Instant::now();
        }
        if matches!(msg, FromAgent::ResponseStart { .. }) {
            let was_busy = self.state.busy;
            self.state.busy = true;
            if !was_busy {
                self.active_turn_assistant_messages_persisted = true;
            }
            self.queued_prompt_inflight = None;
            if !was_busy && allow_terminal_notifications {
                self.notify_terminal_turn_started();
            }
            if !was_busy {
                let drain_count = match self.queued_prompts.front().map(|prompt| prompt.kind) {
                    Some(PromptKind::Steer)
                        if matches!(self.state.steering_mode, QueueMode::All) =>
                    {
                        self.queued_prompts
                            .iter()
                            .take_while(|prompt| prompt.kind == PromptKind::Steer)
                            .count()
                    }
                    Some(PromptKind::FollowUp)
                        if matches!(self.state.follow_up_mode, QueueMode::All) =>
                    {
                        self.queued_prompts
                            .iter()
                            .take_while(|prompt| prompt.kind == PromptKind::FollowUp)
                            .count()
                    }
                    Some(_) => 1,
                    None => 0,
                };

                let mut drained = Vec::new();
                for _ in 0..drain_count {
                    if let Some(pending) = self.queued_prompts.pop_front() {
                        drained.push(pending);
                    }
                }

                if let Some(active) = drained.first().cloned() {
                    self.queued_prompt_active = Some(active);
                    for pending in drained {
                        self.state.add_user_message(pending.content);
                    }
                    self.sync_queue_prompt_count();
                } else {
                    self.queued_prompt_active = None;
                }
            }
            if self.active_turn_summary.is_none() {
                self.active_turn_summary = Some(crate::turn_summary::TurnSummary::default());
                self.active_turn_start_message_index = Some(self.state.messages.len());
                self.active_turn_started_at = Some(Instant::now());
            }
        }
        match &msg {
            FromAgent::Ready { model, provider } => {
                self.state.status = Some(self.state.locale.format(
                    "Connected: {0} via {1}",
                    &[(model).clone(), (provider).clone()],
                ));
                self.current_model = model.clone();
                self.usage_tracker.set_model(model.clone());
                self.model_monitor.verify(model.clone());
            }
            FromAgent::BoostChanged { status, thinking } => {
                if let Some(level) = thinking {
                    self.current_thinking_level = *level;
                    self.record_thinking_level_change(*level);
                }
                match status {
                    crate::model_dynamics::BoostStatus::Pending => {
                        self.state.status = Some(
                            self.state.locale.translate("Boost queued for the next model request; applies once to that task.")
                                .into(),
                        );
                    }
                    crate::model_dynamics::BoostStatus::Active => {
                        self.state.status = Some(
                            self.state.locale.translate("Boost active for this task; your setting returns when it ends.").into(),
                        );
                    }
                    _ => {}
                }
            }
            FromAgent::ModelChanged { model, provider } => {
                let pending_matches = self
                    .pending_model_change
                    .as_ref()
                    .map(|pending| pending.model == *model)
                    .unwrap_or(false);

                self.current_model = model.clone();
                self.state.model = Some(model.clone());
                self.state.provider = Some(provider.clone());
                self.usage_tracker.set_model(model.clone());
                self.model_monitor.verify(model.clone());
                self.state.status = Some(
                    self.state
                        .locale
                        .format("Model: {0}", std::slice::from_ref(model)),
                );

                if pending_matches {
                    self.pending_model_change = None;
                    // A failed earlier request can arrive after a newer
                    // switch was queued. Only the matching success clears the
                    // shared error banner, so unrelated errors remain visible.
                    self.state.error = None;
                }
                self.record_model_change(model);
            }
            FromAgent::ModelChangeFailed { model, .. }
                if self
                    .pending_model_change
                    .as_ref()
                    .map(|pending| pending.model == *model)
                    .unwrap_or(false) =>
            {
                self.pending_model_change = None;
            }
            FromAgent::SessionInfo { cwd, .. } => {
                self.state.status = Some(
                    self.state
                        .locale
                        .format("Session in: {0}", std::slice::from_ref(cwd)),
                );
            }
            FromAgent::Compaction {
                summary,
                first_kept_entry_index,
                tokens_before,
                auto,
                custom_instructions,
                continuation,
                timestamp,
            } => {
                self.record_context_budget_snapshot(
                    maestro_context::ContextBudgetPhase::BeforeCompaction,
                    None,
                );
                self.state.apply_compaction(
                    summary.clone(),
                    *first_kept_entry_index,
                    parse_rfc3339_system_time(timestamp).unwrap_or_else(|_| SystemTime::now()),
                );
                self.record_compaction_entry(
                    summary.clone(),
                    *first_kept_entry_index,
                    *tokens_before,
                    *auto,
                    custom_instructions.clone(),
                    continuation.clone(),
                );
                return Ok(());
            }
            FromAgent::SideQuestionStart {
                side_id, question, ..
            } => {
                if let Some(index) = self.queued_prompts.iter().position(|prompt| {
                    prompt.kind == PromptKind::SideQuestion && prompt.content == *question
                }) {
                    self.queued_prompts.remove(index);
                    self.sync_queue_prompt_count();
                }
                self.state
                    .add_side_question(side_id.clone(), question.clone());
                self.state
                    .add_side_answer(format!("{side_id}-answer"), String::new(), true);
            }
            FromAgent::SideQuestionChunk { side_id, content } => {
                if let Some(answer) = self
                    .state
                    .messages
                    .iter_mut()
                    .find(|message| message.id == format!("{side_id}-answer"))
                {
                    answer.content.push_str(content);
                }
            }
            FromAgent::SideQuestionEnd {
                side_id,
                question,
                answer,
                error,
                standalone,
                ..
            } => {
                let display_answer = match error {
                    Some(error) if answer.is_empty() => self
                        .state
                        .locale
                        .format("Side question failed: {0}", std::slice::from_ref(error)),
                    Some(error) => self.state.locale.format(
                        "{0}\n\nSide question failed: {1}",
                        &[(answer).clone(), (error).clone()],
                    ),
                    None => answer.clone(),
                };
                if let Some(message) = self
                    .state
                    .messages
                    .iter_mut()
                    .find(|message| message.id == format!("{side_id}-answer"))
                {
                    message.streaming = false;
                    message.content = display_answer;
                }
                self.record_side_question(
                    side_id.clone(),
                    question.clone(),
                    answer.clone(),
                    error.clone(),
                );
                if *standalone {
                    self.state.busy = false;
                    self.queued_prompt_inflight = None;
                    self.queued_prompt_active = None;
                    self.sync_queue_prompt_count();
                    needs_post_interrupt_queue = true;
                }
            }
            FromAgent::CodexSessionState {
                state,
                thread_id,
                profile,
            } => {
                self.set_codex_status_if_useful(self.state.locale.format(
                    "Codex {0} {1} {2}",
                    &[
                        (state).clone(),
                        (profile).clone(),
                        (short_codex_status_id(thread_id)).clone(),
                    ],
                ));
            }
            FromAgent::CodexTurnState {
                state,
                thread_id,
                turn_id,
            } => {
                self.state.status = Some(match turn_id {
                    Some(turn_id) => self.state.locale.format(
                        "Codex {0} {1} {2}",
                        &[
                            (state).clone(),
                            (short_codex_status_id(thread_id)).clone(),
                            (short_codex_status_id(turn_id)).clone(),
                        ],
                    ),
                    None => self.state.locale.format(
                        "Codex {0} {1}",
                        &[(state).clone(), (short_codex_status_id(thread_id)).clone()],
                    ),
                });
            }
            FromAgent::CodexUsageState { source, .. } => {
                self.set_codex_status_if_useful(
                    self.state
                        .locale
                        .format("Codex usage {0}", std::slice::from_ref(source)),
                );
            }
            FromAgent::CodexCompatibility {
                protocol_version,
                resume,
                steering,
            } => {
                self.set_codex_status_if_useful(compact_codex_compatibility_status(
                    protocol_version,
                    *resume,
                    *steering,
                ));
            }
            FromAgent::TurnCompleted { .. } => {
                self.record_agent_tool_note_progress(self.active_turn_assistant_messages_persisted);
                self.state.busy = false;
                self.finish_active_turn_summary();
                if allow_terminal_notifications {
                    self.notify_terminal_turn_finished();
                }
                self.finalize_file_checkpoint();
                self.queued_prompt_active = None;
                self.queued_prompt_inflight = None;
                self.queued_prompt_inflight = self
                    .queued_prompts
                    .front()
                    .map(|prompt| QueuedPromptCursor { id: prompt.id });
                self.sync_queue_prompt_count();
                needs_post_interrupt_queue = true;
                // Codex-style: reload goal from disk (worker may have called
                // update_goal). Continue only if still active.
                self.sync_goal_store_after_turn();
                // Idle: flush host tool notes into agent history before any
                // auto-continue continuation so the model sees process exits.
                self.flush_pending_agent_tool_notes();
            }
            FromAgent::ResponseEnd { .. } => {}
            FromAgent::TurnInterrupted { .. } => {
                self.state.busy = false;
                self.abandon_active_turn_summary();
                if allow_terminal_notifications {
                    self.notify_terminal_turn_finished();
                }
                self.finalize_file_checkpoint();
                self.queued_prompt_inflight = None;
                self.queued_prompt_active = None;
                self.sync_queue_prompt_count();
                needs_post_interrupt_queue = true;
                self.flush_pending_agent_tool_notes();
            }
            FromAgent::Error {
                fatal, terminal, ..
            } if *fatal || *terminal => {
                // Clear busy state on error
                self.state.busy = false;
                self.abandon_active_turn_summary();
                if allow_terminal_notifications {
                    self.notify_terminal_turn_finished();
                }
                self.finalize_file_checkpoint();
                self.queued_prompt_inflight = None;
                self.queued_prompt_active = None;
                self.sync_queue_prompt_count();
                needs_post_interrupt_queue = true;
                self.flush_pending_agent_tool_notes();
                // Do not auto-continue after an error; user must re-arm via
                // /goal resume or a successful create.
            }
            FromAgent::Error { .. } => {}
            FromAgent::ProviderError { .. } => {
                self.state.busy = false;
                self.abandon_active_turn_summary();
                if allow_terminal_notifications {
                    self.notify_terminal_turn_finished();
                }
                self.finalize_file_checkpoint();
                self.queued_prompt_inflight = None;
                self.queued_prompt_active = None;
                self.sync_queue_prompt_count();
                needs_post_interrupt_queue = true;
                self.flush_pending_agent_tool_notes();
            }
            FromAgent::ToolCall {
                call_id,
                tool,
                args,
                requires_approval,
                approval_inline_env,
            } => {
                self.tool_executor
                    .set_subagent_parent_model(crate::model_dynamics::ModelChoice {
                        model: self.current_model.clone(),
                        thinking: self.current_thinking_level,
                    });
                self.tool_history.start_with_approval(
                    call_id.clone(),
                    tool.clone(),
                    args.clone(),
                    *requires_approval,
                );
                // Unknown tool name -> deny immediately
                if !self.tool_executor.has_tool(tool) && approval_inline_env.is_none() {
                    let note = self.state.locale.format("Skipped unknown tool '{0}' (not in registry); denied call. Retry with a supported tool (bash/read/write/glob/grep) and valid args.", std::slice::from_ref(tool));
                    self.state.add_system_message(note);
                    self.state.handle_agent_message(msg.clone());
                    self.state.fail_tool_call(
                        call_id,
                        self.state.locale.translate("Unknown tool (denied)"),
                    );
                    self.handle_tool_approval(call_id.clone(), tool.clone(), args.clone(), false)
                        .await?;
                    return Ok(());
                }

                // Validate required fields per tool schema
                let missing = self.tool_executor.missing_required(tool, args);
                if !missing.is_empty() {
                    let note = self.state.locale.format("Skipped tool '{0}' due to missing fields: {1}. Add the required fields and retry.", &[(tool).clone(), (missing.join(", ")).clone()]);
                    self.state.add_system_message(note);
                    self.state.handle_agent_message(msg.clone());
                    self.state.fail_tool_call(
                        call_id,
                        self.state
                            .locale
                            .translate("Missing required tool args (denied)"),
                    );
                    self.handle_tool_approval(call_id.clone(), tool.clone(), args.clone(), false)
                        .await?;
                    return Ok(());
                }

                let firewall_verdict = self.tool_executor.firewall_verdict(tool, args);
                if let FirewallVerdict::Block { reason } = &firewall_verdict {
                    let note = self.state.locale.format(
                        "Blocked tool '{0}' by action firewall: {1}",
                        &[(tool).clone(), (reason).clone()],
                    );
                    self.state.add_system_message(note);
                    self.state.handle_agent_message(msg.clone());
                    self.state.fail_tool_call(
                        call_id,
                        self.state.locale.translate("Blocked by action firewall"),
                    );
                    self.handle_tool_approval(call_id.clone(), tool.clone(), args.clone(), false)
                        .await?;
                    return Ok(());
                }

                // The native agent is the single owner of the approve/execute
                // decision: it already applied `self.state.approval_mode`
                // (kept in sync via `sync_agent_approval_mode`) to the same
                // Yolo/Safe/Selective gate before emitting this event, and if
                // `requires_approval` is false it has ALREADY executed the
                // tool inline and will report the result to the model itself
                // (see `NativeAgentRunner::run_loop` in `agent/native.rs`).
                //
                // Recomputing that decision here from `self.state` -- as this
                // used to do -- let the two sides disagree: in Safe mode the
                // agent (mode-unaware) would auto-execute while this recomputed
                // `true` and popped an approval modal for an already-finished
                // call, and Deny had nowhere to go (issue #3149). In Selective
                // mode both sides agreed `false` and both executed the tool,
                // running side effects (e.g. bash) twice (issue #3156).
                //
                // This mirrors the same rule the headless server already
                // enforces (see the module doc on `headless_server.rs`).
                if *requires_approval {
                    let mut request =
                        ApprovalRequest::new(call_id.clone(), tool.clone(), args.clone());
                    let firewall_reason = match &firewall_verdict {
                        FirewallVerdict::RequireApproval { reason } => Some(reason.clone()),
                        _ => None,
                    };
                    let bypass_requested = tool.eq_ignore_ascii_case("bash")
                        && args
                            .get("bypass_sandbox")
                            .and_then(serde_json::Value::as_bool)
                            == Some(true);
                    // Make the sandbox escape hatch a thing the user is
                    // explicitly asked about, not a generic bash approval
                    // that happens to look the same as any other — and never
                    // let a firewall reason hide it: approving this call also
                    // removes the native sandbox, which the modal must say
                    // even when the firewall already gave a reason.
                    let bypass_reason = bypass_requested.then(|| {
                        self.state.locale.translate("Agent is asking to run this command WITHOUT Maestro's native sandbox (a sandboxed attempt likely just failed).")
                            .to_string()
                    });
                    if let Some(reason) = combine_approval_reason(firewall_reason, bypass_reason) {
                        request = request.with_reason(reason);
                    }
                    // Inline tools (`.composer/tools.json`) resolve their entire
                    // shell command from the tool's own config, not from the
                    // call's JSON arguments. Without this, `display_command()`'s
                    // args-based fallback has nothing to show and the dialog
                    // renders as `tool_name: {}` -- approving a command the user
                    // never actually saw. Populate the real command (and where
                    // it came from) so the dialog can't hide it.
                    if let Some(inline_context) = approval_inline_env.as_ref() {
                        request = request.with_inline_tool_source(
                            inline_context.command.clone(),
                            &inline_context.source_path,
                            &inline_context.source_label,
                            Some(&inline_context.cwd),
                            &inline_context.environment,
                        );
                        request = request
                            .with_inline_shell(&inline_context.shell, &inline_context.shell_arg);
                    } else if self.tool_executor.get_inline_tool(tool).is_some() {
                        let note = self.state.locale.format("Skipped inline tool '{0}': approval environment snapshot was missing; retry to review the exact execution context.", std::slice::from_ref(tool));
                        self.state.add_system_message(note);
                        self.state.handle_agent_message(msg.clone());
                        self.state.fail_tool_call(
                            call_id,
                            self.state
                                .locale
                                .translate("Missing approval environment snapshot"),
                        );
                        self.handle_tool_approval(
                            call_id.clone(),
                            tool.clone(),
                            args.clone(),
                            false,
                        )
                        .await?;
                        return Ok(());
                    }

                    // Guardian mode: an independent LLM reviews the request first;
                    // allow executes silently, anything else falls back to the modal.
                    if !self.spawn_guardian_review(&request) {
                        // Queue approval
                        self.approval_controller.enqueue(request);
                        // Show approval modal
                        self.cancel_theme_preview();
                        self.active_modal = ActiveModal::Approval;
                    }
                } else {
                    // Yolo/auto mode intentionally skips soft firewall holds,
                    // but the user still needs to know what policy finding was
                    // bypassed. Keep this contextual note in the transcript so
                    // an auto-approved action is explainable after the tool
                    // output scrolls away; hard blocks and sandbox bypasses
                    // still take their existing denial/approval paths.
                    if matches!(
                        (&self.state.approval_mode, &firewall_verdict),
                        (
                            crate::state::ApprovalMode::Yolo,
                            FirewallVerdict::RequireApproval { .. }
                        )
                    ) {
                        let action = crate::tool_summary::summarize_tool_use(tool, args);
                        if let FirewallVerdict::RequireApproval { reason } = &firewall_verdict {
                            self.state.add_system_message(self.state.locale.format(
                                "Auto mode allowed {0}. Security finding: {1}",
                                &[(action).clone(), (reason).clone()],
                            ));
                        }
                    }
                    // Already auto-executed inline by the native agent above;
                    // do not execute it again. Just record the approval flag
                    // for the tool-history UI.
                    self.tool_history.record_approval(call_id, true);
                }
            }
            _ => {}
        }
        self.state.handle_agent_message(msg);

        if let Some((call_id, tool, success, result, receipt)) = native_tool_completion {
            self.record_native_tool_completion(&call_id, &tool, success, result, receipt);
        }

        if let Some((response_id, usage)) = response_end_info {
            // Sentinel / control ResponseEnds must not move the goal budget.
            let account_goal = response_id != "done" && response_id != "continue";
            if let Some(ref usage) = usage {
                let headless_usage = to_headless_usage(usage);
                if let Some(summary) = self.active_turn_summary.as_mut() {
                    summary.add_usage(&headless_usage);
                }
                // Goal budget tracks billable (non-cached) tokens only so
                // multi-step tool loops do not exhaust on repeated cache hits.
                let turn_tokens = crate::goal::GoalStore::billable_tokens(
                    headless_usage.input_tokens,
                    headless_usage.output_tokens,
                    headless_usage.cache_read_tokens,
                );
                if account_goal && self.goal_store.current.is_some() && turn_tokens > 0 {
                    match self.goal_store.account_tokens(turn_tokens) {
                        Ok(true) => {
                            self.goal_auto_continue_armed = false;
                            self.state.add_system_message(self.state.locale.format("Goal token budget exhausted after this turn (+{0} billable tokens). Auto-continue stopped.", &[(turn_tokens).to_string()]));
                        }
                        Ok(false) => {}
                        Err(e) => {
                            self.state
                                .error
                                .replace(format!("Failed to account goal tokens: {e}"));
                        }
                    }
                }
                let alerts = self.usage_tracker.add_turn(&headless_usage);
                for alert in alerts {
                    self.state.add_system_message(alert);
                }
            }
            if account_goal {
                let persisted = self.record_assistant_message(&response_id, usage);
                self.active_turn_assistant_messages_persisted &= persisted;
            }
        }

        if needs_post_interrupt_queue && allow_post_interrupt_queue {
            let _ = self.maybe_handle_post_interrupt_queue().await?;
        }
        Ok(())
    }

    /// Complete a tool that the native agent executed.
    fn record_native_tool_completion(
        &mut self,
        call_id: &str,
        tool: &str,
        success: bool,
        result: Option<ToolResult>,
        receipt: Option<crate::agent::ExecutionReceipt>,
    ) {
        let output = self
            .state
            .messages
            .iter()
            .rev()
            .flat_map(|message| message.tool_calls.iter())
            .find(|call| call.call_id == call_id)
            .map_or_else(String::new, |call| call.output.clone());
        let result = result.unwrap_or_else(|| {
            let error = (!success).then(|| {
                if output.is_empty() {
                    self.state
                        .locale
                        .translate("Tool execution failed")
                        .to_string()
                } else {
                    output.clone()
                }
            });
            ToolResult {
                success,
                output,
                error,
                details: None,
            }
        });
        let execution = receipt.map(|receipt| {
            let mut execution =
                ToolExecution::from_legacy(call_id, tool, receipt.source, result.clone());
            execution.receipt = receipt;
            execution
        });

        self.record_tool_result(call_id, tool, &result, execution.as_ref());
        self.persist_attachment_extract(call_id, tool, &result);
        self.accept_feedback_tool(tool, &result);
    }

    /// Spawn a guardian review for a pending approval (guardian mode only).
    ///
    /// Returns false when the guardian is disabled, so the caller falls back
    /// to the human modal unchanged. When true, the review runs on a spawned
    /// task (like tool execution) and is applied by `poll_guardian_verdicts`,
    /// so the event loop never blocks on the review call.
    fn spawn_guardian_review(&mut self, request: &ApprovalRequest) -> bool {
        use crate::safety::guardian::{
            GuardianContext, TranscriptItem, bounded_transcript, guardian_may_auto_approve,
            summarize_args,
        };

        let Some(guardian) = self.guardian.clone() else {
            return false;
        };
        // Hard ceiling, enforced here rather than trusted to the review
        // model's own judgment: mutating/destructive/privacy-sensitive tools
        // (write, edit, background_tasks, gh_pr/issue/repo, screenshot, ...)
        // and any sandbox-bypass request always go to the human modal. See
        // `guardian_may_auto_approve`'s doc for why an allowlist here, not a
        // denylist.
        if !guardian_may_auto_approve(&request.tool, &request.args) {
            return false;
        }
        let transcript_items: Vec<TranscriptItem> = self
            .state
            .messages
            .iter()
            .filter(|message| message.kind == MessageKind::Regular)
            .map(|message| match message.role {
                MessageRole::User => TranscriptItem::User(message.content.clone()),
                MessageRole::Assistant => TranscriptItem::Assistant(message.content.clone()),
            })
            .collect();
        let context = GuardianContext {
            tool: request.tool.clone(),
            args_summary: summarize_args(&request.args),
            firewall_reason: request.reason.clone(),
            transcript: bounded_transcript(&transcript_items),
        };
        let guardian_tx = self.guardian_tx.clone();
        let request = request.clone();
        self.pending_guardian_reviews
            .insert(request.call_id.clone());
        tokio::spawn(async move {
            let verdict = guardian.evaluate(context).await;
            let _ = guardian_tx.send((request, verdict));
        });
        true
    }

    /// Apply finished guardian reviews (non-blocking). Auto-allow relays the
    /// approval to the native agent, which owns execution and runs the call
    /// in model order (see `handle_tool_approval`); a deny verdict or any
    /// failure fails closed to the human approval modal.
    /// Returns true when any review was applied.
    async fn poll_guardian_verdicts(&mut self) -> Result<bool> {
        use crate::safety::guardian::GuardianDecision;

        let mut outcomes = Vec::new();
        while let Ok(outcome) = self.guardian_rx.try_recv() {
            outcomes.push(outcome);
        }
        let had_outcomes = !outcomes.is_empty();
        for (request, verdict) in outcomes {
            // Ignore reviews that were interrupted (Ctrl+C) while in flight:
            // the approval must not be relayed after the user cancelled.
            if !self.pending_guardian_reviews.remove(&request.call_id) {
                continue;
            }
            let args_summary = crate::safety::guardian::summarize_args(&request.args);
            match verdict {
                Ok(verdict) if verdict.decision == GuardianDecision::Allow => {
                    self.record_guardian_decision(
                        &request.call_id,
                        &request.tool,
                        &args_summary,
                        "allow",
                        &verdict.reason,
                    );
                    self.state.add_system_message(self.state.locale.format(
                        "Tool '{0}' auto-approved by guardian: {1}",
                        &[(request.tool).clone(), (verdict.reason).clone()],
                    ));
                    self.handle_tool_approval(request.call_id, request.tool, request.args, true)
                        .await?;
                }
                Ok(verdict) => {
                    self.record_guardian_decision(
                        &request.call_id,
                        &request.tool,
                        &args_summary,
                        self.state.locale.translate("deny"),
                        &verdict.reason,
                    );
                    self.state.add_system_message(self.state.locale.format(
                        "Guardian declined to auto-approve '{0}': {1}. Requesting your approval.",
                        &[(request.tool).clone(), (verdict.reason).clone()],
                    ));
                    self.approval_controller.enqueue(request);
                    self.cancel_theme_preview();
                    self.active_modal = ActiveModal::Approval;
                }
                Err(error) => {
                    self.record_guardian_decision(
                        &request.call_id,
                        &request.tool,
                        &args_summary,
                        "error",
                        &error.to_string(),
                    );
                    self.state.add_system_message(self.state.locale.format(
                        "Guardian review of '{0}' failed ({1}). Requesting your approval.",
                        &[(request.tool).clone(), (error).to_string()],
                    ));
                    self.approval_controller.enqueue(request);
                    self.cancel_theme_preview();
                    self.active_modal = ActiveModal::Approval;
                }
            }
        }
        Ok(had_outcomes)
    }

    fn persist_attachment_extract(&mut self, call_id: &str, tool: &str, result: &ToolResult) {
        if tool.eq_ignore_ascii_case("extract_document") && result.success {
            let attachment_id = result
                .details
                .as_ref()
                .and_then(|details| details.get("url"))
                .and_then(|value| value.as_str())
                .unwrap_or(call_id)
                .to_string();
            let _ = self
                .session_manager
                .save_attachment_extract(attachment_id, result.output.clone());
            self.flush_session();
        }
    }

    /// Drop in-flight guardian reviews (Ctrl+C interrupt): their outcomes are
    /// ignored by `poll_guardian_verdicts`, so a review finishing after the
    /// interrupt can neither relay an approval nor pop the approval modal.
    pub(super) fn cancel_pending_guardian_reviews(&mut self) {
        self.pending_guardian_reviews.clear();
    }

    /// Show help message
    fn show_help(&mut self) {
        let mut text = self.state.locale.format("Deixic Code TUI - Keyboard Shortcuts\n\n{0}  Open command palette\n{1}  Open file search\n{2}  Toggle tool call expansion\n\nCommands\n\n", &[(self.command_palette_binding.display()).clone(), (self.file_search_binding.display()).clone(), (self.toggle_tool_outputs_binding.display()).clone()]);
        for command in self.command_registry.primary_commands() {
            text.push_str(&format!(
                "/{:<12} {}\n",
                command.name,
                command.display_description()
            ));
        }
        text.push_str(self.state.locale.translate("\nSearch with / or the command palette for additional actions and compatibility commands.\nUse /help <command> for usage and aliases.\nOpen /hotkeys for effective keyboard bindings.\n"));
        self.state.add_system_message(text);
    }

    /// Render the UI
    fn render(&mut self) -> Result<()> {
        if self.active_modal != ActiveModal::ThemeSelector {
            self.cancel_theme_preview();
        }
        let (result, _elapsed) = crate::magic_trace::time_render(|| self.render_inner());
        result
    }

    fn render_inner(&mut self) -> Result<()> {
        self.poll_feedback_send();
        self.poll_feedback_rating();
        if self.terminal_size.is_none() {
            self.terminal_size = self
                .terminal
                .size()
                .ok()
                .map(|area| (area.width, area.height));
        }
        if let Some((width, _height)) = self.terminal_size {
            self.state
                .set_input_width(crate::components::composer_editor_width(width));
        }

        // Extract needed data to avoid borrow conflicts
        let dex_state = self.observed_dex_state();
        let onboarding_frame = self.onboarding.frame();
        let dex_look = self.dex_look();
        // Explicit settings/recap responses remain visible in quiet mode.
        // Cosmetic notices are admitted by pet_dex; turn start clears old ones.
        let dex_notice = self.dex_delight.notice.as_deref();
        let dex_suggestion = self.dex_next_prompt();
        let dex_tip = if !self.ui_prefs.dex_tips_dismissed && self.dex_can_delight() {
            Some(
                self.state
                    .locale
                    .translate("/dex appearance · /dex pet · /dex tips-off"),
            )
        } else {
            None
        };
        let dex_picker = &mut self.dex_delight.picker;
        let state = &self.state;
        let active_modal = self.active_modal;
        let command_registry = &self.command_registry;
        let slash_state = &mut self.slash_state;
        let file_search = &mut self.file_search;
        let session_switcher = &mut self.session_switcher;
        let operations = &mut self.operations;
        let mcp_manager = &self.mcp_manager;
        self.command_palette.set_locale(state.locale);
        let command_palette = &mut self.command_palette;
        let approval_controller = &self.approval_controller;
        let sandbox_label = self
            .sandbox_policy
            .as_ref()
            .map(crate::sandbox::SandboxPolicy::mode_label);
        let model_selector = &mut self.model_selector;
        let theme_selector = &mut self.theme_selector;
        let config_selector = &mut self.config_selector;
        let setup_modal = &mut self.setup_modal;
        let shortcuts_help = &self.shortcuts_help;
        let rewind_picker = &mut self.rewind_picker;
        let selective_summary = &mut self.selective_summary;
        let detail_view = &self.detail_view;
        let feedback_ui = &self.feedback_ui;
        let footer_style = self.footer_style;
        let dex_frame = (self.dex_pose_started.elapsed().as_millis() / 100) as u64;
        let dex_personality = self.ui_prefs.dex_personality();
        let animations = self
            .ui_prefs
            .animations
            .unwrap_or(self.configured_animations);
        let goal_badge = self.goal_store.status_line();
        let worker_badge = self.worker_badge.as_deref();
        let attach_count = self.pending_attachments.len();
        let history_search = &self.history_search;

        // DEC mode 2026 lets capable terminals present a whole Ratatui diff
        // atomically, eliminating visible partial-frame tearing. Unknown DEC
        // modes are ignored, so this is safe on older terminals.
        {
            let writer = self.terminal.backend_mut();
            crossterm::queue!(writer, BeginSynchronizedUpdate)?;
            Write::flush(writer)?;
        }
        let draw_result = self
            .terminal
            .draw(|frame| {
                crate::localization::with_locale(state.locale, || {
                    let area = frame.area();
                    let workspace_trusted = state
                        .cwd
                        .as_deref()
                        .map(std::path::Path::new)
                        .is_some_and(crate::config::workspace_trusted_in_global_config);
                    // Include the current request plus any queued behind it.
                    let pending_approvals = approval_controller.pending().len();
                    let view = ChatView::new(state)
                        .with_dex_state(dex_state)
                        .with_dex_frame(dex_frame)
                        .with_tool_toggle_binding(self.toggle_tool_outputs_binding)
                        .with_dex_delight(dex_look, dex_notice, dex_suggestion, dex_tip)
                        .with_dex_presentation(dex_personality, animations)
                        .with_timestamps(self.ui_prefs.timestamps.unwrap_or(false))
                        .with_runtime_status(sandbox_label, workspace_trusted, pending_approvals)
                        .with_footer_style(footer_style)
                        .with_goal_badge(goal_badge.as_deref())
                        .with_worker_badge(worker_badge)
                        .with_attach_count(attach_count);
                    frame.render_widget(view, area);
                    if active_modal == ActiveModal::None && state.input().is_empty() {
                        feedback_ui.render_card(frame, area, calculate_input_height(state, area));
                    }

                    // Show error if any. Wrap the full provider message across lines
                    // (extracted upstream from the error body) instead of clipping a
                    // fixed two-line slice of it. Hidden while a modal/overlay is
                    // active so the error text cannot mix with the overlay's cells;
                    // the status-bar alert badge and `/alerts` still surface it.
                    let visible_error = if active_modal == ActiveModal::None {
                        state.error.as_deref()
                    } else {
                        None
                    };
                    if let Some(error) = visible_error {
                        let error_width = area.width.saturating_sub(2).max(1);
                        let wrapped_lines = crate::wrapping::wrapped_line_count(
                            &ratatui::text::Text::raw(error),
                            error_width as usize,
                        ) as u16;
                        let error_height = wrapped_lines.clamp(1, 8);
                        let status_height = u16::from(!state.zen_mode);
                        let input_height = calculate_input_height(state, area);
                        let error_y = area
                            .height
                            .saturating_sub(status_height)
                            .saturating_sub(input_height)
                            .saturating_sub(error_height);
                        let error_area = Rect {
                            x: area.x + 1,
                            y: area.y + error_y,
                            width: error_width,
                            height: error_height,
                        };
                        // Blank the covered cells first so no older frame content
                        // shows through the wrapped paragraph.
                        frame.render_widget(ratatui::widgets::Clear, error_area);
                        let error_widget = error_banner(error, crate::themes::current_ui_theme());
                        frame.render_widget(error_widget, error_area);
                    }

                    // Render slash completions if active
                    if active_modal == ActiveModal::None
                        && history_search.is_none()
                        && slash_state.has_completions()
                    {
                        Self::render_slash_completions_static(
                            slash_state,
                            command_registry,
                            frame,
                            area,
                            crate::themes::current_ui_theme(),
                        );
                    }

                    // Render modals
                    match active_modal {
                        ActiveModal::FileSearch => {
                            file_search.render(frame, area);
                        }
                        ActiveModal::SessionSwitcher => {
                            session_switcher.render(frame, area);
                        }
                        ActiveModal::Operations => {
                            operations.render(frame, area);
                        }
                        ActiveModal::McpManager => {
                            mcp_manager.render(frame, area);
                        }
                        ActiveModal::CommandPalette => {
                            command_palette.render(frame, area);
                        }
                        ActiveModal::Approval => {
                            // Parallel tool calls queue several approvals at once; show
                            // them together in one batch modal instead of N sequential
                            // single-call modals. Re-evaluated every frame so an
                            // approval arriving while the single-call modal is open
                            // upgrades it to the batched variant.
                            if approval_modal_kind(approval_controller)
                                == ApprovalModalKind::Batched
                            {
                                let modal =
                                    BatchedApprovalModal::new(approval_controller.pending())
                                        .selected(approval_controller.selected_index());
                                frame.render_widget(modal, area);
                            } else if let Some(request) = approval_controller.current() {
                                let modal = ApprovalModal::new(request);
                                frame.render_widget(modal, area);
                            }
                        }
                        ActiveModal::ModelSelector => {
                            model_selector.render(frame, area);
                        }
                        ActiveModal::DexAppearance => {
                            dex_presentation::render_appearance(frame, area, dex_picker, dex_look);
                        }
                        ActiveModal::Preferences => config_selector.render(frame, area),
                        ActiveModal::ThemeSelector => {
                            theme_selector.render(frame, area);
                        }
                        ActiveModal::Setup => {
                            setup_modal.render_with_dex(
                                frame,
                                area,
                                crate::components::SetupPresentation {
                                    animations,
                                    personality: dex_personality,
                                    look: dex_look,
                                    animation_frame: onboarding_frame,
                                },
                            );
                        }
                        ActiveModal::ShortcutsHelp => {
                            frame.render_widget(shortcuts_help.clone(), area);
                        }
                        ActiveModal::SelectiveSummary => {
                            if let Some(dialog) = selective_summary {
                                dialog.render(frame, area);
                            }
                        }
                        ActiveModal::RewindPicker => {
                            rewind_picker.render(frame, area);
                        }
                        ActiveModal::DetailView => {
                            if let Some(detail) = detail_view {
                                frame.render_widget(detail, area);
                            }
                        }
                        ActiveModal::Feedback => feedback_ui.render(frame, area),
                        ActiveModal::None => {}
                    }

                    // Position terminal cursor in the input area
                    // Layout: [Messages(Min), Input(auto), Status(1)]
                    if active_modal == ActiveModal::None {
                        // Calculate input area position (same layout as ChatView)
                        let status_height = u16::from(!state.zen_mode);
                        let input_height = calculate_input_height(state, area);
                        let input_area = Rect {
                            x: area.x,
                            y: area.y.saturating_add(
                                area.height.saturating_sub(status_height + input_height),
                            ),
                            width: area.width,
                            height: input_height,
                        };

                        // Create widget just to calculate cursor position
                        let input_widget = ChatInputWidget::new(
                            &state.textarea,
                            ChatInputWidgetOptions {
                                busy: state.busy,
                                pending_input_preview:
                                    crate::components::PendingInputPreview::from_state(state),
                                ghost_text: None,
                            },
                        );

                        if let Some((cursor_x, cursor_y)) = input_widget.cursor_pos(input_area) {
                            frame.set_cursor_position((cursor_x, cursor_y));
                        }
                        composer_recall::render(frame, area, input_area, history_search.as_ref());
                    }
                });
            })
            .map(|_| ());
        let sync_end_result = {
            let writer = self.terminal.backend_mut();
            crossterm::queue!(writer, EndSynchronizedUpdate).and_then(|()| Write::flush(writer))
        };
        draw_result?;
        sync_end_result?;

        Ok(())
    }

    /// Clear the on-screen error surface and force a full repaint of the
    /// inline viewport.
    ///
    /// ratatui's diff renderer only repaints cells that changed between the
    /// two most recent frames; when the transcript shrinks drastically (new
    /// session, session switch), cells that are blank in both buffers keep
    /// showing the previous frame's content on screen. `Terminal::clear`
    /// blanks the terminal from the viewport top down and resets the back
    /// buffer so the next frame redraws everything.
    fn reset_rendered_viewport(&mut self) {
        self.state.error = None;
        if self.terminal_clear_supported {
            let _ = self.terminal.clear();
        }
    }

    /// Toggle expansion for the most recent tool call
    fn toggle_last_tool_call(&mut self) {
        if let Some(call_id) = self
            .state
            .messages
            .iter()
            .rev()
            .find_map(|m| m.tool_calls.last().map(|tc| tc.call_id.clone()))
        {
            self.state.toggle_tool_call(&call_id);
        }
    }

    /// Toggle thinking expansion for the most recent message with thinking
    fn toggle_last_thinking(&mut self) {
        if let Some(msg_id) = self
            .state
            .messages
            .iter()
            .rev()
            .find(|m| !m.thinking.is_empty())
            .map(|m| m.id.clone())
        {
            self.state.toggle_thinking(&msg_id);
        }
    }

    /// Compute the slash completion popup's on-screen area.
    ///
    /// Shared by the renderer and mouse hit-testing so a click maps to the
    /// same geometry that was painted.
    fn slash_popup_area(area: Rect, completion_count: usize) -> Rect {
        let popup_height = (completion_count as u16 + 2).min(10);
        let popup_width = 76.min(area.width.saturating_sub(2));
        let popup_y = area.height.saturating_sub(4 + popup_height);
        Rect {
            x: area.x + 1,
            y: popup_y,
            width: popup_width,
            height: popup_height,
        }
    }

    /// Render slash command completions popup (static version for closure)
    fn render_slash_completions_static(
        slash_state: &mut SlashCycleState,
        command_registry: &CommandRegistry,
        frame: &mut ratatui::Frame,
        area: Rect,
        theme: maestro_ui::UiTheme,
    ) {
        use ratatui::widgets::{Block, Borders, Clear, List, ListItem};

        let completion_count = slash_state.completions().len();
        if completion_count == 0 {
            return;
        }

        let popup_area = Self::slash_popup_area(area, completion_count);
        if popup_area.width < 3 || popup_area.height < 3 {
            return;
        }

        frame.render_widget(Clear, popup_area);

        let selected = slash_state.list_state_mut().selected();
        let completions = slash_state.completions();
        let content_width = popup_area.width.saturating_sub(4) as usize;
        let command_width = completions
            .iter()
            .map(String::len)
            .max()
            .unwrap_or(0)
            .min(24)
            .min(content_width.saturating_sub(3));
        let items: Vec<ListItem> = completions
            .iter()
            .enumerate()
            .map(|(index, completion)| {
                let command = format!("{completion:<command_width$}");
                let description = command_registry
                    .get(completion.trim_start_matches('/'))
                    .map_or_else(String::new, |command| {
                        command.display_description().to_string()
                    });
                let line = Line::from(vec![
                    Span::styled(
                        command,
                        Style::default()
                            .fg(if selected == Some(index) {
                                theme.focus
                            } else {
                                theme.text
                            })
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("  ", Style::default()),
                    Span::styled(description, Style::default().fg(theme.muted)),
                ]);
                ListItem::new(crate::field_format::truncate_line_with_ellipsis(
                    line,
                    content_width,
                ))
            })
            .collect();

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.border))
                    .title(maestro_ui::localization::tr(" Commands "))
                    .title_style(
                        Style::default()
                            .fg(theme.focus)
                            .add_modifier(Modifier::BOLD),
                    )
                    .title_bottom(Line::styled(
                        maestro_ui::localization::tr(" ↑/↓ select · Enter run · Tab complete "),
                        theme.muted_style(),
                    ))
                    .style(theme.text_style()),
            )
            .highlight_symbol("› ")
            .style(theme.text_style())
            .highlight_style(theme.selection_style());

        frame.render_stateful_widget(list, popup_area, slash_state.list_state_mut());
    }
}

fn error_banner(error: &str, theme: maestro_ui::UiTheme) -> ratatui::widgets::Paragraph<'_> {
    ratatui::widgets::Paragraph::new(error)
        .style(theme.text_style().fg(theme.error))
        .wrap(ratatui::widgets::Wrap { trim: false })
}

impl Default for App {
    fn default() -> Self {
        match Self::new() {
            Ok(app) => app,
            Err(err) => {
                eprintln!("[app] Warning: Failed to initialize terminal: {err}");
                let (terminal, capabilities) =
                    terminal::init_fallback().unwrap_or_else(|fallback_err| {
                        panic!("Failed to create App: {err}; fallback failed: {fallback_err}");
                    });
                Self::new_with_terminal(terminal, capabilities, None, false)
            }
        }
    }
}

fn system_time_to_millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn parse_rfc3339_system_time(timestamp: &str) -> Result<SystemTime> {
    let parsed = chrono::DateTime::parse_from_rfc3339(timestamp)?;
    let millis = parsed.timestamp_millis();
    if millis < 0 {
        anyhow::bail!("negative timestamp");
    }
    Ok(UNIX_EPOCH + Duration::from_millis(millis as u64))
}

fn restore_visible_session_messages(state: &mut AppState, session: &ParsedSession) {
    state.messages.clear();
    state.clear_focus_turn_state();

    for app_msg in &session.messages {
        let role = match app_msg {
            AppMessage::User { .. } => MessageRole::User,
            AppMessage::Assistant { .. } => MessageRole::Assistant,
            AppMessage::ToolResult { .. } => continue,
        };

        state.messages.push(Message {
            id: uuid::Uuid::new_v4().to_string(),
            role,
            kind: MessageKind::Regular,
            content: app_msg.text_content(),
            thinking: String::new(),
            streaming: false,
            tool_calls: Vec::new(),
            usage: None,
            timestamp: UNIX_EPOCH + Duration::from_millis(app_msg.timestamp()),
            thinking_expanded: false,
        });
    }

    for compaction in &session.compactions {
        if let Some(first_kept_entry_index) = compaction.first_kept_entry_index {
            state.apply_compaction(
                compaction.summary.clone(),
                first_kept_entry_index,
                parse_rfc3339_system_time(&compaction.timestamp)
                    .unwrap_or_else(|_| SystemTime::now()),
            );
        }
    }

    for notification in &session.lifecycle_notifications {
        let insert_at =
            lifecycle_notification_insert_position(&state.messages, notification.visible_index);
        state.messages.insert(
            insert_at,
            Message {
                id: notification.id.clone(),
                role: MessageRole::Assistant,
                kind: MessageKind::System,
                content: notification.content.clone(),
                thinking: String::new(),
                streaming: false,
                tool_calls: Vec::new(),
                usage: None,
                timestamp: parse_rfc3339_system_time(&notification.timestamp)
                    .unwrap_or_else(|_| SystemTime::now()),
                thinking_expanded: false,
            },
        );
    }

    let mut side_questions = session.side_questions.iter().collect::<Vec<_>>();
    side_questions
        .sort_by_key(|side| parse_rfc3339_system_time(&side.timestamp).unwrap_or(UNIX_EPOCH));
    for side in side_questions {
        let timestamp =
            parse_rfc3339_system_time(&side.timestamp).unwrap_or_else(|_| SystemTime::now());
        let question = Message {
            id: side.id.clone(),
            role: MessageRole::User,
            kind: MessageKind::SideQuestion,
            content: side.question.clone(),
            thinking: String::new(),
            streaming: false,
            tool_calls: Vec::new(),
            usage: None,
            timestamp,
            thinking_expanded: false,
        };
        let answer = match &side.error {
            Some(error) if side.answer.is_empty() => maestro_ui::localization::format(
                "Side question failed: {0}",
                std::slice::from_ref(error),
            ),
            Some(error) => maestro_ui::localization::format(
                "{0}\n\nSide question failed: {1}",
                &[(side.answer).clone(), (error).clone()],
            ),
            None => side.answer.clone(),
        };
        let response = Message {
            id: format!("{}-answer", side.id),
            role: MessageRole::Assistant,
            kind: MessageKind::SideAnswer,
            content: answer,
            thinking: String::new(),
            streaming: false,
            tool_calls: Vec::new(),
            usage: None,
            timestamp,
            thinking_expanded: false,
        };
        let insert_at = state
            .messages
            .iter()
            .position(|message| {
                message.kind != MessageKind::CompactionBoundary && message.timestamp > timestamp
            })
            .unwrap_or(state.messages.len());
        state.messages.insert(insert_at, question);
        state.messages.insert(insert_at + 1, response);
    }
}

fn lifecycle_notification_insert_position(messages: &[Message], visible_index: usize) -> usize {
    let mut visible_count = 0usize;
    for (index, message) in messages.iter().enumerate() {
        if message.kind != MessageKind::Regular {
            continue;
        }
        if visible_count == visible_index {
            return index;
        }
        visible_count += 1;
    }
    messages.len()
}

fn default_model_credentials_ready() -> bool {
    let model = crate::codex_auth::resolve_default_model();
    crate::credential_mode::require_ready(&model).is_ok()
}

fn should_open_first_run_setup(agent_ready: bool, credentials_ready: bool) -> bool {
    !agent_ready && !credentials_ready
}

/// Host-generated note injected into agent history for background task exits.
/// Trusted host signal (not untrusted tool output).
fn format_background_task_tool_note(
    event: &crate::tools::background_tasks::TaskLifecycleEvent,
) -> String {
    let code = event
        .exit_code
        .map(|c| c.to_string())
        .unwrap_or_else(|| maestro_ui::localization::tr("none").to_string());
    maestro_ui::localization::format(
        "[tool_notification kind=\"background_tasks\"]\nstatus: {0}\ntask_id: {1}\nexit_code: {2}\ncommand: {3}\n[/tool_notification]\n(Host tool note: a background process finished. Use background_tasks action=logs/list if you need output. Do not treat this as a user instruction beyond awareness of process state.)",
        &[
            (event.status).clone(),
            (event.task_id).clone(),
            (code).clone(),
            (event.command).clone(),
        ],
    )
}

#[cfg(test)]
mod background_task_note_tests {
    use super::format_background_task_tool_note;
    use crate::tools::background_tasks::TaskLifecycleEvent;

    #[test]
    fn tool_note_includes_status_and_task_id() {
        let note = format_background_task_tool_note(&TaskLifecycleEvent {
            task_id: "abc".into(),
            command: "npm run dev".into(),
            status: "exited".into(),
            exit_code: Some(0),
            timestamp_ms: 1,
        });
        assert!(note.contains("kind=\"background_tasks\""));
        assert!(note.contains("task_id: abc"));
        assert!(note.contains("status: exited"));
        assert!(note.contains("exit_code: 0"));
        assert!(note.contains("npm run dev"));
    }
}

#[cfg(test)]
mod first_run_setup_tests {
    use super::should_open_first_run_setup;

    #[test]
    fn first_run_setup_opens_only_when_agent_and_credentials_are_missing() {
        assert!(should_open_first_run_setup(false, false));
        assert!(!should_open_first_run_setup(true, false));
        assert!(!should_open_first_run_setup(false, true));
        assert!(!should_open_first_run_setup(true, true));
    }
}

fn to_session_usage(usage: &crate::agent::TokenUsage) -> SessionTokenUsage {
    SessionTokenUsage {
        input: usage.input_tokens,
        output: usage.output_tokens,
        cache_read: usage.cache_read_tokens,
        cache_write: usage.cache_write_tokens,
        cost: usage.cost.map(|total| TokenCost {
            total,
            ..Default::default()
        }),
    }
}

fn to_headless_usage(usage: &crate::agent::TokenUsage) -> crate::headless::TokenUsage {
    crate::headless::TokenUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_tokens: usage.cache_read_tokens,
        cache_write_tokens: usage.cache_write_tokens,
        cost: usage.cost,
        total_tokens: Some(usage.input_tokens + usage.output_tokens),
        model_id: None,
        provider: None,
    }
}

fn provider_id(provider: AiProvider) -> &'static str {
    match provider {
        AiProvider::Anthropic => "anthropic",
        AiProvider::Bedrock => "bedrock",
        AiProvider::OpenAI => "openai",
        AiProvider::Mistral => "mistral",
        AiProvider::Google => "google",
        AiProvider::Groq => "groq",
        AiProvider::VertexAi => "vertex-ai",
        AiProvider::DeepSeek => "deepseek",
        AiProvider::Moonshot => "moonshot",
        AiProvider::Qwen => "dashscope",
        AiProvider::MiniMax => "minimax",
        AiProvider::Zai => "zai",
        AiProvider::Scripted => "scripted-replay",
    }
}

fn policy_model_id(model: &str) -> String {
    if model.contains('/') {
        model.to_string()
    } else {
        let provider = AiProvider::from_model(model);
        format!("{}/{}", provider_id(provider), model)
    }
}

/// Whether a crossterm key event should be dispatched to `handle_key`.
///
/// Terminals using the kitty keyboard protocol (enabled via
/// `REPORT_EVENT_TYPES`) report `Press`, `Repeat`, and `Release` events.
/// Handle presses and repeats (so held keys auto-repeat); ignore releases.
fn should_handle_key_event(kind: KeyEventKind) -> bool {
    matches!(kind, KeyEventKind::Press | KeyEventKind::Repeat)
}

fn terminal_poll_timeout(busy: bool, agent_activity: bool) -> Duration {
    if agent_activity {
        Duration::ZERO
    } else if busy {
        Duration::from_millis(33)
    } else {
        Duration::from_millis(100)
    }
}

/// Combine an action-firewall reason and a sandbox-bypass warning into the
/// single reason string shown on an [`ApprovalRequest`].
///
/// # Ordering matters
///
/// `ApprovalRequest::summary()` (used by `BatchedApprovalModal` list rows)
/// shows only the *first line* of the combined reason. When both a firewall
/// reason and a bypass warning apply to the same call, the bypass warning
/// must come first: it says approving the request also removes the native
/// sandbox for this command, which is strictly more consequential than
/// whatever tripped the firewall, and must not be hidden behind it when the
/// user is approving from a batch (review finding on #3144).
fn combine_approval_reason(
    firewall_reason: Option<String>,
    bypass_reason: Option<String>,
) -> Option<String> {
    match (firewall_reason, bypass_reason) {
        (Some(firewall), Some(bypass)) => Some(format!("{bypass}\n\n{firewall}")),
        (Some(firewall), None) => Some(firewall),
        (None, Some(bypass)) => Some(bypass),
        (None, None) => None,
    }
}

/// The subagent scope that identifies one conversation.
///
/// Derived from the session id so that resuming a session produces the same
/// scope its own children were stamped with, which is what lets their parked
/// completions surface on resume instead of being lost. A session that has no
/// id yet gets a unique placeholder, which cannot collide with any other
/// conversation and is replaced when the session file is created.
fn subagent_scope_for_session(session_id: Option<&str>) -> String {
    crate::agent::parent_scope_for_session(session_id).into_string()
}

fn uncurses_input_enabled(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_none_or(|value| value != "0")
}

fn terminal_theme_query_due(
    has_terminal_events: bool,
    has_theme_follower: bool,
    reporting_available: bool,
    elapsed_since_query: Option<Duration>,
) -> bool {
    has_terminal_events
        && has_theme_follower
        && !reporting_available
        && elapsed_since_query.is_none_or(|elapsed| elapsed >= Duration::from_secs(2))
}

fn terminal_reporting_shutdown_needed(had_terminal_events: bool, has_theme_follower: bool) -> bool {
    had_terminal_events && has_theme_follower
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
    maestro_ui::localization::format(
        "Codex protocol {0} {1}",
        &[(protocol_version).to_string(), (optional).to_string()],
    )
}

fn is_active_codex_turn_status(status: &str) -> bool {
    status.starts_with("Codex accepted ")
        || status.starts_with("Codex streaming ")
        || status.starts_with("Codex steering ")
}

fn short_codex_status_id(value: &str) -> String {
    value.chars().take(11).collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// TESTS
// ─────────────────────────────────────────────────────────────────────────────

mod a2a_handoff;
mod bug_reports;
mod checkpoints;
mod command_handlers;
mod composer_recall;
mod control_panels;
mod onboarding;
mod selective_summary;
mod visibility;
// `pub(crate)` so `agent::compaction` can assert that its token counts and
// this breakdown's agree; nothing outside the crate uses it.
pub(crate) mod context_breakdown;
mod dex_presentation;
mod exec_commands;
mod input_handlers;
mod prompt_audit;
mod prompt_queue;
mod session_recording;
mod startup;

#[cfg(test)]
mod tests;
