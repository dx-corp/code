//! Native `maestro import-claude` command.
//!
//! Best-effort import of Claude Code configuration into maestro:
//!
//! - MCP server definitions from `~/.claude.json`, `<project>/.mcp.json`, and
//!   `<project>/.claude.json` (top-level `mcpServers` objects) are merged into
//!   `[mcp_servers]` in `~/.composer/config.toml`.
//! - `permissions.allow` / `permissions.deny` from `~/.claude/settings.json`
//!   and `<project>/.claude/settings.json` are translated into `prefix_rule`
//!   entries in `~/.composer/execpolicy`.
//!
//! Import semantics:
//! - Writes by default; `--dry-run` reports without touching any file.
//! - Existing entries are never overwritten: identical entries are skipped as
//!   duplicates, entries with different values are skipped as conflicts.
//! - Anything that cannot be mapped faithfully (non-Bash tools, partial-token
//!   wildcards, compound commands) is reported as skipped, not approximated.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value as JsonValue;

use crate::execpolicy::{self, Decision};
use crate::skill_cli::write_atomic;

/// Inputs for an import run. Paths are explicit so tests can use tempdirs.
#[derive(Debug, Clone)]
pub struct ImportOptions {
    /// User home directory (holds `.claude.json`, `.claude/`, `.composer/`).
    pub home_dir: PathBuf,
    /// Project directory (holds `.mcp.json`, `.claude/settings.json`).
    pub project_dir: PathBuf,
    /// When true, compute the report without writing any files.
    pub dry_run: bool,
}

/// What happened to a single candidate entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedEntry {
    pub name: String,
    pub reason: String,
}

/// Outcome of an import run.
#[derive(Debug, Default)]
pub struct ImportReport {
    pub mcp_imported: Vec<String>,
    pub mcp_duplicates: Vec<String>,
    pub mcp_conflicts: Vec<SkippedEntry>,
    pub mcp_skipped: Vec<SkippedEntry>,
    pub rules_imported: Vec<String>,
    pub rules_duplicates: Vec<String>,
    pub rules_conflicts: Vec<SkippedEntry>,
    pub rules_skipped: Vec<SkippedEntry>,
    pub warnings: Vec<String>,
    pub config_path: PathBuf,
    pub execpolicy_path: PathBuf,
}

/// Run `maestro import-claude [--dry-run]`.
pub fn run_import_claude(args: &[String]) -> Result<i32> {
    let mut dry_run = false;
    for arg in args {
        match arg.as_str() {
            "--dry-run" | "-n" => dry_run = true,
            "help" | "--help" | "-h" => {
                println!(
                    "{}",
                    crate::localization::cli_locale()
                        .format("Usage: maestro import-claude [--dry-run]", &[])
                );
                println!();
                println!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "Import Claude Code configuration (MCP servers and permission rules)",
                        &[]
                    )
                );
                println!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "into maestro's ~/.composer/config.toml and ~/.composer/execpolicy.",
                        &[]
                    )
                );
                return Ok(0);
            }
            other => anyhow::bail!("unknown import-claude flag: {other}"),
        }
    }

    let home_dir = dirs::home_dir().context("could not determine home directory")?;
    let project_dir = std::env::current_dir().context("could not determine current directory")?;
    let options = ImportOptions {
        home_dir,
        project_dir,
        dry_run,
    };
    let report = import_claude(&options)?;
    print_report(&report, dry_run);
    Ok(0)
}

