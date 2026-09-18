use anyhow::Result;
use maestro::{Command, classify};
use maestro_runtime_gateway::{RuntimeGatewayConfig, serve};

// Process hardening must run before `main` — and therefore before the Tokio
// runtime spawns worker threads — so the prctl/setrlimit calls and the
// environment sanitization stay race-free. See
// `maestro_tui::process_hardening` for details and the opt-out env var.
#[ctor::ctor(unsafe)]
fn pre_main_hardening() {
    maestro_tui::process_hardening::pre_main_hardening();
}

const HELP: &str = "Deixic Code\n\nUsage:\n  deixic-code [options] [prompt]\n  deixic-code exec <prompt>\n  deixic-code -w <name> [prompt]\n  deixic-code doctor [--json] [--live] [--model <provider/model>]\n  deixic-code setup [--json] [--live] [--model <provider/model>]\n  deixic-code --headless\n  deixic-code serve [--port <port>]\n  deixic-code hosted-runner [options]\n\nCommon commands:\n  deixic-code setup       Check authentication and show the next setup step\n  deixic-code config      Inspect or change Deixic Code configuration\n  deixic-code experiments Review or change tool experiment participation\n  deixic-code models      List available models and providers\n  deixic-code sessions    List, export, or import saved sessions\n  deixic-code scenario    Run or inspect scripted scenarios\n  deixic-code run         Inspect or replay a saved session\n\nAdvanced commands:\n  deixic-code diagnostics Project compiler JSONL, optionally against a prior observation\n  deixic-code codex       Manage Codex authentication and app-server access\n  deixic-code mcp         Configure MCP servers\n  deixic-code plugins     Inspect and manage plugins\n  deixic-code connections Manage API keys and delegated accounts\n  deixic-code init        Register a Deixic agent integration\n\nCompatibility:\n  maestro remains available as an alias; MAESTRO_* variables and .maestro paths are unchanged.\n\nOptions:\n  -w, --worktree <name>  Run the session in a new git worktree at ../<repo>-wt-<name>\n                         on a new branch; interactive worktrees and branches are kept\n                         exec removes only clean worktrees with no new commits\n\nThe product runtime is native Rust; no Node.js or Bun runtime is required.";
const VERSION: &str = concat!("deixic-code ", env!("CARGO_PKG_VERSION"));

fn sync_command_output(command: &Command) -> Option<&'static str> {
    match command {
        Command::Help => Some(maestro_tui::localization::cli_locale().translate(HELP)),
        Command::Version => Some(VERSION),
        _ => None,
    }
}

fn main() -> Result<()> {
    let raw_args = std::env::args_os().collect::<Vec<_>>();
    let command = classify(raw_args.iter().skip(1).cloned()).map_err(anyhow::Error::msg)?;

    if let Some(output) = sync_command_output(&command) {
        println!("{output}");
        return Ok(());
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        // Keep parity with the direct native entry point. The
        // interactive key-dispatch future exceeds Tokio's 2 MiB default
        // worker stack in debug builds.
        .thread_stack_size(8 * 1024 * 1024)
        .build()?
        .block_on(async move {
            match command {
                Command::Serve { port } => {
                    if let Some(port) = port {
                        std::env::set_var("PORT", port.to_string());
                    }
                    serve(RuntimeGatewayConfig::from_env()).await
                }
                Command::Forward => maestro_tui::run_cli(raw_args).await,
                Command::Help | Command::Version => unreachable!("handled before runtime startup"),
            }
        })
}

#[cfg(test)]
mod tests {
    use super::{HELP, VERSION, sync_command_output};
    use maestro::Command;

    #[test]
    fn help_and_version_are_handled_without_async_runtime() {
        assert_eq!(sync_command_output(&Command::Help), Some(HELP));
        assert_eq!(sync_command_output(&Command::Version), Some(VERSION));
    }
}
