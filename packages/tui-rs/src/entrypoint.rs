//! # Deixic Code TUI - Native Terminal Interface
//!
//! This is the main entry point for the Deixic Code native TUI application.
//! It's a pure Rust implementation with native AI provider integrations.
//!
//! ## Rust Concept: Doc Comments
//! Lines starting with `//!` are "inner doc comments" that document the
//! containing item (in this case, the entire module/file). They appear
//! in generated documentation via `cargo doc`.
//!
//! ## Usage
//!
//! ```bash
//! deixic-code [options] [prompt]
//! ```

// ─────────────────────────────────────────────────────────────────────────────
// IMPORTS
// ─────────────────────────────────────────────────────────────────────────────
//
// Rust Concept: The `use` keyword brings items into scope.
// Unlike JavaScript/TypeScript imports, Rust uses a module system where
// crates (packages) are declared in Cargo.toml, and we import specific
// items from them.

use anyhow::{Context, Result};
// `anyhow::Result` is a convenient error type that can hold any error.
// It's shorthand for `Result<T, anyhow::Error>` and is great for applications
// (as opposed to libraries) because it simplifies error handling.

use clap::{Parser, error::ErrorKind};
// `clap` is the standard CLI argument parsing library in Rust.
// The `Parser` trait enables derive macros to auto-generate argument parsing.

use crate::App;
// Import our main `App` struct from the library crate.
// In Rust, a package can have both a binary (main.rs) and a library (lib.rs).
// This imports from lib.rs.

use crate::tools::cleanup_background_processes;
// Import the process cleanup function for signal handlers.

use crate::hosted_runner_cli::run_hosted_runner_cli_from_env;
use crate::sandbox::SandboxPolicy;

/// SIGINT/SIGTERM/SIGHUP (Unix) / console-event (Windows) handling for the
/// interactive path: flushes the session writer, cleans up tracked
/// background processes, and restores the terminal on an externally
/// delivered shutdown signal. See its module docs for the full design.
mod shutdown_signal;

// ─────────────────────────────────────────────────────────────────────────────
// HELPER FUNCTIONS
// ─────────────────────────────────────────────────────────────────────────────

/// The canonical set of native utility subcommands. This is the single
/// source of truth for which first-level argv tokens dispatch to the
/// utility handler instead of the interactive TUI, headless server, or
/// exec/print bridges; `packages/maestro-rs` no longer keeps an independent
/// copy of this list (see `maestro::cli::classify`).
pub const NATIVE_UTILITY_COMMANDS: [&str; 45] = [
    "acp",
    "sessions",
    "search",
    "cost",
    "stats",
    "models",
    "status",
    "hooks",
    "export",
    "import",
    "import-claude",
    "skill",
    "update",
    "modes",
    "agents",
    "specialists",
    "painter",
    "anthropic",
    "memory",
    "mcp",
    "init",
    "login",
    "openai",
    "computer",
    "orb",
    "config",
    "experiments",
    "stealth-models",
    "mission",
    "evalops",
    "operating-plane",
    "remote",
    "value",
    "scenario",
    "codex",
    "context",
    "run",
    "workflow",
    "a2a",
    "plugins",
    "plugin",
    "connections",
    "doctor",
    "diagnostics",
    "setup",
];

const GLOBAL_FLAGS_WITH_VALUES: [&str; 28] = [
    "--mode",
    "--provider",
    "--specialist",
    "--model",
    "-m",
    "--task-budget",
    "--models",
    "--models-file",
    "--api-key",
    "--port",
    "--system-prompt",
    "--append-system-prompt",
    "--session",
    "--approval-mode",
    "--auth",
    "--sandbox",
    "--output-schema",
    "--output-last-message",
    "--tools",
    "--composer",
    "--format",
    "--output-dir",
    "--profile",
    "--config",
    "--junit",
    "--replay",
    "--record-scenario",
    "--plugin",
];

/// Recognize a utility-subcommand invocation and reconstruct its forwarded
/// tokens. Returns `None` when `raw_args` should instead reach the
/// interactive TUI, headless server, or exec/print bridge.
pub fn native_utility_tokens(raw_args: &[std::ffi::OsString]) -> Option<Vec<String>> {
    let mut forwarded_prefix = Vec::new();
    let mut index = 0;
    while index < raw_args.len() {
        let token = raw_args[index].to_string_lossy();
        // `run` is only a utility when followed by reconstruct subcommands.
        if token == "run" {
            let rest = &raw_args[index + 1..];
            let mut j = 0;
            let mut has_sub = false;
            while j < rest.len() {
                let t = rest[j].to_string_lossy();
                if matches!(
                    t.as_ref(),
                    "inspect"
                        | "timeline"
                        | "context"
                        | "ledger"
                        | "replay"
                        | "promote"
                        | "help"
                        | "--help"
                        | "-h"
                ) {
                    has_sub = true;
                    break;
                }
                if t.starts_with('-') {
                    if matches!(t.as_ref(), "--json") {
                        j += 1;
                        continue;
                    }
                    j += 1;
                    continue;
                }
                break;
            }
            if !has_sub {
                return None;
            }
        }
        if NATIVE_UTILITY_COMMANDS.contains(&token.as_ref()) {
            let mut tokens = raw_args[index..]
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            tokens.extend(forwarded_prefix);
            return Some(tokens);
        }
        if matches!(token.as_ref(), "--json" | "--force") {
            forwarded_prefix.push(token.into_owned());
            index += 1;
            continue;
        }
        if token == "--worktree" || token == "-w" {
            index += 1;
            if raw_args
                .get(index)
                .is_some_and(|value| !value.to_string_lossy().starts_with('-'))
            {
                index += 1;
            }
            continue;
        }
        if GLOBAL_FLAGS_WITH_VALUES.contains(&token.as_ref()) {
            let value = raw_args.get(index + 1)?;
            if matches!(token.as_ref(), "--provider" | "--session" | "--format") {
                forwarded_prefix.push(token.into_owned());
                forwarded_prefix.push(value.to_string_lossy().into_owned());
            }
            index += 2;
            continue;
        }
        if token.starts_with("--provider=")
            || token.starts_with("--session=")
            || token.starts_with("--format=")
        {
            forwarded_prefix.push(token.into_owned());
            index += 1;
            continue;
        }
        if token.starts_with('-') {
            index += 1;
            continue;
        }
        return None;
    }
    None
}

fn native_profile(raw_args: &[std::ffi::OsString]) -> Option<String> {
    let mut config_profile = None;
    for (index, argument) in raw_args.iter().enumerate() {
        let argument = argument.to_string_lossy();
        if argument == "--profile" {
            return raw_args
                .get(index + 1)
                .map(|value| value.to_string_lossy().into_owned());
        }
        if let Some(profile) = argument.strip_prefix("--profile=") {
            return Some(profile.to_owned());
        }
        let config = if argument == "--config" {
            raw_args.get(index + 1).map(|value| value.to_string_lossy())
        } else {
            argument
                .strip_prefix("--config=")
                .map(std::borrow::Cow::Borrowed)
        };
        if let Some(profile) =
            config.and_then(|value| value.strip_prefix("profile=").map(str::to_owned))
        {
            config_profile = Some(profile);
        }
    }
    config_profile
}

fn native_config_overrides(raw_args: &[std::ffi::OsString]) -> Vec<String> {
    let mut overrides = Vec::new();
    let mut index = 0;
    while index < raw_args.len() {
        let argument = raw_args[index].to_string_lossy();
        if argument == "--config" {
            if let Some(value) = raw_args.get(index + 1) {
                overrides.push(value.to_string_lossy().into_owned());
                index += 1;
            }
        } else if let Some(value) = argument.strip_prefix("--config=") {
            overrides.push(value.to_owned());
        }
        index += 1;
    }
    overrides
}

/// Infer the AI provider from the model name.
///
/// # Rust Concepts Used
///
/// - **`&str` vs `String`**: `&str` is a borrowed string slice (a view into string data),
///   while `String` is an owned, heap-allocated string. We take `&str` as input because
///   we only need to read the model name, not own it.
///
/// - **`&'static str`**: The `'static` lifetime means the returned string lives for the
///   entire program duration. String literals like `"openai"` have this lifetime because
///   they're embedded in the binary.
///
/// - **`to_lowercase()`**: Returns a new `String` (owned) because the lowercase version
///   might have different UTF-8 byte lengths than the original.
///
/// # Arguments
///
/// * `model` - The model name to analyze (for example, "gpt-5.5")
///
/// # Returns
///
/// A static string identifying the provider (e.g., "openai", "anthropic")
fn infer_direct_provider_from_bare_model(model_lower: &str) -> Option<&'static str> {
    if matches!(model_lower, "deepseek-chat" | "deepseek-reasoner")
        || model_lower.starts_with("deepseek-v")
    {
        return Some("deepseek");
    }

    if model_lower.starts_with("kimi-")
        || model_lower == "kimi-latest"
        || model_lower.starts_with("moonshot-v1-")
    {
        return Some("moonshot");
    }

    if model_lower.starts_with("qwen3-")
        || matches!(
            model_lower,
            "qwen-max" | "qwen-plus" | "qwen-turbo" | "qwen-vl-max" | "qwq-32b"
        )
    {
        return Some("dashscope");
    }

    if matches!(
        model_lower,
        "minimax-m2" | "minimax-m2.5" | "minimax-m2.7" | "minimax-text-01"
    ) {
        return Some("minimax");
    }

    if model_lower.starts_with("glm-") {
        return Some("zai");
    }

    None
}

