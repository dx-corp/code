//! Local mission execution through the existing durable workflow owner.
//!
//! Mission artifacts describe work and assertions. Only command-line options
//! supplied to this entry point can admit tools, write scopes, or verifiers.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use maestro_local_host::mission_cli::{
    MissionState, MissionStore, MissionStoreConfig, MissionStoreSnapshot,
    get_mission_artifact_layout,
};
use maestro_local_host::skill_cli::write_atomic;
use maestro_swarm::SwarmSnapshot;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::workflow_runtime::{
    WorkflowModelConfig, WorkflowRun, WorkflowRunStatus, WorkflowSpec, WorkflowStep, WorkflowStore,
    WorkflowVerification,
};

const SPEC_FILE: &str = "workflow-spec.json";
const JOURNAL_FILE: &str = "workflow-runs.jsonl";
const RUN_HELP: &str = "Usage: deixic-code mission run <mission-id> --model <name> --token-budget <tokens> --allow-tool <tool> --write-scope <path> --verify <assertion-id> <command> [--verify-arg <arg>] [--max-agents <n>] [--max-concurrency <n>] [--json]\n\nThe first run freezes the mission features, objective, assertions, tool grants, and verifiers. Repeat with only the mission ID to resume.\n";

#[derive(Debug, Default)]
struct RunOptions {
    mission_id: String,
    model: Option<String>,
    token_budget: Option<u64>,
    max_agents: Option<u32>,
    max_concurrency: Option<u32>,
    allowed_tools: Vec<String>,
    write_scopes: Vec<String>,
    verification: BTreeMap<String, WorkflowVerification>,
    json: bool,
}

impl RunOptions {
    fn parse(args: &[String]) -> Result<Self> {
        let mut options = Self::default();
        let mut last_verifier: Option<String> = None;
        let mut index = 0;
        while index < args.len() {
            match args[index].as_str() {
                "--json" => options.json = true,
                "--model" => options.model = Some(value(args, &mut index, "--model")?.to_owned()),
                "--token-budget" => {
                    options.token_budget = Some(
                        value(args, &mut index, "--token-budget")?
                            .parse()
                            .context("--token-budget must be a positive integer")?,
                    );
                }
                "--max-agents" => {
                    options.max_agents = Some(
                        value(args, &mut index, "--max-agents")?
                            .parse()
                            .context("--max-agents must be a positive integer")?,
                    );
                }
                "--max-concurrency" => {
                    options.max_concurrency = Some(
                        value(args, &mut index, "--max-concurrency")?
                            .parse()
                            .context("--max-concurrency must be a positive integer")?,
                    );
                }
                "--allow-tool" => options
                    .allowed_tools
                    .push(value(args, &mut index, "--allow-tool")?.to_owned()),
                "--write-scope" => options
                    .write_scopes
                    .push(value(args, &mut index, "--write-scope")?.to_owned()),
                "--verify" => {
                    let assertion_id = value(args, &mut index, "--verify")?.to_owned();
                    let command = value(args, &mut index, "--verify")?.to_owned();
                    if options
                        .verification
                        .insert(
                            assertion_id.clone(),
                            WorkflowVerification {
                                command,
                                args: Vec::new(),
                                timeout_ms: 120_000,
                            },
                        )
                        .is_some()
                    {
                        bail!("duplicate verifier for assertion `{assertion_id}`");
                    }
                    last_verifier = Some(assertion_id);
                }
                "--verify-arg" => {
                    let argument = value(args, &mut index, "--verify-arg")?.to_owned();
                    let assertion_id = last_verifier
                        .as_ref()
                        .ok_or_else(|| anyhow!("--verify-arg requires a preceding --verify"))?;
                    options
                        .verification
                        .get_mut(assertion_id)
                        .expect("preceding verifier exists")
                        .args
                        .push(argument);
                }
                arg if arg.starts_with('-') => bail!("unknown mission run option `{arg}`"),
                mission_id if options.mission_id.is_empty() => {
                    options.mission_id = mission_id.to_owned();
                }
                _ => bail!("mission run accepts one mission ID"),
            }
            index += 1;
        }
        if options.mission_id.is_empty() {
            bail!("mission run requires a mission ID");
        }
        Ok(options)
    }

