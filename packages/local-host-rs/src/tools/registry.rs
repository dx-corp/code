//! Tool registry and execution dispatcher
//!
//! This module implements the central registry and executor for agent tools. It provides
//! a type-safe, validated execution environment that bridges AI tool calls to native Rust
//! implementations.
//!
//! # Architecture
//!
//! The registry system consists of two main components:
//!
//! - **`ToolRegistry`**: A HashMap-based registry of tool definitions with JSON schemas.
//!   It validates arguments, checks required fields, and determines approval requirements.
//! - **`ToolExecutor`**: The execution dispatcher that routes tool calls to implementations.
//!   It manages event streams, handles errors, and ensures consistent result reporting.
//!
//! # Tool Definition System
//!
//! Each tool is registered with:
//! - **Name**: Case-insensitive identifier (e.g., "bash", "read")
//! - **Description**: Human-readable explanation of what the tool does
//! - **JSON Schema**: Defines required/optional parameters with types and descriptions
//! - **Approval requirement**: Static boolean or dynamic function based on arguments
//!
//! ## Schema Validation
//!
//! Tool schemas follow JSON Schema specification. The registry validates:
//! - Required fields are present and non-empty
//! - Field types match expectations (string, number, boolean, etc.)
//! - Nested objects conform to their schemas
//!
//! # Execution Model
//!
//! Tool execution follows a request-response pattern with event streaming:
//!
//! ```text
//! ┌─────────────┐
//! │   AI Agent  │
//! └──────┬──────┘
//!        │ Tool call with JSON args
//!        ▼
//! ┌─────────────────┐
//! │  ToolExecutor   │ 1. Validate arguments
//! │                 │ 2. Check approval
//! │                 │ 3. Dispatch to implementation
//! └────────┬────────┘
//!          │
//!          ├──────────────────┬──────────────────┬──────────────────┐
//!          ▼                  ▼                  ▼                  ▼
//!      BashTool           ReadTool          WriteTool         EditTool
//!          │                  │                  │                  │
//!          └──────────────────┴──────────────────┴──────────────────┘
//!                                     │
//!                   ┌─────────────────┼─────────────────┐
//!                   ▼                 ▼                 ▼
//!              ToolStart          ToolOutput         ToolEnd
//!           (via event_tx)     (via event_tx)    (via event_tx)
//! ```
//!
//! # Event Streaming
//!
//! Tools emit events via an unbounded mpsc channel (`mpsc::UnboundedSender<FromAgent>`):
//!
//! 1. **`ToolStart`**: Emitted when execution begins (contains `call_id`)
//! 2. **`ToolOutput`**: Emitted for progress/partial output (optional, repeatable)
//! 3. **`ToolEnd`**: Emitted when execution completes (contains success flag)
//!
//! These events enable real-time UI updates and streaming output display.
//!
//! # Error Handling
//!
//! Errors are returned in the `ToolResult` structure, never panicked:
//! - **Validation errors**: Missing required fields, invalid JSON
//! - **Execution errors**: File not found, permission denied, timeout
//! - **Unknown tools**: Tool name not found in registry
//!
//! All errors set `success: false` and populate the `error` field with a message.
//!
//! # Tool Implementations
//!
//! The executor currently supports these built-in tools:
//!
//! - **bash**: Execute shell commands (see `BashTool` for details)
//! - **read**: Read file contents with line numbers
//! - **write**: Write content to files, creating directories as needed
//! - **edit**: Exact string replacement in files with uniqueness checks
//! - **glob**: Find files matching glob patterns
//! - **grep**: Search file contents using ripgrep/grep
//!
//! New tools can be added by:
//! 1. Implementing the tool logic in a new module
//! 2. Registering the tool definition in `ToolRegistry::new()`
//! 3. Adding a match arm in `ToolExecutor::execute()`

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime};

use crate::sandbox::SandboxPolicy;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::Value;
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::SubagentLifecycleEvent;
use super::ask_user;
use super::background_tasks;
use super::bash::{BashArgs, BashOutputStream, BashTool, BashVersion};
use super::cache::{CacheConfig, CacheKey, CacheStats, CachedResult, ToolResultCache};
use super::details::{
    DiffDetails, EditDetails, GlobDetails, GrepDetails, ListDetails, ReadDetails, WriteDetails,
};
use super::exa;
use super::extract_document;
use super::gh;
use super::image::{ImageTool, ReadImageArgs, ScreenshotArgs};
use super::inline::{InlineTool, InlineToolExecutor, load_inline_tools};
use super::notebook_edit;
use super::orb_delegation::{
    DEFAULT_ORB_MCP_SERVER, OrbConsoleAction, OrbDelegationAdapter, OrbOperationValidator,
    is_reserved_orb_tool,
};
use super::status;
use super::subagents::SubagentManager;
use super::todo;
use super::versions::ToolVersionOverrides;
use super::web_fetch::{WebFetchArgs, WebFetchTool};
use crate::agent::{
    CredentialVault, DenialReason, ExecutionSource, FromAgent, ToolDefinition, ToolExecution,
    ToolResult,
};
use crate::lsp;
use crate::mcp::{
    McpClient, McpConfig, McpConfigScope, McpContent, McpPrompt, McpServerConfig, McpTransport,
    append_mcp_prompt_summary, load_mcp_config_with_managed_connections,
};
use crate::orb_connection::{HostedOrbOwnerBinding, hosted_orb_owner_binding};
use crate::safety::{
    ActionFirewall, FirewallVerdict, expand_tilde, is_tilde_path, run_validators_with_diagnostics,
};

const MAX_READ_SIZE_BYTES: u64 = 10 * 1024 * 1024;
const MAX_GREP_LINES: usize = 100;
const MAX_LIST_LINES: usize = 200;
const MAX_DIFF_LINES: usize = 400;
const MCP_RECONNECT_RETRY_COOLDOWN: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
struct HostedOrbConnectionSnapshot {
    server: McpServerConfig,
    owner_binding: HostedOrbOwnerBinding,
}

/// Resolve the managed Computer server and its opaque owner binding from one
/// immutable configuration snapshot. The client synchronizer must consume
/// that same snapshot; resolving the owner first and reloading configuration
/// for the client can otherwise pair owner A with connection B during a
/// managed-account switch.
fn hosted_orb_connection_snapshot(
    config: &McpConfig,
) -> Result<HostedOrbConnectionSnapshot, String> {
    hosted_orb_connection_snapshot_with(config, |server| {
        hosted_orb_owner_binding(server)
            .map_err(|error| format!("resolve hosted Computer owner binding: {error:#}"))
    })
}

fn hosted_orb_connection_snapshot_with<F>(
    config: &McpConfig,
    resolve_owner: F,
) -> Result<HostedOrbConnectionSnapshot, String>
where
    F: FnOnce(&McpServerConfig) -> Result<HostedOrbOwnerBinding, String>,
{
    let Some(server) = config
        .enabled_servers()
        .find(|server| server.name == DEFAULT_ORB_MCP_SERVER)
        .cloned()
    else {
        return Err(format!(
            "Computer delegation requires a managed MCP server named \"{DEFAULT_ORB_MCP_SERVER}\""
        ));
    };
    if server.transport != McpTransport::Http {
        return Err(
            "Computer delegation requires the hosted HTTP MCP endpoint; local Computer control planes are not supported"
            .to_string(),
        );
    }
    let owner_binding = resolve_owner(&server)?;
    Ok(HostedOrbConnectionSnapshot {
        server,
        owner_binding,
    })
}

fn vault_tool_result_credentials(
    vault: &CredentialVault,
    generation: u64,
    mut result: ToolResult,
) -> ToolResult {
    result.output = vault.vault_in_text_at_generation(generation, &result.output);
    if let Some(error) = result.error.take() {
        result.error = Some(vault.vault_in_text_at_generation(generation, &error));
    }
    if let Some(details) = result.details.take() {
        result.details = Some(vault.vault_in_json_at_generation(generation, &details));
    }
    result
}

fn cache_dependency_paths(cwd: &str, tool_name: &str, args: &Value) -> Vec<PathBuf> {
    let tool_name = tool_name.to_ascii_lowercase();
    let mut dependencies = Vec::new();

    let mut add_path = |base_cwd: &str, value: &Value| {
        if let Some(raw) = value.as_str() {
            if let Ok(path) = resolve_tool_path(base_cwd, raw) {
                dependencies.push(PathBuf::from(path));
            }
        }
    };

    match tool_name.as_str() {
        "read" | "read_image" => {
            if let Some(path) = args.get("path").or_else(|| args.get("file_path")) {
                add_path(cwd, path);
            }
        }
        "glob" | "find" | "list" | "grep" => {
            if let Some(path) = args.get("path").or_else(|| args.get("directory")) {
                add_path(cwd, path);
            } else {
                dependencies.push(PathBuf::from(cwd));
            }
        }
        "search" | "parallel_ripgrep" => {
            // Search executes relative paths from its command cwd. Keep cache
            // dependencies in that same coordinate system so a later edit of
            // subdir/file.rs invalidates a search run with cwd=subdir.
            let search_cwd = args
                .get("cwd")
                .and_then(Value::as_str)
                .and_then(|raw| resolve_tool_path(cwd, raw).ok())
                .unwrap_or_else(|| cwd.to_string());
            match args.get("paths") {
                Some(Value::Array(paths)) => {
                    for path in paths {
                        add_path(&search_cwd, path);
                    }
                }
                Some(paths) => add_path(&search_cwd, paths),
                None => dependencies.push(PathBuf::from(search_cwd)),
            }
        }
        "diff" | "status" => dependencies.push(PathBuf::from(cwd)),
        "vscode_get_diagnostics" | "jetbrains_get_diagnostics" => {
            if let Some(raw_uri) = args.get("uri").and_then(Value::as_str) {
                let normalized = normalize_uri_input(raw_uri);
                if let Ok(path) = resolve_tool_path(cwd, &normalized) {
                    dependencies.push(PathBuf::from(path));
                }
            } else {
                // Workspace diagnostics have no file-specific key; the
                // workspace directory is the narrowest safe dependency.
                dependencies.push(PathBuf::from(cwd));
            }
        }
        "vscode_get_definition"
        | "jetbrains_get_definition"
        | "vscode_find_references"
        | "jetbrains_find_references" => {
            // Definitions and references are project-sensitive: editing a
            // different file can change the answer to a query for this URI.
            // Keep the URI for precise bookkeeping, but also bind the entry to
            // the workspace so edits elsewhere invalidate it.
            dependencies.push(PathBuf::from(cwd));
            if let Some(raw_uri) = args.get("uri").and_then(Value::as_str) {
                let normalized = normalize_uri_input(raw_uri);
                if let Ok(path) = resolve_tool_path(cwd, &normalized) {
                    dependencies.push(PathBuf::from(path));
                }
            }
        }
        "vscode_read_file_range" | "jetbrains_read_file_range" => {
            if let Some(raw_uri) = args.get("uri").and_then(Value::as_str) {
                let normalized = normalize_uri_input(raw_uri);
                if let Ok(path) = resolve_tool_path(cwd, &normalized) {
                    dependencies.push(PathBuf::from(path));
                }
            }
        }
        _ => {}
    }

    dependencies.sort_unstable();
    dependencies.dedup();
    dependencies
}

fn build_glob_pattern(base_path: &str, pattern: &str) -> String {
    if Path::new(pattern).is_absolute() {
        return pattern.to_string();
    }

    Path::new(base_path)
        .join(pattern)
        .to_string_lossy()
        .to_string()
}

fn resolve_tool_path(cwd: &str, input: &str) -> Result<String, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("Missing file_path argument".to_string());
    }

    let path = Path::new(trimmed);
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else if is_tilde_path(path) {
        expand_tilde(path)
            .ok_or_else(|| "Home directory unavailable for ~ expansion".to_string())?
    } else {
        Path::new(cwd).join(path)
    };

    Ok(resolved.to_string_lossy().to_string())
}