fn infer_provider_from_model(model: &str) -> &'static str {
    // Convert to lowercase for case-insensitive matching.
    // Note: This allocates a new String on the heap.
    let model_lower = model.to_lowercase();

    // Explicit "provider/model" prefixes win over heuristic name matching.
    if let Some((prefix, _)) = model_lower.split_once('/') {
        match prefix {
            "vertex-ai" | "vertex" => return "vertex-ai",
            "deepseek" => return "deepseek",
            "moonshot" | "kimi" => return "moonshot",
            "dashscope" | "qwen" => return "dashscope",
            "minimax" => return "minimax",
            "zai" | "zhipu" => return "zai",
            _ => {}
        }
    }

    if let Some(provider) = infer_direct_provider_from_bare_model(&model_lower) {
        return provider;
    }

    // OpenAI models - check various prefixes that indicate OpenAI
    // The `||` operator short-circuits: if the first condition is true,
    // subsequent conditions aren't evaluated.
    if model_lower.starts_with("gpt")
        || model_lower.starts_with("o1")
        || model_lower.starts_with("o3")
        || model_lower.contains("codex")
        || model_lower.starts_with("text-")
        || model_lower.starts_with("davinci")
    {
        // `return` exits the function early with the given value.
        // In Rust, the last expression without a semicolon is implicitly returned,
        // but explicit `return` is clearer for early exits.
        return "openai";
    }

    // Anthropic models (Claude family)
    if model_lower.starts_with("claude") {
        return "anthropic";
    }

    // Google/Gemini models
    if model_lower.starts_with("gemini") || model_lower.starts_with("palm") {
        return "google";
    }

    // xAI/Grok models
    if model_lower.starts_with("grok") {
        return "xai";
    }

    // Groq hosts Llama plus DeepSeek/Qwen distill/coder variants. Direct-provider
    // bare ids are handled earlier by `infer_direct_provider_from_bare_model`, so
    // these heuristics mirror the Groq fallback in `AiProvider::from_model` and
    // keep `--api-key` env wiring aligned with the client that actually runs
    // (e.g. deepseek-r1-distill-llama-70b, qwen-2.5-coder-32b -> GROQ_API_KEY).
    if model_lower.contains("groq")
        || model_lower.starts_with("llama-")
        || model_lower.starts_with("llama3")
        || model_lower.contains("deepseek")
        || model_lower.contains("qwen")
    {
        return "groq";
    }

    // Cerebras models
    if model_lower.contains("cerebras") {
        return "cerebras";
    }

    // OpenRouter uses a "provider/model" format (e.g., "anthropic/claude-3")
    if model_lower.contains('/') {
        return "openrouter";
    }

    // Default to OpenAI/Codex if we can't identify the provider
    // Note: No semicolon here - this is the implicit return value
    "openai"
}

// ─────────────────────────────────────────────────────────────────────────────
// CLI ARGUMENTS DEFINITION
// ─────────────────────────────────────────────────────────────────────────────

/// Command-line arguments for the Deixic Code TUI.
#[derive(Parser, Debug)]
#[command(name = "deixic-code")]
#[command(about = "Native terminal interface for Deixic Code")]
#[command(version = env!("CARGO_PKG_VERSION"))]
#[command(long_about = "Native terminal UI for Deixic Code.\n\n\
Interactive: deixic-code --provider openai -m gpt-4.1-mini\n\
Print mode:  deixic-code -p --provider openai -m gpt-4.1-mini \"question\"\n\
Trust cwd:   deixic-code trust\n\
MCP manager: deixic-code mcp --help\n\
Sandbox:     use /sandbox in-session or MAESTRO_SANDBOX_MODE")]
struct Args {
    /// Provider to use (for example, openai). When omitted, inferred from the model.
    #[arg(long)]
    provider: Option<String>,

    /// Model to use (for example, gpt-5.5).
    #[arg(short, long)]
    model: Option<String>,

    /// Named focus for a non-interactive print run.
    #[arg(long, requires = "print", conflicts_with_all = ["headless", "rpc", "resume", "continue"])]
    specialist: Option<String>,

    /// API key for authentication (defaults to env / op:// references).
    #[arg(long)]
    api_key: Option<String>,

    /// Load a local plugin for this process only without changing plugin state.
    #[arg(long = "plugin", value_name = "PATH", action = clap::ArgAction::Append)]
    plugins: Vec<std::path::PathBuf>,

    /// Continue the previous session.
    #[arg(short, long)]
    r#continue: bool,

    /// Open the session resume selector.
    #[arg(short, long)]
    resume: bool,

    /// Resume a specific session in its saved workspace.
    #[arg(long, value_name = "ID", conflicts_with_all = ["resume", "continue", "print", "headless", "rpc", "no_session", "worktree", "prompt"])]
    resume_session: Option<String>,

    /// Do not persist this conversation (ephemeral session).
    #[arg(long = "no-session")]
    no_session: bool,

    /// Run the session in a new git worktree at `../<repo>-wt-<name>` on a new branch.
    #[arg(short = 'w', long, value_name = "NAME")]
    worktree: Option<String>,

    /// Non-interactive print mode (single-shot). Prints the answer and exits.
    #[arg(long, short = 'p')]
    print: bool,

    /// With `--print`, emit simple JSONL events instead of plain text.
    #[arg(long)]
    json: bool,

    /// Print the native tool catalog and exit.
    #[arg(long)]
    list_tools: bool,

    /// Restrict a print run to these comma- or space-separated tool names.
    #[arg(long, requires = "print", action = clap::ArgAction::Append)]
    only_tools: Vec<String>,

    /// Add tools to a print run without widening configured policy ceilings.
    #[arg(long, requires = "print", action = clap::ArgAction::Append)]
    add_tools: Vec<String>,

    /// Remove tools from a print run.
    #[arg(long, requires = "print", action = clap::ArgAction::Append)]
    remove_tools: Vec<String>,

    /// Attach a name or JSON object to this run; repeatable.
    #[arg(long, requires = "print", action = clap::ArgAction::Append)]
    tag: Vec<String>,

    /// Correlate this run with related CI, release, or batch runs.
    #[arg(long, requires = "print")]
    log_group_id: Option<String>,

    /// Run as native headless protocol server on stdio.
    #[arg(long)]
    headless: bool,

    /// Alias for `--headless` (RPC clients).
    #[arg(long)]
    rpc: bool,

    /// Write final assistant text to this file (exec parity).
    #[arg(long = "output-last-message")]
    output_last_message: Option<String>,

    /// Validate final assistant text against a JSON Schema file or inline JSON.
    #[arg(long = "output-schema")]
    output_schema: Option<String>,

