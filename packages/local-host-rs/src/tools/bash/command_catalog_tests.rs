use super::BashTool;

#[test]
fn command_catalog_rejects_writes_helpers_and_lookalike_commands_at_both_boundaries() {
    for command in [
        "git branch",
        "git tag",
        "git branch --list",
        "git tag --list",
        "git branch --list --sort=refname",
        "git tag --list --sort=signature",
        "git status",
        "git diff",
        "git show",
        "git ls-files",
        "git log",
        "git log --no-show-signature --format=\"%G?\"",
        "git log --no-show-signature --pretty=custom",
        "git branch --list --format=\"%(signature)\"",
        "git tag --list --format=\"%(signature)\"",
        "PATH=./bin; git branch",
        "for PATH in ./bin; do git -c core.fsmonitor=false status; done",
        "git() { printf marker; }; git -c core.fsmonitor=false status",
        "export PATH=./bin; git branch",
        "gh pr list &",
        "X=1; cat file",
        "git -c core.fsmonitor=false diff --no-ext-diff --no-textconv --ext-diff",
        "git branch -C copied",
        "date 20300101",
        "pwd-helper",
        "git status-helper",
        "git log-helper",
        "CAT file",
        "git diff --output=/tmp/report",
        "git log --output /tmp/report",
        "git diff --ext-diff",
        "git show --textconv",
        "git log --show-signature",
        "git branch --unset-upstream",
        "git branch --list --delete topic",
        "git remote update origin",
        "git remote show origin",
        "git -c core.pager=evil log",
        "git --exec-path=/tmp log",
        "sort -o output input",
        "sort --compress-program=evil input",
        "uniq input output",
        "sed -i.bak s/a/b/ file",
        r#"awk 'BEGIN { system("touch /tmp/marker") }'"#,
        "file -C",
        "date -s 2030-01-01",
        "hostname changed",
        "nohup cat file",
        "command touch file",
        "env X=y cat file",
        "rg -nF --pre sh pattern .",
        "rg -nFx pattern .",
        "jq --arg name",
        "jq --arg name --from-file evil",
        "fd -x rm",
        "cat file > output",
        "python3 -m pip install package",
    ] {
        for candidate in [command.to_string(), format!("{command} | head -n 5")] {
            assert!(BashTool::requires_approval(&candidate), "tool: {candidate}");
            assert!(
                !crate::safety::ActionFirewall::new(".")
                    .check_bash(&candidate)
                    .is_allowed(),
                "firewall: {candidate}"
            );
        }
    }
}

#[test]
fn command_catalog_understands_read_arguments_without_blanket_tool_approval() {
    for command in [
        "rg -nF TODO src",
        "rg -n -g '*.rs' -- '-TODO' src",
        "fd -HI --type f Cargo",
        "jq -cr '.name' package.json",
        r"jq --arg name 'hello world' --argjson count 2 '{name: $name, count: $count}'",
        "cat -- -filename",
        "head -n 5 file",
        "sort -rn input",
        "uniq -c input",
        "cut -d : -f 1 file",
        "ls -lah",
        "git -C repo -c core.fsmonitor=false status --short",
        "git --no-pager log --no-show-signature --oneline -n 5",
        "git -c core.fsmonitor=false diff --no-ext-diff --no-textconv --stat -- src",
        "git branch --list --no-sort 'feature/*'",
        "git remote -v",
        "git remote get-url origin",
        "git tag --list --no-sort 'v*'",
        "git -c core.fsmonitor=false ls-files --cached",
        "git ls-tree -r HEAD",
        "git stash list --no-show-signature --oneline",
        "git config --get user.name",
        "git describe --tags --always",
        "find . -maxdepth 2 -name '*.rs' -print",
        "date -u '+%Y-%m-%d'",
        "hostname",
        "pwd -P",
        "uname -a",
        "wc -lw file",
    ] {
        for candidate in [command.to_string(), format!("{command} | head -n 5")] {
            assert!(
                !BashTool::requires_approval(&candidate),
                "tool: {candidate}"
            );
            assert!(
                crate::safety::ActionFirewall::new(".")
                    .check_bash(&candidate)
                    .is_allowed(),
                "firewall: {candidate}"
            );
        }
    }
}

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
