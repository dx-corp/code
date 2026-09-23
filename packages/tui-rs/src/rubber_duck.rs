//! Rubber duck review: a second-opinion review of uncommitted changes by a
//! model *different* from the one active in the session.
//!
//! `/rubber-duck [model]` gathers the staged + unstaged diff (plus untracked
//! file contents), spawns a fresh read-only agent in a background task, and
//! delivers the finished review back to the App event loop over a channel so
//! the UI never blocks and the session's model/conversation stay untouched.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::process::CommandExt;

use anyhow::{Context, Result};

use crate::agent::{
    CredentialVault, ExecutionSource, ExternalToolSchemaPolicy, FromAgent, NativeAgent,
    NativeAgentConfig,
};
use crate::ai::{AiProvider, provider_model_name};
use crate::model_catalog::{
    VerificationState, available_models, model_route, verify_model_offline,
};
use crate::tools::ToolExecutor;

/// Read-only tools the reviewer may use to inspect surrounding code.
const REVIEW_TOOLS: &[&str] = &[
    "read", "glob", "grep", "list", "search", "find", "diff", "status",
];

/// Hard cap on a single review run so a hung provider can't run forever.
const REVIEW_TIMEOUT: Duration = Duration::from_mins(5);

/// Cap on the diff embedded in the review prompt.
const MAX_DIFF_CHARS: usize = 20_000;

/// Cap on bytes collected from a single git command's stdout. Bounded at
/// collection time (a bit above [`MAX_DIFF_CHARS`] so [`truncate_diff`] still
/// appends its truncation marker) instead of buffering an unbounded diff in
/// memory in the TUI command handler.
const MAX_GIT_OUTPUT_BYTES: u64 = (MAX_DIFF_CHARS + 4_096) as u64;

/// Wall-clock bound for local Git inspection. Diff drivers and hooks are
/// external processes and must not be allowed to stall a review forever.
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// Cap on a single untracked file's contents in the review input.
const MAX_UNTRACKED_FILE_CHARS: usize = 4_000;

/// Cap on how many untracked files are included in the review input.
const MAX_UNTRACKED_FILES: usize = 20;

/// Cap on how many untracked entries are *scanned* while filling the include
/// quota, so a huge listing of skipped entries can't loop forever.
const MAX_UNTRACKED_SCANNED: usize = MAX_UNTRACKED_FILES * 10;

/// Result of a background rubber duck review, delivered to the App event loop.
#[derive(Debug)]
pub enum RubberDuckEvent {
    /// The review finished; `review` is the reviewer's final assistant text.
    Completed { model: String, review: String },
    /// The review could not be produced (agent error, timeout, no output).
    Failed { model: String, message: String },
}

/// Choose the model that performs the review.
///
/// An explicit `requested` model is used as-is, but must differ from
/// `current` -- reviewing with the same model defeats the purpose. Without a
/// request, prefer the first authenticated catalog model from a different
/// provider than the current model, falling back to any authenticated model
/// that isn't the current one.
pub fn pick_review_model(current: &str, requested: Option<&str>) -> Result<String, String> {
    if let Some(requested) = requested.map(str::trim).filter(|m| !m.is_empty()) {
        if same_model(requested, current) {
            return Err(format!(
                "Rubber duck review needs a different model than the current one ({current})."
            ));
        }
        return Ok(requested.to_string());
    }

    let models = available_models();
    // Auto-pick only models whose provider credentials are actually
    // configured; anything else would just fail when the review starts.
    let usable = |id: &str| {
        let verification = verify_model_offline(id);
        verification.state == VerificationState::Verified
            && verification.source != "provider-registry"
    };
    if let Some(route) =
        pick_automatic_review_model(current, models.iter().map(model_route), usable)
    {
        return Ok(route);
    }
    Err(
        "No authenticated alternative model available for a rubber duck review. \
         Configure credentials for another provider or pass a model explicitly: /rubber-duck <model>."
            .to_string(),
    )
}

fn pick_automatic_review_model(
    current: &str,
    routes: impl IntoIterator<Item = String>,
    usable: impl Fn(&str) -> bool,
) -> Option<String> {
    let current_provider = AiProvider::from_model(current);
    let routes: Vec<_> = routes.into_iter().collect();
    routes
        .iter()
        .find(|route| {
            !same_model(route, current)
                && AiProvider::from_model(route) != current_provider
                && usable(route)
        })
        .cloned()
        .or_else(|| {
            routes
                .iter()
                .find(|route| !same_model(route, current) && usable(route))
                .cloned()
        })
}

/// Canonical provider/model identity comparison, so provider-qualified
/// aliases (`openai/gpt-5.5` vs `gpt-5.5`) count as the same model.
fn same_model(a: &str, b: &str) -> bool {
    effective_provider(a) == effective_provider(b)
        && canonical_model_name(a).eq_ignore_ascii_case(&canonical_model_name(b))
}

/// Resolve the provider that will actually own a routed model.
///
/// OpenRouter is an OpenAI-compatible transport, but its model ids may carry
/// an underlying vendor namespace (`openrouter/anthropic/...`).  Comparing the
/// transport provider alone would incorrectly treat that route as independent
/// from a direct Anthropic model.  Managed routes keep the same semantics after
/// their gateway namespace is removed.
fn effective_provider(model: &str) -> AiProvider {
    let mut normalized = model.trim();
    for prefix in ["evalops/", "maestro-managed/"] {
        if normalized
            .get(..prefix.len())
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix))
        {
            normalized = normalized[prefix.len()..].trim();
            break;
        }
    }

    if let Some((route, model_id)) = normalized.split_once('/') {
        if route.eq_ignore_ascii_case("openrouter") && !model_id.trim().is_empty() {
            return AiProvider::from_model(model_id);
        }
    }

    AiProvider::from_model(normalized)
}

