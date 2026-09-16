//! Custom Prompts System - User-definable slash commands from Markdown files.
//!
//! Prompts are discovered from:
//! - ~/.composer/prompts/*.md (user prompts)
//! - .composer/prompts/*.md (project prompts)
//!
//! Each prompt is a Markdown file with optional YAML frontmatter:
//!
//! ```markdown
//! ---
//! description: Request a concise git diff review
//! argument-hint: FILE=<path> [FOCUS=<section>]
//! ---
//!
//! Review the code in $FILE. Pay special attention to $FOCUS.
//! ```
//!
//! Placeholders:
//! - Positional: $1, $2, ..., $9 (from space-separated args)
//! - $ARGUMENTS: All positional arguments joined by space
//! - Named: $FILE, $TICKET_ID (from KEY=value pairs)
//! - Escape: $$ produces a literal $

use crate::path_utils::{legacy_composer_home_dir, maestro_home_dir};
use regex::Regex;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// A prompt definition loaded from a markdown file.
#[derive(Debug, Clone)]
pub struct PromptDefinition {
    /// Unique prompt name (derived from filename)
    pub name: String,
    /// Short description shown in slash popup
    pub description: Option<String>,
    /// Hint for expected arguments
    pub argument_hint: Option<String>,
    /// Full markdown body (content after frontmatter)
    pub body: String,
    /// Source file path
    pub source_path: PathBuf,
    /// Source type: "user" or "project"
    pub source_type: PromptSource,
    /// Named placeholders found in the body
    pub named_placeholders: Vec<String>,
    /// Whether body uses positional placeholders
    pub has_positional_placeholders: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptSource {
    User,
    Plugin,
    Project,
}

impl PromptSource {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Plugin => "plugin",
            Self::Project => "project",
        }
    }
}

/// Parsed arguments from a prompt invocation.
#[derive(Debug, Clone, Default)]
pub struct ParsedArgs {
    /// Positional arguments ($1, $2, etc.)
    pub positional: Vec<String>,
    /// Named arguments (KEY=value)
    pub named: HashMap<String, String>,
}

static NAMED_PLACEHOLDER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\$([A-Z][A-Z0-9_]*)").unwrap());
static POSITIONAL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\$[1-9]|\$ARGUMENTS").unwrap());
/// Regex for parsing named arguments (KEY=value) from argument strings
static NAMED_ARG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([A-Z][A-Z0-9_]*)=(.*)$").unwrap());

/// Parse YAML frontmatter from markdown content.
fn parse_frontmatter(content: &str) -> (HashMap<String, String>, &str) {
    let mut frontmatter = HashMap::new();

    if !content.starts_with("---") {
        return (frontmatter, content);
    }

    let lines: Vec<&str> = content.lines().collect();
    if lines.len() < 2 {
        return (frontmatter, content);
    }

    let mut end_idx = None;
    for (i, line) in lines.iter().enumerate().skip(1) {
        if line.trim() == "---" {
            end_idx = Some(i);
            break;
        }
    }

    let Some(end_idx) = end_idx else {
        return (frontmatter, content);
    };

    // Parse simple key: value pairs
    for line in &lines[1..end_idx] {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(colon_idx) = line.find(':') {
            let key = line[..colon_idx].trim().to_string();
            let mut value = line[colon_idx + 1..].trim().to_string();
            // Remove surrounding quotes
            if (value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\''))
            {
                value = value[1..value.len() - 1].to_string();
            }
            frontmatter.insert(key, value);
        }
    }

    // Body is everything after the closing ---
    let body_start = lines[..=end_idx].iter().map(|l| l.len() + 1).sum::<usize>();
    let body = if body_start < content.len() {
        &content[body_start..]
    } else {
        ""
    };

    (frontmatter, body)
}

/// Extract named placeholders from prompt body.
fn extract_named_placeholders(body: &str) -> Vec<String> {
    let mut placeholders: Vec<String> = NAMED_PLACEHOLDER_RE
        .captures_iter(body)
        .filter_map(|cap| {
            let name = cap.get(1)?.as_str();
            if name == "ARGUMENTS" {
                None
            } else {
                Some(name.to_string())
            }
        })
        .collect();
    placeholders.sort();
    placeholders.dedup();
    placeholders
}

/// Check if body uses positional placeholders.
fn has_positional_placeholders(body: &str) -> bool {
    POSITIONAL_RE.is_match(body)
}

