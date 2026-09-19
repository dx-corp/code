//! Exercise the registered dispatcher with real Git and governed Bash execution.
//! Mission environment overrides belong to a single-test subprocess, so these
//! fixtures cannot race tests that use the process-wide mission-store setting.
use super::*;

const CHILD_MARKER: &str = "MAESTRO_CODING_DISPATCH_TEST_CHILD";
const MISSION: &str = "dispatcher-mission";

fn isolated_child(name: &str) -> bool {
    let exact = format!("tools::registry::coding_task::dispatcher_tests::{name}");
    if std::env::var(CHILD_MARKER).as_deref() == Ok(exact.as_str()) {
        return true;
    }
    let temp = tempfile::tempdir().unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &exact, "--nocapture", "--test-threads=1"])
        .env(CHILD_MARKER, &exact)
        .env("MAESTRO_MISSION_STORE_DIR", temp.path().join("missions"))
        .env("MAESTRO_SUBAGENTS_DIR", temp.path().join("subagents"))
        .env("MAESTRO_HOME", temp.path().join("maestro"))
        .current_dir(temp.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "isolated dispatcher test failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "the exact child test must execute: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    false
}

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    executor: ToolExecutor,
    begin: Value,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(temp.path()).unwrap();
        std::fs::write(root.join("source.txt"), "original\n").unwrap();
        for args in [
            vec!["init", "--quiet"],
            vec!["add", "source.txt"],
            vec![
                "-c",
                "user.name=Dispatcher Test",
                "-c",
                "user.email=dispatcher@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        ] {
            let output = Command::new("git")
                .args(args)
                .current_dir(&root)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "fixture Git failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let executor = ToolExecutor::new(root.to_string_lossy().into_owned());
        executor.set_subagent_parent_scope("session:dispatcher-implementation".into());
        let begin = json!({
            "action":"begin", "mission_id":MISSION, "work_id":"dispatcher-work",
            "contract": {
                "taskId":"feature", "repositoryId":root, "generation":1,
                "requiredAssertionIds":["works"], "requireReview":true,
                "requireBehavior":true, "readinessRequirements":["test"],
                "authorizedSkips":[], "authorizedDispositions":[]
            }
        });
        Self {
            _temp: temp,
            root,
            executor,
            begin,
        }
    }

    async fn action(&self, args: Value) -> ToolResult {
        self.executor
            .execute("coding_task", &args, None, "coding-action")
            .await
    }

    async fn begin(&self) {
        let result = self.action(self.begin.clone()).await;
        assert!(result.success, "begin failed: {result:?}");
    }

    fn manifest(&self, command: &str) {
        let layout = get_mission_artifact_layout(MISSION, None).unwrap();
        let value = json!({
            "version":1,
            "commands":{"fixture-test":{"command":command,"timeoutMs":10000}},
            "readiness":{"feature":{
                "build":{"notApplicable":"No compilation in this text fixture"},
                "test":{"command":"fixture-test"},
                "start":{"notApplicable":"No service in this fixture"},
                "authentication":{"notApplicable":"No authenticated dependency"},
                "observation":{"notApplicable":"The test observes the fixture"}
            }}
        });
        std::fs::write(layout.services_yaml, serde_yaml::to_string(&value).unwrap()).unwrap();
    }

    async fn readiness(&self) -> Value {
        let result = self.action(json!({"action":"readiness"})).await;
        assert!(result.success, "readiness failed: {result:?}");
        result.details.unwrap()
    }

    async fn status(&self) -> Value {
        let result = self.action(json!({"action":"status"})).await;
        assert!(result.success, "status failed: {result:?}");
        result.details.unwrap()
    }
}

/// A real mission-file failure, independent of Unix permissions/root privileges.
/// Restore the original bytes even if an assertion panics.
struct UnavailableMissionState {
    path: PathBuf,
    backup: PathBuf,
}

impl UnavailableMissionState {
    fn inject() -> Self {
        let path = get_mission_artifact_layout(MISSION, None)
            .unwrap()
            .state_json;
        let backup = path.with_extension("saved-for-dispatcher-test");
        std::fs::rename(&path, &backup).unwrap();
        std::fs::create_dir(&path).unwrap();
        Self { path, backup }
    }
}

impl Drop for UnavailableMissionState {
    fn drop(&mut self) {
        std::fs::remove_dir(&self.path).unwrap();
        std::fs::rename(&self.backup, &self.path).unwrap();
    }
}

