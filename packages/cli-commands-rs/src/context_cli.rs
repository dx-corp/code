//! Native `maestro context` command.
//!
//! Explains and diffs the unified prompt-context manifest (project docs + MCP
//! configuration) directly from native configuration and live MCP state.
//!
//! ## Supported
//! - `explain` / `diff` subcommands
//! - `--json` machine-readable output
//! - Project-doc loading (AGENTS.md layers, budget, diagnostics)
//! - Configured MCP servers from disk
//! - `--live-mcp`: connect configured MCP servers via [`crate::mcp::McpClient`]
//!   and surface runtime status, resources, and prompts
//! - Human summary / diff renderers
//!
//! Project MCP paths include both current `.maestro` and legacy `.composer`
//! locations, with redacted auth, helper, trust, and parallel-safety metadata.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Map, Value as JsonValue, json};
use sha2::{Digest, Sha256};
use url::Url;

use crate::config::load_config;
use crate::mcp::{
    McpClient, McpConfig, McpConfigScope, McpPrompt, McpServerConfig, McpTransport,
    append_managed_mcp_connections, load_mcp_config,
};
use crate::path_utils::{env_path, maestro_home_dir};

const PROTOCOL_VERSION: &str = "maestro.unified-context-manifest.v1";

const AGENT_CONTEXT_FILES: &[&str] = &[
    "AGENTS.override.md",
    "AGENTS.md",
    "Agents.md",
    "agents.md",
    "AGENT.md",
    "Agent.md",
    "agent.md",
    "CLAUDE.md",
];

// ─────────────────────────────────────────────────────────────
// Types
// ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PromptProjectDocManifest {
    cwd: String,
    candidates: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_bytes: Option<usize>,
    bytes_read: usize,
    entries: Vec<PromptProjectDocEntry>,
    diagnostics: Vec<ContextDiagnostic>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PromptProjectDocEntry {
    path: String,
    source_kind: String,
    scope_dir: String,
    candidate_name: String,
    bytes_read: usize,
    truncated: bool,
    content_hash: String,
    precedence_index: usize,
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    original_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_bytes: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ContextDiagnostic {
    code: String,
    severity: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    entry_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct UnifiedContextManifestEntry {
    id: String,
    kind: String,
    source: String,
    status: String,
    label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    server_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    precedence_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes_read: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<Map<String, JsonValue>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct UnifiedContextManifest {
    protocol_version: String,
    version: u32,
    #[serde(skip_serializing_if = "String::is_empty")]
    manifest_sha256: String,
    cwd: String,
    project_docs: PromptProjectDocManifest,
    entries: Vec<UnifiedContextManifestEntry>,
    diagnostics: Vec<ContextDiagnostic>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct UnifiedContextManifestDiffEntry {
    id: String,
    kind: String,
    label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    before: Option<UnifiedContextManifestEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    after: Option<UnifiedContextManifestEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    changes: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct UnifiedContextManifestDiff {
    before_cwd: String,
    after_cwd: String,
    added: Vec<UnifiedContextManifestDiffEntry>,
    removed: Vec<UnifiedContextManifestDiffEntry>,
    changed: Vec<UnifiedContextManifestDiffEntry>,
    unchanged: Vec<UnifiedContextManifestDiffEntry>,
    diagnostics: Vec<ContextDiagnostic>,
}

// ─────────────────────────────────────────────────────────────
// CLI entry
// ─────────────────────────────────────────────────────────────

pub async fn run_context(args: &[String]) -> Result<i32> {
    let mut live_mcp = false;
    let mut json = false;
    let mut positional = Vec::new();
    let mut subcommand: Option<String> = None;

    let mut index = 0usize;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "help" | "--help" | "-h" if subcommand.is_none() => {
                println!("{}", context_help());
                return Ok(0);
            }
            "--json" => {
                json = true;
                index += 1;
            }
            "--live-mcp" => {
                live_mcp = true;
                index += 1;
            }
            "--help" | "-h" => {
                println!("{}", context_help());
                return Ok(0);
            }
            arg if arg.starts_with('-') => {
                bail!(
                    "{}",
                    crate::localization::cli_locale()
                        .format("Unknown option: {0}", &[(arg).to_string()])
                );
            }
            arg => {
                if subcommand.is_none() {
                    subcommand = Some(arg.to_owned());
                } else {
                    positional.push(arg.to_owned());
                }
                index += 1;
            }
        }
    }

    // Flags may appear before the subcommand when forwarded via NATIVE_UTILITY.
    // Re-scan: if first positional token is a subcommand, peel it.
    // (Already handled above when subcommand is first non-flag.)

    let command = subcommand.as_deref().unwrap_or("explain");
    if command != "explain" && command != "diff" {
        eprintln!(
            "{}",
            crate::localization::cli_locale().format(
                "Unknown context subcommand: {0}. Try \"maestro context explain\"",
                &[(command).to_string()]
            )
        );
        return Ok(1);
    }

    if command == "diff" {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let before_cwd = if positional.len() >= 2 {
            resolve_path(&positional[0])
        } else {
            cwd.clone()
        };
        let after_cwd = if positional.len() >= 2 {
            resolve_path(&positional[1])
        } else if positional.len() == 1 {
            resolve_path(&positional[0])
        } else {
            cwd
        };

        let (before, after) =
            load_context_manifest_pair_for_command(&before_cwd, &after_cwd, live_mcp).await?;
        let diff = diff_unified_context_manifests(&before, &after);
        if json {
            println!("{}", serde_json::to_string_pretty(&diff)?);
        } else {
            println!("{}", render_context_manifest_diff(&diff));
        }
        return Ok(0);
    }

    // explain
    let cwd = if let Some(path) = positional.first() {
        resolve_path(path)
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    };
    let manifest = load_context_manifest_for_command(&cwd, live_mcp).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&manifest)?);
    } else {
        println!("{}", render_context_manifest_summary(&manifest));
    }
    Ok(0)
}

fn context_help() -> &'static str {
    "Usage: maestro context [explain|diff] [path...] [options]

Commands:
  explain [cwd]              Show the unified prompt-context manifest (default)
  diff [before] [after]      Diff two workspace context manifests

Options:
  --json                     Machine-readable JSON output
  --live-mcp                 Connect live MCP servers for runtime status, resources, and prompts
  --help, -h                 Show this help"
}

// ─────────────────────────────────────────────────────────────
// Project docs
// ─────────────────────────────────────────────────────────────

fn resolve_project_doc_candidate_filenames(config_fallback: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for name in AGENT_CONTEXT_FILES
        .iter()
        .map(|s| (*s).to_string())
        .chain(config_fallback.iter().cloned())
    {
        if seen.insert(name.clone()) {
            out.push(name);
        }
    }
    out
}

fn agent_dir() -> PathBuf {
    env_path("MAESTRO_AGENT_DIR")
        .or_else(|| env_path("PLAYWRIGHT_AGENT_DIR"))
        .or_else(|| env_path("CODING_AGENT_DIR"))
        .or_else(|| maestro_home_dir().map(|home| home.join("agent")))
        .unwrap_or_else(|| PathBuf::from(".maestro/agent"))
}

fn resolve_project_doc_global_directories() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let agent = resolve_path_buf(agent_dir());
    dirs.push(agent);
    if let Some(home) = dirs::home_dir() {
        let config = resolve_path_buf(home.join(".config"));
        if !dirs.iter().any(|d| d == &config) {
            dirs.push(config);
        }
    }
    dirs
}

fn resolve_project_doc_ancestor_directories(cwd: &Path) -> Vec<PathBuf> {
    let mut directories = Vec::new();
    let mut current = resolve_path_buf(cwd.to_path_buf());
    loop {
        directories.push(current.clone());
        match current.parent() {
            Some(parent) if parent != current => {
                current = parent.to_path_buf();
            }
            _ => break,
        }
    }
    directories.reverse();
    directories
}

fn hash_content(content: &str) -> String {
    encode_hex(&Sha256::digest(content.as_bytes()))
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut output, byte| {
        use std::fmt::Write;
        let _ = write!(output, "{byte:02x}");
        output
    })
}

