//! Legacy (`legacy-1`) behavior for the bash tool.
//!
//! Preserves the observable behavior the tool had before the #3070 security
//! hardening (commit `2d90fd7a17d54`), so sessions recorded against that
//! behavior can be replayed with identical approval and output-capture
//! semantics:
//!
//! - **Auto-approval**: `find` exec/delete flags were detected with a plain
//!   `split_whitespace()`, so quoted flags (`find . "-delete"`) slipped past
//!   the guard and were auto-approved. `git branch` and `git remote` were
//!   treated as read-only for *all* arguments (including `-D`/`set-url`),
//!   and `cargo check` was auto-approved.
//! - **Full-output captures**: oversized combined output was written to the
//!   shared system temp dir with default file permissions; there was no
//!   private `~/.composer/logs/bash-output` dir, no 0600 mode, and no sweep
//!   of stale captures.
//!
//! Nothing here is called unless the tool is pinned to
//! [`super::super::BashVersion::Legacy1`]; current behavior lives in
//! `bash/mod.rs`.

use std::path::PathBuf;

use crate::safety::analyze_bash_command;

/// Pre-#3070 approval classification.
///
/// Identical to the historical `BashTool::requires_approval` before the
/// security hardening: quote-blind `find` flag detection, `cargo check`
/// treated as read-only, and no mutating-argument checks for
/// `git branch`/`git remote`.
pub(crate) fn requires_approval(command: &str) -> bool {
    fn is_find_with_exec(cmd_trimmed: &str) -> bool {
        if !cmd_trimmed.starts_with("find ") && cmd_trimmed != "find" {
            return false;
        }

        // Legacy detection split on whitespace only, so quoted flags such as
        // `find . "-delete"` were not recognized and were auto-approved.
        cmd_trimmed
            .split_whitespace()
            .map(|token| {
                token
                    .to_lowercase()
                    .trim_end_matches([';', '+', '\\'])
                    .to_string()
            })
            .any(|token| {
                matches!(
                    token.as_str(),
                    "-exec" | "-execdir" | "-ok" | "-okdir" | "-delete"
                )
            })
    }

    fn is_safe_segment(cmd_trimmed: &str) -> bool {
        if cmd_trimmed.is_empty() {
            return false;
        }

        if is_find_with_exec(cmd_trimmed) {
            return false;
        }

        // Commands that are always safe (read-only). The legacy list also
        // auto-approved `cargo check` and did not special-case mutating
        // `git branch`/`git remote` arguments.
        let safe_prefixes = [
            "ls ",
            "ls\n",
            "cat ",
            "head ",
            "tail ",
            "grep ",
            "find ",
            "pwd",
            "echo ",
            "which ",
            "type ",
            "file ",
            "stat ",
            "wc ",
            "du ",
            "df ",
            "env",
            "printenv",
            "date",
            "whoami",
            "hostname",
            "uname",
            "git status",
            "git log",
            "git diff",
            "git branch",
            "git remote",
            "git show",
            "cargo --version",
            "cargo check",
            "rustc --version",
            "node --version",
            "npm --version",
            "bun --version",
            "python --version",
        ];

        for prefix in safe_prefixes {
            if cmd_trimmed.starts_with(prefix) || cmd_trimmed == prefix.trim() {
                return true;
            }
        }

        false
    }

    let cmd_trimmed = command.trim();

    if cmd_trimmed.is_empty() {
        return true;
    }

    let analysis = analyze_bash_command(cmd_trimmed);

    if analysis.has_command_substitution || analysis.has_background {
        return true;
    }

    if analysis.has_redirects && cmd_trimmed.contains('>') {
        return true;
    }

    if analysis.commands.is_empty() {
        return true;
    }

    if analysis
        .commands
        .iter()
        .all(|cmd| is_safe_segment(cmd.raw.trim()))
    {
        return false;
    }

    // Everything else requires approval
    true
}

/// Generate a unique temp file path the way the legacy behavior did: directly
/// in the shared system temp dir, with no private state dir and no sweep of
/// stale captures.
pub(crate) fn temp_capture_path() -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();

    std::env::temp_dir().join(format!("composer-bash-{pid}-{timestamp}.log"))
}

/// Create the capture file with default permissions, as the legacy behavior
/// did (`tokio::fs::File::create`, i.e. 0666 masked by umask — world-readable
/// on a typical 022 umask, unlike the current 0600 mode).
pub(crate) async fn create_capture_file(
    path: &std::path::Path,
) -> std::io::Result<tokio::fs::File> {
    tokio::fs::File::create(path).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::BashVersion;

    #[test]
    fn legacy_approval_differs_where_hardening_changed_it() {
        // Quoted find flags slip past the legacy whitespace-split guard but
        // are caught by the current quote-aware tokenizer.
        assert!(!requires_approval("find . \"-delete\""));
        assert!(BashVersion::Current.requires_approval("find . \"-delete\""));

        // Mutating git branch/remote arguments were auto-approved in legacy.
        assert!(!requires_approval("git branch -D feature"));
        assert!(BashVersion::Current.requires_approval("git branch -D feature"));
        assert!(!requires_approval(
            "git remote set-url origin https://evil.example/repo.git"
        ));
        assert!(
            BashVersion::Current
                .requires_approval("git remote set-url origin https://evil.example/repo.git")
        );

        // cargo check runs build scripts; only legacy treated it as read-only.
        assert!(!requires_approval("cargo check"));
        assert!(BashVersion::Current.requires_approval("cargo check"));

        // Legacy remains pinned; current Git reads must disable configured helpers.
        for cmd in ["git status", "git log --oneline", "git branch"] {
            assert!(!requires_approval(cmd), "legacy: {cmd}");
            assert!(
                BashVersion::Current.requires_approval(cmd),
                "current: {cmd}"
            );
        }
    }

    #[test]
    fn legacy_approval_matches_current_where_untouched() {
        for cmd in ["ls -la", "git remote -v", "find . -name '*.rs'"] {
            assert_eq!(
                requires_approval(cmd),
                BashVersion::Current.requires_approval(cmd),
                "approval should match for {cmd:?}"
            );
        }
        for cmd in [
            "rm file.txt",
            "git push",
            "find . -exec rm -rf {} +",
            "echo hello > out.txt",
            "npm install",
        ] {
            assert_eq!(
                requires_approval(cmd),
                BashVersion::Current.requires_approval(cmd),
                "approval should match for {cmd:?}"
            );
        }
    }

    #[test]
    fn legacy_capture_path_uses_shared_temp_dir() {
        let path = temp_capture_path();
        assert!(path.starts_with(std::env::temp_dir()));
        assert!(path.to_string_lossy().contains("composer-bash-"));
        assert!(!path.to_string_lossy().contains("bash-output"));
    }

    #[tokio::test]
    async fn legacy_capture_file_uses_default_permissions() {
        let path = temp_capture_path();
        let file = create_capture_file(&path).await.unwrap();
        drop(file);
        assert!(path.exists());
        // Unlike the current 0600 mode, legacy creation applies no explicit
        // mode, so permission bits come from the process umask. Compare
        // against a control file created the same way in this test so the
        // assertion holds under any umask.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let control = std::env::temp_dir().join(format!(
                "composer-bash-umask-control-{}.log",
                std::process::id()
            ));
            drop(std::fs::File::create(&control).unwrap());
            let legacy_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            let control_mode = std::fs::metadata(&control).unwrap().permissions().mode() & 0o777;
            assert_eq!(legacy_mode, control_mode);
            let _ = std::fs::remove_file(&control);
        }
        let _ = std::fs::remove_file(&path);
    }
}