fn normalize_uri_input(input: &str) -> String {
    if let Some(rest) = input.strip_prefix("file://") {
        let mut path = rest.to_string();
        let mut stripped_localhost = false;
        if let Some(stripped) = path.strip_prefix("localhost/") {
            path = stripped.to_string();
            stripped_localhost = true;
        }
        #[cfg(not(windows))]
        if stripped_localhost && !path.starts_with('/') {
            path = format!("/{path}");
        }
        #[cfg(windows)]
        {
            if path.len() >= 3 && path.as_bytes()[0] == b'/' && path.as_bytes()[2] == b':' {
                path = path[1..].to_string();
            }
        }
        return path;
    }
    input.to_string()
}

async fn read_file_range(
    path: &str,
    start_line: usize,
    end_line: usize,
) -> Result<(String, usize), String> {
    if start_line > end_line {
        return Err("startLine must be <= endLine".to_string());
    }

    let file = tokio::fs::File::open(path)
        .await
        .map_err(|e| format!("Failed to open file: {e}"))?;
    let reader = tokio::io::BufReader::new(file);
    let mut lines = reader.lines();
    let mut output = String::new();
    let mut index: usize = 0;
    let mut lines_read = 0;

    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|e| format!("Failed to read file: {e}"))?
    {
        if index >= start_line && index <= end_line {
            output.push_str(&line);
            output.push('\n');
            lines_read += 1;
        }
        if index >= end_line {
            break;
        }
        index += 1;
    }

    Ok((output, lines_read))
}

fn to_shell_path(path: &str) -> String {
    #[cfg(windows)]
    {
        let normalized = path.replace('\\', "/");
        if normalized.starts_with("//") {
            return normalized;
        }
        if normalized.len() >= 2 && normalized.as_bytes().get(1) == Some(&b':') {
            let drive = normalized[0..1].to_ascii_lowercase();
            let rest = normalized[2..].trim_start_matches('/');
            if rest.is_empty() {
                return format!("/{}", drive);
            }
            return format!("/{}/{}", drive, rest);
        }
        normalized
    }

    #[cfg(not(windows))]
    {
        path.to_string()
    }
}

fn normalize_shell_path(input: &str) -> Result<(String, String), String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("Missing path argument".to_string());
    }
    let path = Path::new(trimmed);
    if is_tilde_path(path) {
        let expanded = expand_tilde(path)
            .ok_or_else(|| "Home directory unavailable for ~ expansion".to_string())?;
        let display = expanded.to_string_lossy().to_string();
        let shell_path = to_shell_path(&display);
        return Ok((display, shell_path));
    }

    let display = trimmed.to_string();
    let shell_path = to_shell_path(trimmed);
    Ok((display, shell_path))
}

fn normalize_git_path(cwd: &str, input: &str) -> Result<(String, String), String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("Missing path argument".to_string());
    }

    let path = Path::new(trimmed);
    let mut resolved = if path.is_absolute() {
        path.to_path_buf()
    } else if is_tilde_path(path) {
        expand_tilde(path)
            .ok_or_else(|| "Home directory unavailable for ~ expansion".to_string())?
    } else {
        path.to_path_buf()
    };

    let cwd_path = Path::new(cwd);
    let relative = dunce::canonicalize(cwd_path)
        .ok()
        .and_then(|cwd_canon| {
            dunce::canonicalize(&resolved)
                .ok()
                .and_then(|path_canon| path_canon.strip_prefix(&cwd_canon).ok().map(PathBuf::from))
        })
        .or_else(|| resolved.strip_prefix(cwd_path).ok().map(PathBuf::from));

    if let Some(rel) = relative {
        resolved = rel;
    }

    let display = resolved.to_string_lossy().to_string();
    let shell_path = to_shell_path(&display);
    Ok((display, shell_path))
}

fn extract_grep_path(line: &str) -> Option<&str> {
    let bytes = line.as_bytes();
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == b':' {
            let mut digit_idx = idx + 1;
            if digit_idx < bytes.len() && bytes[digit_idx].is_ascii_digit() {
                while digit_idx < bytes.len() && bytes[digit_idx].is_ascii_digit() {
                    digit_idx += 1;
                }
                if digit_idx < bytes.len() && bytes[digit_idx] == b':' {
                    let path = &line[..idx];
                    return if path.is_empty() { None } else { Some(path) };
                }
            }
        }
        idx += 1;
    }
    None
}

fn is_probably_binary(data: &[u8]) -> bool {
    data.iter().take(2048).any(|byte| *byte == 0)
}

/// Lifecycle state shown by the MCP manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpLifecycleState {
    Connecting,
    Ready,
    NeedsAuth,
    Failed,
    Disabled,
    BlockedByPolicy,
    NeedsWorkspaceTrust,
    ConfigError,
}

impl McpLifecycleState {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Ready => "ready",
            Self::NeedsAuth => "needs auth",
            Self::Failed => "failed",
            Self::Disabled => "disabled",
            Self::BlockedByPolicy => "blocked by policy",
            Self::NeedsWorkspaceTrust => "needs workspace trust",
            Self::ConfigError => "config error",
        }
    }
}

/// MCP server status snapshot for UI and CLI rendering.
#[derive(Debug, Clone, serde::Serialize)]
pub struct McpServerStatus {
    pub name: String,
    pub state: McpLifecycleState,
    pub connected: bool,
    pub scope: McpConfigScope,
    pub transport: McpTransport,
    pub error: Option<String>,
    pub tools: Vec<String>,
    pub disabled_tools: Vec<String>,
    pub resources: Vec<String>,
    pub prompts: Vec<String>,
}

fn mcp_lifecycle_state(
    server: &crate::mcp::McpServerConfig,
    connected: bool,
    error: Option<&str>,
    workspace: &Path,
) -> McpLifecycleState {
    if !server.is_enabled() {
        return McpLifecycleState::Disabled;
    }
    if crate::mcp::server_requires_workspace_approval(server)
        && !crate::config::workspace_trusted_in_global_config(workspace)
    {
        return McpLifecycleState::NeedsWorkspaceTrust;
    }
    if connected {
        return McpLifecycleState::Ready;
    }
    let Some(error) = error else {
        return McpLifecycleState::Connecting;
    };
    let lower = error.to_ascii_lowercase();
    if lower.contains("sandbox policy")
        || lower.contains("managed policy")
        || lower.contains("blocked")
    {
        McpLifecycleState::BlockedByPolicy
    } else if lower.contains("unauthorized")
        || lower.contains("forbidden")
        || lower.contains("authentication")
        || lower.contains("access token")
        || lower.contains("oauth")
    {
        if server.auth_preset.as_deref() == Some("none") {
            McpLifecycleState::Failed
        } else {
            McpLifecycleState::NeedsAuth
        }
    } else {
        McpLifecycleState::Failed
    }
}

/// Tool executor that dispatches and runs agent tools
///
/// The executor is the primary interface for tool execution. It maintains instances
/// of all tool implementations and routes calls based on the tool name. Each executor
/// is bound to a working directory that becomes the current directory for all tools.
///
/// # Design
///
/// The executor uses a match-based dispatch system rather than dynamic dispatch or
/// trait objects. This provides:
/// - Zero-cost abstraction (no vtable lookups)
/// - Compile-time verification of tool implementations
/// - Easy addition of new tools via match arms
///
/// # Working Directory
///
/// The `cwd` field is passed to all tool instances and used for:
/// - Resolving relative paths in file operations
/// - Setting the working directory for bash commands
/// - Glob pattern base directory
///
/// # Thread Safety
///
/// `ToolExecutor` is `Send` but not `Sync` because it contains `BashTool` which uses
/// non-Sync primitives. However, it can be moved across async tasks and used within
/// a single-threaded context safely.
pub struct ToolExecutor {
    code_authority: Option<crate::code_authority::CodeToolAuthority>,
    /// Shared vault used to keep credential references valid across this execution session.
    credential_vault: CredentialVault,

    /// Bash command execution tool
    ///
    /// Handles shell command execution with approval logic and timeout enforcement.
    bash: BashTool,

    /// Native sandbox requested for this executor, if any.
    sandbox_policy: Option<SandboxPolicy>,
    /// Whether this executor runs where no human can answer an approval
    /// prompt (print/exec mode and other headless runs).
    unattended: bool,

    /// The organization's MCP server policy, applied to every MCP client this
    /// executor builds. `None` means no administrator opinion.
    managed_mcp_policy: Option<crate::mcp::ManagedMcpPolicy>,

    /// Whether successful write/edit calls may run process-global safe-mode
    /// validators. Governed runtimes disable this and own validation separately.
    ambient_mutation_validators_enabled: bool,

    /// Pinned behavior versions for version-managed tools (empty = all tools
    /// run their current behavior). Session replay populates this from the
    /// versions recorded in session-entry receipts.
    tool_versions: ToolVersionOverrides,

    /// Web fetch tool for retrieving web content
    ///
    /// Fetches URLs and converts HTML to markdown for the agent to process.
    web_fetch: WebFetchTool,

    /// Image tool for reading images and capturing screenshots
    ///
    /// Enables vision-capable models to work with images.
    image: ImageTool,

    /// Inline tool executor for user-defined shell-based tools
    ///
    /// Executes tools defined in .composer/tools.json files.
    inline_executor: InlineToolExecutor,

    /// Inline tools loaded from configuration
    ///
    /// Maps lowercase tool names to their definitions.
    inline_tools: HashMap<String, InlineTool>,

    /// Durable child-agent delegation shared by tool calls in this workspace.
    subagents: Arc<SubagentManager>,
    coding_task: std::sync::Mutex<Option<coding_task::CodingTaskState>>,

    /// Capability-bound identity for model-facing mailbox operations.
    mailbox_identity: String,

    /// Current working directory for all tool operations
    ///
    /// This directory is used as the base for relative paths and as the cwd for
    /// spawned processes. Typically set to the workspace root.
    cwd: String,

    /// Tool registry for validation and metadata
    ///
    /// Contains tool definitions with JSON schemas, used for argument validation
    /// and approval checking before execution.
    registry: ToolRegistry,

    /// Cache for tool results
    ///
    /// Caches results from read-only tools (read, glob, grep) to avoid redundant
    /// operations. Uses `RwLock` for thread-safe access across async tasks.
    cache: RwLock<ToolResultCache>,

    /// Last full textual `read` projection for each resolved file path.
    /// Cache entries retain full results; this session-local map only changes
    /// what a repeat read returns to the model.
    read_snapshots: RwLock<HashMap<String, String>>,

    /// MCP client for resource tools (lazy-initialized)
    mcp_client: tokio::sync::Mutex<Option<Arc<crate::mcp::McpClient>>>,

    /// Serializes managed-connection snapshots with MCP client replacement and
    /// Orb adapter installation. Existing adapters retain their own client
    /// snapshot when a managed connection changes, so an old owner cannot be
    /// paired with the newly selected connection.
    mcp_sync_lock: tokio::sync::Mutex<()>,

    /// MCP tool annotations for approval hints
    mcp_tool_annotations: RwLock<HashMap<String, crate::mcp::McpToolAnnotations>>,

    /// Last connection error for configured MCP servers.
    mcp_last_errors: RwLock<HashMap<String, String>>,

    /// Last synced MCP config snapshot, keyed by server name.
    mcp_synced_configs: RwLock<HashMap<String, crate::mcp::McpServerConfig>>,

    /// Last reconnect attempt timestamp for configured MCP servers.
    mcp_last_connect_attempts: RwLock<HashMap<String, Instant>>,

    /// Stable local permission identities for the currently admitted MCP
    /// tools. Managed/enterprise tools are deliberately absent.
    mcp_permission_identities: RwLock<HashMap<String, crate::mcp::McpPermissionIdentity>>,

    /// Executor used for the tools in
    /// [`crate::tools::executor::PROCESS_ISOLATABLE_TOOLS`], when the caller
    /// configured one.
    ///
    /// `None` -- the default -- means every tool runs on this process's own
    /// dispatch. When it is set, only the isolatable tools are handed to it;
    /// approvals, hooks, the action firewall, sandbox-policy denial, the
    /// result cache, credential vaulting, and receipt construction have all
    /// already run in this process before the handoff, and every other tool
    /// still runs here.
    isolated_dispatch: Option<Arc<dyn crate::tools::executor::ToolExecutor>>,
}

