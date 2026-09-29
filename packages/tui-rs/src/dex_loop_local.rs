//! Local (non-cloud) turns through the dex-loop kernel, behind
//! `MAESTRO_DEX_LOOP=1`.
//!
//! `products/maestro/packages/dex-host-rs` (`maestro_dex_host`) had no
//! consumer before this: `tests/turn.rs` proved the kernel's park/approve/
//! resume semantics against a scripted model, but nothing in Maestro's own
//! binaries ever ran a real local turn through it. This module is that
//! consumer, wired into `print_mode.rs`'s non-interactive single-shot entry
//! point -- itself already documented as "auto-approves tools" -- so the
//! auto-approve/auto-answer policy `maestro_dex_host::run_local_turn` uses
//! matches what that entry point already promises.
//!
//! Scope, matching `docs/design/maestro-on-dex-loop.md`'s "first slice"
//! framing:
//! - Only two tools run (`fs.read_file`, `fs.write_file`; see
//!   `dex-host-rs/src/tools.rs`). Everything else in Maestro's real tool
//!   registry (bash, native file edit, web fetch, ...) is not on this path
//!   yet -- that is the design doc's next cutover step, tool by tool.
//! - The model talks to Anthropic directly (`ANTHROPIC_API_KEY`), not
//!   through Maestro's managed-credential/provider-routing machinery
//!   (`agent::CredentialVault`) the rest of the TUI uses. A real cutover
//!   reuses that machinery; this slice does not, to stay a small, legible
//!   diff, and says so here instead of silently approximating it.
//! - This is additive: unless `MAESTRO_DEX_LOOP=1` is set, `run_print_mode`
//!   is untouched and still drives every real print-mode turn.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use maestro_ai::{AnthropicClient, UnifiedClient};
use maestro_dex_host::dex_loop::{Exit, PrincipalId, ThreadId, TurnId};
use maestro_dex_host::{AiRsModel, LocalTurnOutcome, LocalTurnRequest, run_local_turn};

use crate::print_mode::PrintModeOptions;

/// The model this slice talks to when the caller does not pick one via
/// `PrintModeOptions::model`. Matches `maestro_ai::RequestConfig`'s own
/// default, so it is a model that crate already knows how to call.
const DEFAULT_MODEL: &str = "claude-sonnet-4-20250514";
const DEFAULT_MAX_TOKENS: u32 = 8_192;

/// `true` when `MAESTRO_DEX_LOOP=1`. Any other value (including unset) keeps
/// the old loop; this is an explicit opt-in, not a fallback trigger.
#[must_use]
pub fn enabled() -> bool {
    std::env::var("MAESTRO_DEX_LOOP")
        .map(|value| value.trim() == "1")
        .unwrap_or(false)
}

/// Runs `options.prompt` through the dex-loop kernel if `enabled()`.
/// Returns `Ok(None)` (never touching anything) when the flag is off, so
/// `run_print_mode` can call this unconditionally at its own top and fall
/// through to its existing body otherwise.
pub async fn maybe_run(options: &PrintModeOptions) -> Result<Option<i32>> {
    if !enabled() {
        return Ok(None);
    }
    let workspace = dunce::canonicalize(
        &std::env::current_dir().context("resolve the dex-loop working directory")?,
    )
    .context("canonicalize the dex-loop working directory")?;
    let outcome = run(&workspace, options).await?;
    Ok(Some(report(&workspace, options, outcome)))
}

fn anthropic_api_key() -> Result<String> {
    for name in ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"] {
        if let Ok(value) = std::env::var(name) {
            let value = value.trim().to_owned();
            if !value.is_empty() {
                return Ok(value);
            }
        }
    }
    bail!(
        "MAESTRO_DEX_LOOP=1 needs a direct Anthropic credential (this slice does not yet reuse \
         Maestro's managed-credential routing). Set ANTHROPIC_API_KEY."
    );
}

/// A stable thread id per workspace, so repeated `MAESTRO_DEX_LOOP=1` runs
/// in the same directory resume the same `LocalLog` instead of starting a
/// fresh one every time. Mirrors `cloud_cli::sha256_hex`'s shape.
fn workspace_thread(workspace: &Path) -> ThreadId {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(workspace.to_string_lossy().as_bytes());
    let mut hex = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    ThreadId {
        org: "local".into(),
        workspace: "local".into(),
        thread: format!("print-mode-{hex}"),
    }
}

fn state_root(workspace: &Path) -> PathBuf {
    workspace.join(".maestro").join("dex-loop")
}

async fn run(workspace: &Path, options: &PrintModeOptions) -> Result<LocalTurnOutcome> {
    let model_name = options
        .model
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_MODEL)
        .to_owned();
    let client = AnthropicClient::new(anthropic_api_key()?)
        .context("construct the Anthropic client for MAESTRO_DEX_LOOP")?;
    let model = AiRsModel::new(
        UnifiedClient::Anthropic(client),
        model_name,
        DEFAULT_MAX_TOKENS,
    );

    let thread = workspace_thread(workspace);
    let request = LocalTurnRequest {
        thread: thread.clone(),
        principal: PrincipalId::new("local-user"),
        turn: TurnId::new(format!("t-{}", uuid::Uuid::new_v4())),
        text: options.prompt.clone(),
    };
    run_local_turn(&state_root(workspace), workspace, model, request)
        .await
        .context("run the local turn through the dex-loop kernel")
}

fn report(workspace: &Path, options: &PrintModeOptions, outcome: LocalTurnOutcome) -> i32 {
    let LocalTurnOutcome { exit, final_text } = outcome;
    let text = final_text.unwrap_or_default();
    if let Some(path) = &options.output_last_message {
        let _ = std::fs::write(path, &text);
    }
    if options.json {
        let payload = serde_json::json!({
            "type": "result",
            "exit": format!("{exit:?}"),
            "text": text,
            "workspace": workspace.to_string_lossy(),
        });
        println!("{payload}");
    } else if !text.is_empty() {
        println!("{text}");
    }
    match exit {
        Exit::Done => 0,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_thread_is_stable_and_scoped_per_directory() {
        let a = workspace_thread(Path::new("/repo/a"));
        let b = workspace_thread(Path::new("/repo/a"));
        let c = workspace_thread(Path::new("/repo/b"));
        assert_eq!(a.thread, b.thread);
        assert_ne!(a.thread, c.thread);
        assert!(a.thread.starts_with("print-mode-"));
    }

    /// Serializes mutation of `MAESTRO_DEX_LOOP` across test threads in this
    /// binary; it is process-global and Rust runs tests in parallel.
    static DEX_LOOP_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn enabled_requires_exactly_the_string_one() {
        let _guard = DEX_LOOP_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (value, expected) in [
            (None, false),
            (Some(""), false),
            (Some("0"), false),
            (Some("true"), false),
            (Some("1"), true),
            (Some(" 1 "), true),
        ] {
            match value {
                // SAFETY: serialized by `DEX_LOOP_ENV_LOCK` above.
                Some(value) => unsafe { std::env::set_var("MAESTRO_DEX_LOOP", value) },
                // SAFETY: see above.
                None => unsafe { std::env::remove_var("MAESTRO_DEX_LOOP") },
            }
            assert_eq!(enabled(), expected, "value={value:?}");
        }
        // SAFETY: see above.
        unsafe { std::env::remove_var("MAESTRO_DEX_LOOP") };
    }
}
