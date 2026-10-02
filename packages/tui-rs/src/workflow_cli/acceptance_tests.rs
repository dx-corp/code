//! End-to-end acceptance coverage for the local workflow CLI owner boundary.
//!
//! These tests use the production scheduler, journal, Git integration, and
//! verifier code.  The only substituted component is the child provider host:
//! a deterministic runner edits an isolated child worktree and returns the
//! same typed outcome the native host would return.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use maestro_swarm::{
    RecoveryDecision, SwarmExecutor, SwarmPlan, SwarmRecoveryHooks, SwarmSnapshot, SwarmStatus,
    SwarmTask, SwarmTaskContext, SwarmTaskOutcome, TaskResult,
};
use serde_json::{Map, Value};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::workflow_runtime::WorkflowStep;

struct RepoFixture {
    _root: TempDir,
    repo: PathBuf,
    journal: PathBuf,
}

impl RepoFixture {
    fn new(label: &str) -> Self {
        let root = tempfile::Builder::new()
            .prefix(&format!("maestro-workflow-cli-{label}-"))
            .tempdir()
            .expect("temporary fixture directory");
        let repo = root.path().join("repo");
        fs::create_dir_all(repo.join("src")).expect("repository directory");
        git_ok(&repo, &["init", "--quiet"]);
        git_ok(&repo, &["config", "user.email", "maestro-test@example.com"]);
        git_ok(&repo, &["config", "user.name", "Maestro acceptance test"]);
        fs::write(repo.join("src/base.txt"), "base\n").expect("base file");
        git_ok(&repo, &["add", "--all"]);
        git_ok(&repo, &["commit", "--quiet", "-m", "base"]);
        let repo = dunce::canonicalize(repo).expect("canonical repository path");

        Self {
            journal: root.path().join("workflow-runs.jsonl"),
            _root: root,
            repo,
        }
    }
}

fn git_output(cwd: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|error| panic!("git {args:?} could not start: {error}"))
}

