use maestro::{Command, classify};
use serde::Deserialize;
use std::ffi::OsString;
use std::process::Command as ProcessCommand;

fn maestro_binary() -> OsString {
    std::env::var_os("CARGO_BIN_EXE_maestro")
        .expect("Cargo must provide the maestro integration-test binary")
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    schema_version: u32,
    cases: Vec<FixtureCase>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureCase {
    name: String,
    argv: Vec<String>,
    route: String,
    exit_code: Option<i32>,
}

#[test]
fn serve_dispatches_to_in_process_control_plane() {
    assert_eq!(classify(["serve"]).unwrap(), Command::Serve { port: None });
    assert_eq!(
        classify(["serve", "--port", "9090"]).unwrap(),
        Command::Serve { port: Some(9090) }
    );
}

#[test]
fn exec_is_forwarded_to_native_dispatch() {
    // `classify` no longer distinguishes exec/print/headless/utility argv from
    // one another; it only needs to know whether to serve the in-process
    // control plane. Everything else forwards to `maestro_tui::run_cli`, which
    // owns the real routing decision (see `packages/tui-rs/tests/entrypoint.rs`).
    assert_eq!(classify(["exec", "hello"]).unwrap(), Command::Forward);
}

#[test]
fn doctor_is_forwarded_to_native_dispatch() {
    assert_eq!(
        classify(["doctor", "--json", "--model", "openai/gpt-4o"]).unwrap(),
        Command::Forward
    );
}

#[test]
fn primary_help_exposes_the_canonical_command_surface() {
    let output = ProcessCommand::new(maestro_binary())
        .arg("--help")
        .output()
        .expect("run maestro --help");

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Deixic Code"));
    assert!(stdout.contains("deixic-code setup"));
    assert!(stdout.contains("deixic-code config"));
    assert!(stdout.contains("deixic-code sessions"));
    assert!(stdout.contains("maestro remains available as an alias"));
    assert!(!stdout.contains("maestro-tui"));
}

#[test]
fn frozen_cli_routes_are_owned_by_native_dispatch() {
    let fixture: Fixture = serde_json::from_str(include_str!(
        "../../../test/fixtures/rust-cutover/cli-routing.json"
    ))
    .expect("parse CLI routing fixture");
    assert_eq!(fixture.schema_version, 1);

    for case in fixture.cases {
        let result = classify(case.argv.into_iter().map(OsString::from));
        if case.exit_code.is_some_and(|code| code != 0) {
            assert!(result.is_err(), "{} should reject invalid argv", case.name);
            continue;
        }
        let command = result.unwrap_or_else(|error| panic!("{}: {error}", case.name));
        match (case.route.as_str(), case.name.as_str()) {
            ("native-control-plane", _) => {
                assert_eq!(command, Command::Serve { port: None }, "{}", case.name)
            }
            ("native", "version") => assert_eq!(command, Command::Version, "{}", case.name),
            ("native", "help" | "hidden-help") => {
                assert_eq!(command, Command::Help, "{}", case.name)
            }
            ("native", _) | ("native-tui", _) | ("native-headless", _) => {
                assert_eq!(command, Command::Forward, "{}", case.name)
            }
            other => panic!("{}: unknown fixture route {other:?}", case.name),
        }
    }
}

#[test]
fn serve_rejects_prompt_arguments() {
    assert!(classify(["serve", "prompt"]).is_err());
}

#[test]
fn hosted_runner_help_reaches_the_native_hosted_runner_dispatch() {
    let output = ProcessCommand::new(maestro_binary())
        .args(["hosted-runner", "--help"])
        .output()
        .expect("run maestro hosted-runner help");

    assert!(
        output.status.success(),
        "hosted-runner help failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("Usage: deixic-code hosted-runner"),
        "primary compatibility binary did not reach the hosted-runner CLI: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn update_help_reaches_the_native_update_dispatch() {
    let output = ProcessCommand::new(maestro_binary())
        .args(["update", "--help"])
        .output()
        .expect("run maestro update --help");

    assert!(
        output.status.success(),
        "update help failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("Usage: deixic-code update"),
        "maestro compatibility binary did not reach the native update command: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn fork_invalid_flag_reaches_run_fork_with_forwarded_arguments() {
    let output = ProcessCommand::new(maestro_binary())
        .args(["fork", "--definitely-invalid"])
        .output()
        .expect("run maestro fork invalid flag");

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "unknown fork flag: --definitely-invalid"
    );
}

#[test]
fn diagnostics_help_reaches_native_utility_without_starting_agent() {
    let output = ProcessCommand::new(maestro_binary())
        .args(["diagnostics", "--help"])
        .output()
        .expect("run diagnostics help");
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("--previous-sha256"));
}