/// Every exact name that `execute_impl`'s dispatch `match` (in
/// `tools/registry/execute.rs`) handles *before* falling through to the
/// inline-tool lookup in its
/// wildcard arm.
///
/// Several of these -- `ls`, `readimage`, `webfetch` -- are pure dispatch
/// aliases that are never separately present in `ToolRegistry`'s own name
/// map, so `registry.get(&name)` alone does not catch them: an inline tool
/// registered under one of these names would pass that check, display its
/// configured command in the approval dialog, and then never actually run
/// (the alias's built-in arm intercepts the call first).
///
/// Kept as an explicit mirror of `execute_impl`'s match arms rather than
/// derived from the registry, since being alias-only is exactly what the
/// registry-only check missed. Update this list alongside any new match arm
/// added there.
///
/// This deliberately does NOT cover the `mcp_`/`mcp__` prefix rule --
/// `execute_impl` checks `McpClient::is_mcp_tool` *before* this dispatch
/// match is even reached (`tools/registry/execute.rs`), so callers of this
/// function (currently just `register_inline_tools`) must also check
/// `McpClient::is_mcp_tool` themselves rather than relying on it being
/// folded in here.
fn is_reserved_execute_dispatch_name(name: &str) -> bool {
    if super::subagents::SUBAGENT_TOOL_NAMES.contains(&name) {
        return true;
    }
    // These spellings intentionally mirror execute_impl exactly. Inline
    // lookup lower-cases its wildcard input, so an unmatched case variant
    // such as `BASH` remains a valid inline name even though `bash` and
    // `Bash` are intercepted by built-in dispatch.
    matches!(
        name,
        "coding_task"
            | "bash"
            | "Bash"
            | "read"
            | "Read"
            | "write"
            | "Write"
            | "glob"
            | "Glob"
            | "grep"
            | "Grep"
            | "edit"
            | "Edit"
            | "diff"
            | "Diff"
            | "list"
            | "List"
            | "ls"
            | "find"
            | "Find"
            | "search"
            | "Search"
            | "parallel_ripgrep"
            | "tool_search"
            | "recall_output"
            | "explore"
            | "Explore"
            | "ParallelRipgrep"
            | "status"
            | "Status"
            | "background_tasks"
            | "todo"
            | "get_goal"
            | "update_goal"
            | "ask_user"
            | "draft_feedback"
            | "extract_document"
            | "notebook_edit"
            | "websearch"
            | "codesearch"
            | "gh_pr"
            | "gh_issue"
            | "gh_repo"
            | "mcp_list_resources"
            | "mcp_list_prompts"
            | "mcp_read_resource"
            | "mcp_get_prompt"
            | "vscode_get_diagnostics"
            | "jetbrains_get_diagnostics"
            | "vscode_get_definition"
            | "jetbrains_get_definition"
            | "vscode_find_references"
            | "jetbrains_find_references"
            | "vscode_read_file_range"
            | "jetbrains_read_file_range"
            | "web_fetch"
            | "WebFetch"
            | "webfetch"
            | "read_image"
            | "ReadImage"
            | "readimage"
            | "screenshot"
            | "Screenshot"
    )
}

/// Register `inline_tools_list` into `registry`, skipping any name that
/// collides with an already-registered built-in tool (including its
/// dispatch aliases -- see [`is_reserved_execute_dispatch_name`]) or with
/// the `mcp_`/`mcp__` prefix `McpClient::is_mcp_tool` reserves.
///
/// Built-in and MCP tools both dispatch ahead of the inline fallback in
/// `execute_impl`, so an inline tool reusing a built-in name, one of its
/// aliases, or an MCP-reserved prefix would never actually run, while the
/// approval dialog would still show its configured command -- approving a
/// call that silently invokes something else (or fails against MCP)
/// instead. Shared by every `ToolExecutor` constructor (and the test-only
/// `with_inline_tools_for_test`) so the collision check can't drift between
/// them.
fn register_inline_tools(
    registry: &mut ToolRegistry,
    inline_tools_list: Vec<InlineTool>,
) -> HashMap<String, InlineTool> {
    let mut inline_tools = HashMap::new();
    for tool in inline_tools_list {
        let name = tool.definition.name.to_lowercase();
        let dispatch_name = tool.definition.name.as_str();
        if is_reserved_execute_dispatch_name(dispatch_name) || McpClient::is_mcp_tool(dispatch_name)
        {
            eprintln!(
                "Warning: Skipping inline tool '{}': name collides with a built-in tool, \
                 one of its dispatch aliases, or the mcp_/mcp__ prefix reserved for MCP tools",
                tool.definition.name
            );
            continue;
        }
        registry.register_exact(
            dispatch_name,
            ToolDefinition {
                tool: tool.to_tool(),
                requires_approval: tool.requires_approval(),
            },
        );
        inline_tools.insert(name, tool);
    }
    inline_tools
}

struct ToolExecutionContext<'a> {
    code_decision: Option<crate::code_authority::CodeAuthorityDecision>,
    cancel: Option<CancellationToken>,
    approved_inline_env: Option<&'a HashMap<String, String>>,
    hooks: Option<&'a mut crate::hooks::IntegratedHookSystem>,
    /// Receipt callers suppress the dispatcher lifecycle events; new execute_impl
    /// arms must use lifecycle_event_tx so ToolEnd remains singular.
    emit_tool_events: bool,
}

pub(crate) struct ToolExecutionOptions<'a> {
    pub(crate) cancel: CancellationToken,
    pub(crate) approved_inline_env: Option<&'a HashMap<String, String>>,
    pub(crate) hooks: Option<&'a mut crate::hooks::IntegratedHookSystem>,
}

impl ToolExecutor {
    #[cfg(test)]
    pub(crate) fn with_test_code_authority(mut self) -> Self {
        self.code_authority = Some(crate::code_authority::CodeToolAuthority::for_test(vec![]));
        self
    }

    pub fn with_code_authority(mut self) -> Self {
        self.code_authority = crate::code_authority::CodeToolAuthority::configured();
        self
    }

    pub(crate) fn has_code_authority(&self) -> bool {
        self.code_authority.is_some()
    }

    async fn authorize_code_call(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        call_id: &str,
        generation: u64,
        inline_env: Option<&HashMap<String, String>>,
    ) -> anyhow::Result<Option<crate::code_authority::CodeAuthorityDecision>> {
        use sha2::{Digest as _, Sha256};
        let Some(authority) = &self.code_authority else {
            return Ok(None);
        };
        let definition = if let Some(definition) = self.registry.get(tool_name) {
            definition.tool.clone()
        } else if McpClient::is_mcp_tool(tool_name) {
            self.ensure_mcp_client()
                .await
                .map_err(anyhow::Error::msg)?
                .list_all_tools()
                .await
                .into_iter()
                .find(|tool| tool.name == tool_name)
                .ok_or_else(|| anyhow::anyhow!("Code MCP schema unavailable"))?
        } else {
            anyhow::bail!("Code tool schema unavailable");
        };
        let task_id = self
            .coding_contract()
            .map(|c| c.task_id)
            .unwrap_or_else(|| authority.session_id().to_string());
        let repository = dunce::canonicalize(&self.cwd)?;
        let inline = self.get_inline_tool(tool_name).map(|tool| serde_json::json!({
            "command": tool.definition.command, "cwd": tool.definition.cwd,
            "environment": inline_env.unwrap_or(&tool.definition.env), "timeout": tool.definition.timeout,
            "sourcePath": tool.source_path,
        }));
        let definition = serde_json::json!({"schema": definition, "inlineExecution": inline});
        let invocation = serde_json::json!({
            "codeSessionId": authority.session_id(), "taskId": task_id,
            "repositoryId": repository.to_string_lossy(), "runtimeGeneration": generation.checked_add(1).ok_or_else(|| anyhow::anyhow!("Code runtime generation exhausted"))?.to_string(),
            "callId": call_id, "toolId": tool_name,
            "toolSchemaDigest": format!("{:x}", Sha256::digest(serde_json::to_vec(&definition)?)),
            "argumentsDigest": format!("{:x}", Sha256::digest(serde_json::to_vec(args)?)),
        });
        Ok(Some(authority.authorize(invocation).await?))
    }

    /// Stop and reap background Bash commands owned by this executor.
    pub async fn shutdown_background_processes(&self) {
        self.bash.shutdown_background_processes().await;
    }

    /// Create a new tool executor with the given working directory
    ///
    /// # Arguments
    ///
    /// - `cwd`: Working directory for all tool operations. Accepts any type that
    ///   converts to String (String, &str, `PathBuf` via display, etc.)
    ///
    /// # Examples
    ///
    /// ```
    /// use maestro_local_host::tools::ToolExecutor;
    ///
    /// // From &str
    /// let executor = ToolExecutor::new("/workspace");
    ///
    /// // From String
    /// let cwd = String::from("/home/user/project");
    /// let executor = ToolExecutor::new(cwd);
    ///
    /// // From PathBuf
    /// use std::path::PathBuf;
    /// let path = PathBuf::from("/tmp");
    /// let executor = ToolExecutor::new(path.display().to_string());
    /// ```
    pub fn new(cwd: impl Into<String>) -> Self {
        Self::with_credential_vault(cwd, CredentialVault::new())
    }

    /// Create a new tool executor using a caller-provided credential vault.
    pub fn with_credential_vault(
        cwd: impl Into<String>,
        credential_vault: CredentialVault,
    ) -> Self {
        let cwd = cwd.into();
        let cwd_path = std::path::Path::new(&cwd);

        // Load inline tools from config files
        let inline_tools_list = load_inline_tools(cwd_path);
        let mut registry = ToolRegistry::new();
        let inline_tools = register_inline_tools(&mut registry, inline_tools_list);

        Self {
            code_authority: None,
            credential_vault,
            bash: BashTool::new(&cwd),
            sandbox_policy: None,
            unattended: false,
            managed_mcp_policy: None,
            ambient_mutation_validators_enabled: true,
            tool_versions: ToolVersionOverrides::default(),
            web_fetch: WebFetchTool::new(),
            image: ImageTool::new(),
            inline_executor: InlineToolExecutor::new(&cwd),
            inline_tools,
            subagents: Arc::new(SubagentManager::new(cwd.clone())),
            coding_task: std::sync::Mutex::new(None),
            mailbox_identity: crate::mailbox::local_identity(),
            cwd,
            registry,
            cache: RwLock::new(ToolResultCache::default()),
            read_snapshots: RwLock::new(HashMap::new()),
            mcp_client: tokio::sync::Mutex::new(None),
            mcp_sync_lock: tokio::sync::Mutex::new(()),
            mcp_tool_annotations: RwLock::new(HashMap::new()),
            mcp_last_errors: RwLock::new(HashMap::new()),
            mcp_synced_configs: RwLock::new(HashMap::new()),
            mcp_last_connect_attempts: RwLock::new(HashMap::new()),
            mcp_permission_identities: RwLock::new(HashMap::new()),
            isolated_dispatch: None,
        }
    }

    /// Test-only constructor that registers a caller-supplied inline tool
    /// list directly instead of loading `.composer/tools.json` from disk.
    ///
    /// The real constructors above go through `load_inline_tools`, which
    /// gates project-level tools on `workspace_trusted_in_global_config` --
    /// reading the *real* process `$HOME`. That can't be faked
    /// deterministically in a test without mutating a process-global env
    /// var, which would race every other test in this (parallel-by-default)
    /// binary that also reads `$HOME`. This entry point exercises exactly
    /// the same registration/collision logic (via [`register_inline_tools`])
    /// as every other constructor, with the tool list supplied directly.
    #[cfg(test)]
    pub(crate) fn with_inline_tools_for_test(
        cwd: impl Into<String>,
        inline_tools_list: Vec<InlineTool>,
    ) -> Self {
        let cwd = cwd.into();
        let mut registry = ToolRegistry::new();
        let inline_tools = register_inline_tools(&mut registry, inline_tools_list);

        Self {
            code_authority: None,
            credential_vault: CredentialVault::new(),
            bash: BashTool::new(&cwd),
            sandbox_policy: None,
            unattended: false,
            managed_mcp_policy: None,
            ambient_mutation_validators_enabled: true,
            tool_versions: ToolVersionOverrides::default(),
            web_fetch: WebFetchTool::new(),
            image: ImageTool::new(),
            inline_executor: InlineToolExecutor::new(&cwd),
            inline_tools,
            subagents: Arc::new(SubagentManager::new(cwd.clone())),
            coding_task: std::sync::Mutex::new(None),
            mailbox_identity: crate::mailbox::local_identity(),
            cwd,
            registry,
            cache: RwLock::new(ToolResultCache::default()),
            read_snapshots: RwLock::new(HashMap::new()),
            mcp_client: tokio::sync::Mutex::new(None),
            mcp_sync_lock: tokio::sync::Mutex::new(()),
            mcp_tool_annotations: RwLock::new(HashMap::new()),
            mcp_last_errors: RwLock::new(HashMap::new()),
            mcp_synced_configs: RwLock::new(HashMap::new()),
            mcp_last_connect_attempts: RwLock::new(HashMap::new()),
            mcp_permission_identities: RwLock::new(HashMap::new()),
            isolated_dispatch: None,
        }
    }

