//! Hook system for Maestro
//!
//! Provides a comprehensive hook system for intercepting and modifying agent
//! behavior. Supports multiple execution backends:
//!
//! - **Native Rust hooks** - Trait-based, zero overhead
//! - **Lua scripts** - Lightweight scripting for custom logic
//! - **WASM plugins** - Sandboxed, polyglot plugins
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────┐     PreToolUse      ┌─────────────────────────────┐
//! │   Agent     │ ────────────────────>│       Hook System           │
//! │             │                      │                             │
//! │             │<──────────────────── │  ┌─────────┐ ┌─────────┐   │
//! └─────────────┘   Allow/Block/Modify │  │  Rust   │ │   Lua   │   │
//!                                      │  │ Traits  │ │ Scripts │   │
//!                                      │  └─────────┘ └─────────┘   │
//!                                      │  ┌─────────┐                │
//!                                      │  │  WASM   │                │
//!                                      │  │ Plugins │                │
//!                                      │  └─────────┘                │
//!                                      └─────────────────────────────┘
//! ```
//!
//! # Hook Types
//!
//! - **PreToolUse**: Called before a tool executes, can block or modify input
//! - **PostToolUse**: Called after tool execution, can modify output
//! - **SessionStart/End**: Called at session lifecycle boundaries
//! - **Overflow**: Called when context overflow is detected
//!
//! # Configuration
//!
//! Hooks can be configured via TOML files:
//! - `~/.composer/hooks.toml` - Global hooks
//! - `.composer/hooks.toml` - Workspace hooks
//!
//! ```toml
//! [settings]
//! enabled = true
//! timeout_ms = 30000
//!
//! [[hooks]]
//! event = "PreToolUse"
//! tools = ["Bash"]
//! lua = """
//! if tool_input.command:match("rm %-rf") then
//!     return { block = true, reason = "Dangerous command" }
//! end
//! """
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use maestro_local_host::hooks::{HookRegistry, PreToolUseHook, HookResult};
//!
//! struct LoggingHook;
//!
//! impl PreToolUseHook for LoggingHook {
//!     fn on_pre_tool_use(&self, input: &PreToolUseInput) -> HookResult {
//!         println!("Tool: {} called", input.tool_name);
//!         HookResult::Continue
//!     }
//! }
//!
//! let mut registry = HookRegistry::new();
//! registry.register_pre_tool_use(Arc::new(LoggingHook));
//! ```

mod claude_code_import;
mod command_output;
mod config;
pub(crate) mod context;
mod hot_reload;
mod integration;
mod lua;
mod matcher;
mod notify;
mod overflow;
mod registry;
mod types;
mod wasm;

pub use claude_code_import::*;
pub use config::*;
pub use context::*;
pub use hot_reload::*;
pub use integration::*;
pub use lua::*;
pub use matcher::*;
pub use notify::*;
pub use overflow::*;
pub use registry::*;
pub use types::*;
pub use wasm::*;