    /// Initial prompt to send (all remaining arguments are joined).
    #[arg(trailing_var_arg = true)]
    prompt: Vec<String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct NativeExecOptions {
    specialist: Option<String>,
    json: bool,
    model: Option<String>,
    output_last: Option<std::path::PathBuf>,
    output_schema: Option<String>,
    sandbox: Option<String>,
    approval_mode: Option<String>,
    provider: Option<String>,
    api_key: Option<String>,
    prompt_stdin: bool,
    prompt: String,
    plugins: Vec<std::path::PathBuf>,
    list_tools: bool,
    only_tools: Vec<String>,
    add_tools: Vec<String>,
    remove_tools: Vec<String>,
    tags: Vec<String>,
    log_group_id: Option<String>,
}

fn parse_native_exec_options(raw_args: &[std::ffi::OsString]) -> NativeExecOptions {
    let mut options = NativeExecOptions::default();
    let mut prompt_parts = Vec::new();
    let mut positional_only = false;
    let mut i = 0usize;
    while i < raw_args.len() {
        let arg = raw_args[i].to_string_lossy();
        if positional_only {
            prompt_parts.push(arg.into_owned());
        } else if arg == "--" {
            positional_only = true;
        } else if arg == "--json" || arg == "--mode=json" {
            options.json = true;
        } else if arg == "--list-tools" {
            options.list_tools = true;
        } else if matches!(
            arg.as_ref(),
            "--only-tools" | "--add-tools" | "--remove-tools" | "--tag" | "--log-group-id"
        ) {
            let flag = arg.into_owned();
            i += 1;
            let value = raw_args
                .get(i)
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_default();
            match flag.as_str() {
                "--only-tools" => options.only_tools.push(value),
                "--add-tools" => options.add_tools.push(value),
                "--remove-tools" => options.remove_tools.push(value),
                "--tag" => options.tags.push(value),
                "--log-group-id" => options.log_group_id = Some(value),
                _ => unreachable!(),
            }
        } else if let Some(value) = arg.strip_prefix("--only-tools=") {
            options.only_tools.push(value.to_string());
        } else if let Some(value) = arg.strip_prefix("--add-tools=") {
            options.add_tools.push(value.to_string());
        } else if let Some(value) = arg.strip_prefix("--remove-tools=") {
            options.remove_tools.push(value.to_string());
        } else if let Some(value) = arg.strip_prefix("--tag=") {
            options.tags.push(value.to_string());
        } else if let Some(value) = arg.strip_prefix("--log-group-id=") {
            options.log_group_id = Some(value.to_string());
        } else if arg == "--mode" {
            i += 1;
            if i < raw_args.len() && raw_args[i] == "json" {
                options.json = true;
            }
        } else if arg == "--specialist" {
            i += 1;
            options.specialist = Some(
                raw_args
                    .get(i)
                    .map(|v| v.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            );
        } else if let Some(value) = arg.strip_prefix("--specialist=") {
            options.specialist = Some(value.to_string());
        } else if arg == "--model" || arg == "-m" {
            i += 1;
            if i < raw_args.len() {
                options.model = Some(raw_args[i].to_string_lossy().into_owned());
            }
        } else if let Some(value) = arg.strip_prefix("--model=") {
            options.model = Some(value.to_string());
        } else if arg == "--output-last-message" {
            i += 1;
            if i < raw_args.len() {
                options.output_last = Some(std::path::PathBuf::from(&raw_args[i]));
            }
        } else if let Some(value) = arg.strip_prefix("--output-last-message=") {
            options.output_last = Some(std::path::PathBuf::from(value));
        } else if arg == "--output-schema" {
            i += 1;
            if i < raw_args.len() {
                options.output_schema = Some(raw_args[i].to_string_lossy().into_owned());
            }
        } else if let Some(value) = arg.strip_prefix("--output-schema=") {
            options.output_schema = Some(value.to_string());
        } else if arg == "--sandbox" {
            i += 1;
            if i < raw_args.len() {
                options.sandbox = Some(raw_args[i].to_string_lossy().into_owned());
            }
        } else if let Some(value) = arg.strip_prefix("--sandbox=") {
            options.sandbox = Some(value.to_string());
        } else if arg == "--approval-mode" {
            i += 1;
            if i < raw_args.len() {
                options.approval_mode = Some(raw_args[i].to_string_lossy().into_owned());
            }
        } else if let Some(value) = arg.strip_prefix("--approval-mode=") {
            options.approval_mode = Some(value.to_string());
        } else if arg == "--provider" {
            i += 1;
            if i < raw_args.len() {
                options.provider = Some(raw_args[i].to_string_lossy().into_owned());
            }
        } else if let Some(value) = arg.strip_prefix("--provider=") {
            options.provider = Some(value.to_string());
        } else if arg == "--api-key" {
            i += 1;
            if i < raw_args.len() {
                options.api_key = Some(raw_args[i].to_string_lossy().into_owned());
            }
        } else if let Some(value) = arg.strip_prefix("--api-key=") {
            options.api_key = Some(value.to_string());
        } else if arg == "--plugin" {
            i += 1;
            if i < raw_args.len() {
                options.plugins.push(std::path::PathBuf::from(&raw_args[i]));
            }
        } else if let Some(value) = arg.strip_prefix("--plugin=") {
            options.plugins.push(std::path::PathBuf::from(value));
        } else if arg == "--prompt-stdin" {
            options.prompt_stdin = true;
        } else if arg == "--worktree" || arg == "-w" {
            // Session worktree is created before dispatch; consume its value
            // so it never leaks into the prompt.
            i += 1;
        } else if arg.starts_with("--worktree=") || (arg.starts_with("-w") && arg.len() > 2) {
            // Inline forms; handled before dispatch.
        } else if arg == "--session" || arg == "--config" || arg == "--profile" {
            // Compatibility option whose value must never leak into the prompt.
            i += 1;
        } else if arg.starts_with("--config=") || arg.starts_with("--profile=") {
            // Parsed before command dispatch; consume inline forms here as well.
        } else if !arg.starts_with('-') {
            prompt_parts.push(arg.into_owned());
        }
        i += 1;
    }
    options.prompt = prompt_parts.join(" ");
    options
}

fn parse_run_tags(values: &[String]) -> std::result::Result<Vec<serde_json::Value>, String> {
    if values.len() > 32 {
        return Err("at most 32 --tag values are allowed".to_owned());
    }
    values
        .iter()
        .map(|value| {
            let value = value.trim();
            if value.is_empty() {
                return Err("--tag must not be empty".to_owned());
            }
            if value.len() > 512 {
                return Err("--tag values must not exceed 512 bytes".to_owned());
            }
            if !value.starts_with('{') {
                return Ok(serde_json::Value::String(value.to_owned()));
            }
            let parsed: serde_json::Value = serde_json::from_str(value)
                .map_err(|error| format!("invalid --tag JSON: {error}"))?;
            let name = parsed
                .get("name")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty());
            if name.is_none() {
                return Err("structured --tag JSON requires a non-empty `name`".to_owned());
            }
            Ok(parsed)
        })
        .collect()
}

fn parse_log_group_id(value: Option<String>) -> std::result::Result<Option<String>, String> {
    value
        .map(|value| value.trim().to_owned())
        .map(|value| {
            if value.is_empty()
                || value.len() > 128
                || !value.chars().all(|character| {
                    character.is_ascii_alphanumeric() || "-_.:/".contains(character)
                })
            {
                return Err(
                    "--log-group-id must be 1-128 ASCII letters, digits, or -_.:/".to_owned(),
                );
            }
            Ok(value)
        })
        .transpose()
}

fn native_exec_sandbox_policy(
    value: Option<&str>,
) -> std::result::Result<Option<SandboxPolicy>, String> {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        None | Some("danger-full-access") => Ok(None),
        Some("read-only") => Ok(Some(SandboxPolicy::ReadOnly)),
        Some("native" | "workspace-write") => Ok(Some(SandboxPolicy::WorkspaceWrite {
            writable_roots: Vec::new(),
            network_access: true,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
        })),
        Some(value) => Err(format!(
            "Unsupported exec sandbox `{value}`; use read-only, workspace-write, native, or danger-full-access"
        )),
    }
}

fn native_exec_model(
    provider: Option<&str>,
    model: Option<&str>,
) -> std::result::Result<Option<String>, String> {
    let provider = provider.map(str::trim).filter(|value| !value.is_empty());
    let model = model.map(str::trim).filter(|value| !value.is_empty());
    if let (Some(provider), None) = (provider, model) {
        let canonical_provider = crate::ai::ProviderRegistry::descriptor(provider)
            .map_or(provider, |descriptor| descriptor.id);
        if matches!(canonical_provider, "lmstudio" | "ollama") {
            return Err(format!(
                "--provider {provider} requires --model because the loaded local model cannot be inferred"
            ));
        }
    }
    Ok(crate::config::compose_model_route(provider, model))
}

fn set_provider_api_key(provider: &str, api_key: &str) {
    std::env::set_var(provider_api_key_env(provider), api_key);
}