fn print_report(report: &ImportReport, dry_run: bool) {
    if dry_run {
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "Import Claude Code configuration (dry run — no files changed)",
                &[]
            )
        );
    } else {
        println!(
            "{}",
            crate::localization::cli_locale().format("Import Claude Code configuration", &[])
        );
    }
    println!();
    println!(
        "{}",
        crate::localization::cli_locale().format(
            "MCP servers: {0} imported, {1} already present, {2} skipped",
            &[
                (report.mcp_imported.len()).to_string(),
                (report.mcp_duplicates.len()).to_string(),
                (report.mcp_conflicts.len() + report.mcp_skipped.len()).to_string()
            ]
        )
    );
    for name in &report.mcp_imported {
        println!("  + {name}");
    }
    for name in &report.mcp_duplicates {
        println!(
            "{}",
            crate::localization::cli_locale()
                .format("  = {0} (already present)", std::slice::from_ref(name))
        );
    }
    for entry in report.mcp_conflicts.iter().chain(&report.mcp_skipped) {
        println!("  ! {} ({})", entry.name, entry.reason);
    }
    println!();
    println!(
        "{}",
        crate::localization::cli_locale().format(
            "Permission rules: {0} imported, {1} already present, {2} skipped",
            &[
                (report.rules_imported.len()).to_string(),
                (report.rules_duplicates.len()).to_string(),
                (report.rules_conflicts.len() + report.rules_skipped.len()).to_string()
            ]
        )
    );
    for rule in &report.rules_imported {
        println!("  + {rule}");
    }
    for rule in &report.rules_duplicates {
        println!(
            "{}",
            crate::localization::cli_locale()
                .format("  = {0} (already present)", std::slice::from_ref(rule))
        );
    }
    for entry in report.rules_conflicts.iter().chain(&report.rules_skipped) {
        println!("  ! {} ({})", entry.name, entry.reason);
    }
    if !report.warnings.is_empty() {
        println!();
        println!(
            "{}",
            crate::localization::cli_locale().format("Warnings:", &[])
        );
        for warning in &report.warnings {
            println!("  - {warning}");
        }
    }
    println!();
    if dry_run {
        println!(
            "{}",
            crate::localization::cli_locale().format("Dry run: no files changed.", &[])
        );
    } else if report.mcp_imported.is_empty() && report.rules_imported.is_empty() {
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "Nothing to write; all entries already present or skipped.",
                &[]
            )
        );
    } else {
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "Updated {0} and {1}.",
                &[
                    (report.config_path.display()).to_string(),
                    (report.execpolicy_path.display()).to_string()
                ]
            )
        );
    }
}

/// Execute the import and produce a report.
pub fn import_claude(options: &ImportOptions) -> Result<ImportReport> {
    let composer_dir = options.home_dir.join(".composer");
    let config_path = composer_dir.join("config.toml");
    let execpolicy_path = composer_dir.join("execpolicy");

    let mut report = ImportReport {
        config_path: config_path.clone(),
        execpolicy_path: execpolicy_path.clone(),
        ..ImportReport::default()
    };

    let mcp_sources = [
        options.home_dir.join(".claude.json"),
        options.project_dir.join(".mcp.json"),
        options.project_dir.join(".claude.json"),
    ];
    let settings_sources = [
        options.home_dir.join(".claude").join("settings.json"),
        options.project_dir.join(".claude").join("settings.json"),
    ];

    import_mcp_servers(&mcp_sources, &config_path, options.dry_run, &mut report)?;
    import_permissions(
        &settings_sources,
        &execpolicy_path,
        options.dry_run,
        &mut report,
    )?;
    Ok(report)
}

// ─────────────────────────────────────────────────────────────
// MCP server import
// ─────────────────────────────────────────────────────────────

