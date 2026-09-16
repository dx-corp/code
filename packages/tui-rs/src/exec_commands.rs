//! Executable slash commands (Droid-style).
//!
//! Executable files with a shebang (`#!`) become slash commands:
//! - `~/.composer/commands/` (user commands)
//! - `.composer/commands/` (project commands)
//!
//! A file `deploy` in one of those directories is invoked as `/deploy [args...]`:
//! the script runs with the session environment, arguments after the command
//! name are passed as argv, and stdout/stderr are captured (each capped at
//! [`MAX_OUTPUT_BYTES`]) and posted into the transcript together with the
//! script source for transparency.
//!
//! A file qualifies only when it is a regular file, has the executable bit
//! set, and starts with `#!`. Markdown files (`.md`) are skipped — those are
//! prompt templates handled by [`crate::prompts`]. Name collisions resolve
//! project-over-user, and built-in commands always win over executable ones
//! (see [`crate::commands::register_exec_commands`]).
//!
//! Trust model: same as repo hooks — the user placed these executables
//! deliberately, so they run without an approval prompt.

use crate::path_utils::legacy_composer_home_dir;
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use wait_timeout::ChildExt;

/// Maximum bytes captured from each of stdout and stderr (64 KB, Droid-style).
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// How long an executable command may run before it is killed.
const EXEC_COMMAND_TIMEOUT: Duration = Duration::from_mins(2);

#[cfg(target_os = "linux")]
const EXECUTABLE_BUSY_RETRIES: usize = 10;

#[cfg(target_os = "linux")]
const EXECUTABLE_BUSY_RETRY_DELAY: Duration = Duration::from_millis(10);

/// Where an executable command was discovered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecCommandSource {
    /// `~/.composer/commands/`
    User,
    /// A trusted plugin `commands/` directory.
    Plugin,
    /// `.composer/commands/` in the workspace
    Project,
}

impl ExecCommandSource {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Plugin => "plugin",
            Self::Project => "project",
        }
    }
}

/// An executable script discovered in a commands directory.
#[derive(Debug, Clone)]
pub struct ExecCommand {
    /// Command name (file name, invoked as `/<name>`).
    pub name: String,
    /// Absolute or CWD-relative path to the script.
    pub path: PathBuf,
    /// Discovery origin (user / project).
    pub source: ExecCommandSource,
}

/// Captured result of an executable command run.
#[derive(Debug, Clone)]
pub struct ExecOutput {
    /// Captured stdout (lossy UTF-8, capped at [`MAX_OUTPUT_BYTES`]).
    pub stdout: String,
    /// Captured stderr (lossy UTF-8, capped at [`MAX_OUTPUT_BYTES`]).
    pub stderr: String,
    /// Process exit code (`None` when killed by a signal or timed out).
    pub exit_code: Option<i32>,
    /// Whether the run was killed after exceeding the timeout.
    pub timed_out: bool,
    /// Whether stdout or stderr exceeded the capture cap.
    pub truncated: bool,
}

/// Discover executable commands from the user and project commands directories.
///
/// Later directories override earlier ones by name (project wins over user).
#[must_use]
pub fn discover(workspace_dir: &Path) -> Vec<ExecCommand> {
    discover_with_plugin_dirs(workspace_dir, &[])
}

/// Discover executable commands while including trusted plugin directories.
#[must_use]
pub fn discover_with_plugin_dirs(
    workspace_dir: &Path,
    plugin_dirs: &[PathBuf],
) -> Vec<ExecCommand> {
    let mut dirs: Vec<(PathBuf, ExecCommandSource)> = Vec::new();
    if let Some(home) = legacy_composer_home_dir() {
        dirs.push((home.join("commands"), ExecCommandSource::User));
    }
    dirs.extend(
        plugin_dirs
            .iter()
            .cloned()
            .map(|path| (path, ExecCommandSource::Plugin)),
    );
    dirs.push((
        workspace_dir.join(".composer").join("commands"),
        ExecCommandSource::Project,
    ));
    discover_in_dirs(&dirs)
}

