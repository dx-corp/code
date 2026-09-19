//! First-run onboarding built on the typed maestro doctor checks.

use std::io::{self, IsTerminal, Write};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::doctor::{CheckStatus, DoctorReport};

pub const SETUP_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupStep {
    pub id: String,
    pub command: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupReport {
    pub schema_version: u32,
    pub ready: bool,
    pub doctor: DoctorReport,
    pub next_steps: Vec<SetupStep>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SetupOptions {
    json: bool,
    live: bool,
    model: Option<String>,
    platform: bool,
    byok: bool,
}

fn parse_options(args: &[String]) -> Result<SetupOptions> {
    let mut options = SetupOptions {
        json: false,
        live: false,
        model: None,
        platform: false,
        byok: false,
    };
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--json" => options.json = true,
            "--live" => options.live = true,
            "--platform" => options.platform = true,
            "--byok" => options.byok = true,
            "--model" => {
                index += 1;
                options.model = Some(args.get(index).context("--model requires a value")?.clone());
            }
            value if value.starts_with("--model=") => {
                options.model = Some(value[8..].to_owned());
            }
            "--help" | "-h" | "help" => bail!("help"),
            other => bail!(
                "{}",
                crate::localization::cli_locale()
                    .format("unknown setup option: {0}", &[(other).to_string()])
            ),
        }
        index += 1;
    }
    Ok(options)
}

pub fn build_setup_report(doctor: DoctorReport) -> SetupReport {
    let selected_provider = doctor.selected_model.provider.as_str();
    let credential_mode = doctor
        .checks
        .iter()
        .find(|check| check.id == "credential_mode");
    let mode_failed = credential_mode.is_some_and(|check| check.status == CheckStatus::Fail);
    let mut next_steps = Vec::new();

    if mode_failed {
        let identity_required =
            credential_mode.is_some_and(|check| check.summary == "EvalOps Identity is required");
        for (id, reason, command) in crate::credential_mode::setup_next_commands(
            selected_provider,
            identity_required,
            !identity_required,
        ) {
            push_step(&mut next_steps, id, &command, reason.to_owned());
        }
    }

    for check in &doctor.checks {
        match check.id.as_str() {
            "credential_mode" => {}
            "config" if check.status == CheckStatus::Fail => push_step(
                &mut next_steps,
                "config",
                "deixic-code config validate",
                check.summary.clone(),
            ),
            "model_catalog" if check.status == CheckStatus::Fail => push_step(
                &mut next_steps,
                "model",
                "deixic-code models",
                check.summary.clone(),
            ),
            "codex_tools" if check.status == CheckStatus::Fail => push_step(
                &mut next_steps,
                "codex-tools",
                "deixic-code codex doctor",
                check.summary.clone(),
            ),
            "live_metadata" if check.status == CheckStatus::Fail => push_step(
                &mut next_steps,
                "live-metadata",
                "deixic-code doctor --live",
                check.summary.clone(),
            ),
            _ if check.status == CheckStatus::Fail => push_step(
                &mut next_steps,
                &check.id,
                "deixic-code doctor --live",
                check.summary.clone(),
            ),
            _ => {}
        }
    }

    SetupReport {
        schema_version: SETUP_SCHEMA_VERSION,
        ready: doctor.ok
            && !doctor
                .checks
                .iter()
                .any(|check| check.status == CheckStatus::Fail),
        doctor,
        next_steps,
    }
}

fn push_step(steps: &mut Vec<SetupStep>, id: &str, command: &str, reason: String) {
    if steps.iter().any(|step| step.id == id) {
        return;
    }
    steps.push(SetupStep {
        id: id.to_owned(),
        command: command.to_owned(),
        reason,
    });
}

async fn ensure_evalops_identity() -> Result<()> {
    let snapshot = crate::init_cli::load_evalops_snapshot().ok().flatten();
    let env = std::env::vars().collect();
    if crate::credential_mode::platform_session_from(snapshot.as_ref(), &env).is_none() {
        crate::init_cli::perform_evalops_login().await?;
    }
    Ok(())
}