fn git_ok(cwd: &Path, args: &[&str]) {
    let output = git_output(cwd, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_revision(cwd: &Path, revision: &str) -> String {
    let output = git_output(cwd, &["rev-parse", "--verify", revision]);
    assert!(
        output.status.success(),
        "git rev-parse {revision:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn git_status(cwd: &Path) -> String {
    let output = git_output(cwd, &["status", "--porcelain=v1", "--untracked-files=all"]);
    assert!(output.status.success(), "git status failed");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn worktree_paths(cwd: &Path) -> Vec<String> {
    let output = git_output(cwd, &["worktree", "list", "--porcelain"]);
    assert!(output.status.success(), "git worktree list failed");
    let mut paths = output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter_map(|line| line.strip_prefix(b"worktree "))
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

fn remove_extra_worktrees(cwd: &Path, before: &[String], after: &[String]) {
    for path in after.iter().filter(|path| !before.contains(path)) {
        let output = git_output(cwd, &["worktree", "remove", "--force", path]);
        assert!(
            output.status.success(),
            "could not clean test worktree {path}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn successful_result(output: impl Into<String>) -> TaskResult {
    TaskResult {
        success: true,
        output: output.into(),
        files_modified: Vec::new(),
        duration_ms: 1,
        error: None,
    }
}

fn verifying_aggregate() -> WorkflowVerification {
    WorkflowVerification {
        command: "sh".to_owned(),
        args: vec![
            "-c".to_owned(),
            "test -f src/root.txt && test -f src/discovered.txt && git rev-parse HEAD".to_owned(),
        ],
        timeout_ms: 10_000,
    }
}

fn verifier_writing_marker(marker: &Path) -> WorkflowVerification {
    WorkflowVerification {
        command: "sh".to_owned(),
        args: vec![
            "-c".to_owned(),
            "printf reran > \"$1\"".to_owned(),
            "verifier".to_owned(),
            marker.to_string_lossy().into_owned(),
        ],
        timeout_ms: 10_000,
    }
}

fn verifier_changes_head() -> WorkflowVerification {
    WorkflowVerification {
        command: "sh".to_owned(),
        args: vec![
            "-c".to_owned(),
            "printf changed > verifier-change.txt && git add verifier-change.txt && git -c user.name=Verifier -c user.email=verifier@example.com commit --quiet -m verifier".to_owned(),
        ],
        timeout_ms: 10_000,
    }
}

fn verifier_with_background_descendant(pid_file: &Path) -> WorkflowVerification {
    WorkflowVerification {
        command: "sh".to_owned(),
        args: vec![
            "-c".to_owned(),
            "sleep 30 & child=$!; printf '%s' \"$child\" > \"$1\"; kill -0 \"$child\"".to_owned(),
            "verifier".to_owned(),
            pid_file.to_string_lossy().into_owned(),
        ],
        timeout_ms: 10_000,
    }
}

fn workflow_spec(name: &str, verification: WorkflowVerification) -> WorkflowSpec {
    WorkflowSpec {
        name: name.to_owned(),
        version: "1".to_owned(),
        steps: vec![
            WorkflowStep {
                id: "root".to_owned(),
                prompt: "make the root contribution".to_owned(),
                depends_on: Vec::new(),
                files: vec!["src/root.txt".to_owned()],
            },
            WorkflowStep {
                id: "final".to_owned(),
                prompt: "synthesize the completed contributions".to_owned(),
                depends_on: vec!["root".to_owned()],
                files: Vec::new(),
            },
        ],
        max_agents: 4,
        max_concurrency: 1,
        token_budget: 512,
        replay_safe: false,
        model: WorkflowModelConfig {
            model: "luna-acceptance-test".to_owned(),
            provider: None,
            reasoning_effort: None,
            max_output_tokens: 64,
        },
        allowed_tools: vec!["write".to_owned()],
        write_scopes: vec!["src".to_owned()],
        verification: vec![verification],
    }
}

struct AcceptedWorkflow {
    spec: WorkflowSpec,
    run_id: String,
    journal: WorkflowJournal,
    integration: Option<IntegrationContext>,
}

fn accepted_workflow(fixture: &RepoFixture, mut spec: WorkflowSpec) -> AcceptedWorkflow {
    validate_cli_spec(&mut spec, &fixture.repo).expect("acceptance spec should validate");
    let mut run = WorkflowRun::start(spec.clone(), Value::Object(Map::new())).expect("run start");
    let integration = IntegrationContext::new_if_needed(
        &fixture.repo,
        &run.id,
        &spec.allowed_tools,
        &spec.write_scopes,
        None,
        None,
    )
    .expect("integration preflight");

    if let Some(integration) = integration.as_ref() {
        run.set_integration_metadata(
            integration.base_revision.clone(),
            integration.aggregate_ref.clone(),
        );
        run.set_integrated_revision(Some(integration.base_revision.clone()));
    }
    run.set_workspace_metadata(
        fixture.repo.to_string_lossy().into_owned(),
        integration.as_ref().map(|integration| {
            integration
                .aggregate_worktree
                .to_string_lossy()
                .into_owned()
        }),
    );

    let run_id = run.id.clone();
    let store = WorkflowStore::with_path(fixture.journal.clone());
    let owner = store.acquire_run_owner(&run_id).expect("workflow owner");
    owner.append_expected(&run, None).expect("accepted run");
    let usage = Arc::new(UsageLedger::default());
    let journal = WorkflowJournal::new(owner, run, usage.clone());

    AcceptedWorkflow {
        spec,
        run_id,
        journal,
        integration,
    }
}

#[derive(Clone)]
struct DeterministicChildRunner {
    run_id: String,
    journal: WorkflowJournal,
    integration: Option<IntegrationContext>,
    calls: Arc<Mutex<Vec<String>>>,
    expand_root: bool,
    fail_task: Option<String>,
}

impl DeterministicChildRunner {
    async fn execute_task(&self, context: SwarmTaskContext) -> Result<SwarmTaskOutcome> {
        self.calls
            .lock()
            .map_err(|_| anyhow!("acceptance call log poisoned"))?
            .push(context.task.id.clone());

        if self.fail_task.as_deref() == Some(context.task.id.as_str()) {
            return Ok(SwarmTaskOutcome {
                result: TaskResult {
                    success: false,
                    output: "deterministic child failure".to_owned(),
                    files_modified: Vec::new(),
                    duration_ms: 1,
                    error: Some("deterministic child failure".to_owned()),
                },
                follow_up_tasks: Vec::new(),
            });
        }

        let follow_up_tasks = if self.expand_root && context.task.id == "root" {
            let mut discovered = SwarmTask::new("discovered", "discovered")
                .with_description("make the discovered contribution");
            discovered.files = vec!["src/discovered.txt".to_owned()];
            vec![discovered]
        } else {
            Vec::new()
        };
        let mut outcome = SwarmTaskOutcome {
            result: successful_result(match context.task.id.as_str() {
                "root" => "root contribution",
                "discovered" => "discovered contribution",
                "final" => "final sink",
                _ => "deterministic contribution",
            }),
            follow_up_tasks,
        };

        if let Some(integration) = self.integration.clone() {
            let base = integration.revision()?;
            let child = integration
                .coordinator
                .create_child_worktree(
                    &self.run_id,
                    &format!("{}-task-{}", self.run_id, context.task.id),
                    &base,
                )
                .map_err(|error| anyhow!("create acceptance child worktree: {error}"))?;
            let result = async {
                if let Some(file_name) = match context.task.id.as_str() {
                    "root" => Some("root.txt"),
                    "discovered" => Some("discovered.txt"),
                    _ => None,
                } {
                    fs::write(
                        child.path().join("src").join(file_name),
                        format!("{}\n", context.task.id),
                    )
                    .with_context(|| format!("write acceptance child {file_name}"))?;
                }
                if outcome.result.success {
                    if let Some(integrated) = integration.integrate_child(&context.task, &child)? {
                        self.journal
                            .record_integrated_revision(&integrated.receipt.result_sha)
                            .await?;
                        outcome.result.files_modified = integrated.receipt.changed_paths.clone();
                        outcome.result.output = format!(
                            "{}\n\nIntegrated revision: {}",
                            outcome.result.output, integrated.receipt.result_sha
                        );
                    }
                }
                Ok::<(), anyhow::Error>(())
            }
            .await;
            if let Err(error) = &result {
                eprintln!(
                    "acceptance child {} callback failed: {error:#}",
                    context.task.id
                );
            }
            child.abort();
            result?;
        }

        Ok(outcome)
    }
}

impl super::WorkflowChildRunner for DeterministicChildRunner {
    fn execute(&self, context: SwarmTaskContext) -> super::WorkflowFuture {
        let runner = self.clone();
        Box::pin(async move { runner.execute_task(context).await })
    }
}

async fn scheduler_terminal_snapshot(
    accepted: &AcceptedWorkflow,
    runner: DeterministicChildRunner,
) -> Result<SwarmSnapshot> {
    let executor = SwarmExecutor::new_with_run_id(
        swarm_plan(&accepted.spec),
        swarm_config(&accepted.spec),
        accepted.run_id.clone(),
    )?;
    let persist_journal = accepted.journal.clone();
    let hooks = SwarmRecoveryHooks::new(
        move |snapshot| {
            let journal = persist_journal.clone();
            async move { journal.persist_snapshot(snapshot).await }
        },
        |_reservation| async {
            Ok(RecoveryDecision::KeepIndeterminate(
                "acceptance fixture has no child receipt".to_owned(),
            ))
        },
    );
    let runner_for_callback = runner.clone();
    executor
        .run_expanding_with_recovery(
            accepted.spec.max_agents as usize,
            move |context| {
                let runner = runner_for_callback.clone();
                async move { runner.execute(context).await }
            },
            hooks,
        )
        .await?;
    let snapshot = executor.snapshot().await;
    accepted.journal.persist_snapshot(snapshot.clone()).await?;
    Ok(snapshot)
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn expanded_workflow_integrates_and_verifies_exact_aggregate_without_touching_checkout() {
    let fixture = RepoFixture::new("expanded");
    let initial_head = git_revision(&fixture.repo, "HEAD");
    let initial_status = git_status(&fixture.repo);
    let accepted = accepted_workflow(&fixture, workflow_spec("expanded", verifying_aggregate()));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let runner = DeterministicChildRunner {
        run_id: accepted.run_id.clone(),
        journal: accepted.journal.clone(),
        integration: accepted.integration.clone(),
        calls: Arc::clone(&calls),
        expand_root: true,
        fail_task: None,
    };

    let exit_code = execute_workflow_with_runner(
        WorkflowExecution {
            cwd: fixture.repo.clone(),
            spec: accepted.spec.clone(),
            journal: accepted.journal.clone(),
            integration: accepted.integration.clone(),
            snapshot: None,
            json_output: false,
        },
        Arc::new(runner),
        CancellationToken::new(),
    )
    .await
    .expect("expanded workflow should complete");
    assert_eq!(exit_code, 0);
    assert_eq!(
        calls.lock().expect("call log").as_slice(),
        [
            "root".to_owned(),
            "discovered".to_owned(),
            "final".to_owned()
        ]
        .as_slice()
    );

    let run = accepted.journal.run_copy().await;
    assert_eq!(run.status, WorkflowRunStatus::Complete);
    assert_eq!(run.verification_results.len(), 1);
    assert!(run.verification_results[0].success);
    let integration = accepted.integration.as_ref().expect("Git integration");
    let aggregate_sha = git_revision(&fixture.repo, &integration.aggregate_ref);
    assert_eq!(run.verification_results[0].output.trim(), aggregate_sha);
    assert_eq!(
        run.integrated_revision.as_deref(),
        Some(aggregate_sha.as_str())
    );
    assert_eq!(
        run.verification_results[0].result_sha.as_deref(),
        Some(aggregate_sha.as_str())
    );
    assert!(
        run.final_output
            .as_deref()
            .is_some_and(|output| output.contains("final [ok]: final sink"))
    );
    assert!(
        !run.final_output
            .as_deref()
            .unwrap_or_default()
            .contains("root [ok]")
    );
    assert!(
        !run.final_output
            .as_deref()
            .unwrap_or_default()
            .contains("discovered [ok]")
    );

    assert_eq!(git_revision(&fixture.repo, "HEAD"), initial_head);
    assert_eq!(git_status(&fixture.repo), initial_status);
    assert!(!fixture.repo.join("src/root.txt").exists());
    assert!(!fixture.repo.join("src/discovered.txt").exists());
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn terminal_snapshot_resume_skips_children_before_verification() {
    let fixture = RepoFixture::new("terminal-resume");
    let initial_head = git_revision(&fixture.repo, "HEAD");
    let accepted = accepted_workflow(
        &fixture,
        workflow_spec("terminal-resume", verifying_aggregate()),
    );
    let calls = Arc::new(Mutex::new(Vec::new()));
    let runner = DeterministicChildRunner {
        run_id: accepted.run_id.clone(),
        journal: accepted.journal.clone(),
        integration: accepted.integration.clone(),
        calls: Arc::clone(&calls),
        expand_root: true,
        fail_task: None,
    };
    let snapshot = scheduler_terminal_snapshot(&accepted, runner.clone())
        .await
        .expect("scheduler should produce a terminal snapshot");
    assert_eq!(snapshot.status, SwarmStatus::Completed);
    assert_eq!(
        accepted.journal.run_copy().await.status,
        WorkflowRunStatus::Running
    );
    calls.lock().expect("call log").clear();

    let exit_code = execute_workflow_with_runner(
        WorkflowExecution {
            cwd: fixture.repo.clone(),
            spec: accepted.spec.clone(),
            journal: accepted.journal.clone(),
            integration: accepted.integration.clone(),
            snapshot: Some(snapshot),
            json_output: false,
        },
        Arc::new(runner),
        CancellationToken::new(),
    )
    .await
    .expect("terminal snapshot should finish verification");
    assert_eq!(exit_code, 0);
    assert!(calls.lock().expect("call log").is_empty());

    let run = accepted.journal.run_copy().await;
    let integration = accepted.integration.as_ref().expect("Git integration");
    let aggregate_sha = git_revision(&fixture.repo, &integration.aggregate_ref);
    assert_eq!(run.status, WorkflowRunStatus::Complete);
    assert_eq!(run.verification_results[0].output.trim(), aggregate_sha);
    assert_eq!(
        run.integrated_revision.as_deref(),
        Some(aggregate_sha.as_str())
    );
    assert_eq!(
        run.verification_results[0].result_sha.as_deref(),
        Some(aggregate_sha.as_str())
    );
    assert_eq!(git_revision(&fixture.repo, "HEAD"), initial_head);
    assert_eq!(git_status(&fixture.repo), "");
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn indeterminate_verifier_reservation_is_not_replayed_or_left_with_a_worktree() {
    let fixture = RepoFixture::new("verifier-recovery");
    let marker = fixture._root.path().join("verifier-ran");
    let accepted = accepted_workflow(
        &fixture,
        workflow_spec("verifier-recovery", verifier_writing_marker(&marker)),
    );
    let calls = Arc::new(Mutex::new(Vec::new()));
    let runner = DeterministicChildRunner {
        run_id: accepted.run_id.clone(),
        journal: accepted.journal.clone(),
        integration: accepted.integration.clone(),
        calls: Arc::clone(&calls),
        expand_root: true,
        fail_task: None,
    };
    let snapshot = scheduler_terminal_snapshot(&accepted, runner.clone())
        .await
        .expect("scheduler should produce a terminal snapshot");
    let integration = accepted.integration.as_ref().expect("Git integration");
    let aggregate_sha = integration.revision().expect("aggregate revision");
    accepted
        .journal
        .begin_verification(0, "sh", Some(&aggregate_sha))
        .await
        .expect("reserve unknown verifier");
    calls.lock().expect("call log").clear();
    let before_worktrees = worktree_paths(&fixture.repo);

    let exit_code = execute_workflow_with_runner(
        WorkflowExecution {
            cwd: fixture.repo.clone(),
            spec: accepted.spec.clone(),
            journal: accepted.journal.clone(),
            integration: accepted.integration.clone(),
            snapshot: Some(snapshot),
            json_output: false,
        },
        Arc::new(runner),
        CancellationToken::new(),
    )
    .await
    .expect("unknown verifier should become resumable input");
    let after_worktrees = worktree_paths(&fixture.repo);
    remove_extra_worktrees(&fixture.repo, &before_worktrees, &after_worktrees);

    assert_eq!(exit_code, 2);
    assert!(calls.lock().expect("call log").is_empty());
    assert!(!marker.exists());
    assert_eq!(before_worktrees, after_worktrees);
    let run = accepted.journal.run_copy().await;
    assert_eq!(run.status, WorkflowRunStatus::NeedsInput);
    assert!(run.verification_results.is_empty());
    assert!(run.verification_reservations.contains_key("0"));
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn failed_child_stays_failed_when_a_verifier_is_required() {
    let fixture = RepoFixture::new("failed-child");
    let marker = fixture._root.path().join("failed-verifier-ran");
    let accepted = accepted_workflow(
        &fixture,
        workflow_spec("failed-child", verifier_writing_marker(&marker)),
    );
    let calls = Arc::new(Mutex::new(Vec::new()));
    let runner = DeterministicChildRunner {
        run_id: accepted.run_id.clone(),
        journal: accepted.journal.clone(),
        integration: accepted.integration.clone(),
        calls: Arc::clone(&calls),
        expand_root: false,
        fail_task: Some("root".to_owned()),
    };

    let exit_code = execute_workflow_with_runner(
        WorkflowExecution {
            cwd: fixture.repo.clone(),
            spec: accepted.spec.clone(),
            journal: accepted.journal.clone(),
            integration: accepted.integration.clone(),
            snapshot: None,
            json_output: false,
        },
        Arc::new(runner),
        CancellationToken::new(),
    )
    .await
    .expect("failed scheduler should still persist its terminal run");

    assert_eq!(exit_code, 1);
    assert_eq!(
        calls.lock().expect("call log").as_slice(),
        ["root".to_owned()].as_slice()
    );
    assert!(
        !marker.exists(),
        "required verifier must not run after child failure"
    );
    let run = accepted.journal.run_copy().await;
    assert_eq!(run.status, WorkflowRunStatus::Failed);
    assert!(
        run.status_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("scheduler"))
    );
    assert!(run.verification_results.is_empty());
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn verifier_changing_head_after_successful_exit_is_not_accepted() {
    let fixture = RepoFixture::new("verifier-head-change");
    let initial_head = git_revision(&fixture.repo, "HEAD");
    let initial_status = git_status(&fixture.repo);
    let accepted = accepted_workflow(
        &fixture,
        workflow_spec("verifier-head-change", verifier_changes_head()),
    );
    let calls = Arc::new(Mutex::new(Vec::new()));
    let runner = DeterministicChildRunner {
        run_id: accepted.run_id.clone(),
        journal: accepted.journal.clone(),
        integration: accepted.integration.clone(),
        calls: Arc::clone(&calls),
        expand_root: false,
        fail_task: None,
    };

    let exit_code = execute_workflow_with_runner(
        WorkflowExecution {
            cwd: fixture.repo.clone(),
            spec: accepted.spec.clone(),
            journal: accepted.journal.clone(),
            integration: accepted.integration.clone(),
            snapshot: None,
            json_output: false,
        },
        Arc::new(runner),
        CancellationToken::new(),
    )
    .await
    .expect("changed verifier checkout should be handled as resumable input");

    assert_eq!(exit_code, 2);
    let run = accepted.journal.run_copy().await;
    assert_eq!(run.status, WorkflowRunStatus::NeedsInput);
    assert!(
        run.status_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("indeterminate"))
    );
    assert!(run.verification_results.is_empty());
    assert!(run.verification_reservations.contains_key("0"));
    assert_eq!(git_revision(&fixture.repo, "HEAD"), initial_head);
    assert_eq!(git_status(&fixture.repo), initial_status);
}

#[cfg(unix)]
fn process_is_alive(pid: i32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(unix)]
fn terminate_process(pid: i32) {
    let _ = Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status();
}

#[cfg(unix)]
fn read_pid(path: &Path) -> Option<i32> {
    fs::read_to_string(path)
        .ok()
        .and_then(|contents| contents.trim().parse().ok())
}

#[cfg(unix)]
fn wait_for_pid(path: &Path) -> Option<i32> {
    for _ in 0..100 {
        if let Some(pid) = read_pid(path) {
            return Some(pid);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    None
}

#[cfg(unix)]
fn wait_for_process_exit(pid: i32) -> bool {
    for _ in 0..100 {
        if !process_is_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn verifier_leader_exit_drains_background_descendant_pipes() {
    #[cfg(target_os = "linux")]
    {
        // Earlier tool execution makes this process own orphaned descendants.
        // Reproduce that production context before starting the verifier.
        // SAFETY: this process-wide ownership flag takes integer arguments.
        assert_eq!(
            unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) },
            0,
            "the verifier fixture must own its orphaned descendants"
        );
    }
    let fixture = RepoFixture::new("verifier-descendant");
    let initial_head = git_revision(&fixture.repo, "HEAD");
    let pid_file = fixture._root.path().join("verifier-descendant.pid");
    let accepted = accepted_workflow(
        &fixture,
        workflow_spec(
            "verifier-descendant",
            verifier_with_background_descendant(&pid_file),
        ),
    );
    let calls = Arc::new(Mutex::new(Vec::new()));
    let runner = DeterministicChildRunner {
        run_id: accepted.run_id.clone(),
        journal: accepted.journal.clone(),
        integration: accepted.integration.clone(),
        calls: Arc::clone(&calls),
        expand_root: false,
        fail_task: None,
    };
    // Build the completed child snapshot before starting the bound.  This
    // keeps scratch worktree and integration cost out of the pipe-drain
    // assertion, while the second call still uses the production resume and
    // verifier path.
    let snapshot = scheduler_terminal_snapshot(&accepted, runner.clone())
        .await
        .expect("scheduler should finish child setup before the verifier bound");
    assert_eq!(snapshot.status, SwarmStatus::Completed);
    assert_eq!(
        calls.lock().expect("call log").as_slice(),
        ["root".to_owned(), "final".to_owned()].as_slice()
    );
    calls.lock().expect("call log").clear();

    let started = Instant::now();

    let workflow_result = tokio::time::timeout(
        Duration::from_secs(3),
        execute_workflow_with_runner(
            WorkflowExecution {
                cwd: fixture.repo.clone(),
                spec: accepted.spec.clone(),
                journal: accepted.journal.clone(),
                integration: accepted.integration.clone(),
                snapshot: Some(snapshot),
                json_output: false,
            },
            Arc::new(runner),
            CancellationToken::new(),
        ),
    )
    .await;
    let verifier_elapsed = started.elapsed();
    let pid = wait_for_pid(&pid_file).expect("verifier should record its descendant PID");
    let drained = wait_for_process_exit(pid);
    if !drained {
        terminate_process(pid);
    }

    let exit_code = workflow_result
        .expect("background verifier must not hold acceptance past the bound")
        .expect("verifier with a drained descendant should succeed");
    assert!(
        drained,
        "background verifier descendant remained after leader exit"
    );
    assert!(verifier_elapsed < Duration::from_secs(3));
    assert_eq!(exit_code, 0);
    assert!(calls.lock().expect("call log").is_empty());
    assert_eq!(
        accepted.journal.run_copy().await.status,
        WorkflowRunStatus::Complete
    );
    assert_eq!(git_revision(&fixture.repo, "HEAD"), initial_head);
}

#[test]
fn five_hundred_predecessor_outputs_leave_a_coherent_final_sink() {
    let mut tasks = Vec::with_capacity(501);
    for index in 0..500 {
        let mut task = SwarmTask::new(format!("predecessor-{index}"), "predecessor")
            .with_description("large predecessor output");
        task.result = Some(successful_result("x".repeat(4 * 1024)));
        tasks.push(task);
    }
    let dependencies = (0..500)
        .map(|index| format!("predecessor-{index}"))
        .collect::<Vec<_>>();
    let mut final_task = SwarmTask::new("final", "final").with_dependencies(dependencies);
    final_task.result = Some(successful_result("final sink survives"));
    tasks.push(final_task);

    let mut state = maestro_swarm::SwarmState::default();
    state.status = SwarmStatus::Completed;
    state.plan = SwarmPlan::new("large-output").with_tasks(tasks);
    state.completed_tasks = state
        .plan
        .tasks
        .iter()
        .map(|task| task.id.clone())
        .collect();
    let output = aggregate_result(&state, Some("aggregate-sha"), &[]);

    assert!(output.contains("final [ok]: final sink survives"));
    assert!(!output.contains("predecessor-0 [ok]"));
    assert!(output.contains("tasks: 501 completed, 0 failed"));
    assert!(output.contains("integrated_revision: aggregate-sha"));
    assert!(output.len() <= MAX_RESULT_OUTPUT_BYTES);
}