    /// Create a new tool executor with custom cache configuration
    ///
    /// # Arguments
    ///
    /// - `cwd`: Working directory for all tool operations
    /// - `cache_config`: Configuration for the tool result cache
    pub fn with_cache_config(cwd: impl Into<String>, cache_config: CacheConfig) -> Self {
        Self::with_cache_config_and_credential_vault(cwd, cache_config, CredentialVault::new())
    }

    /// Create a new tool executor with custom cache configuration and a shared vault.
    pub fn with_cache_config_and_credential_vault(
        cwd: impl Into<String>,
        cache_config: CacheConfig,
        credential_vault: CredentialVault,
    ) -> Self {
        let cwd = cwd.into();
        let cwd_path = std::path::Path::new(&cwd);

        // Load inline tools from config files
        let inline_tools_list = load_inline_tools(cwd_path);
        let mut registry = ToolRegistry::new();
        let inline_tools = register_inline_tools(&mut registry, inline_tools_list);

        Self {
            code_authority: None,
            credential_vault,
            bash: BashTool::new(&cwd),
            sandbox_policy: None,
            unattended: false,
            managed_mcp_policy: None,
            ambient_mutation_validators_enabled: true,
            tool_versions: ToolVersionOverrides::default(),
            web_fetch: WebFetchTool::new(),
            image: ImageTool::new(),
            inline_executor: InlineToolExecutor::new(&cwd),
            inline_tools,
            subagents: Arc::new(SubagentManager::new(cwd.clone())),
            coding_task: std::sync::Mutex::new(None),
            mailbox_identity: crate::mailbox::local_identity(),
            cwd,
            registry,
            cache: RwLock::new(ToolResultCache::new(cache_config)),
            read_snapshots: RwLock::new(HashMap::new()),
            mcp_client: tokio::sync::Mutex::new(None),
            mcp_sync_lock: tokio::sync::Mutex::new(()),
            mcp_tool_annotations: RwLock::new(HashMap::new()),
            mcp_last_errors: RwLock::new(HashMap::new()),
            mcp_synced_configs: RwLock::new(HashMap::new()),
            mcp_last_connect_attempts: RwLock::new(HashMap::new()),
            mcp_permission_identities: RwLock::new(HashMap::new()),
            isolated_dispatch: None,
        }
    }

    /// Route the tools in
    /// [`crate::tools::executor::PROCESS_ISOLATABLE_TOOLS`] through
    /// `dispatch` instead of this process's own dispatch.
    ///
    /// Everything the agent decides before a tool runs still runs here, and
    /// every tool outside that list still runs here too.
    #[must_use]
    pub fn with_isolated_dispatch(
        mut self,
        dispatch: Arc<dyn crate::tools::executor::ToolExecutor>,
    ) -> Self {
        self.isolated_dispatch = Some(dispatch);
        self
    }

    /// The isolated executor for `tool_name`, if one is configured and the
    /// tool is allowed to leave this process.
    fn isolated_dispatch_for(
        &self,
        tool_name: &str,
    ) -> Option<Arc<dyn crate::tools::executor::ToolExecutor>> {
        // Inline tools can never reach here under an isolatable name:
        // `register_inline_tools` rejects every name in
        // `is_reserved_execute_dispatch_name`, which covers all of them.
        let dispatch = self.isolated_dispatch.as_ref()?;
        crate::tools::executor::is_process_isolatable(tool_name).then(|| Arc::clone(dispatch))
    }

    /// The invocation handed across the executor seam for one tool call.
    fn isolated_invocation(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        call_id: &str,
        approved_inline_env: Option<&HashMap<String, String>>,
    ) -> crate::tools::executor::ToolInvocation {
        let env = approved_inline_env
            .map(|environment| {
                let mut entries = environment
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect::<Vec<_>>();
                entries.sort();
                entries
            })
            .unwrap_or_default();
        crate::tools::executor::ToolInvocation::new(
            call_id,
            tool_name,
            args.clone(),
            PathBuf::from(&self.cwd),
        )
        .with_env(env)
        .with_sandbox(crate::tools::executor::SandboxPolicySnapshot::from_policy(
            self.sandbox_policy.clone(),
        ))
    }

    /// Run one invocation on this process's dispatch, with no policy, cache,
    /// hook, or receipt work.
    ///
    /// This is the raw entry point behind
    /// [`crate::tools::executor::InProcessExecutor`]. Callers that have not
    /// already applied the layers above the executor seam must use
    /// [`Self::execute`] instead.
    pub(crate) async fn dispatch_invocation(
        &self,
        invocation: &crate::tools::executor::ToolInvocation,
        event_tx: Option<&mpsc::UnboundedSender<FromAgent>>,
        cancel: CancellationToken,
    ) -> ToolResult {
        self.execute_impl(
            &invocation.tool,
            &invocation.args,
            event_tx,
            &invocation.call_id,
            self.credential_generation(),
            ToolExecutionContext {
                code_decision: None,
                cancel: Some(cancel),
                approved_inline_env: None,
                hooks: None,
                emit_tool_events: false,
            },
        )
        .await
    }

    /// The working directory every tool in this executor runs in.
    #[must_use]
    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    /// Apply a native sandbox to subprocess-backed tools.
    pub fn with_sandbox_policy(mut self, policy: SandboxPolicy) -> Self {
        self.sandbox_policy = Some(policy);
        self.bash = self.build_bash_tool();
        self
    }

    /// Install the organization's MCP server policy.
    ///
    /// Every MCP client this executor builds, including one rebuilt after a
    /// configuration change, carries this policy. That is why the policy lives
    /// on the executor rather than on one client instance.
    #[must_use]
    pub fn with_managed_mcp_policy(mut self, policy: Option<crate::mcp::ManagedMcpPolicy>) -> Self {
        self.managed_mcp_policy = policy;
        self
    }

    #[cfg(test)]
    pub(crate) fn managed_mcp_policy_version_for_test(&self) -> Option<u64> {
        self.managed_mcp_policy
            .as_ref()
            .map(|policy| policy.version)
    }

    /// Mark this executor as running with no approval UI.
    ///
    /// Print/exec mode and other headless runs cannot collect a human
    /// approval, so tools that would otherwise escalate an unclassifiable
    /// input to the user must refuse it instead. Currently this makes the
    /// bash tool fail closed on a command its analyzer cannot parse.
    #[must_use]
    pub fn unattended(mut self) -> Self {
        self.unattended = true;
        self.bash = self.build_bash_tool();
        self
    }

    /// Whether this executor runs with no approval UI.
    #[must_use]
    pub fn is_unattended(&self) -> bool {
        self.unattended
    }

    /// Disable process-global safe-mode validators for callers that own a
    /// separate, bounded validation path.
    #[must_use]
    pub fn without_ambient_mutation_validators(mut self) -> Self {
        self.ambient_mutation_validators_enabled = false;
        self
    }

    /// Whether this registry may spawn a native language server.
    ///
    /// `NativeLspSession::start` launches `rust-analyzer` / `pyright` /
    /// `MAESTRO_LSP_COMMAND` via a bare `tokio::process::Command` that is
    /// never wrapped by Seatbelt or Landlock. Under any policy other than
    /// `DangerFullAccess` (or "no policy"), that child would escape the
    /// advertised containment — including the default network-enabled
    /// `WorkspaceWrite` and implicit diagnostics on `read`/`write`/`edit`.
    /// See the matching gate in `sandbox_policy_denial`.
    #[must_use]
    pub(crate) fn may_launch_native_language_server(&self) -> bool {
        match self.sandbox_policy.as_ref() {
            None | Some(SandboxPolicy::DangerFullAccess) => true,
            Some(_) => false,
        }
    }

    /// Build the bash tool honoring the pinned behavior version and sandbox.
    fn build_bash_tool(&self) -> BashTool {
        let tool = BashTool::new(&self.cwd).with_version(self.resolved_bash_version());
        let tool = if self.unattended {
            tool.unattended()
        } else {
            tool
        };
        match &self.sandbox_policy {
            Some(policy) => tool.with_sandbox_policy(policy.clone()),
            None => tool,
        }
    }

    /// The behavior version currently resolved for the bash tool.
    fn resolved_bash_version(&self) -> BashVersion {
        BashVersion::from_contract(Some(self.tool_versions.resolve("bash")))
    }

    /// Pin a version-managed tool to a specific behavior contract version.
    ///
    /// This is the registry hook for behavior version selection: session
    /// replay reads the version recorded in a session entry's receipt details
    /// (e.g. `BashDetails.version`) and pins it here so re-executed tool
    /// calls reproduce the recorded behavior. Errors if the tool is not
    /// version-managed or the version is unsupported — see
    /// `tools::versions` for the catalog.
    pub fn pin_tool_version(&mut self, tool_name: &str, version: &str) -> Result<(), String> {
        self.tool_versions.pin(tool_name, version)?;
        self.bash = self.build_bash_tool();
        Ok(())
    }

    /// Return a clone of the vault used by this executor.
    #[must_use]
    pub fn credential_vault(&self) -> CredentialVault {
        self.credential_vault.clone()
    }

    pub(crate) fn credential_generation(&self) -> u64 {
        self.credential_vault.generation()
    }

    /// Supply child construction while retaining the shared lifecycle owner.
    pub fn with_child_agent_factory(
        mut self,
        factory: Arc<dyn crate::subagents::factory::ChildAgentFactory>,
    ) -> Self {
        self.subagents = Arc::new((*self.subagents).clone().with_child_agent_factory(factory));
        self
    }

    pub(crate) fn with_subagent_parent_scope(mut self, parent_scope_id: String) -> Self {
        let factory = self.subagents.child_agent_factory();
        self.subagents = Arc::new(
            SubagentManager::with_parent_scope(PathBuf::from(&self.cwd), parent_scope_id)
                .with_child_agent_factory(factory),
        );
        self
    }

    pub(crate) fn with_mailbox_identity(mut self, identity: impl Into<String>) -> Self {
        self.mailbox_identity = identity.into();
        self
    }

    #[cfg(test)]
    pub(crate) fn mailbox_identity(&self) -> &str {
        &self.mailbox_identity
    }

    pub fn subagent_parent_scope_id(&self) -> String {
        self.subagents.parent_scope_id()
    }

    /// Point this executor's subagent scope at a different conversation.
    ///
    /// Takes `&self` because the executor is shared behind an `Arc` that
    /// outlives any one session; a new or resumed session rotates the scope
    /// rather than replacing the executor, which would drop its inline tools
    /// and MCP state.
    /// Point this executor's blocking tool waits at a steering signal.
    ///
    /// Without it `wait_subagent` sleeps until its own timeout and the user's
    /// message waits behind it.
    pub(crate) fn set_steer_signal(&self, signal: std::sync::Arc<crate::agent::SteerSignal>) {
        self.subagents.set_steer_signal(signal);
    }

    pub fn set_subagent_parent_model(&self, choice: crate::model_dynamics::ModelChoice) {
        self.subagents.set_parent_model(choice);
    }

    pub fn set_subagent_parent_scope(&self, parent_scope_id: String) {
        if self.subagents.parent_scope_id() != parent_scope_id {
            self.reset_coding_turn();
        }
        self.subagents.set_parent_scope_id(parent_scope_id);
    }