fn provider_api_key_env(provider: &str) -> &'static str {
    match provider {
        "openai" => "OPENAI_API_KEY",
        "openai-codex" => "OPENAI_CODEX_TOKEN",
        "google" | "gemini" => "GEMINI_API_KEY",
        "vertex-ai" | "vertex" => "GOOGLE_API_KEY",
        "google-gemini-cli" => "GOOGLE_GEMINI_CLI_TOKEN",
        "google-antigravity" => "GOOGLE_ANTIGRAVITY_TOKEN",
        "evalops" | "maestro-managed" => "MAESTRO_EVALOPS_ACCESS_TOKEN",
        "azure-openai" | "azure" => "AZURE_OPENAI_API_KEY",
        "mistral" => "MISTRAL_API_KEY",
        "xai" => "XAI_API_KEY",
        "groq" => "GROQ_API_KEY",
        "cerebras" => "CEREBRAS_API_KEY",
        "openrouter" => "OPENROUTER_API_KEY",
        "deepseek" => "DEEPSEEK_API_KEY",
        "moonshot" | "kimi" => "MOONSHOT_API_KEY",
        "dashscope" | "qwen" => "DASHSCOPE_API_KEY",
        "minimax" => "MINIMAX_API_KEY",
        "zai" | "zhipu" => "ZAI_API_KEY",
        "writer" => "WRITER_API_KEY",
        _ => "ANTHROPIC_API_KEY",
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// MAIN FUNCTION
// ─────────────────────────────────────────────────────────────────────────────

/// Application entry point.
///
/// # Rust Concepts Used
///
/// - **`#[tokio::main]`**: This attribute macro transforms `async fn main()`
///   into a synchronous main that sets up the Tokio async runtime. Without it,
///   we couldn't use `.await` in main.
///
/// - **`async/await`**: Rust's async programming model. `async fn` returns a
///   Future that must be `.await`ed to get the result. This enables non-blocking
///   I/O without callbacks.
///
/// - **`Result<()>`**: Returns either `Ok(())` (success with unit type) or an
///   error. The `?` operator propagates errors up the call stack automatically.
///
/// - **Error Propagation with `?`**: When you see `foo()?`, it means "if `foo()`
///   returns an error, return that error from this function; otherwise, unwrap
///   the Ok value and continue."
pub async fn run_cli(raw_args: Vec<std::ffi::OsString>) -> Result<()> {
    // Machine policy is checked before updates, Identity, configuration or command dispatch.
    maestro_local_host::safety::disconnected_policy().map_err(anyhow::Error::msg)?;
    // maestro-tui remains a Cargo target for compatibility, but it is not a
    // public product name. Normalize argv[0] before Clap renders help or
    // diagnostics so both binaries expose the same Deixic Code surface.
    let mut raw_args = raw_args;
    if let Some(program) = raw_args.first_mut() {
        *program = std::ffi::OsString::from("deixic-code");
    }

    // Set up panic hook for process cleanup on unexpected termination.
    // This ensures background processes are killed even if the app panics.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        // Clean up background processes before panicking
        let count = cleanup_background_processes();
        if count > 0 {
            eprintln!("[panic] Cleaned up {count} background process(es)");
        }
        // Call the default panic hook to print the panic message
        default_hook(panic_info);
    }));

    if raw_args
        .get(1)
        .and_then(|arg| arg.to_str())
        .is_some_and(|arg| arg == "hosted-runner")
    {
        maestro_local_host::safety::require_vendor_network()?;
        let mut hosted_args = vec![std::ffi::OsString::from("deixic-code hosted-runner")];
        hosted_args.extend(raw_args.into_iter().skip(2));
        run_hosted_runner_cli_from_env(hosted_args).await?;
        return Ok(());
    }

    // Hidden release fixture command. It reuses the canonical hosted HTTP/SSE
    // runtime so black-box conformance exercises the exact native executable.
    if raw_args
        .get(1)
        .and_then(|arg| arg.to_str())
        .is_some_and(|arg| arg == "conformance")
    {
        crate::hosted_runner_conformance::run().await?;
        return Ok(());
    }

    // Lightweight CLI helpers (no TUI / no full interactive loop). Utility
    // argument normalization lives here so the package shim can forward argv.
    if let Some(profile) = native_profile(&raw_args[1..]) {
        // SAFETY: `run_cli` is the sole top-level future of this process's
        // `#[tokio::main]` body and this runs before its first `.await` or
        // `tokio::spawn`, so no other task can be concurrently reading or
        // writing the environment even though the runtime's (idle) worker
        // threads already exist. The profile is converted to the same
        // environment contract used by Rust config loading.
        unsafe { std::env::set_var("MAESTRO_PROFILE", profile) };
    }
    let config_overrides = native_config_overrides(&raw_args[1..]);
    if !config_overrides.is_empty() {
        // SAFETY: see above — still before this task's first `.await` or spawn.
        unsafe {
            std::env::set_var(
                "MAESTRO_CLI_CONFIG_OVERRIDES",
                config_overrides.join("\u{1f}"),
            );
        };
    }
    if let Some(mut tokens) = native_utility_tokens(&raw_args[1..]) {
        if tokens.first().is_some_and(|token| token == "specialists") {
            tokens[0] = "agents".into();
            tokens.insert(1, "profile".into());
        }
        if tokens.first().is_some_and(|token| token == "agents") {
            configure_agents_api_key(&raw_args[1..]);
            let outcome = match crate::agents_cli::run(&tokens[1..]) {
                Ok(outcome) => outcome,
                Err(error) => {
                    eprintln!("{error:#}");
                    std::process::exit(1);
                }
            };
            match outcome {
                crate::agents_cli::Outcome::Exit(code) => std::process::exit(code),
                crate::agents_cli::Outcome::Generate { prompt, target } => {
                    let cwd = std::env::current_dir()?;
                    let display = target
                        .strip_prefix(&cwd)
                        .map(|relative| format!("./{}", relative.display()))
                        .unwrap_or_else(|_| target.display().to_string());
                    println!("Drafting AGENTS.md at {display}...");
                    let model = raw_option_value(&raw_args[1..], &["--model", "-m"]);
                    let code =
                        crate::print_mode::run_print_mode(crate::print_mode::PrintModeOptions {
                            specialist: None,
                            prompt,
                            json: false,
                            model,
                            output_last_message: None,
                            output_schema: None,
                            sandbox_policy: None,
                            fail_on_approval: false,
                            only_tools: Vec::new(),
                            add_tools: Vec::new(),
                            remove_tools: Vec::new(),
                            tags: Vec::new(),
                            log_group_id: None,
                        })
                        .await?;
                    crate::telemetry::flush_first_party_telemetry().await;
                    std::process::exit(code);
                }
            }
        }
        match crate::cli_commands::run_cli_command(&tokens).await {
            Ok(code) => std::process::exit(code),
            Err(err) => {
                eprintln!("{err:#}");
                std::process::exit(1);
            }
        }
    }

    if classify_agent_entry(&raw_args) == AgentEntry::ClapParsed
        && classify_clap_dispatch(&raw_args) == ClapDispatch::Interactive
    {
        if let Some(exit_code) = crate::update_cli::run_startup_update(&raw_args).await {
            std::process::exit(exit_code);
        }
    }

    // Droid-style session worktree (`-w` / `--worktree`): set up before any
    // agent surface so the interactive TUI, `exec`, and print mode all run
    // with the worktree as their working directory. Utility commands above
    // dispatch first and never create worktrees.
    let worktree = match crate::worktree::requested_name(&raw_args[1..]) {
        Some(Ok(name)) => {
            let cwd = std::env::current_dir()?;
            match crate::worktree::WorktreeSession::create_in(&cwd, &name) {
                Ok(session) => {
                    if let Err(err) = std::env::set_current_dir(session.path()) {
                        eprintln!(
                            "Failed to enter worktree {}: {err}",
                            session.path().display()
                        );
                        std::process::exit(1);
                    }
                    eprintln!("Using worktree: {}", session.path().display());
                    Some(session)
                }
                Err(err) => {
                    eprintln!("Worktree setup failed: {err:#}");
                    std::process::exit(1);
                }
            }
        }
        Some(Err(message)) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
        None => None,
    };

    let keep_worktree = keep_session_worktree(&raw_args);
    let result = run_agent(raw_args).await;
    crate::telemetry::flush_first_party_telemetry().await;
    if let Some(session) = worktree {
        if keep_worktree {
            session.keep();
        } else {
            session.finish();
        }
    }
    let exit_code = result?;
    std::process::exit(exit_code);
}

/// Which fast path (if any) `run_agent` takes before falling back to the
/// full clap-derived `Args` parse. This is the single decision point for the
/// `headless`/`rpc`/`fork`/`exec`/`print` subcommand words, consumed by `run_agent`
/// and exercised directly by `tests/entrypoint.rs` so a routing change
/// here can't silently drift from what the test matrix expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentEntry {
    /// First argv token is `headless` or `rpc`.
    HeadlessSubcommand,
    /// First argv token is `fork`.
    ForkSubcommand,
    /// First argv token is `exec` or `print`; carries the literal word for
    /// the usage message.
    ExecOrPrintSubcommand(&'static str),
    /// First argv token is `trust` (grant/revoke/status for cwd).
    TrustSubcommand,
    /// Falls through to `Args::try_parse_from` (interactive TUI, `-p`,
    /// `--headless`/`--rpc` flags, unknown commands, `--help`/`--version`
    /// on the native binary directly, or a parse error).
    ClapParsed,
}

/// Classify the first argv token for `run_agent`'s subcommand fast path.
pub fn classify_agent_entry(raw_args: &[std::ffi::OsString]) -> AgentEntry {
    if let Some(cmd) = raw_args.get(1).and_then(|a| a.to_str()) {
        if cmd == "headless" || cmd == "rpc" {
            return AgentEntry::HeadlessSubcommand;
        }
        if cmd == "fork" {
            return AgentEntry::ForkSubcommand;
        }
        if cmd == "exec" {
            return AgentEntry::ExecOrPrintSubcommand("exec");
        }
        if cmd == "print" {
            return AgentEntry::ExecOrPrintSubcommand("print");
        }
        if cmd == "trust" {
            return AgentEntry::TrustSubcommand;
        }
    }
    AgentEntry::ClapParsed
}

/// What `AgentEntry::ClapParsed` resolves to once the argv is actually run
/// through the real `Args` clap derive. Mirrors the branching `run_agent`
/// performs on its own parsed `Args` (see the `args.headless || args.rpc`
/// and `args.print` checks below); kept alongside `classify_agent_entry` so
/// `tests/entrypoint.rs` exercises the production `Args` parser directly
/// for flag-driven routing (`-p`, `--headless`, `--rpc`, `--mode=headless`)
/// rather than re-describing clap's flag names in the test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClapDispatch {
    /// Starts the headless server from either supported headless flag.
    Headless,
    /// Runs a single non-interactive prompt and prints its result.
    Print,
    /// Continues into the interactive agent path.
    Interactive,
    /// Lets clap print help or version information and exit successfully.
    DisplayHelpOrVersion,
    /// Rejects argv that the real clap parser does not accept.
    ParseError,
}

/// Classify a `ClapParsed` argv (see [`AgentEntry`]) by running it through
/// the real `Args::try_parse_from`.
pub fn classify_clap_dispatch(raw_args: &[std::ffi::OsString]) -> ClapDispatch {
    match Args::try_parse_from(raw_args) {
        Ok(args) => {
            if args.headless || args.rpc {
                ClapDispatch::Headless
            } else if args.print {
                ClapDispatch::Print
            } else {
                ClapDispatch::Interactive
            }
        }
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            ClapDispatch::DisplayHelpOrVersion
        }
        Err(_) => ClapDispatch::ParseError,
    }
}

// Only one-shot execution opts into automatic directory cleanup. Interactive,
// forked, and protocol sessions can reconnect and need a stable working directory.
fn keep_session_worktree(raw_args: &[std::ffi::OsString]) -> bool {
    match classify_agent_entry(raw_args) {
        AgentEntry::ExecOrPrintSubcommand(_) => false,
        AgentEntry::ClapParsed => classify_clap_dispatch(raw_args) != ClapDispatch::Print,
        _ => true,
    }
}

