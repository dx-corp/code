//! Paired, tool-free behavior trials through the existing native summary path.
mod report;
mod suite;
#[cfg(test)]
mod tests;
mod trial;

use anyhow::{Context, Result, ensure};
use clap::Parser;
use maestro_local_host as host;
use maestro_local_host::agent::{
    CredentialVault, ModelDynamicsConfig, NativeAgent, NativeAgentConfig,
};
use maestro_runtime::agent::MaxTokensSource;
use sha2::{Digest, Sha256};
use std::{collections::HashSet, path::PathBuf, time::Duration};

#[derive(Parser)]
struct Args {
    #[arg(long)]
    suite: PathBuf,
    /// Explicit provider-qualified model; no automatic routing or fallback.
    #[arg(long)]
    model: String,
    /// A new directory for local manifests, transcripts and reports.
    #[arg(long)]
    output: PathBuf,
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..=600))]
    timeout_seconds: u64,
    #[arg(long, default_value_t = 1024, value_parser = clap::value_parser!(u32).range(1..=4096))]
    max_tokens: u32,
    /// Keep both seeded histories below the automatic compaction threshold.
    #[arg(long)]
    context_window: u64,
}

fn hash(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    ensure!(
        args.model.contains('/') && !args.model.trim().is_empty(),
        "qualify the model with its provider"
    );
    ensure!(
        args.context_window > u64::from(args.max_tokens) + 4096,
        "context window is too small"
    );
    let raw = std::fs::read(&args.suite)?;
    let suite: suite::Suite = serde_json::from_slice(&raw)?;
    suite.validate(args.context_window, args.max_tokens)?;
    std::fs::create_dir(&args.output)
        .context("output directory must be new and its parent must exist")?;
    let binary = std::env::current_exe()?;
    let manifest = serde_json::json!({
        "schema": "maestro.compaction-eval-manifest.v1", "suite_sha256": hash(&raw),
        "suite": suite, "model": args.model, "summary_model": args.model,
        "executable_sha256": hash(&std::fs::read(binary)?),
        "summary_guidance_sha256": hash(maestro_context::compaction::SUMMARY_EVIDENCE_GUIDANCE.as_bytes()),
        "timeout_seconds": args.timeout_seconds, "max_tokens": args.max_tokens,
        "context_window": args.context_window, "thinking_enabled": false,
        "tools": [], "automatic_model_routing": false,
        "claim": "synthetic_context_behavior_only", "promotion_allowed": false
    });
    std::fs::write(
        args.output.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    let mut results = Vec::new();
    for (index, case) in suite.cases.iter().enumerate() {
        // Alternate order to expose rather than systematically favor warm-cache runs.
        let arms = if index % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        };
        for compact in arms {
            let cwd = tempfile::tempdir()?;
            let config = NativeAgentConfig {
                model: args.model.clone(), max_tokens: args.max_tokens,
                max_tokens_source: MaxTokensSource::Explicit,
                context_window: Some(args.context_window),
                cwd: cwd.path().to_string_lossy().into_owned(),
                system_prompt: Some("Answer from the supplied conversation evidence. Return only the JSON object requested by the final question.".into()),
                thinking_enabled: false,
                model_dynamics: ModelDynamicsConfig { summary_model: Some(args.model.clone()), ..Default::default() },
                ..Default::default()
            };
            // Use ordinary authenticated local composition, with an empty tool allowlist.
            let (agent, mut events) = NativeAgent::new_with_allowed_tools_and_credential_vault(
                config,
                &HashSet::new(),
                CredentialVault::new(),
            )?;
            agent.set_hooks_enabled(false)?;
            let arm = if compact { "compacted" } else { "original" };
            let folder = args.output.join(format!("{}--{arm}", case.id));
            std::fs::create_dir(&folder)?;
            let result = trial::run(
                &agent,
                &mut events,
                case,
                compact,
                Duration::from_secs(args.timeout_seconds),
                &folder,
            )
            .await;
            // Cancel and drain the existing actor; do not leave provider work running.
            agent.cancel();
            agent.shutdown().await;
            let result = result?;
            std::fs::write(
                folder.join("result.json"),
                serde_json::to_vec_pretty(&result)?,
            )?;
            results.push(result);
        }
    }
    let report = report::paired(&suite, &results)?;
    std::fs::write(
        args.output.join("report.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    ensure!(
        report.comparison_valid,
        "inconclusive comparison: inspect recorded runtime failures"
    );
    Ok(())
}