fn import_mcp_servers(
    sources: &[PathBuf],
    config_path: &Path,
    dry_run: bool,
    report: &mut ImportReport,
) -> Result<()> {
    let mut candidates: BTreeMap<String, toml::Value> = BTreeMap::new();
    for source in sources {
        let Some(root) = read_json_source(source, report) else {
            continue;
        };
        let Some(servers) = root.get("mcpServers").and_then(JsonValue::as_object) else {
            continue;
        };
        for (name, def) in servers {
            match map_claude_mcp_server(def) {
                Ok(table) => {
                    // First source wins for duplicate names across sources.
                    candidates.entry(name.clone()).or_insert(table);
                }
                Err(reason) => report.mcp_skipped.push(SkippedEntry {
                    name: name.clone(),
                    reason,
                }),
            }
        }
    }

    let mut root = read_toml_config(config_path)?;
    let mcp_table = root
        .as_table_mut()
        .context("config.toml root is not a table")?
        .entry("mcp_servers")
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    let mcp_table = mcp_table
        .as_table_mut()
        .context("mcp_servers in config.toml is not a table")?;

    let mut changed = false;
    for (name, table) in candidates {
        match mcp_table.get(&name) {
            None => {
                mcp_table.insert(name.clone(), table);
                report.mcp_imported.push(name);
                changed = true;
            }
            Some(existing) if *existing == table => report.mcp_duplicates.push(name),
            Some(_) => report.mcp_conflicts.push(SkippedEntry {
                name,
                reason: "conflicts with existing mcp_servers entry (not overwritten)".to_string(),
            }),
        }
    }

    if changed && !dry_run {
        let rendered = toml::to_string_pretty(&root).context("failed to serialize config.toml")?;
        write_atomic(config_path, &rendered)?;
    }
    Ok(())
}

fn read_toml_config(config_path: &Path) -> Result<toml::Value> {
    match fs::read_to_string(config_path) {
        Ok(content) => {
            toml::from_str(&content).with_context(|| format!("parse {}", config_path.display()))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Ok(toml::Value::Table(toml::map::Map::new()))
        }
        Err(err) => Err(err).with_context(|| format!("read {}", config_path.display())),
    }
}

/// Translate a Claude Code MCP server definition into a maestro
/// `[mcp_servers.<name>]` TOML table.
fn map_claude_mcp_server(def: &JsonValue) -> Result<toml::Value, String> {
    let obj = def
        .as_object()
        .ok_or_else(|| "server definition is not an object".to_string())?;

    let mut table = toml::map::Map::new();
    if let Some(command) = obj.get("command").and_then(JsonValue::as_str) {
        table.insert(
            "command".to_string(),
            toml::Value::String(command.to_string()),
        );
        if let Some(args) = obj.get("args").and_then(JsonValue::as_array) {
            let args: Vec<String> = args
                .iter()
                .filter_map(JsonValue::as_str)
                .map(str::to_string)
                .collect();
            if !args.is_empty() {
                table.insert(
                    "args".to_string(),
                    toml::Value::Array(args.into_iter().map(toml::Value::String).collect()),
                );
            }
        }
        if let Some(env) = obj.get("env").and_then(JsonValue::as_object) {
            let env = string_map_to_toml(env);
            if !env.is_empty() {
                table.insert("env".to_string(), toml::Value::Table(env));
            }
        }
        if let Some(cwd) = obj.get("cwd").and_then(JsonValue::as_str) {
            table.insert("cwd".to_string(), toml::Value::String(cwd.to_string()));
        }
        return Ok(toml::Value::Table(table));
    }

    if let Some(url) = obj.get("url").and_then(JsonValue::as_str) {
        table.insert("url".to_string(), toml::Value::String(url.to_string()));
        if let Some(headers) = obj.get("headers").and_then(JsonValue::as_object) {
            let headers = string_map_to_toml(headers);
            if !headers.is_empty() {
                table.insert("http_headers".to_string(), toml::Value::Table(headers));
            }
        }
        return Ok(toml::Value::Table(table));
    }

    let transport = obj
        .get("type")
        .and_then(JsonValue::as_str)
        .unwrap_or("unknown");
    Err(format!(
        "no command or url in server definition (type={transport})"
    ))
}

fn string_map_to_toml(
    map: &serde_json::Map<String, JsonValue>,
) -> toml::map::Map<String, toml::Value> {
    map.iter()
        .filter_map(|(key, value)| {
            value
                .as_str()
                .map(|value| (key.clone(), toml::Value::String(value.to_string())))
        })
        .collect()
}

// ─────────────────────────────────────────────────────────────
// Permission rule import
// ─────────────────────────────────────────────────────────────

