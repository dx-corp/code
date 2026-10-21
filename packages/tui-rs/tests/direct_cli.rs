use std::process::Command;

fn maestro_tui_binary() -> std::ffi::OsString {
    std::env::var_os("CARGO_BIN_EXE_maestro-tui")
        .expect("Cargo must provide the maestro-tui integration-test binary")
}

#[test]
fn direct_binary_help_remains_a_successful_early_exit() {
    let output = Command::new(maestro_tui_binary())
        .arg("--help")
        .output()
        .expect("run maestro-tui --help");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Usage:"));
    assert!(stdout.contains("Usage: deixic-code"));
    assert!(!stdout.contains("maestro-tui"));
}

#[test]
fn direct_binary_supported_noninteractive_commands_exit_successfully() {
    let cases: &[(&[&str], &str)] = &[
        (&["a2a", "--help"], "a2a"),
        (&["exec", "--help"], "deixic-code exec"),
        (&["print", "-h"], "deixic-code print"),
        (&["hosted-runner", "--help"], "hosted-runner"),
        (&["init", "--help"], "init"),
        (&["remote", "--help"], "remote"),
        (&["skill", "--help"], "skill"),
        (&["specialists", "--help"], "specialists"),
        (&["update", "--help"], "update"),
    ];

    for &(args, marker) in cases {
        let output = Command::new(maestro_tui_binary())
            .args(args)
            .output()
            .unwrap_or_else(|error| panic!("run maestro-tui {}: {error}", args.join(" ")));
        assert!(output.status.success(), "{args:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(marker),
            "{args:?}: expected help output to contain {marker:?}"
        );
    }
}

#[test]
fn exec_help_precedes_execution_options_but_respects_literal_prompts() {
    let output = Command::new(maestro_tui_binary())
        .args(["exec", "--help", "--prompt-stdin"])
        .env("MAESTRO_SANDBOX_MODE", "invalid-help-probe")
        .env("OPENAI_API_KEY", "help-must-not-print-this-value")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    for marker in [
        "--only-tools",
        "--approval-mode fail",
        "Examples:",
        "Exit status:",
    ] {
        assert!(stdout.contains(marker), "missing {marker}: {stdout}");
    }
    assert!(!stdout.contains("help-must-not-print-this-value"));
    assert!(output.stderr.is_empty(), "{output:?}");

    let literal = Command::new(maestro_tui_binary())
        .args(["exec", "--sandbox", "invalid-help-probe", "--", "--help"])
        .output()
        .unwrap();
    assert_eq!(literal.status.code(), Some(2), "{literal:?}");
    assert!(String::from_utf8_lossy(&literal.stderr).contains("Unsupported exec sandbox"));
    assert!(!String::from_utf8_lossy(&literal.stdout).contains("Examples:"));
}

#[test]
fn specialists_are_discoverable_and_unknown_selection_fails_before_inference() {
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(maestro_tui_binary())
        .args(["specialists", "list", "--json"])
        .env("MAESTRO_HOME", home.path())
        .current_dir(home.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let profiles: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    for name in ["security", "product", "performance"] {
        assert!(
            profiles
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["name"] == name)
        );
    }
    let output = Command::new(maestro_tui_binary())
        .args(["exec", "--specialist", "does-not-exist", "review"])
        .env("MAESTRO_HOME", home.path())
        .current_dir(home.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("authorized scope"));
}

