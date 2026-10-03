use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::net::TcpStream;
use tokio::process::Command;

use maestro_local_host::checkpoints::{CheckpointStore, restore_latest};
use maestro_local_host::session::SessionManager;

use crate::http::{RequestHead, json_response, read_request_body};
use crate::{
    AppState, BackgroundSettings, BackgroundUpdateRequest, CommandPrefs, FrameworkUpdateRequest,
    run_git,
};

pub(super) async fn workspace_files(cwd: &Path) -> Vec<String> {
    if let Ok(output) = Command::new("rg")
        .arg("--files")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .await
    {
        if output.status.success() {
            let files = lines_from_output(&output.stdout);
            if !files.is_empty() {
                return files.into_iter().take(2000).collect();
            }
        }
    }

    if let Ok(output) = Command::new("git")
        .args(["ls-files", "--cached", "--others", "--exclude-standard"])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .await
    {
        if output.status.success() {
            return lines_from_output(&output.stdout)
                .into_iter()
                .take(2000)
                .collect();
        }
    }

    if let Ok(output) = Command::new("find")
        .args([
            ".",
            "(",
            "-path",
            "./.git",
            "-o",
            "-path",
            "./node_modules",
            "-o",
            "-path",
            "./dist",
            "-o",
            "-path",
            "./target",
            ")",
            "-prune",
            "-o",
            "-type",
            "f",
            "-print",
        ])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .await
    {
        if output.status.success() {
            return lines_from_output(&output.stdout)
                .into_iter()
                .map(|file| file.trim_start_matches("./").to_string())
                .take(2000)
                .collect();
        }
    }

    Vec::new()
}

fn lines_from_output(output: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(output)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

pub(super) async fn command_catalog(cwd: &Path) -> Vec<Value> {
    let mut commands = Vec::new();
    for dir in [
        maestro_home().join("commands"),
        cwd.join(".maestro/commands"),
    ] {
        let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Ok(raw) = tokio::fs::read_to_string(&path).await else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            if value.get("name").and_then(Value::as_str).is_none()
                || value.get("prompt").and_then(Value::as_str).is_none()
            {
                continue;
            }
            commands.push(serde_json::json!({
                "name": value.get("name").cloned().unwrap_or(Value::Null),
                "description": value.get("description").cloned(),
                "prompt": value.get("prompt").cloned().unwrap_or(Value::Null),
                "args": value.get("args").cloned().unwrap_or_else(|| serde_json::json!([]))
            }));
        }
    }
    commands
}

pub(super) fn maestro_home() -> PathBuf {
    env::var("MAESTRO_HOME")
        .map(PathBuf::from)
        .or_else(|_| env::var("HOME").map(|home| PathBuf::from(home).join(".maestro")))
        .unwrap_or_else(|_| PathBuf::from(".maestro"))
}