/// Load a single prompt from a markdown file.
fn load_prompt_from_file(path: &Path, source_type: PromptSource) -> Option<PromptDefinition> {
    let content = fs::read_to_string(path).ok()?;
    let name = path.file_stem()?.to_string_lossy().to_string();

    if name.starts_with('.') {
        return None;
    }

    let (frontmatter, body) = parse_frontmatter(&content);
    let body = body.trim().to_string();

    Some(PromptDefinition {
        name,
        description: frontmatter.get("description").cloned(),
        argument_hint: frontmatter
            .get("argument-hint")
            .or_else(|| frontmatter.get("argument_hint"))
            .cloned(),
        named_placeholders: extract_named_placeholders(&body),
        has_positional_placeholders: has_positional_placeholders(&body),
        body,
        source_path: path.to_path_buf(),
        source_type,
    })
}

/// Scan a directory for prompt markdown files.
fn scan_prompts_directory(dir: &Path, source_type: PromptSource) -> Vec<PromptDefinition> {
    let mut prompts = Vec::new();

    if !dir.exists() {
        return prompts;
    }

    if dir.is_file() {
        if dir.extension().is_some_and(|extension| extension == "md") {
            if let Some(prompt) = load_prompt_from_file(dir, source_type) {
                prompts.push(prompt);
            }
        }
        return prompts;
    }

    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "md") {
                if let Some(prompt) = load_prompt_from_file(&path, source_type) {
                    prompts.push(prompt);
                }
            }
        }
    }

    prompts
}

/// Load all available prompts from user and project directories.
///
/// Later paths override earlier ones by name (project maestro wins).
#[must_use]
pub fn load_prompts(workspace_dir: &Path) -> Vec<PromptDefinition> {
    load_prompts_with_plugin_dirs(workspace_dir, &[])
}

/// Load prompts while including trusted plugin command directories.
#[must_use]
pub fn load_prompts_with_plugin_dirs(
    workspace_dir: &Path,
    plugin_dirs: &[PathBuf],
) -> Vec<PromptDefinition> {
    let mut dirs: Vec<(PathBuf, PromptSource)> = Vec::new();

    if let Some(home) = legacy_composer_home_dir() {
        dirs.push((home.join("commands"), PromptSource::User));
        dirs.push((home.join("prompts"), PromptSource::User));
    }
    if let Some(home) = maestro_home_dir() {
        dirs.push((home.join("commands"), PromptSource::User));
        dirs.push((home.join("prompts"), PromptSource::User));
    }
    dirs.extend(
        plugin_dirs
            .iter()
            .cloned()
            .map(|path| (path, PromptSource::Plugin)),
    );

    dirs.push((
        workspace_dir.join(".composer").join("commands"),
        PromptSource::Project,
    ));
    dirs.push((
        workspace_dir.join(".composer").join("prompts"),
        PromptSource::Project,
    ));
    dirs.push((
        workspace_dir.join(".agents").join("commands"),
        PromptSource::Project,
    ));
    dirs.push((
        workspace_dir.join(".maestro").join("commands"),
        PromptSource::Project,
    ));
    dirs.push((
        workspace_dir.join(".maestro").join("prompts"),
        PromptSource::Project,
    ));

    let mut prompt_map: HashMap<String, PromptDefinition> = HashMap::new();
    for (dir, source) in dirs {
        for prompt in scan_prompts_directory(&dir, source) {
            prompt_map.insert(prompt.name.to_lowercase(), prompt);
        }
    }

    let mut prompts: Vec<_> = prompt_map.into_values().collect();
    prompts.sort_by(|a, b| a.name.cmp(&b.name));
    prompts
}

/// Format a prompt/command template for slash invocation.
pub fn format_prompt_invoke(prompt: &PromptDefinition, raw_args: &str) -> Result<String, String> {
    let args = parse_args(raw_args);
    if prompt.has_positional_placeholders || !prompt.named_placeholders.is_empty() {
        validate_args(prompt, &args)?;
        return Ok(render_prompt(prompt, &args));
    }
    let body = prompt.body.trim();
    let args_trim = raw_args.trim();
    if args_trim.is_empty() {
        Ok(body.to_string())
    } else if body.is_empty() {
        Ok(args_trim.to_string())
    } else {
        Ok(format!("{body}\n\n---\n\n{args_trim}"))
    }
}

