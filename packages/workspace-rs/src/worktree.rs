//! Droid-style session worktrees (`maestro -w <name>` / `--worktree <name>`).
//!
//! When the flag is present, the CLI creates a git worktree for the current
//! repository at `../<repo-name>-wt-<name>` on a new branch derived from
//! `<name>`, then runs the whole session (TUI, `exec`, or print mode) with the
//! worktree as the working directory. On exit a clean worktree (no uncommitted
//! changes, no untracked files, no new commits) can be removed. Interactive
//! sessions keep their worktree. Successful sessions always preserve the branch.
//!
//! Like the rest of the crate's git integration (see [`crate::git`]), this
//! module shells out to the `git` CLI instead of linking libgit2.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;

use anyhow::{Context, Result, anyhow, bail};

const GIT_PROCESS_TIMEOUT: Duration = Duration::from_secs(60);
// Readers run on separate threads and can be delayed after Git exits on a busy host.
const PIPE_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Sanitize a user-supplied worktree name into a valid git branch name.
///
/// Keeps ASCII alphanumerics plus `-`, `_`, and `.`; every other character
/// (including whitespace and `/`) collapses to a single `-`. Leading/trailing
/// `-` and `.` are trimmed, `..` sequences are collapsed, and a trailing
/// `.lock` is stripped, so the result always passes `git check-ref-format`.
pub fn sanitize_branch_name(input: &str) -> Result<String, String> {
    let mut out = String::with_capacity(input.len());
    let mut pending_dash = false;
    for ch in input.trim().chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(ch);
        } else {
            pending_dash = true;
        }
    }
    while out.contains("..") {
        out = out.replace("..", ".");
    }
    let trimmed = out.trim_matches(['-', '.']).to_string();
    let trimmed = trimmed
        .strip_suffix(".lock")
        .map_or(trimmed.as_str(), |stem| stem.trim_matches(['-', '.']))
        .to_string();
    if trimmed.is_empty() {
        return Err(format!(
            "worktree name `{input}` does not produce a valid branch name"
        ));
    }
    Ok(trimmed)
}

/// Extract the requested worktree name from raw CLI arguments.
///
/// Supports `-w <name>`, `--worktree <name>`, `--worktree=<name>`, and the
/// attached short form `-w<name>`. Returns `None` when the flag is absent and
/// `Some(Err(..))` when it is present without a usable value.
#[must_use]
pub fn requested_name(raw_args: &[OsString]) -> Option<Result<String, String>> {
    for (index, raw) in raw_args.iter().enumerate() {
        let arg = raw.to_string_lossy();
        if arg == "-w" || arg == "--worktree" {
            return match raw_args.get(index + 1) {
                Some(value) if !value.to_string_lossy().starts_with('-') => {
                    Some(Ok(value.to_string_lossy().into_owned()))
                }
                _ => Some(Err(format!("{arg} requires a worktree name"))),
            };
        }
        if let Some(value) = arg.strip_prefix("--worktree=") {
            return Some(if value.is_empty() {
                Err("--worktree requires a worktree name".to_string())
            } else {
                Ok(value.to_string())
            });
        }
        if !arg.starts_with("--") && arg.len() > 2 {
            if let Some(value) = arg.strip_prefix("-w") {
                return Some(Ok(value.to_string()));
            }
        }
    }
    None
}

/// A worktree created for one maestro session.
#[derive(Debug)]
pub struct WorktreeSession {
    repo_root: PathBuf,
    path: PathBuf,
    branch: String,
    initial_head: String,
    force_abort: bool,
}

impl WorktreeSession {
    /// Create a worktree for the repository containing `cwd`.
    ///
    /// Fails cleanly when `cwd` is not inside a git repository, when the
    /// sanitized branch already exists, or when the target path is occupied.
    pub fn create_in(cwd: &Path, name: &str) -> Result<Self> {
        let branch = sanitize_branch_name(name).map_err(anyhow::Error::msg)?;

        let root_out = git_output(cwd, &["rev-parse", "--show-toplevel"])
            .context("failed to run git rev-parse")?;
        if !root_out.status.success() {
            bail!("-w/--worktree requires running inside a git repository");
        }
        let repo_root = PathBuf::from(String::from_utf8_lossy(&root_out.stdout).trim());
        let repo_name = repo_root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .ok_or_else(|| {
                anyhow!(
                    "cannot derive a repository name from {}",
                    repo_root.display()
                )
            })?;
        let worktree_path = repo_root
            .parent()
            .ok_or_else(|| {
                anyhow!(
                    "repository root {} has no parent directory",
                    repo_root.display()
                )
            })?
            .join(format!("{repo_name}-wt-{branch}"));

        let branch_ref = format!("refs/heads/{branch}");
        let branch_exists = git_output(
            &repo_root,
            &["rev-parse", "--verify", "--quiet", &branch_ref],
        )
        .map(|output| output.status.success())
        .unwrap_or(false);
        if branch_exists {
            bail!(
                "branch `{branch}` already exists; choose a different worktree name or delete the branch"
            );
        }
        if worktree_path.exists() {
            bail!(
                "worktree path {} already exists; remove it or choose a different name",
                worktree_path.display()
            );
        }

        let initial_head = git_output(&repo_root, &["rev-parse", "--verify", "HEAD"])
            .context("failed to read initial worktree commit")?;
        if !initial_head.status.success() {
            bail!("cannot create a session worktree without a committed HEAD");
        }
        let initial_head = String::from_utf8(initial_head.stdout)?.trim().to_owned();

        let hooks_dir = isolated_hooks_dir()?;
        let hooks_config = format!("core.hooksPath={}", hooks_dir.path().display());
        let mut add = Command::new("git");
        add.args(["-c", &hooks_config, "worktree", "add", "-b", &branch])
            .arg(&worktree_path)
            .arg(&initial_head)
            .current_dir(&repo_root);
        let add_out = output_with_deadline(&mut add).context("failed to run git worktree add")?;
        if !add_out.status.success() {
            let stderr = String::from_utf8_lossy(&add_out.stderr).trim().to_string();
            bail!("git worktree add failed: {stderr}");
        }

        Ok(Self {
            repo_root,
            path: worktree_path,
            branch,
            initial_head,
            force_abort: true,
        })
    }

