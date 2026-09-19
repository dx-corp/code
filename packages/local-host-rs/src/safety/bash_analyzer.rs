//! Bash Command Analyzer
//!
//! Analyzes shell commands with a bounded tree-sitter Bash parse and determines risk.

use std::collections::HashSet;
use std::ops::ControlFlow;
use std::time::{Duration, Instant};
use tree_sitter::{Node, ParseOptions, Parser};

const MAX_BASH_SOURCE_BYTES: usize = 1_048_576;
const MAX_BASH_NODES: usize = 50_000;

/// Risk level for a bash command
///
/// The variants are declared least to most severe, so the derived `Ord`
/// orders them `Safe < RequiresApproval < Dangerous`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CommandRisk {
    /// Safe - read-only operations
    Safe,
    /// Requires approval - potentially destructive
    RequiresApproval,
    /// Dangerous - high risk operation
    Dangerous,
}

/// Analysis result for a bash command
#[derive(Debug, Clone)]
pub struct BashAnalysis {
    /// Overall risk level
    pub risk: CommandRisk,
    /// Reason for the risk assessment
    pub reason: String,
    /// Commands found in the input
    pub commands: Vec<ParsedCommand>,
    /// Whether the command has pipes
    pub has_pipes: bool,
    /// Whether the command has redirects
    pub has_redirects: bool,
    /// Whether the command has subshells
    pub has_subshell: bool,
    /// Whether the command has background jobs
    pub has_background: bool,
    /// Whether the command has command substitution
    pub has_command_substitution: bool,
}

/// A parsed command with program and arguments
#[derive(Debug, Clone)]
pub struct ParsedCommand {
    /// The program name
    pub program: String,
    /// Arguments to the program
    pub args: Vec<String>,
    /// Raw command string
    pub raw: String,
}

/// Safe read-only commands that don't require approval
static SAFE_COMMANDS: std::sync::LazyLock<HashSet<&'static str>> = std::sync::LazyLock::new(|| {
    [
        // File reading
        "cat", "head", "tail", "less", "more", "bat", // Search
        "grep", "rg", "ag", "find", "fd", "locate", // Directory
        "ls", "pwd", "tree", "exa", // Output
        "echo", "printf", // Text processing (read-only)
        "wc", "sort", "uniq", "diff", "cut", "tr", "awk", "sed", // Metadata
        "file", "stat", "du", "df", // Lookup
        "which", "whereis", "type", "command", // Docs
        "man", "help", "info", // System info
        "date", "cal", "whoami", "id", "groups", "hostname", "uname", "env", "printenv",
        // Modern tools
        "jq", "yq", "fzf", // Testing
        "test", "[", "true",
        "false",
        // NOTE: `tee` and `xargs` are intentionally NOT in this set. Both are
        // always-writes-or-executes commands as bare program names: `tee`'s
        // sole purpose is writing its arguments to files (there is no
        // read-only invocation), and `xargs` executes whatever command is
        // handed to it (including one built from piped-in, attacker-influenced
        // data). See CONDITIONALLY_DANGEROUS below.
    ]
    .into_iter()
    .collect()
});

/// Safe git subcommands that don't modify the repository
static SAFE_GIT_SUBCOMMANDS: std::sync::LazyLock<HashSet<&'static str>> =
    std::sync::LazyLock::new(|| {
        [
            "status",
            "log",
            "diff",
            "show",
            "branch",
            "tag",
            "remote",
            "config",
            "describe",
            "rev-parse",
            "ls-files",
            "ls-tree",
            "blame",
            "shortlog",
            "reflog",
            "stash",
        ]
        .into_iter()
        .collect()
    });

/// Check whether arguments to a nominally read-only git subcommand actually
/// mutate repository state (e.g. `git branch -D`, `git remote set-url`,
/// `git tag -d`, `git stash drop`, `git config name value`).
pub(crate) fn git_args_are_mutating(subcommand: &str, args: &[String]) -> bool {
    // Tokens that turn a nominally read-only subcommand into a mutation.
    const MUTATING_TOKENS: &[&str] = &[
        "-d", "-D", "--delete", "add", "remove", "rm", "set-url", "prune", "drop", "clear",
    ];
    if args
        .iter()
        .any(|arg| MUTATING_TOKENS.contains(&arg.as_str()))
    {
        return true;
    }

    match subcommand {
        // `git branch foo` creates a branch; `git tag v1` creates a tag.
        // List mode (`-l`/`--list`) takes a pattern argument and stays read-only.
        "branch" | "tag" => {
            if args.iter().any(|arg| arg == "-l" || arg == "--list") {
                return false;
            }
            args.iter().any(|arg| !arg.starts_with('-'))
        }
        "config" => {
            const MUTATING_CONFIG_FLAGS: &[&str] = &[
                "--add",
                "--unset",
                "--unset-all",
                "--remove-section",
                "--rename-section",
                "--edit",
                "-e",
            ];
            if args
                .iter()
                .any(|arg| MUTATING_CONFIG_FLAGS.contains(&arg.as_str()))
            {
                return true;
            }
            // `git config <name>` reads; `git config <name> <value>` writes.
            args.iter().filter(|arg| !arg.starts_with('-')).count() > 1
        }
        _ => false,
    }
}

/// Dangerous git subcommands that require approval
static DANGEROUS_GIT_SUBCOMMANDS: std::sync::LazyLock<HashSet<&'static str>> =
    std::sync::LazyLock::new(|| {
        [
            "reset",
            "clean",
            "rm",
            "push",
            "rebase",
            "merge",
            "cherry-pick",
            "checkout",
            "restore",
            "switch",
        ]
        .into_iter()
        .collect()
    });