fn import_permissions(
    sources: &[PathBuf],
    execpolicy_path: &Path,
    dry_run: bool,
    report: &mut ImportReport,
) -> Result<()> {
    let mut candidates: Vec<(String, Vec<String>, Decision)> = Vec::new();
    for source in sources {
        let Some(root) = read_json_source(source, report) else {
            continue;
        };
        let Some(permissions) = root.get("permissions").and_then(JsonValue::as_object) else {
            continue;
        };
        for (list, decision) in [("allow", Decision::Allow), ("deny", Decision::Forbidden)] {
            let Some(entries) = permissions.get(list).and_then(JsonValue::as_array) else {
                continue;
            };
            for entry in entries {
                let Some(rule) = entry.as_str() else {
                    report.rules_skipped.push(SkippedEntry {
                        name: format!("{list}: <non-string entry>"),
                        reason: "permission entry is not a string".to_string(),
                    });
                    continue;
                };
                match map_claude_permission(rule) {
                    Ok(tokens) => candidates.push((rule.to_string(), tokens, decision)),
                    Err(reason) => report.rules_skipped.push(SkippedEntry {
                        name: rule.to_string(),
                        reason,
                    }),
                }
            }
        }
    }

    let existing_content = match fs::read_to_string(execpolicy_path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err).with_context(|| format!("read {}", execpolicy_path.display())),
    };
    let existing_policy = execpolicy::parse_policy(&existing_content, "existing execpolicy")
        .map_err(|err| anyhow::anyhow!(err))?;

    let mut pending: Vec<(Vec<String>, Decision)> = Vec::new();
    for (source_rule, tokens, decision) in candidates {
        let rendered = execpolicy::render_prefix_rule(&tokens, decision);
        match existing_decision(&existing_policy, &tokens) {
            Some(existing) if existing == decision => report.rules_duplicates.push(rendered),
            Some(existing) => report.rules_conflicts.push(SkippedEntry {
                name: source_rule,
                reason: format!(
                    "conflicts with existing execpolicy rule (decision={}); not overwritten",
                    existing.as_str()
                ),
            }),
            None => {
                report.rules_imported.push(rendered);
                pending.push((tokens, decision));
            }
        }
    }

    if !dry_run {
        for (tokens, decision) in pending {
            execpolicy::append_prefix_rule(execpolicy_path, &tokens, decision)
                .map_err(|err| anyhow::anyhow!(err))?;
        }
    }
    Ok(())
}

/// Look up an exact token-for-token prefix pattern in an existing policy and
/// return its decision when present.
fn existing_decision(policy: &execpolicy::Policy, tokens: &[String]) -> Option<Decision> {
    let first = tokens.first()?;
    let rules = policy.rules().get(first)?;
    for rule in rules {
        if rule.pattern.rest.len() + 1 != tokens.len() {
            continue;
        }
        // Only single-token patterns compare equal to a plain token list;
        // alternatives never match and are treated as distinct rules.
        let matches = rule
            .pattern
            .rest
            .iter()
            .zip(&tokens[1..])
            .all(|(token, want)| {
                matches!(token, execpolicy::PatternToken::Single(value) if value == want)
            });
        if matches {
            return Some(rule.decision);
        }
    }
    None
}