/// Agent dispatch shared by the interactive TUI, fork/resume, exec, print,
/// and headless modes. Returns the process exit code instead of exiting
/// directly so the caller can run worktree teardown first.
async fn run_agent(raw_args: Vec<std::ffi::OsString>) -> Result<i32> {
    match classify_agent_entry(&raw_args) {
        AgentEntry::HeadlessSubcommand => {
            let code = crate::headless_server::run_headless_server(raw_option_value(
                &raw_args,
                &["--model", "-m"],
            ))
            .await?;
            return Ok(code);
        }
        AgentEntry::ForkSubcommand => return run_fork(&raw_args[2..]).await,
        AgentEntry::TrustSubcommand => return run_trust_cli(&raw_args[2..]),
        AgentEntry::ExecOrPrintSubcommand(cmd) => {
            // maestro print|exec [--json] [--model X] [--output-last-message P]
            //   [--output-schema S] <prompt...>
            let mut options = parse_native_exec_options(&raw_args[2..]);
            if options.list_tools {
                crate::print_mode::print_available_tools(options.json)?;
                return Ok(0);
            }
            let env_sandbox = std::env::var("MAESTRO_SANDBOX_MODE").ok();
            let sandbox_value = options.sandbox.as_deref().or(env_sandbox.as_deref());
            let sandbox_policy = match native_exec_sandbox_policy(sandbox_value) {
                Ok(policy) => policy,
                Err(message) => {
                    eprintln!("{message}");
                    return Ok(2);
                }
            };
            if let Some(api_key) = &options.api_key {
                let provider = options.provider.as_deref().unwrap_or_else(|| {
                    options
                        .model
                        .as_deref()
                        .map(infer_provider_from_model)
                        .unwrap_or("openai")
                });
                set_provider_api_key(provider, api_key);
            }
            if options.prompt_stdin {
                use tokio::io::AsyncReadExt as _;
                tokio::io::stdin()
                    .read_to_string(&mut options.prompt)
                    .await?;
            }
            if options.prompt.is_empty() {
                eprintln!(
                    "Usage: deixic-code {cmd} [--json] [--model <id>] [--only-tools <names>] [--add-tools <names>] [--remove-tools <names>] [--tag <name|json>] [--log-group-id <id>] <prompt>"
                );
                return Ok(2);
            }
            let tags = match parse_run_tags(&options.tags) {
                Ok(tags) => tags,
                Err(message) => {
                    eprintln!("{message}");
                    return Ok(2);
                }
            };
            let log_group_id = match parse_log_group_id(options.log_group_id) {
                Ok(log_group_id) => log_group_id,
                Err(message) => {
                    eprintln!("{message}");
                    return Ok(2);
                }
            };
            let model =
                match native_exec_model(options.provider.as_deref(), options.model.as_deref()) {
                    Ok(model) => model,
                    Err(message) => {
                        eprintln!("{message}");
                        return Ok(2);
                    }
                };
            configure_ephemeral_plugins(&options.plugins)?;
            let code = crate::print_mode::run_print_mode(crate::print_mode::PrintModeOptions {
                specialist: options.specialist,
                prompt: options.prompt,
                json: options.json,
                model,
                output_last_message: options.output_last,
                output_schema: options.output_schema,
                sandbox_policy,
                fail_on_approval: options.approval_mode.as_deref() == Some("fail"),
                only_tools: options.only_tools,
                add_tools: options.add_tools,
                remove_tools: options.remove_tools,
                tags,
                log_group_id,
            })
            .await?;
            return Ok(code);
        }
        AgentEntry::ClapParsed => {}
    }

    // Parse command-line arguments using clap.
    // `Args::parse()` reads from std::env::args() and returns our Args struct.
    // If parsing fails (e.g., unknown flag), clap prints help and exits.
    let mut args = match Args::try_parse_from(raw_args.clone()) {
        Ok(args) => args,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            error.print()?;
            return Ok(0);
        }
        Err(error) => return Err(error.into()),
    };

    if let Some(id) = &args.resume_session {
        let cwd = std::env::current_dir()?;
        let manager = crate::session::SessionManager::new(cwd.to_string_lossy().to_string());
        let session = manager.find_session(id)?;
        if args.model.is_none() {
            args.model = Some(session.model);
        }
    }

    // Set API key from CLI if provided.
    // This allows users to override environment variables via command line.
    //
    // Rust Concept: `if let Some(x) = option` is pattern matching that only
    // executes the block if the Option is Some, binding the inner value to `x`.
    if let Some(api_key) = &args.api_key {
        // Determine which provider's API key to set.
        //
        // Rust Concept: `unwrap_or_else` takes a closure (anonymous function)
        // that's only called if the Option is None. This is lazier than
        // `unwrap_or` which always evaluates its argument.
        //
        // The `|| { ... }` syntax creates a closure. The `||` are the parameter
        // list (empty in this case), and `{ ... }` is the body.
        let provider = args.provider.as_deref().unwrap_or_else(|| {
            // Infer provider from model name if no explicit provider given
            if let Some(model) = &args.model {
                infer_provider_from_model(model)
            } else {
                "openai"
            }
        });

        // Set the appropriate environment variable based on provider.
        //
        // Rust Concept: `match` is exhaustive pattern matching. Unlike switch
        // in other languages, it must handle all possible cases (or use `_`
        // as a catch-all).
        set_provider_api_key(provider, api_key);
    }

    // Preserve an explicit provider in the model route for every runtime.
    // This is particularly important for locally hosted model families whose
    // bare names may also match a cloud-provider inference heuristic.
    let selected_model = match native_exec_model(args.provider.as_deref(), args.model.as_deref()) {
        Ok(model) => model,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };

    // Set model from CLI if provided.
    // This environment variable is read by the App during initialization.
    if let Some(model) = &selected_model {
        std::env::set_var("MAESTRO_MODEL", model);
    }

    // Worktree setup already ran in run_cli before clap parsing; the field
    // stays in Args so the flag validates and shows up in --help.
    let _ = args.worktree.as_deref();

    configure_ephemeral_plugins(&args.plugins)?;

    if args.no_session {
        // SessionManager still creates an id for UI purposes, but callers that
        // honor this env skip durable transcript persistence.
        std::env::set_var("MAESTRO_NO_SESSION", "1");
    }

    // Trailing positional args become the initial prompt (Grok-style).
    let initial_prompt = if args.prompt.is_empty() {
        None
    } else {
        Some(args.prompt.join(" "))
    };

    // Native headless/RPC server (kills TS agent path for these modes)
    if args.headless || args.rpc {
        let code = crate::headless_server::run_headless_server(selected_model.clone()).await?;
        return Ok(code);
    }

    if args.list_tools {
        crate::print_mode::print_available_tools(args.json)?;
        return Ok(0);
    }

    // Non-interactive print mode (single-shot / exec bridge)
    if args.print {
        let prompt = initial_prompt.unwrap_or_default();
        if prompt.is_empty() {
            eprintln!("--print requires a prompt");
            return Ok(2);
        }
        let tags = parse_run_tags(&args.tag).map_err(anyhow::Error::msg)?;
        let log_group_id = parse_log_group_id(args.log_group_id).map_err(anyhow::Error::msg)?;
        let code = crate::print_mode::run_print_mode(crate::print_mode::PrintModeOptions {
            specialist: args.specialist,
            prompt,
            json: args.json,
            model: selected_model,
            output_last_message: args
                .output_last_message
                .as_ref()
                .map(std::path::PathBuf::from),
            output_schema: args.output_schema.clone(),
            sandbox_policy: None,
            fail_on_approval: false,
            only_tools: args.only_tools,
            add_tools: args.add_tools,
            remove_tools: args.remove_tools,
            tags,
            log_group_id,
        })
        .await?;
        return Ok(code);
    }

    run_interactive_with_shutdown(move || {
        let mut app = App::new_with_initial_prompt(initial_prompt)?;
        if let Some(id) = &args.resume_session {
            app.resume_session_at_startup(id);
        }
        Ok(app)
    })
    .await
}

fn configure_ephemeral_plugins(paths: &[std::path::PathBuf]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut admitted = Vec::with_capacity(paths.len());
    for path in paths {
        if !path.is_dir() {
            anyhow::bail!("one-run plugin is not a directory: {}", path.display());
        }
        admitted
            .push(dunce::canonicalize(path).with_context(|| {
                format!("failed to resolve one-run plugin: {}", path.display())
            })?);
    }
    let value = std::env::join_paths(admitted).context("plugin path cannot be encoded")?;
    std::env::set_var("MAESTRO_EPHEMERAL_PLUGINS", value);
    Ok(())
}

/// The trust command writes global trust for the current working directory.
fn run_trust_cli(args: &[std::ffi::OsString]) -> Result<i32> {
    let action = args
        .first()
        .and_then(|a| a.to_str())
        .unwrap_or("grant")
        .to_ascii_lowercase();
    let cwd = std::env::current_dir().context("failed to resolve current directory")?;
    match action.as_str() {
        "status" | "show" => {
            let trusted = crate::config::workspace_trusted_in_global_config(&cwd);
            println!(
                "{}: {}",
                cwd.display(),
                if trusted { "trusted" } else { "untrusted" }
            );
            Ok(0)
        }
        "grant" | "on" | "yes" | "true" | "trusted" => {
            let path = crate::config::set_workspace_trust_in_global_config(&cwd, true)
                .map_err(anyhow::Error::msg)?;
            println!("Trusted {}. Wrote {}.", cwd.display(), path.display());
            println!("Reload skills with /skills reload in a running TUI, or restart.");
            Ok(0)
        }
        "revoke" | "off" | "no" | "false" | "untrusted" => {
            let path = crate::config::set_workspace_trust_in_global_config(&cwd, false)
                .map_err(anyhow::Error::msg)?;
            println!(
                "Revoked trust for {}. Wrote {}.",
                cwd.display(),
                path.display()
            );
            Ok(0)
        }
        "help" | "-h" | "--help" => {
            println!("Usage: deixic-code trust [status|grant|revoke]");
            println!("Grant or revoke project skills/plugins/hooks for the current workspace.");
            println!("Writes only ~/.composer/config.toml (repositories cannot self-trust).");
            Ok(0)
        }
        other => {
            eprintln!("Unknown trust action: {other}");
            eprintln!("Usage: deixic-code trust [status|grant|revoke]");
            Ok(2)
        }
    }
}