    /// Create a worktree from an explicitly pinned commit.
    ///
    /// Workflow children must run against the revision recorded by the
    /// coordinator.  `create_in` intentionally uses the current repository
    /// `HEAD` for the interactive `-w` flow, while this constructor accepts a
    /// commit (or another commit-ish) and resolves it before creating the
    /// branch.  The resolved commit is retained as `initial_head`, so callers
    /// can bind child evidence to the exact base they requested.
    pub fn create_in_at(cwd: &Path, name: &str, revision: &str) -> Result<Self> {
        let branch = sanitize_branch_name(name).map_err(anyhow::Error::msg)?;

        let root_out = git_output(cwd, &["rev-parse", "--show-toplevel"])
            .context("failed to run git rev-parse")?;
        if !root_out.status.success() {
            bail!("worktree creation requires running inside a git repository");
        }
        let repo_root = PathBuf::from(String::from_utf8_lossy(&root_out.stdout).trim());
        let repo_name = repo_root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .ok_or_else(|| {
                anyhow!(
                    "cannot derive a repository name from {}",
                    repo_root.display()
                )
            })?;
        let worktree_path = repo_root
            .parent()
            .ok_or_else(|| {
                anyhow!(
                    "repository root {} has no parent directory",
                    repo_root.display()
                )
            })?
            .join(format!("{repo_name}-wt-{branch}"));

        let branch_ref = format!("refs/heads/{branch}");
        let branch_exists = git_output(
            &repo_root,
            &["rev-parse", "--verify", "--quiet", &branch_ref],
        )
        .map(|output| output.status.success())
        .unwrap_or(false);
        if branch_exists {
            bail!(
                "branch `{branch}` already exists; choose a different worktree name or delete the branch"
            );
        }
        if worktree_path.exists() {
            bail!(
                "worktree path {} already exists; remove it or choose a different name",
                worktree_path.display()
            );
        }

        let initial_head = git_output_os(
            &repo_root,
            &[
                OsString::from("rev-parse"),
                OsString::from("--verify"),
                OsString::from("--end-of-options"),
                OsString::from(format!("{revision}^{{commit}}")),
            ],
        )
        .context("failed to resolve pinned worktree revision")?;
        if !initial_head.status.success() {
            bail!(
                "cannot resolve pinned worktree revision `{revision}`: {}",
                String::from_utf8_lossy(&initial_head.stderr).trim()
            );
        }
        let initial_head = String::from_utf8(initial_head.stdout)?.trim().to_owned();
        if initial_head.is_empty() {
            bail!("git returned an empty pinned worktree revision");
        }

        let hooks_dir = isolated_hooks_dir()?;
        let hooks_config = format!("core.hooksPath={}", hooks_dir.path().display());
        let mut add = Command::new("git");
        add.args(["-c", &hooks_config, "worktree", "add", "-b", &branch])
            .arg(&worktree_path)
            .arg(&initial_head)
            .current_dir(&repo_root);
        let add_out = output_with_deadline(&mut add).context("failed to run git worktree add")?;
        if !add_out.status.success() {
            let stderr = String::from_utf8_lossy(&add_out.stderr).trim().to_string();
            bail!("git worktree add failed: {stderr}");
        }

        Ok(Self {
            repo_root,
            path: worktree_path,
            branch,
            initial_head,
            force_abort: true,
        })
    }