/// Find a prompt by name (case-insensitive).
#[must_use]
pub fn find_prompt<'a>(
    prompts: &'a [PromptDefinition],
    name: &str,
) -> Option<&'a PromptDefinition> {
    let name_lower = name.to_lowercase();
    prompts.iter().find(|p| p.name.to_lowercase() == name_lower)
}

/// Parse arguments from a command invocation.
pub fn parse_args(arg_string: &str) -> ParsedArgs {
    let mut result = ParsedArgs::default();

    if arg_string.trim().is_empty() {
        return result;
    }

    // Tokenize respecting quoted strings
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut quote_char = ' ';

    for ch in arg_string.chars() {
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

    // Separate named and positional arguments
    for token in tokens {
        if let Some(caps) = NAMED_ARG_RE.captures(&token) {
            let key = caps.get(1).unwrap().as_str().to_string();
            let value = caps.get(2).unwrap().as_str().to_string();
            result.named.insert(key, value);
        } else {
            result.positional.push(token);
        }
    }

    result
}

/// Validate that all required named placeholders are provided.
pub fn validate_args(prompt: &PromptDefinition, args: &ParsedArgs) -> Result<(), String> {
    let missing: Vec<_> = prompt
        .named_placeholders
        .iter()
        .filter(|p| !args.named.contains_key(*p))
        .collect();

    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Missing required arguments: {}",
            missing
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }
}

/// Render a prompt with the given arguments.
#[must_use]
pub fn render_prompt(prompt: &PromptDefinition, args: &ParsedArgs) -> String {
    let mut result = prompt.body.clone();

    // Escape $$ to a placeholder
    let escape_marker = "\x00DOLLAR\x00";
    result = result.replace("$$", escape_marker);

    // Substitute $ARGUMENTS
    result = result.replace("$ARGUMENTS", &args.positional.join(" "));

    // Substitute positional arguments $1-$9
    for i in 1..=9 {
        let pattern = format!("${i}");
        let value = args
            .positional
            .get(i - 1)
            .map_or("", std::string::String::as_str);
        result = result.replace(&pattern, value);
    }

    // Substitute named arguments
    for (key, value) in &args.named {
        let pattern = format!("${key}");
        result = result.replace(&pattern, value);
    }

    // Restore escaped dollars
    result = result.replace(escape_marker, "$");

    result
}

/// Format prompt for display in a list.
#[must_use]
pub fn format_prompt_list_item(prompt: &PromptDefinition) -> String {
    let source = match prompt.source_type {
        PromptSource::User => "(user)",
        PromptSource::Plugin => "(plugin)",
        PromptSource::Project => "(project)",
    };
    let desc = prompt.description.as_deref().unwrap_or("(no description)");
    format!("{} {} - {}", prompt.name, source, desc)
}

