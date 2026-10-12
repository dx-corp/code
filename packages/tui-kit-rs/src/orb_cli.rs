//! Native hosted Computer task console.
//!
//! This is the command-line sibling of `/computer` in the TUI. Both surfaces call
//! the same [`ToolExecutor`] operation, so they share managed authentication,
//! durable task records, typed delegation projections, and failure semantics.

use std::path::PathBuf;

use anyhow::{Result, bail};

use crate::tools::ToolExecutor;
use crate::tools::orb_delegation::OrbConsoleAction;

/// Run `maestro computer ...` (`maestro orb` remains a compatibility alias).
pub async fn run_orb(args: &[String]) -> Result<i32> {
    maestro_local_host::safety::require_vendor_network()?;
    let mut args = args.iter().map(String::as_str).collect::<Vec<_>>();
    let json = args.contains(&"--json");
    args.retain(|arg| *arg != "--json");
    if args.is_empty() || args[0] == "help" || args[0] == "--help" || args[0] == "-h" {
        print_usage();
        return Ok(0);
    }
    let action = parse_action(&args)?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let executor = ToolExecutor::new(cwd.to_string_lossy().into_owned());
    let result = executor.run_orb_console(action).await;
    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else if result.success {
        if !result.output.is_empty() {
            println!("{}", result.output);
        }
    } else {
        eprintln!(
            "{}",
            result
                .error
                .as_deref()
                .unwrap_or("Hosted Computer operation failed")
        );
    }
    Ok(i32::from(!result.success))
}

fn parse_action(args: &[&str]) -> Result<OrbConsoleAction> {
    let operation = args[0].to_ascii_lowercase();
    match operation.as_str() {
        "list" | "ls" => {
            ensure_no_extra(&args[1..], "maestro computer list").map(|()| OrbConsoleAction::List)
        }
        "status" => Ok(OrbConsoleAction::Status {
            id: required_id(&args[1..], "maestro computer status <task-id>")?,
        }),
        "followup" | "follow-up" => {
            let Some(id) = args.get(1).copied().filter(|id| !id.trim().is_empty()) else {
                bail!("{}", crate::localization::cli_locale().format("Usage: maestro computer followup <task-id> <prompt>", &[]));
            };
            let prompt = args[2..].join(" ");
            if prompt.trim().is_empty() {
                bail!("{}", crate::localization::cli_locale().format("Usage: maestro computer followup <task-id> <prompt>", &[]));
            }
            Ok(OrbConsoleAction::Followup {
                id: id.to_string(),
                prompt: prompt.trim().to_string(),
            })
        }
        "pause" => Ok(OrbConsoleAction::Pause {
            id: required_id(&args[1..], "maestro computer pause <task-id>")?,
        }),
        "resume" => Ok(OrbConsoleAction::Resume {
            id: required_id(&args[1..], "maestro computer resume <task-id>")?,
        }),
        "cancel" => Ok(OrbConsoleAction::Cancel {
            id: required_id(&args[1..], "maestro computer cancel <task-id>")?,
        }),
        "collect" => Ok(OrbConsoleAction::Collect {
            id: required_id(&args[1..], "maestro computer collect <task-id>")?,
        }),
        "handoff" => parse_handoff_action(&args[1..]),
        _ => bail!("{}", crate::localization::cli_locale().format("Unknown Computer command '{0}'. Use list|status|followup|pause|resume|cancel|collect|handoff", &[args[0].to_string()])),
    }
}

fn parse_handoff_action(args: &[&str]) -> Result<OrbConsoleAction> {
    let Some(operation) = args.first().copied() else {
        bail!(
            "{}",
            crate::localization::cli_locale()
                .format("Usage: maestro computer handoff create|list|read ...", &[])
        );
    };
    match operation.to_ascii_lowercase().as_str() {
        "create" | "capture" => parse_handoff_create(&args[1..]),
        "list" | "ls" => {
            if args.len() != 2 || args[1].trim().is_empty() {
                bail!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "Usage: maestro computer handoff list <target-thread-id>",
                        &[]
                    )
                );
            }
            Ok(OrbConsoleAction::HandoffList {
                target_thread_id: args[1].to_string(),
            })
        }
        "read" => {
            if args.len() != 3 || args[1].trim().is_empty() || args[2].trim().is_empty() {
                bail!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "Usage: maestro computer handoff read <target-thread-id> <package-id>",
                        &[]
                    )
                );
            }
            Ok(OrbConsoleAction::HandoffRead {
                target_thread_id: args[1].to_string(),
                package_id: args[2].to_string(),
            })
        }
        _ => bail!(
            "{}",
            crate::localization::cli_locale().format(
                "Unknown handoff command '{0}'. Use create|list|read",
                &[(operation).to_string()]
            )
        ),
    }
}