#[tokio::test]
async fn dispatcher_does_not_publish_handoff_or_readiness_when_mission_persistence_fails() {
    if !isolated_child(
        "dispatcher_does_not_publish_handoff_or_readiness_when_mission_persistence_fails",
    ) {
        return;
    }
    let fixture = Fixture::new();
    fixture.begin().await;
    fixture.manifest("cat source.txt");
    let plan = fixture.readiness().await;
    let output = fixture
        .executor
        .execute("bash", &plan["bashCalls"][0], None, "initial-proof")
        .await;
    assert!(output.success, "{output:?}");
    let open = json!({"action":"handoff","item":{
        "id":"unfinished", "disposition":"open", "evidenceRefs":["source.txt:1"]
    }});
    assert!(fixture.action(open.clone()).await.success);
    let before = fixture.status().await;
    let disk_before = std::fs::read(
        get_mission_artifact_layout(MISSION, None)
            .unwrap()
            .state_json,
    )
    .unwrap();

    let unavailable = UnavailableMissionState::inject();
    let mut resolved = open;
    resolved["item"]["disposition"] = json!("resolved");
    assert!(!fixture.action(resolved.clone()).await.success);
    assert_eq!(fixture.status().await, before);
    fixture.manifest("test -f source.txt");
    assert!(!fixture.action(json!({"action":"readiness"})).await.success);
    assert_eq!(fixture.status().await, before);

    // An actual command whose receipt cannot be persisted must also invalidate
    // any old success, so the caller cannot complete using stale evidence.
    let output = fixture
        .executor
        .execute("bash", &plan["bashCalls"][0], None, "unpersisted-proof")
        .await;
    assert!(
        !output.success,
        "unpersisted command reported success: {output:?}"
    );
    let failed = fixture.status().await;
    assert_eq!(failed["handoffItems"], before["handoffItems"]);
    assert_eq!(failed["readiness"][0]["status"], "blocked");
    assert_eq!(failed["completed"], false);
    drop(unavailable);
    assert_eq!(
        std::fs::read(
            get_mission_artifact_layout(MISSION, None)
                .unwrap()
                .state_json
        )
        .unwrap(),
        disk_before
    );
    assert!(fixture.action(resolved).await.success);
    fixture.readiness().await;
    assert_eq!(
        fixture.status().await["handoffItems"][0]["disposition"],
        "resolved"
    );
}