fn truncate_utf8_to_valid_bytes(buffer: &[u8], bytes_read: usize) -> usize {
    let mut end = bytes_read;
    if end == 0 {
        return 0;
    }

    let mut start = end - 1;
    while start > 0 && (buffer[start] & 0b1100_0000) == 0b1000_0000 {
        start -= 1;
    }
    // Also handle start == 0 case for continuation bytes
    if (buffer[start] & 0b1100_0000) == 0b1000_0000 {
        return 0;
    }

    let lead = buffer[start];
    let expected = if lead & 0b1000_0000 == 0 {
        1
    } else if lead & 0b1110_0000 == 0b1100_0000 {
        2
    } else if lead & 0b1111_0000 == 0b1110_0000 {
        3
    } else if lead & 0b1111_1000 == 0b1111_0000 {
        4
    } else {
        end = start;
        return end;
    };

    if start + expected > end {
        end = start;
    }
    end
}

type ProjectDocRead = (String, usize, bool, Option<u64>, Option<usize>);

fn read_project_doc_content(file_path: &Path, budget: Option<usize>) -> Result<ProjectDocRead> {
    let metadata =
        fs::metadata(file_path).with_context(|| format!("stat {}", file_path.display()))?;
    let original_size = metadata.len();

    if let Some(budget) = budget {
        if budget > 0 && original_size > budget as u64 {
            let mut file =
                File::open(file_path).with_context(|| format!("open {}", file_path.display()))?;
            let mut buffer = vec![0u8; budget];
            let bytes_read = file.read(&mut buffer)?;
            let valid = truncate_utf8_to_valid_bytes(&buffer, bytes_read);
            let content = String::from_utf8_lossy(&buffer[..valid]).into_owned();
            return Ok((content, valid, true, Some(original_size), Some(budget)));
        }
    }

    let content =
        fs::read_to_string(file_path).with_context(|| format!("read {}", file_path.display()))?;
    let bytes_read = content.len(); // TS uses Buffer.byteLength (UTF-8); str.len() is UTF-8 bytes in Rust
    Ok((content, bytes_read, false, Some(original_size), None))
}

fn load_first_project_doc_in_dir(
    dir: &Path,
    candidates: &[String],
    source_kind: &str,
    remaining_bytes: Option<usize>,
    diagnostics: &mut Vec<ContextDiagnostic>,
) -> Option<PromptProjectDocEntry> {
    if remaining_bytes == Some(0) {
        return None;
    }
    for filename in candidates {
        let file_path = dir.join(filename);
        if !file_path.is_file() {
            continue;
        }
        let resolved = resolve_path_buf(file_path);
        let read = match read_project_doc_content(&resolved, remaining_bytes) {
            Ok(v) => v,
            Err(error) => {
                diagnostics.push(ContextDiagnostic {
                    code: "read_failed".into(),
                    severity: "warning".into(),
                    message: format!(
                        "Could not read instruction file {}: {error:#}",
                        resolved.display()
                    ),
                    path: Some(resolved.display().to_string()),
                    scope_dir: Some(resolve_path_buf(dir.to_path_buf()).display().to_string()),
                    entry_id: None,
                });
                continue;
            }
        };
        let (raw_content, bytes_read, truncated, original_size, max_bytes) = read;
        let note = if truncated {
            format!(
                "\n\n[Truncated to {bytes_read} bytes from {} bytes.]",
                original_size.unwrap_or(0)
            )
        } else {
            String::new()
        };
        let content = format!("{raw_content}{note}");
        return Some(PromptProjectDocEntry {
            path: resolved.display().to_string(),
            source_kind: source_kind.to_string(),
            scope_dir: resolve_path_buf(dir.to_path_buf()).display().to_string(),
            candidate_name: filename.clone(),
            bytes_read,
            truncated,
            content_hash: hash_content(&raw_content),
            precedence_index: 0,
            content,
            original_size,
            max_bytes,
        });
    }
    None
}

fn load_prompt_project_doc_manifest(cwd: &Path) -> PromptProjectDocManifest {
    let cwd = resolve_path_buf(cwd.to_path_buf());
    let config = load_config(&cwd, None);
    let fallback = config
        .project_doc_fallback_filenames
        .clone()
        .unwrap_or_else(|| vec!["CLAUDE.md".to_string()]);
    let candidates = resolve_project_doc_candidate_filenames(&fallback);
    let max_bytes = config.project_doc_max_bytes;
    let mut remaining_bytes = max_bytes;
    let mut entries: Vec<PromptProjectDocEntry> = Vec::new();
    let mut diagnostics: Vec<ContextDiagnostic> = Vec::new();
    let mut loaded_paths: HashSet<String> = HashSet::new();

    let dirs_to_scan: Vec<(PathBuf, &str)> = resolve_project_doc_global_directories()
        .into_iter()
        .map(|d| (d, "global"))
        .chain(
            resolve_project_doc_ancestor_directories(&cwd)
                .into_iter()
                .map(|d| (d, "project")),
        )
        .collect();

    for (dir, source_kind) in dirs_to_scan {
        if remaining_bytes == Some(0) {
            diagnostics.push(ContextDiagnostic {
                code: "budget_exhausted".into(),
                severity: "warning".into(),
                message: format!(
                    "Skipped instruction lookup under {} because project_doc_max_bytes was exhausted.",
                    resolve_path_buf(dir.clone()).display()
                ),
                path: None,
                scope_dir: Some(resolve_path_buf(dir).display().to_string()),
                entry_id: None,
            });
            break;
        }
        let entry = load_first_project_doc_in_dir(
            &dir,
            &candidates,
            source_kind,
            remaining_bytes,
            &mut diagnostics,
        );
        let Some(mut entry) = entry else {
            continue;
        };
        let resolved_path = entry.path.clone();
        if loaded_paths.contains(&resolved_path) {
            diagnostics.push(ContextDiagnostic {
                code: "duplicate_skipped".into(),
                severity: "warning".into(),
                message: format!(
                    "Skipped duplicate instruction file already loaded from {resolved_path}."
                ),
                path: Some(resolved_path),
                scope_dir: Some(entry.scope_dir.clone()),
                entry_id: None,
            });
            continue;
        }
        loaded_paths.insert(resolved_path.clone());
        entry.precedence_index = entries.len();
        if entry.truncated {
            diagnostics.push(ContextDiagnostic {
                code: "truncated".into(),
                severity: "warning".into(),
                message: format!(
                    "Loaded only {} of {} bytes from {resolved_path}.",
                    entry.bytes_read,
                    entry
                        .original_size
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "unknown".into())
                ),
                path: Some(resolved_path),
                scope_dir: Some(entry.scope_dir.clone()),
                entry_id: None,
            });
        }
        if let Some(remaining) = remaining_bytes.as_mut() {
            *remaining = remaining.saturating_sub(entry.bytes_read);
        }
        entries.push(entry);
    }

    let mut layer_counts: BTreeMap<String, usize> = BTreeMap::new();
    for entry in &entries {
        *layer_counts
            .entry(entry.candidate_name.clone())
            .or_default() += 1;
    }
    for (candidate_name, count) in layer_counts {
        if count > 1 {
            diagnostics.push(ContextDiagnostic {
                code: "multiple_instruction_layers".into(),
                severity: "info".into(),
                message: format!(
                    "{count} {candidate_name} instruction layers were loaded; later project scopes have higher precedence in the prompt."
                ),
                path: None,
                scope_dir: None,
                entry_id: None,
            });
        }
    }

    let bytes_read = entries.iter().map(|e| e.bytes_read).sum();
    PromptProjectDocManifest {
        cwd: cwd.display().to_string(),
        candidates,
        max_bytes,
        bytes_read,
        entries,
        diagnostics,
    }
}

// ─────────────────────────────────────────────────────────────
// Unified manifest
// ─────────────────────────────────────────────────────────────

fn stable_json(value: &JsonValue) -> String {
    match value {
        JsonValue::Null => "null".into(),
        JsonValue::Bool(b) => b.to_string(),
        JsonValue::Number(n) => n.to_string(),
        JsonValue::String(s) => serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into()),
        JsonValue::Array(items) => {
            let parts: Vec<String> = items.iter().map(stable_json).collect();
            format!("[{}]", parts.join(","))
        }
        JsonValue::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .into_iter()
                .map(|k| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(k).unwrap_or_else(|_| "\"\"".into()),
                        stable_json(&map[k])
                    )
                })
                .collect();
            format!("{{{}}}", parts.join(","))
        }
    }
}

fn hash_value(value: &JsonValue) -> String {
    hash_content(&stable_json(value))
}

fn normalize_metadata(metadata: Map<String, JsonValue>) -> Option<Map<String, JsonValue>> {
    let filtered: Map<String, JsonValue> =
        metadata.into_iter().filter(|(_, v)| !v.is_null()).collect();
    if filtered.is_empty() {
        None
    } else {
        Some(filtered)
    }
}

