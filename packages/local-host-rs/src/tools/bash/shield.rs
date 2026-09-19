//! Default-on publication guard for literal Git commit/push calls in BashTool.
//! Scans stay local. Findings contain locations and kinds, never source excerpts.
use std::collections::HashMap;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::io::AsyncReadExt;
use tree_sitter::ParseOptions;
use zeroize::Zeroizing;

use crate::agent::credential_store::{publication_secret_kind, redact_credentials_in_json};

const MAX_OUTPUT: u64 = 8 * 1024 * 1024;
const MAX_COMMITS: usize = 256;
const MAX_FINDINGS: usize = 20;
const HELP: &str = "Maestro Shield could not safely scan this Git operation. Run staging separately, then use a single literal git commit or git push command.";

struct Publication {
    cwd: PathBuf,
    operation: String,
    args: Vec<String>,
}

/// Detect literal publication commands with the same Bash grammar used by
/// command approval. A compound publication must be split so staging or a
/// directory change cannot invalidate the scan before the command executes.
fn publication(command: &str, cwd: &Path) -> Result<Option<Publication>> {
    if command.len() > 64 * 1024 {
        bail!("{HELP}");
    }
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .context(HELP)?;
    let started_at = Instant::now();
    let timeout = Duration::from_millis(50);
    let mut cancel_after_timeout = |_: &tree_sitter::ParseState| {
        if started_at.elapsed() >= timeout {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let bytes = command.as_bytes();
    let tree = parser
        .parse_with_options(
            &mut |offset, _| bytes.get(offset..).unwrap_or_default(),
            None,
            Some(ParseOptions::new().progress_callback(&mut cancel_after_timeout)),
        )
        .context(HELP)?;
    let root = tree.root_node();
    let mut stack = vec![root];
    let mut candidate = None;
    while let Some(node) = stack.pop() {
        // Visit nested commands even when this command is not Git.
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
        if node.kind() == "command" {
            let raw = node.utf8_text(command.as_bytes()).context(HELP)?;
            let words = shlex::split(raw).unwrap_or_default();
            let git_index = words.iter().position(|word| {
                Path::new(word)
                    .file_name()
                    .is_some_and(|name| name == "git")
            });
            if let Some(index) = git_index {
                // `echo "git"` is not a Git invocation. Recognized wrappers
                // are rejected for publication rather than guessed through.
                let executable = node
                    .child_by_field_name("name")
                    .and_then(|name| name.utf8_text(command.as_bytes()).ok())
                    .unwrap_or("");
                let executable = shlex::split(executable)
                    .and_then(|words| words.into_iter().next())
                    .unwrap_or_default();
                let executable = Path::new(&executable)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("");
                let wrapper = matches!(executable, "env" | "command" | "sudo" | "exec");
                if index != 0 && !wrapper && executable != "git" {
                    continue;
                }
                let mut offset = index + 1;
                let mut directory = cwd.to_path_buf();
                while words.get(offset).is_some_and(|word| word == "-C") {
                    directory = directory.join(words.get(offset + 1).context(HELP)?);
                    offset += 2;
                }
                let operation = words.get(offset).map(String::as_str).unwrap_or("");
                let publishes = matches!(operation, "commit" | "push");
                // Global overrides may change repository, index, hooks, or
                // aliases. If they precede a publication, require a plain call.
                if operation.starts_with('-')
                    && words[offset..]
                        .iter()
                        .any(|word| matches!(word.as_str(), "commit" | "push"))
                {
                    bail!("{HELP}");
                }
                if publishes {
                    if raw.trim() != command.trim()
                        || root.has_error()
                        || root.named_child_count() != 1
                        || root.named_child(0).is_none_or(|child| {
                            child.kind() != "command" || child.id() != node.id()
                        })
                        || index != 0
                        || candidate.is_some()
                        || raw.contains(['$', '`', '\n'])
                    {
                        bail!("{HELP}");
                    }
                    candidate = Some(Publication {
                        cwd: directory,
                        operation: operation.to_owned(),
                        args: words[offset + 1..].to_vec(),
                    });
                }
            }
        }
    }
    Ok(candidate)
}

/// Cancellation and scan limits must reap the inspection and its helpers.
struct InspectionChild(tokio::process::Child);
impl Drop for InspectionChild {
    fn drop(&mut self) {
        if let Some(pid) = self.0.id() {
            crate::tools::process_utils::kill_process_tree(pid);
            // Tokio's child reaper handles the terminated direct child.
            let _ = self.0.start_kill();
        }
    }
}

struct Scanner<'a> {
    cwd: &'a Path,
    execution_cwd: &'a Path,
    env: &'a HashMap<String, String>,
    findings: Vec<String>,
    policy: Option<&'a crate::sandbox::SandboxPolicy>,
}

impl Scanner<'_> {
    async fn git(&self, args: &[&str]) -> Result<Zeroizing<String>> {
        let mut command = vec![
            "git".to_owned(),
            "--no-pager".to_owned(),
            "-c".to_owned(),
            "core.fsmonitor=false".to_owned(),
            "-C".to_owned(),
            self.cwd
                .to_str()
                .context("Maestro Shield requires a UTF-8 repository path")?
                .to_owned(),
        ];
        command.extend(args.iter().map(|arg| (*arg).to_owned()));
        let mut env = self.env.clone();
        env.insert("GIT_TERMINAL_PROMPT".to_owned(), "0".to_owned());
        let child = if let Some(policy) = self.policy {
            crate::sandbox::spawn_sandboxed_command(
                command,
                self.execution_cwd.to_path_buf(),
                policy,
                env,
            )
            .await
        } else {
            crate::sandbox::spawn_unsandboxed_command(
                command,
                self.execution_cwd.to_path_buf(),
                env,
            )
            .await
        }
        .context("Maestro Shield could not start its inspection under the command's permissions")?;
        let mut child = InspectionChild(child);
        // Never expose subprocess diagnostics. Close input so a Git helper
        // cannot request a credential through the agent's terminal.
        drop(child.0.stdin.take());
        drop(child.0.stderr.take());
        let mut bytes = Zeroizing::new(Vec::new());
        child
            .0
            .stdout
            .take()
            .context("Maestro Shield could not read Git output")?
            .take(MAX_OUTPUT + 1)
            .read_to_end(&mut bytes)
            .await
            .context("Maestro Shield could not read Git output")?;
        if bytes.len() as u64 > MAX_OUTPUT {
            bail!(
                "Maestro Shield scan exceeds the 8 MiB limit; split the change before publishing"
            );
        }
        let status = child
            .0
            .wait()
            .await
            .context("Maestro Shield could not wait for Git")?;
        if !status.success() {
            // Git config --get-all uses exit 1 specifically for an absent key.
            if status.code() == Some(1)
                && args.len() == 3
                && args[0] == "config"
                && args[1] == "--get-all"
            {
                return Ok(Zeroizing::new(String::new()));
            }
            bail!(
                "Maestro Shield could not inspect Git state; check the repository and remote access, then retry"
            );
        }
        let text = String::from_utf8_lossy(&bytes);
        Ok(Zeroizing::new(text.into_owned()))
    }

    fn inspect_line(&mut self, location: &str, line: usize, value: &str) {
        if self.findings.len() >= MAX_FINDINGS {
            return;
        }
        if let Some(kind) = publication_secret_kind(value) {
            let path = if publication_secret_kind(location).is_some() {
                serde_json::Value::String("credential-bearing path".to_owned())
            } else {
                redact_credentials_in_json(&serde_json::Value::String(location.to_owned()))
            };
            self.findings.push(format!("{path}:{line} ({kind:?})"));
        }
    }

    fn inspect_text(&mut self, location: &str, text: &str) {
        for (index, line) in text.lines().enumerate() {
            self.inspect_line(location, index + 1, line);
        }
    }

    fn inspect_diff(&mut self, diff: &str) -> Result<()> {
        let mut path = "changed file";
        let mut line_number = 0usize;
        let mut in_hunk = false;
        for line in diff.lines() {
            if line.starts_with("diff --git ") {
                in_hunk = false;
            } else if !in_hunk && line.starts_with("+++ ") {
                let header = line
                    .strip_prefix("+++ ")
                    .context("Maestro Shield could not parse a diff path")?;
                path = header.strip_prefix("b/").unwrap_or(header);
            } else if line.starts_with("@@ ") {
                line_number = line
                    .split(" +")
                    .nth(1)
                    .and_then(|range| range.split([',', ' ']).next())
                    .and_then(|start| start.parse().ok())
                    .context("Maestro Shield could not parse a diff hunk")?;
                in_hunk = true;
            } else if !in_hunk && (line.starts_with("Binary files ") || line == "GIT binary patch")
            {
                bail!(
                    "Maestro Shield cannot scan a binary change; it has not been cleared for publication"
                );
            } else if in_hunk {
                if let Some(added) = line.strip_prefix('+') {
                    self.inspect_line(path, line_number, added);
                    line_number += 1;
                } else if line.starts_with(' ') {
                    line_number += 1;
                }
            }
        }
        Ok(())
    }

    async fn commit(&mut self, args: &[String]) -> Result<()> {
        let mut all = false;
        let mut amend = false;
        let mut has_message = false;
        let mut index = 0;
        while let Some(arg) = args.get(index) {
            match arg.as_str() {
                "-a" | "--all" => all = true,
                "--amend" => amend = true,
                "-m" | "--message" => {
                    index += 1;
                    self.inspect_text("commit message", args.get(index).context(HELP)?);
                    has_message = true;
                }
                "-F" | "--file" => bail!(
                    "Maestro Shield requires a literal -m message; message files are not supported yet"
                ),
                "--no-edit"
                | "--allow-empty"
                | "--allow-empty-message"
                | "-s"
                | "--signoff"
                | "-q"
                | "--quiet"
                | "-v"
                | "--verbose" => {}
                _ if arg.starts_with("--message=") => {
                    self.inspect_text("commit message", &arg[10..]);
                    has_message = true;
                }
                _ if arg.starts_with("-m") && arg.len() > 2 => {
                    self.inspect_text("commit message", &arg[2..]);
                    has_message = true;
                }
                _ => bail!(
                    "Maestro Shield cannot establish the commit scope for these options; use a staged commit with a literal message"
                ),
            }
            index += 1;
        }
        let mut base = None;
        if amend {
            let parents = self
                .git(&["rev-list", "--parents", "-n", "1", "HEAD"])
                .await?;
            base = match parents.split_whitespace().nth(1) {
                Some(parent) => Some(parent.to_owned()),
                None => Some(
                    self.git(&["hash-object", "-t", "tree", "--stdin"])
                        .await?
                        .trim()
                        .to_owned(),
                ),
            };
            if !has_message {
                let message = self.git(&["log", "-1", "--format=%B"]).await?;
                self.inspect_text("commit message", &message);
            }
        } else if !has_message {
            bail!(
                "Maestro Shield requires a literal commit message or message file so it can be scanned before Git starts"
            );
        }
        let mut diff_args = vec![
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--text",
            "--unified=0",
        ];
        if !all {
            diff_args.push("--cached");
        }
        if let Some(base) = base.as_deref() {
            diff_args.push(base);
        } else if all {
            diff_args.push("HEAD");
        }
        diff_args.push("--");
        let diff = self.git(&diff_args).await?;
        self.inspect_diff(&diff)
    }

    async fn push(&mut self, args: &[String]) -> Result<()> {
        let mut positional = Vec::new();
        for arg in args {
            if matches!(
                arg.as_str(),
                "-u" | "--set-upstream"
                    | "--atomic"
                    | "--dry-run"
                    | "-n"
                    | "--porcelain"
                    | "--force-with-lease"
            ) {
                continue;
            }
            if arg.starts_with('-') {
                bail!(
                    "Maestro Shield requires an explicit branch push; mirror, tag, deletion, and custom push options are not supported"
                );
            }
            positional.push(arg.clone());
        }
        if positional.len() < 2 {
            let branch = self.git(&["symbolic-ref", "--short", "HEAD"]).await?;
            if positional.is_empty() {
                let mut remote = self
                    .git(&[
                        "config",
                        "--default",
                        "",
                        "--get",
                        &format!("branch.{}.pushRemote", branch.trim()),
                    ])
                    .await?
                    .trim()
                    .to_owned();
                if remote.is_empty() {
                    remote = self
                        .git(&["config", "--default", "", "--get", "remote.pushDefault"])
                        .await?
                        .trim()
                        .to_owned();
                }
                if remote.is_empty() {
                    remote = self
                        .git(&[
                            "config",
                            "--default",
                            "origin",
                            "--get",
                            &format!("branch.{}.remote", branch.trim()),
                        ])
                        .await?
                        .trim()
                        .to_owned();
                }
                positional.push(remote);
            }
            let defaults = self
                .git(&[
                    "config",
                    "--get-all",
                    &format!("remote.{}.push", positional[0]),
                ])
                .await?;
            let mode = self
                .git(&["config", "--default", "simple", "--get", "push.default"])
                .await?;
            if !defaults.trim().is_empty()
                || !matches!(mode.trim(), "simple" | "current" | "upstream")
            {
                bail!(
                    "Maestro Shield needs an explicit branch with this push configuration: git push <remote> HEAD:<branch>"
                );
            }
            positional.push("HEAD".to_owned());
        }
        if positional.len() != 2 {
            bail!("Maestro Shield currently scans one pushed branch at a time");
        }
        let follow_tags = self
            .git(&[
                "config",
                "--default",
                "false",
                "--bool",
                "--get",
                "push.followTags",
            ])
            .await?;
        let recurse = self
            .git(&[
                "config",
                "--default",
                "no",
                "--get",
                "push.recurseSubmodules",
            ])
            .await?;
        if follow_tags.trim() != "false" || recurse.trim() != "no" {
            bail!("Maestro Shield cannot scan implicit tag or recursive submodule pushes");
        }
        self.inspect_text("push reference", &positional[1]);
        let remote = positional[0].as_str();
        let source = positional[1].split(':').next().context(HELP)?;
        if source.is_empty() || source.starts_with('+') || source.contains('*') {
            bail!("{HELP}");
        }
        let source_ref = self
            .git(&["rev-parse", "--symbolic-full-name", source])
            .await?;
        if source != "HEAD" && !source_ref.trim().starts_with("refs/heads/") {
            bail!("Maestro Shield currently supports branch pushes only");
        }
        let tip = self
            .git(&["rev-parse", "--verify", &format!("{source}^{{commit}}")])
            .await?;
        let url = self
            .git(&["remote", "get-url", "--push", "--all", remote])
            .await?;
        if url.lines().count() != 1 {
            bail!("Maestro Shield requires one push destination per remote");
        }
        let fetch_url = self.git(&["remote", "get-url", "--all", remote]).await?;
        if fetch_url.lines().count() != 1 || fetch_url.trim() != url.trim() {
            bail!(
                "Maestro Shield requires matching fetch and push destinations for remote inspection"
            );
        }
        // Use the remote name, never a possibly credential-bearing URL in argv.
        let advertised = self.git(&["ls-remote", "--", remote]).await?;
        let mut exclusions = Vec::new();
        for line in advertised.lines().take(4096) {
            let hash = line
                .split_whitespace()
                .next()
                .context("Maestro Shield received an invalid remote advertisement")?;
            if !matches!(hash.len(), 40 | 64) || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                bail!("Maestro Shield received an invalid remote object ID");
            }
            exclusions.push(hash);
        }
        let mut rev_args = vec![
            "rev-list",
            "--ignore-missing",
            "--max-count=257",
            tip.trim(),
            "--not",
        ];
        rev_args.extend(exclusions);
        let commits = self.git(&rev_args).await?;
        if commits.lines().count() > MAX_COMMITS {
            bail!(
                "Maestro Shield push exceeds the 256-commit scan limit; publish smaller reviewed batches"
            );
        }
        for commit in commits.lines() {
            let message = self.git(&["show", "-s", "--format=%B", commit]).await?;
            self.inspect_text("outgoing commit message", &message);
            let diff = self
                .git(&[
                    "show",
                    "--format=",
                    "--root",
                    "--diff-merges=first-parent",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-renames",
                    "--text",
                    "--unified=0",
                    commit,
                    "--",
                ])
                .await?;
            self.inspect_diff(&diff)?;
        }
        Ok(())
    }
}