/// Commands that are always dangerous
static DANGEROUS_COMMANDS: std::sync::LazyLock<HashSet<&'static str>> =
    std::sync::LazyLock::new(|| {
        [
            // Destructive
            "rm",
            "rmdir",
            "shred",
            // Disk - including common mkfs variants
            "mkfs",
            "mkfs.ext2",
            "mkfs.ext3",
            "mkfs.ext4",
            "mkfs.xfs",
            "mkfs.btrfs",
            "mkfs.vfat",
            "mkfs.ntfs",
            "mkfs.fat",
            "dd",
            "fdisk",
            "parted",
            "format",
            // Permissions
            "chmod",
            "chown",
            "chgrp",
            // Process
            "kill",
            "killall",
            "pkill",
            // System
            "reboot",
            "shutdown",
            "halt",
            "poweroff",
            "init",
            "systemctl",
            "service",
            // Privilege
            "sudo",
            "su",
            "doas",
        ]
        .into_iter()
        .collect()
    });

/// Check if a command matches a dangerous command (including prefix matches)
fn is_dangerous_command(program: &str) -> bool {
    // Exact match
    if DANGEROUS_COMMANDS.contains(program) {
        return true;
    }
    // Prefix match for commands like mkfs.* (mkfs.ext4, mkfs.xfs, etc.)
    if program.starts_with("mkfs.") {
        return true;
    }
    false
}

/// Commands that can be dangerous with certain flags
static CONDITIONALLY_DANGEROUS: std::sync::LazyLock<HashSet<&'static str>> =
    std::sync::LazyLock::new(|| {
        [
            "mv", "cp", "tar", "zip", "unzip", "gzip", "gunzip", "curl", "wget", "scp", "rsync",
            "tee", "xargs",
        ]
        .into_iter()
        .collect()
    });

/// GNU `find` predicates that cause `find` to write file contents, delete
/// paths, or execute arbitrary commands. `find` itself is in `SAFE_COMMANDS`
/// (it is a read-only directory walk by default), so any of these predicates
/// slipping through here would let an attacker use "safe" `find` to write to
/// or execute anything the process can reach -- see `-fprintf`/`-fprint`
/// below, which write to an arbitrary path with no approval prompt and are
/// not caught by path containment (containment only applies to the
/// write/edit tools, never to bash).
const FIND_DANGEROUS_PREDICATES: &[&str] = &[
    // Execution
    "-exec", "-execdir", "-ok", "-okdir", // Deletion
    "-delete",
    // Arbitrary-path file writes (the GNU `-f*` family writes to a target
    // file named as the predicate's argument, independent of any shell
    // redirection, so the existing "requires approval on `>`" redirect
    // check never sees it).
    "-fprintf", "-fprint", "-fprint0", "-fls",
];

/// True if any token in a `find` invocation's arguments is one of the
/// predicates in [`FIND_DANGEROUS_PREDICATES`]. Tokens are matched
/// case-insensitively and with trailing `find`-syntax punctuation
/// (`;`, `+`, `\`) stripped so `-exec ... \;`/`-exec ... +` can't hide
/// behind a suffix, mirroring the quoting-aware tokenization already used
/// for command parsing.
pub(crate) fn find_has_dangerous_predicate(tokens: &[String]) -> bool {
    tokens.iter().any(|token| {
        let normalized = token
            .to_lowercase()
            .trim_end_matches([';', '+', '\\'])
            .to_string();
        FIND_DANGEROUS_PREDICATES.contains(&normalized.as_str())
    })
}

/// Whether a human is present to answer an approval prompt for this run.
///
/// Interactive sessions (the TUI) can escalate an unclassifiable command to
/// the user. Print/exec and other headless runs have no approval UI, so an
/// unclassifiable command there has only two possible outcomes: run it
/// unchecked, or refuse it. This enum makes the caller state which it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunAttendance {
    /// A human can be shown an approval prompt.
    #[default]
    Interactive,
    /// No approval UI exists for this run.
    Unattended,
}

/// Reason reported when the analyzer cannot parse a command and a human can
/// still be asked about it.
pub const UNPARSEABLE_INTERACTIVE_REASON: &str = "Bash command could not be parsed safely";

/// Reason reported when the analyzer cannot parse a command in a run with no
/// approval UI. The command is refused; it cannot be approved from the
/// conversation.
pub const UNPARSEABLE_UNATTENDED_REASON: &str = "Bash command could not be analyzed and this run has no approval prompt, so it was refused \
     and did not execute. It cannot be approved from this conversation; run it manually outside \
     the agent if you intend it.";

/// Analyze a bash command for safety
///
/// Equivalent to [`analyze_bash_command_with_attendance`] with
/// [`RunAttendance::Interactive`].
///
/// The command is classified twice: once as written, and once with every
/// ANSI-C quoted span (`$'...'`) decoded by [`canonicalize_for_matching`].
/// The higher of the two risk levels wins, so `$'\x72\x6d' -rf /` is rated
/// `Dangerous` like the `rm -rf /` it runs. The decoded form can only raise
/// the rating, never lower it, and the reported commands and structural flags
/// always come from the command as written.
#[must_use]
pub fn analyze_bash_command(command: &str) -> BashAnalysis {
    analyze_bash_command_with_attendance(command, RunAttendance::Interactive)
}

