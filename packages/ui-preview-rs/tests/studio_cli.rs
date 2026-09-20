use std::{fs, process::Command};

fn run(args: &[&str]) -> std::process::Output {
    let test_exe = std::env::current_exe().unwrap();
    let binary = test_exe
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("maestro-ui-preview");
    Command::new(binary)
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap()
}

#[test]
fn parser_fails_closed_for_missing_ambiguous_and_unknown_arguments() {
    for (args, message) in [
        (vec!["studio", "new", "story"], "requires --adapter"),
        (
            vec!["studio", "new", "--adapter", "missing", "story"],
            "unknown adapter",
        ),
        (
            vec!["studio", "new", "--adapter", "shared-menu", "one", "two"],
            "exactly one",
        ),
        (vec!["studio", "new", "--wat", "story"], "unknown argument"),
        (
            vec![
                "studio",
                "new",
                "--adapter",
                "shared-menu",
                "--adapter",
                "theme-selector",
                "story",
            ],
            "may be specified once",
        ),
    ] {
        let output = run(&args);
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains(message));
    }
}

#[test]
fn new_check_prints_a_non_mutating_plan() {
    let output = run(&[
        "studio",
        "new",
        "--adapter",
        "theme-selector",
        "workspace-picker",
        "--check",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["adapter"], "theme-selector");
    assert_eq!(plan["story_id"], "workspace-picker");
    assert!(
        plan["fixture"]
            .as_str()
            .unwrap()
            .ends_with("support/ui_stories/workspace_picker.rs")
    );
}

#[test]
fn verify_uses_registered_story_results() {
    let output = run(&["studio", "verify", "menu-ready"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["summary"]["story"], "menu-ready");
    assert_eq!(report["summary"]["owner"], "maestro-ui");
    assert_eq!(report["captures"].as_array().unwrap().len(), 3);
    assert_eq!(report["summary"]["assertions"], 0);
    assert_eq!(
        report["summary"]["contract_status"],
        "behavior-not-asserted"
    );
}

#[test]
fn inspect_and_strict_check_expose_the_contribution_contract() {
    let output = run(&["studio", "inspect", "menu-ready"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["owner"], "maestro-ui");
    assert_eq!(report["story"]["adapter"], "shared-menu");
    assert!(report["story"]["cases"].as_array().unwrap().len() >= 3);
    assert_eq!(report["commands"]["check"], "./dev ui check menu-ready");

    let strict = run(&["studio", "verify", "menu-ready", "--require-contract"]);
    assert_eq!(strict.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&strict.stderr).contains("behavior not asserted"));
}

#[test]
fn migrate_check_validates_a_canonical_portable_receipt() {
    let path = std::env::temp_dir().join(format!(
        "maestro-ui-receipt-{}-{}.json",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let value: serde_json::Value =
        serde_json::from_slice(include_bytes!("fixtures/recipe-v1.json")).unwrap();
    fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({
            "schema": "maestro.ui.menu-recipe",
            "version": 1,
            "value": value,
        }))
        .unwrap(),
    )
    .unwrap();
    let output = run(&["studio", "migrate", path.to_str().unwrap(), "--check"]);
    let _ = fs::remove_file(path);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn coverage_command_uses_profile_evaluation_and_declared_inventory() {
    let output = run(&["studio", "coverage"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let coverage = report["coverage"].as_array().unwrap();
    assert_eq!(coverage.len(), 2);
    assert_eq!(coverage[0]["profile"]["id"], "pr-v1");
    assert!(!coverage[0]["declared"].as_array().unwrap().is_empty());
    let pr_states = coverage[0]["states"].as_object().unwrap();
    assert!(pr_states.values().any(|state| state == "unavailable"));
    let scheduled_states = coverage[1]["states"].as_object().unwrap();
    assert!(scheduled_states.values().any(|state| state == "skipped"));
    assert!(!report["contributions"].as_array().unwrap().is_empty());
}

#[test]
fn catalog_filters_are_explicit_and_reject_ansi_ambiguity() {
    let output = run(&["--json", "--story", "menu-ready"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let captures: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(captures.as_array().unwrap().len(), 3);
    assert_eq!(captures[0]["filter"]["kind"], "story");
    let adapter = run(&["--list", "--adapter", "shared-menu"]);
    assert!(
        adapter.status.success(),
        "{}",
        String::from_utf8_lossy(&adapter.stderr)
    );
    let listed: serde_json::Value = serde_json::from_slice(&adapter.stdout).unwrap();
    assert_eq!(listed["filter"]["kind"], "adapter");
    assert!(listed["scenes"].as_array().unwrap().len() > 7);
    let invalid = run(&["--story", "menu-ready"]);
    assert_eq!(invalid.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("require --html"));
}

#[test]
fn named_profile_export_selects_and_stamps_only_profile_cases() {
    let output = run(&["--json", "--profile", "pr-v1"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let captures: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let captures = captures.as_array().unwrap();
    assert!(!captures.is_empty());
    assert!(captures.iter().all(|capture| {
        capture
            .pointer("/metadata/profile/id")
            .and_then(|id| id.as_str())
            == Some("pr-v1")
    }));
    let unknown = run(&["--json", "--profile", "unknown"]);
    assert_eq!(unknown.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("unknown coverage profile"));
}
