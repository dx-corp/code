//! # Shell command classification
//!
//! Deixic Code recognizes common developer tools separately from deciding whether
//! an invocation can run without approval. The built-in catalog in
//! `command_catalog_data.rs` is embedded in the native binary; a workspace copy
//! cannot grant itself permission.
//!
//! The catalog covers 150 executable names, including GitHub CLI, Python versions,
//! other interpreters, build and package tools, containers, cloud CLIs, and file,
//! JSON, and process inspection tools. A known tool with unreviewed arguments gets
//! an explanation of its possible effects instead of “Unknown command”.
//!
//! Reviewed read-only forms include GitHub pull request, issue, repository, release,
//! workflow, run, and search queries; Python version/help; and argument-checked `rg`, `fd`, `jq`, and system inspection.
//! The same catalog is used by the Bash tool's approval check and the action
//! firewall's shell analyzer, including pipelines.
//!
//! Recognition is not a blanket allowlist. GitHub mutations, authentication, API
//! requests, extensions, browser launches, and downloads remain reviewable.
//! Python scripts, modules, stdin, and inline code also remain reviewable because
//! recognizing the interpreter does not establish the program's effects. Unknown
//! flags, shell expansion, wrappers, background jobs at the tool boundary, and
//! output redirection retain approval. High-risk pattern checks and sandbox policy
//! remain in force. GitHub reads still obey `MAESTRO_NO_EGRESS_SHELL=1`.
//!
//! An exact probe matches the entire argument vector. For example,
//! `python3 --version` is a version query; `python3 script.py --version` executes a
//! script and does not inherit that permission. Empty quoted arguments are retained
//! so they cannot change the command the classifier sees. Double-quoted backslashes
//! are preserved according to Bash semantics: `python3 "\--version"` names a script,
//! not a version probe. Build and package tools stay reviewable even for version
//! checks because startup can load repository hooks or download toolchains.
//!
//! The argument semantics follow the [GitHub CLI manual](https://cli.github.com/manual/)
//! and [Python command-line documentation](https://docs.python.org/3/using/cmdline.html).
//! New command knowledge is added to the catalog with safe and concerning examples
//! in `safety/bash_analyzer.rs` and the Bash tool boundary tests in
//! `tools/bash/command_catalog_tests.rs`.

use std::sync::LazyLock;

use serde::Deserialize;