fn parse_handoff_create(args: &[&str]) -> Result<OrbConsoleAction> {
    let Some(source_id) = args.first().copied().filter(|id| !id.trim().is_empty()) else {
        bail!("{}", crate::localization::cli_locale().format("Usage: maestro computer handoff create <source-task-id> <target-thread-id> [--file path] [--artifact id] [--include-diff]", &[]));
    };
    let Some(target_thread_id) = args.get(1).copied().filter(|id| !id.trim().is_empty()) else {
        bail!("{}", crate::localization::cli_locale().format("Usage: maestro computer handoff create <source-task-id> <target-thread-id> [--file path] [--artifact id] [--include-diff]", &[]));
    };
    let mut files = Vec::new();
    let mut artifact_ids = Vec::new();
    let mut include_diff = false;
    let mut index = 2;
    while index < args.len() {
        match args[index] {
            "--include-diff" => include_diff = true,
            "--file" | "--artifact" => {
                let flag = args[index];
                let Some(value) = args
                    .get(index + 1)
                    .copied()
                    .filter(|value| !value.is_empty())
                else {
                    bail!("{flag} requires a value");
                };
                if flag == "--file" {
                    files.push(value.to_string());
                } else {
                    artifact_ids.push(value.to_string());
                }
                index += 1;
            }
            value if value.starts_with("--file=") => {
                let value = value.trim_start_matches("--file=");
                if value.is_empty() {
                    bail!(
                        "{}",
                        crate::localization::cli_locale().format("--file requires a value", &[])
                    );
                }
                files.push(value.to_string());
            }
            value if value.starts_with("--artifact=") => {
                let value = value.trim_start_matches("--artifact=");
                if value.is_empty() {
                    bail!(
                        "{}",
                        crate::localization::cli_locale()
                            .format("--artifact requires a value", &[])
                    );
                }
                artifact_ids.push(value.to_string());
            }
            value => bail!(
                "{}",
                crate::localization::cli_locale().format(
                    "Unknown handoff create argument '{0}'",
                    &[(value).to_string()]
                )
            ),
        }
        index += 1;
    }
    if files.is_empty() && artifact_ids.is_empty() && !include_diff {
        bail!(
            "{}",
            crate::localization::cli_locale().format(
                "handoff create requires --file, --artifact, or --include-diff",
                &[]
            )
        );
    }
    Ok(OrbConsoleAction::HandoffCreate {
        source_id: source_id.to_string(),
        target_thread_id: target_thread_id.to_string(),
        files,
        artifact_ids,
        include_diff,
    })
}

fn required_id(args: &[&str], usage: &str) -> Result<String> {
    let Some(id) = args.first().copied().filter(|id| !id.trim().is_empty()) else {
        bail!(
            "{}",
            crate::localization::cli_locale().format("Usage: {0}", &[(usage).to_string()])
        );
    };
    ensure_no_extra(&args[1..], usage)?;
    Ok(id.to_string())
}

fn ensure_no_extra(args: &[&str], usage: &str) -> Result<()> {
    if args.is_empty() {
        return Ok(());
    }
    bail!(
        "{}",
        crate::localization::cli_locale().format("Usage: {0}", &[(usage).to_string()])
    )
}

fn print_usage() {
    println!("{}", crate::localization::cli_locale().format("Usage: maestro computer [list|status <task-id>|followup <task-id> <prompt>|pause <task-id>|resume <task-id>|cancel <task-id>|collect <task-id>|handoff create <source-task-id> <target-thread-id> [--file path] [--artifact id] [--include-diff]|handoff list <target-thread-id>|handoff read <target-thread-id> <package-id>] [--json]", &[]));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_product_operations_without_mcp_names() {
        assert_eq!(
            parse_action(&["list"]).expect("list"),
            OrbConsoleAction::List
        );
        assert_eq!(
            parse_action(&["followup", "task-1", "check", "the", "build"]).expect("followup"),
            OrbConsoleAction::Followup {
                id: "task-1".to_string(),
                prompt: "check the build".to_string(),
            }
        );
        assert_eq!(
            parse_action(&["resume", "task-1"]).expect("resume"),
            OrbConsoleAction::Resume {
                id: "task-1".to_string()
            }
        );
        assert_eq!(
            parse_action(&[
                "handoff",
                "create",
                "task-1",
                "thread-2",
                "--file",
                "src/lib.rs",
                "--include-diff"
            ])
            .expect("handoff create"),
            OrbConsoleAction::HandoffCreate {
                source_id: "task-1".to_string(),
                target_thread_id: "thread-2".to_string(),
                files: vec!["src/lib.rs".to_string()],
                artifact_ids: Vec::new(),
                include_diff: true,
            }
        );
        assert_eq!(
            parse_action(&["handoff", "read", "thread-2", "a".repeat(64).as_str()])
                .expect("handoff read"),
            OrbConsoleAction::HandoffRead {
                target_thread_id: "thread-2".to_string(),
                package_id: "a".repeat(64),
            }
        );
    }

    #[test]
    fn rejects_missing_ids_and_extra_list_arguments() {
        assert!(parse_action(&["status"]).is_err());
        assert!(parse_action(&["list", "task-1"]).is_err());
        assert!(parse_action(&["followup", "task-1"]).is_err());
        assert!(parse_action(&["handoff", "create", "task-1", "thread-2"]).is_err());
        assert!(parse_action(&["handoff", "read", "thread-2"]).is_err());
    }
}