    /// Reopen an existing workflow child worktree after a process restart.
    ///
    /// This only accepts the deterministic branch/path owned by the caller.
    /// It never resets, cleans, or removes anything. If the owned branch still
    /// exists but its worktree path is missing, it reattaches that path to the
    /// existing branch. Callers must validate the ownership marker and
    /// cleanliness before using the session.
    pub fn reopen_in_at(cwd: &Path, name: &str, revision: &str) -> Result<Self> {
        let branch = sanitize_branch_name(name).map_err(anyhow::Error::msg)?;
        let root_out = git_output(cwd, &["rev-parse", "--show-toplevel"])
            .context("failed to run git rev-parse")?;
        if !root_out.status.success() {
            bail!("worktree reopening requires running inside a git repository");
        }
        let repo_root = PathBuf::from(String::from_utf8_lossy(&root_out.stdout).trim());
        let repo_name = repo_root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .ok_or_else(|| {
                anyhow!(
                    "cannot derive a repository name from {}",
                    repo_root.display()
                )
            })?;
        let worktree_path = repo_root
            .parent()
            .ok_or_else(|| {
                anyhow!(
                    "repository root {} has no parent directory",
                    repo_root.display()
                )
            })?
            .join(format!("{repo_name}-wt-{branch}"));
        let branch_ref = format!("refs/heads/{branch}");
        let branch_exists = git_output(
            &repo_root,
            &["rev-parse", "--verify", "--quiet", &branch_ref],
        )
        .context("failed to inspect child worktree branch")?;
        if !branch_exists.status.success() {
            if worktree_path.exists() {
                bail!("owned child worktree branch {branch} does not exist");
            }
            bail!(
                "owned child worktree path {} and branch {branch} do not exist",
                worktree_path.display()
            );
        }

        let expected_head = git_output_os(
            &repo_root,
            &[
                OsString::from("rev-parse"),
                OsString::from("--verify"),
                OsString::from("--end-of-options"),
                OsString::from(format!("{revision}^{{commit}}")),
            ],
        )
        .context("failed to resolve pinned worktree revision")?;
        if !expected_head.status.success() {
            bail!(
                "cannot resolve pinned worktree revision {revision}: {}",
                String::from_utf8_lossy(&expected_head.stderr).trim()
            );
        }
        let initial_head = String::from_utf8(expected_head.stdout)?.trim().to_owned();
        if !worktree_path.exists() {
            let hooks_dir = isolated_hooks_dir()?;
            let hooks_config = format!("core.hooksPath={}", hooks_dir.path().display());
            let mut add = Command::new("git");
            add.args(["-c", &hooks_config, "worktree", "add"])
                .arg(&worktree_path)
                .arg(&branch)
                .current_dir(&repo_root);
            let add_out = output_with_deadline(&mut add)
                .context("failed to run git worktree add for child recovery")?;
            if !add_out.status.success() {
                bail!(
                    "git worktree add failed while reopening child: {}",
                    String::from_utf8_lossy(&add_out.stderr).trim()
                );
            }
        }
        let current_root = git_output(&worktree_path, &["rev-parse", "--show-toplevel"])
            .context("failed to inspect existing child worktree")?;
        if !current_root.status.success() {
            bail!("existing child path is not a Git worktree");
        }
        let current_root =
            dunce::canonicalize(String::from_utf8_lossy(&current_root.stdout).trim())
                .context("canonicalize existing child worktree")?;
        let expected_root =
            dunce::canonicalize(&worktree_path).context("canonicalize child worktree")?;
        if current_root != expected_root {
            bail!(
                "existing child path {} resolves to a different directory",
                worktree_path.display()
            );
        }
        let current_common = git_common_dir(&worktree_path)?;
        let expected_common = git_common_dir(&repo_root)?;
        if current_common != expected_common {
            bail!(
                "existing child path {} belongs to a different repository",
                worktree_path.display()
            );
        }
        let current_branch = git_output(&worktree_path, &["branch", "--show-current"])
            .context("failed to inspect existing child branch")?;
        if !current_branch.status.success()
            || String::from_utf8_lossy(&current_branch.stdout).trim() != branch
        {
            bail!(
                "existing child path {} is not checked out on branch {branch}",
                worktree_path.display()
            );
        }

        Ok(Self {
            repo_root,
            path: worktree_path,
            branch,
            initial_head,
            force_abort: false,
        })
    }

    /// Copy the source repository's tracked and non-ignored untracked changes
    /// into this clean worktree.
    ///
    /// `git worktree add ... HEAD` intentionally starts from the committed
    /// tree. A delegated child must see the parent session's current working
    /// state as well, otherwise it can inspect stale files or overwrite a
    /// parent change with an older version. Tracked changes are transferred
    /// with a binary-capable `git diff`/`git apply` pair; untracked files are
    /// copied individually from Git's authoritative path list. Ignored files
    /// are deliberately excluded so local secrets and caches do not cross the
    /// isolation boundary.
    pub fn copy_changes_from(&self, source: &Path) -> Result<()> {
        let source_root = repository_root(source)?;
        let staged_diff = git_output(
            &source_root,
            &["diff", "--no-ext-diff", "--binary", "--cached", "--"],
        )
        .context("failed to inspect source staged changes")?;
        if !staged_diff.status.success() {
            bail!(
                "git diff --cached failed: {}",
                String::from_utf8_lossy(&staged_diff.stderr).trim()
            );
        }
        apply_diff(&self.path, &staged_diff.stdout, true)
            .context("apply source staged changes to child worktree")?;

        let unstaged_diff = git_output(&source_root, &["diff", "--no-ext-diff", "--binary", "--"])
            .context("failed to inspect source unstaged changes")?;
        if !unstaged_diff.status.success() {
            bail!(
                "git diff failed: {}",
                String::from_utf8_lossy(&unstaged_diff.stderr).trim()
            );
        }
        apply_diff(&self.path, &unstaged_diff.stdout, false)
            .context("apply source unstaged changes to child worktree")?;

        let untracked = git_output(
            &source_root,
            &[
                "ls-files",
                "--others",
                "--exclude-standard",
                "--full-name",
                "-z",
            ],
        )
        .context("failed to inspect source untracked files")?;
        if !untracked.status.success() {
            bail!(
                "git ls-files failed: {}",
                String::from_utf8_lossy(&untracked.stderr).trim()
            );
        }
        for relative in untracked
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
        {
            copy_untracked_path(&source_root, &self.path, &path_from_git_bytes(relative))?;
        }
        Ok(())
    }