use super::bash_analyzer::{CommandRisk, ParsedCommand, tokenize};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Catalog {
    commands: Vec<KnownCommand>,
    reads: Vec<ReadCommand>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KnownCommand {
    names: Vec<String>,
    reason: String,
    #[serde(default)]
    probes: Vec<Vec<String>>,
    #[serde(default)]
    global_value_flags: Vec<String>,
    #[serde(default)]
    network: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadCommand {
    programs: Vec<String>,
    prefixes: Vec<Vec<String>>,
    #[serde(default)]
    flags: Vec<String>,
    #[serde(default)]
    value_flags: Vec<String>,
    #[serde(default)]
    max_positionals: usize,
}

static CATALOG: LazyLock<Option<Catalog>> = LazyLock::new(|| {
    // A malformed built-in catalog grants no new permissions. Tests validate it.
    toml::from_str(super::command_catalog_data::DEFINITION).ok()
});

fn definition(program: &str) -> Option<&'static KnownCommand> {
    CATALOG
        .as_ref()?
        .commands
        .iter()
        .find(|entry| entry.names.iter().any(|name| name == program))
}

/// Return a known command's risk and explanation, without bypassing shell,
/// dangerous-pattern, enterprise policy, or sandbox checks.
pub(crate) fn classify_known_command(cmd: &ParsedCommand) -> Option<(CommandRisk, &'static str)> {
    let entry = definition(&cmd.program)?;
    // Wrappers such as nohup have their own side effects. Shell expansion can
    // introduce flags/subcommands that are absent from the parsed arguments.
    if tokenize(&cmd.raw).first() != Some(&cmd.program) || !has_literal_arguments(&cmd.raw) {
        return Some((
            CommandRisk::RequiresApproval,
            "Command uses a wrapper or shell expansion",
        ));
    }
    if entry.probes.contains(&cmd.args) {
        return Some((CommandRisk::Safe, "Version or help query"));
    }
    if CATALOG
        .as_ref()?
        .reads
        .iter()
        .any(|read| read.programs.contains(&cmd.program) && matches_read(read, entry, &cmd.args))
    {
        return Some((CommandRisk::Safe, "Known read-only command"));
    }
    Some((CommandRisk::RequiresApproval, entry.reason.as_str()))
}

pub(crate) fn known_command_uses_network(cmd: &ParsedCommand) -> bool {
    definition(&cmd.program).is_some_and(|entry| entry.network && !entry.probes.contains(&cmd.args))
}

fn matches_read(read: &ReadCommand, entry: &KnownCommand, args: &[String]) -> bool {
    let mut offset = 0;
    while args.get(offset).is_some_and(|arg| arg.starts_with('-')) {
        let Some(consumed) = consume_value_flag(&args[offset..], &entry.global_value_flags) else {
            break;
        };
        offset += consumed;
    }
    read.prefixes.iter().any(|prefix| {
        let args = &args[offset..];
        if !args.starts_with(prefix) {
            return false;
        }
        let mut args = &args[prefix.len()..];
        let mut positionals = 0;
        while let Some(arg) = args.first() {
            if arg.is_empty() {
                return false;
            }
            if read.flags.contains(arg) {
                args = &args[1..];
            } else if let Some(consumed) = consume_value_flag(args, &read.value_flags)
                .or_else(|| consume_value_flag(args, &entry.global_value_flags))
            {
                args = &args[consumed..];
            } else if arg.starts_with('-') {
                // Unknown flags, option terminators and bundles need review.
                return false;
            } else {
                positionals += 1;
                if positionals > read.max_positionals {
                    return false;
                }
                args = &args[1..];
            }
        }
        true
    })
}

fn consume_value_flag(args: &[String], flags: &[String]) -> Option<usize> {
    let arg = args.first()?;
    for flag in flags {
        if arg == flag {
            return args
                .get(1)
                .filter(|value| !value.is_empty() && !value.starts_with('-'))
                .map(|_| 2);
        }
        if let Some(value) = arg.strip_prefix(&format!("{flag}=")) {
            return (!value.is_empty() && !value.starts_with('-')).then_some(1);
        }
        if flag.len() == 2 && flag.starts_with('-') {
            if let Some(value) = arg.strip_prefix(flag) {
                return (!value.is_empty() && !value.starts_with('-')).then_some(1);
            }
        }
    }
    None
}

fn has_literal_arguments(raw: &str) -> bool {
    let mut single = false;
    let mut double = false;
    let mut escaped = false;
    for ch in raw.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if !single => escaped = true,
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            '$' | '`' if !single => return false,
            '*' | '?' | '[' | '{' | '~' if !single && !double => return false,
            _ => {}
        }
    }
    !single && !double && !escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_catalog_is_valid_and_unambiguous() {
        let catalog = CATALOG.as_ref().expect("valid embedded command catalog");
        let mut names = std::collections::HashSet::new();
        for entry in &catalog.commands {
            assert!(!entry.names.is_empty());
            assert!(!entry.reason.is_empty());
            for name in &entry.names {
                assert!(names.insert(name), "duplicate command: {name}");
            }
            assert!(entry.probes.iter().all(|probe| !probe.is_empty()));
        }
        for read in &catalog.reads {
            assert!(!read.prefixes.is_empty());
            for program in &read.programs {
                assert!(
                    names.contains(program),
                    "read rule for unknown command: {program}"
                );
            }
        }
    }
}
