//! `deixic-code cloud` — an interactive local session whose agent, tools,
//! shell, builds and git all run in a persistent remote workspace on EvalOps
//! remote runners. See `docs/cloud-mode.md`.
//!
//! Phase 1 drives the existing `RemoteRunnerService` and the Runner Host
//! operating-thread routes from the client:
//!
//! 1. `cloud up`: `CreateRunnerSession` keyed by the workspace identity
//!    (so a second `up` from any machine resolves the same live session),
//!    wait until the sandbox runs, then materialize the local repository
//!    with `ExecuteRunnerSessionStep` (`GENERIC`): `gh` login from stdin, a
//!    partial clone of the merge base, a `git bundle` of local commits, the
//!    working-tree diff and untracked files, all uploaded in stdin chunks.
//! 2. `cloud attach`: a line REPL. Each prompt is one `user_message` turn on
//!    `/internal/v1/operating-threads/{thread}/turns`; events come from
//!    `/events/replay`; approvals and user-input requests are answered on
//!    `/responses`. `/detach` leaves the turn running in the sandbox.
//! 3. `cloud pull`: the sandbox commits its tree and pushes
//!    `cloud/<name>`; the laptop fast-forwards onto it.
//!
//! The GitHub token travels only as step stdin. Runner Host persists the
//! stdin SHA-256, never the bytes (`rust/services/runner-host/src/handlers/steps.rs`).

use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, BufReader};
use uuid::Uuid;

use crate::remote_cli::{
    ClientOpts, Config, EVENTS_PATH, LIST_PATH, STOP_PATH, Session, array_events, array_sessions,
    first_str, get_session, post, print_json, print_table, require_config, strip_null, wait_ready,
};

const CREATE_PATH: &str = "/remoterunner.v1.RemoteRunnerService/CreateRunnerSession";
const EXTEND_PATH: &str = "/remoterunner.v1.RemoteRunnerService/ExtendRunnerSession";
const STEP_PATH: &str = "/remoterunner.v1.RemoteRunnerService/ExecuteRunnerSessionStep";
const STEP_OPERATION_GENERIC: &str = "RUNNER_SESSION_STEP_OPERATION_GENERIC";

/// Scopes `RemoteRunnerService` requires on top of the default login scopes
/// (`proto/remoterunner/v1/remoterunner.proto`, `required_scopes`).
pub const CLOUD_LOGIN_SCOPES: &str = "remote-runner:read remote-runner:write";

/// Where the repository lives inside the sandbox. `/workspace` is the hosted
/// runner's `MAESTRO_WORKSPACE_ROOT`.
pub const CLOUD_REPO_DIR: &str = "/workspace/repo";
/// Scratch directory for sync payloads and the sync marker.
pub const CLOUD_SYNC_DIR: &str = "/workspace/.deixic-cloud";

const DEFAULT_TTL_MINUTES: u64 = 8 * 60;
const EXTEND_BELOW_MINUTES: i64 = 60;
const READY_TIMEOUT_MS: u64 = 10 * 60 * 1000;
const READY_POLL_MS: u64 = 5_000;
const BINDING_TIMEOUT: Duration = Duration::from_mins(3);
const BINDING_POLL: Duration = Duration::from_secs(3);
const REPLAY_POLL: Duration = Duration::from_secs(1);
const REPLAY_IDLE_TIMEOUT: Duration = Duration::from_mins(30);
const STEP_HTTP_SLACK_MS: u64 = 15_000;
const CLONE_TIMEOUT_SECONDS: u32 = 1_800;
const SHORT_STEP_TIMEOUT_SECONDS: u32 = 120;
/// Runner Host accepts at most 1 MiB of step stdin
/// (`rust/services/runner-host/src/dto.rs`, `MAX_RUNNER_STEP_STDIN_BYTES`).
pub const SYNC_CHUNK_BYTES: usize = 900 * 1024;
/// Phase 1 cap on the uploaded snapshot (bundle + diff + untracked files).
pub const SYNC_MAX_BYTES: usize = 32 * 1024 * 1024;
const _: () = assert!(SYNC_CHUNK_BYTES < 1024 * 1024);
const UNATTENDED_USER_INPUT_ANSWER: &str =
    "The user is not watching this turn. Proceed with your best judgment and state the assumption.";

const USAGE: &str = "\
deixic-code cloud <command> [options]

Commands:
  up                    Bind this repository to a remote workspace, sync it, and attach
  attach [session-id]   Re-attach to the workspace bound to this repository (or a session id)
  pull                  Bring the remote workspace's commits back into this repository
  list                  List remote workspaces in the EvalOps workspace
  logs <session-id>     Print lifecycle events for a session
  down [session-id]     Stop the remote workspace
  login                 EvalOps login that also requests the remote-runner scopes

up options:
  --name <ws>           Workspace name (default: the repository directory name)
  --ttl <duration>      Session TTL, for example 8h (default: 8h)
  --model <id>          Model for the hosted session
  --resync              Re-upload the local snapshot into an existing workspace
  --no-attach           Return after the workspace is ready

Inside an attached session: type a prompt to run a turn; /detach leaves it
running; /status prints the session; /exit stops the workspace.

Shared options:
  --workspace <id>      EvalOps workspace id (default: the stored login)
  --org <id>            EvalOps organization id
  --token <token>       EvalOps access token
  --base-url <url>      Remote runner URL (default: https://runner.evalops.dev)
  --json                Machine-readable output

The GitHub token is read from MAESTRO_CLOUD_GITHUB_TOKEN, GH_TOKEN, GITHUB_TOKEN,
or `gh auth token`, in that order. See docs/cloud-mode.md.";

/// Entry point for `deixic-code cloud ...`.
pub async fn run_cloud(args: &[String]) -> Result<i32> {
    maestro_local_host::safety::require_vendor_network()?;
    let Some(command) = args.first().map(String::as_str) else {
        println!("{USAGE}");
        return Ok(0);
    };
    let rest = &args[1..];
    if matches!(command, "help" | "--help" | "-h") || rest.iter().any(|arg| arg == "--help") {
        println!("{USAGE}");
        return Ok(0);
    }
    let outcome = match command {
        "up" => cmd_up(rest).await,
        "attach" => cmd_attach(rest).await,
        "pull" => cmd_pull(rest).await,
        "list" => cmd_list(rest).await,
        "logs" => cmd_logs(rest).await,
        "down" | "stop" => cmd_down(rest).await,
        "login" => cmd_login().await,
        other => Err(anyhow!("unknown cloud command: {other}")),
    };
    match outcome {
        Ok(code) => Ok(code),
        Err(error) => {
            eprintln!("{error:#}");
            Ok(1)
        }
    }
}

// ---------------------------------------------------------------------------
// Option parsing and tenant resolution
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct Opts {
    flags: BTreeMap<String, String>,
    switches: Vec<String>,
    positionals: Vec<String>,
}

const VALUE_FLAGS: &[&str] = &[
    "name",
    "model",
    "ttl",
    "workspace",
    "org",
    "token",
    "base-url",
    "after",
    "limit",
    "state",
];

fn parse_opts(args: &[String]) -> Opts {
    let mut opts = Opts::default();
    let mut i = 0;
    let mut positional_only = false;
    while i < args.len() {
        let arg = &args[i];
        if positional_only {
            opts.positionals.push(arg.clone());
        } else if arg == "--" {
            positional_only = true;
        } else if let Some(body) = arg.strip_prefix("--") {
            if let Some((name, value)) = body.split_once('=') {
                opts.flags.insert(name.to_owned(), value.to_owned());
            } else if VALUE_FLAGS.contains(&body) {
                i += 1;
                let value = args.get(i).cloned().unwrap_or_default();
                opts.flags.insert(body.to_owned(), value);
            } else {
                opts.switches.push(body.to_owned());
            }
        } else if arg == "-m" {
            i += 1;
            opts.flags
                .insert("model".to_owned(), args.get(i).cloned().unwrap_or_default());
        } else {
            opts.positionals.push(arg.clone());
        }
        i += 1;
    }
    opts
}

impl Opts {
    fn flag(&self, name: &str) -> Option<String> {
        self.flags
            .get(name)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    }
    fn has(&self, name: &str) -> bool {
        self.switches.iter().any(|switch| switch == name)
    }
    fn client_opts(&self) -> ClientOpts {
        ClientOpts {
            base_url: self.flag("base-url"),
            token: self.flag("token"),
            organization_id: self.flag("org"),
            workspace_id: self.flag("workspace"),
        }
    }
}

/// Session context: Connect config plus the resolved workspace and user.
struct Tenant {
    config: Config,
    workspace_id: String,
    user_id: Option<String>,
    email: Option<String>,
}

fn resolve_tenant(opts: &Opts) -> Result<Tenant> {
    let co = opts.client_opts();
    let config = require_config(&co).map_err(annotate_auth_error)?;
    let snapshot = crate::init_cli::load_evalops_snapshot().ok().flatten();
    let workspace_id = config
        .workspace_id
        .clone()
        .or_else(|| snapshot.as_ref().and_then(|snap| snap.workspace_id.clone()))
        .ok_or_else(|| {
            anyhow!(
                "cloud mode requires a workspace id. Pass --workspace or run `deixic-code cloud login`."
            )
        })?;
    Ok(Tenant {
        config,
        workspace_id,
        user_id: snapshot.as_ref().and_then(|snap| snap.user_id.clone()),
        email: snapshot.as_ref().and_then(|snap| snap.email.clone()),
    })
}

fn annotate_auth_error(error: anyhow::Error) -> anyhow::Error {
    anyhow!("{error:#}\nRun `deixic-code cloud login` to sign in with the remote-runner scopes.")
}

/// Explains the 403 that a login without `remote-runner:*` scopes produces.
fn annotate_rpc_error(error: anyhow::Error) -> anyhow::Error {
    let text = format!("{error:#}");
    if text.contains("returned 403") {
        anyhow!(
            "{text}\nThe access token lacks the remote-runner scopes. Run `deixic-code cloud login`."
        )
    } else {
        error
    }
}

// ---------------------------------------------------------------------------
// Local repository state and workspace identity
// ---------------------------------------------------------------------------

/// A GitHub repository resolved from the `origin` remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRef {
    pub owner: String,
    pub name: String,
}