fn summarize_redacted_args(args: &[String]) -> Option<JsonValue> {
    if args.is_empty() {
        // TS: `args ? { count } : undefined` — empty array is still truthy
        return Some(json!({ "count": 0, "redacted": true }));
    }
    Some(json!({ "count": args.len(), "redacted": true }))
}

fn summarize_redacted_command(command: Option<&str>) -> Option<JsonValue> {
    command.map(|_| json!({ "configured": true, "redacted": true }))
}

fn summarize_redacted_url(url: Option<&str>) -> Option<JsonValue> {
    let url = url?;
    match Url::parse(url) {
        Ok(parsed) => {
            let scheme = parsed.scheme().to_string();
            let host = parsed.host_str().unwrap_or("").to_string();
            let host_with_port = if let Some(port) = parsed.port() {
                format!("{host}:{port}")
            } else {
                host
            };
            Some(json!({
                "scheme": scheme,
                "host": host_with_port,
                "redacted": true,
            }))
        }
        Err(_) => Some(json!({ "redacted": true })),
    }
}

fn scope_name(scope: McpConfigScope) -> &'static str {
    match scope {
        McpConfigScope::User => "user",
        McpConfigScope::Managed => "managed",
        McpConfigScope::Local => "local",
        McpConfigScope::Project => "project",
        McpConfigScope::Enterprise => "enterprise",
    }
}

fn transport_name(transport: McpTransport) -> &'static str {
    match transport {
        McpTransport::Stdio => "stdio",
        McpTransport::Http => "http",
        McpTransport::Sse => "sse",
    }
}

fn remote_trust(url: Option<&str>) -> &'static str {
    let Some(url) = url else {
        return "unknown";
    };
    let Ok(url) = Url::parse(url) else {
        return "unknown";
    };
    if crate::orb_connection::validate_hosted_orb_endpoint(url.as_str()).is_ok() {
        return "official";
    }
    match url.host_str().unwrap_or_default() {
        "mcp.evalops.dev" | "api.evalops.dev" => "official",
        "" => "unknown",
        _ => "custom",
    }
}

fn project_doc_entry_id(cwd: &str, entry: &PromptProjectDocEntry) -> String {
    if entry.source_kind == "project" {
        let relative = pathdiff::diff_paths(&entry.path, cwd)
            .map(|p| p.display().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| entry.candidate_name.clone());
        // Normalize Windows separators just in case
        let relative = relative.replace('\\', "/");
        format!("project_doc:{}:{relative}", entry.source_kind)
    } else {
        format!("project_doc:{}:{}", entry.source_kind, entry.path)
    }
}

fn project_doc_entries(manifest: &PromptProjectDocManifest) -> Vec<UnifiedContextManifestEntry> {
    manifest
        .entries
        .iter()
        .map(|entry| {
            let mut metadata = Map::new();
            metadata.insert("sourceKind".into(), json!(entry.source_kind));
            metadata.insert("truncated".into(), json!(entry.truncated));
            if let Some(size) = entry.original_size {
                metadata.insert("originalSize".into(), json!(size));
            }
            if let Some(max) = entry.max_bytes {
                metadata.insert("maxBytes".into(), json!(max));
            }
            UnifiedContextManifestEntry {
                id: project_doc_entry_id(&manifest.cwd, entry),
                kind: "project_doc".into(),
                source: "filesystem".into(),
                status: "loaded".into(),
                label: entry.candidate_name.clone(),
                path: Some(entry.path.clone()),
                scope_dir: Some(entry.scope_dir.clone()),
                server_name: None,
                uri: None,
                prompt_name: None,
                precedence_index: Some(entry.precedence_index),
                bytes_read: Some(entry.bytes_read),
                content_hash: Some(entry.content_hash.clone()),
                metadata: normalize_metadata(metadata),
            }
        })
        .collect()
}

fn mcp_server_config_entry(server: &McpServerConfig) -> UnifiedContextManifestEntry {
    let mut metadata = Map::new();
    metadata.insert("transport".into(), json!(transport_name(server.transport)));
    metadata.insert("scope".into(), json!(scope_name(server.scope)));
    if let Some(cmd) = summarize_redacted_command(server.command.as_deref()) {
        metadata.insert("command".into(), cmd);
    }
    if !server.args.is_empty() {
        if let Some(args) = summarize_redacted_args(&server.args) {
            metadata.insert("args".into(), args);
        }
    }
    if server.cwd.is_some() {
        metadata.insert("cwdConfigured".into(), json!(true));
    }
    if let Some(url) = summarize_redacted_url(server.url.as_deref()) {
        metadata.insert("url".into(), url);
    }
    if !server.env.is_empty() {
        let mut keys: Vec<&String> = server.env.keys().collect();
        keys.sort();
        metadata.insert(
            "envKeys".into(),
            json!(keys.into_iter().cloned().collect::<Vec<_>>()),
        );
    }
    if !server.headers.is_empty() {
        let mut keys: Vec<&String> = server.headers.keys().collect();
        keys.sort();
        metadata.insert(
            "headerKeys".into(),
            json!(keys.into_iter().cloned().collect::<Vec<_>>()),
        );
    }
    if server.headers_helper.is_some() {
        metadata.insert(
            "headersHelper".into(),
            json!({ "configured": true, "redacted": true }),
        );
    }
    if let Some(auth_preset) = &server.auth_preset {
        metadata.insert("authPreset".into(), json!(auth_preset));
    }
    if let Some(supports_parallel) = server.supports_parallel_tool_calls {
        metadata.insert("supportsParallelToolCalls".into(), json!(supports_parallel));
        metadata.insert("parallelSafetyProvenance".into(), json!("static_config"));
    }
    if let Some(requires_approval) = server.requires_project_approval {
        metadata.insert("requiresProjectApproval".into(), json!(requires_approval));
    }
    if matches!(server.transport, McpTransport::Http | McpTransport::Sse) {
        metadata.insert(
            "remoteTrust".into(),
            json!(remote_trust(server.url.as_deref())),
        );
    }
    if let Some(timeout) = server.timeout {
        metadata.insert("timeout".into(), json!(timeout));
    }

    let metadata = normalize_metadata(metadata);
    let content_hash = metadata
        .as_ref()
        .map(|m| hash_value(&JsonValue::Object(m.clone())));

    UnifiedContextManifestEntry {
        id: format!("mcp_server:{}", server.name),
        kind: "mcp_server".into(),
        source: "mcp_config".into(),
        status: "configured".into(),
        label: server.name.clone(),
        path: None,
        scope_dir: None,
        server_name: Some(server.name.clone()),
        uri: None,
        prompt_name: None,
        precedence_index: None,
        bytes_read: None,
        content_hash,
        metadata,
    }
}