#[tokio::test]
async fn dispatcher_completes_with_actual_terminal_producer_receipts_only_after_persistence() {
    if !isolated_child(
        "dispatcher_completes_with_actual_terminal_producer_receipts_only_after_persistence",
    ) {
        return;
    }
    use crate::tools::subagents::{
        SubagentBackend, SubagentIsolation, SubagentRecord, SubagentRole, SubagentStatus,
    };
    let fixture = Fixture::new();
    fixture.begin().await;
    fixture.manifest("cat source.txt");
    let plan = fixture.readiness().await;
    let command = fixture
        .executor
        .execute("bash", &plan["bashCalls"][0], None, "verified-command")
        .await;
    assert!(command.success, "{command:?}");
    let head = plan["revision"].as_str().unwrap();
    let child_worktrees = tempfile::tempdir().unwrap();
    let mut child_ids = vec![];
    for (name, role, builtin) in [
        (
            "review",
            SubagentRole::Review,
            BuiltinValidatorRole::CodingReviewer,
        ),
        (
            "behavior",
            SubagentRole::Code,
            BuiltinValidatorRole::CodingFlowValidator,
        ),
    ] {
        let child_root = child_worktrees.path().join(name);
        let output = Command::new("git")
            .args(["worktree", "add", "--quiet", "--detach"])
            .arg(&child_root)
            .arg(head)
            .current_dir(&fixture.root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let child_root = dunce::canonicalize(child_root).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let profile = crate::agents_cli::trusted_builtin_validator_profile(builtin);
        let record = SubagentRecord {
            parent_requests: Vec::new(),
            id: id.clone(),
            parent_scope_id: "session:dispatcher-implementation".into(),
            parent_call_id: format!("validate-{name}"),
            last_parent_scope_id: "session:dispatcher-implementation".into(),
            last_call_id: format!("validate-{name}"),
            task: "Validate fixture assertion".into(),
            current_prompt: "Validate fixture assertion".into(),
            role,
            backend: SubagentBackend::Native,
            orb: None,
            profile: Some(profile.name),
            profile_prompt: None,
            profile_tools: None,
            model: None,
            thinking: None,
            difficulty: Default::default(),
            timeout_ms: 10000,
            max_tokens: 1000,
            isolation: SubagentIsolation::Worktree,
            // Production records encode path bytes. Exercise both that wire
            // representation and legacy plain UTF-8 records through completion.
            cwd: if name == "behavior" {
                use std::fmt::Write as _;
                let mut encoded = String::new();
                for byte in child_root.to_string_lossy().as_bytes() {
                    write!(&mut encoded, "{byte:02x}").unwrap();
                }
                format!("\0maestro-path-v1:{encoded}")
            } else {
                child_root.to_string_lossy().into_owned()
            },
            worktree_path: Some(child_root.to_string_lossy().into_owned()),
            worktree_cleaned: false,
            initial_files: vec![],
            initial_file_fingerprints: Default::default(),
            initial_head: Some(head.into()),
            session_dir: child_worktrees
                .path()
                .join(format!("session-{name}"))
                .to_string_lossy()
                .into_owned(),
            status: SubagentStatus::Running,
            attempt: 1,
            snapshot_attempt: None,
            created_at_ms: 1,
            started_at_ms: Some(1),
            finished_at_ms: None,
            result: None,
            error: None,
            lifecycle_notification_published: false,
        };
        // This is the real terminal producer, including credential translation,
        // record persistence and runtime-held sealing; no model/network is used.
        fixture.executor.subagents.finish_coding_validator_for_test(record, json!({
            "revision":head,
            "assertions":[{"id":"works","status":"passed","evidence":["source.txt:1"],"observation":"The committed fixture contains original"}],
            "summary":"Fixture assertion passed"
        }).to_string()).unwrap();
        child_ids.push(id);
    }
    {
        // Supply the two launch handles normally returned by trusted dispatch.
        // Completion still has to collect and validate actual owner-held records.
        let mut locked = fixture.executor.coding_task.lock().unwrap();
        let state = locked.as_mut().unwrap();
        state.review_child = Some(child_ids[0].clone());
        state.behavior_child = Some(child_ids[1].clone());
        state.persist().unwrap();
    }
    let unavailable = UnavailableMissionState::inject();
    let failed = fixture.action(json!({"action":"complete"})).await;
    assert!(
        !failed.success,
        "unpersisted completion succeeded: {failed:?}"
    );
    assert_eq!(fixture.status().await["completed"], false);
    assert!(fixture.executor.coding_completion().is_err());
    drop(unavailable);

    let completed = fixture.action(json!({"action":"complete", "outputs":[{"path":"forged.py","content":"model supplied bytes"}]})).await;
    assert!(completed.success, "completion failed: {completed:?}");
    assert_eq!(completed.details.unwrap()["accepted"], true);
    let (_, proof, children) = fixture.executor.coding_completion().unwrap().unwrap();
    assert_eq!(proof.revision, head);
    assert!(
        proof.outputs.is_empty(),
        "model arguments cannot fabricate native captured outputs"
    );
    assert_eq!(children.len(), 2);
    assert_ne!(children[0].session_id, children[1].session_id);
    let store = MissionStore::load(MISSION, MissionStoreConfig::default()).unwrap();
    assert_eq!(
        store.get_snapshot().unwrap().features[0]["status"],
        "passed"
    );
}

#[tokio::test]
async fn dispatcher_records_actual_bash_readiness_and_rejects_unvalidated_completion() {
    if !isolated_child(
        "dispatcher_records_actual_bash_readiness_and_rejects_unvalidated_completion",
    ) {
        return;
    }
    let fixture = Fixture::new();
    fixture.begin().await;
    let layout = get_mission_artifact_layout(MISSION, None).unwrap();
    assert!(layout.state_json.is_file());
    assert!(layout.validation_contract_markdown.is_file());
    fixture.manifest("cat source.txt");
    let plan = fixture.readiness().await;
    assert_eq!(plan["bashCalls"].as_array().unwrap().len(), 1);
    assert_eq!(fixture.status().await["readiness"][0]["status"], "blocked");

    let fabricated = fixture
        .action(json!({
            "action":"complete", "revision":plan["revision"],
            "commands":[{"command":"cat source.txt","exitCode":0,"evidenceRefs":["invented"]}],
            "review":{"status":"passed"}, "behavior":{"status":"passed"}
        }))
        .await;
    assert!(!fabricated.success);
    assert_eq!(fixture.status().await["completed"], false);

    let output = fixture
        .executor
        .execute(
            "bash",
            &plan["bashCalls"][0],
            None,
            "actual-readiness-command",
        )
        .await;
    assert!(output.success, "Bash failed: {output:?}");
    assert!(output.output.contains("original"));
    let status = fixture.status().await;
    assert_eq!(status["readiness"][0]["status"], "passed");
    assert_eq!(
        status["readiness"][0]["evidenceRefs"],
        json!(["session:dispatcher-implementation/tool:actual-readiness-command"])
    );
    assert!(!fixture.action(json!({"action":"complete"})).await.success);
    assert!(fixture.executor.coding_completion().is_err());
}

#[tokio::test]
async fn dispatcher_restores_open_handoffs_and_refuses_replacement_or_corrupt_state() {
    if !isolated_child("dispatcher_restores_open_handoffs_and_refuses_replacement_or_corrupt_state")
    {
        return;
    }
    let fixture = Fixture::new();
    let mut invalid = fixture.begin.clone();
    invalid["contract"]["requireReview"] = json!(false);
    assert!(!fixture.action(invalid).await.success);
    assert!(fixture.executor.coding_contract().is_none());
    fixture.begin().await;
    let open = json!({"action":"handoff","item":{
        "id":"unfinished", "disposition":"open", "evidenceRefs":["source.txt:1"]
    }});
    assert!(fixture.action(open.clone()).await.success);
    for disposition in ["deferred", "dismissed"] {
        let mut unauthorized = open.clone();
        unauthorized["item"]["disposition"] = json!(disposition);
        assert!(!fixture.action(unauthorized).await.success);
    }
    assert_eq!(
        fixture.status().await["handoffItems"][0]["disposition"],
        "open"
    );
    assert!(
        !fixture
            .action(json!({"action":"validate","role":"review"}))
            .await
            .success
    );
    assert!(!fixture.action(json!({"action":"complete"})).await.success);

    fixture.executor.reset_coding_turn();
    let mut replacement = fixture.begin.clone();
    replacement["contract"]["requiredAssertionIds"] = json!(["easier"]);
    assert!(!fixture.action(replacement).await.success);
    let mut other_work = fixture.begin.clone();
    other_work["work_id"] = json!("replacement-work");
    assert!(!fixture.action(other_work).await.success);
    fixture.begin().await;
    assert_eq!(
        fixture.status().await["handoffItems"],
        json!([open["item"].clone()])
    );
    assert_eq!(fixture.status().await["completed"], false);

    fixture.executor.reset_coding_turn();
    let layout = get_mission_artifact_layout(MISSION, None).unwrap();
    std::fs::write(&layout.state_json, "{malformed").unwrap();
    assert!(!fixture.action(fixture.begin.clone()).await.success);
    assert_eq!(
        std::fs::read_to_string(&layout.state_json).unwrap(),
        "{malformed"
    );
    std::fs::remove_file(&layout.state_json).unwrap();
    assert!(!fixture.action(fixture.begin.clone()).await.success);
    assert!(!layout.state_json.exists());
}

#[tokio::test]
async fn dispatcher_rejects_dirty_readiness_and_commands_that_change_source() {
    if !isolated_child("dispatcher_rejects_dirty_readiness_and_commands_that_change_source") {
        return;
    }
    let fixture = Fixture::new();
    fixture.begin().await;
    fixture.manifest("cat source.txt");
    std::fs::write(fixture.root.join("source.txt"), "uncommitted\n").unwrap();
    assert!(!fixture.action(json!({"action":"readiness"})).await.success);
    std::fs::write(fixture.root.join("source.txt"), "original\n").unwrap();
    let clean_plan = fixture.readiness().await;
    std::fs::write(fixture.root.join("source.txt"), "changed-after-planning\n").unwrap();
    let dirty_execution = fixture
        .executor
        .execute(
            "bash",
            &clean_plan["bashCalls"][0],
            None,
            "dirty-before-execution",
        )
        .await;
    assert!(
        !dirty_execution.success,
        "a clean plan cannot authorize dirty-source readiness: {dirty_execution:?}"
    );
    assert_ne!(fixture.status().await["readiness"][0]["status"], "passed");
    std::fs::write(fixture.root.join("source.txt"), "original\n").unwrap();

    fixture.manifest("printf changed > source.txt");
    let plan = fixture.readiness().await;
    let output = fixture
        .executor
        .execute(
            "bash",
            &plan["bashCalls"][0],
            None,
            "mutating-readiness-command",
        )
        .await;
    assert!(
        !output.success,
        "source-changing readiness cannot produce proof: {output:?}"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("source.txt")).unwrap(),
        "changed"
    );
    assert_ne!(fixture.status().await["readiness"][0]["status"], "passed");
    assert!(fixture.executor.coding_completion().is_err());
}
