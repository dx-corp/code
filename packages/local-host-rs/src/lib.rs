//! Local execution host shared by the native gateway and terminal application.
//!
//! Owns local tools, hooks, credentials, configuration, and session integration.
//! The native actor remains in maestro-runtime; terminal application state and
//! event rendering remain in maestro-tui.

pub use maestro_ai as ai;
pub use maestro_codex::codex_app_server;
pub use maestro_codex::codex_session;
pub use maestro_sandbox as sandbox;
pub use maestro_session::checkpoints;
pub use maestro_session::fs_atomic;
pub use maestro_workspace::files;
pub use maestro_workspace::git;
pub use maestro_workspace::worktree;

pub mod agent;
pub mod agents_cli;
pub mod bug_report;
pub mod code_authority;
pub mod codex_auth;
pub mod codex_cli;
pub mod codex_identity;
pub mod color_utils;
pub mod config;
pub mod credential_mode;
pub mod diagnostics;
pub mod doctor;
pub mod embedding;
pub mod evalops_cli;
pub mod experiments;
pub mod goal;
pub mod harness;
pub mod headless;
pub mod headless_server;
pub mod hooks;
pub mod init_cli;
pub mod local_models;
pub mod localization;
pub mod lsp;
pub mod mailbox;
pub mod managed_setup;
pub mod mcp;
pub mod mission_cli;
pub mod mission_readiness;
pub mod model_catalog;
pub mod model_dynamics;
mod native_credentials;
pub mod openai_cli;
pub mod orb_connection;
pub mod output_sanitize;
pub mod path_utils;
pub mod pending_decisions;
pub mod plan_mode;
pub mod plugins;
pub mod rlm;
pub mod safety;
pub mod sandbox_policy;
pub mod service_connections;
pub mod session;
pub mod skill_cli;
pub mod skill_package_cli;
pub mod skills;
pub mod state;
pub mod telemetry;
pub mod terminal_info;
pub mod tool_output;
pub mod tools;
pub mod transcript;
pub mod ui_prefs;
pub mod video;
pub mod workflow_runtime;
pub use sandbox::SandboxPolicy;

pub mod hosted_runner;
pub mod hosted_runner_cli;
pub mod hosted_runner_conformance;
pub mod subagents;