fn try_load_maestro_project_mcp(cwd: &Path, config: &mut McpConfig) {
    // Rust load_mcp_config only scans `.composer` for project paths; also pull
    // `.maestro/mcp.json` and `.maestro/mcp.local.json` so context matches TS.
    for (rel, scope) in [
        (".maestro/mcp.local.json", McpConfigScope::Local),
        (".maestro/mcp.json", McpConfigScope::Project),
        (".composer/mcp.local.json", McpConfigScope::Local),
        (".composer/mcp.json", McpConfigScope::Project),
    ] {
        let path = cwd.join(rel);
        if !path.is_file() {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<JsonValue>(&content) else {
            continue;
        };
        merge_raw_mcp_servers(&value, scope, &mut config.servers);
    }
}

fn merge_raw_mcp_servers(
    value: &JsonValue,
    scope: McpConfigScope,
    servers: &mut Vec<McpServerConfig>,
) {
    let mut by_name: HashMap<String, McpServerConfig> =
        servers.drain(..).map(|s| (s.name.clone(), s)).collect();

    if let Some(array) = value.get("servers").and_then(|v| v.as_array()) {
        for item in array {
            if let Some(server) = parse_mcp_server_value(item, None, scope) {
                if server.is_enabled() {
                    by_name.insert(server.name.clone(), server);
                } else {
                    by_name.remove(&server.name);
                }
            }
        }
    }
    if let Some(map) = value.get("mcpServers").and_then(|v| v.as_object()) {
        for (name, item) in map {
            if let Some(server) = parse_mcp_server_value(item, Some(name), scope) {
                if server.is_enabled() {
                    by_name.insert(server.name.clone(), server);
                } else {
                    by_name.remove(&server.name);
                }
            }
        }
    }
    *servers = by_name.into_values().collect();
}

fn parse_mcp_server_value(
    value: &JsonValue,
    name_override: Option<&str>,
    scope: McpConfigScope,
) -> Option<McpServerConfig> {
    let obj = value.as_object()?;
    let name = name_override
        .map(str::to_string)
        .or_else(|| obj.get("name").and_then(|v| v.as_str()).map(str::to_string))?;
    let url = obj.get("url").and_then(|v| v.as_str()).map(str::to_string);
    let command = obj
        .get("command")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let transport = match obj.get("transport").and_then(|v| v.as_str()) {
        Some("http") => McpTransport::Http,
        Some("sse") => McpTransport::Sse,
        Some("stdio") => McpTransport::Stdio,
        _ if url.is_some() => McpTransport::Http,
        _ => McpTransport::Stdio,
    };
    let args = obj
        .get("args")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let mut env = HashMap::new();
    if let Some(map) = obj.get("env").and_then(|v| v.as_object()) {
        for (k, v) in map {
            if let Some(s) = v.as_str() {
                env.insert(k.clone(), s.to_string());
            }
        }
    }
    let mut headers = HashMap::new();
    if let Some(map) = obj.get("headers").and_then(|v| v.as_object()) {
        for (k, v) in map {
            if let Some(s) = v.as_str() {
                headers.insert(k.clone(), s.to_string());
            }
        }
    }
    let cwd = obj.get("cwd").and_then(|v| v.as_str()).map(str::to_string);
    let timeout = obj.get("timeout").and_then(|v| v.as_u64());
    let headers_helper = obj
        .get("headersHelper")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let auth_preset = obj
        .get("authPreset")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let connection_ref = obj
        .get("connectionRef")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let credential_ref = obj
        .get("credentialRef")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let supports_parallel_tool_calls = obj
        .get("supportsParallelToolCalls")
        .and_then(|v| v.as_bool());
    let requires_project_approval = obj.get("requiresProjectApproval").and_then(|v| v.as_bool());
    let disabled_tools = obj
        .get("disabledTools")
        .and_then(|value| value.as_array())
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let enabled = obj.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
    let disabled = obj
        .get("disabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    Some(McpServerConfig {
        name,
        transport,
        command,
        args,
        env,
        cwd,
        url,
        headers,
        headers_helper,
        auth_preset,
        connection_ref,
        credential_ref,
        managed_generation: None,
        supports_parallel_tool_calls,
        requires_project_approval,
        disabled_tools,
        timeout,
        enabled,
        disabled,
        scope,
    })
}

fn load_mcp_config_for_cwd(cwd: &Path) -> McpConfig {
    let mut config = load_mcp_config(Some(cwd));
    try_load_maestro_project_mcp(cwd, &mut config);
    append_managed_mcp_connections(&mut config);
    config
}

fn load_configured_mcp_entries(
    cwd: &Path,
    diagnostics: &mut Vec<ContextDiagnostic>,
) -> Vec<UnifiedContextManifestEntry> {
    let config = load_mcp_config_for_cwd(cwd);

    if !config.servers.is_empty() {
        let n = config.servers.len();
        diagnostics.push(ContextDiagnostic {
            code: "mcp_config_loaded".into(),
            severity: "info".into(),
            message: format!(
                "{n} configured MCP server{} included from config.",
                if n == 1 { "" } else { "s" }
            ),
            path: None,
            scope_dir: None,
            entry_id: None,
        });
    }
    config.servers.iter().map(mcp_server_config_entry).collect()
}

fn summarize_redacted_error(error: Option<&str>) -> Option<JsonValue> {
    error.map(|_| json!({ "present": true, "redacted": true }))
}

fn remote_host_from_url(url: Option<&str>) -> Option<String> {
    let url = url?;
    match Url::parse(url) {
        Ok(parsed) => {
            let host = parsed.host_str().unwrap_or("").to_string();
            if let Some(port) = parsed.port() {
                Some(format!("{host}:{port}"))
            } else if host.is_empty() {
                None
            } else {
                Some(host)
            }
        }
        Err(_) => None,
    }
}

fn mcp_server_status_entry(
    server: &McpServerConfig,
    connected: bool,
    error: Option<&str>,
    tool_count: usize,
    resource_count: usize,
    prompt_count: usize,
) -> UnifiedContextManifestEntry {
    let mut metadata = Map::new();
    metadata.insert("transport".into(), json!(transport_name(server.transport)));
    metadata.insert("scope".into(), json!(scope_name(server.scope)));
    if server.transport == McpTransport::Stdio {
        if let Some(cmd) = summarize_redacted_command(server.command.as_deref()) {
            metadata.insert("command".into(), cmd);
        }
        if !server.args.is_empty() {
            if let Some(args) = summarize_redacted_args(&server.args) {
                metadata.insert("args".into(), args);
            }
        }
        if server.cwd.is_some() {
            metadata.insert("cwdConfigured".into(), json!(true));
        }
    }
    let remote_url = match server.transport {
        McpTransport::Http | McpTransport::Sse => server.url.as_deref(),
        McpTransport::Stdio => None,
    };
    if let Some(url) = summarize_redacted_url(remote_url) {
        metadata.insert("remoteUrl".into(), url);
    }
    if let Some(host) = remote_host_from_url(remote_url) {
        metadata.insert("remoteHost".into(), json!(host));
    }
    if !server.env.is_empty() {
        let mut keys: Vec<&String> = server.env.keys().collect();
        keys.sort();
        metadata.insert(
            "envKeys".into(),
            json!(keys.into_iter().cloned().collect::<Vec<_>>()),
        );
    }
    if !server.headers.is_empty() {
        let mut keys: Vec<&String> = server.headers.keys().collect();
        keys.sort();
        metadata.insert(
            "headerKeys".into(),
            json!(keys.into_iter().cloned().collect::<Vec<_>>()),
        );
    }
    if let Some(timeout) = server.timeout {
        metadata.insert("timeout".into(), json!(timeout));
    }
    metadata.insert("toolCount".into(), json!(tool_count));
    metadata.insert("resourceCount".into(), json!(resource_count));
    metadata.insert("promptCount".into(), json!(prompt_count));
    if let Some(err) = summarize_redacted_error(error) {
        metadata.insert("error".into(), err);
    }

    let metadata = normalize_metadata(metadata);
    let content_hash = metadata
        .as_ref()
        .map(|m| hash_value(&JsonValue::Object(m.clone())));

    let status = if error.is_some() {
        "error"
    } else if connected {
        "connected"
    } else {
        "disconnected"
    };

    UnifiedContextManifestEntry {
        id: format!("mcp_server:{}", server.name),
        kind: "mcp_server".into(),
        source: "mcp_runtime".into(),
        status: status.into(),
        label: server.name.clone(),
        path: None,
        scope_dir: None,
        server_name: Some(server.name.clone()),
        uri: None,
        prompt_name: None,
        precedence_index: None,
        bytes_read: None,
        content_hash,
        metadata,
    }
}

fn mcp_resource_entry(server_name: &str, uri: &str) -> UnifiedContextManifestEntry {
    let content_hash = hash_value(&json!({
        "serverName": server_name,
        "uri": uri,
    }));
    UnifiedContextManifestEntry {
        id: format!("mcp_resource:{server_name}:{uri}"),
        kind: "mcp_resource".into(),
        source: "mcp_runtime".into(),
        status: "available".into(),
        label: uri.to_string(),
        path: None,
        scope_dir: None,
        server_name: Some(server_name.to_string()),
        uri: Some(uri.to_string()),
        prompt_name: None,
        precedence_index: None,
        bytes_read: None,
        content_hash: Some(content_hash),
        metadata: None,
    }
}

fn mcp_prompt_entry(server_name: &str, prompt: &McpPrompt) -> UnifiedContextManifestEntry {
    let mut metadata = Map::new();
    if let Some(title) = &prompt.title {
        metadata.insert("title".into(), json!(title));
    }
    if let Some(description) = &prompt.description {
        metadata.insert("description".into(), json!(description));
    }
    if let Some(arguments) = &prompt.arguments {
        if let Ok(value) = serde_json::to_value(arguments) {
            metadata.insert("arguments".into(), value);
        }
    }
    let metadata = normalize_metadata(metadata);
    let content_hash = hash_value(&json!({
        "serverName": server_name,
        "promptName": prompt.name,
        "metadata": metadata,
    }));
    UnifiedContextManifestEntry {
        id: format!("mcp_prompt:{server_name}:{}", prompt.name),
        kind: "mcp_prompt".into(),
        source: "mcp_runtime".into(),
        status: "available".into(),
        label: prompt.title.clone().unwrap_or_else(|| prompt.name.clone()),
        path: None,
        scope_dir: None,
        server_name: Some(server_name.to_string()),
        uri: None,
        prompt_name: Some(prompt.name.clone()),
        precedence_index: None,
        bytes_read: None,
        content_hash: Some(content_hash),
        metadata,
    }
}

