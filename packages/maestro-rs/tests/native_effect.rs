#![cfg(unix)]
use std::{fs, process::Command};
fn maestro_binary() -> std::ffi::OsString {
    option_env!("CARGO_BIN_EXE_maestro")
        .map(std::ffi::OsString::from)
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_maestro"))
        .expect("Cargo must provide the maestro integration-test binary")
}

fn git(cwd: &std::path::Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .args(args)
            .current_dir(cwd)
            .status()
            .unwrap()
            .success()
    );
}
fn effect(cwd: &std::path::Path, key: &str, command: &str) -> serde_json::Value {
    let output = Command::new(maestro_binary())
        .args(["native-code-effect", key, command])
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.len() <= 60 * 1024);
    serde_json::from_slice(&output.stdout).unwrap()
}
#[test]
fn guest_projection_probe_identifies_the_effect_protocol_without_running_commands() {
    let output = Command::new(maestro_binary())
        .args(["native-code-effect", "--protocol-version"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"maestro-native-code-effect-v1\n");
}

#[test]
fn approved_wrapper_saves_untracked_and_multiple_command_net_changes() {
    let cwd = std::env::temp_dir().join(format!("maestro-native-effect-{}", std::process::id()));
    fs::create_dir_all(&cwd).unwrap();
    git(&cwd, &["init", "-q"]);
    fs::write(cwd.join("tracked.txt"), "before\n").unwrap();
    git(&cwd, &["add", "."]);
    git(
        &cwd,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-qm",
            "initial",
        ],
    );
    let key = maestro_runtime_contracts::native_code::checkpoint_key(
        "org",
        "workspace",
        &cwd.to_string_lossy(),
    );
    let first = effect(
        &cwd,
        &key,
        "printf 'middle\\n' > tracked.txt; printf 'first\\n' > new.txt",
    );
    assert_eq!(first["changes"]["availability"], "ready");
    let second = effect(
        &cwd,
        &key,
        "printf 'after\\n' > tracked.txt; printf 'final\\n' > new.txt",
    );
    let files = second["changes"]["files"].as_array().unwrap();
    let tracked = files
        .iter()
        .find(|file| file["path"] == "tracked.txt")
        .unwrap();
    assert_eq!(tracked["beforeContent"], "before\n");
    assert_eq!(tracked["afterContent"], "after\n");
    let created = files.iter().find(|file| file["path"] == "new.txt").unwrap();
    assert_eq!(created["kind"], "created");
    assert_eq!(created["afterContent"], "final\n");
    let failed = Command::new(maestro_binary())
        .args([
            "native-code-effect",
            &key,
            "printf 'saved despite failure\\n' > failed.txt; exit 7",
        ])
        .current_dir(&cwd)
        .output()
        .unwrap();
    assert_eq!(failed.status.code(), Some(7));
    let receipt: serde_json::Value = serde_json::from_slice(&failed.stdout).unwrap();
    assert_eq!(receipt["exitCode"], 7);
    assert!(
        receipt["changes"]["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file["path"] == "failed.txt"
                && file["afterContent"] == "saved despite failure\n")
    );
    // Bounded patch content is truthful when a tool writes a larger file.
    let large = effect(
        &cwd,
        &key,
        "head -c 100000 /dev/zero | tr '\\0' 'x' > large.txt",
    );
    let large_file = large["changes"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == "large.txt")
        .unwrap();
    assert_eq!(large_file["availability"], "truncated");
    let bounded = effect(&cwd, &key, "head -c 10000 /dev/zero | tr '\\0' 'x'");
    assert_eq!(bounded["outputTruncated"], true);
    assert!(bounded["stdout"].as_str().unwrap().len() <= 4096);
    fs::remove_dir_all(&cwd).unwrap();
}

#[test]
fn first_approved_command_in_blank_workspace_saves_changes_but_broken_git_is_preserved() {
    let cwd = std::env::temp_dir().join(format!("maestro-native-blank-{}", std::process::id()));
    fs::create_dir_all(&cwd).unwrap();
    let key = maestro_runtime_contracts::native_code::checkpoint_key(
        "org",
        "workspace",
        &cwd.to_string_lossy(),
    );
    let output = effect(&cwd, &key, "printf 'first source\\n' > first.txt");
    assert!(cwd.join(".git").is_dir());
    let files = output["changes"]["files"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["path"], "first.txt");
    assert_eq!(files[0]["kind"], "created");
    assert_eq!(files[0]["afterContent"], "first source\n");
    fs::remove_dir_all(&cwd).unwrap();
    fs::create_dir_all(&cwd).unwrap();
    fs::write(cwd.join(".git"), "broken repository metadata").unwrap();
    let rejected = Command::new(maestro_binary())
        .args(["native-code-effect", &key, "touch must-not-run"])
        .current_dir(&cwd)
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(!cwd.join("must-not-run").exists());
    assert_eq!(
        fs::read_to_string(cwd.join(".git")).unwrap(),
        "broken repository metadata"
    );
    fs::remove_dir_all(cwd).unwrap();
}