/// Translate one Claude Code permission rule (e.g. `Bash(npm run *)`) into
/// execpolicy prefix tokens. Returns `Err(reason)` when no faithful mapping
/// exists.
fn map_claude_permission(rule: &str) -> Result<Vec<String>, String> {
    let rule = rule.trim();
    let Some(open) = rule.find('(') else {
        return Err(format!(
            "bare tool rule `{rule}` has no execpolicy equivalent (only Bash command rules can be mapped)"
        ));
    };
    let tool = rule[..open].trim();
    let spec = rule[open + 1..]
        .strip_suffix(')')
        .ok_or_else(|| format!("malformed rule `{rule}` (missing closing parenthesis)"))?
        .trim();

    if tool != "Bash" {
        return Err(format!(
            "tool `{tool}` rules have no execpolicy equivalent (execpolicy only governs shell commands)"
        ));
    }
    if spec.is_empty() {
        return Err(
            "`Bash()` matches every command; execpolicy prefix rules cannot express that"
                .to_string(),
        );
    }

    let tokens = execpolicy::parse_command(spec);
    if tokens.is_empty() {
        return Err(format!("rule `{rule}` has no command tokens"));
    }

    const SHELL_OPERATORS: &[&str] = &["&&", "||", ";", "|", ">", "<", ">>", "2>"];
    if tokens
        .iter()
        .any(|token| SHELL_OPERATORS.contains(&token.as_str()))
    {
        return Err(format!(
            "compound command `{spec}` cannot be expressed as a single prefix rule"
        ));
    }

    let wildcard = |token: &str| token.contains('*') || token.contains('?') || token.contains('[');
    match tokens.iter().position(|token| wildcard(token)) {
        None => Ok(tokens),
        Some(index) if tokens[index] == "*" && index == tokens.len() - 1 => {
            let prefix: Vec<String> = tokens[..index].to_vec();
            if prefix.is_empty() {
                Err(
                    "`Bash(*)` matches every command; execpolicy prefix rules cannot express that"
                        .to_string(),
                )
            } else {
                Ok(prefix)
            }
        }
        Some(index) => Err(format!(
            "wildcard token `{}` cannot be expressed as an execpolicy prefix rule (only a trailing `*` wildcard is supported)",
            tokens[index]
        )),
    }
}

// ─────────────────────────────────────────────────────────────
// Shared helpers
// ─────────────────────────────────────────────────────────────

fn read_json_source(path: &Path, report: &mut ImportReport) -> Option<JsonValue> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            report
                .warnings
                .push(format!("failed to read {}: {err}", path.display()));
            return None;
        }
    };
    match serde_json::from_str(&content) {
        Ok(value) => Some(value),
        Err(err) => {
            report
                .warnings
                .push(format!("failed to parse {}: {err}", path.display()));
            None
        }
    }
}

