//! Read-only GitHub snapshots and a deterministic PR-watch transition reducer.

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

#[derive(Clone, Debug)]
pub(crate) struct PullRequestRef {
    pub(crate) url: String,
    pub(crate) repo: String,
    pub(crate) number: u64,
}

impl PullRequestRef {
    pub(crate) fn parse(url: &str) -> Result<Self, String> {
        let path = url
            .strip_prefix("https://github.com/")
            .ok_or("Expected a canonical https://github.com/owner/repo/pull/number URL")?;
        let parts: Vec<_> = path.trim_end_matches('/').split('/').collect();
        let valid_name = |name: &str| {
            !name.is_empty()
                && name != "."
                && name != ".."
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        };
        if parts.len() != 4 || !valid_name(parts[0]) || !valid_name(parts[1]) || parts[2] != "pull"
        {
            return Err("Expected a canonical GitHub pull-request URL".into());
        }
        let number = parts[3]
            .parse::<u64>()
            .map_err(|_| "Invalid pull-request number")?;
        if number == 0 || parts[3] != number.to_string() {
            return Err("Invalid pull-request number".into());
        }
        let repo = format!("{}/{}", parts[0], parts[1]);
        Ok(Self {
            url: format!("https://github.com/{repo}/pull/{number}"),
            repo,
            number,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PullRequestState {
    Open,
    Merged,
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CheckState {
    Pending,
    Passed,
    Failed,
    Unknown,
}

#[derive(Clone, Debug)]
pub(crate) struct Snapshot {
    pub(crate) head: String,
    pub(crate) state: PullRequestState,
    pub(crate) checks: BTreeMap<String, CheckState>,
    pub(crate) required_checks: BTreeSet<String>,
    pub(crate) comments: BTreeSet<String>,
    pub(crate) conflicting: Option<bool>,
}

async fn gh_json(cwd: &Path, args: &[&str], check_exit: bool) -> Result<Value, String> {
    let mut command = Command::new("gh");
    if args.first() == Some(&"api") {
        command
            .arg("api")
            .args(["--hostname", "github.com"])
            .args(&args[1..]);
    } else {
        command.args(args);
    }
    command
        .current_dir(cwd)
        .env("GH_PROMPT_DISABLED", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| format!("Could not read GitHub: {error}"))?;
    let stdout = child.stdout.take().ok_or("GitHub stdout is unavailable")?;
    let stderr = child.stderr.take().ok_or("GitHub stderr is unavailable")?;
    let read = async {
        tokio::try_join!(
            read_bounded(stdout, 4 * 1024 * 1024),
            read_bounded(stderr, 16 * 1024),
            async { child.wait().await.map_err(|error| error.to_string()) }
        )
    };
    let (stdout, stderr, status) = tokio::time::timeout(Duration::from_secs(20), read)
        .await
        .map_err(|_| "GitHub read timed out".to_string())??;
    // `gh pr checks` exits 1 for failed checks and 8 for pending checks, while
    // still returning a usable JSON snapshot. Other failures stay errors.
    if !(status.success() || check_exit && matches!(status.code(), Some(1 | 8))) {
        return Err(format!(
            "GitHub read failed: {}",
            String::from_utf8_lossy(&stderr).trim()
        ));
    }
    serde_json::from_slice(&stdout).map_err(|_| "GitHub returned invalid JSON".into())
}

async fn read_bounded(mut stream: impl AsyncRead + Unpin, limit: usize) -> Result<Vec<u8>, String> {
    let mut output = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let count = stream
            .read(&mut buffer)
            .await
            .map_err(|error| error.to_string())?;
        if count == 0 {
            return Ok(output);
        }
        if output.len().saturating_add(count) > limit {
            return Err("GitHub output exceeded its size limit".into());
        }
        output.extend_from_slice(&buffer[..count]);
    }
}

fn string_field<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("GitHub snapshot is missing {key}"))
}

fn check_states(value: &Value) -> Result<BTreeMap<String, CheckState>, String> {
    let mut states = BTreeMap::new();
    for check in value.as_array().ok_or("GitHub checks are not an array")? {
        let name = string_field(check, "name")?.to_string();
        let state = match string_field(check, "bucket")? {
            "pass" => CheckState::Passed,
            "fail" | "cancel" => CheckState::Failed,
            "pending" => CheckState::Pending,
            // Skipped required checks do not establish a passing result.
            _ => CheckState::Unknown,
        };
        if let Some(prior) = states.insert(name, state) {
            if prior != state {
                return Err("GitHub returned conflicting checks with the same name".into());
            }
        }
    }
    Ok(states)
}

fn comment_ids(value: &Value, family: &str, own_login: &str) -> Result<BTreeSet<String>, String> {
    let mut ids = BTreeSet::new();
    for page in value
        .as_array()
        .ok_or("GitHub comment pages are not an array")?
    {
        for comment in page.as_array().ok_or("GitHub comments are not an array")? {
            let login = comment.pointer("/user/login").and_then(Value::as_str);
            if login.is_some_and(|login| login.eq_ignore_ascii_case(own_login)) {
                continue;
            }
            let id = comment
                .get("id")
                .and_then(Value::as_u64)
                .ok_or("GitHub comment is missing its ID")?;
            ids.insert(format!("{family}:{id}"));
        }
    }
    Ok(ids)
}

pub(crate) async fn read_snapshot(cwd: &Path, target: &PullRequestRef) -> Result<Snapshot, String> {
    // Concurrent bounded stream readers retain their buffers across awaits.
    // Keep that state on the heap so tool and HTTP futures remain small.
    Box::pin(read_snapshot_inner(cwd, target)).await
}

async fn read_snapshot_inner(cwd: &Path, target: &PullRequestRef) -> Result<Snapshot, String> {
    // Capture the head before concurrent reads, then confirm it afterwards.
    let pull = gh_json(
        cwd,
        &[
            "pr",
            "view",
            &target.url,
            "--json",
            "headRefOid,state,mergeable,baseRefName",
        ],
        false,
    )
    .await?;
    let head = string_field(&pull, "headRefOid")?.to_string();
    let state = match string_field(&pull, "state")? {
        "OPEN" => PullRequestState::Open,
        "MERGED" => PullRequestState::Merged,
        "CLOSED" => PullRequestState::Closed,
        _ => return Err("GitHub returned an unknown pull-request state".into()),
    };
    // Terminal state is enough to stop even when obsolete check reads fail.
    if state != PullRequestState::Open {
        return Ok(Snapshot {
            head,
            state,
            checks: BTreeMap::new(),
            required_checks: BTreeSet::new(),
            comments: BTreeSet::new(),
            conflicting: None,
        });
    }
    let issue_path = format!(
        "repos/{}/issues/{}/comments?per_page=100",
        target.repo, target.number
    );
    let review_path = format!(
        "repos/{}/pulls/{}/reviews?per_page=100",
        target.repo, target.number
    );
    let inline_path = format!(
        "repos/{}/pulls/{}/comments?per_page=100",
        target.repo, target.number
    );
    let checks_args = ["pr", "checks", &target.url, "--json", "name,bucket"];
    let own_args = ["api", "user"];
    let issue_args = ["api", &issue_path, "--paginate", "--slurp"];
    let review_args = ["api", &review_path, "--paginate", "--slurp"];
    let inline_args = ["api", &inline_path, "--paginate", "--slurp"];
    let (checks, own, issue, reviews, inline) = tokio::try_join!(
        gh_json(cwd, &checks_args, true),
        gh_json(cwd, &own_args, false),
        gh_json(cwd, &issue_args, false),
        gh_json(cwd, &review_args, false),
        gh_json(cwd, &inline_args, false),
    )?;
    let base = string_field(&pull, "baseRefName")?;
    let protection_path = format!(
        "repos/{}/branches/{}/protection/required_status_checks",
        target.repo,
        encode_path(base)
    );
    let rules_path = format!("repos/{}/rules/branches/{}", target.repo, encode_path(base));
    let branch_path = format!("repos/{}/branches/{}", target.repo, encode_path(base));
    let branch_args = ["api", &branch_path];
    let rules_args = ["api", &rules_path];
    let (branch, rules) = tokio::try_join!(
        gh_json(cwd, &branch_args, false),
        gh_json(cwd, &rules_args, false),
    )?;
    let protection = if branch
        .get("protected")
        .and_then(Value::as_bool)
        .ok_or("GitHub did not report branch protection status")?
    {
        gh_json(cwd, &["api", &protection_path], false).await?
    } else {
        serde_json::json!({"contexts":[],"checks":[]})
    };
    // Confirm only after every dependent read has completed.
    let confirmed = gh_json(
        cwd,
        &["pr", "view", &target.url, "--json", "headRefOid"],
        false,
    )
    .await?;
    if string_field(&confirmed, "headRefOid")? != head {
        return Err("Pull-request head changed during GitHub reads; retrying the snapshot".into());
    }
    let mut required_checks = BTreeSet::new();
    for context in protection
        .get("contexts")
        .and_then(Value::as_array)
        .ok_or("Missing required-check contexts")?
    {
        required_checks.insert(
            context
                .as_str()
                .ok_or("Invalid required-check context")?
                .to_string(),
        );
    }
    for check in protection
        .get("checks")
        .and_then(Value::as_array)
        .ok_or("Missing required checks")?
    {
        required_checks.insert(string_field(check, "context")?.to_string());
    }
    for rule in rules
        .as_array()
        .ok_or("GitHub branch rules are not an array")?
    {
        if rule.get("type").and_then(Value::as_str) == Some("required_status_checks") {
            let checks = rule
                .pointer("/parameters/required_status_checks")
                .and_then(Value::as_array)
                .ok_or("Missing ruleset required checks")?;
            for check in checks {
                required_checks.insert(string_field(check, "context")?.to_string());
            }
        }
    }
    let own_login = string_field(&own, "login")?;
    let mut comments = comment_ids(&issue, "issue", own_login)?;
    comments.extend(comment_ids(&reviews, "review", own_login)?);
    comments.extend(comment_ids(&inline, "inline", own_login)?);
    let conflicting = match string_field(&pull, "mergeable")? {
        "CONFLICTING" => Some(true),
        "MERGEABLE" => Some(false),
        _ => None,
    };
    Ok(Snapshot {
        head,
        state,
        checks: check_states(&checks)?,
        required_checks,
        comments,
        conflicting,
    })
}

fn encode_path(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

pub(crate) struct WatchBaseline {
    head: String,
    checks: BTreeMap<String, CheckState>,
    required_checks: BTreeSet<String>,
    comments: BTreeSet<String>,
    conflicting: Option<bool>,
    notified_failures: BTreeSet<String>,
    required_green: bool,
    consecutive_errors: u8,
    comments_only_wakes: u8,
    stopped: bool,
}

impl WatchBaseline {
    pub(crate) fn new(snapshot: &Snapshot) -> Self {
        Self {
            head: snapshot.head.clone(),
            checks: snapshot.checks.clone(),
            required_checks: snapshot.required_checks.clone(),
            comments: snapshot.comments.clone(),
            conflicting: snapshot.conflicting,
            notified_failures: BTreeSet::new(),
            required_green: false,
            consecutive_errors: 0,
            comments_only_wakes: 0,
            stopped: false,
        }
    }
}

pub(crate) struct WatchTransition {
    pub(crate) prompt: Option<String>,
    pub(crate) stop: bool,
}

pub(crate) fn reduce(
    baseline: &mut WatchBaseline,
    result: Result<Snapshot, String>,
) -> WatchTransition {
    if baseline.stopped {
        return WatchTransition {
            prompt: None,
            stop: true,
        };
    }
    let current = match result {
        Ok(current) => {
            baseline.consecutive_errors = 0;
            current
        }
        Err(error) => {
            baseline.consecutive_errors = baseline.consecutive_errors.saturating_add(1);
            baseline.stopped = baseline.consecutive_errors >= 15;
            return WatchTransition {
                prompt: baseline.stopped.then(|| {
                    format!("PR watch stopped after 15 consecutive GitHub read failures: {error}")
                }),
                stop: baseline.stopped,
            };
        }
    };
    if current.state != PullRequestState::Open {
        baseline.stopped = true;
        return WatchTransition {
            prompt: Some(format!(
                "Pull request {}. PR watch stopped.",
                if current.state == PullRequestState::Merged {
                    "merged"
                } else {
                    "closed"
                }
            )),
            stop: true,
        };
    }
    let mut reasons = Vec::new();
    let head_changed = baseline.head != current.head;
    let checks_changed =
        baseline.checks != current.checks || baseline.required_checks != current.required_checks;
    let conflict_changed =
        current.conflicting.is_some() && current.conflicting != baseline.conflicting;
    let material = head_changed || checks_changed || conflict_changed;
    if head_changed {
        reasons.push(format!("Head changed to {}", current.head));
        baseline.notified_failures.clear();
        baseline.required_green = false;
    }
    let failed: BTreeSet<_> = current
        .checks
        .iter()
        .filter(|(_, state)| **state == CheckState::Failed)
        .map(|(name, _)| name.clone())
        .collect();
    let new_failed: Vec<_> = failed
        .difference(&baseline.notified_failures)
        .cloned()
        .collect();
    if !new_failed.is_empty() {
        reasons.push(format!("Failed checks: {}", new_failed.join(", ")));
    }
    baseline.notified_failures = failed;
    let green = !current.required_checks.is_empty()
        && current
            .required_checks
            .iter()
            .all(|name| current.checks.get(name) == Some(&CheckState::Passed));
    let green_transition = green && !baseline.required_green;
    if green_transition {
        reasons.push(format!("Required checks passed on head {}", current.head));
    } else if checks_changed && reasons.is_empty() {
        reasons.push("Check status changed".into());
    }
    if conflict_changed {
        reasons.push(if current.conflicting == Some(true) {
            "Merge conflict detected".into()
        } else {
            "Conflict resolved".into()
        });
    }
    let new_comments = current.comments.difference(&baseline.comments).count();
    if new_comments > 0 {
        reasons.push(format!("{new_comments} new review or comment item(s)"));
    }
    if material || !new_failed.is_empty() || green_transition {
        baseline.comments_only_wakes = 0;
    } else if new_comments > 0 {
        baseline.comments_only_wakes = baseline.comments_only_wakes.saturating_add(1);
    }
    baseline.stopped = baseline.comments_only_wakes >= 10;
    if baseline.stopped {
        reasons.push("PR watch stopped after 10 consecutive comments-only wakes".into());
    }
    baseline.head = current.head;
    baseline.checks = current.checks;
    baseline.required_checks = current.required_checks;
    baseline.comments.extend(current.comments);
    if current.conflicting.is_some() {
        baseline.conflicting = current.conflicting;
    }
    baseline.required_green = green;
    WatchTransition {
        prompt: (!reasons.is_empty()).then(|| reasons.join(". ")),
        stop: baseline.stopped,
    }
}

#[cfg(test)]
#[path = "pull_request_watch_state_tests.rs"]
mod pull_request_watch_state_tests;
