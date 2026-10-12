//! Command-line surfaces split out of `maestro-tui`: update, memory, search,
//! plugins, MCP config, config, context, ACP, remote, cloud and thread commands.
//!
//! Edits here recompile this crate instead of the terminal application.

pub(crate) use maestro_execpolicy as execpolicy;
pub(crate) use maestro_local_host::{
    config, headless, init_cli, local_models, localization, mcp, model_catalog, orb_connection,
    output_sanitize, path_utils, plugins, session, skill_cli, tools,
};

pub mod acp_cli;
pub mod cloud_cli;
pub mod config_cli;
pub mod context_cli;
pub mod deixic_operating_client;
pub mod evidence;
pub mod import_claude_cli;
pub mod mcp_catalog;
pub mod mcp_config_cli;
pub mod memory_cli;
pub mod plugins_cli;
pub mod remote_attach;
pub mod remote_cli;
pub mod search_cli;
pub mod thread_cli;
pub mod update_cli;