impl RepoRef {
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
    pub fn https_url(&self) -> String {
        format!("https://github.com/{}/{}.git", self.owner, self.name)
    }
}

/// Parses `owner/repo`, `https://github.com/owner/repo(.git)`,
/// `git@github.com:owner/repo(.git)`, and `ssh://git@github.com/owner/repo(.git)`.
pub fn parse_repo_ref(raw: &str) -> Result<RepoRef> {
    let value = raw.trim();
    let path = if let Some(rest) = value.strip_prefix("git@github.com:") {
        rest
    } else if let Some(rest) = value.strip_prefix("ssh://git@github.com/") {
        rest
    } else if let Some(rest) = value.strip_prefix("https://github.com/") {
        rest
    } else if let Some(rest) = value.strip_prefix("http://github.com/") {
        rest
    } else if value.contains("://") || value.contains('@') {
        bail!("only github.com repositories are supported in cloud mode: {value}");
    } else {
        value
    };
    let path = path.trim_end_matches('/').trim_end_matches(".git");
    let mut parts = path.split('/');
    let owner = parts.next().unwrap_or_default();
    let name = parts.next().unwrap_or_default();
    let valid = |segment: &str| {
        !segment.is_empty()
            && segment
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    };
    if !valid(owner) || !valid(name) || parts.next().is_some() {
        bail!("cannot parse a GitHub repository from {value}; expected owner/repo");
    }
    Ok(RepoRef {
        owner: owner.to_owned(),
        name: name.to_owned(),
    })
}

/// What `cloud up` needs to know about the local checkout.
#[derive(Debug, Clone)]
struct LocalRepo {
    root: PathBuf,
    repo: RepoRef,
    default_branch: String,
    head: String,
    base: String,
}

fn git_in(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .with_context(|| format!("run git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn inspect_local_repo() -> Result<LocalRepo> {
    let cwd = std::env::current_dir().context("read the current directory")?;
    let root = PathBuf::from(git_in(&cwd, &["rev-parse", "--show-toplevel"])?);
    let origin = git_in(&root, &["remote", "get-url", "origin"])
        .map_err(|_| anyhow!("the repository has no `origin` remote; cloud mode needs one"))?;
    let repo = parse_repo_ref(&origin)?;
    let default_branch = git_in(
        &root,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    )
    .ok()
    .and_then(|value| value.strip_prefix("origin/").map(str::to_owned))
    .unwrap_or_else(|| "main".to_owned());
    let head = git_in(&root, &["rev-parse", "HEAD"])?;
    let base = git_in(
        &root,
        &["merge-base", "HEAD", &format!("origin/{default_branch}")],
    )
    .map_err(|error| anyhow!("{error:#}\nRun `git fetch origin {default_branch}` and retry."))?;
    Ok(LocalRepo {
        root,
        repo,
        default_branch,
        head,
        base,
    })
}

/// Workspace name: `--name` or the repository directory basename, lowercased
/// and restricted to `[a-z0-9-]`.
pub fn workspace_name(explicit: Option<&str>, root: &Path) -> String {
    let raw = explicit
        .map(str::to_owned)
        .or_else(|| {
            root.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "workspace".to_owned());
    let mut name = String::new();
    let mut last_dash = true;
    for ch in raw.chars().take(64) {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            name.push(lower);
            last_dash = false;
        } else if !last_dash {
            name.push('-');
            last_dash = true;
        }
    }
    let name = name.trim_matches('-');
    if name.is_empty() {
        "workspace".to_owned()
    } else {
        name.to_owned()
    }
}

pub fn sha256_hex(input: &str) -> String {
    use std::fmt::Write as _;
    let digest = Sha256::digest(input.as_bytes());
    let mut hex = String::with_capacity(64);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Stable identity of a remote workspace. It is both the operating-thread id
/// (`maestroSessionId`, which Runner Host uses to resolve the thread routes)
/// and the `CreateRunnerSession` idempotency key, so `cloud up` from any
/// machine resolves the same live session.
pub fn workspace_thread_id(org: &str, workspace: &str, user: &str, name: &str) -> String {
    format!(
        "cloud-{}",
        &sha256_hex(&format!("{org}\n{workspace}\n{user}\n{name}"))[..24]
    )
}

fn github_token() -> Result<String> {
    for name in ["MAESTRO_CLOUD_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(value) = std::env::var(name) {
            let value = value.trim().to_owned();
            if !value.is_empty() {
                return Ok(value);
            }
        }
    }
    let output = Command::new("gh")
        .args(["auth", "token"])
        .output()
        .context("run `gh auth token`")?;
    let token = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if !output.status.success() || token.is_empty() {
        bail!(
            "no GitHub token found. Set MAESTRO_CLOUD_GITHUB_TOKEN, GH_TOKEN, or GITHUB_TOKEN, or run `gh auth login` locally."
        );
    }
    Ok(token)
}

fn parse_ttl_minutes(raw: Option<String>) -> Result<u64> {
    let Some(raw) = raw else {
        return Ok(DEFAULT_TTL_MINUTES);
    };
    let minutes =
        crate::remote_cli::parse_remote_duration_minutes(Some(raw.as_str()), DEFAULT_TTL_MINUTES)?;
    if minutes == 0 || minutes > 1440 {
        bail!("--ttl must be between 1m and 24h");
    }
    Ok(minutes)
}

// ---------------------------------------------------------------------------
// Steps (ExecuteRunnerSessionStep, GENERIC)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct StepReceipt {
    exit_code: i32,
    stdout: String,
    stderr: String,
}

/// One bounded argv run inside the sandbox. Secrets travel in `stdin` only.
#[derive(Debug, Clone)]
pub struct Step {
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    pub stdin: Option<Vec<u8>>,
    pub timeout_seconds: u32,
}

impl Step {
    fn new(argv: &[&str], cwd: Option<&str>, timeout_seconds: u32) -> Self {
        Self {
            argv: argv.iter().map(|arg| (*arg).to_owned()).collect(),
            cwd: cwd.map(str::to_owned),
            stdin: None,
            timeout_seconds,
        }
    }
    fn with_stdin(mut self, stdin: Vec<u8>) -> Self {
        self.stdin = Some(stdin);
        self
    }
    fn sh(script: &str, cwd: Option<&str>, timeout_seconds: u32) -> Self {
        Self::new(&["sh", "-ec", script], cwd, timeout_seconds)
    }
}

async fn run_step(tenant: &Tenant, session_id: &str, step: &Step) -> Result<StepReceipt> {
    let mut config = tenant.config.clone();
    config.timeout_ms = u64::from(step.timeout_seconds) * 1000 + STEP_HTTP_SLACK_MS;
    config.max_attempts = 1;
    let body = strip_null(json!({
        "organizationId": config.organization_id,
        "workspaceId": tenant.workspace_id,
        "sessionId": session_id,
        "idempotencyKey": Uuid::new_v4().to_string(),
        "argv": step.argv,
        "cwd": step.cwd,
        "timeoutSeconds": step.timeout_seconds,
        "operation": STEP_OPERATION_GENERIC,
        "stdin": step
            .stdin
            .as_ref()
            .map(|bytes| base64::engine::general_purpose::STANDARD.encode(bytes)),
    }));
    let payload = post(&config, STEP_PATH, body)
        .await
        .map_err(annotate_rpc_error)?;
    let receipt = payload
        .get("receipt")
        .ok_or_else(|| anyhow!("remote runner step returned no receipt"))?;
    Ok(StepReceipt {
        exit_code: receipt
            .get("exitCode")
            .or_else(|| receipt.get("exit_code"))
            .and_then(Value::as_i64)
            .unwrap_or(-1) as i32,
        stdout: first_str(receipt, &["stdout"]).unwrap_or_default(),
        stderr: first_str(receipt, &["stderr"]).unwrap_or_default(),
    })
}

async fn run_step_ok(
    tenant: &Tenant,
    session_id: &str,
    step: &Step,
    label: &str,
) -> Result<StepReceipt> {
    let receipt = run_step(tenant, session_id, step).await?;
    if receipt.exit_code != 0 {
        bail!(
            "{label} failed in the sandbox (exit {}): {}",
            receipt.exit_code,
            summarize_output(&receipt)
        );
    }
    Ok(receipt)
}

fn summarize_output(receipt: &StepReceipt) -> String {
    let text = if receipt.stderr.trim().is_empty() {
        receipt.stdout.trim()
    } else {
        receipt.stderr.trim()
    };
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(5)..].join(" | ")
}

