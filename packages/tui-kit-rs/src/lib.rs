//! Terminal, theme, markdown, history, usage and other self-contained pieces of the
//! terminal application, split out of `maestro-tui` so that edits to the
//! application recompile the application rather than these modules.

pub(crate) use maestro_ai as ai;
pub(crate) use maestro_codex::codex_app_server;
pub(crate) use maestro_local_host::{
    codex_auth, color_utils, config, credential_mode, doctor, headless, init_cli, localization,
    managed_setup, model_catalog, output_sanitize, path_utils, safety, service_connections,
    session, telemetry, tools,
};
pub(crate) use maestro_sandbox as sandbox;
pub(crate) use maestro_swarm as swarm;
pub(crate) use maestro_ui::{key_hints, render_utils, sync_output, wrapping};

pub mod ansi_commands;
pub mod bridge;
pub mod clipboard;
pub mod config_watcher;
pub mod crash_handler;
pub mod dex_actions;
pub mod effects;
pub mod file_mentions;
pub mod history;
pub mod hyperlink;
pub mod inline_scroll;
pub mod keybindings;
pub mod loop_wake;
pub mod magic_trace;
pub mod markdown;
pub mod markdown_stream;
pub mod mode_cli;
pub mod model_monitor;
pub mod notifications;
pub mod onboarding_checks;
pub mod orb_cli;
pub mod painter_cli;
pub mod palette;
pub mod palette_resource;
pub mod platform_provider_refs;
pub mod process_hardening;
pub mod prompts;
pub mod protocol;
pub mod shimmer;
pub mod subscription_usage;
pub mod syntax;
pub mod terminal;
pub mod themes;
pub mod tool_summary;
pub mod tooltips;
pub mod turn_summary;
pub mod ui_prefs;
pub mod usage;