/// Connect configured MCP servers, collect runtime manifest entries, then leave
/// cleanup to the caller (`disconnect_all`).
async fn load_runtime_mcp_entries(
    cwd: &Path,
    client: &McpClient,
    diagnostics: &mut Vec<ContextDiagnostic>,
) -> Vec<UnifiedContextManifestEntry> {
    let config = load_mcp_config_for_cwd(cwd);
    let mut connect_errors: HashMap<String, String> = HashMap::new();
    for server in &config.servers {
        if let Err(error) = client
            .connect_with_workspace_trust(server.clone(), Some(cwd))
            .await
        {
            connect_errors.insert(server.name.clone(), error.to_string());
        }
    }

    let connected: HashSet<String> = client.connected_servers().await.into_iter().collect();
    let tools_by_server: HashMap<String, Vec<String>> =
        client.list_tools_by_server().await.into_iter().collect();
    let resources_by_server: HashMap<String, Vec<String>> =
        client.list_all_resources().await.into_iter().collect();
    let prompts_by_server: HashMap<String, Vec<McpPrompt>> =
        client.list_all_prompt_details().await.into_iter().collect();

    let mut entries = Vec::new();
    for server in &config.servers {
        let is_connected = connected.contains(&server.name);
        let tools = tools_by_server.get(&server.name).map(Vec::len).unwrap_or(0);
        let resources = resources_by_server
            .get(&server.name)
            .cloned()
            .unwrap_or_default();
        let prompts = prompts_by_server
            .get(&server.name)
            .cloned()
            .unwrap_or_default();
        let error = connect_errors.get(&server.name).map(String::as_str);

        entries.push(mcp_server_status_entry(
            server,
            is_connected,
            error,
            tools,
            resources.len(),
            prompts.len(),
        ));
        for uri in &resources {
            entries.push(mcp_resource_entry(&server.name, uri));
        }
        for prompt in &prompts {
            entries.push(mcp_prompt_entry(&server.name, prompt));
        }
        if !is_connected && error.is_some() {
            diagnostics.push(ContextDiagnostic {
                code: "mcp_runtime_unavailable".into(),
                severity: "warning".into(),
                message: format!(
                    "MCP server {} is unavailable; error details redacted.",
                    server.name
                ),
                path: None,
                scope_dir: None,
                entry_id: Some(format!("mcp_server:{}", server.name)),
            });
        }
    }
    entries
}

fn load_unified_context_manifest(cwd: &Path) -> Result<UnifiedContextManifest> {
    let cwd = resolve_path_buf(cwd.to_path_buf());
    let project_docs = load_prompt_project_doc_manifest(&cwd);
    let mut diagnostics = project_docs.diagnostics.clone();
    let mut entries = project_doc_entries(&project_docs);
    entries.extend(load_configured_mcp_entries(&cwd, &mut diagnostics));

    Ok(finalize_context_manifest(UnifiedContextManifest {
        protocol_version: PROTOCOL_VERSION.into(),
        version: 1,
        manifest_sha256: String::new(),
        cwd: cwd.display().to_string(),
        project_docs,
        entries,
        diagnostics,
    }))
}

async fn load_unified_context_manifest_live(
    cwd: &Path,
    client: &McpClient,
) -> Result<UnifiedContextManifest> {
    let cwd = resolve_path_buf(cwd.to_path_buf());
    let project_docs = load_prompt_project_doc_manifest(&cwd);
    let mut diagnostics = project_docs.diagnostics.clone();
    let mut entries = project_doc_entries(&project_docs);
    // When runtime status is present, TS skips configured-only MCP entries.
    entries.extend(load_runtime_mcp_entries(&cwd, client, &mut diagnostics).await);

    Ok(finalize_context_manifest(UnifiedContextManifest {
        protocol_version: PROTOCOL_VERSION.into(),
        version: 1,
        manifest_sha256: String::new(),
        cwd: cwd.display().to_string(),
        project_docs,
        entries,
        diagnostics,
    }))
}

fn finalize_context_manifest(mut manifest: UnifiedContextManifest) -> UnifiedContextManifest {
    let value = serde_json::to_value(&manifest).expect("context manifests are serializable");
    manifest.manifest_sha256 = crate::evidence::canonical_json_sha256(&value);
    manifest
}

/// Build the exact redacted context manifest persisted in a native session header.
pub fn load_unified_context_manifest_json(cwd: &Path) -> Result<JsonValue> {
    serde_json::to_value(load_unified_context_manifest(cwd)?)
        .context("failed to serialize unified context manifest")
}

async fn load_context_manifest_for_command(
    cwd: &Path,
    live_mcp: bool,
) -> Result<UnifiedContextManifest> {
    if !live_mcp {
        return load_unified_context_manifest(cwd);
    }

    let client = McpClient::new();
    let result = load_unified_context_manifest_live(cwd, &client).await;
    client.disconnect_all().await;
    result
}

async fn load_context_manifest_pair_for_command(
    before_cwd: &Path,
    after_cwd: &Path,
    live_mcp: bool,
) -> Result<(UnifiedContextManifest, UnifiedContextManifest)> {
    if !live_mcp {
        return Ok((
            load_unified_context_manifest(before_cwd)?,
            load_unified_context_manifest(after_cwd)?,
        ));
    }

    // Match TS: connect live for each side, then disconnect_all once in finally.
    let client = McpClient::new();
    let result = async {
        let before = load_unified_context_manifest_live(before_cwd, &client).await?;
        // Drop before-side connections so after-side config is authoritative.
        client.disconnect_all().await;
        let after = load_unified_context_manifest_live(after_cwd, &client).await?;
        Ok((before, after))
    }
    .await;
    client.disconnect_all().await;
    result
}

fn entry_field_json(entry: &UnifiedContextManifestEntry, field: &str) -> JsonValue {
    match field {
        "source" => json!(entry.source),
        "status" => json!(entry.status),
        "label" => json!(entry.label),
        "path" => json!(entry.path),
        "scopeDir" => json!(entry.scope_dir),
        "serverName" => json!(entry.server_name),
        "uri" => json!(entry.uri),
        "promptName" => json!(entry.prompt_name),
        "precedenceIndex" => json!(entry.precedence_index),
        "bytesRead" => json!(entry.bytes_read),
        "contentHash" => json!(entry.content_hash),
        "metadata" => json!(entry.metadata),
        _ => JsonValue::Null,
    }
}

fn compare_entry(
    before: &UnifiedContextManifestEntry,
    after: &UnifiedContextManifestEntry,
) -> Vec<String> {
    let fields = [
        "source",
        "status",
        "label",
        "path",
        "scopeDir",
        "serverName",
        "uri",
        "promptName",
        "precedenceIndex",
        "bytesRead",
        "contentHash",
        "metadata",
    ];
    let skip_path_scope = before.kind == "project_doc" && after.kind == "project_doc";
    fields
        .into_iter()
        .filter(|field| {
            if skip_path_scope && (*field == "path" || *field == "scopeDir") {
                return false;
            }
            stable_json(&entry_field_json(before, field))
                != stable_json(&entry_field_json(after, field))
        })
        .map(str::to_string)
        .collect()
}