    /// Map a directory from the source repository into this worktree.
    pub fn path_for(&self, source: &Path) -> Result<PathBuf> {
        let source_root = repository_root(source)?;
        let source_path = dunce::canonicalize(source)
            .with_context(|| format!("canonicalize source path {}", source.display()))?;
        let source_root = dunce::canonicalize(&source_root)
            .with_context(|| format!("canonicalize source repository {}", source_root.display()))?;
        let relative = source_path.strip_prefix(&source_root).with_context(|| {
            format!(
                "source path {} is outside repository {}",
                source_path.display(),
                source_root.display()
            )
        })?;
        Ok(self.path.join(relative))
    }

    /// Discard a worktree and its branch after setup fails.
    ///
    /// This is intentionally forceful because the worktree is newly created
    /// by the caller and may contain a partially applied parent diff.
    pub fn abort(self) {
        if !self.force_abort && !self.is_clean() {
            eprintln!(
                "Child worktree has local changes; keeping {} and branch {}",
                self.path.display(),
                self.branch
            );
            return;
        }
        let mut remove = Command::new("git");
        remove
            .args(["worktree", "remove", "--force"])
            .arg(&self.path)
            .current_dir(&self.repo_root);
        let removed = output_with_deadline(&mut remove)
            .map(|output| output.status.success())
            .unwrap_or(false);
        if !removed {
            eprintln!(
                "Worktree cleanup failed; keeping {} and branch {}",
                self.path.display(),
                self.branch
            );
            return;
        }

        let deleted = git_output(&self.repo_root, &["branch", "-D", &self.branch])
            .map(|output| output.status.success())
            .unwrap_or(false);
        if !deleted {
            eprintln!(
                "Removed failed worktree {} but could not delete branch {}",
                self.path.display(),
                self.branch
            );
        }
    }

    /// Filesystem path of the worktree the session should run in.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The commit from which this worktree was created.
    #[must_use]
    pub fn initial_head(&self) -> &str {
        &self.initial_head
    }

    /// The branch owned by this worktree session.
    #[must_use]
    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// True when the worktree has no uncommitted changes and no untracked files.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        git_output(&self.path, &["status", "--porcelain"])
            .map(|output| output.status.success() && output.stdout.is_empty())
            .unwrap_or(false)
    }

    /// Keep an interactive session's working directory available for resume.
    pub fn keep(self) {
        eprintln!(
            "Worktree kept: {}\n  branch: {}\n  Resume from this directory with deixic-code --continue",
            self.path.display(),
            self.branch
        );
    }

    /// Remove only an unchanged, clean non-interactive worktree; keep its branch.
    pub fn finish(self) {
        let unchanged =
            git_output(&self.path, &["rev-parse", "--verify", "HEAD"]).is_ok_and(|output| {
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).trim() == self.initial_head
            });
        if !self.is_clean() || !unchanged {
            self.keep();
            return;
        }
        let mut remove = Command::new("git");
        remove
            .args(["worktree", "remove"])
            .arg(&self.path)
            .current_dir(&self.repo_root);
        let removed = output_with_deadline(&mut remove).is_ok_and(|output| output.status.success());
        if removed {
            eprintln!(
                "Removed unchanged worktree {}\n  branch kept: {}",
                self.path.display(),
                self.branch
            );
        } else {
            self.keep();
        }
    }
}

fn isolated_hooks_dir() -> Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix("maestro-worktree-hooks-")
        .tempdir()
        .context("create isolated hooks path")
}