fn canonical_model_name(model: &str) -> String {
    let normalized = provider_model_name(model);
    let without_openrouter = normalized
        .split_once('/')
        .filter(|(prefix, model_id)| {
            prefix.eq_ignore_ascii_case("openrouter") && !model_id.trim().is_empty()
        })
        .map_or(normalized.as_str(), |(_, model_id)| model_id.trim());
    provider_model_name(without_openrouter)
}

/// Build the prompt handed to the reviewing agent.
pub fn build_review_prompt(current_model: &str, diff: &str) -> String {
    format!(
        r#"You are reviewing uncommitted code changes as a "rubber duck" second opinion. The changes were authored with the help of a different AI model ({current_model}); your job is to catch what it may have missed.

Hunt for:
- Bugs and logic errors
- Security issues
- Regressions or unintended behavior changes
- Missing edge-case handling

Report findings ordered by severity, most severe first. For each finding give a short title, a file:line reference where possible, and a one- or two-sentence explanation. Be concise. If the changes look sound, say so plainly instead of inventing issues. Do not modify any files.

```diff
{diff}
```
"#
    )
}

/// Diff of staged + unstaged changes against HEAD (`git diff HEAD`), plus the
/// contents of untracked files, which `git diff` never shows.
///
/// Falls back to concatenating the staged and unstaged diffs when HEAD does
/// not exist yet (a fresh repo with no commits).
pub fn uncommitted_diff(cwd: &Path) -> Result<String, String> {
    let tracked = match run_git(cwd, &["diff", "HEAD"]) {
        Ok(diff) => diff,
        Err(diff_error) => {
            // Only an unborn repository may use the staged/unstaged
            // fallback. If HEAD exists, the original diff failure (for
            // example a corrupt index) must remain visible to the caller.
            if repository_has_head(cwd)? {
                return Err(diff_error);
            }
            let staged = run_git(cwd, &["diff", "--cached"])?;
            let unstaged = run_git(cwd, &["diff"])?;
            format!("{staged}\n{unstaged}").trim().to_string()
        }
    };
    let untracked = untracked_files_section(cwd);
    Ok(format!("{tracked}\n{untracked}").trim().to_string())
}

/// Distinguish a normal unborn repository from a broken repository. With
/// `--quiet`, rev-parse exits nonzero without stderr only when the requested
/// revision does not exist; repository errors still carry diagnostic text.
fn repository_has_head(cwd: &Path) -> Result<bool, String> {
    const ARGS: &[&str] = &["rev-parse", "--verify", "--quiet", "HEAD"];
    match run_git(cwd, ARGS) {
        Ok(_) => Ok(true),
        Err(error) if error == "git rev-parse --verify --quiet HEAD failed" => Ok(false),
        Err(error) => Err(error),
    }
}

/// Format the contents of untracked (and not ignored) files for the review
/// input, so an untracked-only changeset still gets reviewed. Binary files
/// are skipped, symlinks resolving outside the workspace are skipped, and
/// per-file contents and the file count are capped (the caller's
/// [`truncate_diff`] still bounds the overall size).
fn untracked_files_section(cwd: &Path) -> String {
    // `-z` gives raw NUL-delimited pathnames; the default C-quoted display
    // form would silently drop names containing tabs/newlines/special chars.
    // Use the untrimmed output so a first filename with leading whitespace
    // survives intact.
    let Ok(listing) =
        run_git_raw_bounded(cwd, &["ls-files", "-z", "--others", "--exclude-standard"])
    else {
        return String::new();
    };
    // A complete `-z` listing ends with NUL. Without it the bounded reader
    // cut the output mid-record, so drop the incomplete trailing record.
    let complete = !listing.truncated && listing.bytes.ends_with(&[0]);
    let mut paths: Vec<&[u8]> = listing
        .bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .collect();
    if !complete {
        paths.pop();
    }
    let total = paths.len();

    let workspace = dunce::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let mut section = String::new();
    let mut included = 0usize;
    let mut scanned = 0usize;
    for raw_path in paths {
        // The cap counts files actually included, so leading binary/
        // unreadable/escaping-symlink entries can't consume the quota; the
        // scan itself stays bounded against a huge junk listing.
        if included >= MAX_UNTRACKED_FILES || scanned >= MAX_UNTRACKED_SCANNED {
            break;
        }
        scanned += 1;
        // Canonicalize and require containment before reading: an untracked
        // symlink pointing outside the repo (or a loop/missing target) must
        // not leak its target's contents into the review prompt.
        let path = path_from_git_bytes(raw_path);
        let candidate = cwd.join(&path);
        let Ok(canonical) = dunce::canonicalize(&candidate) else {
            continue;
        };
        if !canonical.starts_with(&workspace) {
            continue;
        }
        // Read only a bounded prefix (one byte past the per-file cap, so the
        // truncation marker below still fires) instead of the whole file.
        let mut bytes = Vec::new();
        let read_result = std::fs::File::open(&canonical).and_then(|file| {
            file.take(MAX_UNTRACKED_FILE_CHARS as u64 + 1)
                .read_to_end(&mut bytes)
        });
        let Ok(_) = read_result else {
            continue;
        };
        // Skip binary files; their contents are noise for a text review.
        if bytes.iter().take(8_192).any(|byte| *byte == 0) {
            continue;
        }
        let truncated = bytes.len() > MAX_UNTRACKED_FILE_CHARS;
        let mut content =
            truncate_chars(&String::from_utf8_lossy(&bytes), MAX_UNTRACKED_FILE_CHARS);
        if truncated {
            content.push_str(&format!(
                "\n... (file truncated to {MAX_UNTRACKED_FILE_CHARS} characters)"
            ));
        }
        section.push_str(&format!(
            "\n## Untracked file: {}\n\n```\n{content}\n```\n",
            path.to_string_lossy()
        ));
        included += 1;
    }
    let omitted = total - scanned;
    if listing.truncated {
        section.push_str("\n... (additional untracked files omitted)\n");
    } else if omitted > 0 {
        section.push_str(&format!("\n... ({omitted} more untracked files omitted)\n"));
    }
    section.trim().to_string()
}