    fn has_admission_options(&self) -> bool {
        self.model.is_some()
            || self.token_budget.is_some()
            || self.max_agents.is_some()
            || self.max_concurrency.is_some()
            || !self.allowed_tools.is_empty()
            || !self.write_scopes.is_empty()
            || !self.verification.is_empty()
    }
}

fn value<'a>(args: &'a [String], index: &mut usize, flag: &str) -> Result<&'a str> {
    *index += 1;
    args.get(*index)
        .map(String::as_str)
        .ok_or_else(|| anyhow!("{flag} requires a value"))
}

fn assertions(
    layout: &maestro_local_host::mission_cli::MissionArtifactLayout,
) -> Result<BTreeSet<String>> {
    let contract = fs::read_to_string(&layout.validation_contract_markdown)
        .context("read validation-contract.md")?;
    let meaningful = contract.lines().any(|line| {
        let line = line.trim();
        !line.is_empty()
            && line != "# Validation Contract"
            && line != "Add durable behavioral assertions before decomposing features."
    });
    if !meaningful {
        bail!("validation-contract.md has no authored assertions");
    }
    let state: Value = serde_json::from_slice(
        &fs::read(&layout.validation_state_json).context("read validation-state.json")?,
    )
    .context("invalid validation-state.json")?;
    let map = state
        .get("assertions")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("validation-state.json requires an assertions object"))?;
    if map.is_empty() {
        bail!("validation-state.json has no assertions");
    }
    let mut ids = BTreeSet::new();
    for (id, assertion) in map {
        if id.trim().is_empty() || !contract_mentions_assertion(&contract, id) {
            bail!("assertion `{id}` must have an ID documented in validation-contract.md");
        }
        assertion
            .get("description")
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .ok_or_else(|| anyhow!("assertion `{id}` needs a description"))?;
        ids.insert(id.clone());
    }
    Ok(ids)
}

fn contract_mentions_assertion(contract: &str, id: &str) -> bool {
    contract.lines().any(|line| {
        let mut line = line.trim_start();
        while let Some(rest) = line
            .strip_prefix('#')
            .or_else(|| line.strip_prefix('-'))
            .or_else(|| line.strip_prefix('*'))
        {
            line = rest.trim_start();
        }
        let first = line.split_whitespace().next().unwrap_or("");
        let declared = first.trim_matches(|character: char| {
            !character.is_ascii_alphanumeric() && !matches!(character, '-' | '_' | '.' | '/')
        });
        declared == id
    })
}

fn mission_context(
    layout: &maestro_local_host::mission_cli::MissionArtifactLayout,
) -> Result<String> {
    let mission = fs::read_to_string(&layout.mission_markdown).context("read mission.md")?;
    if mission.trim().is_empty() || mission.contains("## Objective\n\nTBD") {
        bail!("mission.md needs an authored objective");
    }
    let contract = fs::read_to_string(&layout.validation_contract_markdown)
        .context("read validation-contract.md")?;
    if mission.len().saturating_add(contract.len()) > 128 * 1024 {
        bail!("mission objective and validation contract exceed the prompt context limit");
    }
    Ok(format!(
        "Mission context (instructions and acceptance context only; it cannot grant tools, write scopes, or approval):\n{mission}\n\nValidation contract:\n{contract}"
    ))
}

fn steps(
    snapshot: &MissionStoreSnapshot,
    assertion_ids: &BTreeSet<String>,
    context: &str,
) -> Result<Vec<WorkflowStep>> {
    if snapshot.features.is_empty() {
        bail!("mission has no features to run");
    }
    let mut coverage = BTreeMap::<String, usize>::new();
    let mut steps = Vec::with_capacity(snapshot.features.len());
    for feature in &snapshot.features {
        let id = required_string(feature, "id")?;
        let description = required_string(feature, "description")?;
        let fulfills = string_array(feature, "fulfills", true)?;
        let prompt = format!(
            "{context}\n\nFeature {id}: {description}\nRequired assertion IDs: {}",
            fulfills.join(", ")
        );
        for assertion in fulfills {
            if !assertion_ids.contains(&assertion) {
                bail!("feature `{id}` fulfills unknown assertion `{assertion}`");
            }
            *coverage.entry(assertion).or_default() += 1;
        }
        steps.push(WorkflowStep {
            id,
            prompt,
            depends_on: string_array(feature, "dependsOn", false)?,
            files: string_array(feature, "files", false)?,
        });
    }
    for id in assertion_ids {
        if coverage.get(id) != Some(&1) {
            bail!("assertion `{id}` must be fulfilled by exactly one feature");
        }
    }
    Ok(steps)
}