fn diff_unified_context_manifests(
    before: &UnifiedContextManifest,
    after: &UnifiedContextManifest,
) -> UnifiedContextManifestDiff {
    let before_map: HashMap<&str, &UnifiedContextManifestEntry> =
        before.entries.iter().map(|e| (e.id.as_str(), e)).collect();
    let after_map: HashMap<&str, &UnifiedContextManifestEntry> =
        after.entries.iter().map(|e| (e.id.as_str(), e)).collect();

    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut changed = Vec::new();
    let mut unchanged = Vec::new();

    for (id, after_entry) in &after_map {
        match before_map.get(id) {
            None => {
                added.push(UnifiedContextManifestDiffEntry {
                    id: (*id).to_string(),
                    kind: after_entry.kind.clone(),
                    label: after_entry.label.clone(),
                    before: None,
                    after: Some((*after_entry).clone()),
                    changes: None,
                });
            }
            Some(before_entry) => {
                let changes = compare_entry(before_entry, after_entry);
                if changes.is_empty() {
                    unchanged.push(UnifiedContextManifestDiffEntry {
                        id: (*id).to_string(),
                        kind: after_entry.kind.clone(),
                        label: after_entry.label.clone(),
                        before: Some((*before_entry).clone()),
                        after: Some((*after_entry).clone()),
                        changes: None,
                    });
                } else {
                    changed.push(UnifiedContextManifestDiffEntry {
                        id: (*id).to_string(),
                        kind: after_entry.kind.clone(),
                        label: after_entry.label.clone(),
                        before: Some((*before_entry).clone()),
                        after: Some((*after_entry).clone()),
                        changes: Some(changes),
                    });
                }
            }
        }
    }

    for (id, before_entry) in &before_map {
        if !after_map.contains_key(id) {
            removed.push(UnifiedContextManifestDiffEntry {
                id: (*id).to_string(),
                kind: before_entry.kind.clone(),
                label: before_entry.label.clone(),
                before: Some((*before_entry).clone()),
                after: None,
                changes: None,
            });
        }
    }

    added.sort_by(|a, b| a.id.cmp(&b.id));
    removed.sort_by(|a, b| a.id.cmp(&b.id));
    changed.sort_by(|a, b| a.id.cmp(&b.id));
    unchanged.sort_by(|a, b| a.id.cmp(&b.id));

    let mut diagnostics = before.diagnostics.clone();
    diagnostics.extend(after.diagnostics.clone());

    UnifiedContextManifestDiff {
        before_cwd: before.cwd.clone(),
        after_cwd: after.cwd.clone(),
        added,
        removed,
        changed,
        unchanged,
        diagnostics,
    }
}

// ─────────────────────────────────────────────────────────────
// Renderers
// ─────────────────────────────────────────────────────────────

fn format_path(path: &str, cwd: &str) -> String {
    let resolved = resolve_path(path);
    let resolved_cwd = resolve_path(cwd);
    let home = dirs::home_dir();

    if resolved == resolved_cwd {
        return ".".into();
    }
    if let Ok(rel) = resolved.strip_prefix(&resolved_cwd) {
        return rel.display().to_string();
    }
    if let Some(ref home) = home {
        if resolved == *home {
            return "~".into();
        }
        if let Ok(rel) = resolved.strip_prefix(home) {
            return format!("~/{}", rel.display());
        }
    }
    resolved.display().to_string()
}

fn format_bytes(n: usize) -> String {
    // TS uses toLocaleString(); keep simple for portability.
    let s = n.to_string();
    let mut out = String::new();
    let chars: Vec<char> = s.chars().rev().collect();
    for (i, c) in chars.iter().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(*c);
    }
    out.chars().rev().collect()
}

fn render_context_manifest_summary(manifest: &UnifiedContextManifest) -> String {
    let mut lines = Vec::new();
    lines.push(crate::localization::cli_locale().format(
        "Prompt context for {0}",
        std::slice::from_ref(&(manifest.cwd)),
    ));
    lines.push(crate::localization::cli_locale().format(
        "Generation: {0}",
        std::slice::from_ref(&(manifest.manifest_sha256)),
    ));
    let budget = match manifest.project_docs.max_bytes {
        None => crate::localization::cli_locale().format(
            "{0} bytes used (unlimited)",
            &[(format_bytes(manifest.project_docs.bytes_read)).clone()],
        ),
        Some(max) => crate::localization::cli_locale().format(
            "{0} / {1} bytes used",
            &[
                (format_bytes(manifest.project_docs.bytes_read)).clone(),
                (format_bytes(max)).clone(),
            ],
        ),
    };
    lines.push(
        crate::localization::cli_locale().format("Budget: {0}", std::slice::from_ref(&(budget))),
    );
    lines.push(crate::localization::cli_locale().format(
        "Candidate order: {0}",
        &[(manifest.project_docs.candidates.join(", ")).clone()],
    ));
    lines.push(String::new());

    let project_docs: Vec<_> = manifest
        .entries
        .iter()
        .filter(|e| e.kind == "project_doc")
        .collect();
    if project_docs.is_empty() {
        lines.push(
            crate::localization::cli_locale()
                .translate("Loaded files: none")
                .into(),
        );
    } else {
        lines.push(
            crate::localization::cli_locale()
                .translate("Loaded files:")
                .into(),
        );
        for entry in project_docs {
            let entry_path = entry.path.as_deref().unwrap_or(entry.id.as_str());
            let mut flags = Vec::new();
            let source_kind = entry
                .metadata
                .as_ref()
                .and_then(|m| m.get("sourceKind"))
                .and_then(|v| v.as_str())
                .unwrap_or("project");
            flags.push(source_kind.to_string());
            flags.push(format!(
                "{} bytes",
                format_bytes(entry.bytes_read.unwrap_or(0))
            ));
            if let Some(hash) = &entry.content_hash {
                let short = if hash.len() >= 12 { &hash[..12] } else { hash };
                flags.push(format!("sha256:{short}"));
            }
            if entry
                .metadata
                .as_ref()
                .and_then(|m| m.get("truncated"))
                .and_then(|v| v.as_bool())
                == Some(true)
            {
                flags.push("truncated".into());
            }
            lines.push(format!(
                "{}. {} ({})",
                entry.precedence_index.unwrap_or(0) + 1,
                format_path(entry_path, &manifest.cwd),
                flags.join(", ")
            ));
            if let Some(scope) = &entry.scope_dir {
                lines.push(format!("   scope: {}", format_path(scope, &manifest.cwd)));
            }
        }
    }

    let mcp_entries: Vec<_> = manifest
        .entries
        .iter()
        .filter(|e| e.kind.starts_with("mcp_"))
        .collect();
    if !mcp_entries.is_empty() {
        lines.push(String::new());
        lines.push(
            crate::localization::cli_locale()
                .translate("MCP context:")
                .into(),
        );
        for entry in mcp_entries {
            let location = entry
                .uri
                .as_ref()
                .or(entry.prompt_name.as_ref())
                .or(entry.server_name.as_ref())
                .unwrap_or(&entry.id);
            lines.push(format!("- {} {} ({})", entry.kind, location, entry.status));
        }
    }

    if !manifest.diagnostics.is_empty() {
        lines.push(String::new());
        lines.push(
            crate::localization::cli_locale()
                .translate("Diagnostics:")
                .into(),
        );
        for diagnostic in &manifest.diagnostics {
            let location = diagnostic.path.as_ref().or(diagnostic.scope_dir.as_ref());
            let suffix = location
                .map(|loc| format!(" [{}]", format_path(loc, &manifest.cwd)))
                .unwrap_or_default();
            lines.push(format!(
                "- {} {}: {}{suffix}",
                diagnostic.severity, diagnostic.code, diagnostic.message
            ));
        }
    }

    lines.join("\n")
}

fn render_context_manifest_diff(diff: &UnifiedContextManifestDiff) -> String {
    let locale = crate::localization::cli_locale();
    let mut lines = vec![
        locale.translate("Context diff").into(),
        locale.format("Before: {0}", std::slice::from_ref(&diff.before_cwd)),
        locale.format("After:  {0}", std::slice::from_ref(&diff.after_cwd)),
        String::new(),
        locale.format(
            "Summary: {0} added, {1} removed, {2} changed, {3} unchanged",
            &[
                diff.added.len().to_string(),
                diff.removed.len().to_string(),
                diff.changed.len().to_string(),
                diff.unchanged.len().to_string(),
            ],
        ),
    ];

    let append_group = |lines: &mut Vec<String>,
                        title: &str,
                        prefix: &str,
                        entries: &[UnifiedContextManifestDiffEntry]| {
        if entries.is_empty() {
            return;
        }
        lines.push(String::new());
        lines.push(format!("{title}:"));
        for entry in entries {
            let changes = entry
                .changes
                .as_ref()
                .filter(|c| !c.is_empty())
                .map(|c| format!(" [{}]", c.join(", ")))
                .unwrap_or_default();
            lines.push(format!("{prefix} {} {}{changes}", entry.kind, entry.label));
        }
    };

    append_group(
        &mut lines,
        crate::localization::cli_locale().translate("Added"),
        "+",
        &diff.added,
    );
    append_group(
        &mut lines,
        crate::localization::cli_locale().translate("Removed"),
        "-",
        &diff.removed,
    );
    append_group(
        &mut lines,
        crate::localization::cli_locale().translate("Changed"),
        "~",
        &diff.changed,
    );

    if !diff.diagnostics.is_empty() {
        lines.push(String::new());
        lines.push(
            crate::localization::cli_locale()
                .translate("Diagnostics:")
                .into(),
        );
        for diagnostic in &diff.diagnostics {
            lines.push(format!(
                "- {} {}: {}",
                diagnostic.severity, diagnostic.code, diagnostic.message
            ));
        }
    }

    lines.join("\n")
}

