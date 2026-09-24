use super::*;
use crate::exec_commands::{self, ExecOutput};
use std::path::PathBuf;

/// Max script source bytes embedded in the transcript message.
const MAX_SOURCE_DISPLAY_BYTES: usize = 8 * 1024;

/// Result of a finished executable slash command run, delivered from the
/// worker thread to the event loop via `App::exec_command_rx`.
pub(super) struct ExecCommandOutcome {
    name: String,
    path: PathBuf,
    source: String,
    result: Result<ExecOutput, String>,
}

impl ExecCommandOutcome {
    /// Render the script source plus captured output as a transcript message
    /// so the user can see exactly what ran and what it produced.
    fn format_transcript(&self) -> String {
        let mut msg = maestro_ui::localization::format(
            "## /{0} (executable command)\n\n",
            std::slice::from_ref(&(self.name)),
        );
        msg.push_str(&format!("**Script:** `{}`\n\n", self.path.display()));
        msg.push_str("```sh\n");
        msg.push_str(self.source.trim_end());
        msg.push_str("\n```\n\n");
        match &self.result {
            Ok(output) => {
                let exit = output.exit_code.map_or_else(
                    || maestro_ui::localization::tr("none").to_string(),
                    |code| code.to_string(),
                );
                msg.push_str(&maestro_ui::localization::format(
                    "**Exit code:** {0}\n",
                    std::slice::from_ref(&(exit)),
                ));
                if output.timed_out {
                    msg.push_str(maestro_ui::localization::tr(
                        "**Killed:** exceeded the 120s timeout\n",
                    ));
                }
                msg.push('\n');
                if output.stdout.trim().is_empty() && output.stderr.trim().is_empty() {
                    msg.push_str(maestro_ui::localization::tr("*(no output)*\n"));
                } else {
                    if !output.stdout.trim().is_empty() {
                        msg.push_str("**Output:**\n```\n");
                        msg.push_str(output.stdout.trim_end());
                        msg.push_str("\n```\n");
                    }
                    if !output.stderr.trim().is_empty() {
                        msg.push_str("**Stderr:**\n```\n");
                        msg.push_str(output.stderr.trim_end());
                        msg.push_str("\n```\n");
                    }
                }
                if output.truncated {
                    msg.push_str(&maestro_ui::localization::format(
                        "\n*(output truncated at {0}KB)*\n",
                        &[(exec_commands::MAX_OUTPUT_BYTES / 1024).to_string()],
                    ));
                }
            }
            Err(err) => {
                msg.push_str(&maestro_ui::localization::format(
                    "**Error:** {0}\n",
                    std::slice::from_ref(err),
                ));
            }
        }
        msg
    }
}

/// One-time warning for executable commands that lost a name collision.
pub(super) fn exec_collision_warning(skipped: &[String]) -> String {
    let names = skipped
        .iter()
        .map(|name| format!("/{name}"))
        .collect::<Vec<_>>()
        .join(", ");
    maestro_ui::localization::format(
        "Executable command(s) skipped because the name is already taken by a built-in or extension command: {0}. Built-in commands always win; rename the script to use it.",
        std::slice::from_ref(&(names)),
    )
}

impl App {
    /// Run an executable slash command on a worker thread (Droid-style):
    /// argv is the tokenized text after the command name, the script inherits
    /// the session environment, and the source + output are posted to the
    /// transcript when the run finishes (see `poll_exec_commands`).
    pub(super) fn handle_invoke_exec_command(&mut self, name: &str, raw_args: &str) {
        let Some(exec) = self
            .exec_commands
            .iter()
            .find(|cmd| cmd.name.eq_ignore_ascii_case(name))
            .cloned()
        else {
            self.state.error = Some(
                self.state
                    .locale
                    .format("Executable command '{0}' not found", &[(name).to_string()]),
            );
            return;
        };

        let args = exec_commands::tokenize_args(raw_args);
        let cwd = self
            .state
            .cwd
            .clone()
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let tx = self.exec_command_tx.clone();
        let wake = self.loop_wake.clone();
        self.state.status = Some(format!("Running /{name} ..."));

        let outcome_name = name.to_string();
        let spawned = std::thread::Builder::new()
            .name(format!("maestro-exec-cmd-{name}"))
            .spawn(move || {
                let source = exec_commands::read_source(&exec.path, MAX_SOURCE_DISPLAY_BYTES);
                let result =
                    exec_commands::run(&exec.path, &args, &cwd).map_err(|err| err.to_string());
                let _ = tx.send(ExecCommandOutcome {
                    name: outcome_name,
                    path: exec.path.clone(),
                    source,
                    result,
                });
                wake.signal();
            });
        if let Err(err) = spawned {
            self.state.error = Some(self.state.locale.format(
                "Failed to start /{0}: {1}",
                &[(name).to_string(), (err).to_string()],
            ));
        }
    }

    /// Apply finished executable command runs as they arrive (non-blocking).
    /// Returns true when at least one outcome was applied.
    pub(super) fn poll_exec_commands(&mut self) -> bool {
        let mut applied = false;
        while let Ok(outcome) = self.exec_command_rx.try_recv() {
            applied = true;
            self.state.status = Some(match &outcome.result {
                Ok(output) => {
                    let exit = output
                        .exit_code
                        .map_or_else(|| "?".to_string(), |code| code.to_string());
                    format!("/{} finished (exit {exit})", outcome.name)
                }
                Err(_) => format!("/{} failed", outcome.name),
            });
            self.state.add_system_message(outcome.format_transcript());
        }
        applied
    }
}