// ---------------------------------------------------------------------------
// Repository sync (2.4 in docs/cloud-mode.md)
// ---------------------------------------------------------------------------

/// The local snapshot that `cloud up` uploads: local commits as a bundle,
/// the working-tree diff, and untracked files as a tar.
#[derive(Debug, Default)]
struct SyncPayload {
    bundle: Option<Vec<u8>>,
    patch: Vec<u8>,
    untracked: Option<Vec<u8>>,
}

fn build_sync_payload(local: &LocalRepo) -> Result<SyncPayload> {
    let mut payload = SyncPayload::default();
    if local.head != local.base {
        let dir = tempfile::tempdir().context("create a temp dir for the bundle")?;
        let bundle_path = dir.path().join("local.bundle");
        git_in(
            &local.root,
            &[
                "bundle",
                "create",
                &bundle_path.to_string_lossy(),
                &format!("{}..HEAD", local.base),
            ],
        )?;
        payload.bundle = Some(std::fs::read(&bundle_path).context("read the bundle")?);
    }
    let diff = Command::new("git")
        .arg("-C")
        .arg(&local.root)
        .args(["diff", "--binary", "HEAD"])
        .output()
        .context("run git diff")?;
    if !diff.status.success() {
        bail!(
            "git diff failed: {}",
            String::from_utf8_lossy(&diff.stderr).trim()
        );
    }
    payload.patch = diff.stdout;
    let untracked = git_in(
        &local.root,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?;
    if !untracked.is_empty() {
        let tar = Command::new("tar")
            .arg("-C")
            .arg(&local.root)
            .args(["--null", "-T", "-", "-czf", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .context("spawn tar for untracked files")?;
        let mut child = tar;
        {
            let mut stdin = child.stdin.take().context("tar stdin")?;
            stdin
                .write_all(untracked.as_bytes())
                .context("write untracked list to tar")?;
        }
        let output = child.wait_with_output().context("collect untracked tar")?;
        if !output.status.success() {
            bail!("tar of untracked files failed");
        }
        payload.untracked = Some(output.stdout);
    }
    Ok(payload)
}

/// Serializes the payload as a plain tar with fixed member names so the
/// sandbox side is one `tar -xf`. Uses the system `tar` so no new crate is
/// needed.
fn sync_archive(payload: &SyncPayload) -> Result<Vec<u8>> {
    let dir = tempfile::tempdir().context("create a temp dir for the sync archive")?;
    let mut members: Vec<&str> = Vec::new();
    if let Some(bundle) = &payload.bundle {
        std::fs::write(dir.path().join("local.bundle"), bundle).context("write local.bundle")?;
        members.push("local.bundle");
    }
    std::fs::write(dir.path().join("working.patch"), &payload.patch)
        .context("write working.patch")?;
    members.push("working.patch");
    if let Some(untracked) = &payload.untracked {
        std::fs::write(dir.path().join("untracked.tgz"), untracked)
            .context("write untracked.tgz")?;
        members.push("untracked.tgz");
    }
    let output = Command::new("tar")
        .arg("-C")
        .arg(dir.path())
        .args(["-cf", "-"])
        .args(&members)
        .output()
        .context("run tar for the sync archive")?;
    if !output.status.success() {
        bail!(
            "tar failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

/// Splits `bytes` into stdin-sized chunks.
pub fn sync_chunks(bytes: &[u8]) -> Vec<&[u8]> {
    bytes.chunks(SYNC_CHUNK_BYTES).collect()
}

/// Sandbox-side steps that turn the uploaded archive into a checkout on
/// `cloud/<name>` at the local HEAD with the working tree applied.
pub fn materialize_steps(
    local_head: &str,
    local_base: &str,
    has_bundle: bool,
    name: &str,
) -> Vec<Step> {
    let mut steps = vec![
        Step::sh(
            &format!(
                "mkdir -p {CLOUD_SYNC_DIR}/sync && tar -xf {CLOUD_SYNC_DIR}/sync.tar -C {CLOUD_SYNC_DIR}/sync"
            ),
            None,
            SHORT_STEP_TIMEOUT_SECONDS,
        ),
        Step::new(
            &["git", "checkout", "-q", "--detach", local_base],
            Some(CLOUD_REPO_DIR),
            SHORT_STEP_TIMEOUT_SECONDS,
        ),
    ];
    if has_bundle {
        steps.push(Step::new(
            &[
                "git",
                "bundle",
                "unbundle",
                &format!("{CLOUD_SYNC_DIR}/sync/local.bundle"),
            ],
            Some(CLOUD_REPO_DIR),
            SHORT_STEP_TIMEOUT_SECONDS,
        ));
    }
    steps.push(Step::new(
        &["git", "switch", "-C", &format!("cloud/{name}"), local_head],
        Some(CLOUD_REPO_DIR),
        SHORT_STEP_TIMEOUT_SECONDS,
    ));
    steps.push(Step::sh(
        &format!(
            "if [ -s {CLOUD_SYNC_DIR}/sync/working.patch ]; then git apply --binary --whitespace=nowarn {CLOUD_SYNC_DIR}/sync/working.patch; fi; \
             if [ -f {CLOUD_SYNC_DIR}/sync/untracked.tgz ]; then tar -xzf {CLOUD_SYNC_DIR}/sync/untracked.tgz -C {CLOUD_REPO_DIR}; fi; \
             rm -rf {CLOUD_SYNC_DIR}/sync {CLOUD_SYNC_DIR}/sync.tar; printf '%s\\n' '{local_head}' > {CLOUD_SYNC_DIR}/synced"
        ),
        Some(CLOUD_REPO_DIR),
        SHORT_STEP_TIMEOUT_SECONDS,
    ));
    steps
}

async fn sync_repository(
    tenant: &Tenant,
    session_id: &str,
    local: &LocalRepo,
    name: &str,
    github_token: &str,
    emit: &Emitter,
) -> Result<()> {
    let git_name = tenant
        .email
        .as_deref()
        .and_then(|email| email.split('@').next())
        .filter(|value| !value.is_empty())
        .unwrap_or("deixic-code-cloud")
        .to_owned();
    let git_email = tenant
        .email
        .clone()
        .unwrap_or_else(|| "deixic-code-cloud@users.noreply.github.com".to_owned());

    emit.status("gh auth login");
    run_step_ok(
        tenant,
        session_id,
        &Step::new(
            &[
                "gh",
                "auth",
                "login",
                "--hostname",
                "github.com",
                "--with-token",
            ],
            None,
            SHORT_STEP_TIMEOUT_SECONDS,
        )
        .with_stdin(github_token.as_bytes().to_vec()),
        "gh auth login",
    )
    .await?;
    for (label, step) in [
        (
            "gh auth setup-git",
            Step::new(
                &["gh", "auth", "setup-git", "--hostname", "github.com"],
                None,
                SHORT_STEP_TIMEOUT_SECONDS,
            ),
        ),
        (
            "git identity",
            Step::sh(
                &format!(
                    "git config --global user.name '{git_name}' && git config --global user.email '{git_email}'"
                ),
                None,
                SHORT_STEP_TIMEOUT_SECONDS,
            ),
        ),
    ] {
        emit.status(label);
        run_step_ok(tenant, session_id, &step, label).await?;
    }

    emit.status(&format!(
        "git clone {} ({})",
        local.repo.slug(),
        local.default_branch
    ));
    run_step_ok(
        tenant,
        session_id,
        &Step::sh(
            &format!(
                "rm -rf {CLOUD_REPO_DIR} {CLOUD_SYNC_DIR} && mkdir -p {CLOUD_SYNC_DIR} && \
                 git clone -q --filter=blob:none --single-branch --branch '{}' '{}' {CLOUD_REPO_DIR}",
                local.default_branch,
                local.repo.https_url()
            ),
            None,
            CLONE_TIMEOUT_SECONDS,
        ),
        "git clone",
    )
    .await?;

    let payload = build_sync_payload(local)?;
    let archive = sync_archive(&payload)?;
    if archive.len() > SYNC_MAX_BYTES {
        bail!(
            "the local snapshot is {} MiB; phase 1 uploads at most {} MiB. Commit and push large changes, or shrink the working tree.",
            archive.len() / (1024 * 1024),
            SYNC_MAX_BYTES / (1024 * 1024)
        );
    }
    let chunks = sync_chunks(&archive);
    emit.status(&format!(
        "uploading snapshot: {} KiB in {} chunk(s)",
        archive.len() / 1024,
        chunks.len()
    ));
    for chunk in chunks {
        run_step_ok(
            tenant,
            session_id,
            &Step::sh(
                &format!("cat >> {CLOUD_SYNC_DIR}/sync.tar"),
                None,
                SHORT_STEP_TIMEOUT_SECONDS,
            )
            .with_stdin(chunk.to_vec()),
            "snapshot upload",
        )
        .await?;
    }
    emit.status("materializing the checkout");
    for step in materialize_steps(&local.head, &local.base, payload.bundle.is_some(), name) {
        run_step_ok(tenant, session_id, &step, "materialize").await?;
    }
    Ok(())
}

async fn synced_marker(tenant: &Tenant, session_id: &str) -> Result<Option<String>> {
    let receipt = run_step(
        tenant,
        session_id,
        &Step::sh(
            &format!("cat {CLOUD_SYNC_DIR}/synced"),
            None,
            SHORT_STEP_TIMEOUT_SECONDS,
        ),
    )
    .await?;
    Ok((receipt.exit_code == 0)
        .then(|| receipt.stdout.trim().to_owned())
        .filter(|value| !value.is_empty()))
}

// ---------------------------------------------------------------------------
// Operating-thread routes: binding, turns, replay, responses
// ---------------------------------------------------------------------------

struct Emitter {
    json: bool,
}

impl Emitter {
    fn status(&self, message: &str) {
        if self.json {
            let _ = print_json(&json!({"type": "status", "message": message}));
        } else {
            eprintln!("[cloud] {message}");
        }
    }
}

/// Runtime binding for the session's operating thread.
struct ThreadBinding {
    thread_id: String,
    runtime_generation: u64,
}

fn thread_route(thread_id: &str, suffix: &str) -> String {
    format!(
        "/internal/v1/operating-threads/{}/{suffix}",
        urlencoding::encode(thread_id)
    )
}

/// Waits until Runner Host reports a resident runtime generation for the
/// thread. `503 runtime_not_ready` and `404` are retried until the deadline.
async fn wait_thread_binding(tenant: &Tenant, thread_id: &str) -> Result<ThreadBinding> {
    let deadline = Instant::now() + BINDING_TIMEOUT;
    let mut last_error;
    loop {
        let body = json!({
            "organizationId": tenant.config.organization_id,
            "workspaceId": tenant.workspace_id,
            "verifyResident": true,
        });
        match post(&tenant.config, &thread_route(thread_id, "binding"), body).await {
            Ok(payload) => {
                let generation = payload
                    .get("runtime_generation")
                    .or_else(|| payload.get("runtimeGeneration"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                if generation > 0 {
                    return Ok(ThreadBinding {
                        thread_id: thread_id.to_owned(),
                        runtime_generation: generation,
                    });
                }
                last_error = anyhow!("binding returned runtime generation 0");
            }
            Err(error) => {
                if format!("{error:#}").contains("returned 403") {
                    return Err(annotate_rpc_error(error));
                }
                last_error = error;
            }
        }
        if Instant::now() >= deadline {
            return Err(anyhow!(
                "the hosted runtime did not bind within {}s: {last_error:#}",
                BINDING_TIMEOUT.as_secs()
            ));
        }
        tokio::time::sleep(BINDING_POLL).await;
    }
}

/// Appends one `user_message` turn. Returns the turn id and the cursor from
/// which replay must start.
async fn append_turn(
    tenant: &Tenant,
    binding: &ThreadBinding,
    content: String,
) -> Result<(String, u64)> {
    let turn_id = format!("cloud-turn-{}", Uuid::new_v4());
    let body = json!({
        "organizationId": tenant.config.organization_id,
        "workspaceId": tenant.workspace_id,
        "runtimeGeneration": binding.runtime_generation,
        "turnId": turn_id,
        "kind": "user_message",
        "content": content,
        "attachments": [],
    });
    let payload = post(
        &tenant.config,
        &thread_route(&binding.thread_id, "turns"),
        body,
    )
    .await
    .map_err(annotate_rpc_error)?;
    let cursor = payload
        .get("accepted_cursor")
        .or_else(|| payload.get("cursor"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Ok((turn_id, cursor.saturating_sub(1)))
}

/// One safe thread event from `/events/replay`.
#[derive(Debug, Clone)]
struct ThreadEvent {
    cursor: u64,
    turn_id: String,
    kind: String,
    text: String,
    error_code: String,
    request_id: Option<String>,
    request_type: Option<String>,
    tool_name: Option<String>,
}

fn parse_thread_events(payload: &Value) -> Vec<ThreadEvent> {
    payload
        .get("events")
        .and_then(Value::as_array)
        .map(|events| {
            events
                .iter()
                .map(|event| ThreadEvent {
                    cursor: event
                        .get("source_cursor")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    turn_id: first_str(event, &["turn_id", "turnId"]).unwrap_or_default(),
                    kind: first_str(event, &["kind"]).unwrap_or_default(),
                    text: first_str(event, &["safe_text", "safeText"]).unwrap_or_default(),
                    error_code: first_str(event, &["error_code", "errorCode"]).unwrap_or_default(),
                    request_id: first_str(event, &["request_id", "requestId"]),
                    request_type: first_str(event, &["request_type", "requestType"]),
                    tool_name: first_str(event, &["tool_name", "toolName", "request_tool"]),
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn replay_events(
    tenant: &Tenant,
    binding: &ThreadBinding,
    after_cursor: u64,
) -> Result<(Vec<ThreadEvent>, u64, Value)> {
    let body = json!({
        "organizationId": tenant.config.organization_id,
        "workspaceId": tenant.workspace_id,
        "runtimeGeneration": binding.runtime_generation,
        "afterCursor": after_cursor,
    });
    let payload = post(
        &tenant.config,
        &thread_route(&binding.thread_id, "events/replay"),
        body,
    )
    .await
    .map_err(annotate_rpc_error)?;
    let next = payload
        .get("next_cursor")
        .or_else(|| payload.get("nextCursor"))
        .and_then(Value::as_u64)
        .unwrap_or(after_cursor);
    let snapshot = payload.get("snapshot").cloned().unwrap_or(Value::Null);
    Ok((parse_thread_events(&payload), next, snapshot))
}

/// Answers an approval or user-input request while the user is attached.
/// Approvals are granted: the sandbox is isolated, and the user sees every
/// `tool_proposed` event as it happens and can `/detach` or `/exit`.
async fn respond_to_request(
    tenant: &Tenant,
    binding: &ThreadBinding,
    turn_id: &str,
    event: &ThreadEvent,
) -> Result<()> {
    let (Some(request_id), Some(request_type)) =
        (event.request_id.as_deref(), event.request_type.as_deref())
    else {
        return Ok(());
    };
    let (action, text) = match request_type {
        "approval" => ("approve", String::new()),
        "user_input" => ("answer", UNATTENDED_USER_INPUT_ANSWER.to_owned()),
        _ => return Ok(()),
    };
    let body = json!({
        "organizationId": tenant.config.organization_id,
        "workspaceId": tenant.workspace_id,
        "runtimeGeneration": binding.runtime_generation,
        "turnId": turn_id,
        "requestId": request_id,
        "callId": "",
        "requestType": request_type,
        "action": action,
        "text": text,
        "isError": false,
        "idempotencyKey": format!("cloud-response:{request_id}"),
    });
    post(
        &tenant.config,
        &thread_route(&binding.thread_id, "responses"),
        body,
    )
    .await
    .map(|_| ())
    .map_err(annotate_rpc_error)
}

/// Outcome of following one turn to its end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnEnd {
    Completed,
    Failed(String),
    Interrupted(String),
    /// The user detached (Ctrl-C); the turn keeps running in the sandbox.
    Detached,
}

/// Classifies a replay event. `None` means the turn is still running.
pub fn classify_turn_event(kind: &str, error_code: &str, text: &str) -> Option<TurnEnd> {
    match kind {
        "turn_completed" => Some(TurnEnd::Completed),
        "turn_failed" => Some(TurnEnd::Failed(if error_code.is_empty() {
            text.to_owned()
        } else {
            format!("{error_code}: {text}")
        })),
        "turn_interrupted" => Some(TurnEnd::Interrupted(text.to_owned())),
        _ => None,
    }
}

/// Polls replay until the turn ends or Ctrl-C. Prints assistant deltas and
/// tool proposals and answers requests.
async fn follow_turn(
    tenant: &Tenant,
    binding: &ThreadBinding,
    turn_id: &str,
    mut cursor: u64,
    emit: &Emitter,
) -> Result<TurnEnd> {
    let mut line_open = false;
    let mut answered: Vec<String> = Vec::new();
    let mut last_progress = Instant::now();
    loop {
        let replay = tokio::select! {
            biased;
            _ = tokio::signal::ctrl_c() => {
                if line_open && !emit.json {
                    println!();
                }
                return Ok(TurnEnd::Detached);
            }
            replay = replay_events(tenant, binding, cursor) => replay?,
        };
        let (events, next, snapshot) = replay;
        if !events.is_empty() {
            last_progress = Instant::now();
        }
        for event in &events {
            if emit.json {
                let _ = print_json(&json!({
                    "type": "event",
                    "cursor": event.cursor,
                    "turn_id": event.turn_id,
                    "kind": event.kind,
                    "text": event.text,
                    "error_code": event.error_code,
                    "tool": event.tool_name,
                }));
            }
            if !event.turn_id.is_empty() && event.turn_id != turn_id {
                continue;
            }
            match event.kind.as_str() {
                "assistant_text_delta" if !emit.json => {
                    print!("{}", event.text);
                    let _ = io::stdout().flush();
                    line_open = !event.text.ends_with('\n');
                }
                "tool_proposed" if !emit.json => {
                    if line_open {
                        println!();
                        line_open = false;
                    }
                    eprintln!("[tool] {}", event.tool_name.as_deref().unwrap_or("-"));
                }
                "approval_required" | "input_required" => {
                    if let Some(id) = event.request_id.as_deref() {
                        if !answered.iter().any(|seen| seen == id) {
                            respond_to_request(tenant, binding, turn_id, event).await?;
                            answered.push(id.to_owned());
                        }
                    }
                }
                _ => {}
            }
            if let Some(end) = classify_turn_event(&event.kind, &event.error_code, &event.text) {
                if line_open && !emit.json {
                    println!();
                }
                return Ok(end);
            }
        }
        if let Some(error) = snapshot
            .get("terminal_error")
            .filter(|error| !error.is_null())
        {
            if line_open && !emit.json {
                println!();
            }
            let message = first_str(error, &["display_safe_message", "displaySafeMessage"])
                .unwrap_or_default();
            let code = first_str(error, &["code"]).unwrap_or_default();
            return Ok(TurnEnd::Failed(format!("{code}: {message}")));
        }
        cursor = next.max(cursor);
        if last_progress.elapsed() > REPLAY_IDLE_TIMEOUT {
            if line_open && !emit.json {
                println!();
            }
            bail!(
                "no thread events for {}s; the session is still running (re-attach with `deixic-code cloud attach`)",
                REPLAY_IDLE_TIMEOUT.as_secs()
            );
        }
        tokio::time::sleep(REPLAY_POLL).await;
    }
}

fn active_turn_id(snapshot: &Value) -> Option<String> {
    first_str(snapshot, &["active_turn_id", "activeTurnId"]).or_else(|| {
        snapshot
            .get("turns")
            .and_then(Value::as_array)
            .and_then(|turns| turns.last())
            .filter(|turn| {
                first_str(turn, &["phase"]).is_some_and(|phase| {
                    !matches!(phase.as_str(), "completed" | "failed" | "interrupted")
                })
            })
            .and_then(|turn| first_str(turn, &["turn_id", "turnId"]))
    })
}

// ---------------------------------------------------------------------------
// The attached REPL
// ---------------------------------------------------------------------------

/// What a line typed at the REPL means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplInput {
    Prompt(String),
    Detach,
    Exit,
    Status,
    Help,
    Empty,
}

pub fn parse_repl_input(line: &str) -> ReplInput {
    match line.trim() {
        "" => ReplInput::Empty,
        "/detach" | "/d" => ReplInput::Detach,
        "/exit" | "/quit" | "/stop" => ReplInput::Exit,
        "/status" => ReplInput::Status,
        "/help" | "/?" => ReplInput::Help,
        other => ReplInput::Prompt(other.to_owned()),
    }
}

const REPL_HELP: &str = "\
Type a prompt and press Enter to run a turn in the remote workspace.
  /detach   leave the workspace running and return to the shell
  /status   print the session state and expiry
  /exit     stop the remote workspace
  Ctrl-C    detach from a running turn (the turn keeps running)";

/// Returns `true` when the user asked to stop the workspace.
async fn attached_repl(
    tenant: &Tenant,
    session: &Session,
    thread_id: &str,
    emit: &Emitter,
) -> Result<bool> {
    let binding = wait_thread_binding(tenant, thread_id).await?;
    // Follow a turn that is still running from a previous attach.
    let (_events, _next, snapshot) = replay_events(tenant, &binding, 0).await?;
    if let Some(turn_id) = active_turn_id(&snapshot) {
        emit.status("following the running turn (Ctrl-C detaches)");
        match follow_turn(tenant, &binding, &turn_id, 0, emit).await? {
            TurnEnd::Detached => return Ok(false),
            TurnEnd::Failed(reason) | TurnEnd::Interrupted(reason) => {
                eprintln!("turn ended: {reason}");
            }
            TurnEnd::Completed => {}
        }
    }
    println!(
        "attached to {} ({}); tools run in {CLOUD_REPO_DIR}. /help for commands.",
        session.id,
        session.state.as_deref().unwrap_or("-")
    );
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        print!("> ");
        let _ = io::stdout().flush();
        let line = tokio::select! {
            biased;
            _ = tokio::signal::ctrl_c() => {
                println!();
                return Ok(false);
            }
            line = lines.next_line() => line.context("read stdin")?,
        };
        let Some(line) = line else {
            return Ok(false);
        };
        match parse_repl_input(&line) {
            ReplInput::Empty => {}
            ReplInput::Help => println!("{REPL_HELP}"),
            ReplInput::Detach => return Ok(false),
            ReplInput::Exit => return Ok(true),
            ReplInput::Status => {
                let current = get_session(
                    &session.id,
                    &ClientOpts {
                        base_url: Some(tenant.config.base_url.clone()),
                        token: Some(tenant.config.token.clone()),
                        organization_id: Some(tenant.config.organization_id.clone()),
                        workspace_id: Some(tenant.workspace_id.clone()),
                    },
                )
                .await
                .map_err(annotate_rpc_error)?;
                println!(
                    "{}  {}  expires {}",
                    current.id,
                    current.state.as_deref().unwrap_or("-"),
                    current.expires_at.as_deref().unwrap_or("-")
                );
            }
            ReplInput::Prompt(prompt) => {
                let (turn_id, cursor) = append_turn(tenant, &binding, prompt).await?;
                match follow_turn(tenant, &binding, &turn_id, cursor, emit).await? {
                    TurnEnd::Completed => {}
                    TurnEnd::Detached => {
                        eprintln!(
                            "detached; the turn keeps running. Re-attach with `deixic-code cloud attach`."
                        );
                        return Ok(false);
                    }
                    TurnEnd::Failed(reason) | TurnEnd::Interrupted(reason) => {
                        eprintln!("turn ended: {reason}");
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn tenant_client_opts(tenant: &Tenant) -> ClientOpts {
    ClientOpts {
        base_url: Some(tenant.config.base_url.clone()),
        token: Some(tenant.config.token.clone()),
        organization_id: Some(tenant.config.organization_id.clone()),
        workspace_id: Some(tenant.workspace_id.clone()),
    }
}

fn tenant_user(tenant: &Tenant) -> String {
    tenant
        .user_id
        .clone()
        .or_else(|| tenant.email.clone())
        .unwrap_or_else(|| "unknown".to_owned())
}

async fn cmd_up(args: &[String]) -> Result<i32> {
    let opts = parse_opts(args);
    let emit = Emitter {
        json: opts.has("json"),
    };
    let tenant = resolve_tenant(&opts)?;
    let local = inspect_local_repo()?;
    let name = workspace_name(opts.flag("name").as_deref(), &local.root);
    let thread_id = workspace_thread_id(
        &tenant.config.organization_id,
        &tenant.workspace_id,
        &tenant_user(&tenant),
        &name,
    );
    let ttl_minutes = parse_ttl_minutes(opts.flag("ttl"))?;
    let github_token = github_token()?;

    let create = strip_null(json!({
        "organizationId": tenant.config.organization_id,
        "workspaceId": tenant.workspace_id,
        "userId": tenant.user_id,
        "idempotencyKey": thread_id,
        "maestroSessionId": thread_id,
        "runnerProfile": "maestro-standard",
        "model": opts.flag("model"),
        "ttlMinutes": ttl_minutes,
        "metadata": {
            "cloud_mode": "v1",
            "cloud_workspace_name": name,
            "cloud_repo": local.repo.slug(),
        },
    }));
    let payload = post(&tenant.config, CREATE_PATH, create)
        .await
        .map_err(annotate_rpc_error)?;
    let session = crate::remote_cli::require_session(&payload)?;
    let replayed = payload.get("replayed").and_then(Value::as_bool) == Some(true);
    println!("session: {}  workspace: {name}", session.id);
    emit.status(if replayed {
        "reusing the live workspace"
    } else {
        "waiting for the sandbox"
    });
    let (ready, _attempts, elapsed_ms) = wait_ready(
        &session.id,
        &tenant_client_opts(&tenant),
        READY_TIMEOUT_MS,
        READY_POLL_MS,
    )
    .await
    .map_err(annotate_rpc_error)?;
    emit.status(&format!(
        "sandbox {} after {}s",
        ready.state.as_deref().unwrap_or("-"),
        elapsed_ms / 1000
    ));

    let synced = synced_marker(&tenant, &session.id).await?;
    match (&synced, opts.has("resync")) {
        (Some(head), false) => emit.status(&format!(
            "workspace already synced at {}; pass --resync to upload again",
            &head[..head.len().min(12)]
        )),
        _ => sync_repository(&tenant, &session.id, &local, &name, &github_token, &emit).await?,
    }

    if opts.has("no-attach") {
        println!("ready; attach with: deixic-code cloud attach");
        return Ok(0);
    }
    let stop = attached_repl(&tenant, &ready, &thread_id, &emit).await?;
    if stop {
        stop_session(&tenant, &session.id, "cloud_exit").await?;
        println!("workspace stopped");
    } else {
        println!("detached; re-attach with: deixic-code cloud attach");
    }
    Ok(0)
}

/// Resolves the session for `cloud attach|pull|down`: an explicit session id
/// or the live session bound to this repository.
async fn resolve_bound_session(
    tenant: &Tenant,
    explicit: Option<&str>,
    name: Option<&str>,
) -> Result<(Session, String)> {
    if let Some(id) = explicit {
        let session = get_session(id, &tenant_client_opts(tenant))
            .await
            .map_err(annotate_rpc_error)?;
        let thread_id = session
            .maestro_session_id
            .clone()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                anyhow!("session {id} has no operating thread; it was not created by cloud up")
            })?;
        return Ok((session, thread_id));
    }
    let local = inspect_local_repo()?;
    let name = workspace_name(name, &local.root);
    let thread_id = workspace_thread_id(
        &tenant.config.organization_id,
        &tenant.workspace_id,
        &tenant_user(tenant),
        &name,
    );
    let payload = post(
        &tenant.config,
        LIST_PATH,
        json!({
            "organizationId": tenant.config.organization_id,
            "workspaceId": tenant.workspace_id,
            "limit": 100,
            "offset": 0,
        }),
    )
    .await
    .map_err(annotate_rpc_error)?;
    let session = array_sessions(&payload)
        .into_iter()
        .find(|session| {
            session.maestro_session_id.as_deref() == Some(thread_id.as_str())
                && session.state.as_deref().is_some_and(|state| {
                    matches!(
                        state,
                        "RUNNER_SESSION_STATE_RUNNING"
                            | "RUNNER_SESSION_STATE_IDLE"
                            | "RUNNER_SESSION_STATE_REQUESTED"
                            | "RUNNER_SESSION_STATE_PROVISIONING"
                    )
                })
        })
        .ok_or_else(|| anyhow!("no live workspace is bound to this repository as `{name}`; run `deixic-code cloud up`"))?;
    Ok((session, thread_id))
}

async fn extend_if_needed(tenant: &Tenant, session: &Session) -> Result<()> {
    let Some(expires_at) = session.expires_at.as_deref() else {
        return Ok(());
    };
    let Ok(expires) = chrono::DateTime::parse_from_rfc3339(expires_at) else {
        return Ok(());
    };
    let remaining = (expires.with_timezone(&chrono::Utc) - chrono::Utc::now()).num_minutes();
    if remaining >= EXTEND_BELOW_MINUTES {
        return Ok(());
    }
    let additional = i64::try_from(DEFAULT_TTL_MINUTES).unwrap_or(480) - remaining.max(0);
    let _ = post(
        &tenant.config,
        EXTEND_PATH,
        json!({
            "organizationId": tenant.config.organization_id,
            "workspaceId": tenant.workspace_id,
            "sessionId": session.id,
            "additionalMinutes": additional,
            "reason": "cloud_attach",
        }),
    )
    .await;
    Ok(())
}

async fn cmd_attach(args: &[String]) -> Result<i32> {
    let opts = parse_opts(args);
    let emit = Emitter {
        json: opts.has("json"),
    };
    let tenant = resolve_tenant(&opts)?;
    let (session, thread_id) = resolve_bound_session(
        &tenant,
        opts.positionals.first().map(String::as_str),
        opts.flag("name").as_deref(),
    )
    .await?;
    extend_if_needed(&tenant, &session).await?;
    let stop = attached_repl(&tenant, &session, &thread_id, &emit).await?;
    if stop {
        stop_session(&tenant, &session.id, "cloud_exit").await?;
        println!("workspace stopped");
    } else {
        println!("detached; re-attach with: deixic-code cloud attach");
    }
    Ok(0)
}

async fn cmd_pull(args: &[String]) -> Result<i32> {
    let opts = parse_opts(args);
    let tenant = resolve_tenant(&opts)?;
    let local = inspect_local_repo()?;
    if !git_in(&local.root, &["status", "--porcelain"])?.is_empty() {
        bail!("the local working tree has changes; commit or stash them before `cloud pull`");
    }
    let name = workspace_name(opts.flag("name").as_deref(), &local.root);
    let (session, _thread_id) = resolve_bound_session(&tenant, None, Some(&name)).await?;
    let branch = format!("cloud/{name}");
    let receipt = run_step_ok(
        &tenant,
        &session.id,
        &Step::sh(
            &format!(
                "if [ -n \"$(git status --porcelain)\" ]; then git add -A && git commit -q -m 'cloud: sync working tree'; fi; \
                 git push -q --force-with-lease origin HEAD:refs/heads/{branch} && git rev-parse HEAD"
            ),
            Some(CLOUD_REPO_DIR),
            CLONE_TIMEOUT_SECONDS,
        ),
        "remote push",
    )
    .await?;
    let remote_head = receipt.stdout.trim().to_owned();
    git_in(&local.root, &["fetch", "-q", "origin", &branch])?;
    match git_in(&local.root, &["merge", "--ff-only", "FETCH_HEAD"]) {
        Ok(_) => {
            println!(
                "local {} now at {}",
                git_in(&local.root, &["rev-parse", "--abbrev-ref", "HEAD"])?,
                &remote_head[..remote_head.len().min(12)]
            );
            Ok(0)
        }
        Err(error) => {
            eprintln!("{error:#}");
            eprintln!(
                "origin/{branch} is fetched; the local branch has diverged. Merge or rebase it yourself."
            );
            Ok(1)
        }
    }
}

async fn stop_session(tenant: &Tenant, session_id: &str, reason: &str) -> Result<Session> {
    let payload = post(
        &tenant.config,
        STOP_PATH,
        json!({
            "organizationId": tenant.config.organization_id,
            "workspaceId": tenant.workspace_id,
            "sessionId": session_id,
            "reason": reason,
        }),
    )
    .await
    .map_err(annotate_rpc_error)?;
    crate::remote_cli::require_session(&payload)
}

async fn cmd_down(args: &[String]) -> Result<i32> {
    let opts = parse_opts(args);
    let tenant = resolve_tenant(&opts)?;
    let (session, _thread_id) = resolve_bound_session(
        &tenant,
        opts.positionals.first().map(String::as_str),
        opts.flag("name").as_deref(),
    )
    .await?;
    let stopped = stop_session(&tenant, &session.id, "cloud_down").await?;
    if opts.has("json") {
        print_json(&json!({"session": stopped}))?;
    } else {
        println!(
            "{}  {}",
            stopped.id,
            stopped.state.as_deref().unwrap_or("-")
        );
    }
    Ok(0)
}

async fn cmd_list(args: &[String]) -> Result<i32> {
    let opts = parse_opts(args);
    let tenant = resolve_tenant(&opts)?;
    let limit: i64 = opts
        .flag("limit")
        .map(|value| value.parse())
        .transpose()
        .context("--limit must be an integer")?
        .unwrap_or(20);
    let body = strip_null(json!({
        "organizationId": tenant.config.organization_id,
        "workspaceId": tenant.workspace_id,
        "state": opts.flag("state").map(|state| normalize_state(&state)),
        "limit": limit,
        "offset": 0,
    }));
    let payload = post(&tenant.config, LIST_PATH, body)
        .await
        .map_err(annotate_rpc_error)?;
    let sessions = array_sessions(&payload);
    if opts.has("json") {
        print_json(&json!({"sessions": sessions}))?;
    } else {
        print_table(&sessions);
    }
    Ok(0)
}

fn normalize_state(raw: &str) -> String {
    let upper = raw.trim().to_ascii_uppercase();
    if upper.starts_with("RUNNER_SESSION_STATE_") {
        upper
    } else {
        format!("RUNNER_SESSION_STATE_{upper}")
    }
}

async fn cmd_logs(args: &[String]) -> Result<i32> {
    let opts = parse_opts(args);
    let Some(id) = opts.positionals.first().cloned() else {
        bail!("Usage: deixic-code cloud logs <session-id>");
    };
    let tenant = resolve_tenant(&opts)?;
    let after: i64 = opts
        .flag("after")
        .map(|value| value.parse())
        .transpose()
        .context("--after must be an integer")?
        .unwrap_or(0);
    let limit: i64 = opts
        .flag("limit")
        .map(|value| value.parse())
        .transpose()
        .context("--limit must be an integer")?
        .unwrap_or(200);
    let payload = post(
        &tenant.config,
        EVENTS_PATH,
        json!({
            "organizationId": tenant.config.organization_id,
            "workspaceId": tenant.workspace_id,
            "sessionId": id,
            "afterSequence": after,
            "limit": limit,
        }),
    )
    .await
    .map_err(annotate_rpc_error)?;
    let events = array_events(&payload);
    if opts.has("json") {
        print_json(&json!({"events": events}))?;
        return Ok(0);
    }
    if events.is_empty() {
        println!("no events");
        return Ok(0);
    }
    for event in events {
        println!(
            "{:>5}  {}  {}",
            event
                .sequence
                .map(|s| format!("{s:.0}"))
                .unwrap_or_else(|| "-".into()),
            event.occurred_at.as_deref().unwrap_or("-"),
            event.event_type.as_deref().unwrap_or("-")
        );
    }
    Ok(0)
}

async fn cmd_login() -> Result<i32> {
    if !io::stdin().is_terminal() {
        bail!("`deixic-code cloud login` needs an interactive terminal for the browser login");
    }
    println!("Deixic Code cloud login (scopes: {CLOUD_LOGIN_SCOPES})");
    crate::init_cli::perform_evalops_login_with_scopes(CLOUD_LOGIN_SCOPES).await?;
    println!("EvalOps credentials saved. Run `deixic-code cloud list` to verify the scopes.");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_repo_refs() {
        for raw in [
            "dx-corp/mono",
            "https://github.com/dx-corp/mono",
            "https://github.com/dx-corp/mono.git",
            "git@github.com:dx-corp/mono.git",
            "ssh://git@github.com/dx-corp/mono.git",
            "  https://github.com/dx-corp/mono/  ",
        ] {
            let parsed = parse_repo_ref(raw).unwrap_or_else(|error| panic!("{raw}: {error}"));
            assert_eq!(parsed.slug(), "dx-corp/mono", "{raw}");
            assert_eq!(parsed.https_url(), "https://github.com/dx-corp/mono.git");
        }
        assert!(parse_repo_ref("https://gitlab.com/a/b").is_err());
        assert!(parse_repo_ref("dx-corp").is_err());
        assert!(parse_repo_ref("dx-corp/mono/extra").is_err());
        assert!(parse_repo_ref("dx corp/mono").is_err());
    }

    #[test]
    fn workspace_identity_is_stable_and_sanitized() {
        let root = Path::new("/Users/x/code/Mono Repo!");
        assert_eq!(workspace_name(None, root), "mono-repo");
        assert_eq!(workspace_name(Some("Feature/X"), root), "feature-x");
        assert_eq!(workspace_name(Some("!!!"), root), "workspace");
        let a = workspace_thread_id("org", "ws", "user", "mono");
        assert_eq!(a, workspace_thread_id("org", "ws", "user", "mono"));
        assert_ne!(a, workspace_thread_id("org", "ws", "other", "mono"));
        assert!(a.starts_with("cloud-") && a.len() == 30, "{a}");
    }

    #[test]
    fn sync_archive_and_chunks_round_trip() {
        let payload = SyncPayload {
            bundle: Some(vec![1, 2, 3]),
            patch: b"diff --git a/x b/x\n".to_vec(),
            untracked: None,
        };
        let archive = sync_archive(&payload).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sync.tar");
        std::fs::write(&path, &archive).unwrap();
        let listing = Command::new("tar")
            .args(["-tf"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(listing.status.success());
        let names: Vec<String> = String::from_utf8_lossy(&listing.stdout)
            .lines()
            .map(|line| line.trim_start_matches("./").to_owned())
            .collect();
        assert_eq!(names, vec!["local.bundle", "working.patch"]);
        let extracted = dir.path().join("out");
        std::fs::create_dir_all(&extracted).unwrap();
        assert!(
            Command::new("tar")
                .arg("-xf")
                .arg(&path)
                .arg("-C")
                .arg(&extracted)
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(
            std::fs::read(extracted.join("local.bundle")).unwrap(),
            vec![1, 2, 3]
        );
        let big = vec![0u8; SYNC_CHUNK_BYTES * 2 + 1];
        let chunks = sync_chunks(&big);
        assert_eq!(chunks.len(), 3);
        assert!(chunks.iter().all(|chunk| chunk.len() <= SYNC_CHUNK_BYTES));
    }

    #[test]
    fn materialize_steps_check_out_the_local_head() {
        let steps = materialize_steps("deadbeef", "cafebabe", true, "mono");
        assert_eq!(steps.len(), 5);
        assert_eq!(
            steps[1].argv,
            ["git", "checkout", "-q", "--detach", "cafebabe"]
        );
        assert_eq!(steps[2].argv[1], "bundle");
        assert_eq!(
            steps[3].argv,
            ["git", "switch", "-C", "cloud/mono", "deadbeef"]
        );
        assert!(steps[4].argv[2].contains("git apply --binary"));
        assert!(steps[4].argv[2].contains(&format!("{CLOUD_SYNC_DIR}/synced")));
        assert!(steps.iter().all(|step| step.stdin.is_none()));
        let without_bundle = materialize_steps("deadbeef", "deadbeef", false, "mono");
        assert_eq!(without_bundle.len(), 4);
        assert!(
            without_bundle
                .iter()
                .all(|step| step.argv.get(1).map(String::as_str) != Some("bundle"))
        );
    }

    #[test]
    fn classifies_turn_end_events() {
        assert_eq!(classify_turn_event("assistant_text_delta", "", "hi"), None);
        assert_eq!(
            classify_turn_event("turn_completed", "", ""),
            Some(TurnEnd::Completed)
        );
        assert_eq!(
            classify_turn_event("turn_failed", "provider_error", "boom"),
            Some(TurnEnd::Failed("provider_error: boom".to_owned()))
        );
        assert_eq!(
            classify_turn_event("turn_interrupted", "", "cancelled"),
            Some(TurnEnd::Interrupted("cancelled".to_owned()))
        );
    }

    #[test]
    fn parses_replay_events_and_active_turn() {
        let payload = json!({
            "next_cursor": 7,
            "snapshot": {"active_turn_id": "t1", "turns": []},
            "events": [
                {"source_cursor": 5, "turn_id": "t1", "kind": "assistant_text_delta", "safe_text": "hello", "error_code": ""},
                {"source_cursor": 6, "turn_id": "t1", "kind": "approval_required", "safe_text": "", "error_code": "", "request_id": "r1", "request_type": "approval", "tool_name": "bash"}
            ]
        });
        let events = parse_thread_events(&payload);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].text, "hello");
        assert_eq!(events[1].request_id.as_deref(), Some("r1"));
        assert_eq!(events[1].tool_name.as_deref(), Some("bash"));
        assert_eq!(active_turn_id(&payload["snapshot"]).as_deref(), Some("t1"));
        let finished = json!({"turns": [{"turn_id": "t2", "phase": "completed"}]});
        assert_eq!(active_turn_id(&finished), None);
        let running = json!({"turns": [{"turn_id": "t3", "phase": "running"}]});
        assert_eq!(active_turn_id(&running).as_deref(), Some("t3"));
        assert_eq!(
            thread_route("cloud-x", "events/replay"),
            "/internal/v1/operating-threads/cloud-x/events/replay"
        );
    }

    #[test]
    fn parses_repl_input_and_options() {
        assert_eq!(parse_repl_input("  "), ReplInput::Empty);
        assert_eq!(parse_repl_input("/detach"), ReplInput::Detach);
        assert_eq!(parse_repl_input("/exit"), ReplInput::Exit);
        assert_eq!(parse_repl_input("/status"), ReplInput::Status);
        assert_eq!(
            parse_repl_input(" fix the test "),
            ReplInput::Prompt("fix the test".to_owned())
        );
        let args: Vec<String> = [
            "--name",
            "mono",
            "--ttl=2h",
            "--json",
            "-m",
            "evalops/x",
            "--resync",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        let opts = parse_opts(&args);
        assert_eq!(opts.flag("name").as_deref(), Some("mono"));
        assert_eq!(opts.flag("ttl").as_deref(), Some("2h"));
        assert_eq!(opts.flag("model").as_deref(), Some("evalops/x"));
        assert!(opts.has("json") && opts.has("resync"));
        assert_eq!(normalize_state("running"), "RUNNER_SESSION_STATE_RUNNING");
    }
}