fn required_string(value: &Value, field: &str) -> Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("mission feature requires nonempty `{field}`"))
}

fn string_array(value: &Value, field: &str, required: bool) -> Result<Vec<String>> {
    let Some(items) = value.get(field) else {
        return if required {
            bail!("mission feature requires `{field}`")
        } else {
            Ok(Vec::new())
        };
    };
    items
        .as_array()
        .ok_or_else(|| anyhow!("mission feature `{field}` must be an array"))?
        .iter()
        .map(|item| {
            item.as_str()
                .filter(|text| !text.trim().is_empty())
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("mission feature `{field}` must contain nonempty strings"))
        })
        .collect()
}

fn contract_version(
    layout: &maestro_local_host::mission_cli::MissionArtifactLayout,
    steps: &[WorkflowStep],
) -> Result<String> {
    let contract = fs::read(&layout.validation_contract_markdown)?;
    let mission = fs::read(&layout.mission_markdown)?;
    let state: Value = serde_json::from_slice(&fs::read(&layout.validation_state_json)?)?;
    let mut assertions = state
        .get("assertions")
        .cloned()
        .ok_or_else(|| anyhow!("validation-state.json has no assertions"))?;
    let entries = assertions
        .as_object_mut()
        .ok_or_else(|| anyhow!("validation-state.json assertions must be an object"))?;
    for assertion in entries.values_mut() {
        if let Some(fields) = assertion.as_object_mut() {
            for key in [
                "status",
                "resultSha",
                "workflowRunId",
                "verificationCommand",
                "verificationExitCode",
            ] {
                fields.remove(key);
            }
        }
    }
    let bytes = serde_json::to_vec(&(mission, contract, assertions, steps))?;
    Ok(format!("mission-v1-{:x}", Sha256::digest(bytes)))
}

fn build_spec(
    options: &RunOptions,
    snapshot: &MissionStoreSnapshot,
    assertion_ids: &BTreeSet<String>,
    steps: Vec<WorkflowStep>,
    version: String,
) -> Result<WorkflowSpec> {
    if options
        .verification
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>()
        != *assertion_ids
    {
        bail!("supply exactly one --verify <assertion-id> <command> for each assertion");
    }
    if !options.allowed_tools.iter().any(|tool| {
        matches!(
            tool.as_str(),
            "write" | "edit" | "apply_patch" | "bash" | "shell" | "command"
        )
    }) {
        bail!(
            "coding mission requires an explicit write-capable --allow-tool for revision-bound verification"
        );
    }
    let max_agents = options
        .max_agents
        .unwrap_or(u32::try_from(steps.len()).context("too many mission features")?);
    let max_concurrency = options.max_concurrency.unwrap_or(max_agents.min(2));
    let mut model = WorkflowModelConfig::default();
    model.model = options
        .model
        .clone()
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| anyhow!("first mission admission requires --model"))?;
    let token_budget = options
        .token_budget
        .ok_or_else(|| anyhow!("first mission admission requires --token-budget"))?;
    let spec = WorkflowSpec {
        name: snapshot.mission_id.clone(),
        version,
        steps,
        max_agents,
        max_concurrency,
        token_budget,
        replay_safe: false,
        model,
        allowed_tools: options.allowed_tools.clone(),
        write_scopes: options.write_scopes.clone(),
        verification: options.verification.values().cloned().collect(),
    };
    spec.validate().map_err(anyhow::Error::msg)?;
    Ok(spec)
}

fn ensure_regular_file_or_missing(path: &Path) -> Result<()> {
    for component in path.ancestors() {
        let metadata = match fs::symlink_metadata(component) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| component.display().to_string()),
        };
        if metadata.file_type().is_symlink() {
            bail!(
                "mission run path traverses a symlink: {}",
                component.display()
            );
        }
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(_) => bail!("mission run path is not a regular file: {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| path.display().to_string()),
    }
}