/// Construct and run an interactive app under the complete shutdown lifecycle.
///
/// The constructor runs only after process signal receivers are registered,
/// so both terminal initialization and any startup session restoration are
/// covered by the same orderly signal path as the main event loop.
async fn run_interactive_with_shutdown<F>(constructor: F) -> Result<i32>
where
    F: FnOnce() -> Result<App> + Send + 'static,
{
    // Register process signal streams before terminal initialization. Tokio
    // keeps its process-wide signal disposition installed, so the monitor
    // must retain active receivers from this point through final process exit.
    let mut shutdown = shutdown_signal::ShutdownMonitor::register()
        .context("Failed to register interactive shutdown signals")?;
    let app = match shutdown_signal::construct_while_monitoring(&mut shutdown, constructor).await? {
        Ok(app) => app,
        Err((signal, construction)) => {
            // `construction`'s closure cannot be cancelled now that
            // `spawn_blocking` has started it, and it may still be mid-way
            // through `terminal::init()` (raw mode / bracketed paste /
            // mouse capture applied before the global TTY handle is
            // published -- see `terminal/setup.rs::init`). Calling
            // `terminal::restore()` without first waiting for it to finish
            // would race that setup: the abandoned thread can re-enable
            // modes after our restore call returns, and nothing runs a
            // second restore before `run_cli`'s `std::process::exit` a few
            // frames up the stack. Await it first so restore always runs
            // strictly after whatever terminal setup already happened --
            // this part must stay on the async worker. A wedged constructor
            // is still bounded: `ShutdownMonitor`'s second-signal watchdog
            // runs on its own detached task and force-exits on a repeated
            // signal regardless of this await.
            let construction_result = construction.await;
            let (disable_theme_reporting, construction_error) = match construction_result {
                Ok(Ok(mut app)) => {
                    let disable_theme_reporting = app.prepare_terminal_restore();
                    drop(app);
                    (disable_theme_reporting, None)
                }
                Ok(Err(error)) => (false, Some(format!("{error:#}"))),
                Err(error) => (false, Some(error.to_string())),
            };

            // Deliberately not `eprintln!`'d inline here, mirroring
            // `run_with_shutdown` in shutdown_signal.rs: this arm runs on
            // the async worker, and `eprintln!` takes stderr's process-
            // global lock and performs a real (possibly blocking) write
            // syscall -- on a full pipe or wedged terminal that would stall
            // this worker with nothing left on a single-worker runtime to
            // poll the repeat-signal monitor the escape hatch above depends
            // on. All of this arm's diagnostics, plus the blocking terminal
            // restore itself, now run together on a blocking thread, same
            // as the run-loop shutdown branch.
            tokio::task::spawn_blocking(move || {
                eprintln!("[shutdown] received signal during app construction");
                if let Some(error) = construction_error {
                    eprintln!(
                        "[shutdown] app construction task ended unexpectedly while waiting for it: {error}"
                    );
                }
                if disable_theme_reporting {
                    let _ = crate::terminal::disable_theme_reporting();
                }
                if let Err(error) = crate::terminal::restore() {
                    eprintln!("[shutdown] failed to restore terminal: {error}");
                }
            })
            .await
            .map_err(|error| anyhow::anyhow!("shutdown cleanup task failed: {error}"))?;
            shutdown.complete_platform_cleanup();
            return Ok(signal.exit_code());
        }
    };

    // Run the application's main loop, racing it against SIGINT/SIGTERM/
    // SIGHUP (Unix) or the equivalent console events (Windows). An external
    // termination now flushes the session writer, cleans up tracked
    // background processes, and restores the terminal instead of hitting
    // the OS default disposition (immediate exit, no `Drop`, no panic
    // hook). See `shutdown_signal` for the full design, including why this
    // cannot and does not change in-app Ctrl+C-as-keypress behavior.
    //
    // `.await` suspends this function until the Future completes.
    // The app handles all user interaction, AI communication, and rendering.
    let exit_code = shutdown_signal::run_with_shutdown(app, shutdown).await?;

    // Final cleanup - the app should have already cleaned up, but this is a safety net.
    // This catches cases where the app returned without going through its normal exit path.
    // (On the signal path above this is a no-op: cleanup already ran there.)
    let remaining = cleanup_background_processes();
    if remaining > 0 {
        eprintln!("[main] Final cleanup: {remaining} background process(es)");
    }

    // Return the exit code to `run_cli`, which runs worktree teardown and
    // then terminates the process with it via `std::process::exit`.
    Ok(exit_code)
}

/// Parsed `maestro fork` arguments.
#[derive(Debug)]
enum ForkRequest {
    Help,
    Fork { session_id: Option<String> },
}

fn parse_fork_args(args: &[std::ffi::OsString]) -> std::result::Result<ForkRequest, String> {
    let mut session_id: Option<String> = None;
    for arg in args {
        let token = arg.to_string_lossy();
        if matches!(token.as_ref(), "help" | "--help" | "-h") {
            return Ok(ForkRequest::Help);
        }
        if token.starts_with('-') {
            return Err(format!("unknown fork flag: {token}"));
        }
        if session_id.is_some() {
            return Err(format!("unexpected extra argument: {token}"));
        }
        session_id = Some(token.into_owned());
    }
    Ok(ForkRequest::Fork { session_id })
}

/// `maestro fork [session-id]`: copy a session JSONL under a new session id
/// and continue it interactively, so a conversation can be branched for
/// what-if experiments. Defaults to the most recent session for the current
/// working directory.
async fn run_fork(args: &[std::ffi::OsString]) -> Result<i32> {
    let session_id = match parse_fork_args(args) {
        Ok(ForkRequest::Help) => {
            println!("Usage: deixic-code fork [session-id]");
            println!();
            println!("Copy a session (default: most recent for this directory) under a new");
            println!("session id and continue it in the TUI. The fork starts with the full");
            println!("history but records new messages independently of the source session.");
            return Ok(0);
        }
        Ok(ForkRequest::Fork { session_id }) => session_id,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };

    let cwd = std::env::current_dir()?;
    let manager = crate::session::SessionManager::new(cwd.to_string_lossy().to_string());
    let source = match session_id.as_deref() {
        Some(id) => manager
            .find_session(id)
            .map_err(|err| anyhow::anyhow!("failed to load session {id}: {err}"))?,
        None => manager
            .recent_sessions(1)?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("no session to fork for {}", cwd.display()))?,
    };
    let forked = crate::session::fork_session_file(&source.path)?;

    // Continue with the model that recorded the source session unless the
    // user already pinned one (`spawn_agent` reads MAESTRO_MODEL).
    if std::env::var_os("MAESTRO_MODEL").is_none() && !source.model.is_empty() {
        // SAFETY: the agent has not spawned worker threads yet.
        unsafe { std::env::set_var("MAESTRO_MODEL", &source.model) };
    }

    println!(
        "Forked session {} -> {} ({})",
        source.id,
        forked.id,
        forked.path.display()
    );

    run_interactive_with_shutdown(move || {
        let mut app = App::new_with_initial_prompt(None)?;
        // Forked chat must not keep the parent process goal (session-global
        // goals.json). Clear after process-start demotion.
        if let Ok(Some(goal_id)) = app.clear_goal_for_fork() {
            app.note_system_message(format!(
                "Goal {goal_id} was cleared for this forked session. Create a new goal with `/goal create` if needed."
            ));
        }
        app.resume_session_at_startup(&forked.id);
        Ok(app)
    })
    .await
}