// ─────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _home: tempfile::TempDir,
        _project: tempfile::TempDir,
        options: ImportOptions,
    }

    fn fixture() -> Fixture {
        let home = tempfile::tempdir().expect("home tempdir");
        let project = tempfile::tempdir().expect("project tempdir");
        Fixture {
            options: ImportOptions {
                home_dir: home.path().to_path_buf(),
                project_dir: project.path().to_path_buf(),
                dry_run: false,
            },
            _home: home,
            _project: project,
        }
    }

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().expect("parent dir")).expect("mkdir");
        fs::write(path, content).expect("write fixture");
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).expect("read result")
    }

    // ── MCP mapping ──────────────────────────────────────────

    #[test]
    fn maps_stdio_server() {
        let def = serde_json::json!({
            "command": "npx",
            "args": ["-y", "@upstash/context7-mcp"],
            "env": {"API_KEY": "abc"}
        });
        let table = map_claude_mcp_server(&def).expect("stdio mapping");
        assert_eq!(table["command"], toml::Value::String("npx".to_string()));
        assert_eq!(
            table["args"],
            toml::Value::Array(vec![
                toml::Value::String("-y".to_string()),
                toml::Value::String("@upstash/context7-mcp".to_string())
            ])
        );
        assert_eq!(
            table["env"]["API_KEY"],
            toml::Value::String("abc".to_string())
        );
    }

    #[test]
    fn maps_http_server_with_headers() {
        let def = serde_json::json!({
            "type": "http",
            "url": "https://mcp.example.com/rpc",
            "headers": {"Authorization": "Bearer tok"}
        });
        let table = map_claude_mcp_server(&def).expect("http mapping");
        assert_eq!(
            table["url"],
            toml::Value::String("https://mcp.example.com/rpc".to_string())
        );
        assert_eq!(
            table["http_headers"]["Authorization"],
            toml::Value::String("Bearer tok".to_string())
        );
        assert!(table.get("command").is_none());
    }

    #[test]
    fn rejects_server_without_command_or_url() {
        let def = serde_json::json!({"type": "sse"});
        assert!(map_claude_mcp_server(&def).is_err());
    }

    // ── Permission mapping ───────────────────────────────────

    #[test]
    fn maps_trailing_wildcard_to_prefix() {
        assert_eq!(
            map_claude_permission("Bash(npm run *)").expect("wildcard mapping"),
            vec!["npm".to_string(), "run".to_string()]
        );
    }

    #[test]
    fn maps_exact_command_to_prefix() {
        assert_eq!(
            map_claude_permission("Bash(git status)").expect("exact mapping"),
            vec!["git".to_string(), "status".to_string()]
        );
    }

    #[test]
    fn rejects_non_bash_tools() {
        let err = map_claude_permission("Read(./src/**)").expect_err("unmappable");
        assert!(err.contains("Read"));
    }

    #[test]
    fn rejects_bare_tool_rules() {
        assert!(map_claude_permission("WebSearch").is_err());
    }

    #[test]
    fn rejects_partial_token_wildcards() {
        let err = map_claude_permission("Bash(npm run dev:*)").expect_err("partial wildcard");
        assert!(err.contains("dev:*"));
        assert!(map_claude_permission("Bash(rm * --force)").is_err());
        assert!(map_claude_permission("Bash(*)").is_err());
        assert!(map_claude_permission("Bash()").is_err());
    }

    #[test]
    fn rejects_compound_commands() {
        let err = map_claude_permission("Bash(git fetch && git reset *)").expect_err("compound");
        assert!(err.contains("compound"));
    }

    // ── End-to-end import ────────────────────────────────────

    #[test]
    fn imports_stdio_and_http_servers() {
        let fx = fixture();
        write(
            &fx.options.home_dir.join(".claude.json"),
            r#"{"mcpServers": {"context7": {"command": "npx", "args": ["-y", "@upstash/context7-mcp"]}}}"#,
        );
        write(
            &fx.options.project_dir.join(".mcp.json"),
            r#"{"mcpServers": {"remote": {"type": "http", "url": "https://mcp.example.com/rpc"}}}"#,
        );

        let report = import_claude(&fx.options).expect("import");
        assert_eq!(report.mcp_imported, vec!["context7", "remote"]);
        assert!(report.mcp_conflicts.is_empty());
        assert!(report.mcp_skipped.is_empty());

        let config = read(&report.config_path);
        let parsed: toml::Value = toml::from_str(&config).expect("valid toml");
        assert_eq!(
            parsed["mcp_servers"]["context7"]["command"],
            toml::Value::String("npx".to_string())
        );
        assert_eq!(
            parsed["mcp_servers"]["remote"]["url"],
            toml::Value::String("https://mcp.example.com/rpc".to_string())
        );
    }

    #[test]
    fn skips_duplicate_and_conflicting_servers() {
        let fx = fixture();
        write(
            &fx.options.home_dir.join(".composer").join("config.toml"),
            r#"
[mcp_servers.context7]
command = "npx"
args = ["-y", "@upstash/context7-mcp"]

[mcp_servers.other]
command = "uvx"
"#,
        );
        write(
            &fx.options.home_dir.join(".claude.json"),
            r#"{"mcpServers": {
                "context7": {"command": "npx", "args": ["-y", "@upstash/context7-mcp"]},
                "other": {"command": "different"}
            }}"#,
        );

        let report = import_claude(&fx.options).expect("import");
        assert_eq!(report.mcp_duplicates, vec!["context7"]);
        assert_eq!(report.mcp_conflicts.len(), 1);
        assert_eq!(report.mcp_conflicts[0].name, "other");
        assert!(report.mcp_imported.is_empty());

        // Existing entry must be untouched.
        let parsed: toml::Value = toml::from_str(&read(&report.config_path)).expect("valid toml");
        assert_eq!(
            parsed["mcp_servers"]["other"]["command"],
            toml::Value::String("uvx".to_string())
        );
    }

    #[test]
    fn imports_allow_and_deny_rules() {
        let fx = fixture();
        write(
            &fx.options.home_dir.join(".claude").join("settings.json"),
            r#"{"permissions": {
                "allow": ["Bash(npm run *)"],
                "deny": ["Bash(rm -rf *)"]
            }}"#,
        );

        let report = import_claude(&fx.options).expect("import");
        assert_eq!(report.rules_imported.len(), 2);
        let policy = read(&report.execpolicy_path);
        assert!(policy.contains(r#"prefix_rule(pattern=["npm", "run"], decision="allow")"#));
        assert!(policy.contains(r#"prefix_rule(pattern=["rm", "-rf"], decision="forbidden")"#));
    }

    #[test]
    fn reports_unmappable_permission_rules() {
        let fx = fixture();
        write(
            &fx.options.project_dir.join(".claude").join("settings.json"),
            r#"{"permissions": {"allow": ["Read(./src/**)", "Bash(npm test)"]}}"#,
        );

        let report = import_claude(&fx.options).expect("import");
        assert_eq!(report.rules_imported.len(), 1);
        assert_eq!(report.rules_skipped.len(), 1);
        assert_eq!(report.rules_skipped[0].name, "Read(./src/**)");
        assert!(report.rules_skipped[0].reason.contains("Read"));
    }

    #[test]
    fn skips_duplicate_and_conflicting_rules() {
        let fx = fixture();
        write(
            &fx.options.home_dir.join(".composer").join("execpolicy"),
            "prefix_rule(pattern=[\"git\", \"status\"], decision=\"allow\")\nprefix_rule(pattern=[\"cargo\", \"publish\"], decision=\"prompt\")\n",
        );
        write(
            &fx.options.home_dir.join(".claude").join("settings.json"),
            r#"{"permissions": {"allow": ["Bash(git status)", "Bash(cargo publish)"]}}"#,
        );

        let report = import_claude(&fx.options).expect("import");
        assert!(report.rules_imported.is_empty());
        assert_eq!(report.rules_duplicates.len(), 1);
        assert_eq!(report.rules_conflicts.len(), 1);
        assert_eq!(report.rules_conflicts[0].name, "Bash(cargo publish)");

        // Nothing appended.
        let policy = read(&report.execpolicy_path);
        assert_eq!(policy.matches("prefix_rule").count(), 2);
    }

    #[test]
    fn dry_run_writes_nothing() {
        let mut fx = fixture();
        write(
            &fx.options.home_dir.join(".claude.json"),
            r#"{"mcpServers": {"context7": {"command": "npx"}}}"#,
        );
        write(
            &fx.options.home_dir.join(".claude").join("settings.json"),
            r#"{"permissions": {"allow": ["Bash(npm run *)"]}}"#,
        );
        fx.options.dry_run = true;

        let report = import_claude(&fx.options).expect("dry-run import");
        assert_eq!(report.mcp_imported, vec!["context7"]);
        assert_eq!(report.rules_imported.len(), 1);
        assert!(!report.config_path.exists());
        assert!(!report.execpolicy_path.exists());
    }

    #[test]
    fn missing_sources_are_silently_skipped() {
        let fx = fixture();
        let report = import_claude(&fx.options).expect("import with no sources");
        assert!(report.mcp_imported.is_empty());
        assert!(report.rules_imported.is_empty());
        assert!(report.warnings.is_empty());
    }

    #[test]
    fn invalid_json_source_produces_warning_not_error() {
        let fx = fixture();
        write(&fx.options.home_dir.join(".claude.json"), "{not json");
        let report = import_claude(&fx.options).expect("import despite bad json");
        assert_eq!(report.warnings.len(), 1);
        assert!(report.warnings[0].contains("failed to parse"));
    }
}