fn read_frozen_spec(path: &Path) -> Result<Option<WorkflowSpec>> {
    ensure_regular_file_or_missing(path)?;
    if !path.exists() {
        return Ok(None);
    }
    let spec =
        serde_json::from_slice(&fs::read(path)?).context("invalid frozen mission workflow spec")?;
    Ok(Some(spec))
}

fn freeze_spec(path: &Path, spec: &WorkflowSpec) -> Result<()> {
    ensure_regular_file_or_missing(path)?;
    let bytes = serde_json::to_vec_pretty(spec)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("freeze mission workflow spec at {}", path.display()))?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

fn unresolved_handoffs(snapshot: &MissionStoreSnapshot) -> Vec<String> {
    let mut unresolved = Vec::new();
    for feature in &snapshot.features {
        let id = feature
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if let Some(handoff) = feature.get("handoff") {
            if handoff.get("success").and_then(Value::as_bool) == Some(false)
                || handoff
                    .get("whatWasLeftUndone")
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.trim().is_empty())
            {
                unresolved.push(id.to_owned());
            }
            if handoff
                .get("discoveredIssues")
                .and_then(Value::as_array)
                .is_some_and(|issues| {
                    issues.iter().any(|issue| {
                        issue.get("severity").and_then(Value::as_str) == Some("blocking")
                    })
                })
            {
                unresolved.push(id.to_owned());
            }
        }
        let tracked = feature.get("trackedHandoffItems").and_then(Value::as_array);
        let dismissals = feature.get("handoffDismissals").and_then(Value::as_array);
        if tracked.is_some_and(|items| {
            items.iter().any(|item| {
                !dismissals.is_some_and(|entries| {
                    entries.iter().any(|dismissal| {
                        dismissal.get("kind") == item.get("kind")
                            && dismissal.get("key") == item.get("key")
                    })
                })
            })
        }) {
            unresolved.push(id.to_owned());
        }
    }
    unresolved.sort();
    unresolved.dedup();
    unresolved
}

fn verify_completed_run(run: &WorkflowRun, spec: &WorkflowSpec) -> Result<()> {
    if run.status != WorkflowRunStatus::Complete {
        bail!(
            "workflow is {:?}: {}",
            run.status,
            run.status_reason.as_deref().unwrap_or("no reason recorded")
        );
    }
    if run.spec_sha != run.spec.sha256()
        || run.spec.version != spec.version
        || run.spec.steps != spec.steps
    {
        bail!("workflow journal does not match the frozen mission contract");
    }
    let revision = run
        .integrated_revision
        .as_deref()
        .filter(|sha| !sha.is_empty())
        .ok_or_else(|| anyhow!("workflow has no integrated revision"))?;
    if !run.verification_reservations.is_empty()
        || run.verification_results.len() != spec.verification.len()
    {
        bail!("workflow verifier receipts are incomplete");
    }
    for (expected, result) in spec.verification.iter().zip(&run.verification_results) {
        if !result.success
            || result.exit_code != Some(0)
            || result.timed_out
            || result.command != expected.command
            || result.result_sha.as_deref() != Some(revision)
        {
            bail!(
                "workflow verifier `{}` lacks passing revision-bound proof",
                expected.command
            );
        }
    }
    let snapshot: SwarmSnapshot = serde_json::from_value(
        run.swarm_snapshot
            .clone()
            .ok_or_else(|| anyhow!("workflow has no scheduler snapshot"))?,
    )
    .context("workflow scheduler snapshot is malformed")?;
    snapshot
        .validate()
        .context("workflow scheduler snapshot is invalid")?;
    if !snapshot.failed_tasks.is_empty()
        || !snapshot.in_flight.is_empty()
        || !snapshot.indeterminate_tasks.is_empty()
    {
        bail!("workflow has failed or unresolved tasks");
    }
    for step in &spec.steps {
        if !snapshot.completed_tasks.contains_key(&step.id) {
            bail!("feature `{}` has no accepted workflow result", step.id);
        }
    }
    Ok(())
}