fn discover_in_dirs(dirs: &[(PathBuf, ExecCommandSource)]) -> Vec<ExecCommand> {
    let mut by_name: HashMap<String, ExecCommand> = HashMap::new();
    for (dir, source) in dirs {
        for cmd in scan_dir(dir, *source) {
            by_name.insert(cmd.name.to_lowercase(), cmd);
        }
    }
    let mut cmds: Vec<_> = by_name.into_values().collect();
    cmds.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.path.cmp(&b.path))
    });
    cmds
}

fn scan_dir(dir: &Path, source: ExecCommandSource) -> Vec<ExecCommand> {
    let mut out = Vec::new();
    if dir.is_file() {
        let Some(name) = dir
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
        else {
            return out;
        };
        if !name.starts_with('.')
            && dir.extension().is_none_or(|extension| extension != "md")
            && is_executable_script(dir)
        {
            out.push(ExecCommand {
                name,
                path: dir.to_path_buf(),
                source,
            });
        }
        return out;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let Some(name) = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(std::string::ToString::to_string)
        else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        // Markdown files are prompt templates handled by `prompts.rs`.
        if path.extension().is_some_and(|ext| ext == "md") {
            continue;
        }
        if !is_executable_script(&path) {
            continue;
        }
        out.push(ExecCommand { name, path, source });
    }
    out
}

fn is_executable_script(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() || !has_executable_bit(&metadata) {
        return false;
    }
    has_shebang(path)
}

fn has_executable_bit(metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        true
    }
}

fn has_shebang(path: &Path) -> bool {
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; 2];
    file.read_exact(&mut buf).is_ok() && &buf == b"#!"
}