// ─────────────────────────────────────────────────────────────
// Path helpers
// ─────────────────────────────────────────────────────────────

fn resolve_path(path: &str) -> PathBuf {
    resolve_path_buf(PathBuf::from(path))
}

fn resolve_path_buf(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        // Best-effort canonicalize without requiring existence
        return dunce::canonicalize(&path).unwrap_or(path);
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let joined = cwd.join(path);
    dunce::canonicalize(&joined).unwrap_or(joined)
}

// ─────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn write_file(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut f = File::create(path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    #[test]
    fn mcp_manifest_redacts_auth_and_records_native_trust_metadata() {
        let server = McpServerConfig {
            name: "platform".to_string(),
            transport: McpTransport::Http,
            command: None,
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            url: Some("https://mcp.evalops.dev/v1".to_string()),
            headers: HashMap::from([("Authorization".to_string(), "secret".to_string())]),
            headers_helper: Some("credential-helper".to_string()),
            auth_preset: Some("evalops".to_string()),
            connection_ref: None,
            credential_ref: None,
            managed_generation: None,
            supports_parallel_tool_calls: Some(true),
            requires_project_approval: Some(true),
            disabled_tools: Vec::new(),
            timeout: Some(5_000),
            enabled: true,
            disabled: false,
            scope: McpConfigScope::Project,
        };
        let entry = mcp_server_config_entry(&server);
        let metadata = entry.metadata.expect("MCP metadata");
        assert_eq!(metadata["headerKeys"], json!(["Authorization"]));
        assert_eq!(metadata["headersHelper"]["redacted"], true);
        assert_eq!(metadata["authPreset"], "evalops");
        assert_eq!(metadata["supportsParallelToolCalls"], true);
        assert_eq!(metadata["parallelSafetyProvenance"], "static_config");
        assert_eq!(metadata["requiresProjectApproval"], true);
        assert_eq!(metadata["remoteTrust"], "official");
        assert!(!serde_json::to_string(&metadata).unwrap().contains("secret"));
    }

    #[test]
    fn mcp_manifest_marks_canonical_hosted_orb_as_official() {
        assert_eq!(
            remote_trust(Some(crate::orb_connection::HOSTED_ORB_MCP_ENDPOINT)),
            "official"
        );
    }

    #[test]
    fn renders_prompt_context_manifest_summary() {
        let root = tempdir().unwrap();
        let app = root.path().join("apps").join("web");
        fs::create_dir_all(&app).unwrap();
        write_file(&root.path().join("AGENTS.md"), "root rules");
        write_file(&app.join("AGENTS.md"), "app rules");

        let manifest = load_unified_context_manifest(&app).unwrap();
        let summary = render_context_manifest_summary(&manifest);

        assert!(summary.contains(&format!(
            "Prompt context for {}",
            resolve_path_buf(app).display()
        )));
        assert!(summary.contains("Loaded files:"));
        assert!(summary.contains("AGENTS.md"));
        assert!(summary.contains("sha256:"));
        assert!(summary.contains("multiple_instruction_layers"));
    }

    #[test]
    fn explain_json_includes_project_docs() {
        let root = tempdir().unwrap();
        write_file(&root.path().join("AGENTS.md"), "root rules");

        let manifest = load_unified_context_manifest(root.path()).unwrap();
        assert_eq!(manifest.version, 1);
        assert!(manifest.manifest_sha256.starts_with("sha256:"));
        assert_eq!(
            manifest.cwd,
            resolve_path_buf(root.path().to_path_buf())
                .display()
                .to_string()
        );
        assert!(!manifest.project_docs.entries.is_empty());
        let first = &manifest.project_docs.entries[0];
        assert_eq!(first.candidate_name, "AGENTS.md");
        assert_eq!(first.source_kind, "project");
        assert_eq!(first.precedence_index, 0);
        assert!(manifest.entries.iter().any(|e| e.kind == "project_doc"));
    }

    #[test]
    fn manifest_generation_changes_with_context_content() {
        let root = tempdir().unwrap();
        let rules = root.path().join("AGENTS.md");
        write_file(&rules, "first rules");
        let first = load_unified_context_manifest(root.path()).unwrap();

        write_file(&rules, "second rules");
        let second = load_unified_context_manifest(root.path()).unwrap();

        assert_ne!(first.manifest_sha256, second.manifest_sha256);
    }

    #[test]
    fn includes_configured_mcp_servers() {
        let root = tempdir().unwrap();
        let mcp_dir = root.path().join(".maestro");
        fs::create_dir_all(&mcp_dir).unwrap();
        write_file(
            &mcp_dir.join("mcp.json"),
            r#"{
              "servers": [
                {
                  "name": "docs",
                  "transport": "http",
                  "url": "https://mcp.example.test",
                  "scope": "project"
                }
              ]
            }"#,
        );

        let manifest = load_unified_context_manifest(root.path()).unwrap();
        let entry = manifest
            .entries
            .iter()
            .find(|e| e.id == "mcp_server:docs")
            .expect("mcp server entry");
        assert_eq!(entry.kind, "mcp_server");
        assert_eq!(entry.source, "mcp_config");
        assert_eq!(entry.status, "configured");
        let metadata = entry.metadata.as_ref().unwrap();
        assert_eq!(metadata.get("transport").unwrap(), "http");
        let url = metadata.get("url").unwrap();
        assert_eq!(
            url.get("host").and_then(|v| v.as_str()),
            Some("mcp.example.test")
        );
        assert_eq!(url.get("scheme").and_then(|v| v.as_str()), Some("https"));
        assert_eq!(url.get("redacted").and_then(|v| v.as_bool()), Some(true));
    }

    #[test]
    fn redacts_expanded_mcp_config_strings() {
        let root = tempdir().unwrap();
        let mcp_dir = root.path().join(".maestro");
        fs::create_dir_all(&mcp_dir).unwrap();
        write_file(
            &mcp_dir.join("mcp.json"),
            r#"{
              "servers": [
                {
                  "name": "secrets",
                  "transport": "stdio",
                  "command": "mcp-super-secret-token",
                  "args": ["--token", "super-secret-token"],
                  "cwd": "/tmp/super-secret-token"
                },
                {
                  "name": "remote",
                  "transport": "http",
                  "url": "https://example.test/super-secret-token?token=super-secret-token"
                }
              ]
            }"#,
        );

        let manifest = load_unified_context_manifest(root.path()).unwrap();
        let serialized = serde_json::to_string(&manifest).unwrap();
        assert!(!serialized.contains("super-secret-token"));

        let secrets = manifest
            .entries
            .iter()
            .find(|e| e.id == "mcp_server:secrets")
            .unwrap();
        let meta = secrets.metadata.as_ref().unwrap();
        assert_eq!(
            meta.get("command").unwrap(),
            &json!({ "configured": true, "redacted": true })
        );
        assert_eq!(
            meta.get("args").unwrap(),
            &json!({ "count": 2, "redacted": true })
        );
        assert_eq!(meta.get("cwdConfigured").unwrap(), &json!(true));

        let remote = manifest
            .entries
            .iter()
            .find(|e| e.id == "mcp_server:remote")
            .unwrap();
        let url = remote.metadata.as_ref().unwrap().get("url").unwrap();
        assert_eq!(
            url.get("host").and_then(|v| v.as_str()),
            Some("example.test")
        );
        assert_eq!(url.get("scheme").and_then(|v| v.as_str()), Some("https"));
    }

    #[test]
    fn renders_context_diffs() {
        let before_root = tempdir().unwrap();
        let after_root = tempdir().unwrap();
        write_file(&before_root.path().join("AGENTS.md"), "root rules");
        write_file(&after_root.path().join("AGENTS.md"), "new root rules");
        write_file(&after_root.path().join("CLAUDE.md"), "fallback rules");

        let before = load_unified_context_manifest(before_root.path()).unwrap();
        let after = load_unified_context_manifest(after_root.path()).unwrap();
        let diff = diff_unified_context_manifests(&before, &after);
        let rendered = render_context_manifest_diff(&diff);

        // CLAUDE.md is a candidate after AGENTS.md; first match wins per dir so
        // CLAUDE.md at after root is not loaded when AGENTS.md exists. Content
        // change of AGENTS.md should show as changed.
        assert_eq!(diff.changed.len(), 1);
        assert!(rendered.contains("Context diff"));
        assert!(rendered.contains("Summary:"));
    }

    #[test]
    fn matches_project_docs_across_workspace_roots_by_logical_path() {
        let before_root = tempdir().unwrap();
        let after_root = tempdir().unwrap();
        write_file(&before_root.path().join("AGENTS.md"), "root rules");
        write_file(&after_root.path().join("AGENTS.md"), "root rules");

        let before = load_unified_context_manifest(before_root.path()).unwrap();
        let after = load_unified_context_manifest(after_root.path()).unwrap();
        let diff = diff_unified_context_manifests(&before, &after);

        assert!(diff.added.is_empty());
        assert!(diff.removed.is_empty());
        assert!(diff.changed.is_empty());
        assert!(diff.unchanged.iter().any(|e| {
            e.id == "project_doc:project:AGENTS.md"
                && e.kind == "project_doc"
                && e.label == "AGENTS.md"
        }));
    }

    #[test]
    fn truncate_utf8_handles_multibyte() {
        // "é" is 2 bytes in UTF-8 (0xC3 0xA9)
        let bytes = "abé".as_bytes();
        assert_eq!(truncate_utf8_to_valid_bytes(bytes, 3), 2); // cut mid-char → "ab"
        assert_eq!(truncate_utf8_to_valid_bytes(bytes, 4), 4);
    }

    /// Minimal NDJSON MCP stdio server used by live-mcp integration tests.
    fn write_fake_mcp_stdio_server(path: &Path) {
        write_file(
            path,
            r#"#!/usr/bin/env python3
import json
import sys

def reply(msg_id, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": msg_id, "result": result}) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except json.JSONDecodeError:
        continue
    method = msg.get("method")
    msg_id = msg.get("id")
    if method == "initialize":
        reply(msg_id, {
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}, "resources": {}, "prompts": {}},
            "serverInfo": {"name": "fake-context-mcp", "version": "1.0.0"},
        })
    elif method == "notifications/initialized":
        continue
    elif method == "tools/list":
        reply(msg_id, {
            "tools": [{
                "name": "echo",
                "description": "Echo input",
                "inputSchema": {"type": "object", "properties": {}},
            }],
        })
    elif method == "resources/list":
        reply(msg_id, {
            "resources": [{
                "uri": "file://docs/readme",
                "name": "readme",
            }],
        })
    elif method == "prompts/list":
        reply(msg_id, {
            "prompts": [{
                "name": "greet",
                "title": "Greet",
                "description": "Say hello",
            }],
        })
    elif msg_id is not None:
        sys.stdout.write(json.dumps({
            "jsonrpc": "2.0",
            "id": msg_id,
            "error": {"code": -32601, "message": f"Method not found: {method}"},
        }) + "\n")
        sys.stdout.flush()
"#,
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(path, perms).unwrap();
        }
    }

    #[tokio::test]
    async fn live_mcp_denies_untrusted_project_servers() {
        let root = tempdir().unwrap();
        write_file(&root.path().join("AGENTS.md"), "root rules");
        let server_script = root.path().join("fake_mcp_server.py");
        write_fake_mcp_stdio_server(&server_script);

        let mcp_dir = root.path().join(".maestro");
        fs::create_dir_all(&mcp_dir).unwrap();
        write_file(
            &mcp_dir.join("mcp.json"),
            &format!(
                r#"{{
              "servers": [
                {{
                  "name": "fake",
                  "transport": "stdio",
                  "command": "python3",
                  "args": ["{}"]
                }}
              ]
            }}"#,
                server_script.display()
            ),
        );

        let manifest = load_context_manifest_for_command(root.path(), true)
            .await
            .expect("live mcp explain");

        let server = manifest
            .entries
            .iter()
            .find(|e| e.id == "mcp_server:fake")
            .expect("runtime server entry");
        assert_eq!(server.source, "mcp_runtime");
        assert_eq!(server.status, "error");
        let meta = server.metadata.as_ref().unwrap();
        assert_eq!(
            meta.get("error").unwrap(),
            &json!({ "present": true, "redacted": true })
        );
        assert!(
            !manifest
                .entries
                .iter()
                .any(|e| e.id == "mcp_resource:fake:file://docs/readme")
        );
        assert!(
            !manifest
                .entries
                .iter()
                .any(|e| e.id == "mcp_prompt:fake:greet")
        );
        assert!(manifest.diagnostics.iter().any(|d| {
            d.code == "mcp_runtime_unavailable" && d.entry_id.as_deref() == Some("mcp_server:fake")
        }));

        // Runtime mode replaces configured-only entries (TS loadConfiguredMcpEntries skip).
        assert!(
            !manifest
                .entries
                .iter()
                .any(|e| e.id == "mcp_server:fake" && e.source == "mcp_config")
        );
    }

    #[tokio::test]
    async fn live_mcp_records_unavailable_server_diagnostics() {
        let root = tempdir().unwrap();
        write_file(&root.path().join("AGENTS.md"), "root rules");
        let mcp_dir = root.path().join(".maestro");
        fs::create_dir_all(&mcp_dir).unwrap();
        write_file(
            &mcp_dir.join("mcp.json"),
            r#"{
              "servers": [
                {
                  "name": "broken",
                  "transport": "stdio",
                  "command": "this-command-does-not-exist-maestro-context-live-mcp",
                  "args": []
                }
              ]
            }"#,
        );

        let manifest = load_context_manifest_for_command(root.path(), true)
            .await
            .expect("live mcp with failed server");

        let server = manifest
            .entries
            .iter()
            .find(|e| e.id == "mcp_server:broken")
            .expect("error server entry");
        assert_eq!(server.source, "mcp_runtime");
        assert_eq!(server.status, "error");
        let meta = server.metadata.as_ref().unwrap();
        assert_eq!(
            meta.get("error").unwrap(),
            &json!({ "present": true, "redacted": true })
        );
        assert!(manifest.diagnostics.iter().any(|d| {
            d.code == "mcp_runtime_unavailable"
                && d.entry_id.as_deref() == Some("mcp_server:broken")
        }));
    }

    #[tokio::test]
    async fn live_mcp_diff_denies_untrusted_project_servers_on_both_sides() {
        let before_root = tempdir().unwrap();
        let after_root = tempdir().unwrap();
        write_file(&before_root.path().join("AGENTS.md"), "before rules");
        write_file(&after_root.path().join("AGENTS.md"), "after rules");

        let server_script = before_root.path().join("fake_mcp_server.py");
        write_fake_mcp_stdio_server(&server_script);

        for root in [before_root.path(), after_root.path()] {
            let mcp_dir = root.join(".maestro");
            fs::create_dir_all(&mcp_dir).unwrap();
            write_file(
                &mcp_dir.join("mcp.json"),
                &format!(
                    r#"{{
                  "servers": [
                    {{
                      "name": "fake",
                      "transport": "stdio",
                      "command": "python3",
                      "args": ["{}"]
                    }}
                  ]
                }}"#,
                    server_script.display()
                ),
            );
        }

        let (before, after) =
            load_context_manifest_pair_for_command(before_root.path(), after_root.path(), true)
                .await
                .expect("live mcp diff pair");

        for manifest in [&before, &after] {
            let server = manifest
                .entries
                .iter()
                .find(|e| e.id == "mcp_server:fake")
                .expect("runtime server");
            assert_eq!(server.status, "error");
            assert!(
                !manifest
                    .entries
                    .iter()
                    .any(|e| e.kind == "mcp_resource" && e.server_name.as_deref() == Some("fake"))
            );
            assert!(manifest.diagnostics.iter().any(|d| {
                d.code == "mcp_runtime_unavailable"
                    && d.entry_id.as_deref() == Some("mcp_server:fake")
            }));
        }

        let diff = diff_unified_context_manifests(&before, &after);
        assert!(diff.changed.iter().any(|e| e.kind == "project_doc"));
    }

    #[test]
    fn help_documents_live_mcp_without_deferred_label() {
        let help = context_help();
        assert!(help.contains("--live-mcp"));
        assert!(!help.to_lowercase().contains("deferred"));
        assert!(help.contains("Connect live MCP servers"));
    }
}