fn record_verified_assertions(
    layout: &maestro_local_host::mission_cli::MissionArtifactLayout,
    assertion_ids: &BTreeSet<String>,
    run: &WorkflowRun,
) -> Result<()> {
    let mut state: Value = serde_json::from_slice(&fs::read(&layout.validation_state_json)?)?;
    let entries = state
        .get_mut("assertions")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow!("validation-state.json has no assertions object"))?;
    let revision = run
        .integrated_revision
        .as_deref()
        .ok_or_else(|| anyhow!("workflow has no integrated revision"))?;
    for (id, result) in assertion_ids.iter().zip(&run.verification_results) {
        let assertion = entries
            .get_mut(id)
            .and_then(Value::as_object_mut)
            .ok_or_else(|| anyhow!("assertion `{id}` disappeared"))?;
        assertion.insert("status".to_owned(), Value::String("passed".to_owned()));
        assertion.insert("resultSha".to_owned(), Value::String(revision.to_owned()));
        assertion.insert("workflowRunId".to_owned(), Value::String(run.id.clone()));
        assertion.insert(
            "verificationCommand".to_owned(),
            Value::String(result.command.clone()),
        );
        assertion.insert("verificationExitCode".to_owned(), Value::Number(0.into()));
    }
    state["updatedAt"] = Value::String(run.updated_at.clone());
    write_atomic(
        &layout.validation_state_json,
        &format!("{}\n", serde_json::to_string_pretty(&state)?),
    )
}