fn agent_dir() -> PathBuf {
    env::var("MAESTRO_AGENT_DIR")
        .or_else(|_| env::var("PLAYWRIGHT_AGENT_DIR"))
        .or_else(|_| env::var("CODING_AGENT_DIR"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| maestro_home().join("agent"))
}

pub(super) fn model_config_path() -> String {
    env::var("MAESTRO_MODELS_FILE").unwrap_or_else(|_| {
        maestro_home()
            .join("models.json")
            .to_string_lossy()
            .to_string()
    })
}

pub(super) fn command_prefs_path() -> PathBuf {
    env::var("MAESTRO_COMMAND_PREFS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| agent_dir().join("command-prefs.json"))
}

pub(super) fn default_session_store_path(cwd: &Path) -> PathBuf {
    if let Ok(state_dir) = env::var("MAESTRO_STATE_DIR") {
        return PathBuf::from(state_dir).join("sessions.json");
    }
    if cwd == Path::new("/app") {
        return env::temp_dir().join("maestro/sessions.json");
    }
    PathBuf::from(".maestro/sessions.json")
}

pub(super) fn default_session_messages_path(cwd: &Path) -> PathBuf {
    if let Ok(state_dir) = env::var("MAESTRO_STATE_DIR") {
        return PathBuf::from(state_dir).join("session-messages.json");
    }
    if cwd == Path::new("/app") {
        return env::temp_dir().join("maestro/session-messages.json");
    }
    PathBuf::from(".maestro/session-messages.json")
}

pub(super) fn usage_file_path() -> PathBuf {
    env::var("MAESTRO_USAGE_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| maestro_home().join("usage.json"))
}

pub(super) fn a2a_tasks_file_path() -> PathBuf {
    env::var("MAESTRO_A2A_TASKS_FILE")
        .or_else(|_| env::var("CODEX_A2A_TASKS_FILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| maestro_home().join("a2a/tasks.json"))
}

pub(super) fn automation_file_path() -> PathBuf {
    env::var("MAESTRO_AUTOMATIONS_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| maestro_home().join("automations.json"))
}

pub(super) async fn read_json_value(path: &str) -> Option<Value> {
    let raw = tokio::fs::read_to_string(path).await.ok()?;
    serde_json::from_str(&raw).ok()
}

pub(super) fn contains_forbidden_json_key(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(key, value)| {
            matches!(key.as_str(), "__proto__" | "constructor" | "prototype")
                || contains_forbidden_json_key(value)
        }),
        Value::Array(values) => values.iter().any(contains_forbidden_json_key),
        _ => false,
    }
}

pub(super) async fn load_command_prefs(path: &Path) -> CommandPrefs {
    let Ok(raw) = tokio::fs::read_to_string(path).await else {
        return CommandPrefs::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

pub(super) async fn persist_command_prefs(path: &Path, prefs: &CommandPrefs) {
    if let Some(parent) = path.parent() {
        if tokio::fs::create_dir_all(parent).await.is_err() {
            return;
        }
    }
    if let Ok(bytes) = serde_json::to_vec_pretty(prefs) {
        let _ = tokio::fs::write(path, bytes).await;
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageEntry {
    #[serde(default)]
    provider: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    tokens_input: u64,
    #[serde(default)]
    tokens_output: u64,
    #[serde(default)]
    tokens_cache_read: u64,
    #[serde(default)]
    tokens_cache_write: u64,
    #[serde(default)]
    cost: Option<f64>,
    #[serde(default)]
    cost_source: Option<crate::usage_cost::UsageCostSource>,
}

impl UsageEntry {
    fn known_cost(&self) -> Option<f64> {
        if self.cost_source == Some(crate::usage_cost::UsageCostSource::Unpriced) {
            return None;
        }
        self.cost.filter(|cost| cost.is_finite())
    }

    fn cost_source(&self) -> crate::usage_cost::UsageCostSource {
        if self.known_cost().is_none() {
            crate::usage_cost::UsageCostSource::Unpriced
        } else {
            self.cost_source
                .unwrap_or(crate::usage_cost::UsageCostSource::LegacyRecorded)
        }
    }
}

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct UsageTokenTotals {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    total: u64,
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct UsageBucket {
    // Sum of known costs; counts below describe completeness.
    cost: f64,
    cost_sources: crate::usage_cost::UsageCostCounts,
    known_cost_requests: u64,
    unpriced_requests: u64,
    cost_complete: bool,
    requests: u64,
    tokens: u64,
    tokens_detailed: UsageTokenTotals,
    calls: u64,
    cached_tokens: u64,
}

async fn load_usage_entries(path: &Path) -> Vec<UsageEntry> {
    let Ok(raw) = tokio::fs::read_to_string(path).await else {
        return Vec::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

pub(super) async fn usage_snapshot(path: &Path) -> Value {
    let entries = load_usage_entries(path).await;
    let mut total_cost = 0.0;
    let mut cost_sources = crate::usage_cost::UsageCostCounts::default();
    let mut totals = UsageTokenTotals::default();
    let mut by_provider: HashMap<String, UsageBucket> = HashMap::new();
    let mut by_model: HashMap<String, UsageBucket> = HashMap::new();

    for entry in &entries {
        let tokens = entry.tokens_input
            + entry.tokens_output
            + entry.tokens_cache_read
            + entry.tokens_cache_write;
        total_cost += entry.known_cost().unwrap_or(0.0);
        cost_sources.add(entry.cost_source());
        totals.input += entry.tokens_input;
        totals.output += entry.tokens_output;
        totals.cache_read += entry.tokens_cache_read;
        totals.cache_write += entry.tokens_cache_write;
        totals.total += tokens;

        let provider = if entry.provider.is_empty() {
            "unknown"
        } else {
            &entry.provider
        };
        let provider_bucket = by_provider.entry(provider.to_string()).or_default();
        add_usage_to_bucket(provider_bucket, tokens, entry);

        let model = if entry.model.is_empty() {
            "unknown"
        } else {
            &entry.model
        };
        let model_bucket = by_model.entry(format!("{provider}/{model}")).or_default();
        add_usage_to_bucket(model_bucket, tokens, entry);
    }

    serde_json::json!({
        "summary": {
            "totalCost": total_cost,
            "knownCostRequests": cost_sources.known(),
            "unpricedRequests": cost_sources.unpriced(),
            "costComplete": cost_sources.unpriced() == 0,
            "costSources": cost_sources,
            "totalRequests": entries.len(),
            "totalTokens": totals.total,
            "tokensDetailed": totals,
            "totalTokensDetailed": totals,
            "totalTokensBreakdown": totals,
            "totalCachedTokens": totals.cache_read + totals.cache_write,
            "byProvider": by_provider,
            "byModel": by_model
        },
        "hasData": !entries.is_empty()
    })
}

fn add_usage_to_bucket(bucket: &mut UsageBucket, tokens: u64, entry: &UsageEntry) {
    bucket.cost += entry.known_cost().unwrap_or(0.0);
    bucket.cost_sources.add(entry.cost_source());
    bucket.known_cost_requests = bucket.cost_sources.known();
    bucket.unpriced_requests = bucket.cost_sources.unpriced();
    bucket.cost_complete = bucket.unpriced_requests == 0;
    bucket.requests += 1;
    bucket.tokens += tokens;
    bucket.calls += 1;
    bucket.cached_tokens += entry.tokens_cache_read + entry.tokens_cache_write;
    bucket.tokens_detailed.input += entry.tokens_input;
    bucket.tokens_detailed.output += entry.tokens_output;
    bucket.tokens_detailed.cache_read += entry.tokens_cache_read;
    bucket.tokens_detailed.cache_write += entry.tokens_cache_write;
    bucket.tokens_detailed.total += tokens;
}

#[cfg(test)]
#[path = "local_usage_tests.rs"]
mod local_usage_tests;

pub(super) async fn package_scripts(cwd: &Path) -> Vec<String> {
    let allowlist = allowed_run_scripts();
    let mut scripts: Vec<String> = package_script_map(cwd)
        .await
        .into_keys()
        .filter(|script| allowlist.contains(script))
        .collect();
    scripts.sort();
    scripts
}

async fn package_script_map(cwd: &Path) -> HashMap<String, String> {
    let package_json = cwd.join("package.json");
    let Some(value) = read_json_value(&package_json.to_string_lossy()).await else {
        return HashMap::new();
    };
    value
        .get("scripts")
        .and_then(Value::as_object)
        .map(|scripts| {
            scripts
                .iter()
                .filter_map(|(name, command)| {
                    command
                        .as_str()
                        .map(|command| (name.to_string(), command.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RunScriptRequest {
    script: String,
    args: Option<String>,
}

fn allowed_run_scripts() -> HashSet<String> {
    let configured =
        env::var("MAESTRO_RUN_SCRIPT_ALLOWLIST").unwrap_or_else(|_| "db:migrate".to_string());
    configured
        .split(',')
        .map(str::trim)
        .filter(|script| !script.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

pub(super) fn runner_args_for_script(runner: &str, script: &str) -> Vec<String> {
    let mut args = Vec::new();
    if runner_supports_ignore_scripts(runner) {
        args.push("--ignore-scripts".to_string());
    }
    args.push("run".to_string());
    args.push(script.to_string());
    args
}

pub(super) fn runner_supports_ignore_scripts(runner: &str) -> bool {
    let command_name = runner.trim().rsplit(['/', '\\']).next().unwrap_or(runner);
    let command_name = command_name.to_ascii_lowercase();
    matches!(command_name.as_str(), "npm" | "npm.cmd")
}

pub(super) async fn run_script_response(cwd: &Path, request: RunScriptRequest) -> Vec<u8> {
    let script = request.script.trim();
    if script.is_empty() {
        return json_response(
            400,
            &serde_json::json!({ "error": "Script name is required" }),
        );
    }
    if !is_valid_script_name(script) {
        return json_response(
            400,
            &serde_json::json!({ "error": "Invalid script name format" }),
        );
    }

    let allowlist = allowed_run_scripts();
    if !allowlist.contains(script) {
        let mut allowed: Vec<String> = allowlist.iter().cloned().collect();
        allowed.sort();
        return json_response(
            403,
            &serde_json::json!({
                "error": format!("Script \"{script}\" is not allowed in this environment"),
                "allowed": allowed,
            }),
        );
    }

    let available_scripts = package_script_map(cwd).await;
    if !available_scripts.contains_key(script) {
        let mut available: Vec<String> = available_scripts
            .keys()
            .filter(|name| allowlist.contains(*name))
            .cloned()
            .collect();
        available.sort();
        return json_response(
            400,
            &serde_json::json!({
                "error": format!("Script \"{script}\" not found in package.json"),
                "available": available,
            }),
        );
    }

    let args = request.args.unwrap_or_default();
    if contains_shell_metachars(&args) {
        return json_response(
            400,
            &serde_json::json!({
                "error": "Arguments contain invalid characters. Shell metacharacters are not allowed."
            }),
        );
    }

    let Some(runner) = script_runner_command().await else {
        return json_response(
            503,
            &serde_json::json!({
                "error": "No JavaScript package runner with lifecycle suppression is available for /api/run. Install npm or set MAESTRO_SCRIPT_RUNNER to an npm-compatible runner."
            }),
        );
    };

    let args = args.trim();
    let mut command = Command::new(&runner);
    command.args(runner_args_for_script(&runner, script));
    if !args.is_empty() {
        command.arg("--");
        command.args(args.split_whitespace());
    }

    match command.current_dir(cwd).stdin(Stdio::null()).output().await {
        Ok(output) => json_response(
            200,
            &serde_json::json!({
                "success": output.status.success(),
                "exitCode": output.status.code().unwrap_or(1),
                "stdout": String::from_utf8_lossy(&output.stdout),
                "stderr": String::from_utf8_lossy(&output.stderr),
                "command": script_run_display(&runner, script, args),
            }),
        ),
        Err(error) => json_response(
            500,
            &serde_json::json!({ "error": format!("failed to run script: {error}") }),
        ),
    }
}

pub(super) async fn approval_mode_response(head: &RequestHead, state: &AppState) -> Vec<u8> {
    let session_id = approval_session_id(head);
    let mode = state
        .approval_modes
        .lock()
        .await
        .get(&session_id)
        .cloned()
        .unwrap_or_else(default_approval_mode);
    json_response(
        200,
        &serde_json::json!({
            "mode": mode,
            "availableModes": ["auto", "prompt", "fail"]
        }),
    )
}

pub(super) async fn set_approval_mode_response(
    stream: &mut TcpStream,
    initial: &mut Vec<u8>,
    head: &RequestHead,
    state: &AppState,
) -> Vec<u8> {
    let body = match read_request_body(stream, initial, head).await {
        Ok(body) => body,
        Err(error) => return json_response(400, &serde_json::json!({ "error": error })),
    };
    let payload = if body.is_empty() {
        Value::Object(serde_json::Map::new())
    } else {
        match serde_json::from_slice::<Value>(&body) {
            Ok(value) if value.is_object() => value,
            Ok(_) => {
                return json_response(
                    400,
                    &serde_json::json!({ "error": "approval payload must be an object" }),
                );
            }
            Err(error) => {
                return json_response(
                    400,
                    &serde_json::json!({ "error": format!("invalid approval request: {error}") }),
                );
            }
        }
    };
    let Some(mode) = payload
        .get("mode")
        .and_then(Value::as_str)
        .filter(|mode| matches!(*mode, "auto" | "prompt" | "fail"))
    else {
        return json_response(
            400,
            &serde_json::json!({ "error": "mode must be auto, prompt, or fail" }),
        );
    };
    let session_id = payload
        .get("sessionId")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| approval_session_id(head));
    state
        .approval_modes
        .lock()
        .await
        .insert(session_id, mode.to_string());
    json_response(
        200,
        &serde_json::json!({
            "success": true,
            "mode": mode,
            "message": format!("Approval mode set to {mode}")
        }),
    )
}

fn approval_session_id(head: &RequestHead) -> String {
    head.query
        .get("sessionId")
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .unwrap_or_else(|| "default".to_string())
}

fn default_approval_mode() -> String {
    env::var("MAESTRO_APPROVAL_MODE")
        .ok()
        .filter(|mode| matches!(mode.as_str(), "auto" | "prompt" | "fail"))
        .unwrap_or_else(|| "prompt".to_string())
}

pub(super) async fn approval_mode_for_session(
    state: &AppState,
    session_id: Option<&str>,
) -> String {
    let key = session_id.unwrap_or("default");
    state
        .approval_modes
        .lock()
        .await
        .get(key)
        .cloned()
        .unwrap_or_else(default_approval_mode)
}

async fn script_runner_command() -> Option<String> {
    if let Ok(runner) = env::var("MAESTRO_SCRIPT_RUNNER") {
        let runner = runner.trim();
        if !runner.is_empty() {
            return runner_supports_ignore_scripts(runner).then(|| runner.to_string());
        }
    }
    if executable_on_path("npm").await {
        return Some("npm".to_string());
    }
    None
}

async fn executable_on_path(name: &str) -> bool {
    Command::new("sh")
        .arg("-lc")
        .arg(format!("command -v {name} >/dev/null 2>&1"))
        .stdin(Stdio::null())
        .output()
        .await
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn script_run_display(runner: &str, script: &str, args: &str) -> String {
    if args.is_empty() {
        format!("{runner} run {script}")
    } else {
        format!("{runner} run {script} -- {args}")
    }
}

pub(super) fn is_valid_script_name(script: &str) -> bool {
    script.len() <= 100
        && !script.is_empty()
        && script
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | ':' | '.' | '-'))
}

pub(super) fn contains_shell_metachars(value: &str) -> bool {
    value.chars().any(|ch| {
        matches!(
            ch,
            ';' | '&'
                | '|'
                | '`'
                | '$'
                | '('
                | ')'
                | '{'
                | '}'
                | '['
                | ']'
                | '<'
                | '>'
                | '\\'
                | '!'
                | '#'
                | '*'
                | '?'
                | '"'
                | '\''
                | '\n'
                | '\r'
                | '\t'
        )
    })
}

pub(super) fn background_response(head: &RequestHead, settings: &BackgroundSettings) -> Value {
    match head.query.get("action").map(String::as_str) {
        Some("history") => serde_json::json!({ "history": [], "truncated": false }),
        Some("path") => serde_json::json!({
            "path": maestro_home().join("background-tasks.jsonl").to_string_lossy(),
            "exists": false,
            "overridden": env::var("MAESTRO_BACKGROUND_TASKS_FILE").is_ok()
        }),
        _ => serde_json::json!({
            "settings": settings,
            "snapshot": {
                "running": 0,
                "total": 0,
                "failed": 0,
                "detailsRedacted": true
            }
        }),
    }
}

pub(super) async fn update_background_response(
    stream: &mut TcpStream,
    initial: &mut Vec<u8>,
    head: &RequestHead,
    state: &AppState,
) -> Vec<u8> {
    let action = match head.query.get("action").map(String::as_str) {
        Some("notify") => "notify",
        Some("details") => "details",
        Some(action) => {
            return json_response(
                400,
                &serde_json::json!({ "error": format!("unsupported background action \"{action}\"") }),
            );
        }
        None => {
            return json_response(
                400,
                &serde_json::json!({ "error": "background action is required" }),
            );
        }
    };
    let body = match read_request_body(stream, initial, head).await {
        Ok(body) => body,
        Err(error) => return json_response(400, &serde_json::json!({ "error": error })),
    };
    let request = match serde_json::from_slice::<BackgroundUpdateRequest>(&body) {
        Ok(request) => request,
        Err(error) => {
            return json_response(
                400,
                &serde_json::json!({ "error": format!("invalid background update request: {error}") }),
            );
        }
    };

    let mut settings = state.background_settings.lock().await;
    let message = match action {
        "notify" => {
            settings.notifications_enabled = request.enabled;
            format!(
                "Background task notifications {}.",
                if request.enabled {
                    "enabled"
                } else {
                    "disabled"
                }
            )
        }
        "details" => {
            settings.status_details_enabled = request.enabled;
            format!(
                "Background task details {}.",
                if request.enabled {
                    "enabled"
                } else {
                    "disabled"
                }
            )
        }
        _ => unreachable!("background action was validated"),
    };
    json_response(
        200,
        &serde_json::json!({ "success": true, "message": message }),
    )
}

pub(super) fn undo_response(head: &RequestHead, cwd: &Path) -> Value {
    let Some(store) = checkpoint_store_for_request(head, cwd) else {
        return match head.query.get("action").map(String::as_str) {
            Some("history") => serde_json::json!({ "history": [] }),
            _ => serde_json::json!({
                "totalChanges": 0,
                "canUndo": false,
                "checkpoints": []
            }),
        };
    };
    undo_response_for_store(head, &store)
}

pub(super) fn undo_response_for_store(head: &RequestHead, store: &CheckpointStore) -> Value {
    match head.query.get("action").map(String::as_str) {
        Some("history") => serde_json::json!({ "history": [] }),
        _ => {
            let checkpoints = store.list();
            let total_changes = checkpoints
                .iter()
                .map(|checkpoint| checkpoint.entries.len())
                .sum::<usize>();
            serde_json::json!({
                "totalChanges": total_changes,
                "canUndo": !checkpoints.is_empty(),
                "checkpoints": checkpoints.into_iter().map(|checkpoint| {
                    serde_json::json!({
                        "id": checkpoint.id,
                        "createdAt": checkpoint.created_at,
                        "prompt": checkpoint.prompt,
                        "changedFiles": checkpoint.entries.len(),
                        "canUndo": true
                    })
                }).collect::<Vec<_>>()
            })
        }
    }
}

pub(super) fn restore_undo_response(head: &RequestHead, cwd: &Path) -> Value {
    let Some(store) = checkpoint_store_for_request(head, cwd) else {
        return serde_json::json!({
            "success": false,
            "message": "No session checkpoint is available to undo.",
            "changedFiles": []
        });
    };
    restore_undo_response_for_store(&store)
}

pub(super) fn restore_undo_response_for_store(store: &CheckpointStore) -> Value {
    match restore_latest(store) {
        Ok(Some(report)) => {
            let mut changed_files = report.restored;
            changed_files.extend(report.deleted);
            serde_json::json!({
                "success": report.failed.is_empty(),
                "message": if report.failed.is_empty() {
                    format!("Restored files from checkpoint {}.", report.checkpoint_id)
                } else {
                    format!("Some files could not be restored from checkpoint {}.", report.checkpoint_id)
                },
                "checkpointId": report.checkpoint_id,
                "changedFiles": changed_files,
                "skippedFiles": report.skipped,
                "goneFiles": report.gone,
                "failedFiles": report.failed
            })
        }
        Ok(None) => serde_json::json!({
            "success": false,
            "message": "No session checkpoint is available to undo.",
            "changedFiles": []
        }),
        Err(error) => serde_json::json!({
            "success": false,
            "message": format!("Failed to restore checkpoint: {error}"),
            "changedFiles": []
        }),
    }
}

fn checkpoint_store_for_request(head: &RequestHead, cwd: &Path) -> Option<CheckpointStore> {
    let manager = SessionManager::new(cwd.to_string_lossy().to_string());
    let session_id = head
        .query
        .get("sessionId")
        .or_else(|| head.query.get("session_id"))
        .cloned()
        .or_else(|| {
            manager
                .list_sessions()
                .ok()?
                .first()
                .map(|session| session.id.clone())
        })?;
    Some(CheckpointStore::new(manager.sessions_dir(), &session_id))
}

pub(super) async fn changes_snapshot(cwd: &Path) -> Value {
    let output = run_git(cwd, &["status", "--porcelain"])
        .await
        .unwrap_or_default();
    let files: Vec<Value> = output
        .lines()
        .filter(|line| line.len() > 3)
        .map(|line| {
            serde_json::json!({
                "path": line[3..].trim(),
                "status": line[..2].trim()
            })
        })
        .collect();
    let total = files.len();
    serde_json::json!({ "files": files, "tools": [], "total": total })
}

struct FrameworkInfo {
    id: &'static str,
    summary: &'static str,
}

const FRAMEWORKS: &[FrameworkInfo] = &[
    FrameworkInfo {
        id: "express",
        summary: "Preferred framework: Express.js on Node 20. Use TypeScript, zod for validation, vitest, and supertest for HTTP tests.",
    },
    FrameworkInfo {
        id: "fastapi",
        summary: "Preferred framework: FastAPI on Python 3.12. Use pydantic v2, uvicorn, pytest, httpx for tests, and typed routers.",
    },
    FrameworkInfo {
        id: "node",
        summary: "Preferred framework: Generic Node.js on TypeScript/Node 20. Use zod for validation, vitest for unit tests, supertest for HTTP, and eslint/biome for linting.",
    },
];

pub(super) fn framework_response(head: &RequestHead, current: Option<&str>) -> Value {
    match head.query.get("action").map(String::as_str) {
        Some("list") => serde_json::json!({
            "frameworks": FRAMEWORKS
                .iter()
                .map(|info| serde_json::json!({ "id": info.id, "summary": info.summary }))
                .collect::<Vec<_>>()
        }),
        _ => serde_json::json!({
            "framework": current.unwrap_or("none"),
            "source": "rust-control-plane",
            "locked": false,
            "scope": framework_scope(head.query.get("scope").map(String::as_str).unwrap_or("user")).unwrap_or("user")
        }),
    }
}

pub(super) async fn update_framework_response(
    stream: &mut TcpStream,
    initial: &mut Vec<u8>,
    head: &RequestHead,
    state: &AppState,
) -> Vec<u8> {
    let body = match read_request_body(stream, initial, head).await {
        Ok(body) => body,
        Err(error) => return json_response(400, &serde_json::json!({ "error": error })),
    };
    let request = match serde_json::from_slice::<FrameworkUpdateRequest>(&body) {
        Ok(request) => request,
        Err(error) => {
            return json_response(
                400,
                &serde_json::json!({ "error": format!("invalid framework update request: {error}") }),
            );
        }
    };
    let scope = match request.scope.as_deref().map(framework_scope).transpose() {
        Ok(scope) => scope.unwrap_or("user"),
        Err(error) => return json_response(400, &serde_json::json!({ "error": error })),
    };
    let Some(raw_framework) = request.framework else {
        return json_response(
            400,
            &serde_json::json!({ "error": "framework is required" }),
        );
    };
    let normalized = raw_framework.and_then(normalize_framework_id);
    if normalized.is_none() {
        *state.framework_preference.lock().await = None;
        return json_response(
            200,
            &serde_json::json!({
                "success": true,
                "message": format!("Default framework cleared for {scope} scope"),
                "framework": Value::Null,
                "scope": scope
            }),
        );
    }
    let framework = normalized.expect("framework none case returned");
    let Some(info) = framework_info(&framework) else {
        let available = FRAMEWORKS
            .iter()
            .map(|info| info.id)
            .collect::<Vec<_>>()
            .join(", ");
        return json_response(
            400,
            &serde_json::json!({ "error": format!("Unknown framework \"{framework}\". Available options: {available}") }),
        );
    };
    *state.framework_preference.lock().await = Some(info.id.to_string());
    json_response(
        200,
        &serde_json::json!({
            "success": true,
            "framework": info.id,
            "summary": info.summary,
            "scope": scope,
            "message": format!("{} (scope: {scope})", info.summary)
        }),
    )
}

fn framework_scope(scope: &str) -> Result<&'static str, String> {
    match scope {
        "user" => Ok("user"),
        "workspace" => Ok("workspace"),
        other => Err(format!("unsupported framework scope \"{other}\"")),
    }
}

fn normalize_framework_id(value: String) -> Option<String> {
    let normalized = value.trim().to_lowercase();
    if normalized.is_empty() || matches!(normalized.as_str(), "none" | "off") {
        None
    } else {
        Some(normalized)
    }
}

fn framework_info(id: &str) -> Option<&'static FrameworkInfo> {
    FRAMEWORKS.iter().find(|info| info.id == id)
}

pub(super) fn telemetry_status(override_value: Option<bool>) -> Value {
    let flag = env::var("MAESTRO_TELEMETRY")
        .or_else(|_| env::var("PLAYWRIGHT_TELEMETRY"))
        .ok();
    let endpoint = env::var("MAESTRO_TELEMETRY_ENDPOINT")
        .or_else(|_| env::var("PLAYWRIGHT_TELEMETRY_ENDPOINT"))
        .ok();
    let file_path = env::var("MAESTRO_TELEMETRY_FILE")
        .or_else(|_| env::var("PLAYWRIGHT_TELEMETRY_FILE"))
        .unwrap_or_else(|_| {
            maestro_home()
                .join("telemetry.jsonl")
                .to_string_lossy()
                .to_string()
        });
    let file_configured =
        env::var("MAESTRO_TELEMETRY_FILE").is_ok() || env::var("PLAYWRIGHT_TELEMETRY_FILE").is_ok();
    let enabled = telemetry_enabled(
        override_value,
        flag.as_deref(),
        endpoint.is_some(),
        file_configured,
    );
    serde_json::json!({
        "enabled": enabled,
        "reason": if override_value.is_some() { "runtime override" } else if enabled { "configured" } else { "disabled" },
        "endpoint": endpoint,
        "filePath": file_path,
        "sampleRate": 1,
        "flagValue": flag,
        "runtimeOverride": override_value.map(|enabled| if enabled { "enabled" } else { "disabled" })
    })
}

pub(super) fn telemetry_enabled(
    override_value: Option<bool>,
    flag: Option<&str>,
    endpoint_configured: bool,
    file_configured: bool,
) -> bool {
    override_value
        .or_else(|| parse_bool_flag(flag))
        .unwrap_or(endpoint_configured || file_configured)
}

pub(super) fn training_status(override_value: Option<bool>) -> Value {
    let flag = env::var("MAESTRO_TRAINING_OPT_OUT").ok();
    let opt_out = override_value.or_else(|| parse_bool_flag(flag.as_deref()));
    let preference = match opt_out {
        Some(true) => "opted-out",
        Some(false) => "opted-in",
        None => "provider-default",
    };
    serde_json::json!({
        "preference": preference,
        "optOut": opt_out,
        "reason": if override_value.is_some() { "runtime override" } else if flag.is_some() { "MAESTRO_TRAINING_OPT_OUT" } else { "provider default" },
        "flagValue": flag,
        "runtimeOverride": override_value.map(|opt_out| if opt_out { "opted-out" } else { "opted-in" })
    })
}

fn parse_bool_flag(value: Option<&str>) -> Option<bool> {
    match value?.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

pub(super) async fn read_required_action(
    stream: &mut TcpStream,
    initial: &mut Vec<u8>,
    head: &RequestHead,
    valid_actions: &[&str],
) -> Result<String, Vec<u8>> {
    let body = read_request_body(stream, initial, head)
        .await
        .map_err(|error| json_response(400, &serde_json::json!({ "error": error })))?;
    parse_action_body(&body, valid_actions)
        .map_err(|error| json_response(400, &serde_json::json!({ "error": error })))
}

pub(super) fn parse_action_body(body: &[u8], valid_actions: &[&str]) -> Result<String, String> {
    let payload = serde_json::from_slice::<Value>(body)
        .map_err(|error| format!("invalid action request: {error}"))?;
    let action = payload
        .get("action")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|action| !action.is_empty())
        .ok_or_else(|| "action is required".to_string())?;
    if !valid_actions.contains(&action) {
        return Err(format!(
            "invalid action \"{action}\". Expected one of: {}",
            valid_actions.join(", ")
        ));
    }
    Ok(action.to_string())
}