pub async fn run_setup(args: &[String]) -> Result<i32> {
    let options = match parse_options(args) {
        Ok(options) => options,
        Err(error) if error.to_string() == "help" => {
            println!("{}", crate::localization::cli_locale().format("Usage: deixic-code setup [--json] [--live] [--model <provider/model>] [--platform|--byok]", &[]));
            return Ok(0);
        }
        Err(error) => return Err(error),
    };
    if options.platform && options.byok {
        bail!(
            "{}",
            crate::localization::cli_locale().format("choose either --platform or --byok", &[])
        );
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    if options.platform {
        crate::init_cli::perform_evalops_login().await?;
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "EvalOps Identity login saved. Run deixic-code to start a session.",
                &[]
            )
        );
        return Ok(0);
    }
    if options.byok {
        ensure_evalops_identity().await?;
        crate::connections_cli::run_add_wizard(None)?;
        return Ok(0);
    }
    let doctor = crate::doctor::build_report(options.model.as_deref(), options.live, &cwd).await;
    let report = build_setup_report(doctor);
    if options.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(i32::from(!report.ready));
    }
    println!(
        "{}",
        crate::localization::cli_locale().format(
            "Deixic Code Setup (schema v{0})",
            &[(report.schema_version).to_string()]
        )
    );
    println!(
        "{}",
        crate::localization::cli_locale().format(
            "Model: {0} ({1}, {2})",
            &[
                (report.doctor.selected_model.requested).clone(),
                (report.doctor.selected_model.provider).clone(),
                (report.doctor.selected_model.protocol).clone()
            ]
        )
    );
    if report.ready {
        println!("{}", crate::localization::cli_locale().format("Ready: EvalOps Identity is configured with managed inference or a local provider credential.", &[]));
        println!(
            "{}",
            crate::localization::cli_locale().format("Run deixic-code to start a session.", &[])
        );
        return Ok(0);
    }
    if report
        .doctor
        .checks
        .iter()
        .any(|check| check.id == "credential_mode" && check.status == CheckStatus::Fail)
        && io::stdin().is_terminal()
        && io::stdout().is_terminal()
    {
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "EvalOps Identity is required before Deixic Code can run.",
                &[]
            )
        );
        println!(
            "{}",
            crate::localization::cli_locale().format("Choose how to continue:", &[])
        );
        println!(
            "{}",
            crate::localization::cli_locale().format("  1. Sign in and use managed inference", &[])
        );
        println!(
            "{}",
            crate::localization::cli_locale()
                .format("  2. Sign in, then use your own API key", &[])
        );
        print!("Selection [1/2]: ");
        let _ = io::stdout().flush();
        let mut selection = String::new();
        io::stdin().read_line(&mut selection)?;
        match selection.trim() {
            "1" | "" => {
                crate::init_cli::perform_evalops_login().await?;
                println!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "EvalOps Identity login saved. Run deixic-code to start a session.",
                        &[]
                    )
                );
                return Ok(0);
            }
            "2" => {
                ensure_evalops_identity().await?;
                crate::connections_cli::run_add_wizard(None)?;
                return Ok(0);
            }
            other => bail!(
                "{}",
                crate::localization::cli_locale()
                    .format("unknown setup selection: {0}", &[(other).to_string()])
            ),
        }
    }
    println!(
        "{}",
        crate::localization::cli_locale().format("Next steps:", &[])
    );
    for (index, step) in report.next_steps.iter().enumerate() {
        println!("  {}. {}", index + 1, step.reason);
        println!(
            "{}",
            crate::localization::cli_locale()
                .format("     Run: {0}", std::slice::from_ref(&(step.command)))
        );
    }
    Ok(i32::from(!report.ready))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::{CheckStatus, DoctorCheck, SelectedModelReport};

    fn report(provider: &str, checks: Vec<DoctorCheck>) -> DoctorReport {
        DoctorReport {
            schema_version: crate::doctor::REPORT_SCHEMA_VERSION,
            ok: true,
            live_requested: false,
            selected_model: SelectedModelReport {
                requested: format!("{provider}/model"),
                provider: provider.to_owned(),
                protocol: "test".to_owned(),
                catalog: None,
            },
            checks,
        }
    }

    fn check(id: &str, status: CheckStatus, summary: &str, detail: Option<&str>) -> DoctorCheck {
        DoctorCheck {
            id: id.to_owned(),
            status,
            summary: summary.to_owned(),
            detail: detail.map(str::to_owned),
            live: false,
            duration_ms: None,
            remediation: None,
        }
    }

    #[test]
    fn failures_without_a_repair_mapping_cannot_claim_ready() {
        let result = build_setup_report(report(
            "openai",
            vec![check(
                "provider",
                CheckStatus::Fail,
                "resolution failed",
                None,
            )],
        ));
        assert!(!result.ready);
        assert_eq!(result.next_steps[0].command, "deixic-code doctor --live");
    }

    #[test]
    fn setup_requires_identity_before_any_provider_setup() {
        let result = build_setup_report(report(
            "openai",
            vec![
                check(
                    "credential_mode",
                    CheckStatus::Fail,
                    "EvalOps Identity is required",
                    Some("Run `deixic-code evalops login`"),
                ),
                check(
                    "provider",
                    CheckStatus::Warning,
                    "openai resolved; credentials not found",
                    Some("OPENAI_API_KEY"),
                ),
                check(
                    "auth_health",
                    CheckStatus::Warning,
                    "openai: no credential found",
                    Some("OPENAI_API_KEY"),
                ),
            ],
        ));

        assert!(!result.ready);
        assert_eq!(result.next_steps.len(), 1);
        assert_eq!(result.next_steps[0].command, "deixic-code evalops login");
    }

    #[test]
    fn setup_uses_codex_login_as_the_byok_path_for_codex() {
        let result = build_setup_report(report(
            "openai-codex",
            vec![check(
                "credential_mode",
                CheckStatus::Fail,
                "no usable managed or local provider credential",
                None,
            )],
        ));

        assert!(!result.ready);
        assert_eq!(result.next_steps.len(), 1);
        assert_eq!(result.next_steps[0].command, "deixic-code codex login");
    }

    #[test]
    fn platform_session_makes_setup_ready_without_local_keys() {
        let result = build_setup_report(report(
            "evalops",
            vec![check(
                "credential_mode",
                CheckStatus::Pass,
                "platform: org org_1 via EvalOps identity",
                None,
            )],
        ));

        assert!(result.ready);
        assert!(result.next_steps.is_empty());
    }

    #[test]
    fn unrelated_provider_warnings_do_not_block_setup() {
        let result = build_setup_report(report(
            "openai",
            vec![check(
                "auth_health",
                CheckStatus::Warning,
                "anthropic: no credential found",
                Some("ANTHROPIC_API_KEY"),
            )],
        ));

        assert!(result.ready);
        assert!(result.next_steps.is_empty());
    }

    #[test]
    fn stored_openai_oauth_credential_satisfies_provider_warning() {
        let result = build_setup_report(report(
            "openai",
            vec![
                check(
                    "provider",
                    CheckStatus::Warning,
                    "openai resolved; credentials not found",
                    Some("OPENAI_API_KEY"),
                ),
                check(
                    "auth_health",
                    CheckStatus::Pass,
                    "openai: stored OAuth credential present",
                    None,
                ),
            ],
        ));

        assert!(result.ready);
        assert!(result.next_steps.is_empty());
    }
}