fn raw_option_value(raw_args: &[std::ffi::OsString], names: &[&str]) -> Option<String> {
    for (index, argument) in raw_args.iter().enumerate() {
        let argument = argument.to_string_lossy();
        if names.contains(&argument.as_ref()) {
            return raw_args
                .get(index + 1)
                .map(|value| value.to_string_lossy().into_owned());
        }
        for name in names {
            if let Some(value) = argument.strip_prefix(&format!("{name}=")) {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn configure_agents_api_key(raw_args: &[std::ffi::OsString]) {
    let Some(api_key) = raw_option_value(raw_args, &["--api-key"]) else {
        return;
    };
    let model = raw_option_value(raw_args, &["--model", "-m"]);
    let provider = raw_option_value(raw_args, &["--provider"])
        .unwrap_or_else(|| infer_provider_from_model(model.as_deref().unwrap_or("")).to_string());
    let variable = configure_agents_api_key_env(&provider);
    std::env::set_var(variable, api_key);
}

fn configure_agents_api_key_env(provider: &str) -> &'static str {
    match provider {
        "openai" => "OPENAI_API_KEY",
        "google" | "vertex-ai" | "vertex" => "GOOGLE_API_KEY",
        "xai" => "XAI_API_KEY",
        "groq" => "GROQ_API_KEY",
        "cerebras" => "CEREBRAS_API_KEY",
        "openrouter" => "OPENROUTER_API_KEY",
        "deepseek" => "DEEPSEEK_API_KEY",
        "moonshot" | "kimi" => "MOONSHOT_API_KEY",
        "dashscope" | "qwen" => "DASHSCOPE_API_KEY",
        "minimax" => "MINIMAX_API_KEY",
        "zai" | "zhipu" => "ZAI_API_KEY",
        _ => "ANTHROPIC_API_KEY",
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn specialist_exec_selector_is_not_part_of_the_task() {
        for args in [
            vec!["--specialist", "product", "inspect", "journey"],
            vec!["--specialist=product", "inspect", "journey"],
        ] {
            let args = args
                .into_iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>();
            let options = parse_native_exec_options(&args);
            assert_eq!(options.specialist.as_deref(), Some("product"));
            assert_eq!(options.prompt, "inspect journey");
        }
        let args = ["--", "--specialist", "product"].map(std::ffi::OsString::from);
        assert!(parse_native_exec_options(&args).specialist.is_none());
        assert!(Args::try_parse_from(["maestro", "--specialist", "product", "hello"]).is_err());
        assert!(
            Args::try_parse_from(["maestro", "--print", "--specialist", "product", "hello"])
                .is_ok()
        );
        assert_eq!(
            native_utility_tokens(&["specialists".into(), "list".into()]).unwrap(),
            vec!["specialists", "list"]
        );
    }

    #[test]
    fn worktree_cleanup_follows_the_actual_session_mode() {
        for (args, keep) in [
            (vec!["maestro", "-w", "task"], true),
            (vec!["maestro", "fork", "session", "-w", "task"], true),
            (vec!["maestro", "--headless", "-w", "task"], true),
            (vec!["maestro", "exec", "-w", "task", "hello"], false),
            (vec!["maestro", "print", "-w", "task", "hello"], false),
            (vec!["maestro", "-p", "-w", "task", "hello"], false),
        ] {
            let raw: Vec<std::ffi::OsString> = args.iter().map(Into::into).collect();
            assert_eq!(super::keep_session_worktree(&raw), keep, "{args:?}");
        }
    }

    use super::*;
    use clap::CommandFactory;

    #[test]
    fn args_use_deixic_code_branding() {
        let command = Args::command();
        assert_eq!(command.get_name(), "deixic-code");
        assert_eq!(
            command.get_about().map(|about| about.to_string()),
            Some("Native terminal interface for Deixic Code".to_string())
        );
    }

    #[test]
    fn print_flag_parses() {
        use clap::Parser;
        let args = Args::try_parse_from(["maestro-tui", "--print", "--json", "hello"])
            .expect("parse print");
        assert!(args.print);
        assert!(args.json);
        assert_eq!(args.prompt, vec!["hello"]);

        let tools = Args::try_parse_from(["maestro-tui", "--list-tools", "--json"])
            .expect("parse top-level tool listing");
        assert!(tools.list_tools);
        assert!(!tools.print);
    }

    #[test]
    fn one_run_tools_and_lineage_are_parsed_without_entering_the_prompt() {
        let args = [
            "--only-tools=read,bash",
            "--add-tools",
            "write",
            "--remove-tools=write",
            "--tag",
            "reviewed",
            "--tag={\"name\":\"lane\",\"value\":\"release\"}",
            "--log-group-id",
            "deploy:42",
            "ship",
        ]
        .into_iter()
        .map(std::ffi::OsString::from)
        .collect::<Vec<_>>();
        let options = parse_native_exec_options(&args);
        assert_eq!(options.only_tools, vec!["read,bash"]);
        assert_eq!(options.add_tools, vec!["write"]);
        assert_eq!(options.remove_tools, vec!["write"]);
        assert_eq!(options.tags.len(), 2);
        assert_eq!(options.log_group_id.as_deref(), Some("deploy:42"));
        assert_eq!(options.prompt, "ship");

        let tags = parse_run_tags(&options.tags).expect("valid run tags");
        assert_eq!(tags[0], serde_json::json!("reviewed"));
        assert_eq!(tags[1]["name"], "lane");
        assert!(parse_run_tags(&["{\"value\":1}".to_string()]).is_err());
        assert_eq!(
            parse_log_group_id(options.log_group_id).unwrap().as_deref(),
            Some("deploy:42")
        );
        assert!(parse_log_group_id(Some("bad group".to_string())).is_err());
    }

    #[test]
    fn one_run_plugin_flag_is_repeatable_and_not_prompt_text() {
        use clap::Parser;
        let args = Args::try_parse_from([
            "maestro-tui",
            "--plugin",
            "./first",
            "--plugin=./second",
            "hello",
        ])
        .expect("parse plugin paths");
        assert_eq!(
            args.plugins,
            vec![
                std::path::PathBuf::from("./first"),
                std::path::PathBuf::from("./second")
            ]
        );
        assert_eq!(args.prompt, vec!["hello"]);

        let exec = parse_native_exec_options(
            &["--plugin", "./first", "hello"]
                .into_iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>(),
        );
        assert_eq!(exec.plugins, vec![std::path::PathBuf::from("./first")]);
        assert_eq!(exec.prompt, "hello");
    }

    #[test]
    fn worktree_flag_parses() {
        use clap::Parser;
        let args = Args::try_parse_from(["maestro-tui", "--worktree", "feat-x", "do", "it"])
            .expect("parse worktree");
        assert_eq!(args.worktree.as_deref(), Some("feat-x"));
        assert_eq!(args.prompt, vec!["do", "it"]);

        let args = Args::try_parse_from(["maestro-tui", "-w", "feat-x"]).expect("parse -w");
        assert_eq!(args.worktree.as_deref(), Some("feat-x"));

        let args =
            Args::try_parse_from(["maestro-tui", "--worktree=feat-x"]).expect("parse inline");
        assert_eq!(args.worktree.as_deref(), Some("feat-x"));

        // Droid-style: the worktree name is required.
        assert!(Args::try_parse_from(["maestro-tui", "--worktree"]).is_err());
    }

    #[test]
    fn trailing_prompt_args_are_captured() {
        use clap::Parser;
        let args = Args::try_parse_from(["maestro-tui", "--model", "gpt-5.1", "fix", "the", "bug"])
            .expect("parse trailing prompt");
        assert_eq!(args.model.as_deref(), Some("gpt-5.1"));
        assert_eq!(args.prompt, vec!["fix", "the", "bug"]);
        assert_eq!(args.prompt.join(" "), "fix the bug");
    }

    #[test]
    fn fork_args_parse_session_id_and_help() {
        let empty: Vec<std::ffi::OsString> = Vec::new();
        assert!(matches!(
            parse_fork_args(&empty),
            Ok(ForkRequest::Fork { session_id: None })
        ));

        let with_id = [std::ffi::OsString::from("abc123")];
        match parse_fork_args(&with_id) {
            Ok(ForkRequest::Fork { session_id }) => {
                assert_eq!(session_id.as_deref(), Some("abc123"));
            }
            other => panic!("expected fork request, got {other:?}"),
        }

        let help = [std::ffi::OsString::from("--help")];
        assert!(matches!(parse_fork_args(&help), Ok(ForkRequest::Help)));

        let unknown_flag = [std::ffi::OsString::from("--json")];
        assert!(parse_fork_args(&unknown_flag).is_err());

        let extra = [
            std::ffi::OsString::from("abc123"),
            std::ffi::OsString::from("def456"),
        ];
        assert!(parse_fork_args(&extra).is_err());
    }

    #[test]
    fn fork_is_not_dispatched_as_a_utility_command() {
        // `maestro fork` must reach the agent path so it can launch the TUI;
        // utility dispatch exits after running and would never resume.
        let args = ["fork", "abc123"]
            .into_iter()
            .map(std::ffi::OsString::from)
            .collect::<Vec<_>>();
        assert_eq!(native_utility_tokens(&args), None);
    }

    #[test]
    fn run_graphics_are_dispatched_as_utility_commands() {
        for subcommand in ["timeline", "context"] {
            let args = ["run", subcommand, "session-1"]
                .into_iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>();
            assert_eq!(
                native_utility_tokens(&args),
                Some(vec!["run".into(), subcommand.into(), "session-1".into(),])
            );
        }
    }

    #[test]
    fn native_utility_tokens_forward_command_argv() {
        let args = ["stats", "month", "--json", "--session", "session-1"]
            .into_iter()
            .map(std::ffi::OsString::from)
            .collect::<Vec<_>>();
        assert_eq!(
            native_utility_tokens(&args),
            Some(vec![
                "stats".into(),
                "month".into(),
                "--json".into(),
                "--session".into(),
                "session-1".into(),
            ])
        );
    }

    #[test]
    fn native_utility_tokens_preserve_relevant_global_options() {
        let args = [
            "--profile",
            "local",
            "--provider",
            "openai",
            "models",
            "providers",
        ]
        .into_iter()
        .map(std::ffi::OsString::from)
        .collect::<Vec<_>>();
        assert_eq!(
            native_utility_tokens(&args),
            Some(vec![
                "models".into(),
                "providers".into(),
                "--provider".into(),
                "openai".into(),
            ])
        );
    }

    #[test]
    fn native_utility_tokens_forward_force_before_agents() {
        let args = ["--force", "agents", "init", "/tmp/project"]
            .into_iter()
            .map(std::ffi::OsString::from)
            .collect::<Vec<_>>();

        assert_eq!(
            native_utility_tokens(&args),
            Some(vec![
                "agents".to_string(),
                "init".to_string(),
                "/tmp/project".to_string(),
                "--force".to_string(),
            ])
        );
    }

    #[test]
    fn native_utility_tokens_skip_all_value_taking_cli_globals() {
        for flag in [
            "--output-schema",
            "--output-last-message",
            "--output-dir",
            "--junit",
            "--replay",
            "--record-scenario",
        ] {
            let args = [flag, "ignored-value", "modes", "describe", "high"]
                .into_iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>();
            assert_eq!(
                native_utility_tokens(&args),
                Some(vec!["modes".into(), "describe".into(), "high".into()]),
                "scanner did not consume {flag}'s value"
            );
        }
    }

    #[test]
    fn native_utility_tokens_dispatch_modes_with_provider() {
        let args = [
            "--provider",
            "openai",
            "modes",
            "describe",
            "high",
            "--json",
        ]
        .into_iter()
        .map(std::ffi::OsString::from)
        .collect::<Vec<_>>();
        assert_eq!(
            native_utility_tokens(&args),
            Some(vec![
                "modes".into(),
                "describe".into(),
                "high".into(),
                "--json".into(),
                "--provider".into(),
                "openai".into(),
            ])
        );
    }

    #[test]
    fn native_utility_tokens_consume_named_worktree_before_modes() {
        let args = ["--worktree", "feature-branch", "modes", "describe", "high"]
            .into_iter()
            .map(std::ffi::OsString::from)
            .collect::<Vec<_>>();
        assert_eq!(
            native_utility_tokens(&args),
            Some(vec!["modes".into(), "describe".into(), "high".into(),])
        );

        let short = ["-w", "feature-branch", "modes", "describe", "high"]
            .into_iter()
            .map(std::ffi::OsString::from)
            .collect::<Vec<_>>();
        assert_eq!(
            native_utility_tokens(&short),
            Some(vec!["modes".into(), "describe".into(), "high".into(),])
        );
    }

    #[test]
    fn native_exec_options_consume_worktree_flag() {
        let args = ["-w", "feat-x", "fix", "the", "bug"].map(std::ffi::OsString::from);
        let options = parse_native_exec_options(&args);
        assert_eq!(options.prompt, "fix the bug");

        let inline = ["--worktree=feat-x", "fix", "the", "bug"].map(std::ffi::OsString::from);
        let options = parse_native_exec_options(&inline);
        assert_eq!(options.prompt, "fix the bug");

        let attached = ["-wfeat-x", "fix", "the", "bug"].map(std::ffi::OsString::from);
        let options = parse_native_exec_options(&attached);
        assert_eq!(options.prompt, "fix the bug");
    }

    #[test]
    fn native_exec_options_accept_prompt_stdin() {
        let args = ["--approval-mode", "fail", "--prompt-stdin"].map(std::ffi::OsString::from);
        let options = parse_native_exec_options(&args);
        assert!(options.prompt_stdin);
        assert!(options.prompt.is_empty());
    }

    #[test]
    fn native_utility_tokens_dispatch_plugins() {
        let args = ["plugins", "list", "--json"]
            .into_iter()
            .map(std::ffi::OsString::from)
            .collect::<Vec<_>>();
        assert_eq!(
            native_utility_tokens(&args),
            Some(vec!["plugins".into(), "list".into(), "--json".into(),])
        );
        let alias = ["plugin", "info", "team-tools"]
            .into_iter()
            .map(std::ffi::OsString::from)
            .collect::<Vec<_>>();
        assert_eq!(
            native_utility_tokens(&alias),
            Some(vec!["plugin".into(), "info".into(), "team-tools".into(),])
        );
    }

    #[test]
    fn native_utility_tokens_dispatch_connections() {
        let args = ["connections", "list", "--json"]
            .into_iter()
            .map(std::ffi::OsString::from)
            .collect::<Vec<_>>();
        assert_eq!(
            native_utility_tokens(&args),
            Some(vec!["connections".into(), "list".into(), "--json".into()])
        );
    }

    #[test]
    fn native_utility_tokens_dispatch_orb_console() {
        let args = ["computer", "status", "task-1"]
            .into_iter()
            .map(std::ffi::OsString::from)
            .collect::<Vec<_>>();
        assert_eq!(
            native_utility_tokens(&args),
            Some(vec!["computer".into(), "status".into(), "task-1".into()])
        );
    }

    #[test]
    fn native_utility_tokens_do_not_scan_prompt_text() {
        let args = ["write", "a", "models", "command"]
            .into_iter()
            .map(std::ffi::OsString::from)
            .collect::<Vec<_>>();
        assert_eq!(native_utility_tokens(&args), None);
    }

    #[test]
    fn native_profile_accepts_split_and_inline_forms() {
        let split = ["--profile", "local", "skill", "list"]
            .into_iter()
            .map(std::ffi::OsString::from)
            .collect::<Vec<_>>();
        let inline = ["--profile=review", "skill", "list"]
            .into_iter()
            .map(std::ffi::OsString::from)
            .collect::<Vec<_>>();
        assert_eq!(native_profile(&split).as_deref(), Some("local"));
        assert_eq!(native_profile(&inline).as_deref(), Some("review"));
    }

    #[test]
    fn native_profile_uses_config_fallback_but_explicit_profile_wins() {
        let fallback = ["--config", "profile=review", "skill", "list"]
            .into_iter()
            .map(std::ffi::OsString::from)
            .collect::<Vec<_>>();
        let explicit = [
            "--config=profile=review",
            "--profile",
            "release",
            "skill",
            "list",
        ]
        .into_iter()
        .map(std::ffi::OsString::from)
        .collect::<Vec<_>>();
        assert_eq!(native_profile(&fallback).as_deref(), Some("review"));
        assert_eq!(native_profile(&explicit).as_deref(), Some("release"));
    }

    #[test]
    fn native_config_overrides_preserve_all_cli_entries() {
        let args = [
            "--config",
            "profile=review",
            "--config=projects.\"/tmp/repo\".trust_level=trusted",
            "skill",
            "list",
        ]
        .map(std::ffi::OsString::from);
        assert_eq!(
            native_config_overrides(&args),
            [
                "profile=review",
                "projects.\"/tmp/repo\".trust_level=trusted",
            ]
        );
    }

    #[test]
    fn codex_models_infer_openai_provider() {
        assert_eq!(infer_provider_from_model("gpt-5.1-codex-max"), "openai");
        assert_eq!(infer_provider_from_model("codex-mini-latest"), "openai");
    }

    #[test]
    fn vertex_models_infer_vertex_provider() {
        assert_eq!(
            infer_provider_from_model("vertex-ai/gemini-2.5-pro"),
            "vertex-ai"
        );
        assert_eq!(
            infer_provider_from_model("vertex/gemini-2.5-pro"),
            "vertex-ai"
        );
        assert_eq!(
            native_exec_model(Some("vertex-ai"), None)
                .expect("vertex default")
                .as_deref(),
            Some("vertex-ai/gemini-2.5-pro")
        );
        assert_eq!(provider_api_key_env("vertex-ai"), "GOOGLE_API_KEY");
        assert_eq!(provider_api_key_env("vertex"), "GOOGLE_API_KEY");
        assert_eq!(configure_agents_api_key_env("vertex-ai"), "GOOGLE_API_KEY");
        assert_eq!(configure_agents_api_key_env("vertex"), "GOOGLE_API_KEY");
    }

    #[test]
    fn chinese_bare_models_infer_direct_provider() {
        assert_eq!(infer_provider_from_model("deepseek-chat"), "deepseek");
        assert_eq!(infer_provider_from_model("kimi-k2.6"), "moonshot");
        assert_eq!(infer_provider_from_model("qwen3-max"), "dashscope");
        assert_eq!(infer_provider_from_model("MiniMax-M2"), "minimax");
        assert_eq!(infer_provider_from_model("glm-4.6"), "zai");
    }

    #[test]
    fn explicit_local_provider_qualifies_ambiguous_bare_model_names() {
        assert_eq!(
            native_exec_model(Some("ollama"), Some("qwen3.6:27b"))
                .expect("explicit Ollama model")
                .as_deref(),
            Some("ollama/qwen3.6:27b")
        );
        assert_eq!(
            native_exec_model(Some("lmstudio"), Some("local-model"))
                .expect("explicit LM Studio model")
                .as_deref(),
            Some("lmstudio/local-model")
        );
    }

    #[test]
    fn explicit_provider_alias_does_not_double_qualify_an_equivalent_route() {
        for (provider, model, expected) in [
            ("llama.cpp", "llamacpp/Qwen3.8-27B", "llamacpp/Qwen3.8-27B"),
            ("llamacpp", "llama.cpp/Qwen3.8-27B", "llama.cpp/Qwen3.8-27B"),
            ("lm-studio", "lmstudio/local-model", "lmstudio/local-model"),
            (
                "vertex",
                "vertex-ai/gemini-2.5-pro",
                "vertex-ai/gemini-2.5-pro",
            ),
        ] {
            assert_eq!(
                native_exec_model(Some(provider), Some(model))
                    .expect("explicit aliased model")
                    .as_deref(),
                Some(expected)
            );
        }
    }

    #[test]
    fn arbitrary_local_provider_requires_an_explicit_model() {
        for provider in ["lmstudio", "lm-studio", "ollama"] {
            let error = native_exec_model(Some(provider), None).expect_err("model is required");
            assert!(error.contains("requires --model"), "{error}");
        }
        assert_eq!(
            native_exec_model(Some("llamacpp"), None)
                .expect("llama.cpp has a built-in model")
                .as_deref(),
            Some("llamacpp/Qwen3.8-27B")
        );
    }

    #[test]
    fn groq_hosted_bare_models_infer_groq() {
        // Key inference must mirror AiProvider::from_model so --api-key writes the
        // env var the actually-selected client reads.
        assert_eq!(
            infer_provider_from_model("deepseek-r1-distill-llama-70b"),
            "groq"
        );
        assert_eq!(infer_provider_from_model("qwen-2.5-coder-32b"), "groq");
        assert_eq!(infer_provider_from_model("llama-3.3-70b-versatile"), "groq");
    }

    #[test]
    fn unknown_models_default_to_openai() {
        assert_eq!(infer_provider_from_model("unknown-model"), "openai");
        assert_eq!(infer_provider_from_model(""), "openai");
    }

    #[test]
    fn native_exec_consumes_compatibility_option_values() {
        let args = [
            "--full-auto",
            "--sandbox",
            "docker",
            "--approval-mode",
            "fail",
            "--provider",
            "openai-codex",
            "--api-key",
            "secret",
            "--config",
            "profile=sandbox",
            "--profile=review",
            "--json",
            "fix the issue",
        ]
        .map(std::ffi::OsString::from);
        let options = parse_native_exec_options(&args);
        assert_eq!(options.sandbox.as_deref(), Some("docker"));
        assert_eq!(options.approval_mode.as_deref(), Some("fail"));
        assert_eq!(options.provider.as_deref(), Some("openai-codex"));
        assert_eq!(options.api_key.as_deref(), Some("secret"));
        assert!(options.json);
        assert_eq!(options.prompt, "fix the issue");
        assert!(native_exec_sandbox_policy(Some("docker")).is_err());
        assert!(matches!(
            native_exec_sandbox_policy(Some("workspace-write")),
            Ok(Some(SandboxPolicy::WorkspaceWrite { .. }))
        ));
        assert_eq!(
            native_exec_model(Some("anthropic"), None)
                .expect("anthropic default")
                .as_deref(),
            Some("anthropic/claude-sonnet-4-6")
        );
        assert_eq!(
            native_exec_model(Some("evalops"), Some("gpt-4o-mini"))
                .expect("managed explicit model")
                .as_deref(),
            Some("evalops/gpt-4o-mini")
        );
    }
}