    /// Drain terminal child-agent notifications for this parent executor.
    pub fn poll_subagent_lifecycle_events(&self) -> Vec<SubagentLifecycleEvent> {
        self.subagents.poll_lifecycle_events()
    }

    pub fn acknowledge_subagent_lifecycle_event(
        &self,
        event: &SubagentLifecycleEvent,
    ) -> Result<(), String> {
        self.subagents.acknowledge_lifecycle_event(event)
    }

    pub(crate) fn set_subagent_parent_requests(&self, requests: Vec<String>) {
        self.subagents.set_parent_requests(requests);
    }

    pub fn worker_activity(&self) -> Result<(usize, usize), String> {
        self.subagents.worker_activity()
    }

    pub fn subagent_mailbox_recipients(&self) -> Vec<String> {
        self.subagents.active_mailbox_recipients()
    }

    pub fn inspect_subagent_control(
        &self,
        message_id: &str,
    ) -> Result<crate::mailbox::MailboxMessage, String> {
        self.subagents.inspect_control(message_id)
    }

    pub fn approve_held_subagent_control(
        &self,
        message_id: &str,
    ) -> Result<crate::mailbox::MailboxMessage, String> {
        self.subagents.approve_held_control(message_id)
    }

    pub fn cancel_subagent_by_id(&self, subagent_id: &str) -> Result<(), String> {
        let result = self
            .subagents
            .cancel_native(&serde_json::json!({"subagent_id": subagent_id}));
        if result.success {
            Ok(())
        } else {
            Err(result.output)
        }
    }

    /// Get the list of loaded inline tools
    ///
    /// Returns an iterator over the inline tool definitions.
    pub fn inline_tools(&self) -> impl Iterator<Item = &InlineTool> {
        self.inline_tools.values()
    }

    /// Get the count of loaded inline tools
    pub fn inline_tool_count(&self) -> usize {
        self.inline_tools.len()
    }

    /// Runtime tool definitions sent to model transports.
    pub fn tool_definitions(&self) -> impl Iterator<Item = &crate::agent::ToolDefinition> {
        self.registry.tools()
    }

    /// Look up the inline tool that this exact spelling will execute.
    ///
    /// Used to resolve the real command string (and its source config path)
    /// for the approval dialog, since an inline tool call's JSON arguments
    /// don't contain the command -- it lives in the tool's own definition.
    /// Exact built-in and MCP spellings dispatch before the inline fallback,
    /// so they must never inherit same-fold inline approval context.
    #[must_use]
    pub fn get_inline_tool(&self, name: &str) -> Option<&InlineTool> {
        if is_reserved_execute_dispatch_name(name) || McpClient::is_mcp_tool(name) {
            return None;
        }
        self.inline_tools.get(&name.to_lowercase())
    }

    /// Resolve the directory an inline tool will actually execute in.
    ///
    /// Approval rendering uses this same resolver as execution so an omitted
    /// `cwd` cannot hide the implicit workspace directory from the approver.
    #[must_use]
    pub fn inline_tool_effective_cwd(&self, tool: &InlineTool) -> String {
        self.inline_executor
            .effective_cwd(tool)
            .display()
            .to_string()
    }

    /// Resolve the environment context an approver must see before an inline
    /// tool executes, including inherited shell startup controls.
    #[must_use]
    pub fn inline_tool_effective_env(&self, tool: &InlineTool) -> HashMap<String, String> {
        self.inline_executor.effective_env_approval_context(tool)
    }

    /// Resolve the exact shell executable and flag used for inline tools.
    ///
    /// Approval rendering uses the same resolver as execution so inherited
    /// `SHELL`/`COMSPEC` values cannot hide the executable being approved.
    #[must_use]
    pub fn inline_tool_effective_shell(&self) -> (String, &'static str) {
        InlineToolExecutor::effective_shell()
    }

    /// Get cache statistics
    ///
    /// Returns statistics about cache performance including hit rate, entries, etc.
    pub fn cache_stats(&self) -> CacheStats {
        self.cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .stats()
    }

    /// Clear the tool result cache
    pub fn clear_cache(&self) {
        if let Ok(mut cache) = self.cache.write() {
            cache.clear();
        }
    }

