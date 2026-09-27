//! One truthful `/usage` view for Maestro activity and linked subscriptions.
//! Quota is provider-reported. Local model turns and active time are separate.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Local, NaiveDate, TimeZone, Utc};
use serde_json::Value;

use crate::codex_app_server::{CodexAppServerClient, InitializeOptions};
use crate::service_connections::ConnectionStore;
use crate::session::{AppMessage, SessionEntry, SessionHeader};

#[derive(Default, Debug, PartialEq, Eq)]
struct Activity {
    model_turns: u64,
    active_secs: u64,
    claude_turns: u64,
    codex_turns: u64,
    copilot_turns: u64,
    other_turns: u64,
}

impl Activity {
    fn record_model(&mut self, model: &str) {
        self.model_turns += 1;
        match model.split_once('/').map(|(provider, _)| provider) {
            Some("claude-code") => self.claude_turns += 1,
            Some("openai-codex") => self.codex_turns += 1,
            Some("github-copilot") => self.copilot_turns += 1,
            _ => self.other_turns += 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LinkState {
    Linked,
    NotLinked,
    SignInRequired,
    WrongOwner,
    ConnectionDataUnavailable,
}

/// Render immediately from recorded activity, with one bounded official Codex read.
pub(crate) async fn report(cwd: &str) -> String {
    let today = Local::now().date_naive();
    let root = crate::session::sessions_dir(cwd)
        .parent()
        .map(Path::to_path_buf);
    let activity = match root {
        Some(root) => tokio::task::spawn_blocking(move || collect_activity(&root, today))
            .await
            .ok()
            .and_then(Result::ok),
        None => None,
    };

    let store = ConnectionStore::default_path().and_then(|path| ConnectionStore::load(&path));
    let owner = crate::credential_mode::verified_current_identity_session().ok();
    let link = |provider: &str| match &store {
        Ok(store) => match store.selected(provider, None) {
            Ok(None) => LinkState::NotLinked,
            Ok(Some(connection)) => match (&connection.owner, &owner) {
                (Some(connection_owner), Some(identity)) => {
                    if connection_owner.require_session(identity).is_ok() {
                        LinkState::Linked
                    } else {
                        LinkState::WrongOwner
                    }
                }
                (None, _) => LinkState::ConnectionDataUnavailable,
                (_, None) => LinkState::SignInRequired,
            },
            Err(_) => LinkState::ConnectionDataUnavailable,
        },
        Err(_) => LinkState::ConnectionDataUnavailable,
    };
    let claude = link("claude-code");
    let codex = link("openai-codex");
    let copilot = link("github-copilot");

    let codex_quota = if codex == LinkState::Linked {
        match crate::service_connections::selected_delegated_profile_from_env("openai-codex") {
            Ok(profile) => read_codex_quota(profile.as_deref(), cwd).await.ok(),
            Err(_) => None,
        }
    } else {
        None
    };
    format_report(
        activity.as_ref(),
        claude,
        codex,
        copilot,
        codex_quota.as_ref(),
    )
}

fn collect_activity(root: &Path, today: NaiveDate) -> std::io::Result<Activity> {
    let mut activity = Activity::default();
    let mut seen_messages = HashSet::new();
    let mut seen_events = HashSet::new();
    if !root.exists() {
        return Ok(activity);
    }
    let mut sessions = Vec::new();
    for directory in fs::read_dir(root)? {
        let directory = directory?;
        if !directory.file_type()?.is_dir() {
            continue;
        }
        for file in fs::read_dir(directory.path())? {
            let file = file?;
            if file.path().extension().is_none_or(|ext| ext != "jsonl") {
                continue;
            }
            // Old sessions cannot contain today's append-only activity.
            if file
                .metadata()?
                .modified()
                .ok()
                .and_then(|modified| modified.duration_since(SystemTime::UNIX_EPOCH).ok())
                .and_then(|duration| DateTime::<Utc>::from_timestamp(duration.as_secs() as i64, 0))
                .is_some_and(|modified| modified.with_timezone(&Local).date_naive() < today)
            {
                continue;
            }
            let path = file.path();
            let header = read_session_header(&path)?;
            sessions.push((path, header));
        }
    }
    let fork_parents = sessions
        .iter()
        .filter_map(|(_, header)| {
            let header = header.as_ref()?;
            header
                .branched_from
                .as_ref()
                .and(header.parent_session.as_ref())
                .map(|parent| (header.id.clone(), parent.clone()))
        })
        .collect::<std::collections::HashMap<_, _>>();
    for (path, header) in sessions {
        // Legacy or partially written files may contain valid entries without
        // a parseable header. Keep their prior counting behavior and isolate
        // their ordinals by file path.
        let mut origin = header
            .map(|header| header.id)
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        let mut remaining = fork_parents.len() + 1;
        while remaining > 0 {
            let Some(parent) = fork_parents.get(&origin) else {
                break;
            };
            origin.clone_from(parent);
            remaining -= 1;
        }
        collect_file(
            &path,
            today,
            &origin,
            &mut activity,
            &mut seen_messages,
            &mut seen_events,
        )?;
    }
    Ok(activity)
}

fn read_session_header(path: &Path) -> std::io::Result<Option<SessionHeader>> {
    let Some(line) = BufReader::new(File::open(path)?).lines().next() else {
        return Ok(None);
    };
    Ok(match serde_json::from_str::<SessionEntry>(&line?) {
        Ok(SessionEntry::Session(header)) => Some(header),
        _ => None,
    })
}

fn collect_file(
    path: &Path,
    today: NaiveDate,
    initial_origin: &str,
    activity: &mut Activity,
    seen_messages: &mut HashSet<(String, u64)>,
    seen_events: &mut HashSet<(String, u64)>,
) -> std::io::Result<()> {
    let mut model = String::new();
    let mut turn_started: Option<DateTime<Utc>> = None;
    let mut origin = initial_origin.to_owned();
    let mut message_ordinal = 0;
    let mut event_ordinal = 0;
    for line in BufReader::new(File::open(path)?).lines() {
        let line = line?;
        let Ok(entry) = serde_json::from_str::<SessionEntry>(&line) else {
            continue;
        };
        match entry {
            SessionEntry::Session(header) => model = header.model,
            SessionEntry::ModelChange(change) => model = change.model,
            SessionEntry::Message(message) => {
                if let AppMessage::Assistant {
                    model: response_model,
                    timestamp,
                    ..
                } = message.message
                {
                    message_ordinal += 1;
                    if !seen_messages.insert((origin.clone(), message_ordinal)) {
                        continue;
                    }
                    let time = DateTime::<Utc>::from_timestamp_millis(timestamp as i64)
                        .or_else(|| parse_time(&message.timestamp));
                    if time.is_some_and(|time| time.with_timezone(&Local).date_naive() == today) {
                        activity.record_model(response_model.as_deref().unwrap_or(&model));
                    }
                }
            }
            SessionEntry::Custom(custom) if custom.custom_type == "session_event_v1" => {
                let Some(data) = custom.data else {
                    continue;
                };
                if matches!(
                    data.get("kind").and_then(Value::as_str),
                    Some("session.forked" | "session.rewound")
                ) {
                    if let Some(session_id) = data.get("sessionId").and_then(Value::as_str) {
                        origin = session_id.to_owned();
                        message_ordinal = 0;
                        event_ordinal = 0;
                    }
                }
                event_ordinal += 1;
                if !seen_events.insert((origin.clone(), event_ordinal)) {
                    continue;
                }
                let Some(time) = data
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .and_then(parse_time)
                else {
                    continue;
                };
                match data.get("kind").and_then(Value::as_str) {
                    Some("turn.started") => turn_started = Some(time),
                    Some("turn.completed" | "turn.cancelled") => {
                        if let Some(started) = turn_started.take() {
                            let seconds = time.signed_duration_since(started).num_seconds();
                            if (0..=86_400).contains(&seconds)
                                && time.with_timezone(&Local).date_naive() == today
                            {
                                let midnight = today
                                    .and_hms_opt(0, 0, 0)
                                    .and_then(|time| time.and_local_timezone(Local).earliest())
                                    .map(|time| time.with_timezone(&Utc));
                                let counted_from = midnight.map_or(started, |day| started.max(day));
                                activity.active_secs +=
                                    time.signed_duration_since(counted_from).num_seconds() as u64;
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn parse_time(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

async fn read_codex_quota(profile: Option<&str>, cwd: &str) -> anyhow::Result<Value> {
    let identity =
        maestro_local_host::codex_identity::resolve_codex_identity(profile, Path::new(cwd))?;
    let client =
        CodexAppServerClient::spawn_with_env(None, None, Some(4_000), &identity.child_env())
            .await?;
    let result = tokio::time::timeout(Duration::from_secs(6), async {
        client.initialize(InitializeOptions::default()).await?;
        let account = client.read_account(false).await?;
        anyhow::ensure!(
            account
                .account
                .as_ref()
                .and_then(|value| value.get("type"))
                .and_then(Value::as_str)
                == Some("chatgpt"),
            "ChatGPT subscription sign-in unavailable"
        );
        client
            .request("account/rateLimits/read", None, Some(4_000))
            .await
    })
    .await;
    client.close();
    result?
}

fn format_report(
    activity: Option<&Activity>,
    claude: LinkState,
    codex: LinkState,
    copilot: LinkState,
    codex_quota: Option<&Value>,
) -> String {
    let mut rows = vec!["## Usage — Today".to_owned()];
    match activity {
        Some(activity) => {
            rows.push(format!("{} model turns", activity.model_turns));
            if activity.active_secs > 0 {
                rows.push(format!(
                    "{} tracked agent time",
                    format_duration(activity.active_secs)
                ));
            } else {
                rows.push("Agent time unavailable".to_owned());
            }
        }
        None => rows.push("Maestro activity unavailable".to_owned()),
    }

    rows.push(String::new());
    rows.push("Claude subscription".to_owned());
    rows.push(match claude {
        LinkState::Linked => unavailable_quota(activity.map(|activity| activity.claude_turns)),
        state => link_message(state),
    });

    rows.push(String::new());
    let snapshot = codex_quota.and_then(|value| {
        value
            .pointer("/rateLimitsByLimitId/codex")
            .or_else(|| value.get("rateLimits"))
    });
    let plan = snapshot
        .and_then(|value| value.get("planType"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    rows.push(match plan {
        Some(plan) => format!("ChatGPT {}", plan_label(plan)),
        None => "ChatGPT Codex".to_owned(),
    });
    match codex {
        LinkState::Linked => {
            if let Some(snapshot) = snapshot {
                let mut windows = 0;
                for (key, label) in [("primary", ""), ("secondary", "additional ")] {
                    if let Some(window) = snapshot.get(key).filter(|value| !value.is_null()) {
                        if let Some(used) = window.get("usedPercent").and_then(Value::as_f64) {
                            rows.push(format_window(label, used, window));
                            windows += 1;
                        }
                    }
                }
                if windows == 0 {
                    rows.push("Quota unavailable".to_owned());
                }
            } else {
                rows.push("Quota unavailable".to_owned());
            }
        }
        state => rows.push(link_message(state)),
    }

    rows.push(String::new());
    rows.push("GitHub Copilot".to_owned());
    rows.push(match copilot {
        LinkState::Linked => unavailable_quota(activity.map(|activity| activity.copilot_turns)),
        state => link_message(state),
    });

    rows.push(String::new());
    rows.push("Maestro model turns".to_owned());
    if let Some(activity) = activity.filter(|activity| activity.model_turns > 0) {
        for (label, count) in [
            ("Claude", activity.claude_turns),
            ("Codex", activity.codex_turns),
            ("Copilot", activity.copilot_turns),
            ("Other", activity.other_turns),
        ] {
            if count > 0 {
                rows.push(format!(
                    "{}% {label} ({count})",
                    (count * 100 + activity.model_turns / 2) / activity.model_turns
                ));
            }
        }
    } else {
        rows.push("No recorded model turns today".to_owned());
    }
    rows.push(
        "Quota is provider-reported; Maestro turns and time are local to this device.".to_owned(),
    );
    rows.join("\n")
}

fn link_message(state: LinkState) -> String {
    match state {
        LinkState::Linked => "Quota unavailable".to_owned(),
        LinkState::NotLinked => "Not linked · connect during managed onboarding".to_owned(),
        LinkState::SignInRequired => "Sign in to Deixic to inspect this connection".to_owned(),
        LinkState::WrongOwner => {
            "Linked to another Deixic user · relink this subscription".to_owned()
        }
        LinkState::ConnectionDataUnavailable => {
            "Connection data unavailable · run `deixic-code connections list`".to_owned()
        }
    }
}

fn unavailable_quota(turns: Option<u64>) -> String {
    match turns {
        Some(turns) => format!(
            "{turns} Maestro turn{} today · account quota unavailable",
            if turns == 1 { "" } else { "s" }
        ),
        None => "Maestro turns unavailable · account quota unavailable".to_owned(),
    }
}

fn plan_label(plan: &str) -> String {
    let mut characters = plan.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().collect::<String>() + characters.as_str(),
        None => String::new(),
    }
}

fn format_window(label: &str, used: f64, window: &Value) -> String {
    let used = used.clamp(0.0, 100.0);
    let filled = (used * 18.0 / 100.0).round() as usize;
    let bar = format!("{}{}", "█".repeat(filled), "░".repeat(18 - filled));
    let duration = window.get("windowDurationMins").and_then(Value::as_u64);
    let label = if duration.is_some_and(|mins| mins >= 6 * 24 * 60) {
        "weekly "
    } else {
        label
    };
    let reset = window
        .get("resetsAt")
        .and_then(Value::as_i64)
        .and_then(|seconds| Utc.timestamp_opt(seconds, 0).single())
        .filter(|time| *time > Utc::now())
        .map(|time| {
            let remaining = time.signed_duration_since(Utc::now()).num_seconds();
            if remaining > 0 && remaining < 24 * 3600 {
                format!(" · resets in {}", format_duration(remaining as u64))
            } else {
                format!(
                    " · resets {}",
                    time.with_timezone(&Local).format("%a %H:%M")
                )
            }
        })
        .unwrap_or_default();
    format!("{label}{bar} {used:.0}% used{reset}")
}

fn format_duration(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unified_view_separates_quota_from_maestro_turns() {
        let activity = Activity {
            model_turns: 10,
            active_secs: 3_660,
            claude_turns: 6,
            codex_turns: 3,
            copilot_turns: 1,
            other_turns: 0,
        };
        let quota = json!({"rateLimitsByLimitId": {"codex": {
            "planType": "pro",
            "primary": {"usedPercent": 41, "windowDurationMins": 300},
            "secondary": {"usedPercent": 20, "windowDurationMins": 10080}
        }}});
        let text = format_report(
            Some(&activity),
            LinkState::Linked,
            LinkState::Linked,
            LinkState::Linked,
            Some(&quota),
        );
        assert!(text.contains("10 model turns"));
        assert!(text.contains("ChatGPT Pro"));
        assert!(text.contains("1h 1m tracked agent time"));
        assert!(text.contains("41% used"));
        assert!(text.contains("weekly"));
        assert!(text.contains("1 Maestro turn today · account quota unavailable"));
        assert!(text.contains("60% Claude (6)"));
        assert!(text.contains("30% Codex (3)"));
    }

    #[test]
    fn absent_provider_quota_is_not_unlimited_or_zero() {
        let text = format_report(
            Some(&Activity::default()),
            LinkState::Linked,
            LinkState::Linked,
            LinkState::Linked,
            None,
        );
        assert!(text.contains("Quota unavailable"));
        assert!(!text.contains("unlimited"));
        assert!(!text.contains("0% used"));
    }

    #[test]
    fn activity_counts_each_recorded_response_and_completed_turn_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let today = Local::now().date_naive();
        let started = today
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_local_timezone(Local)
            .single()
            .unwrap()
            .with_timezone(&Utc);
        let completed = started + chrono::Duration::seconds(75);
        let line = |value: Value| value.to_string();
        let entries = [
            json!({"type":"model_change","timestamp":started.to_rfc3339(),"model":"claude-code/sonnet"}),
            json!({"type":"custom","timestamp":started.to_rfc3339(),"customType":"session_event_v1","data":{"eventId":"start-1","kind":"turn.started","timestamp":started.to_rfc3339()}}),
            json!({"type":"message","id":"claude-1","timestamp":completed.to_rfc3339(),"message":{"role":"assistant","content":[],"model":"claude-code/sonnet","timestamp":completed.timestamp_millis()}}),
            json!({"type":"custom","timestamp":completed.to_rfc3339(),"customType":"session_event_v1","data":{"eventId":"end-1","kind":"turn.completed","timestamp":completed.to_rfc3339()}}),
            json!({"type":"message","id":"copilot-1","timestamp":completed.to_rfc3339(),"message":{"role":"assistant","content":[],"model":"github-copilot/auto","timestamp":completed.timestamp_millis()}}),
        ];
        fs::write(
            &path,
            entries.into_iter().map(line).collect::<Vec<_>>().join("\n"),
        )
        .unwrap();
        let mut activity = Activity::default();
        let mut seen_messages = HashSet::new();
        let mut seen_events = HashSet::new();
        collect_file(
            &path,
            today,
            "session-1",
            &mut activity,
            &mut seen_messages,
            &mut seen_events,
        )
        .unwrap();
        // Forked session files can contain the same recorded activity.
        collect_file(
            &path,
            today,
            "session-1",
            &mut activity,
            &mut seen_messages,
            &mut seen_events,
        )
        .unwrap();
        assert_eq!(activity.model_turns, 2);
        assert_eq!(activity.claude_turns, 1);
        assert_eq!(activity.copilot_turns, 1);
        assert_eq!(activity.active_secs, 75);
    }

    #[test]
    fn activity_deduplicates_idless_fork_history_without_merging_distinct_sessions() {
        use std::io::Write;

        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("project");
        fs::create_dir(&sessions).unwrap();
        let today = Local::now().date_naive();
        let timestamp = today
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_local_timezone(Local)
            .single()
            .unwrap()
            .with_timezone(&Utc);
        let assistant = || {
            json!({
                "type":"message",
                "timestamp":timestamp.to_rfc3339(),
                "message":{
                    "role":"assistant",
                    "content":[{"type":"text","text":"same response"}],
                    "model":"openai-codex/gpt-5",
                    "timestamp":timestamp.timestamp_millis()
                }
            })
            .to_string()
        };
        let write_session = |path: &Path, id: &str| {
            let header = json!({
                "type":"session",
                "id":id,
                "timestamp":timestamp.to_rfc3339(),
                "cwd":"/tmp/project",
                "model":"openai-codex/gpt-5"
            });
            fs::write(
                path,
                format!("{}\n{}\n{}\n", header, assistant(), assistant()),
            )
            .unwrap();
        };

        let source = sessions.join("source.jsonl");
        write_session(&source, "source-session");
        let fork = crate::session::fork_session_file(&source).unwrap();
        writeln!(
            fs::OpenOptions::new().append(true).open(&source).unwrap(),
            "{}",
            assistant()
        )
        .unwrap();
        writeln!(
            fs::OpenOptions::new()
                .append(true)
                .open(&fork.path)
                .unwrap(),
            "{}",
            assistant()
        )
        .unwrap();

        // Identical id-less messages in an unrelated session remain distinct,
        // as do repeated equal-looking messages within either session.
        write_session(&sessions.join("unrelated.jsonl"), "unrelated-session");

        // A valid message in a headerless legacy file still contributes.
        fs::write(
            sessions.join("headerless.jsonl"),
            format!("{}\n", assistant()),
        )
        .unwrap();

        let activity = collect_activity(root.path(), today).unwrap();
        assert_eq!(activity.model_turns, 7);
        assert_eq!(activity.codex_turns, 7);
    }
}