/// Run a prepared local mission through the native workflow owner.
pub async fn run(args: &[String]) -> Result<i32> {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h" | "help"))
    {
        print!("{RUN_HELP}");
        return Ok(0);
    }
    let options = RunOptions::parse(args)?;
    let mut store = MissionStore::load(&options.mission_id, MissionStoreConfig::default())?;
    let snapshot = store.get_snapshot()?;
    if snapshot.state == MissionState::Failed {
        bail!(
            "mission `{}` is already {:?}",
            snapshot.mission_id,
            snapshot.state
        );
    }
    let layout = get_mission_artifact_layout(&snapshot.mission_id, None)?;
    let assertion_ids = assertions(&layout)?;
    let context = mission_context(&layout)?;
    let current_steps = steps(&snapshot, &assertion_ids, &context)?;
    let version = contract_version(&layout, &current_steps)?;
    let initial_handoffs = unresolved_handoffs(&snapshot);
    if !initial_handoffs.is_empty() {
        bail!(
            "mission has unresolved handoffs for features: {}",
            initial_handoffs.join(", ")
        );
    }
    let spec_path = layout.mission_dir.join(SPEC_FILE);
    let journal_path = layout.mission_dir.join(JOURNAL_FILE);
    ensure_regular_file_or_missing(&journal_path)?;
    let frozen = read_frozen_spec(&spec_path)?;
    if snapshot.state == MissionState::Completed && frozen.is_none() {
        bail!("completed mission has no frozen workflow proof to reconcile");
    }
    let spec = match frozen {
        Some(spec) => {
            if options.has_admission_options() {
                bail!(
                    "mission already has a frozen workflow spec; resume without admission options"
                );
            }
            if spec.version != version || spec.steps != current_steps {
                bail!("mission features or validation contract changed after workflow admission");
            }
            spec
        }
        None => {
            if journal_path.exists() {
                bail!("mission workflow journal exists without its frozen spec");
            }
            let spec = build_spec(&options, &snapshot, &assertion_ids, current_steps, version)?;
            freeze_spec(&spec_path, &spec)?;
            spec
        }
    };
    if spec.verification.len() != assertion_ids.len() {
        bail!("frozen workflow verifier coverage no longer matches mission assertions");
    }
    let journal = WorkflowStore::with_path(journal_path.clone());
    let runs = journal.list().map_err(anyhow::Error::msg)?;
    if runs.len() > 1 {
        bail!("mission workflow journal contains multiple runs");
    }
    let workflow_args = if let Some(existing) = runs.first() {
        if existing.spec_sha != existing.spec.sha256() || existing.spec != spec {
            bail!("mission workflow journal disagrees with its frozen spec");
        }
        if existing.status == WorkflowRunStatus::Complete {
            Vec::new()
        } else {
            vec![
                "resume".to_owned(),
                existing.id.clone(),
                "--journal".to_owned(),
                journal_path.display().to_string(),
            ]
        }
    } else {
        vec![
            "run".to_owned(),
            spec_path.display().to_string(),
            "--journal".to_owned(),
            journal_path.display().to_string(),
        ]
    };
    if !workflow_args.is_empty() {
        store.set_state(MissionState::Running, Some("Native workflow admitted"))?;
        let exit = match crate::workflow_cli::run_workflow(&workflow_args).await {
            Ok(exit) => exit,
            Err(error) => {
                store.set_state(
                    MissionState::Blocked,
                    Some(&format!("Native workflow could not continue: {error}")),
                )?;
                return Err(error);
            }
        };
        if exit != 0 {
            store.set_state(
                MissionState::Blocked,
                Some("Native workflow did not complete"),
            )?;
            return Ok(exit);
        }
    }
    let runs = journal.list().map_err(anyhow::Error::msg)?;
    let run = runs
        .first()
        .ok_or_else(|| anyhow!("workflow produced no journal record"))?;
    verify_completed_run(run, &spec)?;
    let current =
        MissionStore::load(&options.mission_id, MissionStoreConfig::default())?.get_snapshot()?;
    let postflight_assertions = assertions(&layout)?;
    let postflight_context = mission_context(&layout)?;
    let postflight_steps = steps(&current, &postflight_assertions, &postflight_context)?;
    if contract_version(&layout, &postflight_steps)? != spec.version {
        bail!("mission features or validation contract changed during workflow execution");
    }
    let unresolved = unresolved_handoffs(&current);
    if !unresolved.is_empty() {
        bail!(
            "mission has unresolved handoffs for features: {}",
            unresolved.join(", ")
        );
    }
    if current.state != MissionState::Completed {
        let mut store = MissionStore::load(&options.mission_id, MissionStoreConfig::default())?;
        let mut features = current.features.clone();
        for feature in &mut features {
            if feature.get("codingAcceptance").is_none() {
                feature["status"] = Value::String("passed".to_owned());
            }
        }
        store.set_features(features)?;
        store.set_state(
            MissionState::Completed,
            Some(&format!(
                "Workflow {} verified at {}",
                run.id,
                run.integrated_revision.as_deref().unwrap_or_default()
            )),
        )?;
    }
    record_verified_assertions(&layout, &assertion_ids, run)?;
    if options.json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &MissionStore::load(&options.mission_id, MissionStoreConfig::default())?
                    .get_snapshot()?
            )?
        );
    } else {
        println!(
            "Mission {} completed at {}",
            current.mission_id,
            run.integrated_revision.as_deref().unwrap_or_default()
        );
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use maestro_local_host::mission_cli::create_mission_store_snapshot;
    use serde_json::json;
    use tempfile::TempDir;

    fn feature(id: &str, fulfills: &[&str]) -> Value {
        json!({"id":id,"description":format!("Implement {id}"),"status":"pending","fulfills":fulfills})
    }

    #[test]
    fn contract_requires_declared_exact_assertion_id() {
        assert!(contract_mentions_assertion(
            "- `login-expired`: reject old tokens",
            "login-expired"
        ));
        assert!(contract_mentions_assertion(
            "### login-expired\nReject old tokens",
            "login-expired"
        ));
        assert!(!contract_mentions_assertion(
            "- login-expired-extra: reject old tokens",
            "login-expired"
        ));
        assert!(!contract_mentions_assertion(
            "The login-expired check matters",
            "login-expired"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn spec_path_rejects_symlinked_mission_directory() {
        let temp = TempDir::new().unwrap();
        let actual = temp.path().join("actual");
        fs::create_dir(&actual).unwrap();
        let alias = temp.path().join("alias");
        std::os::unix::fs::symlink(&actual, &alias).unwrap();
        assert!(
            ensure_regular_file_or_missing(&alias.join(SPEC_FILE))
                .unwrap_err()
                .to_string()
                .contains("symlink")
        );
    }

    #[test]
    fn assertions_require_one_feature_each() {
        let assertions = BTreeSet::from(["a".to_owned(), "b".to_owned()]);
        let snapshot = create_mission_store_snapshot(
            "sample",
            None,
            vec![feature("one", &["a"]), feature("two", &["b"])],
            "2026-01-01T00:00:00Z",
        )
        .unwrap();
        assert_eq!(steps(&snapshot, &assertions, "context").unwrap().len(), 2);
        let duplicate = create_mission_store_snapshot(
            "sample",
            None,
            vec![feature("one", &["a"]), feature("two", &["a", "b"])],
            "2026-01-01T00:00:00Z",
        )
        .unwrap();
        assert!(
            steps(&duplicate, &assertions, "context")
                .unwrap_err()
                .to_string()
                .contains("exactly one")
        );
        let missing = create_mission_store_snapshot(
            "sample",
            None,
            vec![feature("one", &["a"])],
            "2026-01-01T00:00:00Z",
        )
        .unwrap();
        assert!(
            steps(&missing, &assertions, "context")
                .unwrap_err()
                .to_string()
                .contains("exactly one")
        );
    }

    #[test]
    fn verifier_mapping_must_cover_all_assertions() {
        let assertions = BTreeSet::from(["a".to_owned(), "b".to_owned()]);
        let snapshot = create_mission_store_snapshot(
            "sample",
            None,
            vec![feature("one", &["a"]), feature("two", &["b"])],
            "2026-01-01T00:00:00Z",
        )
        .unwrap();
        let steps = steps(&snapshot, &assertions, "context").unwrap();
        let options = RunOptions {
            allowed_tools: vec!["edit".into()],
            write_scopes: vec!["src".into()],
            verification: BTreeMap::from([(
                "a".into(),
                WorkflowVerification {
                    command: "true".into(),
                    args: Vec::new(),
                    timeout_ms: 1000,
                },
            )]),
            ..RunOptions::default()
        };
        assert!(
            build_spec(&options, &snapshot, &assertions, steps, "v1".into())
                .unwrap_err()
                .to_string()
                .contains("exactly one --verify")
        );
    }

    #[test]
    fn changed_validation_contract_changes_frozen_version() {
        let temp = TempDir::new().unwrap();
        let layout = get_mission_artifact_layout("sample", Some(temp.path())).unwrap();
        fs::create_dir_all(&layout.mission_dir).unwrap();
        fs::write(
            &layout.validation_contract_markdown,
            "# Validation Contract\n\na: works\n",
        )
        .unwrap();
        fs::write(
            &layout.mission_markdown,
            "# Sample\n\n## Objective\n\nFix it\n",
        )
        .unwrap();
        fs::write(
            &layout.validation_state_json,
            r#"{"assertions":{"a":{"description":"works"}}}"#,
        )
        .unwrap();
        let steps = vec![WorkflowStep {
            id: "one".into(),
            prompt: "work".into(),
            depends_on: Vec::new(),
            files: Vec::new(),
        }];
        let original = contract_version(&layout, &steps).unwrap();
        fs::write(
            &layout.validation_state_json,
            r#"{"assertions":{"a":{"description":"works well"}}}"#,
        )
        .unwrap();
        assert_ne!(contract_version(&layout, &steps).unwrap(), original);
    }

    #[test]
    fn fabricated_or_incomplete_verifier_receipts_fail_closed() {
        let snapshot = create_mission_store_snapshot(
            "sample",
            None,
            vec![feature("one", &["a"])],
            "2026-01-01T00:00:00Z",
        )
        .unwrap();
        let assertions = BTreeSet::from(["a".to_owned()]);
        let options = RunOptions {
            model: Some("test-model".into()),
            token_budget: Some(1000),
            allowed_tools: vec!["edit".into()],
            write_scopes: vec!["src".into()],
            verification: BTreeMap::from([(
                "a".into(),
                WorkflowVerification {
                    command: "true".into(),
                    args: Vec::new(),
                    timeout_ms: 1000,
                },
            )]),
            ..RunOptions::default()
        };
        let spec = build_spec(
            &options,
            &snapshot,
            &assertions,
            steps(&snapshot, &assertions, "context").unwrap(),
            "v1".into(),
        )
        .unwrap();
        let mut run = WorkflowRun::start(spec.clone(), json!({})).unwrap();
        run.status = WorkflowRunStatus::Complete;
        run.integrated_revision = Some("revision".into());
        assert!(
            verify_completed_run(&run, &spec)
                .unwrap_err()
                .to_string()
                .contains("incomplete")
        );
        run.verification_results
            .push(crate::workflow_runtime::WorkflowVerificationResult {
                command: "true".into(),
                exit_code: Some(0),
                success: true,
                output: String::new(),
                timed_out: false,
                result_sha: Some("different-revision".into()),
            });
        assert!(
            verify_completed_run(&run, &spec)
                .unwrap_err()
                .to_string()
                .contains("revision-bound")
        );
    }
}