#[test]
fn tool_listing_reports_run_selection_without_widening_policy() {
    let home = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        Command::new(maestro_tui_binary())
            .args(args)
            .env("MAESTRO_HOME", home.path())
            .env("MAESTRO_PRINT_ALLOWED_TOOLS", "read,grep,write")
            .current_dir(home.path())
            .output()
            .unwrap()
    };
    for entry in ["exec", "--print"] {
        let output = run(&[
            entry,
            "--list-tools",
            "--json",
            "--only-tools",
            "READ,write",
            "--add-tools",
            "grep,bash",
            "--remove-tools",
            "write",
        ]);
        assert!(output.status.success(), "{entry}: {output:?}");
        let catalog: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let tools = catalog["tools"].as_array().unwrap();
        let selected: Vec<_> = tools
            .iter()
            .filter(|tool| tool["selected"] == true)
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(selected, ["grep", "read"], "{entry}: {catalog}");
        assert!(
            tools
                .iter()
                .any(|tool| tool["name"] == "bash" && tool["selected"] == false)
        );
        assert!(
            tools
                .iter()
                .all(|tool| tool["requiresApproval"].is_boolean())
        );
    }

    let output = run(&[
        "exec",
        "--list-tools",
        "--json",
        "--only-tools",
        "read",
        "--remove-tools",
        "read",
    ]);
    assert!(output.status.success(), "{output:?}");
    let catalog: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        catalog["tools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|tool| tool["selected"] == false)
    );

    let output = run(&["exec", "--list-tools", "--only-tools", "read"]);
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("approval and sandbox checks still apply"));
    assert!(
        text.lines()
            .any(|line| line.starts_with("write ") && line.ends_with("not selected"))
    );

    let output = run(&[
        "exec",
        "--list-tools",
        "--json",
        "--only-tools",
        "missing-tool-9f64",
    ]);
    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("missing-tool-9f64"));
}

#[test]
fn tool_listing_honors_specialist_limits_and_rejects_unknown_profiles() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir(home.path().join("agent-profiles")).unwrap();
    std::fs::write(
        home.path().join("agent-profiles/listing-probe.md"),
        "---\nname: listing-probe\ntools: [read,write]\n---\nInspect the repository.\n",
    )
    .unwrap();
    for (profile, success) in [("listing-probe", true), ("missing-profile-9f64", false)] {
        let output = Command::new(maestro_tui_binary())
            .args(["exec", "--list-tools", "--json", "--specialist", profile])
            .env("MAESTRO_HOME", home.path())
            .env("MAESTRO_PRINT_ALLOWED_TOOLS", "read,grep")
            .current_dir(home.path())
            .output()
            .unwrap();
        assert_eq!(output.status.success(), success, "{output:?}");
        if success {
            let catalog: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            let selected: Vec<_> = catalog["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|tool| tool["selected"] == true)
                .map(|tool| tool["name"].as_str().unwrap())
                .collect();
            assert_eq!(selected, ["read"]);
        } else {
            assert!(String::from_utf8_lossy(&output.stderr).contains("authorized scope"));
        }
    }
}

#[test]
fn experiments_cli_persists_consent_and_never_exports_the_installation_seed() {
    let home = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        Command::new(maestro_tui_binary())
            .args(args)
            .env("MAESTRO_HOME", home.path())
            .env("MAESTRO_TELEMETRY", "0")
            .current_dir(home.path())
            .output()
            .unwrap()
    };
    let status = run(&["experiments", "status", "--json"]);
    assert!(status.status.success(), "{status:?}");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&status.stdout).unwrap()["enabled"],
        false
    );
    assert!(!home.path().join("config.toml").exists());
    for (action, enabled) in [("on", true), ("off", false)] {
        let changed = run(&["experiments", action, "--json"]);
        assert!(changed.status.success(), "{changed:?}");
        let consent: toml::Value =
            toml::from_str(&std::fs::read_to_string(home.path().join("config.toml")).unwrap())
                .unwrap();
        assert_eq!(consent["experiments"]["enabled"].as_bool(), Some(enabled));
        let seed = consent["experiments"]["installation_seed"]
            .as_str()
            .unwrap();
        let stdout = String::from_utf8_lossy(&changed.stdout);
        assert!(!stdout.contains(seed));
        assert!(!stdout.contains("installation_seed"));
        let status = run(&["experiments", "status", "--json"]);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&status.stdout).unwrap()["enabled"],
            enabled
        );
    }
    std::fs::write(home.path().join("config.toml"), "invalid = [").unwrap();
    let failed = run(&["experiments", "on"]);
    assert!(!failed.status.success());
    assert!(!String::from_utf8_lossy(&failed.stdout).contains("Experiments: On"));
}
