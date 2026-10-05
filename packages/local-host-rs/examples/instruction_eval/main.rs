//! Paired AGENTS.md instruction fragments through existing native trials.
mod instructions;
#[path = "../behavior_eval/measurement.rs"]
mod report;
#[path = "../compaction_eval/suite.rs"]
mod suite;
#[cfg(test)]
mod tests;
#[path = "../compaction_eval/trial.rs"]
mod trial;

use anyhow::{Context, Result, ensure};
use clap::Parser;
use instructions::{AssignedTrial, InstructionSuite};
use maestro_local_host as host;
use maestro_local_host::agent::{
    CredentialVault, ModelDynamicsConfig, NativeAgent, NativeAgentConfig,
};
use maestro_runtime::agent::MaxTokensSource;
use sha2::{Digest, Sha256};
use std::{collections::HashSet, path::Path, path::PathBuf, time::Duration};

#[derive(Parser)]
struct Args {
    #[arg(long)]
    suite: PathBuf,
    /// Explicit provider-qualified model; no routing or fallback.
    #[arg(long)]
    model: String,
    /// New directory for the frozen plan, arm artifacts and partial reports.
    #[arg(long)]
    output: PathBuf,
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..=600))]
    timeout_seconds: u64,
    #[arg(long, default_value_t = 1024, value_parser = clap::value_parser!(u32).range(1..=4096))]
    max_tokens: u32,
    #[arg(long)]
    context_window: u64,
    /// Validate and write the plan without authentication or model calls.
    #[arg(long)]
    plan_only: bool,
}

fn hash(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn save_report(
    output: &Path,
    suite: &InstructionSuite,
    protocol: &str,
    rows: &[AssignedTrial],
) -> Result<instructions::Report> {
    let report = instructions::paired(suite, protocol, rows);
    // A later interruption cannot leave a stale complete report. Each arm is
    // persisted before the report advances; rename replaces a complete file.
    std::fs::write(
        output.join("report.pending.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    std::fs::rename(
        output.join("report.pending.json"),
        output.join("report.json"),
    )?;
    Ok(report)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let (provider, model) = args
        .model
        .split_once('/')
        .context("qualify model with its provider")?;
    ensure!(
        !provider.trim().is_empty() && !model.trim().is_empty(),
        "qualify model with its provider"
    );
    let raw = std::fs::read(&args.suite)?;
    ensure!(raw.len() <= 2_000_000, "suite exceeds byte budget");
    let suite: InstructionSuite = serde_json::from_slice(&raw)?;
    suite.validate(args.context_window, args.max_tokens)?;
    let binary = std::env::current_exe()?;
    let protocol_inputs = serde_json::json!({
        "schema": "maestro.instruction-eval-protocol.v1", "suite_sha256": hash(&raw),
        "model": args.model, "executable_sha256": hash(&std::fs::read(binary)?),
        "timeout_seconds": args.timeout_seconds, "max_tokens": args.max_tokens,
        "context_window": args.context_window, "thinking_enabled": false,
        "tools": [], "hooks_enabled": false, "automatic_model_routing": false,
        "treatment_surface": "agents_md_fragment", "compaction": "none",
        "isolation": "fresh_workspace_and_native_agent_per_arm"
    });
    let protocol = hash(&serde_json::to_vec(&protocol_inputs)?);
    instructions::unique_plan(&suite, &protocol)?;
    let plan = suite.plan(&protocol);
    std::fs::create_dir(&args.output).context("output must be new and its parent must exist")?;
    std::fs::write(
        args.output.join("manifest.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "protocol_sha256": protocol, "protocol_inputs": protocol_inputs, "suite": suite,
            "expected_runs": plan, "promotion_allowed": false
        }))?,
    )?;
    let mut rows = Vec::new();
    save_report(&args.output, &suite, &protocol, &rows)?;
    if args.plan_only {
        println!(
            "validated {} assigned native trials; no model calls",
            plan.len()
        );
        return Ok(());
    }
    for slot in plan {
        let case = suite
            .cohort
            .cases
            .iter()
            .find(|case| case.id == slot.case_id)
            .expect("planned case");
        let cwd = tempfile::tempdir()?;
        let config = NativeAgentConfig {
            model: args.model.clone(),
            max_tokens: args.max_tokens,
            max_tokens_source: MaxTokensSource::Explicit,
            context_window: Some(args.context_window),
            cwd: cwd.path().to_string_lossy().into_owned(),
            system_prompt: Some(suite.system_prompt(slot.candidate)),
            thinking_enabled: false,
            model_dynamics: ModelDynamicsConfig::default(),
            ..Default::default()
        };
        let (agent, mut events) = NativeAgent::new_with_allowed_tools_and_credential_vault(
            config,
            &HashSet::new(),
            CredentialVault::new(),
        )?;
        agent.set_hooks_enabled(false)?;
        let folder = args.output.join(format!(
            "{}--{}--{}",
            slot.case_id,
            slot.repetition,
            if slot.candidate {
                "candidate"
            } else {
                "control"
            }
        ));
        std::fs::create_dir(&folder)?;
        let result = trial::run(
            &agent,
            &mut events,
            case,
            false,
            Duration::from_secs(args.timeout_seconds),
            &folder,
        )
        .await;
        agent.cancel();
        agent.shutdown().await;
        let assigned = AssignedTrial {
            slot,
            trial: result?,
        };
        std::fs::write(
            folder.join("result.json"),
            serde_json::to_vec_pretty(&assigned)?,
        )?;
        rows.push(assigned);
        save_report(&args.output, &suite, &protocol, &rows)?;
    }
    let report = save_report(&args.output, &suite, &protocol, &rows)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    ensure!(
        report.comparison_valid,
        "inconclusive comparison: inspect assigned outcomes"
    );
    Ok(())
}