pub(super) async fn check(
    command: &str,
    cwd: &Path,
    env: &HashMap<String, String>,
    policy: Option<&crate::sandbox::SandboxPolicy>,
) -> Result<()> {
    let execution_cwd = if cwd.is_absolute() {
        cwd.to_path_buf()
    } else {
        std::env::current_dir()
            .context("Maestro Shield could not resolve the working directory")?
            .join(cwd)
    };
    let Some(publication) = publication(command, &execution_cwd)? else {
        return Ok(());
    };
    if env.keys().any(|key| {
        matches!(
            key.as_str(),
            "GIT_DIR"
                | "GIT_COMMON_DIR"
                | "GIT_WORK_TREE"
                | "GIT_INDEX_FILE"
                | "GIT_OBJECT_DIRECTORY"
                | "GIT_ALTERNATE_OBJECT_DIRECTORIES"
                | "GIT_NAMESPACE"
                | "GIT_CONFIG"
                | "GIT_CONFIG_PARAMETERS"
                | "GIT_CONFIG_GLOBAL"
                | "GIT_CONFIG_SYSTEM"
                | "GIT_CONFIG_COUNT"
        ) || key.starts_with("GIT_CONFIG_KEY_")
            || key.starts_with("GIT_CONFIG_VALUE_")
    }) {
        bail!("Maestro Shield cannot scan with repository or index overrides in the environment");
    }
    let mut scanner = Scanner {
        cwd: &publication.cwd,
        execution_cwd: &execution_cwd,
        env,
        findings: Vec::new(),
        policy,
    };
    tokio::time::timeout(Duration::from_secs(30), async {
        if publication.operation == "commit" {
            scanner.commit(&publication.args).await
        } else {
            scanner.push(&publication.args).await
        }
    })
    .await
    .context("Maestro Shield timed out; Git was not started")??;
    if !scanner.findings.is_empty() {
        bail!(
            "Maestro Shield blocked publication: possible credentials at:\n{}\nRemove the credential from the pending commit history, use a credential reference, and retry. No secret values are shown.",
            scanner.findings.join("\n")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(root: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "fixture Git command failed");
        String::from_utf8(output.stdout).unwrap()
    }
    fn repo() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-b", "main"]);
        git(root.path(), &["config", "user.name", "Shield fixture"]);
        git(
            root.path(),
            &["config", "user.email", "shield@example.test"],
        );
        git(root.path(), &["config", "commit.gpgsign", "false"]);
        git(root.path(), &["config", "push.followTags", "false"]);
        git(root.path(), &["config", "push.recurseSubmodules", "no"]);
        root
    }
    fn env() -> HashMap<String, String> {
        std::env::vars()
            .filter(|(name, _)| !name.starts_with("GIT_"))
            .collect()
    }
    fn token() -> String {
        ["ghp_", "aB3dE6gH9jK2mN5pQ8sT1vW4yZ7cF0iL3oR6"].concat()
    }
    fn stage(root: &Path, value: &str) {
        std::fs::write(root.join("config.txt"), value).unwrap();
        git(root, &["add", "config.txt"]);
    }

    #[test]
    fn findings_hide_credentials_in_file_names() {
        let environment = env();
        let mut scanner = Scanner {
            cwd: Path::new("."),
            execution_cwd: Path::new("."),
            env: &environment,
            findings: Vec::new(),
            policy: None,
        };
        let secret = ["ebfacefa", "cedeafbe"].concat();
        scanner.inspect_line(&format!("api_key = {secret}"), 1, &token());
        assert_eq!(scanner.findings.len(), 1);
        assert!(scanner.findings[0].contains("credential-bearing path"));
        assert!(!scanner.findings[0].contains(&secret));
        assert!(!scanner.findings[0].contains(&token()));
    }

    #[test]
    fn compound_publication_and_overrides_fail_closed() {
        for command in [
            "git add . && git commit -m ship",
            "cd other; git push origin HEAD",
            "git -c core.hooksPath=/dev/null commit -m ship",
            "env X=1 git commit -m ship",
            "git commit -m \"$(cat message)\"",
            "git commit -m ship &",
            "echo $(git add .; git commit -m ship)",
            "GIT_INDEX_FILE=other git commit -m ship",
            "/usr/bin/env FOO=bar git push origin HEAD",
        ] {
            assert!(publication(command, Path::new(".")).is_err(), "{command}");
        }
        assert!(publication("git status", Path::new(".")).unwrap().is_none());
        assert!(publication("echo git", Path::new(".")).unwrap().is_none());
        let parsed = publication(
            "git -C 'a directory' commit -m 'ship it'",
            Path::new("root"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(parsed.cwd, Path::new("root/a directory"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn inspection_keeps_the_original_sandbox_working_directory() {
        use std::os::unix::fs::PermissionsExt;
        let workspace = tempfile::tempdir().unwrap();
        let repository = tempfile::tempdir().unwrap();
        let bin = workspace.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let fake = bin.join("git");
        std::fs::write(&fake, "#!/bin/sh\npwd\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut environment = env();
        environment.insert("PATH".into(), bin.display().to_string());
        let scanner = Scanner {
            cwd: repository.path(),
            execution_cwd: workspace.path(),
            env: &environment,
            findings: Vec::new(),
            policy: None,
        };
        let output = scanner.git(&["status"]).await.unwrap();
        assert_eq!(
            dunce::canonicalize(output.trim()).unwrap(),
            dunce::canonicalize(workspace.path()).unwrap()
        );
    }

    #[tokio::test]
    async fn staged_secret_blocks_without_reporting_the_value() {
        let root = repo();
        stage(root.path(), &format!("access = {}\n", token()));
        let error = check("git commit -m ship", root.path(), &env(), None)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("config.txt"));
        assert!(error.contains(":1"));
        assert!(!error.contains(&token()));
    }

    #[tokio::test]
    async fn removed_lines_do_not_block_a_repair_commit() {
        let root = repo();
        stage(root.path(), &token());
        git(root.path(), &["commit", "-m", "fixture"]);
        stage(root.path(), "access = $SERVICE_TOKEN\n");
        check("git commit -m repair", root.path(), &env(), None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn unstaged_secret_is_scanned_for_commit_all() {
        let root = repo();
        stage(root.path(), "safe\n");
        git(root.path(), &["commit", "-m", "fixture"]);
        std::fs::write(root.path().join("config.txt"), token()).unwrap();
        check("git commit -m staged", root.path(), &env(), None)
            .await
            .unwrap();
        assert!(
            check("git commit -a -m tracked", root.path(), &env(), None)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn amend_can_remove_a_secret_from_the_root_commit() {
        let root = repo();
        stage(root.path(), &token());
        git(root.path(), &["commit", "-m", "fixture"]);
        stage(root.path(), "safe\n");
        check("git commit --amend --no-edit", root.path(), &env(), None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn push_checks_intermediate_commits_even_when_tip_is_clean() {
        let root = repo();
        let remote = tempfile::tempdir().unwrap();
        git(remote.path(), &["init", "--bare"]);
        git(
            root.path(),
            &["remote", "add", "origin", remote.path().to_str().unwrap()],
        );
        stage(root.path(), "safe\n");
        git(root.path(), &["commit", "-m", "initial"]);
        git(root.path(), &["push", "origin", "HEAD"]);
        stage(root.path(), &token());
        git(root.path(), &["commit", "-m", "fixture"]);
        stage(root.path(), "safe again\n");
        git(root.path(), &["commit", "-m", "cleanup"]);
        let error = check("git push origin HEAD", root.path(), &env(), None)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("blocked publication"), "{error}");
        assert!(!error.contains(&token()));
    }

    #[tokio::test]
    async fn clean_push_and_message_scanning() {
        let root = repo();
        let remote = tempfile::tempdir().unwrap();
        git(remote.path(), &["init", "--bare"]);
        git(
            root.path(),
            &["remote", "add", "origin", remote.path().to_str().unwrap()],
        );
        stage(root.path(), "safe\n");
        git(root.path(), &["commit", "-m", "initial"]);
        check("git push origin HEAD", root.path(), &env(), None)
            .await
            .unwrap();
        check("git push origin", root.path(), &env(), None)
            .await
            .unwrap();
        check("git push", root.path(), &env(), None).await.unwrap();
        assert!(
            check(
                &format!("git commit --allow-empty -m '{}'", token()),
                root.path(),
                &env(),
                None
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn oversized_changes_are_not_cleared() {
        let root = repo();
        stage(root.path(), &"x".repeat(MAX_OUTPUT as usize + 1));
        let error = check("git commit -m large", root.path(), &env(), None)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("limit"), "{error}");
    }

    #[tokio::test]
    async fn different_push_destination_is_not_excluded_using_fetch_refs() {
        let root = repo();
        let fetch = tempfile::tempdir().unwrap();
        let push = tempfile::tempdir().unwrap();
        git(fetch.path(), &["init", "--bare"]);
        git(push.path(), &["init", "--bare"]);
        git(
            root.path(),
            &["remote", "add", "origin", fetch.path().to_str().unwrap()],
        );
        git(
            root.path(),
            &[
                "remote",
                "set-url",
                "--push",
                "origin",
                push.path().to_str().unwrap(),
            ],
        );
        stage(root.path(), "safe");
        git(root.path(), &["commit", "-m", "fixture"]);
        let error = check("git push origin HEAD", root.path(), &env(), None)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("matching fetch and push"), "{error}");
    }

    #[tokio::test]
    async fn binary_credentials_and_unavailable_remote_are_not_cleared() {
        let root = repo();
        stage(root.path(), "binary\0data");
        check("git commit -m binary", root.path(), &env(), None)
            .await
            .unwrap();
        stage(root.path(), &format!("binary\0{}", token()));
        assert!(
            check("git commit -m binary", root.path(), &env(), None)
                .await
                .is_err()
        );
        assert!(
            check("git push missing HEAD", root.path(), &env(), None)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn bash_tool_blocks_before_creating_the_commit() {
        let root = repo();
        stage(root.path(), &token());
        let tool = super::super::BashTool::new(root.path().display().to_string());
        let result = tool
            .execute(super::super::BashArgs {
                command: "git commit -m shield-regression".into(),
                timeout: None,
                description: None,
                run_in_background: false,
                bypass_sandbox: false,
            })
            .await;
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|message| message.contains("blocked publication"))
        );
        let output = std::process::Command::new("git")
            .args(["rev-parse", "--verify", "HEAD"])
            .current_dir(root.path())
            .output()
            .unwrap();
        assert!(!output.status.success());
    }
}