fn output_with_deadline(command: &mut Command) -> Result<Output> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = spawn_owned_command(command).context("failed to start git command")?;
    let stdout = child.stdout.take().map(drain_output_pipe);
    let stderr = child.stderr.take().map(drain_output_pipe);
    let deadline = Instant::now() + GIT_PROCESS_TIMEOUT;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                timed_out = true;
                kill_owned_process(&mut child);
                break child
                    .wait()
                    .context("failed to reap timed-out git command")?;
            }
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                kill_owned_process(&mut child);
                let _ = child.wait();
                let _ = join_output_pipe(stdout);
                let _ = join_output_pipe(stderr);
                return Err(anyhow!("failed to wait for git command: {error}"));
            }
        }
    };
    let stdout = match join_output_pipe(stdout) {
        Ok(stdout) => stdout,
        Err(error) => {
            kill_owned_process(&mut child);
            let _ = child.wait();
            return Err(error);
        }
    };
    let stderr = match join_output_pipe(stderr) {
        Ok(stderr) => stderr,
        Err(error) => {
            kill_owned_process(&mut child);
            let _ = child.wait();
            return Err(error);
        }
    };
    if timed_out {
        bail!(
            "git command exceeded {}s deadline: {}",
            GIT_PROCESS_TIMEOUT.as_secs(),
            String::from_utf8_lossy(&stderr).trim()
        );
    }
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn output_with_stdin(command: &mut Command, input: &[u8]) -> Result<Output> {
    command.stdin(Stdio::piped());
    let mut child = spawn_owned_command(command).context("failed to start git command")?;
    let stdout = child.stdout.take().map(drain_output_pipe);
    let stderr = child.stderr.take().map(drain_output_pipe);
    let (sender, receiver) = mpsc::channel();
    match child.stdin.take() {
        Some(mut stdin) => {
            let input = input.to_vec();
            thread::spawn(move || {
                let result = stdin.write_all(&input);
                drop(stdin);
                let _ = sender.send(result);
            });
        }
        None => {
            let _ = sender.send(Err(io::Error::other("git command did not expose stdin")));
        }
    }
    let deadline = Instant::now() + GIT_PROCESS_TIMEOUT;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                timed_out = true;
                kill_owned_process(&mut child);
                break child
                    .wait()
                    .context("failed to reap timed-out git command")?;
            }
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                kill_owned_process(&mut child);
                let _ = child.wait();
                let _ = join_output_pipe(stdout);
                let _ = join_output_pipe(stderr);
                let _ = receiver.recv_timeout(PIPE_DRAIN_TIMEOUT);
                return Err(anyhow!("failed to wait for git command: {error}"));
            }
        }
    };
    let stdout = match join_output_pipe(stdout) {
        Ok(stdout) => stdout,
        Err(error) => {
            kill_owned_process(&mut child);
            let _ = child.wait();
            return Err(error);
        }
    };
    let stderr = match join_output_pipe(stderr) {
        Ok(stderr) => stderr,
        Err(error) => {
            kill_owned_process(&mut child);
            let _ = child.wait();
            return Err(error);
        }
    };
    if timed_out {
        let _ = receiver.recv_timeout(PIPE_DRAIN_TIMEOUT);
        bail!(
            "git command exceeded {}s deadline: {}",
            GIT_PROCESS_TIMEOUT.as_secs(),
            String::from_utf8_lossy(&stderr).trim()
        );
    }
    match receiver.recv_timeout(PIPE_DRAIN_TIMEOUT) {
        Ok(result) => match result {
            Ok(()) => {}
            Err(error) => {
                kill_owned_process(&mut child);
                let _ = child.wait();
                return Err(anyhow!("failed to write git command input: {error}"));
            }
        },
        Err(error) => {
            kill_owned_process(&mut child);
            let _ = child.wait();
            bail!("git command input did not finish: {error}");
        }
    }
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn git_output(cwd: &Path, args: &[&str]) -> Result<Output> {
    output_with_deadline(Command::new("git").args(args).current_dir(cwd))
}

fn git_output_os(cwd: &Path, args: &[OsString]) -> Result<Output> {
    output_with_deadline(Command::new("git").args(args).current_dir(cwd))
}

fn drain_output_pipe<R>(mut pipe: R) -> mpsc::Receiver<(Vec<u8>, io::Result<usize>)>
where
    R: Read + Send + 'static,
{
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = pipe.read_to_end(&mut bytes);
        let _ = sender.send((bytes, result));
    });
    receiver
}

fn spawn_owned_command(command: &mut Command) -> io::Result<Child> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        // Keep Git and hooks/drivers in a private process group so a timed
        // out operation cannot leave a helper holding our output pipes.
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
    command.spawn()
}

fn kill_owned_process(child: &mut Child) {
    #[cfg(unix)]
    {
        let process_group = -(child.id() as libc::pid_t);
        unsafe {
            let _ = libc::kill(process_group, libc::SIGKILL);
        }
    }
    let _ = child.kill();
}

