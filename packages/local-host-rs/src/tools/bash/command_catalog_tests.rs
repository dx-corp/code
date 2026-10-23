use super::BashTool;

#[test]
fn common_command_catalog_uses_the_same_rules_at_the_tool_boundary() {
    for command in [
        "gh pr list",
        "gh pr view 123 --json title",
        "gh -R dx-corp/mono pr checks 123",
        "gh pr list | head -n 5",
        "rg -n --glob '*.rs' TODO src",
        "fd --type f Cargo",
        "jq -r '.name' package.json",
        "ps aux",
        "uptime",
        "sha256sum Cargo.lock",
        "python3 --version",
        "python3.13 -V",
        "gh pr list && python3 --version",
    ] {
        assert!(!BashTool::requires_approval(command), "{command}");
        assert!(
            crate::safety::ActionFirewall::new(".")
                .check_bash(command)
                .is_allowed(),
            "{command}"
        );
    }
    for command in [
        "pnpm --version",
        "yarn --version",
        "go version",
        r#"python3 "\--version""#,
        r#"node "\--version""#,
        "gh pr merge 123",
        "gh auth token",
        "gh pr view 123 --web",
        "rg --pre sh pattern .",
        "fd --exec rm {}",
        "gh pr view --repo",
        "gh pr list > out.txt",
        "gh pr list &",
        "gh pr view $(touch file)",
        "gh api repos/dx-corp/mono -X DELETE",
        "python3 script.py",
        "python3 '' --version",
        r#"python3 -c 'import os; os.remove("file")'"#,
        "python3 --version script.py",
        "env GH_PAGER=evil gh pr view 123",
        "nohup gh pr list",
        "GH pr list",
    ] {
        assert!(BashTool::requires_approval(command), "{command}");
    }
}

#[test]
fn test_requires_approval_version_checks() {
    // Version flags are not universally safe: package/build startup may
    // load repository hooks or select another toolchain. Keep the tool
    // boundary consistent with the firewall's reviewed catalog forms.
    for command in [
        "cargo --version",
        "rustc --version",
        "node --version",
        "npm --version",
        "bun --version",
        "pnpm --version",
        "yarn --version",
        "go version",
    ] {
        assert!(BashTool::requires_approval(command), "{command}");
    }
    assert!(!BashTool::requires_approval("python --version"));
    assert!(!BashTool::requires_approval("python3 --version"));
    assert!(!BashTool::requires_approval("gh --version"));
}