#[cfg(unix)]
fn path_from_git_bytes(bytes: &[u8]) -> PathBuf {
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
fn path_from_git_bytes(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

/// Truncate an over-long diff for the review prompt.
pub fn truncate_diff(diff: &str) -> String {
    if diff.len() <= MAX_DIFF_CHARS {
        return diff.to_string();
    }
    format!(
        "{}\n\n... (diff truncated to {MAX_DIFF_CHARS} characters)",
        truncate_chars(diff, MAX_DIFF_CHARS)
    )
}

/// Truncate `text` to at most `max` bytes on a char boundary.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// Run the review in the background and report the outcome on `tx`.
///
/// Never panics: every failure path ends in a `Failed` event.
pub async fn run_review(
    model: String,
    cwd: String,
    current_model: String,
    tx: std::sync::mpsc::Sender<RubberDuckEvent>,
) {
    // Git diff drivers are external processes. Gather the input off the TUI
    // event loop so even a slow repository cannot freeze keyboard handling.
    let gather_cwd = cwd.clone();
    let gathered =
        tokio::task::spawn_blocking(move || uncommitted_diff(Path::new(&gather_cwd))).await;
    let event = match gathered {
        Ok(Ok(diff)) if diff.is_empty() => RubberDuckEvent::Failed {
            model,
            message: "No uncommitted changes to review.".to_string(),
        },
        Ok(Ok(diff)) => {
            let prompt = build_review_prompt(&current_model, &truncate_diff(&diff));
            match drive_review(&model, &cwd, &prompt).await {
                Ok(review) => RubberDuckEvent::Completed { model, review },
                Err(err) => RubberDuckEvent::Failed {
                    model,
                    message: format!("{err:#}"),
                },
            }
        }
        Ok(Err(message)) => RubberDuckEvent::Failed { model, message },
        Err(err) => RubberDuckEvent::Failed {
            model,
            message: format!("Failed to gather changes: {err}"),
        },
    };
    let _ = tx.send(event);
}

/// Drive the review agent to completion and return its final assistant text.
///
/// Mirrors the print-mode event loop (`print_mode::run_print_mode`), minus
/// stdout printing and exit codes.
async fn drive_review(model: &str, cwd: &str, prompt: &str) -> Result<String> {
    let config = NativeAgentConfig {
        model_capabilities: None,
        model_dynamics: crate::config::model_dynamics_config(),
        model: model.to_string(),
        max_tokens: crate::model_catalog::default_max_output_tokens(model),
        max_tokens_source: crate::agent::MaxTokensSource::Catalog,
        system_prompt: Some(format!(
            "You are a senior code reviewer giving a second opinion on uncommitted changes. Working directory: {cwd}. Be concise. Your tools are read-only; never modify anything."
        )),
        thinking_enabled: false,
        thinking_budget: 0,
        cwd: cwd.to_string(),
        approval_mode: crate::state::ApprovalMode::Selective,
        context_window: None,
        // Read-only review: no workspace writes needed; leave sandboxed off so
        // the second-opinion agent matches print-mode / headless default until
        // a caller chooses an explicit policy.
        sandbox_policy: None,
        managed_mcp_policy: None,
        external_tool_schema_policy: ExternalToolSchemaPolicy::Eager,
        max_turn_steps: crate::agent::DEFAULT_MAX_TURN_STEPS,
        allow_unbounded_turn: false,
        retry_config: crate::agent::retry::RetryConfig::default(),
    };

    let allowed_tools: HashSet<String> = REVIEW_TOOLS
        .iter()
        .map(|tool| (*tool).to_string())
        .collect();
    let credential_vault = CredentialVault::new();
    let (agent, mut event_rx) = NativeAgent::new_with_allowed_tools_and_credential_vault(
        config,
        &allowed_tools,
        credential_vault.clone(),
    )
    .context("Failed to create rubber duck review agent")?;
    let tool_tx = agent.tool_response_sender();
    let tool_executor = ToolExecutor::with_credential_vault(cwd, credential_vault.clone());

    agent.send_ready();
    agent
        .prompt(prompt.to_string(), vec![])
        .await
        .context("Failed to send review prompt")?;

    let workspace = dunce::canonicalize(Path::new(cwd)).unwrap_or_else(|_| PathBuf::from(cwd));
    let drained = tokio::time::timeout(
        REVIEW_TIMEOUT,
        drain_events(
            &mut event_rx,
            &tool_tx,
            &tool_executor,
            &allowed_tools,
            &workspace,
        ),
    )
    .await;
    match drained {
        Ok(review) => review,
        Err(_) => {
            // The drive future is dropped on timeout; cancel the agent runner
            // explicitly or it stays detached and keeps consuming tokens.
            agent.cancel();
            anyhow::bail!("Timed out after {} seconds", REVIEW_TIMEOUT.as_secs());
        }
    }
}

/// Bookkeeping for the review event drain, extracted from [`drain_events`]
/// so the text/error logic is testable without a live agent.
#[derive(Default)]
struct ReviewDrain {
    /// Text of the in-flight (not yet completed) model response.
    assistant_buf: String,
    /// Text of the last completed model response.
    last_completed: String,
    /// Last terminal (nonfatal) provider error seen. Cleared when a later
    /// completed model response supersedes it (the provider recovered).
    terminal_error: Option<String>,
}

impl ReviewDrain {
    fn on_chunk(&mut self, content: &str, is_thinking: bool) {
        if !is_thinking {
            self.assistant_buf.push_str(content);
        }
    }

    /// Record a completed model response. The enclosing turn remains active
    /// until an explicit turn terminal arrives.
    fn on_response_end(&mut self, response_id: &str) {
        if response_id == "done" {
            return;
        }
        if !self.assistant_buf.is_empty() {
            self.last_completed = std::mem::take(&mut self.assistant_buf);
            self.terminal_error = None;
        }
    }

    /// Handle a provider error. Fatal errors abort the drain; nonfatal ones
    /// are terminal for the current response and recorded so a stale partial
    /// buffer can't be posted as a successful review.
    fn on_error(&mut self, message: &str, fatal: bool) -> Result<()> {
        if fatal {
            anyhow::bail!("Review agent error: {message}");
        }
        self.terminal_error = Some(message.to_string());
        Ok(())
    }

    fn finish(self) -> Result<String> {
        if let Some(message) = self.terminal_error {
            anyhow::bail!("Review provider failed: {message}");
        }
        let review = if self.last_completed.is_empty() {
            self.assistant_buf
        } else {
            self.last_completed
        };
        if review.trim().is_empty() {
            anyhow::bail!("Review agent returned no output");
        }
        Ok(review)
    }
}

/// Consume agent events until an explicit turn terminal, executing allowed
/// read-only tool calls (workspace-contained) along the way, and return the
/// last completed assistant text.
async fn drain_events(
    event_rx: &mut tokio::sync::mpsc::UnboundedReceiver<FromAgent>,
    tool_tx: &tokio::sync::mpsc::UnboundedSender<crate::agent::ToolResponseMessage>,
    tool_executor: &ToolExecutor,
    allowed_tools: &HashSet<String>,
    workspace: &Path,
) -> Result<String> {
    let mut drain = ReviewDrain::default();

    loop {
        let Some(msg) = event_rx.recv().await else {
            break;
        };

        match msg {
            FromAgent::ResponseChunk {
                content,
                is_thinking,
                ..
            } => {
                drain.on_chunk(&content, is_thinking);
            }
            FromAgent::ToolCall {
                call_id,
                tool,
                args,
                ..
            } => {
                let normalized = tool.to_ascii_lowercase();
                let prepared = if allowed_tools.contains(&normalized) {
                    contain_tool_args(&normalized, &args, workspace)
                } else {
                    Err(format!(
                        "Tool `{tool}` is not allowed for the rubber duck reviewer"
                    ))
                };
                let (approved, result) = match prepared {
                    Ok(prepared_args) => (
                        true,
                        tool_executor
                            .execute(&tool, &prepared_args, None, &call_id)
                            .await,
                    ),
                    Err(message) => (false, crate::agent::ToolResult::failure(message)),
                };
                let _ = tool_tx.send((
                    call_id,
                    approved,
                    Some(result),
                    ExecutionSource::Native,
                    None,
                ));
            }
            FromAgent::ResponseEnd { response_id, .. } => {
                drain.on_response_end(&response_id);
            }
            FromAgent::TurnCompleted { .. } => break,
            FromAgent::TurnInterrupted { reason, .. } => {
                drain.on_error(&reason, false)?;
                break;
            }
            FromAgent::ProviderError { message, .. } => {
                drain.on_error(&message, false)?;
                break;
            }
            FromAgent::Error {
                message,
                fatal,
                terminal,
                ..
            } => {
                drain.on_error(&message, fatal)?;
                if terminal {
                    break;
                }
            }
            _ => {}
        }
    }

    drain.finish()
}

/// Constrain a reviewer tool call's path-like arguments to the workspace,
/// mirroring print mode's `workspace_only_file_tools` handling so a
/// prompt-injected diff (or the reviewer itself) cannot read files outside
/// the repo (e.g. `~/.ssh`) and ship them to the review provider. Returns an
/// error message when any path escapes or cannot be resolved.
/// Contain tool args to the workspace for read-only second-opinion agents
/// (rubber duck, goal judge). `pub(crate)` so sibling modules can reuse it.
pub(crate) fn contain_tool_args(
    tool: &str,
    args: &serde_json::Value,
    workspace: &Path,
) -> Result<serde_json::Value, String> {
    reject_option_like_args(args)?;
    // `read` and `glob` are covered by print mode's shared helper.
    let mut prepared = crate::print_mode::prepare_workspace_tool_args(tool, args, workspace)
        .map_err(|error| format!("{error:#}"))?;
    match tool {
        "grep" | "list" | "find" | "diff" => {
            contain_scalar_path(&mut prepared, "path", workspace)?;
        }
        "search" => {
            contain_scalar_path(&mut prepared, "cwd", workspace)?;
            contain_paths_arg(&mut prepared, workspace)?;
        }
        "status" => contain_paths_arg(&mut prepared, workspace)?,
        _ => {}
    }
    Ok(prepared)
}

/// Reject option-like values (`-...`) in path-like arguments and the diff
/// tool's `target`: tools pass these into subprocess argv, where a leading
/// dash turns a "path" into a command option (e.g. `git diff --output=...`
/// writing arbitrary files). Option-like values have no legitimate use here.
fn reject_option_like_args(args: &serde_json::Value) -> Result<(), String> {
    let check = |value: &str| {
        if value.starts_with('-') {
            Err(format!(
                "Argument value `{value}` looks like an option, not a path or git ref"
            ))
        } else {
            Ok(())
        }
    };
    for key in ["path", "file_path", "cwd", "target"] {
        if let Some(value) = args.get(key).and_then(serde_json::Value::as_str) {
            check(value)?;
        }
    }
    match args.get("paths") {
        Some(serde_json::Value::String(value)) => check(value)?,
        Some(serde_json::Value::Array(values)) => {
            for value in values.iter().filter_map(serde_json::Value::as_str) {
                check(value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Canonicalize an optional string path argument in place.
fn contain_scalar_path(
    args: &mut serde_json::Value,
    key: &str,
    workspace: &Path,
) -> Result<(), String> {
    let Some(input) = args.get(key).and_then(serde_json::Value::as_str) else {
        return Ok(());
    };
    args[key] = serde_json::Value::String(contain_one_path(workspace, input)?);
    Ok(())
}

/// Canonicalize the `paths` argument (string or string array) in place.
fn contain_paths_arg(args: &mut serde_json::Value, workspace: &Path) -> Result<(), String> {
    let Some(paths) = args.get("paths").cloned() else {
        return Ok(());
    };
    let contained = match paths {
        serde_json::Value::String(input) => {
            serde_json::Value::String(contain_one_path(workspace, &input)?)
        }
        serde_json::Value::Array(inputs) => serde_json::Value::Array(
            inputs
                .iter()
                .map(|input| match input.as_str() {
                    Some(input) => Ok(serde_json::Value::String(contain_one_path(
                        workspace, input,
                    )?)),
                    None => Ok(input.clone()),
                })
                .collect::<Result<Vec<_>, String>>()?,
        ),
        other => other,
    };
    args["paths"] = contained;
    Ok(())
}

fn contain_one_path(workspace: &Path, input: &str) -> Result<String, String> {
    Ok(
        crate::print_mode::canonical_workspace_path(workspace, input)
            .map_err(|error| format!("{error:#}"))?
            .display()
            .to_string(),
    )
}

/// Run a git command in the style of `crate::git` and return trimmed stdout.
fn run_git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    Ok(run_git_raw(cwd, args)?.trim().to_string())
}

/// Run a git command, reading only a bounded stdout prefix
/// ([`MAX_GIT_OUTPUT_BYTES`]) so a huge diff can't exhaust memory or stall
/// the caller; the child is killed once the cap is hit. Stderr is drained
/// concurrently on a helper thread (bounded buffer, excess discarded) so a
/// child writing to stderr can't deadlock against the stdout read. Stdout is
/// returned untrimmed: callers that parse NUL-delimited output must keep
/// leading whitespace intact, text callers use [`run_git`].
fn run_git_raw(cwd: &Path, args: &[&str]) -> Result<String, String> {
    Ok(String::from_utf8_lossy(&run_git_raw_bounded(cwd, args)?.bytes).into_owned())
}

#[derive(Debug)]
struct BoundedGitOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

fn run_git_raw_bounded(cwd: &Path, args: &[&str]) -> Result<BoundedGitOutput, String> {
    run_git_raw_bounded_with_timeout(cwd, args, GIT_COMMAND_TIMEOUT)
}

fn run_git_raw_bounded_with_timeout(
    cwd: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<BoundedGitOutput, String> {
    let mut command = Command::new("git");
    // `/rubber-duck` is advertised as read-only review input gathering. A
    // user or repo `diff.external` / textconv driver would otherwise run
    // arbitrary local commands when we call `git diff`. Force every git
    // invocation through the built-in path with no external helpers and no
    // attribute-driven textconv (review finding on evalops/maestro#917).
    command
        .args([
            "-c",
            "diff.external=",
            "-c",
            "core.attributesFile=/dev/null",
            "-c",
            "core.safecrlf=false",
        ])
        .env_remove("GIT_EXTERNAL_DIFF")
        .env_remove("GIT_DIFF_OPTS");
    // For `diff`, also pass the CLI flags that disable external drivers /
    // textconv even if a later config layer re-enables them.
    if args.first().is_some_and(|arg| *arg == "diff") {
        command.arg("diff").args(["--no-ext-diff", "--no-textconv"]);
        if args.len() > 1 {
            command.args(&args[1..]);
        }
    } else {
        command.args(args);
    }
    command
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .map_err(|err| format!("Failed to run git {}: {err}", args.join(" ")))?;

    // Drain both pipes concurrently so neither can block the child.
    let mut stderr_pipe = child.stderr.take().expect("stderr piped");
    let (stderr_tx, stderr_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8_192];
        loop {
            match stderr_pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    // Keep draining (discarding past the cap) so the child
                    // never blocks on a full stderr pipe.
                    let room = MAX_GIT_OUTPUT_BYTES as usize - buf.len();
                    buf.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
        }
        let _ = stderr_tx.send(buf);
    });

    let mut stdout_pipe = child.stdout.take().expect("stdout piped");
    let (stdout_tx, stdout_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut stdout = Vec::new();
        let result = stdout_pipe
            .by_ref()
            .take(MAX_GIT_OUTPUT_BYTES)
            .read_to_end(&mut stdout);
        let _ = stdout_tx.send((result, stdout));
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                terminate_git_process(&mut child);
                let _ = child.wait();
                return Err(format!(
                    "git {} timed out after {} seconds",
                    args.join(" "),
                    timeout.as_secs_f64()
                ));
            }
            Err(err) => {
                terminate_git_process(&mut child);
                let _ = child.wait();
                return Err(format!("Failed to wait for git {}: {err}", args.join(" ")));
            }
        }
    };
    let Some(stdout_remaining) = deadline.checked_duration_since(Instant::now()) else {
        terminate_git_process(&mut child);
        return Err(format!("git {} timed out draining stdout", args.join(" ")));
    };
    let (read_result, stdout) = match stdout_rx.recv_timeout(stdout_remaining) {
        Ok(output) => output,
        Err(_) => {
            terminate_git_process(&mut child);
            return Err(format!("git {} timed out draining stdout", args.join(" ")));
        }
    };
    if let Err(err) = read_result {
        terminate_git_process(&mut child);
        return Err(format!(
            "Failed to read git {} output: {err}",
            args.join(" ")
        ));
    }
    // At the cap there may be more output; preserve that fact for callers
    // that need to mark their listing as incomplete.
    let hit_cap = stdout.len() as u64 == MAX_GIT_OUTPUT_BYTES;
    let Some(stderr_remaining) = deadline.checked_duration_since(Instant::now()) else {
        terminate_git_process(&mut child);
        return Err(format!("git {} timed out draining stderr", args.join(" ")));
    };
    let stderr_bytes = match stderr_rx.recv_timeout(stderr_remaining) {
        Ok(stderr) => stderr,
        Err(_) => {
            terminate_git_process(&mut child);
            return Err(format!("git {} timed out draining stderr", args.join(" ")));
        }
    };
    let stderr = String::from_utf8_lossy(&stderr_bytes).trim().to_string();

    if hit_cap || status.success() {
        Ok(BoundedGitOutput {
            bytes: stdout,
            truncated: hit_cap,
        })
    } else {
        Err(if stderr.is_empty() {
            format!("git {} failed", args.join(" "))
        } else {
            stderr
        })
    }
}

fn terminate_git_process(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let Ok(group_id) = i32::try_from(child.id()) else {
            let _ = child.kill();
            return;
        };
        // SAFETY: the Git command was placed in a new process group above;
        // a negative PID targets only that group, including diff helpers
        // which may still hold inherited stdout/stderr handles.
        unsafe {
            let _ = libc::kill(-group_id, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_pick_prefers_a_different_provider() {
        let picked = pick_automatic_review_model(
            "gpt-5.5",
            ["gpt-5.5", "gpt-5.6", "anthropic/claude-sonnet-4-6"]
                .into_iter()
                .map(str::to_owned),
            |_| true,
        )
        .expect("an alternative model");
        assert_eq!(picked, "anthropic/claude-sonnet-4-6");
    }

    #[test]
    fn automatic_pick_falls_back_to_a_different_model_from_the_same_provider() {
        let picked = pick_automatic_review_model(
            "gpt-5.5",
            ["gpt-5.5", "gpt-5.6"].into_iter().map(str::to_owned),
            |_| true,
        )
        .expect("an alternative model");
        assert_eq!(picked, "gpt-5.6");
    }

    #[test]
    fn pick_honors_explicit_different_model() {
        let picked = pick_review_model("gpt-5.5", Some("claude-opus-4-6")).expect("explicit model");
        assert_eq!(picked, "claude-opus-4-6");
    }

    #[test]
    fn pick_rejects_explicit_same_model() {
        assert!(pick_review_model("gpt-5.5", Some("gpt-5.5")).is_err());
    }

    #[test]
    fn pick_rejects_provider_qualified_alias_of_current_model() {
        // Bare current + qualified requested.
        assert!(pick_review_model("gpt-5.5", Some("openai/gpt-5.5")).is_err());
        // Qualified current + bare requested.
        assert!(pick_review_model("openai/gpt-5.5", Some("gpt-5.5")).is_err());
        // An OpenRouter vendor route is not independent from the same direct
        // vendor model, even though the transport providers differ.
        assert!(
            pick_review_model(
                "openrouter/anthropic/claude-sonnet-4.5",
                Some("anthropic/claude-sonnet-4.5")
            )
            .is_err()
        );
        assert!(
            pick_review_model(
                "openrouter/google/gemini-2.5-pro",
                Some("google/gemini-2.5-pro")
            )
            .is_err()
        );
    }

    #[test]
    fn same_model_matches_provider_qualified_aliases() {
        assert!(same_model("gpt-5.5", "openai/gpt-5.5"));
        assert!(same_model("openai/gpt-5.5", "gpt-5.5"));
        assert!(same_model("gpt-5.6", "openrouter/gpt-5.6"));
        assert!(same_model("openrouter/gpt-5.6", "gpt-5.6"));
        assert!(same_model("openrouter/openai/gpt-5.6", "openai/gpt-5.6"));
        assert!(same_model(
            "openrouter/anthropic/claude-sonnet-4.5",
            "anthropic/claude-sonnet-4.5"
        ));
        assert!(same_model(
            "evalops/openrouter/google/gemini-2.5-pro",
            "google/gemini-2.5-pro"
        ));
        assert!(same_model(
            "claude-sonnet-4-6",
            "anthropic/claude-sonnet-4-6"
        ));
        assert!(!same_model("gpt-5.5", "claude-sonnet-4-6"));
    }

    #[test]
    fn uncommitted_diff_includes_untracked_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let init = Command::new("git")
            .arg("init")
            .current_dir(temp.path())
            .output()
            .expect("git init");
        assert!(init.status.success());
        std::fs::write(temp.path().join("new_file.rs"), "fn main() {}\n").expect("write file");

        let diff = uncommitted_diff(temp.path()).expect("diff");
        assert!(!diff.is_empty());
        assert!(diff.contains("new_file.rs"));
        assert!(diff.contains("fn main() {}"));
    }

    #[test]
    fn uncommitted_diff_includes_filenames_with_special_chars() {
        let temp = tempfile::tempdir().expect("tempdir");
        let init = Command::new("git")
            .arg("init")
            .current_dir(temp.path())
            .output()
            .expect("git init");
        assert!(init.status.success());
        // Default `git ls-files` C-quotes this name; `-z` must keep it raw.
        std::fs::write(temp.path().join("line\nbreak.rs"), "fn special() {}\n")
            .expect("write file");

        let diff = uncommitted_diff(temp.path()).expect("diff");
        assert!(diff.contains("line\nbreak.rs"));
        assert!(diff.contains("fn special() {}"));
    }

    #[cfg(unix)]
    #[test]
    fn uncommitted_diff_includes_non_utf8_filenames() {
        use std::os::unix::ffi::OsStringExt;

        let temp = tempfile::tempdir().expect("tempdir");
        let init = Command::new("git")
            .arg("init")
            .current_dir(temp.path())
            .output()
            .expect("git init");
        assert!(init.status.success());
        let name = std::ffi::OsString::from_vec(b"invalid-\xff.rs".to_vec());
        // APFS (the default macOS filesystem) rejects filenames that are not
        // valid UTF-8 with EILSEQ, so the fixture cannot even be created
        // there. Skip when the filesystem refuses the name, but only for
        // that specific refusal: on Linux, where arbitrary non-NUL bytes are
        // always valid in filenames, creation must succeed and any error
        // fails the test.
        if let Err(err) = std::fs::write(temp.path().join(&name), "fn non_utf8() {}\n") {
            assert!(
                std::env::consts::OS != "linux",
                "creating a non-UTF-8 filename must succeed on Linux: {err}"
            );
            assert_eq!(
                err.raw_os_error(),
                Some(libc::EILSEQ),
                "unexpected fixture-creation failure: {err}"
            );
            return;
        }

        let diff = uncommitted_diff(temp.path()).expect("diff");
        assert!(diff.contains("fn non_utf8() {}"));
    }

    #[test]
    fn uncommitted_diff_preserves_leading_whitespace_in_filenames() {
        let temp = tempfile::tempdir().expect("tempdir");
        let init = Command::new("git")
            .arg("init")
            .current_dir(temp.path())
            .output()
            .expect("git init");
        assert!(init.status.success());
        std::fs::write(temp.path().join(" leading.rs"), "fn leading() {}\n").expect("write file");

        let diff = uncommitted_diff(temp.path()).expect("diff");
        assert!(diff.contains(" leading.rs"));
        assert!(diff.contains("fn leading() {}"));
    }

    #[test]
    fn uncommitted_diff_skipped_entries_do_not_consume_file_quota() {
        let temp = tempfile::tempdir().expect("tempdir");
        let init = Command::new("git")
            .arg("init")
            .current_dir(temp.path())
            .output()
            .expect("git init");
        assert!(init.status.success());
        // More binary (skipped) entries than the include cap, named to sort
        // first, must not push the real source file out of the review input.
        for index in 0..MAX_UNTRACKED_FILES {
            std::fs::write(
                temp.path().join(format!("a_junk_{index:02}.bin")),
                b"\0junk",
            )
            .expect("write junk");
        }
        std::fs::write(temp.path().join("real.rs"), "fn real() {}\n").expect("write file");

        let diff = uncommitted_diff(temp.path()).expect("diff");
        assert!(diff.contains("real.rs"));
        assert!(diff.contains("fn real() {}"));
    }

    #[test]
    fn untracked_listing_reports_when_bounded_output_is_incomplete() {
        let temp = tempfile::tempdir().expect("tempdir");
        let init = Command::new("git")
            .arg("init")
            .current_dir(temp.path())
            .output()
            .expect("git init");
        assert!(init.status.success());
        // Force `git ls-files -z` beyond MAX_GIT_OUTPUT_BYTES without
        // exceeding the per-component filesystem limit.
        for index in 0..160 {
            let name = format!("{index:03}_{}.rs", "x".repeat(180));
            std::fs::write(temp.path().join(name), "fn item() {}\n").expect("write file");
        }

        let section = untracked_files_section(temp.path());
        assert!(section.contains("additional untracked files omitted"));
    }

    #[test]
    fn tracked_diff_failure_is_not_treated_as_an_unborn_repository() {
        let temp = tempfile::tempdir().expect("tempdir");
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(temp.path())
                .status()
                .expect("git init")
                .success()
        );
        std::fs::write(temp.path().join("tracked.rs"), "fn tracked() {}\n").expect("write file");
        for args in [
            ["add", "tracked.rs"].as_slice(),
            [
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-qm",
                "initial",
            ]
            .as_slice(),
        ] {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(temp.path())
                    .status()
                    .expect("git command")
                    .success()
            );
        }
        std::fs::write(temp.path().join(".git/index"), b"corrupt index").expect("corrupt index");

        let err = uncommitted_diff(temp.path()).expect_err("corrupt index must be surfaced");
        assert!(err.contains("failed") || err.contains("fatal"));
    }

    #[test]
    fn invalid_head_is_not_treated_as_an_unborn_repository() {
        let temp = tempfile::tempdir().expect("tempdir");
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(temp.path())
                .status()
                .expect("git init")
                .success()
        );
        std::fs::write(temp.path().join(".git/HEAD"), "not a valid HEAD\n").expect("break HEAD");

        let err = uncommitted_diff(temp.path()).expect_err("invalid HEAD must be surfaced");
        assert!(err.contains("fatal") || err.contains("failed"));
    }

    /// External diff drivers must not run: `/rubber-duck` is read-only input
    /// gathering (review finding on evalops/maestro#917).
    #[cfg(unix)]
    #[test]
    fn uncommitted_diff_does_not_invoke_external_diff_driver() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("tempdir");
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(temp.path())
                .status()
                .expect("git init")
                .success()
        );
        std::fs::write(temp.path().join("tracked.rs"), "before\n").expect("write tracked");
        for args in [
            ["add", "tracked.rs"].as_slice(),
            [
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-qm",
                "initial",
            ]
            .as_slice(),
        ] {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(temp.path())
                    .status()
                    .expect("git command")
                    .success()
            );
        }
        let sentinel = temp.path().join("external-diff-ran");
        let helper = temp.path().join("trap-diff");
        std::fs::write(
            &helper,
            format!("#!/bin/sh\ntouch '{}'\nexit 0\n", sentinel.display()),
        )
        .expect("write helper");
        let mut permissions = std::fs::metadata(&helper).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&helper, permissions).expect("chmod");
        assert!(
            Command::new("git")
                .args(["config", "diff.external", helper.to_str().expect("utf8")])
                .current_dir(temp.path())
                .status()
                .expect("git config")
                .success()
        );
        std::fs::write(temp.path().join("tracked.rs"), "after\n").expect("edit tracked");

        let diff = uncommitted_diff(temp.path()).expect("diff must use built-in path");
        assert!(
            diff.contains("after") || diff.contains("tracked.rs"),
            "built-in diff must still show the change: {diff}"
        );
        assert!(
            !sentinel.exists(),
            "diff.external helper must not run under rubber_duck git collection"
        );
    }

    #[cfg(unix)]
    #[test]
    fn uncommitted_diff_skips_untracked_symlink_escaping_workspace() {
        use std::os::unix::fs::symlink;

        let repo = tempfile::tempdir().expect("repo tempdir");
        let init = Command::new("git")
            .arg("init")
            .current_dir(repo.path())
            .output()
            .expect("git init");
        assert!(init.status.success());
        let outside = tempfile::tempdir().expect("outside tempdir");
        std::fs::write(outside.path().join("secret.txt"), "top secret").expect("write secret");
        symlink(
            outside.path().join("secret.txt"),
            repo.path().join("leak.txt"),
        )
        .expect("symlink");
        std::fs::write(repo.path().join("normal.rs"), "fn normal() {}\n").expect("write file");

        let diff = uncommitted_diff(repo.path()).expect("diff");
        assert!(diff.contains("fn normal() {}"));
        assert!(!diff.contains("top secret"));
        assert!(!diff.contains("leak.txt"));
    }

    #[test]
    fn contain_tool_args_rejects_option_like_diff_target() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let result = contain_tool_args(
            "diff",
            &serde_json::json!({"target": "--output=/tmp/pwned"}),
            workspace.path(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn contain_tool_args_rejects_option_like_paths() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let scalar = contain_tool_args(
            "grep",
            &serde_json::json!({"pattern": "x", "path": "--include=*.rs"}),
            workspace.path(),
        );
        assert!(scalar.is_err());
        let list = contain_tool_args(
            "status",
            &serde_json::json!({"paths": ["--others"]}),
            workspace.path(),
        );
        assert!(list.is_err());
    }

    #[test]
    fn prompt_mentions_current_model_and_includes_diff() {
        let prompt = build_review_prompt("gpt-5.5", "diff --git a/x.rs b/x.rs");
        assert!(prompt.contains("gpt-5.5"));
        assert!(prompt.contains("diff --git a/x.rs b/x.rs"));
    }

    #[test]
    fn truncate_diff_leaves_short_diffs_untouched() {
        assert_eq!(truncate_diff("small"), "small");
    }

    #[test]
    fn truncate_diff_caps_long_diffs() {
        let diff = "a".repeat(MAX_DIFF_CHARS * 2);
        let truncated = truncate_diff(&diff);
        assert!(truncated.len() < diff.len());
        assert!(truncated.contains("truncated"));
    }

    #[test]
    fn drain_partial_output_then_provider_failure_is_not_a_success() {
        let mut drain = ReviewDrain::default();
        drain.on_chunk("Partial review findings", false);
        drain
            .on_error("rate limit exceeded", false)
            .expect("nonfatal");
        drain.on_response_end("done");

        let err = drain
            .finish()
            .expect_err("partial buffer must not pass as a completed review");
        assert!(err.to_string().contains("rate limit exceeded"));
    }

    #[test]
    fn drain_completed_response_after_error_recovers() {
        let mut drain = ReviewDrain::default();
        drain.on_error("transient error", false).expect("nonfatal");
        drain.on_chunk("Full review", false);
        drain.on_response_end("resp-1");
        drain.on_response_end("done");

        assert_eq!(drain.finish().expect("recovered review"), "Full review");
    }

    #[test]
    fn response_end_does_not_turn_an_error_into_success() {
        let mut drain = ReviewDrain::default();
        drain
            .on_error("UserPromptSubmit hook blocked the review", false)
            .expect("nonfatal");

        drain.on_response_end("blocked");
        let err = drain.finish().expect_err("blocked hook must fail");
        assert!(err.to_string().contains("hook blocked"));
    }

    #[test]
    fn drain_fatal_error_aborts() {
        let mut drain = ReviewDrain::default();
        assert!(drain.on_error("boom", true).is_err());
    }
}