fn git_common_dir(cwd: &Path) -> Result<PathBuf> {
    let output = git_output(cwd, &["rev-parse", "--git-common-dir"])
        .context("failed to locate git common directory")?;
    if !output.status.success() {
        bail!(
            "git common directory lookup failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let raw = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    let absolute = if raw.is_absolute() {
        raw
    } else {
        cwd.join(raw)
    };
    dunce::canonicalize(&absolute)
        .with_context(|| format!("canonicalize git common directory {}", absolute.display()))
}

fn join_output_pipe(pipe: Option<mpsc::Receiver<(Vec<u8>, io::Result<usize>)>>) -> Result<Vec<u8>> {
    let Some(pipe) = pipe else {
        return Ok(Vec::new());
    };
    let (bytes, result) = pipe
        .recv_timeout(PIPE_DRAIN_TIMEOUT)
        .context("git output drain did not finish within deadline")?;
    result.context("failed to drain git output")?;
    Ok(bytes)
}

fn repository_root(cwd: &Path) -> Result<PathBuf> {
    let output = git_output(cwd, &["rev-parse", "--show-toplevel"])
        .context("failed to locate source git repository")?;
    if !output.status.success() {
        bail!(
            "source path is not inside a git repository: {}",
            cwd.display()
        );
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if root.is_empty() {
        bail!("git returned an empty source repository root");
    }
    Ok(PathBuf::from(root))
}

fn apply_diff(worktree_path: &Path, diff: &[u8], update_index: bool) -> Result<()> {
    if diff.is_empty() {
        return Ok(());
    }

    let mut command = Command::new("git");
    command.args(["apply", "--binary", "--whitespace=nowarn"]);
    if update_index {
        command.arg("--index");
    }
    command
        .arg("-")
        .current_dir(worktree_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let output =
        output_with_stdin(&mut command, diff).context("failed to apply source worktree changes")?;
    if !output.status.success() {
        bail!(
            "git apply failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn path_from_git_bytes(path: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        PathBuf::from(OsString::from_vec(path.to_vec()))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(path).into_owned())
    }
}

fn copy_untracked_path(source_root: &Path, target_root: &Path, relative: &Path) -> Result<()> {
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        bail!(
            "git returned an unsafe untracked path: {}",
            relative.display()
        );
    }

    let source = source_root.join(relative);
    let destination = target_root.join(relative);
    let metadata = fs::symlink_metadata(&source)
        .with_context(|| format!("inspect untracked file {}", source.display()))?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create destination directory {}", parent.display()))?;
    }

    if metadata.file_type().is_symlink() {
        remove_existing_path(&destination)?;
        let link_target = fs::read_link(&source)
            .with_context(|| format!("read untracked symlink {}", source.display()))?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&link_target, &destination)
            .with_context(|| format!("copy untracked symlink {}", relative.display()))?;
        #[cfg(windows)]
        {
            let target_metadata = fs::metadata(&source).ok();
            if target_metadata.is_some_and(|metadata| metadata.is_dir()) {
                std::os::windows::fs::symlink_dir(&link_target, &destination)
            } else {
                std::os::windows::fs::symlink_file(&link_target, &destination)
            }
            .with_context(|| format!("copy untracked symlink {}", relative.display()))?;
        }
    } else if metadata.is_file() {
        fs::copy(&source, &destination)
            .with_context(|| format!("copy untracked file {}", relative.display()))?;
        fs::set_permissions(&destination, metadata.permissions()).with_context(|| {
            format!(
                "preserve permissions for untracked file {}",
                relative.display()
            )
        })?;
    } else {
        bail!("unsupported untracked path: {}", relative.display());
    }
    Ok(())
}

fn remove_existing_path(path: &Path) -> Result<()> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if metadata.file_type().is_dir() {
        fs::remove_dir_all(path).with_context(|| format!("remove {}", path.display()))?;
    } else {
        fs::remove_file(path).with_context(|| format!("remove {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::fs;
    use std::process::Command;
    use std::sync::{Arc, Barrier};

    #[test]
    fn isolated_hooks_dirs_are_unique_and_cleaned_up_under_parallel_creation() {
        const WORKERS: usize = 16;
        const DIRS_PER_WORKER: usize = 8;
        let start = Arc::new(Barrier::new(WORKERS));
        let workers: Vec<_> = (0..WORKERS)
            .map(|_| {
                let start = Arc::clone(&start);
                std::thread::spawn(move || {
                    start.wait();
                    (0..DIRS_PER_WORKER)
                        .map(|_| isolated_hooks_dir().expect("reserve an isolated hooks directory"))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let dirs: Vec<_> = workers
            .into_iter()
            .flat_map(|worker| worker.join().expect("hooks directory worker"))
            .collect();
        let paths: HashSet<_> = dirs.iter().map(|dir| dir.path().to_path_buf()).collect();
        assert_eq!(paths.len(), WORKERS * DIRS_PER_WORKER);
        assert!(paths.iter().all(|path| path.is_dir()));
        drop(dirs);
        assert!(paths.iter().all(|path| !path.exists()));
    }

    #[test]
    fn sanitize_keeps_valid_names() {
        assert_eq!(sanitize_branch_name("feat-x").unwrap(), "feat-x");
        assert_eq!(
            sanitize_branch_name("release_1.2.3").unwrap(),
            "release_1.2.3"
        );
        assert_eq!(sanitize_branch_name("FixBug").unwrap(), "FixBug");
    }

    #[test]
    fn sanitize_replaces_invalid_characters() {
        assert_eq!(sanitize_branch_name("feat x").unwrap(), "feat-x");
        assert_eq!(sanitize_branch_name("feat/x").unwrap(), "feat-x");
        assert_eq!(sanitize_branch_name("a  b/c:d").unwrap(), "a-b-c-d");
        assert_eq!(sanitize_branch_name(" ticket #42 ").unwrap(), "ticket-42");
    }

    #[test]
    fn sanitize_trims_git_forbidden_edges() {
        assert_eq!(sanitize_branch_name("-feat-").unwrap(), "feat");
        assert_eq!(sanitize_branch_name("..feat..").unwrap(), "feat");
        assert_eq!(sanitize_branch_name("a..b").unwrap(), "a.b");
        assert_eq!(sanitize_branch_name("x.lock").unwrap(), "x");
    }

    #[test]
    fn sanitize_rejects_unusable_names() {
        assert!(sanitize_branch_name("").is_err());
        assert!(sanitize_branch_name("   ").is_err());
        assert!(sanitize_branch_name("///").is_err());
        assert!(sanitize_branch_name("...").is_err());
        assert_eq!(sanitize_branch_name(".lock").unwrap(), "lock");
    }

    #[test]
    fn requested_name_supports_split_and_inline_forms() {
        let args = |argv: &[&str]| argv.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            requested_name(&args(&["-w", "feat-x"])).unwrap().unwrap(),
            "feat-x"
        );
        assert_eq!(
            requested_name(&args(&["--worktree", "feat-x"]))
                .unwrap()
                .unwrap(),
            "feat-x"
        );
        assert_eq!(
            requested_name(&args(&["--worktree=feat-x"]))
                .unwrap()
                .unwrap(),
            "feat-x"
        );
        assert_eq!(
            requested_name(&args(&["-wfeat-x"])).unwrap().unwrap(),
            "feat-x"
        );
        assert_eq!(
            requested_name(&args(&["exec", "-w", "feat-x", "do", "it"]))
                .unwrap()
                .unwrap(),
            "feat-x"
        );
        assert!(requested_name(&args(&["exec", "do", "it"])).is_none());
        assert!(requested_name(&args(&["-w"])).unwrap().is_err());
        assert!(
            requested_name(&args(&["--worktree", "--json"]))
                .unwrap()
                .is_err()
        );
        assert!(requested_name(&args(&["--worktree="])).unwrap().is_err());
    }

    fn temp_repo(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "maestro-worktree-test-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock should follow the Unix epoch")
                .as_nanos()
        ));
        let repo = root.join("repo");
        fs::create_dir_all(&repo).expect("repo directory should be created");
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .expect("git should run");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "--quiet"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        fs::write(repo.join("README.md"), "hello\n").expect("file should be written");
        git(&["add", "README.md"]);
        git(&["commit", "--quiet", "-m", "init"]);
        repo
    }

    /// `git branch --list <branch>` output, stripped of the `+`/`*` markers
    /// git adds for branches checked out in linked worktrees.
    fn listed_branch(repo: &Path, branch: &str) -> String {
        let output = Command::new("git")
            .args(["branch", "--list", branch])
            .current_dir(repo)
            .output()
            .expect("git branch should run");
        String::from_utf8_lossy(&output.stdout)
            .trim_start_matches(['+', '*', ' '])
            .trim_end()
            .to_string()
    }

    #[test]
    fn create_builds_sibling_worktree_on_new_branch() {
        let repo = temp_repo("create");
        let session =
            WorktreeSession::create_in(&repo, "My Feature!").expect("worktree should be created");
        let expected = repo
            .parent()
            .expect("repo has a parent")
            .join("repo-wt-My-Feature");
        assert_eq!(
            session.path(),
            dunce::canonicalize(expected.parent().expect("worktree has a parent"))
                .expect("canonical worktree parent")
                .join(expected.file_name().expect("worktree has a name"))
        );
        assert!(expected.join("README.md").is_file());
        assert!(session.is_clean());
        assert_eq!(listed_branch(&repo, "My-Feature"), "My-Feature");

        session.finish();
        assert!(!expected.exists(), "clean worktree should be removed");
        assert_eq!(listed_branch(&repo, "My-Feature"), "My-Feature");
        fs::remove_dir_all(repo.parent().expect("repo has a parent"))
            .expect("test directory should be removed");
    }

    #[test]
    fn interactive_clean_worktree_is_kept_for_resume() {
        let repo = temp_repo("interactive-keep");
        let session = WorktreeSession::create_in(&repo, "interactive").unwrap();
        let path = session.path().to_path_buf();
        session.keep();
        assert!(path.join("README.md").is_file());
        assert_eq!(listed_branch(&repo, "interactive"), "interactive");
        fs::remove_dir_all(repo.parent().unwrap()).unwrap();
    }

    #[test]
    fn committed_worktree_is_kept_even_when_commit_is_merged() {
        let repo = temp_repo("committed-keep");
        let session = WorktreeSession::create_in(&repo, "committed").unwrap();
        let path = session.path().to_path_buf();
        let output = Command::new("git")
            .args(["commit", "--allow-empty", "-m", "session work"])
            .current_dir(&path)
            .output()
            .unwrap();
        assert!(output.status.success());
        let output = Command::new("git")
            .args(["merge", "--ff-only", "committed"])
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(session.is_clean());
        session.finish();
        assert!(
            path.exists(),
            "committed work must retain its resume directory"
        );
        assert_eq!(listed_branch(&repo, "committed"), "committed");
        fs::remove_dir_all(repo.parent().unwrap()).unwrap();
    }

    #[test]
    fn dirty_worktree_is_kept_on_finish() {
        let repo = temp_repo("dirty");
        let session =
            WorktreeSession::create_in(&repo, "keepme").expect("worktree should be created");
        let path = session.path().to_path_buf();
        fs::write(path.join("scratch.txt"), "uncommitted\n").expect("file should be written");
        assert!(!session.is_clean());

        session.finish();
        assert!(path.exists(), "dirty worktree should be kept");
        assert_eq!(listed_branch(&repo, "keepme"), "keepme");

        Command::new("git")
            .args(["worktree", "remove", "--force"])
            .arg(&path)
            .current_dir(&repo)
            .output()
            .expect("git worktree remove should run");
        Command::new("git")
            .args(["branch", "-D", "keepme"])
            .current_dir(&repo)
            .output()
            .expect("git branch delete should run");
        fs::remove_dir_all(repo.parent().expect("repo has a parent"))
            .expect("test directory should be removed");
    }

    #[test]
    fn copy_changes_transfers_tracked_and_untracked_state() {
        let repo = temp_repo("copy-changes");
        fs::write(repo.join("README.md"), "parent change\n").expect("tracked file should change");
        fs::write(repo.join("new.txt"), "parent addition\n")
            .expect("untracked file should be written");

        let session =
            WorktreeSession::create_in(&repo, "copy-changes").expect("worktree should be created");
        session
            .copy_changes_from(&repo)
            .expect("parent changes should be copied");
        assert_eq!(
            fs::read_to_string(session.path().join("README.md")).expect("tracked file exists"),
            "parent change\n"
        );
        assert_eq!(
            fs::read_to_string(session.path().join("new.txt")).expect("untracked file exists"),
            "parent addition\n"
        );

        let path = session.path().to_path_buf();
        session.finish();
        assert!(path.exists(), "dirty child worktree should be retained");
        Command::new("git")
            .args(["worktree", "remove", "--force"])
            .arg(&path)
            .current_dir(&repo)
            .output()
            .expect("git worktree remove should run");
        Command::new("git")
            .args(["branch", "-D", "copy-changes"])
            .current_dir(&repo)
            .output()
            .expect("git branch delete should run");
        fs::remove_dir_all(repo.parent().expect("repo has a parent"))
            .expect("test directory should be removed");
    }

    #[test]
    fn copy_changes_preserves_staged_and_unstaged_state() {
        let repo = temp_repo("copy-staged");
        let git = |args: &[&str], cwd: &Path| {
            let output = Command::new("git")
                .args(args)
                .current_dir(cwd)
                .output()
                .expect("git should run");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output
        };
        fs::write(repo.join("README.md"), "staged\n").expect("staged file should be written");
        git(&["add", "README.md"], &repo);
        fs::write(repo.join("README.md"), "staged and unstaged\n")
            .expect("unstaged file should be written");

        let session =
            WorktreeSession::create_in(&repo, "copy-staged").expect("worktree should be created");
        session
            .copy_changes_from(&repo)
            .expect("parent changes should be copied");
        assert_eq!(
            fs::read_to_string(session.path().join("README.md")).expect("tracked file exists"),
            "staged and unstaged\n"
        );
        let staged = git(&["diff", "--cached", "--", "README.md"], session.path());
        assert!(
            String::from_utf8_lossy(&staged.stdout).contains("+staged\n"),
            "child index should contain the staged version"
        );
        let unstaged = git(&["diff", "--", "README.md"], session.path());
        assert!(
            String::from_utf8_lossy(&unstaged.stdout).contains("+staged and unstaged\n"),
            "child worktree should retain the unstaged version"
        );

        let path = session.path().to_path_buf();
        session.abort();
        assert!(!path.exists(), "test worktree should be removed");
        fs::remove_dir_all(repo.parent().expect("repo has a parent"))
            .expect("test directory should be removed");
    }

    #[cfg(unix)]
    #[test]
    fn git_path_bytes_preserve_non_utf8_names() {
        let raw = b"nested/\xFFname";
        assert_eq!(path_from_git_bytes(raw).into_os_string().into_vec(), raw);
    }

    #[test]
    fn abort_removes_worktree_and_branch() {
        let repo = temp_repo("abort");
        let session =
            WorktreeSession::create_in(&repo, "abort-me").expect("worktree should be created");
        let path = session.path().to_path_buf();

        session.abort();

        assert!(!path.exists(), "aborted worktree should be removed");
        assert_eq!(listed_branch(&repo, "abort-me"), "");
        fs::remove_dir_all(repo.parent().expect("repo has a parent"))
            .expect("test directory should be removed");
    }

    #[test]
    fn path_for_preserves_source_subdirectory() {
        let repo = temp_repo("subdirectory");
        let source = repo.join("packages").join("foo");
        fs::create_dir_all(&source).expect("source subdirectory should be created");
        let session = WorktreeSession::create_in(&source, "subdirectory")
            .expect("worktree should be created");

        assert_eq!(
            session.path_for(&source).expect("path should map"),
            session.path().join("packages").join("foo")
        );

        session.finish();
        assert!(
            !repo
                .parent()
                .expect("repo has a parent")
                .join("repo-wt-subdirectory")
                .exists()
        );
        fs::remove_dir_all(repo.parent().expect("repo has a parent"))
            .expect("test directory should be removed");
    }

    #[test]
    fn create_fails_when_branch_exists_or_outside_repo() {
        let repo = temp_repo("conflict");
        let session =
            WorktreeSession::create_in(&repo, "taken").expect("worktree should be created");
        let path = session.path().to_path_buf();

        let error = WorktreeSession::create_in(&repo, "taken")
            .expect_err("existing branch must be rejected");
        assert!(
            error.to_string().contains("already exists"),
            "unexpected error: {error:#}"
        );
        session.finish();
        assert!(!path.exists());

        let outside =
            std::env::temp_dir().join(format!("maestro-worktree-norepo-{}", std::process::id()));
        fs::create_dir_all(&outside).expect("directory should be created");
        let error = WorktreeSession::create_in(&outside, "x")
            .expect_err("non-repository directories must be rejected");
        assert!(
            error.to_string().contains("git repository"),
            "unexpected error: {error:#}"
        );
        fs::remove_dir_all(&outside).expect("test directory should be removed");
        fs::remove_dir_all(repo.parent().expect("repo has a parent"))
            .expect("test directory should be removed");
    }
}
