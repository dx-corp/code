//! The installed command must reject unsafe input before starting an agent.

macro_rules! cargo_bin {
    ($name:literal) => {{
        option_env!(concat!("CARGO_BIN_EXE_", $name))
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os(concat!("CARGO_BIN_EXE_", $name)).map(std::path::PathBuf::from)
            })
            .expect(concat!("Cargo binary path unavailable: ", $name))
    }};
}

use std::{
    fs,
    path::PathBuf,
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "maestro-workflow-dispatch-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("create test workspace");
        Self(path)
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut child = Command::new(cargo_bin!("maestro"))
            .args(args)
            .current_dir(&self.0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start native workflow command");
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if child.try_wait().expect("poll command").is_some() {
                return child.wait_with_output().expect("collect command output");
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let output = child.wait_with_output().expect("drain timed-out command");
                panic!("workflow command did not reject input before agent startup: {output:?}");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn workflow_help_reaches_the_native_workflow_dispatch() {
    for args in [
        vec!["workflow", "--help"],
        vec!["workflow", "run", "--help"],
        vec!["workflow", "resume", "--help"],
        vec!["workflow", "status", "--help"],
    ] {
        let workspace = Workspace::new();
        let output = workspace.run(&args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        let help = String::from_utf8_lossy(&output.stdout);
        assert!(help.contains("workflow"), "{help}");
        assert!(help.contains("resume"), "{help}");
        assert!(help.contains("run"), "{help}");
        assert_eq!(fs::read_dir(&workspace.0).unwrap().count(), 0);
    }
}

#[test]
fn workflow_invalid_budget_fails_before_workspace_or_provider_startup() {
    let workspace = Workspace::new();
    fs::write(
        workspace.0.join("invalid.json"),
        r#"{
            "name":"invalid-budget","version":"1",
            "steps":[{"id":"read","prompt":"Summarize this repository"}],
            "maxAgents":0,"maxConcurrency":1,"tokenBudget":100,
            "model":{"model":"unconfigured-test-model","maxOutputTokens":100},
            "allowedTools":[],"writeScopes":[],
            "verification":[{"command":"true","timeoutMs":1000}]
        }"#,
    )
    .unwrap();
    let output = workspace.run(&["workflow", "run", "invalid.json"]);
    assert!(!output.status.success(), "{output:?}");
    let diagnostic = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(diagnostic.contains("budget"), "{diagnostic}");
    assert_eq!(fs::read_dir(&workspace.0).unwrap().count(), 1);
}

#[test]
fn workflow_missing_resume_journal_never_starts_new_work() {
    let workspace = Workspace::new();
    let output = workspace.run(&["workflow", "resume", "missing-workflow.json"]);
    assert!(!output.status.success(), "{output:?}");
    assert_eq!(fs::read_dir(&workspace.0).unwrap().count(), 0);
}

#[test]
fn workflow_invalid_graphs_fail_before_creating_runtime_state() {
    for (steps, expected_diagnostic) in [
        (
            r#"[{"id":"same","prompt":"One"},{"id":"same","prompt":"Two"}]"#,
            "unique",
        ),
        (
            r#"[{"id":"one","prompt":"One","dependsOn":["missing"]}]"#,
            "missing",
        ),
        (
            r#"[{"id":"one","prompt":"One","dependsOn":["two"]},{"id":"two","prompt":"Two","dependsOn":["one"]}]"#,
            "acyclic",
        ),
    ] {
        let workspace = Workspace::new();
        let spec = format!(
            r#"{{"name":"invalid-graph","version":"1","steps":{steps},
                "maxAgents":10,"maxConcurrency":2,"tokenBudget":100,
                "model":{{"model":"unconfigured-test-model","maxOutputTokens":100}},
                "allowedTools":[],"writeScopes":[],
                "verification":[{{"command":"true","timeoutMs":1000}}]}}"#
        );
        fs::write(workspace.0.join("invalid.json"), spec).unwrap();
        let output = workspace.run(&["workflow", "run", "invalid.json"]);
        assert!(!output.status.success(), "{steps}: {output:?}");
        let diagnostic = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(diagnostic.contains(expected_diagnostic), "{diagnostic}");
        assert_eq!(fs::read_dir(&workspace.0).unwrap().count(), 1);
    }
}