/// Analyze a bash command for safety, fail-closed when nobody can approve it.
///
/// A command the bounded tree-sitter parse rejects is unclassifiable: the
/// analyzer cannot say which programs it runs. Interactively that is rated
/// [`CommandRisk::RequiresApproval`] and the user decides. Under
/// [`RunAttendance::Unattended`] there is no user to decide, so it is rated
/// [`CommandRisk::Dangerous`] and refused instead of being executed unchecked.
#[must_use]
pub fn analyze_bash_command_with_attendance(
    command: &str,
    attendance: RunAttendance,
) -> BashAnalysis {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return BashAnalysis {
            risk: CommandRisk::Safe,
            reason: "Empty command".to_string(),
            commands: Vec::new(),
            has_pipes: false,
            has_redirects: false,
            has_subshell: false,
            has_background: false,
            has_command_substitution: false,
        };
    }

    let mut analysis = analyze_command_form(trimmed, attendance);

    let canonical = canonicalize_for_matching(trimmed);
    let canonical = canonical.trim();
    if canonical != trimmed {
        // A decoded form that no longer parses is not evidence of anything, so
        // it is ignored rather than being downgraded to "requires approval".
        if let Ok(parsed) = parse_commands(canonical) {
            let (risk, reason) = determine_risk(
                &parsed.commands,
                parsed.has_pipes,
                parsed.has_redirects,
                parsed.has_command_substitution,
            );
            if risk > analysis.risk {
                analysis.risk = risk;
                analysis.reason = format!("{reason} (after decoding ANSI-C quoting)");
            }
        }
    }

    analysis
}

/// Parse and classify one concrete form of a command.
fn analyze_command_form(trimmed: &str, attendance: RunAttendance) -> BashAnalysis {
    let Ok(parsed) = parse_commands(trimmed) else {
        let (risk, reason) = match attendance {
            RunAttendance::Interactive => (
                CommandRisk::RequiresApproval,
                UNPARSEABLE_INTERACTIVE_REASON,
            ),
            RunAttendance::Unattended => (CommandRisk::Dangerous, UNPARSEABLE_UNATTENDED_REASON),
        };
        return BashAnalysis {
            risk,
            reason: reason.to_string(),
            commands: Vec::new(),
            has_pipes: false,
            has_redirects: false,
            has_subshell: false,
            has_background: false,
            has_command_substitution: false,
        };
    };

    // Determine overall risk
    let (risk, reason) = determine_risk(
        &parsed.commands,
        parsed.has_pipes,
        parsed.has_redirects,
        parsed.has_command_substitution,
    );

    BashAnalysis {
        risk,
        reason,
        commands: parsed.commands,
        has_pipes: parsed.has_pipes,
        has_redirects: parsed.has_redirects,
        has_subshell: parsed.has_subshell,
        has_background: parsed.has_background,
        has_command_substitution: parsed.has_command_substitution,
    }
}

struct ParsedShell {
    commands: Vec<ParsedCommand>,
    has_pipes: bool,
    has_redirects: bool,
    has_subshell: bool,
    has_background: bool,
    has_command_substitution: bool,
}