/// Split raw argument text into argv tokens, honoring single/double quotes.
#[must_use]
pub fn tokenize_args(raw: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut quote_char = ' ';

    for ch in raw.chars() {
        if !in_quotes && (ch == '"' || ch == '\'') {
            in_quotes = true;
            quote_char = ch;
            continue;
        }
        if in_quotes && ch == quote_char {
            in_quotes = false;
            continue;
        }
        if !in_quotes && ch.is_whitespace() {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            continue;
        }
        current.push(ch);
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Read a script's source for transcript display, capped at `max_bytes`.
#[must_use]
pub fn read_source(path: &Path, max_bytes: usize) -> String {
    match fs::read(path) {
        Ok(bytes) => {
            if bytes.len() > max_bytes {
                format!(
                    "{}\n... (source truncated)",
                    String::from_utf8_lossy(&bytes[..max_bytes])
                )
            } else {
                String::from_utf8_lossy(&bytes).into_owned()
            }
        }
        Err(err) => format!("(failed to read script source: {err})"),
    }
}

/// Run an executable command, capturing stdout/stderr (each capped at
/// [`MAX_OUTPUT_BYTES`]). The process inherits the session environment and
/// runs in `cwd`. Runs longer than the timeout are killed.
pub fn run(path: &Path, args: &[String], cwd: &Path) -> std::io::Result<ExecOutput> {
    let mut child = spawn_exec(path, args, cwd)?;

    let (Some(mut stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
        let _ = child.kill();
        return Err(std::io::Error::other(
            "failed to capture child output pipes",
        ));
    };
    let stdout_handle = std::thread::spawn(move || read_capped(&mut stdout));
    let stderr_handle = std::thread::spawn(move || read_capped(&mut stderr));

    let (status, timed_out) = match child.wait_timeout(EXEC_COMMAND_TIMEOUT) {
        Ok(Some(status)) => (Some(status), false),
        Ok(None) => {
            let _ = child.kill();
            (child.wait().ok(), true)
        }
        Err(err) => return Err(err),
    };

    let (stdout_bytes, stdout_truncated) = stdout_handle.join().unwrap_or_default();
    let (stderr_bytes, stderr_truncated) = stderr_handle.join().unwrap_or_default();

    Ok(ExecOutput {
        stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
        exit_code: status.and_then(|s| s.code()),
        timed_out,
        truncated: stdout_truncated || stderr_truncated,
    })
}

#[cfg(target_os = "linux")]
fn spawn_exec(path: &Path, args: &[String], cwd: &Path) -> std::io::Result<std::process::Child> {
    for retries in 0..=EXECUTABLE_BUSY_RETRIES {
        match Command::new(path)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => return Ok(child),
            Err(err)
                if err.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && retries < EXECUTABLE_BUSY_RETRIES =>
            {
                std::thread::sleep(EXECUTABLE_BUSY_RETRY_DELAY);
            }
            Err(err) => return Err(err),
        }
    }
    unreachable!("the final retry returns its error")
}

#[cfg(not(target_os = "linux"))]
fn spawn_exec(path: &Path, args: &[String], cwd: &Path) -> std::io::Result<std::process::Child> {
    Command::new(path)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
}

/// Read a stream to EOF, keeping at most [`MAX_OUTPUT_BYTES`] and discarding
/// the rest (so the child never blocks on a full pipe). Returns the kept
/// bytes and whether anything was discarded.
fn read_capped<R: Read>(reader: &mut R) -> (Vec<u8>, bool) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    loop {
        match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let remaining = MAX_OUTPUT_BYTES.saturating_sub(buf.len());
                if remaining > 0 {
                    buf.extend_from_slice(&chunk[..n.min(remaining)]);
                }
                if n > remaining {
                    truncated = true;
                }
            }
        }
    }
    (buf, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_script(dir: &Path, name: &str, contents: &str, executable: bool) -> PathBuf {
        let path = dir.join(name);
        fs::create_dir_all(dir).unwrap();
        fs::write(&path, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = if executable { 0o755 } else { 0o644 };
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        }
        let _ = executable;
        path
    }

    #[test]
    fn discovers_executable_shebang_scripts() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("commands");
        write_script(&dir, "deploy", "#!/bin/sh\necho hi\n", true);

        let cmds = discover_in_dirs(&[(dir, ExecCommandSource::Project)]);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "deploy");
        assert_eq!(cmds[0].source, ExecCommandSource::Project);
    }

    #[test]
    fn skips_non_executable_and_shebangless_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("commands");
        // Executable bit but no shebang.
        write_script(&dir, "no-shebang", "echo hi\n", true);
        // Shebang but not executable.
        write_script(&dir, "not-exec", "#!/bin/sh\necho hi\n", false);
        // Plain text file.
        write_script(&dir, "notes.txt", "hello\n", false);

        let cmds = discover_in_dirs(&[(dir, ExecCommandSource::User)]);
        assert!(cmds.is_empty());
    }

    #[test]
    fn skips_markdown_and_hidden_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("commands");
        // Prompt templates belong to prompts.rs even when executable.
        write_script(&dir, "review.md", "#!/bin/sh\necho hi\n", true);
        write_script(&dir, ".hidden", "#!/bin/sh\necho hi\n", true);

        let cmds = discover_in_dirs(&[(dir, ExecCommandSource::Project)]);
        assert!(cmds.is_empty());
    }

    #[test]
    fn project_overrides_user_on_name_collision() {
        let tmp = tempfile::TempDir::new().unwrap();
        let user_dir = tmp.path().join("user").join("commands");
        let project_dir = tmp.path().join("project").join("commands");
        write_script(&user_dir, "deploy", "#!/bin/sh\necho user\n", true);
        write_script(&project_dir, "deploy", "#!/bin/sh\necho project\n", true);

        let cmds = discover_in_dirs(&[
            (user_dir, ExecCommandSource::User),
            (project_dir.clone(), ExecCommandSource::Project),
        ]);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].source, ExecCommandSource::Project);
        assert!(cmds[0].path.starts_with(&project_dir));
    }

    #[test]
    fn discovers_executable_plugin_commands() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        let plugin_commands = tmp.path().join("plugin").join("commands");
        write_script(
            &plugin_commands,
            "plugin-task",
            "#!/bin/sh\necho plugin\n",
            true,
        );

        let commands =
            discover_with_plugin_dirs(&workspace, std::slice::from_ref(&plugin_commands));

        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].source, ExecCommandSource::Plugin);
        assert!(commands[0].path.starts_with(plugin_commands));
    }

    #[test]
    fn discovers_an_exact_filtered_plugin_command_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        let path = write_script(
            &tmp.path().join("plugin/commands"),
            "plugin-task",
            "#!/bin/sh\necho plugin\n",
            true,
        );

        let commands = discover_with_plugin_dirs(&workspace, std::slice::from_ref(&path));

        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].path, path);
        assert_eq!(commands[0].source, ExecCommandSource::Plugin);
    }

    #[test]
    fn tokenize_args_handles_quotes_and_named_args() {
        assert_eq!(
            tokenize_args("foo bar baz"),
            vec!["foo".to_string(), "bar".to_string(), "baz".to_string()]
        );
        assert_eq!(
            tokenize_args(r#"TITLE="Fix the bug" --flag 'two words'"#),
            vec![
                "TITLE=Fix the bug".to_string(),
                "--flag".to_string(),
                "two words".to_string()
            ]
        );
        assert!(tokenize_args("   ").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn run_captures_output_args_and_exit_code() {
        let tmp = tempfile::TempDir::new().unwrap();
        let script = write_script(
            tmp.path(),
            "echo-args",
            "#!/bin/sh\necho \"out:$1:$2\"\necho \"err\" >&2\nexit 3\n",
            true,
        );
        let args = vec!["one".to_string(), "two words".to_string()];
        let output = run(&script, &args, tmp.path()).unwrap();
        assert_eq!(output.stdout.trim(), "out:one:two words");
        assert_eq!(output.stderr.trim(), "err");
        assert_eq!(output.exit_code, Some(3));
        assert!(!output.timed_out);
        assert!(!output.truncated);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn run_retries_while_executable_is_temporarily_busy() {
        use std::fs::OpenOptions;

        let tmp = tempfile::TempDir::new().unwrap();
        let script = write_script(tmp.path(), "busy", "#!/bin/sh\necho ready\n", true);
        let writer = OpenOptions::new().write(true).open(&script).unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(25));
            drop(writer);
        });

        let output = run(&script, &[], tmp.path()).unwrap();
        release.join().unwrap();
        assert_eq!(output.stdout.trim(), "ready");
        assert_eq!(output.exit_code, Some(0));
    }

    #[cfg(unix)]
    #[test]
    fn run_caps_output_at_64kb() {
        let tmp = tempfile::TempDir::new().unwrap();
        let script = write_script(
            tmp.path(),
            "flood",
            "#!/bin/sh\nhead -c 100000 /dev/zero | tr '\\0' 'a'\n",
            true,
        );
        let output = run(&script, &[], tmp.path()).unwrap();
        assert_eq!(output.stdout.len(), MAX_OUTPUT_BYTES);
        assert!(output.truncated);
        assert_eq!(output.exit_code, Some(0));
    }

    #[test]
    fn builtin_commands_win_over_executable_commands() {
        use crate::commands::{build_command_registry, register_exec_commands};

        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("commands");
        write_script(&dir, "help", "#!/bin/sh\necho shadowed\n", true);
        write_script(&dir, "my-deploy", "#!/bin/sh\necho ok\n", true);
        let execs = discover_in_dirs(&[(dir, ExecCommandSource::Project)]);

        let mut registry = build_command_registry();
        let builtin_desc = registry.get("help").unwrap().description.clone();
        let skipped = register_exec_commands(&mut registry, &execs);

        assert_eq!(skipped, vec!["help".to_string()]);
        // The built-in /help is untouched.
        assert_eq!(registry.get("help").unwrap().description, builtin_desc);
        // Non-colliding executable commands register and dispatch correctly.
        let cmd = registry.get("my-deploy").unwrap();
        assert!(cmd.description.contains("Executable command"));
    }
}