    /// Replace a repeated full textual read with the delta from the last
    /// model-visible version. Partial, binary, image, PDF, and notebook reads
    /// preserve their existing behavior because they are views rather than a
    /// complete textual file snapshot.
    fn project_repeated_read(&self, tool_name: &str, args: &Value, result: &mut ToolResult) {
        if !tool_name.eq_ignore_ascii_case("read") || !result.success {
            return;
        }
        if args.get("offset").is_some()
            || args.get("limit").is_some()
            || args
                .get("mode")
                .and_then(Value::as_str)
                .is_some_and(|mode| mode != "normal")
            || args
                .get("asBase64")
                .or_else(|| args.get("as_base64"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        {
            return;
        }
        let Some(raw_path) = args
            .get("path")
            .or_else(|| args.get("file_path"))
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
        else {
            return;
        };
        let Ok(resolved_path) = resolve_tool_path(&self.cwd, raw_path) else {
            return;
        };
        let extension = Path::new(&resolved_path)
            .extension()
            .and_then(|value| value.to_str())
            .map(str::to_ascii_lowercase);
        if extension.as_deref().is_some_and(|extension| {
            matches!(
                extension,
                "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" | "pdf" | "ipynb"
            )
        }) {
            return;
        }

        let current = result.output.clone();
        let previous = self
            .read_snapshots
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(resolved_path, current.clone());
        let Some(previous) = previous else {
            return;
        };
        if previous == current {
            result.output = format!("Unchanged since previous read: {raw_path}");
            return;
        }

        let diff = similar::TextDiff::from_lines(&previous, &current)
            .unified_diff()
            .header("previous", "current")
            .to_string();
        result.output = format!("Diff since previous read: {raw_path}\n{diff}");
    }

    async fn ensure_mcp_client(&self) -> Result<Arc<McpClient>, String> {
        let _sync_guard = self.mcp_sync_lock.lock().await;
        let config = load_mcp_config_with_managed_connections(Some(Path::new(&self.cwd)));
        self.ensure_mcp_client_for_config(&config).await
    }

    /// Synchronize a client from one immutable configuration snapshot. The
    /// caller must hold `mcp_sync_lock`; replacing the client, rather than
    /// mutating it in place, keeps adapters created for the previous managed
    /// connection paired with that connection after an account switch.
    async fn ensure_mcp_client_for_config(
        &self,
        config: &McpConfig,
    ) -> Result<Arc<McpClient>, String> {
        if self
            .sandbox_policy
            .as_ref()
            .is_some_and(|policy| !policy.has_full_network_access())
        {
            return Err(
                "MCP blocked because the active sandbox policy disables network access".to_string(),
            );
        }
        let servers: Vec<_> = config.enabled_servers().cloned().collect();
        let desired_configs: HashMap<String, crate::mcp::McpServerConfig> = servers
            .iter()
            .cloned()
            .map(|server| (server.name.clone(), server))
            .collect();
        let previous_configs = self
            .mcp_synced_configs
            .read()
            .map(|configs| configs.clone())
            .unwrap_or_default();
        let mut last_errors = self
            .mcp_last_errors
            .read()
            .map(|errors| errors.clone())
            .unwrap_or_default();
        let mut last_attempts = self
            .mcp_last_connect_attempts
            .read()
            .map(|attempts| attempts.clone())
            .unwrap_or_default();
        let replace_client =
            previous_configs != desired_configs || self.mcp_client.lock().await.as_ref().is_none();
        let client = {
            if replace_client {
                let client = Arc::new(McpClient::new());
                client
                    .set_managed_policy(self.managed_mcp_policy.clone())
                    .await;
                client
            } else {
                let guard = self.mcp_client.lock().await;
                guard
                    .as_ref()
                    .map(Arc::clone)
                    .expect("MCP client exists when replacement is not required")
            }
        };

        if !replace_client {
            for (name, previous) in &previous_configs {
                if desired_configs.get(name) != Some(previous) {
                    let _ = client.disconnect(name).await;
                }
            }
        }

        let connected: std::collections::HashSet<_> =
            client.connected_servers().await.into_iter().collect();
        let now = Instant::now();

        // Project/local-scoped servers come from the repository and may be
        // hostile. Enforce the per-workspace trust approval for every
        // transport. Trust is read from global config only, so a repository
        // cannot grant itself trust.
        let workspace_trusted =
            crate::config::workspace_trusted_in_global_config(Path::new(&self.cwd));

        for server in &servers {
            let name = &server.name;
            if crate::mcp::server_requires_workspace_approval(server) && !workspace_trusted {
                if connected.contains(name) {
                    let _ = client.disconnect(name).await;
                }
                last_errors.insert(
                    name.clone(),
                    format!(
                        "MCP server \"{name}\" requires workspace trust approval; \
                         set projects.\"<workspace>\".trust_level = \"trusted\" in global \
                         config (~/.composer/config.toml) to enable it"
                    ),
                );
                continue;
            }

            let config_changed = replace_client || previous_configs.get(name) != Some(server);
            let is_connected = connected.contains(name);
            let retry_allowed = match last_attempts.get(name) {
                Some(last_attempt) => {
                    now.duration_since(*last_attempt) >= MCP_RECONNECT_RETRY_COOLDOWN
                }
                None => true,
            };

            if config_changed || (!is_connected && retry_allowed) {
                match client
                    .connect_with_workspace_trust(server.clone(), Some(Path::new(&self.cwd)))
                    .await
                {
                    Ok(()) => {
                        last_errors.remove(name);
                    }
                    Err(err) => {
                        let message = err.to_string();
                        if last_errors.get(name) != Some(&message) {
                            eprintln!(
                                "[mcp] Failed to connect to server {}: {}",
                                server.name, message
                            );
                        }
                        last_errors.insert(name.clone(), message);
                    }
                }
                last_attempts.insert(name.clone(), Instant::now());
            } else if is_connected {
                last_errors.remove(name);
            }
        }

        last_errors.retain(|name, _| desired_configs.contains_key(name));
        last_attempts.retain(|name, _| desired_configs.contains_key(name));

        if let Ok(mut map) = self.mcp_synced_configs.write() {
            *map = desired_configs;
        }
        if let Ok(mut map) = self.mcp_last_errors.write() {
            *map = last_errors;
        }
        if let Ok(mut map) = self.mcp_last_connect_attempts.write() {
            *map = last_attempts;
        }

        if replace_client {
            let mut guard = self.mcp_client.lock().await;
            *guard = Some(Arc::clone(&client));
        }

        let annotations = client.list_tool_annotations().await;
        if let Ok(mut map) = self.mcp_tool_annotations.write() {
            map.clear();
            for (name, meta) in annotations {
                map.insert(name.to_lowercase(), meta);
            }
        }

        // Publish the identity of every dispatchable MCP tool so the session
        // recorder can stamp each call with the definition it was issued
        // against. See `tools::tool_call_contract`.
        let live_tools = client.list_all_tools().await;
        if let Ok(mut identities) = self.mcp_permission_identities.write() {
            identities.clear();
            for tool in &live_tools {
                let server = servers.iter().find(|server| {
                    tool.name
                        .strip_prefix("mcp__")
                        .is_some_and(|rest| rest.starts_with(&format!("{}__", server.name)))
                });
                if let Some(identity) = server.and_then(|server| {
                    crate::mcp::permission_identity(server, &tool.name, &tool.input_schema)
                }) {
                    identities.insert(tool.name.to_lowercase(), identity);
                }
            }
        }
        crate::tools::tool_call_contract::publish_live_identities(
            live_tools
                .iter()
                .map(|tool| (tool.name.as_str(), Some(&tool.input_schema))),
        );

        Ok(client)
    }

    /// Whether a local remembered grant matches this exact admitted MCP tool.
    #[must_use]
    pub(crate) fn mcp_permission_allows(&self, tool_name: &str) -> bool {
        self.mcp_permission_identities
            .read()
            .ok()
            .and_then(|identities| identities.get(&tool_name.to_lowercase()).cloned())
            .is_some_and(|identity| crate::mcp::permission_is_allowed(&identity))
    }

    /// Remember approval for this exact local MCP transport + schema identity.
    pub fn remember_mcp_permission(&self, tool_name: &str, persistent: bool) -> Result<(), String> {
        let identity = self
            .mcp_permission_identities
            .read()
            .map_err(|_| "MCP permission identity lock poisoned".to_string())?
            .get(&tool_name.to_lowercase())
            .cloned()
            .ok_or_else(|| {
                "Only admitted local MCP tools can receive remembered permissions".to_string()
            })?;
        if persistent {
            crate::mcp::grant_persistent_permission(&identity).map_err(|error| error.to_string())
        } else {
            crate::mcp::grant_session_permission(&identity);
            Ok(())
        }
    }

    /// Identity of an MCP tool as the currently configured servers define it.
    ///
    /// `None` when no configured server offers that name any more.
    pub(crate) async fn live_mcp_tool_contract(
        &self,
        call_id: &str,
        tool_name: &str,
    ) -> Option<crate::tools::tool_call_contract::ToolCallContract> {
        let client = self.ensure_mcp_client().await.ok()?;
        client
            .list_all_tools()
            .await
            .into_iter()
            .find(|tool| tool.name == tool_name)
            .map(|tool| {
                crate::tools::tool_call_contract::ToolCallContract::record(
                    call_id,
                    tool_name,
                    Some(&tool.input_schema),
                )
            })
    }

    pub(crate) async fn ensure_orb_delegation_adapter(&self) -> Result<(), String> {
        // The managed connection authority is the source of truth for hosted
        // Orb bindings. Keep the owner and client on one immutable merged
        // snapshot, otherwise an account/connection switch between two
        // independent loads can pair owner A with client B.
        let _sync_guard = self.mcp_sync_lock.lock().await;
        let config = load_mcp_config_with_managed_connections(Some(Path::new(&self.cwd)));
        let snapshot = hosted_orb_connection_snapshot(&config)?;
        let client = self.ensure_mcp_client_for_config(&config).await?;
        let expected_server = snapshot.server.clone();
        let expected_owner = snapshot.owner_binding.clone();
        let cwd = self.cwd.clone();
        let operation_validator: OrbOperationValidator = Arc::new(move || {
            let current_config = load_mcp_config_with_managed_connections(Some(Path::new(&cwd)));
            let current_snapshot = hosted_orb_connection_snapshot(&current_config)?;
            if current_snapshot.server != expected_server {
                return Err(
                    "active managed Computer connection configuration changed; remote operation refused"
                        .to_string(),
                );
            }
            if current_snapshot.owner_binding != expected_owner {
                return Err(
                    "active account, workspace, or managed Computer connection owner changed; remote operation refused"
                        .to_string(),
                );
            }
            Ok(())
        });
        self.subagents
            .set_orb_adapter(OrbDelegationAdapter::from_mcp_client_with_validator(
                client,
                snapshot.server.name,
                snapshot.owner_binding,
                Some(operation_validator),
            ))
            .await;
        Ok(())
    }

    /// Execute a native hosted Computer operation through the same managed
    /// connection and durable subagent registry used by model tool calls.
    /// Provider setup failures are projected as `Unavailable` by the manager;
    /// durable local task identity is never discarded.
    pub async fn run_orb_console(&self, action: OrbConsoleAction) -> ToolResult {
        let provider_error = self.ensure_orb_delegation_adapter().await.err();
        self.subagents.orb_console(action, provider_error).await
    }

    /// Get MCP server status snapshots for UI display
    pub async fn mcp_status(&self) -> Result<Vec<McpServerStatus>, String> {
        let config = load_mcp_config_with_managed_connections(Some(Path::new(&self.cwd)));
        let connect_error = self.ensure_mcp_client().await.err();
        let client = self.mcp_client.lock().await.as_ref().cloned();

        let connected: std::collections::HashSet<_> = match &client {
            Some(client) => client.connected_servers().await.into_iter().collect(),
            None => std::collections::HashSet::new(),
        };
        let tools_map: HashMap<String, Vec<String>> = match &client {
            Some(client) => client.list_tools_by_server().await.into_iter().collect(),
            None => HashMap::new(),
        };
        let resources_map: HashMap<String, Vec<String>> = match &client {
            Some(client) => client.list_all_resources().await.into_iter().collect(),
            None => HashMap::new(),
        };
        let prompts_map: HashMap<String, Vec<String>> = match &client {
            Some(client) => client.list_all_prompts().await.into_iter().collect(),
            None => HashMap::new(),
        };
        let last_errors = self
            .mcp_last_errors
            .read()
            .map(|errors| errors.clone())
            .unwrap_or_default();

        let mut statuses = Vec::new();
        for server in &config.servers {
            let name = server.name.clone();
            let error = if server.is_enabled() {
                last_errors
                    .get(&name)
                    .cloned()
                    .or_else(|| connect_error.clone())
            } else {
                None
            };
            let state = mcp_lifecycle_state(
                server,
                connected.contains(&name),
                error.as_deref(),
                Path::new(&self.cwd),
            );
            let status = McpServerStatus {
                name: name.clone(),
                state,
                connected: connected.contains(&name),
                scope: server.scope,
                transport: server.transport,
                error,
                tools: tools_map.get(&name).cloned().unwrap_or_default(),
                disabled_tools: server.disabled_tools.clone(),
                resources: resources_map.get(&name).cloned().unwrap_or_default(),
                prompts: prompts_map.get(&name).cloned().unwrap_or_default(),
            };
            statuses.push(status);
        }

        for issue in config.issues {
            statuses.push(McpServerStatus {
                name: issue
                    .server
                    .unwrap_or_else(|| issue.path.display().to_string()),
                state: McpLifecycleState::ConfigError,
                connected: false,
                scope: issue.scope,
                transport: crate::mcp::McpTransport::Stdio,
                error: Some(issue.message),
                tools: Vec::new(),
                disabled_tools: Vec::new(),
                resources: Vec::new(),
                prompts: Vec::new(),
            });
        }
        statuses.sort_by(|left, right| left.name.cmp(&right.name));

        Ok(statuses)
    }

    /// Clear the reconnect cooldown and disconnect one cached server so the
    /// next background status refresh performs a fresh connection attempt.
    pub async fn retry_mcp_server(&self, name: &str) {
        if let Ok(mut attempts) = self.mcp_last_connect_attempts.write() {
            attempts.remove(name);
        }
        let client = self.mcp_client.lock().await.as_ref().cloned();
        if let Some(client) = client {
            let _ = client.disconnect(name).await;
        }
    }

    /// Get detailed MCP prompt metadata for connected servers
    pub async fn mcp_prompt_details(
        &self,
        server_filter: Option<&str>,
    ) -> Result<Vec<(String, Vec<McpPrompt>)>, String> {
        let client = self.ensure_mcp_client().await?;
        let mut prompts = client.list_all_prompt_details().await;
        if let Some(filter) = server_filter {
            prompts.retain(|(name, _)| name == filter);
        }

        Ok(prompts
            .into_iter()
            .filter(|(_, prompt_entries)| !prompt_entries.is_empty())
            .collect())
    }

    /// Read an MCP resource via the configured client
    pub async fn mcp_read_resource(
        &self,
        server: &str,
        uri: &str,
    ) -> Result<crate::mcp::protocol::ResourceReadResult, String> {
        let client = self.ensure_mcp_client().await?;
        client
            .read_resource(server, uri)
            .await
            .map_err(|err| err.to_string())
    }

    /// Get an MCP prompt via the configured client
    pub async fn mcp_get_prompt(
        &self,
        server: &str,
        name: &str,
        arguments: Option<HashMap<String, String>>,
    ) -> Result<crate::mcp::PromptGetResult, String> {
        let client = self.ensure_mcp_client().await?;
        client
            .get_prompt(server, name, arguments)
            .await
            .map_err(|err| err.to_string())
    }

    /// Invalidate cache entries for a specific file path
    ///
    /// Called when files are modified to ensure stale data isn't returned.
    fn invalidate_file_cache(&self, path: &str) {
        if let Ok(mut cache) = self.cache.write() {
            // Invalidate only reads that recorded this file (or a containing
            // directory) as a dependency. Unrelated cached reads survive a
            // write elsewhere in the workspace.
            cache.invalidate_for_file(Path::new(path));
        }
    }

    /// Check if a tool exists in the registry
    ///
    /// Performs case-insensitive lookup. Returns true if the tool is registered,
    /// false otherwise.
    ///
    /// # Examples
    ///
    /// ```
    /// use maestro_local_host::tools::ToolExecutor;
    ///
    /// let executor = ToolExecutor::new(".");
    /// assert!(executor.has_tool("bash"));
    /// assert!(executor.has_tool("Bash"));  // Case-insensitive
    /// assert!(!executor.has_tool("nonexistent"));
    /// ```
    pub fn has_tool(&self, name: &str) -> bool {
        self.registry.get(name).is_some()
    }

    /// Ensure MCP client is initialized and annotations are cached.
    /// Call this before checking annotations for MCP tools.
    pub async fn ensure_mcp_annotations(&self) -> Result<(), String> {
        let _ = self.ensure_mcp_client().await?;
        self.poll_mcp_updates().await?;
        Ok(())
    }

    /// Drain live MCP notifications and refresh cached metadata when server lists change.
    pub async fn poll_mcp_updates(&self) -> Result<Vec<crate::mcp::McpRuntimeEvent>, String> {
        // Connection reconciliation runs on the background status worker.
        // The UI loop only drains already-connected notification queues here;
        // it must never dial or respawn a server while handling input.
        let client = {
            let guard = self.mcp_client.lock().await;
            guard.as_ref().cloned()
        };
        let Some(client) = client else {
            return Ok(Vec::new());
        };

        let events = client
            .poll_notifications()
            .await
            .map_err(|err| err.to_string())?;

        if events
            .iter()
            .any(crate::mcp::McpRuntimeEvent::changes_tools)
        {
            let annotations = client.list_tool_annotations().await;
            if let Ok(mut map) = self.mcp_tool_annotations.write() {
                map.clear();
                for (name, meta) in annotations {
                    map.insert(name.to_lowercase(), meta);
                }
            }
        }

        Ok(events)
    }

    /// Get MCP tool annotations if available
    pub fn tool_annotations(&self, name: &str) -> Option<crate::mcp::McpToolAnnotations> {
        let key = name.to_lowercase();
        self.mcp_tool_annotations
            .read()
            .ok()
            .and_then(|map| map.get(&key).cloned())
    }

    /// Return missing required fields for a tool given its arguments
    ///
    /// Validates the provided arguments against the tool's JSON schema and returns
    /// a list of required field names that are missing or empty.
    ///
    /// # Arguments
    ///
    /// - `name`: Tool name (case-insensitive)
    /// - `args`: JSON object containing the tool arguments
    ///
    /// # Returns
    ///
    /// Vector of missing field names. Empty vector if all required fields are present.
    ///
    /// # Examples
    ///
    /// ```
    /// use maestro_local_host::tools::ToolExecutor;
    /// use serde_json::json;
    ///
    /// let executor = ToolExecutor::new(".");
    ///
    /// // Missing required field
    /// let args = json!({});
    /// let missing = executor.missing_required("bash", &args);
    /// assert_eq!(missing, vec!["command"]);
    ///
    /// // All required fields present
    /// let args = json!({"command": "ls"});
    /// let missing = executor.missing_required("bash", &args);
    /// assert!(missing.is_empty());
    /// ```
    pub fn missing_required(&self, name: &str, args: &serde_json::Value) -> Vec<String> {
        let registry_name = self
            .get_inline_tool(name)
            .map_or(name, |tool| tool.definition.name.as_str());
        self.registry.missing_required(registry_name, args)
    }

    /// Check whether a tool requires user approval given its arguments
    ///
    /// This method consults both static and dynamic approval logic:
    /// - Static approval: Set per-tool in the registry (e.g., write always needs approval)
    /// - Dynamic approval: Computed based on arguments (e.g., bash inspects the command)
    ///
    /// # Arguments
    ///
    /// - `name`: Tool name (case-insensitive)
    /// - `args`: JSON object containing the tool arguments
    ///
    /// # Returns
    ///
    /// True if the tool requires user approval, false if it can execute automatically.
    /// Unknown tools default to requiring approval.
    ///
    /// # Examples
    ///
    /// ```
    /// use maestro_local_host::tools::ToolExecutor;
    /// use serde_json::json;
    ///
    /// let executor = ToolExecutor::new(".");
    ///
    /// // Read is safe - no approval needed
    /// let args = json!({"file_path": "/tmp/test.txt"});
    /// assert!(!executor.requires_approval("read", &args));
    ///
    /// // Write always needs approval
    /// let args = json!({"file_path": "/tmp/test.txt", "content": "hello"});
    /// assert!(executor.requires_approval("write", &args));
    ///
    /// // Bash approval is dynamic based on command
    /// let safe_cmd = json!({"command": "ls -la"});
    /// assert!(!executor.requires_approval("bash", &safe_cmd));
    ///
    /// let unsafe_cmd = json!({"command": "cargo build"});
    /// assert!(executor.requires_approval("bash", &unsafe_cmd));
    /// ```
    pub fn requires_approval(&self, name: &str, args: &serde_json::Value) -> bool {
        // This check runs before the allowlist below on purpose.
        if self.requires_sandbox_bypass_approval(name, args) {
            return true;
        }

        // Version-managed tools classify approval with their pinned behavior
        // version so replayed sessions reproduce the recorded decisions.
        if matches!(name, "bash" | "Bash") {
            if let Some(command) = args.get("command").and_then(|v| v.as_str()) {
                return self.resolved_bash_version().requires_approval(command);
            }
        }
        let registry_name = self
            .get_inline_tool(name)
            .map_or(name, |tool| tool.definition.name.as_str());
        self.registry.requires_approval(registry_name, args)
    }

    /// Whether this tool call is asking to run a command outside the native
    /// sandbox while a sandbox policy is active.
    ///
    /// A request to bypass the native sandbox always requires human
    /// approval, regardless of the command allowlist: this is the
    /// per-command sandbox escape hatch, and it must be a thing the user is
    /// asked about, never something that silently slips through because the
    /// command text happens to look safe. This is also the check approval
    /// gates use when they would otherwise auto-approve unconditionally
    /// (e.g. `ApprovalMode::Yolo`), so a bypass request can never be
    /// auto-approved on any surface.
    #[must_use]
    pub fn requires_sandbox_bypass_approval(&self, name: &str, args: &serde_json::Value) -> bool {
        name.eq_ignore_ascii_case("bash")
            && self.sandbox_policy.is_some()
            && args
                .get("bypass_sandbox")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
    }

    /// Check a tool call against the action firewall.
    pub fn firewall_verdict(&self, name: &str, args: &serde_json::Value) -> FirewallVerdict {
        let firewall = ActionFirewall::new(&self.cwd);
        if self.get_inline_tool(name).is_some() {
            // Preserve a non-built-in spelling so a same-fold inline name
            // cannot inherit the built-in tool's argument policy.
            firewall.check_tool(name, args)
        } else {
            firewall.check_tool(&name.to_lowercase(), args)
        }
    }

    fn owns_cancellation_cleanup(&self, tool_name: &str) -> bool {
        let tool_key = tool_name.to_lowercase();
        tool_name.eq_ignore_ascii_case("bash")
            || McpClient::is_mcp_tool(tool_name)
            || self.inline_tools.contains_key(&tool_key)
            || matches!(
                tool_key.as_str(),
                "write"
                    | "edit"
                    | "notebook_edit"
                    | "todo"
                    | "extract_document"
                    | "spawn_subagent"
                    | "resume_subagent"
                    | "control_subagent"
            )
    }

    fn sandbox_policy_denial(&self, name: &str, args: &serde_json::Value) -> Option<String> {
        let policy = self.sandbox_policy.as_ref()?;
        let tool = name.to_ascii_lowercase();

        let uses_mcp_transport = tool.starts_with("mcp_") || McpClient::is_mcp_tool(name);
        if !policy.has_full_network_access()
            && (uses_mcp_transport
                || matches!(
                    tool.as_str(),
                    "web_fetch"
                        | "websearch"
                        | "codesearch"
                        | "extract_document"
                        | "gh_pr"
                        | "gh_issue"
                        | "gh_repo"
                ))
        {
            return Some(format!(
                "Tool '{name}' blocked because the active sandbox policy disables network access"
            ));
        }

        if !matches!(policy, SandboxPolicy::DangerFullAccess) {
            let action = args
                .get("action")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let unsandboxed_git_mutation = (tool == "gh_pr" && action == "checkout")
                || (tool == "gh_repo" && action == "clone");
            if unsandboxed_git_mutation {
                return Some(format!(
                    "Tool '{name}' blocked because its git mutation is not contained by the active sandbox policy"
                ));
            }
        }

        // The VS Code/JetBrains diagnostics/definition/references tools
        // launch a language server (rust-analyzer, pyright, or
        // MAESTRO_LSP_COMMAND) via `NativeLspSession::start` -- a bare
        // `tokio::process::Command` that the OS-level sandbox never
        // contains, with full write and network access. The same escape
        // applies under default WorkspaceWrite (network on) and under
        // ReadOnly: the child is never Seatbelt/Landlock-wrapped. Block
        // every non-DangerFullAccess policy; implicit diagnostics on
        // read/write/edit are gated the same way via
        // `ToolRegistry::may_launch_native_language_server`.
        let launches_native_language_server = matches!(
            tool.as_str(),
            "vscode_get_diagnostics"
                | "jetbrains_get_diagnostics"
                | "vscode_get_definition"
                | "jetbrains_get_definition"
                | "vscode_find_references"
                | "jetbrains_find_references"
        );
        if launches_native_language_server && !matches!(policy, SandboxPolicy::DangerFullAccess) {
            return Some(format!(
                "Tool '{name}' blocked because it launches a language server outside the active sandbox policy"
            ));
        }

        if matches!(policy, SandboxPolicy::ReadOnly) {
            let action = args
                .get("action")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let mutates_files = matches!(tool.as_str(), "write" | "edit" | "notebook_edit")
                || (tool == "background_tasks" && action == "start")
                || tool == "extract_document"
                // `todo` persists to ~/.composer/todos.json (or
                // MAESTRO_TODO_FILE) via tools/todo.rs::save_store.
                || tool == "todo"
                // `screenshot` launches an unsandboxed capture program that
                // writes a temp PNG (tools/image.rs).
                || tool == "screenshot";
            if mutates_files {
                return Some(format!(
                    "Tool '{name}' blocked by the active read-only sandbox policy"
                ));
            }
        }

        None
    }

    /// Execute a tool by name with the given arguments
    ///
    /// This is the main entry point for tool execution. It dispatches to the appropriate
    /// tool implementation, manages event streams, and returns a result.
    ///
    /// # Process Flow
    ///
    /// 1. Match on tool name (case-insensitive)
    /// 2. Deserialize JSON args to tool-specific argument struct
    /// 3. Send `ToolStart` event (if `event_tx` provided)
    /// 4. Execute tool implementation
    /// 5. Send `ToolOutput` event for any output (if `event_tx` provided)
    /// 6. Send `ToolEnd` event with success status (if `event_tx` provided)
    /// 7. Return `ToolResult`
    ///
    /// # Arguments
    ///
    /// - `tool_name`: Name of the tool to execute (e.g., "bash", "read")
    /// - `args`: JSON object containing tool arguments
    /// - `event_tx`: Optional channel for streaming progress events to the UI
    /// - `call_id`: Unique identifier for this tool call (used in events)
    ///
    /// # Returns
    ///
    /// A `ToolResult` containing:
    /// - `success`: Whether the tool executed successfully
    /// - `output`: Tool output (stdout, file contents, etc.)
    /// - `error`: Optional error message if success is false
    ///
    /// # Event Streaming
    ///
    /// If `event_tx` is provided, the executor sends events for real-time updates:
    /// - **`ToolStart`**: Sent before execution begins
    /// - **`ToolOutput`**: Sent when output is available (may be sent multiple times)
    /// - **`ToolEnd`**: Sent after execution completes
    ///
    /// # Error Handling
    ///
    /// Errors are never panicked. Instead, they are returned in the `ToolResult`:
    /// - Invalid arguments: Deserialization errors
    /// - Tool errors: File not found, permission denied, etc.
    /// - Unknown tool: Tool name not found in registry
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use maestro_local_host::tools::ToolExecutor;
    /// use serde_json::json;
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let executor = ToolExecutor::new("/workspace");
    ///
    /// // Execute without event streaming
    /// let args = json!({"command": "git status"});
    /// let result = executor.execute("bash", &args, None, "call-1").await;
    ///
    /// if result.success {
    ///     println!("Output: {}", result.output);
    /// } else {
    ///     eprintln!("Error: {:?}", result.error);
    /// }
    ///
    /// // Execute with event streaming
    /// use tokio::sync::mpsc;
    /// use maestro_local_host::agent::FromAgent;
    ///
    /// let (tx, mut rx) = mpsc::unbounded_channel();
    /// let result = executor.execute("read", &json!({"file_path": "Cargo.toml"}), Some(&tx), "call-2").await;
    ///
    /// // Process events from rx
    /// while let Some(event) = rx.recv().await {
    ///     match event {
    ///         FromAgent::ToolStart { call_id } => println!("Tool started: {}", call_id),
    ///         FromAgent::ToolOutput { content, .. } => println!("Output: {}", content),
    ///         FromAgent::ToolEnd { success, .. } => println!("Done: {}", success),
    ///         _ => {}
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn execute(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        event_tx: Option<&mpsc::UnboundedSender<FromAgent>>,
        call_id: &str,
    ) -> ToolResult {
        self.execute_at_generation(
            tool_name,
            args,
            event_tx,
            call_id,
            self.credential_generation(),
            None,
        )
        .await
    }

    pub(crate) async fn execute_at_generation(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        event_tx: Option<&mpsc::UnboundedSender<FromAgent>>,
        call_id: &str,
        generation: u64,
        cancel: Option<CancellationToken>,
    ) -> ToolResult {
        self.execute_at_generation_with_inline_env(
            tool_name,
            args,
            event_tx,
            call_id,
            generation,
            ToolExecutionContext {
                code_decision: None,
                cancel,
                approved_inline_env: None,
                hooks: None,
                emit_tool_events: true,
            },
        )
        .await
    }

    async fn execute_at_generation_with_inline_env(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        event_tx: Option<&mpsc::UnboundedSender<FromAgent>>,
        call_id: &str,
        generation: u64,
        execution_context: ToolExecutionContext<'_>,
    ) -> ToolResult {
        let mut execution_context = execution_context;
        if execution_context.code_decision.is_none() {
            execution_context.code_decision = match self
                .authorize_code_call(
                    tool_name,
                    args,
                    call_id,
                    generation,
                    execution_context.approved_inline_env,
                )
                .await
            {
                Ok(decision) => decision,
                Err(error) => {
                    return ToolResult::failure(format!(
                        "Code tool authority denied execution: {error}"
                    ));
                }
            };
        }
        if execution_context
            .code_decision
            .as_ref()
            .is_some_and(|decision| !decision.is_current())
        {
            return ToolResult::failure("Code tool authority expired before dispatch");
        }
        let emit_tool_events = execution_context.emit_tool_events;
        if let Some(message) = self.sandbox_policy_denial(tool_name, args) {
            return ToolResult::failure(message);
        }
        if self.sandbox_policy.is_some() && self.get_inline_tool(tool_name).is_some() {
            return ToolResult::failure("Inline shell tools are disabled for sandboxed exec runs");
        }
        if let FirewallVerdict::Block { reason } = self.firewall_verdict(tool_name, args) {
            return ToolResult::failure(format!("Blocked by action firewall: {reason}"));
        }

        // Check cache for cacheable tools
        let cache_key = CacheKey::for_generation(tool_name, args, generation);
        let is_cacheable = self
            .cache
            .read()
            .map(|c| c.is_cacheable(tool_name))
            .unwrap_or(false);

        if !self.owns_cancellation_cleanup(tool_name)
            && execution_context
                .cancel
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
        {
            let result = ToolResult::failure(format!("{tool_name} cancelled"))
                .with_details(serde_json::json!({"cancelled": true}));
            if emit_tool_events {
                if let Some(tx) = event_tx {
                    let _ = tx.send(FromAgent::ToolEnd {
                        call_id: call_id.to_string(),
                        success: false,
                        result: Some(result.clone()),
                        receipt: None,
                    });
                }
            }
            return result;
        }

        if is_cacheable {
            if let Ok(mut cache) = self.cache.write() {
                if let Some(cached) = cache.get(&cache_key) {
                    // Cache hit for tool execution
                    let mut result = ToolResult {
                        success: !cached.is_error,
                        output: cached.output.clone(),
                        error: if cached.is_error {
                            Some(cached.output.clone())
                        } else {
                            None
                        },
                        details: None,
                    };

                    self.project_repeated_read(tool_name, args, &mut result);
                    self.append_directory_skill_catalog(tool_name, args, generation, &mut result);

                    // Send events for cached result
                    if emit_tool_events {
                        if let Some(tx) = event_tx {
                            let _ = tx.send(FromAgent::ToolStart {
                                call_id: call_id.to_string(),
                            });
                            if !result.output.is_empty() {
                                let _ = tx.send(FromAgent::ToolOutput {
                                    call_id: call_id.to_string(),
                                    content: result.output.clone(),
                                });
                            }
                            let _ = tx.send(FromAgent::ToolEnd {
                                call_id: call_id.to_string(),
                                success: !cached.is_error,
                                result: Some(result.clone()),
                                receipt: None,
                            });
                        }
                    }

                    return result;
                }
            }
        }

        // Process-owning tools must run their kill-and-reap paths after
        // cancellation. Transactional file mutations must also finish so a
        // dropped future cannot strand a target between backup and publish.
        // Other tools can safely race their whole execution against the
        // per-call token.
        let cancellation = execution_context.cancel.clone();
        let isolated = self.isolated_dispatch_for(tool_name);
        // An isolated executor owns its own kill path, so the outer select
        // below must not abandon the call and leave the child running.
        let owns_cancellation_cleanup =
            isolated.is_some() || self.owns_cancellation_cleanup(tool_name);
        let execution = Box::pin(async {
            match isolated {
                Some(dispatch) => {
                    let invocation = self.isolated_invocation(
                        tool_name,
                        args,
                        call_id,
                        execution_context.approved_inline_env,
                    );
                    let sink = event_tx
                        .cloned()
                        .map(crate::tools::executor::OutputSink::new);
                    let token = execution_context.cancel.clone().unwrap_or_default();
                    dispatch.execute(invocation, token, sink).await
                }
                None => {
                    self.execute_impl(
                        tool_name,
                        args,
                        event_tx,
                        call_id,
                        generation,
                        execution_context,
                    )
                    .await
                }
            }
        });
        let (uncached_result, synthetically_cancelled) = match cancellation {
            Some(token) if !owns_cancellation_cleanup => {
                tokio::select! {
                    biased;
                    () = token.cancelled() => {
                        let result = ToolResult::failure(format!("{tool_name} cancelled"))
                            .with_details(serde_json::json!({"cancelled": true}));
                        if emit_tool_events {
                            if let Some(tx) = event_tx {
                            let _ = tx.send(FromAgent::ToolEnd {
                                call_id: call_id.to_string(),
                                success: false,
                                result: Some(result.clone()),
                                receipt: None,
                            });
                            }
                        }
                        (result, true)
                    }
                    result = execution => (result, false),
                }
            }
            _ => (execution.await, false),
        };

        // Execute the tool
        let mut result =
            vault_tool_result_credentials(&self.credential_vault, generation, uncached_result);

        // Store result in cache for cacheable tools
        if is_cacheable && !synthetically_cancelled && !result.is_cancelled() {
            if let Ok(mut cache) = self.cache.write() {
                let cached_result = CachedResult::new(
                    if result.success {
                        &result.output
                    } else {
                        result.error.as_deref().unwrap_or("")
                    },
                    !result.success,
                );
                let dependencies = cache_dependency_paths(&self.cwd, tool_name, args);
                if dependencies.is_empty() {
                    cache.put(cache_key, cached_result);
                } else {
                    cache.put_with_deps(cache_key, cached_result, dependencies);
                }
                // Stored result in cache
            }
        }

        // Project only the cached/raw file contents. Derived catalogs are added
        // afterwards so trust changes cannot leak a prior catalog through a diff.
        self.project_repeated_read(tool_name, args, &mut result);
        self.append_directory_skill_catalog(tool_name, args, generation, &mut result);
        result
    }

    fn append_directory_skill_catalog(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        generation: u64,
        result: &mut ToolResult,
    ) {
        if !result.success
            || args
                .get("asBase64")
                .or_else(|| args.get("as_base64"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
            || self.isolated_dispatch_for(tool_name).is_some()
            || !matches!(tool_name, "read" | "Read" | "list" | "List" | "ls")
        {
            return;
        }
        let raw_path = args
            .get("path")
            .or_else(|| args.get("file_path"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or(&self.cwd);
        let path = Path::new(&self.cwd).join(raw_path);
        let catalog = crate::skills::directory::catalog(Path::new(&self.cwd), &path);
        result.output.push_str(
            &self
                .credential_vault
                .vault_in_text_at_generation(generation, &catalog),
        );
    }

    /// Execute a tool and convert the legacy transport DTO into the typed internal outcome.
    ///
    /// The existing `execute` API remains for headless and control-plane clients which still
    /// exchange the legacy `{ success, output, error, details }` schema.
    pub async fn execute_with_receipt(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        event_tx: Option<&mpsc::UnboundedSender<FromAgent>>,
        call_id: &str,
    ) -> ToolExecution {
        self.execute_with_receipt_at_generation(
            tool_name,
            args,
            event_tx,
            call_id,
            self.credential_generation(),
            None,
        )
        .await
    }

    /// Execute a tool like [`Self::execute_with_receipt`], aborting early when
    /// `cancel` fires (the TUI wires this to Ctrl+C so long-running commands
    /// are interrupted instead of blocking until their timeout).
    pub async fn execute_with_receipt_cancellable(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        event_tx: Option<&mpsc::UnboundedSender<FromAgent>>,
        call_id: &str,
        cancel: CancellationToken,
    ) -> ToolExecution {
        self.execute_with_receipt_at_generation(
            tool_name,
            args,
            event_tx,
            call_id,
            self.credential_generation(),
            Some(cancel),
        )
        .await
    }

    pub(crate) async fn execute_with_receipt_cancellable_inline_env(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        event_tx: Option<&mpsc::UnboundedSender<FromAgent>>,
        call_id: &str,
        options: ToolExecutionOptions<'_>,
    ) -> ToolExecution {
        self.execute_with_receipt_at_generation_with_inline_env(
            tool_name,
            args,
            event_tx,
            call_id,
            self.credential_generation(),
            ToolExecutionContext {
                code_decision: None,
                cancel: Some(options.cancel),
                approved_inline_env: options.approved_inline_env,
                hooks: options.hooks,
                emit_tool_events: false,
            },
        )
        .await
    }

    pub(crate) async fn execute_with_receipt_at_generation(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        event_tx: Option<&mpsc::UnboundedSender<FromAgent>>,
        call_id: &str,
        generation: u64,
        cancel: Option<CancellationToken>,
    ) -> ToolExecution {
        self.execute_with_receipt_at_generation_with_inline_env(
            tool_name,
            args,
            event_tx,
            call_id,
            generation,
            ToolExecutionContext {
                code_decision: None,
                cancel,
                approved_inline_env: None,
                hooks: None,
                emit_tool_events: false,
            },
        )
        .await
    }

    async fn execute_with_receipt_at_generation_with_inline_env(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        event_tx: Option<&mpsc::UnboundedSender<FromAgent>>,
        call_id: &str,
        generation: u64,
        execution_context: ToolExecutionContext<'_>,
    ) -> ToolExecution {
        let code_decision = match self
            .authorize_code_call(
                tool_name,
                args,
                call_id,
                generation,
                execution_context.approved_inline_env,
            )
            .await
        {
            Ok(decision) => decision,
            Err(error) => {
                let execution = ToolExecution::denied(
                    call_id,
                    tool_name,
                    DenialReason::IdentityAuthority {
                        message: format!("Code tool authority denied execution: {error}"),
                    },
                )
                .with_managed_policy(crate::safety::managed_policy_metadata());
                emit_typed_tool_end(event_tx, call_id, &execution);
                return execution;
            }
        };
        let mut execution_context = execution_context;
        execution_context.code_decision = code_decision.clone();
        if let Some(message) = self.sandbox_policy_denial(tool_name, args) {
            let execution =
                ToolExecution::denied(call_id, tool_name, DenialReason::SandboxPolicy { message })
                    .with_managed_policy(crate::safety::managed_policy_metadata());
            emit_typed_tool_end(event_tx, call_id, &execution);
            return execution;
        }
        if self.sandbox_policy.is_some() && self.get_inline_tool(tool_name).is_some() {
            let execution = ToolExecution::denied(
                call_id,
                tool_name,
                DenialReason::SandboxPolicy {
                    message: "Inline shell tools are disabled for sandboxed exec runs".to_string(),
                },
            )
            .with_managed_policy(crate::safety::managed_policy_metadata());
            emit_typed_tool_end(event_tx, call_id, &execution);
            return execution;
        }
        if let FirewallVerdict::Block { reason } = self.firewall_verdict(tool_name, args) {
            let execution = ToolExecution::denied(
                call_id,
                tool_name,
                DenialReason::ActionFirewall {
                    message: format!("Blocked by action firewall: {reason}"),
                },
            )
            .with_managed_policy(crate::safety::managed_policy_metadata());
            emit_typed_tool_end(event_tx, call_id, &execution);
            return execution;
        }

        let started = Instant::now();
        let cache_key = CacheKey::for_generation(tool_name, args, generation);
        let cache_hit =
            self.cache.read().ok().is_some_and(|cache| {
                cache.is_cacheable(tool_name) && cache.contains_fresh(&cache_key)
            });
        if let Some(tx) = event_tx {
            let _ = tx.send(FromAgent::ToolStart {
                call_id: call_id.to_string(),
            });
        }
        // Execute without event forwarding so the receipt-bearing ToolEnd below is the
        // sole terminal event for this call.
        let result = self
            .execute_at_generation_with_inline_env(
                tool_name,
                args,
                event_tx,
                call_id,
                generation,
                execution_context,
            )
            .await;
        let used_cache = cache_hit && !result.is_cancelled();
        let mut execution = ToolExecution::from_legacy(
            call_id,
            tool_name,
            if used_cache {
                ExecutionSource::Cache
            } else {
                ExecutionSource::Native
            },
            result,
        )
        .with_duration(started.elapsed().as_millis() as u64)
        .with_managed_policy(crate::safety::managed_policy_metadata());
        execution.receipt.code_authority = code_decision.map(Box::new);
        if used_cache {
            execution.receipt.details = crate::agent::ToolReceiptDetails::Cached;
        }
        emit_typed_tool_end(event_tx, call_id, &execution);
        execution
    }
}

fn emit_typed_tool_end(
    event_tx: Option<&mpsc::UnboundedSender<FromAgent>>,
    call_id: &str,
    execution: &ToolExecution,
) {
    let Some(tx) = event_tx else {
        return;
    };

    let result = execution.to_legacy();
    let output_was_streamed = result
        .details
        .as_ref()
        .and_then(|details| details.get("streamed"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !output_was_streamed && !result.output.is_empty() {
        let _ = tx.send(FromAgent::ToolOutput {
            call_id: call_id.to_string(),
            content: result.output.clone(),
        });
    }
    let _ = tx.send(FromAgent::ToolEnd {
        call_id: call_id.to_string(),
        success: result.success,
        result: Some(result),
        receipt: Some(execution.receipt.clone()),
    });
}

mod coding_task;
mod execute;
mod tool_registry;
pub use tool_registry::ToolRegistry;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod token_efficiency_tests;