/// Parse a command string into individual commands.
fn parse_commands(input: &str) -> Result<ParsedShell, ()> {
    if input.len() > MAX_BASH_SOURCE_BYTES {
        return Err(());
    }
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .map_err(|_| ())?;
    let started_at = Instant::now();
    let timeout = Duration::from_millis(50);
    let mut cancel_after_timeout = |_: &tree_sitter::ParseState| {
        if started_at.elapsed() >= timeout {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let bytes = input.as_bytes();
    let tree = parser
        .parse_with_options(
            &mut |offset, _| bytes.get(offset..).unwrap_or_default(),
            None,
            Some(ParseOptions::new().progress_callback(&mut cancel_after_timeout)),
        )
        .ok_or(())?;
    if tree.root_node().has_error() {
        return Err(());
    }

    let mut parsed = ParsedShell {
        commands: Vec::new(),
        has_pipes: false,
        has_redirects: false,
        has_subshell: false,
        has_background: false,
        has_command_substitution: false,
    };
    let mut stack = vec![tree.root_node()];
    let mut visited = 0usize;
    while let Some(node) = stack.pop() {
        visited += 1;
        if visited > MAX_BASH_NODES {
            return Err(());
        }
        classify_syntax_node(node, input, &mut parsed);
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    Ok(parsed)
}

fn classify_syntax_node(node: Node<'_>, source: &str, parsed: &mut ParsedShell) {
    match node.kind() {
        "command" => {
            if let Ok(raw) = node.utf8_text(source.as_bytes()) {
                if let Some(command) = parse_single_command(raw) {
                    parsed.commands.push(command);
                }
            }
        }
        "pipeline" => parsed.has_pipes = true,
        "redirected_statement" | "file_redirect" | "heredoc_redirect" => {
            parsed.has_redirects = true;
        }
        "subshell" => parsed.has_subshell = true,
        "command_substitution" | "process_substitution" => {
            parsed.has_command_substitution = true;
        }
        _ => {
            if !node.is_named() {
                if let Ok(text) = node.utf8_text(source.as_bytes()) {
                    if text == "&" {
                        parsed.has_background = true;
                    }
                }
            }
        }
    }
}

/// Parse a single command into program and arguments
fn parse_single_command(input: &str) -> Option<ParsedCommand> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Tokenize respecting quotes
    let tokens = tokenize(trimmed);
    if tokens.is_empty() {
        return None;
    }

    // Skip common wrappers
    let (program, args) = skip_wrappers(&tokens);

    Some(ParsedCommand {
        program: program.to_string(),
        args: args.iter().map(std::string::ToString::to_string).collect(),
        raw: trimmed.to_string(),
    })
}

/// Tokenize a command string, respecting quotes
pub(crate) fn tokenize(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escape_next = false;

    for ch in input.chars() {
        if escape_next {
            current.push(ch);
            escape_next = false;
            continue;
        }

        match ch {
            '\\' if !in_single_quote => {
                escape_next = true;
            }
            '\'' if !in_double_quote => {
                in_single_quote = !in_single_quote;
            }
            '"' if !in_single_quote => {
                in_double_quote = !in_double_quote;
            }
            ' ' | '\t' if !in_single_quote && !in_double_quote => {
                if !current.is_empty() {
                    tokens.push(current.clone());
                    current.clear();
                }
            }
            _ => {
                current.push(ch);
            }
        }
    }

    if !current.is_empty() {
        tokens.push(current);
    }

    tokens
}

/// Named ANSI-C escapes (`$'...'`) recognized by bash.
fn ansi_c_named_escape(ch: char) -> Option<char> {
    Some(match ch {
        'a' => '\u{7}',
        'b' => '\u{8}',
        'e' | 'E' => '\u{1b}',
        'f' => '\u{c}',
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        'v' => '\u{b}',
        '\\' => '\\',
        '\'' => '\'',
        '"' => '"',
        '?' => '?',
        _ => return None,
    })
}

/// Read up to `max` digits of the given radix starting at `chars[start]`.
/// Returns the parsed value and the number of digits consumed.
fn read_radix_digits(chars: &[char], start: usize, radix: u32, max: usize) -> Option<(u32, usize)> {
    let mut value: u32 = 0;
    let mut consumed = 0usize;
    while consumed < max {
        let Some(digit) = chars.get(start + consumed).and_then(|c| c.to_digit(radix)) else {
            break;
        };
        value = value.saturating_mul(radix).saturating_add(digit);
        consumed += 1;
    }
    if consumed == 0 {
        None
    } else {
        Some((value, consumed))
    }
}

/// Decode every ANSI-C quoted span (`$'...'`) in a shell command into the
/// literal text bash would pass to the program, leaving all other bytes
/// untouched.
///
/// `$'\x72\x6d' -rf /` is the same command as `rm -rf /`, but neither
/// [`tokenize`] nor the regexes in [`crate::safety::dangerous_patterns`] see
/// `rm` in the raw text: `tokenize` treats a backslash inside single quotes as
/// literal, so the program name comes out as `$\x72\x6d`. Classifying the
/// decoded form as well closes that gap.
///
/// The result is only ever used as an *additional* form to match against; it
/// is never executed and never lowers a risk rating. When the input contains
/// no `$'` the input is returned unchanged, so commands without ANSI-C quoting
/// are classified exactly as before.
///
/// Supported escapes: the named set (`\a \b \e \E \f \n \r \t \v \\ \' \" \?`),
/// `\xHH` (1-2 hex digits), `\uHHHH` (1-4 hex digits), `\UHHHHHHHH`
/// (1-8 hex digits), and `\nnn` (1-3 octal digits). An unrecognized escape
/// decodes to the escaped character itself, matching bash.
#[must_use]
pub fn canonicalize_for_matching(input: &str) -> String {
    if !input.contains("$'") {
        return input.to_string();
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mode {
        Plain,
        Single,
        Double,
        AnsiC,
    }

    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut mode = Mode::Plain;
    let mut idx = 0usize;

    while idx < chars.len() {
        let ch = chars[idx];
        match mode {
            Mode::Plain => {
                if ch == '$' && chars.get(idx + 1) == Some(&'\'') {
                    // Enter ANSI-C quoting. The `$'` opener is dropped: the
                    // decoded body replaces the whole span.
                    mode = Mode::AnsiC;
                    idx += 2;
                } else if ch == '\\' && idx + 1 < chars.len() {
                    // Preserve plain-mode escapes verbatim; `tokenize` already
                    // resolves them.
                    out.push(ch);
                    out.push(chars[idx + 1]);
                    idx += 2;
                } else {
                    if ch == '\'' {
                        mode = Mode::Single;
                    } else if ch == '"' {
                        mode = Mode::Double;
                    }
                    out.push(ch);
                    idx += 1;
                }
            }
            Mode::Single => {
                // Inside `'...'` nothing is special, including `$'`.
                if ch == '\'' {
                    mode = Mode::Plain;
                }
                out.push(ch);
                idx += 1;
            }
            Mode::Double => {
                // Inside `"..."`, `$'` is a literal dollar sign followed by a
                // quote, not ANSI-C quoting.
                if ch == '\\' && idx + 1 < chars.len() {
                    out.push(ch);
                    out.push(chars[idx + 1]);
                    idx += 2;
                    continue;
                }
                if ch == '"' {
                    mode = Mode::Plain;
                }
                out.push(ch);
                idx += 1;
            }
            Mode::AnsiC => {
                if ch == '\'' {
                    mode = Mode::Plain;
                    idx += 1;
                    continue;
                }
                if ch != '\\' || idx + 1 >= chars.len() {
                    out.push(ch);
                    idx += 1;
                    continue;
                }
                let escaped = chars[idx + 1];
                if let Some(decoded) = ansi_c_named_escape(escaped) {
                    out.push(decoded);
                    idx += 2;
                    continue;
                }
                let numeric = match escaped {
                    'x' => {
                        read_radix_digits(&chars, idx + 2, 16, 2).map(|(v, n)| (v, n + 2, false))
                    }
                    'u' => read_radix_digits(&chars, idx + 2, 16, 4).map(|(v, n)| (v, n + 2, true)),
                    'U' => read_radix_digits(&chars, idx + 2, 16, 8).map(|(v, n)| (v, n + 2, true)),
                    _ => read_radix_digits(&chars, idx + 1, 8, 3).map(|(v, n)| (v, n + 1, false)),
                };
                if let Some((value, consumed, is_unicode)) = numeric {
                    let decoded = if is_unicode {
                        char::from_u32(value)
                    } else {
                        u8::try_from(value).ok().map(char::from)
                    };
                    if let Some(decoded) = decoded {
                        out.push(decoded);
                        idx += consumed;
                        continue;
                    }
                }
                // Unrecognized escape: bash emits the escaped character.
                out.push(escaped);
                idx += 2;
            }
        }
    }

    out
}

/// Skip common command wrappers (nice, command, etc.)
fn skip_wrappers(tokens: &[String]) -> (&str, &[String]) {
    let mut idx = 0;

    while idx < tokens.len() {
        let token = &tokens[idx];

        // Skip nice with optional -n flag and priority
        if token == "nice" {
            idx += 1;
            // Skip -n and its numeric argument
            if idx < tokens.len() && tokens[idx] == "-n" {
                idx += 1;
                // Skip the numeric priority if present
                if idx < tokens.len() && tokens[idx].parse::<i32>().is_ok() {
                    idx += 1;
                }
            }
            continue;
        }

        // Skip simple wrappers that take no arguments before the command
        if token == "nohup" || token == "command" {
            idx += 1;
            continue;
        }

        // Skip time with optional flags
        if token == "time" {
            idx += 1;
            // Skip time flags
            while idx < tokens.len() && tokens[idx].starts_with('-') {
                idx += 1;
            }
            continue;
        }

        // Skip timeout with duration argument
        if token == "timeout" {
            idx += 1;
            // Skip duration argument (e.g., "5s", "30")
            if idx < tokens.len() && !tokens[idx].starts_with('-') {
                idx += 1;
            }
            continue;
        }

        // NOTE: We do NOT skip sudo/doas here because they represent
        // privilege escalation, which should be detected as dangerous.
        // The determine_risk function checks for these explicitly.

        break;
    }

    if idx < tokens.len() {
        (&tokens[idx], &tokens[idx + 1..])
    } else {
        ("", &[])
    }
}

/// Determine the risk level of parsed commands
fn determine_risk(
    commands: &[ParsedCommand],
    has_pipes: bool,
    has_redirects: bool,
    has_command_substitution: bool,
) -> (CommandRisk, String) {
    // Command substitution is always risky
    if has_command_substitution {
        return (
            CommandRisk::RequiresApproval,
            "Command contains command substitution".to_string(),
        );
    }

    let mut highest_risk = CommandRisk::Safe;
    let mut reason = "Read-only command".to_string();

    for cmd in commands {
        let program = cmd.program.to_lowercase();
        let program = program.as_str();

        // Check for always-dangerous commands (including prefix matches)
        if is_dangerous_command(program) {
            return (
                CommandRisk::Dangerous,
                format!("Dangerous command: {program}"),
            );
        }

        // Check for sudo/doas
        if program == "sudo" || program == "doas" || program == "su" {
            return (
                CommandRisk::Dangerous,
                "Command uses privilege escalation".to_string(),
            );
        }

        // Environment variables are an open-ended extension surface for the
        // wrapped program. A finite denylist cannot prove that an assignment
        // is harmless (`LESSOPEN`, for example, executes a command inside an
        // otherwise read-only `less`). Bare `env` only prints the environment,
        // but every argument-bearing invocation therefore requires approval.
        if program == "env" && !cmd.args.is_empty() {
            highest_risk = CommandRisk::RequiresApproval;
            reason = "env arguments can alter wrapped program behavior".to_string();
            continue;
        }

        // Check git subcommands
        if program == "git" && !cmd.args.is_empty() {
            let subcommand = cmd.args[0].to_lowercase();
            if DANGEROUS_GIT_SUBCOMMANDS.contains(subcommand.as_str()) {
                highest_risk = CommandRisk::RequiresApproval;
                reason = format!("Git {subcommand} can modify repository");
            } else if SAFE_GIT_SUBCOMMANDS.contains(subcommand.as_str()) {
                if git_args_are_mutating(&subcommand, &cmd.args[1..])
                    && highest_risk == CommandRisk::Safe
                {
                    highest_risk = CommandRisk::RequiresApproval;
                    reason = format!("Git {subcommand} arguments can modify repository");
                }
            } else if highest_risk == CommandRisk::Safe {
                highest_risk = CommandRisk::RequiresApproval;
                reason = format!("Unknown git subcommand: {subcommand}");
            }
            continue;
        }

        // `find` is in SAFE_COMMANDS (a bare directory walk is read-only),
        // but several of its predicates write files, delete paths, or exec
        // arbitrary commands. Those must not inherit `find`'s safe rating.
        if program == "find" && find_has_dangerous_predicate(&cmd.args) {
            if highest_risk == CommandRisk::Safe {
                highest_risk = CommandRisk::RequiresApproval;
                reason = "find uses a predicate that can write or execute".to_string();
            }
            continue;
        }

        // Check for conditionally dangerous commands
        if CONDITIONALLY_DANGEROUS.contains(program) {
            if highest_risk == CommandRisk::Safe {
                highest_risk = CommandRisk::RequiresApproval;
                reason = format!("{program} may modify files");
            }
            continue;
        }

        // Check if command is safe
        if !SAFE_COMMANDS.contains(program) && highest_risk == CommandRisk::Safe {
            highest_risk = CommandRisk::RequiresApproval;
            reason = format!("Unknown command: {program}");
        }
    }

    // Pipes with non-safe commands need approval
    if has_pipes && highest_risk == CommandRisk::Safe {
        // Check if ALL commands in pipe are safe
        let all_safe = commands.iter().all(|cmd| {
            let program = cmd.program.to_lowercase();
            if program == "find" && find_has_dangerous_predicate(&cmd.args) {
                return false;
            }
            SAFE_COMMANDS.contains(program.as_str())
                || (program == "git"
                    && !cmd.args.is_empty()
                    && SAFE_GIT_SUBCOMMANDS.contains(cmd.args[0].to_lowercase().as_str())
                    && !git_args_are_mutating(&cmd.args[0].to_lowercase(), &cmd.args[1..]))
        });

        if !all_safe {
            highest_risk = CommandRisk::RequiresApproval;
            reason = "Pipeline contains potentially unsafe commands".to_string();
        }
    }

    // Redirects to files need approval (could overwrite)
    if has_redirects && highest_risk == CommandRisk::Safe {
        highest_risk = CommandRisk::RequiresApproval;
        reason = "Command uses file redirection".to_string();
    }

    (highest_risk, reason)
}

/// Quick check if a command is likely safe
#[must_use]
pub fn is_likely_safe(command: &str) -> bool {
    let analysis = analyze_bash_command(command);
    analysis.risk == CommandRisk::Safe
}

/// Quick check if a command is dangerous
#[must_use]
pub fn is_dangerous(command: &str) -> bool {
    let analysis = analyze_bash_command(command);
    analysis.risk == CommandRisk::Dangerous
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_canonicalize_ansi_c_hex_escapes() {
        assert_eq!(canonicalize_for_matching(r"$'\x72\x6d' -rf /"), "rm -rf /");
    }

    #[test]
    fn test_canonicalize_ansi_c_octal_escapes() {
        assert_eq!(canonicalize_for_matching(r"$'\162\155' -rf /"), "rm -rf /");
    }

    #[test]
    fn test_canonicalize_ansi_c_unicode_escapes() {
        assert_eq!(canonicalize_for_matching(r"$'rm'"), "rm");
        assert_eq!(canonicalize_for_matching(r"$'\U00000072\U0000006d'"), "rm");
    }

    #[test]
    fn test_canonicalize_ansi_c_named_escapes() {
        assert_eq!(
            canonicalize_for_matching(r"echo $'hello\n'"),
            "echo hello\n"
        );
        assert_eq!(canonicalize_for_matching(r"echo $'a\tb'"), "echo a\tb");
        assert_eq!(canonicalize_for_matching(r"echo $'q\'z'"), "echo q'z");
    }

    #[test]
    fn test_canonicalize_leaves_other_quoting_alone() {
        // No `$'` anywhere: byte-for-byte identical, so classification of
        // every command without ANSI-C quoting is unchanged.
        for command in [
            "ls -la",
            r#"grep "pattern" file.txt"#,
            "echo 'literal $HOME'",
            r"echo \$HOME",
        ] {
            assert_eq!(
                canonicalize_for_matching(command),
                command,
                "unexpected rewrite of: {command}"
            );
        }
    }

    #[test]
    fn test_canonicalize_ignores_ansi_c_inside_other_quotes() {
        // Inside `"..."` and `'...'` bash does not apply ANSI-C decoding, so
        // neither does the canonicalizer.
        for command in [r#"echo "$'\x72\x6d'""#, r"echo 'a $'"] {
            assert_eq!(
                canonicalize_for_matching(command),
                command,
                "unexpected rewrite of: {command}"
            );
        }
    }

    #[test]
    fn test_ansi_c_quoted_dangerous_command_is_dangerous() {
        for command in [
            r"$'\x72\x6d' -rf /",
            r"$'\162\155' -rf /",
            r"$'rm' -rf /",
            r"$'s\x75do' anything",
        ] {
            let analysis = analyze_bash_command(command);
            assert_eq!(
                analysis.risk,
                CommandRisk::Dangerous,
                "expected Dangerous for: {command} (reason: {})",
                analysis.reason
            );
        }
    }

    #[test]
    fn test_benign_ansi_c_quoting_keeps_prior_risk() {
        // `echo` is a safe command and stays safe once `$'hello\n'` is decoded.
        let analysis = analyze_bash_command(r"echo $'hello\n'");
        assert_eq!(analysis.risk, CommandRisk::Safe, "{}", analysis.reason);

        // A benign-but-unknown program keeps its pre-existing rating rather
        // than being escalated by canonicalization.
        let analysis = analyze_bash_command(r"custom_cmd $'hello\n'");
        assert_eq!(
            analysis.risk,
            CommandRisk::RequiresApproval,
            "{}",
            analysis.reason
        );
    }

    #[test]
    fn test_command_risk_orders_least_to_most_severe() {
        assert!(CommandRisk::Safe < CommandRisk::RequiresApproval);
        assert!(CommandRisk::RequiresApproval < CommandRisk::Dangerous);
    }

    #[test]
    fn test_safe_commands() {
        assert!(is_likely_safe("ls -la"));
        assert!(is_likely_safe("cat file.txt"));
        assert!(is_likely_safe("grep pattern file"));
        assert!(is_likely_safe("git status"));
        assert!(is_likely_safe("git log"));
        assert!(is_likely_safe("pwd"));
        assert!(is_likely_safe("echo hello"));
    }

    #[test]
    fn test_dangerous_commands() {
        assert!(is_dangerous("rm -rf /"));
        // sudo gets detected via the determine_risk function
        let analysis = analyze_bash_command("sudo anything");
        assert_eq!(analysis.risk, CommandRisk::Dangerous);
        assert!(is_dangerous("shutdown now"));
        assert!(is_dangerous("mkfs.ext4 /dev/sda"));
    }

    #[test]
    fn test_git_subcommands() {
        // Safe git commands
        assert!(is_likely_safe("git status"));
        assert!(is_likely_safe("git log"));
        assert!(is_likely_safe("git diff"));
        assert!(is_likely_safe("git branch"));
        assert!(is_likely_safe("git branch -a"));
        assert!(is_likely_safe("git tag"));
        assert!(is_likely_safe("git tag -l 'v*'"));
        assert!(is_likely_safe("git stash list"));
        assert!(is_likely_safe("git config user.name"));
        assert!(is_likely_safe("git config --get user.name"));

        // Dangerous git commands
        let analysis = analyze_bash_command("git reset --hard");
        assert_eq!(analysis.risk, CommandRisk::RequiresApproval);

        let analysis = analyze_bash_command("git push --force");
        assert_eq!(analysis.risk, CommandRisk::RequiresApproval);
    }

    #[test]
    fn test_git_mutating_args_require_approval() {
        for cmd in [
            "git branch -D feature",
            "git branch -d feature",
            "git branch --delete feature",
            "git branch new-branch",
            "git tag -d v1.0",
            "git tag v1.0",
            "git remote add origin https://example.com/repo.git",
            "git remote set-url origin https://evil.example/repo.git",
            "git remote remove origin",
            "git remote prune origin",
            "git stash drop",
            "git stash clear",
            "git config user.name evil",
            "git config --unset user.name",
        ] {
            let analysis = analyze_bash_command(cmd);
            assert_eq!(
                analysis.risk,
                CommandRisk::RequiresApproval,
                "expected approval for: {cmd}"
            );
        }
    }

    #[test]
    fn test_pipes() {
        // Safe pipe
        assert!(is_likely_safe("cat file | grep pattern"));
        assert!(is_likely_safe("ls -la | head"));

        // Unsafe pipe (unknown command)
        let analysis = analyze_bash_command("cat file | custom_cmd");
        assert_eq!(analysis.risk, CommandRisk::RequiresApproval);
    }

    #[test]
    fn quoted_shell_operators_are_not_parsed_as_commands() {
        let analysis = analyze_bash_command("printf '%s|%s;still-one-command' left right");
        assert_eq!(analysis.commands.len(), 1);
        assert_eq!(analysis.commands[0].program, "printf");
        assert!(!analysis.has_pipes);
    }

    #[test]
    fn malformed_bash_requires_approval() {
        let analysis = analyze_bash_command("echo $(unterminated");
        assert_eq!(analysis.risk, CommandRisk::RequiresApproval);
        assert!(analysis.reason.contains("could not be parsed"));
    }

    #[test]
    fn malformed_bash_is_refused_when_unattended() {
        let analysis =
            analyze_bash_command_with_attendance("echo $(unterminated", RunAttendance::Unattended);
        assert_eq!(analysis.risk, CommandRisk::Dangerous);
        assert_eq!(analysis.reason, UNPARSEABLE_UNATTENDED_REASON);

        // Interactive classification is unchanged: a human can still decide.
        let interactive =
            analyze_bash_command_with_attendance("echo $(unterminated", RunAttendance::Interactive);
        assert_eq!(interactive.risk, CommandRisk::RequiresApproval);
        assert_eq!(interactive.reason, UNPARSEABLE_INTERACTIVE_REASON);
    }

    #[test]
    fn attendance_does_not_change_parseable_commands() {
        for command in ["ls -la", "rm -rf /", "cargo build", "git status"] {
            assert_eq!(
                analyze_bash_command_with_attendance(command, RunAttendance::Unattended).risk,
                analyze_bash_command(command).risk,
                "attendance changed the rating of: {command}"
            );
        }
    }

    #[test]
    fn test_redirects() {
        let analysis = analyze_bash_command("echo hello > file.txt");
        assert_eq!(analysis.risk, CommandRisk::RequiresApproval);
    }

    #[test]
    fn test_command_substitution() {
        let analysis = analyze_bash_command("echo $(whoami)");
        assert_eq!(analysis.risk, CommandRisk::RequiresApproval);

        let analysis = analyze_bash_command("echo `date`");
        assert_eq!(analysis.risk, CommandRisk::RequiresApproval);
    }

    #[test]
    fn test_tokenize() {
        let tokens = tokenize("echo 'hello world' \"foo bar\"");
        assert_eq!(tokens, vec!["echo", "hello world", "foo bar"]);
    }

    #[test]
    fn test_skip_wrappers() {
        let tokens = tokenize("nice -n 10 command myprogram arg1");
        let (program, args) = skip_wrappers(&tokens);
        assert_eq!(program, "myprogram");
        assert_eq!(args, &["arg1"]);
    }

    #[test]
    fn test_sudo_and_env_are_not_skipped() {
        // sudo is NOT skipped because it's a privilege escalation indicator
        let tokens = tokenize("sudo -u root rm -rf /tmp/test");
        let (program, _) = skip_wrappers(&tokens);
        assert_eq!(program, "sudo");

        // env is also authority-bearing: assignments can change the behavior
        // of the wrapped program in program-specific, open-ended ways.
        let tokens = tokenize("env VAR=value sudo rm -rf /tmp");
        let (program, _) = skip_wrappers(&tokens);
        assert_eq!(program, "env");
    }

    #[test]
    fn test_complex_command() {
        let analysis = analyze_bash_command("cd /tmp && git clone repo && npm install");
        assert!(analysis.commands.len() >= 2);
    }

    #[test]
    fn test_empty_command() {
        let analysis = analyze_bash_command("");
        assert_eq!(analysis.risk, CommandRisk::Safe);
        assert!(analysis.commands.is_empty());
    }

    // ========================================================================
    // Security regressions: `env` assignment injection (root cause B)
    // ========================================================================
    //
    // A finite list of loader/interpreter variables is not a proof that the
    // remaining environment is safe. Programs have their own hooks: GNU
    // `less`, for example, executes `LESSOPEN` while otherwise looking like a
    // read-only command. Keep `env` as the parsed program and require approval
    // for every argument-bearing invocation.

    #[test]
    fn test_env_arguments_do_not_resolve_to_wrapped_command() {
        let tokens = tokenize("env LD_PRELOAD=/tmp/evil.so cat /etc/hostname");
        let (program, args) = skip_wrappers(&tokens);
        assert_eq!(program, "env");
        assert_eq!(args[0], "LD_PRELOAD=/tmp/evil.so");

        for command in [
            "env LD_PRELOAD=/tmp/evil.so cat /etc/hostname",
            "env LESSOPEN='|sh -c \"echo INJECTED\" %s' less /etc/hostname",
            "env FOO=bar cat file",
        ] {
            let analysis = analyze_bash_command(command);
            assert_eq!(analysis.risk, CommandRisk::RequiresApproval, "{command}");
        }
        assert_eq!(analyze_bash_command("env").risk, CommandRisk::Safe);
    }

    #[test]
    fn test_env_dyld_insert_libraries_requires_approval() {
        let analysis =
            analyze_bash_command("env DYLD_INSERT_LIBRARIES=/tmp/evil.dylib cat /etc/hostname");
        assert_eq!(analysis.risk, CommandRisk::RequiresApproval);
    }

    #[test]
    fn test_env_other_loader_variables_require_approval() {
        for cmd in [
            "env BASH_ENV=/tmp/evil.sh cat file",
            "env ENV=/tmp/evil.sh cat file",
            "env PERL5OPT=-Mfoo perl -e 1",
            "env PYTHONSTARTUP=/tmp/evil.py python -c 1",
            "env PYTHONPATH=/tmp/evil python -c 1",
            "env NODE_OPTIONS=--require=/tmp/evil.js node -e 1",
            "env RUBYOPT=-r/tmp/evil ruby -e 1",
            "env GIT_SSH_COMMAND=/tmp/evil.sh git log",
            "env GIT_EDITOR=/tmp/evil.sh git log",
            "env GIT_ASKPASS=/tmp/evil.sh git log",
        ] {
            let analysis = analyze_bash_command(cmd);
            assert_eq!(
                analysis.risk,
                CommandRisk::RequiresApproval,
                "expected approval for: {cmd}"
            );
        }
    }

    // ========================================================================
    // Security regressions: `find` file-writing predicates (root cause A)
    // ========================================================================
    //
    // GNU find's `-fprintf`/-fprint`/`-fprint0`/`-fls` write to an arbitrary
    // path named as their own argument -- independent of shell redirection,
    // so they bypass both the redirect-approval check and (since containment
    // only applies to the write/edit tools, never to bash) path containment.

    #[test]
    fn test_find_write_predicates_require_approval() {
        for cmd in [
            "find . -maxdepth 0 -fprintf /home/developer/.bashrc x",
            "find . -maxdepth 0 -fprint /home/developer/.ssh/authorized_keys",
            "find . -fprint0 /tmp/out",
            "find . -fls /tmp/out",
        ] {
            let analysis = analyze_bash_command(cmd);
            assert_eq!(
                analysis.risk,
                CommandRisk::RequiresApproval,
                "expected approval for: {cmd}"
            );
        }
    }

    #[test]
    fn test_find_write_predicate_in_pipeline_requires_approval() {
        // Exercises the separate pipe "all_safe" check (has_pipes branch),
        // not just the main per-command loop.
        let analysis = analyze_bash_command(
            "echo start | find . -maxdepth 0 -fprintf /home/developer/.bashrc x",
        );
        assert_eq!(analysis.risk, CommandRisk::RequiresApproval);
    }

    #[test]
    fn test_find_write_predicate_in_conjunction_requires_approval() {
        let analysis =
            analyze_bash_command("echo start && find . -fprintf /home/developer/.bashrc x");
        assert_eq!(analysis.risk, CommandRisk::RequiresApproval);
    }

    #[test]
    fn test_find_dangerous_predicate_case_and_terminator_insensitive() {
        // Case-insensitive and with GNU find's `;`/`+` terminator punctuation
        // attached, mirroring how `-exec ... ;` is already handled.
        assert!(find_has_dangerous_predicate(&[
            "-FPRINTF".to_string(),
            "/tmp/x".to_string()
        ]));
        assert!(find_has_dangerous_predicate(&["-delete;".to_string()]));
    }

    #[test]
    fn test_find_read_only_predicates_stay_safe() {
        // These filter/print to stdout only; no filesystem write, so they
        // must not be swept up by the dangerous-predicate check.
        assert!(is_likely_safe("find . -newer /etc/passwd"));
        assert!(is_likely_safe("find . -printf '%h\\n'"));
        assert!(is_likely_safe("find . -files0-from /etc/passwd"));
        assert!(!is_likely_safe("find . -delete"));
    }

    // ========================================================================
    // Security regressions: `tee`/`xargs` are not safe as bare programs
    // ========================================================================
    //
    // Both were previously in SAFE_COMMANDS with zero args-awareness. Gate 1
    // (tools::bash::current_requires_approval) happens to omit both from its
    // own allowlist today, which is why the end-to-end AND-combined system
    // was not exploitable via these two -- but that made bash_analyzer's own
    // classification a silent single point of failure. These regressions
    // ensure this layer is sound on its own.

    #[test]
    fn test_tee_and_xargs_require_approval() {
        for cmd in [
            "tee /tmp/out",
            "xargs rm -rf",
            "echo hi | tee /etc/cron.d/backdoor",
        ] {
            let analysis = analyze_bash_command(cmd);
            assert_eq!(
                analysis.risk,
                CommandRisk::RequiresApproval,
                "expected approval for: {cmd}"
            );
        }
    }
}