/// Get usage hint for a prompt.
#[must_use]
pub fn get_usage_hint(prompt: &PromptDefinition) -> String {
    let mut parts = vec![format!("/{}", prompt.name)];

    if let Some(hint) = &prompt.argument_hint {
        parts.push(hint.clone());
    } else if !prompt.named_placeholders.is_empty() {
        let args: Vec<_> = prompt
            .named_placeholders
            .iter()
            .map(|p| format!("{p}=<value>"))
            .collect();
        parts.push(args.join(" "));
    } else if prompt.has_positional_placeholders {
        parts.push("<args...>".to_string());
    }

    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_prompts_prefers_maestro_over_composer() {
        let temp = tempfile::TempDir::new().unwrap();
        let workspace = temp.path();
        let composer = workspace.join(".composer").join("prompts");
        let maestro = workspace.join(".maestro").join("prompts");
        let commands = workspace.join(".maestro").join("commands");
        fs::create_dir_all(&composer).unwrap();
        fs::create_dir_all(&maestro).unwrap();
        fs::create_dir_all(&commands).unwrap();
        fs::write(
            composer.join("review.md"),
            "---
description: composer
---
composer body
",
        )
        .unwrap();
        fs::write(
            maestro.join("review.md"),
            "---
description: maestro
---
maestro body
",
        )
        .unwrap();
        fs::write(
            commands.join("ship.md"),
            "---
description: ship
---
Ship $ARGUMENTS
",
        )
        .unwrap();
        let prompts = load_prompts(workspace);
        assert_eq!(
            find_prompt(&prompts, "review").unwrap().body.trim(),
            "maestro body"
        );
        assert!(find_prompt(&prompts, "ship").is_some());
    }

    #[test]
    fn loads_markdown_commands_from_plugins() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let commands = temp.path().join("plugin").join("commands");
        fs::create_dir_all(&commands).unwrap();
        fs::write(
            commands.join("plugin-review.md"),
            "---\ndescription: Review from plugin\n---\nReview $ARGUMENTS",
        )
        .unwrap();

        let prompts = load_prompts_with_plugin_dirs(&workspace, &[commands]);
        let prompt = find_prompt(&prompts, "plugin-review").expect("plugin prompt");

        assert_eq!(prompt.source_type, PromptSource::Plugin);
        assert_eq!(prompt.description.as_deref(), Some("Review from plugin"));
    }

    #[test]
    fn loads_an_exact_filtered_plugin_prompt_path() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let path = temp.path().join("plugin/commands/review.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "Review exactly this resource").unwrap();

        let prompts = load_prompts_with_plugin_dirs(&workspace, std::slice::from_ref(&path));

        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].source_path, path);
        assert_eq!(prompts[0].source_type, PromptSource::Plugin);
    }

    #[test]
    fn test_format_prompt_invoke_appends_args_without_placeholders() {
        let prompt = PromptDefinition {
            name: "commit".to_string(),
            description: None,
            argument_hint: None,
            body: "Create a conventional commit.".to_string(),
            source_path: PathBuf::new(),
            source_type: PromptSource::User,
            named_placeholders: vec![],
            has_positional_placeholders: false,
        };
        let out = format_prompt_invoke(&prompt, "fix typo").unwrap();
        assert!(out.contains("Create a conventional commit."));
        assert!(out.contains("fix typo"));
    }

    #[test]
    fn test_parse_args_positional() {
        let args = parse_args("foo bar baz");
        assert_eq!(args.positional, vec!["foo", "bar", "baz"]);
        assert!(args.named.is_empty());
    }

    #[test]
    fn test_parse_args_named() {
        let args = parse_args("FILE=main.ts FOCUS=security");
        assert!(args.positional.is_empty());
        assert_eq!(args.named.get("FILE"), Some(&"main.ts".to_string()));
        assert_eq!(args.named.get("FOCUS"), Some(&"security".to_string()));
    }

    #[test]
    fn test_parse_args_quoted() {
        let args = parse_args(r#"TITLE="Fix the bug" FILE=main.ts"#);
        assert_eq!(args.named.get("TITLE"), Some(&"Fix the bug".to_string()));
    }

    #[test]
    fn test_render_prompt_named() {
        let prompt = PromptDefinition {
            name: "test".to_string(),
            description: None,
            argument_hint: None,
            body: "Review $FILE focusing on $FOCUS".to_string(),
            source_path: PathBuf::new(),
            source_type: PromptSource::User,
            named_placeholders: vec!["FILE".to_string(), "FOCUS".to_string()],
            has_positional_placeholders: false,
        };
        let mut args = ParsedArgs::default();
        args.named.insert("FILE".to_string(), "main.ts".to_string());
        args.named
            .insert("FOCUS".to_string(), "security".to_string());
        let result = render_prompt(&prompt, &args);
        assert_eq!(result, "Review main.ts focusing on security");
    }

    #[test]
    fn test_render_prompt_positional() {
        let prompt = PromptDefinition {
            name: "test".to_string(),
            description: None,
            argument_hint: None,
            body: "Args: $1, $2, $3".to_string(),
            source_path: PathBuf::new(),
            source_type: PromptSource::User,
            named_placeholders: vec![],
            has_positional_placeholders: true,
        };
        let args = ParsedArgs {
            positional: vec!["foo".to_string(), "bar".to_string(), "baz".to_string()],
            named: HashMap::new(),
        };
        let result = render_prompt(&prompt, &args);
        assert_eq!(result, "Args: foo, bar, baz");
    }

    #[test]
    fn test_render_prompt_escape() {
        let prompt = PromptDefinition {
            name: "test".to_string(),
            description: None,
            argument_hint: None,
            body: "Cost is $$100".to_string(),
            source_path: PathBuf::new(),
            source_type: PromptSource::User,
            named_placeholders: vec![],
            has_positional_placeholders: false,
        };
        let result = render_prompt(&prompt, &ParsedArgs::default());
        assert_eq!(result, "Cost is $100");
    }
}
