//! Local A2A fleet probe and cockpit summary.
//!
//! Ports `src/platform/a2a-fleet.ts` and `src/platform/a2a-cockpit.ts`.

use serde::Serialize;
use serde_json::Value;

use super::client::{
    discover_agent_card, is_action_required_state, is_completed_state, is_failed_state,
    is_final_state, is_terminal_state,
};
use super::ledger::{TaskLedgerEntry, get_task_ledger_path, list_task_entries, load_task_ledger};
use super::registry::{PeerRegistryEntry, ResolvePeerOptions, list_peers, resolve_peer};

const DEFAULT_A2A_FLEET_PROBE_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_COCKPIT_LIMIT: usize = 8;

#[derive(Debug, Clone, Default)]
pub struct FleetOptions {
    pub registry_path: Option<String>,
    pub tasks_path: Option<String>,
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FleetSummary {
    pub generated_at: String,
    pub registry_path: String,
    pub tasks_path: String,
    pub peers: Vec<FleetPeerSummary>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FleetPeerSummary {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_card_url: Option<String>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol_binding: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_task: Option<FleetTaskSummary>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FleetTaskSummary {
    pub id: String,
    pub ledger_id: String,
    pub state: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_graph: Option<Value>,
    pub updated_at: String,
}

#[derive(Debug, Clone, Default)]
pub struct CockpitOptions {
    pub registry_path: Option<String>,
    pub tasks_path: Option<String>,
    pub timeout_ms: Option<u64>,
    pub peer: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CockpitSummary {
    pub generated_at: String,
    pub registry_path: String,
    pub tasks_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
    pub counts: CockpitCounts,
    pub peers: Vec<CockpitPeerSummary>,
    pub tasks: Vec<CockpitTaskSummary>,
    pub next_actions: Vec<CockpitNextAction>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CockpitCounts {
    pub peers: usize,
    pub online_peers: usize,
    pub unreachable_peers: usize,
    pub tasks: usize,
    pub running_tasks: usize,
    pub action_required_tasks: usize,
    pub failed_tasks: usize,
    pub completed_tasks: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CockpitPeerSummary {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub url: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub task_counts: CockpitPeerTaskCounts,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_task: Option<CockpitLastTask>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CockpitPeerTaskCounts {
    pub tasks: usize,
    pub running_tasks: usize,
    pub action_required_tasks: usize,
    pub failed_tasks: usize,
    pub completed_tasks: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CockpitLastTask {
    pub id: String,
    pub state: String,
    pub status: String,
    pub updated_at: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CockpitTaskSummary {
    pub ledger_id: String,
    pub peer: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orphaned_peer: Option<bool>,
    pub task_id: String,
    pub state: String,
    pub status: String,
    pub requires_input: bool,
    pub terminal: bool,
    pub final_state: bool,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_text: Option<String>,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_graph: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_command: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CockpitNextAction {
    pub id: String,
    pub label: String,
    pub command: String,
    pub severity: String,
    pub peer: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub reason: String,
}

pub async fn inspect_a2a_fleet(options: FleetOptions) -> anyhow::Result<FleetSummary> {
    let (registry_path, registry) = list_peers(options.registry_path.as_deref())?;
    let ledger = load_task_ledger(options.tasks_path.as_deref())?;
    let tasks_path = get_task_ledger_path(options.tasks_path.as_deref())?;
    let mut peers = Vec::new();
    let mut names: Vec<_> = registry.peers.keys().cloned().collect();
    names.sort();
    for name in names {
        let entry = registry.peers.get(&name).cloned().unwrap();
        let last_task = list_task_entries(&ledger, Some(&name))
            .into_iter()
            .next()
            .map(fleet_task_summary);
        peers.push(inspect_peer(name, entry, last_task, &options, registry.timeout_ms).await);
    }
    Ok(FleetSummary {
        generated_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        registry_path: registry_path.display().to_string(),
        tasks_path: tasks_path.display().to_string(),
        peers,
    })
}

pub async fn build_a2a_cockpit(options: CockpitOptions) -> anyhow::Result<CockpitSummary> {
    let fleet = inspect_a2a_fleet(FleetOptions {
        registry_path: options.registry_path.clone(),
        tasks_path: options.tasks_path.clone(),
        timeout_ms: options.timeout_ms,
    })
    .await?;
    let ledger = load_task_ledger(options.tasks_path.as_deref())?;
    Ok(summarize_a2a_cockpit(
        &fleet,
        &ledger.tasks,
        options.peer.as_deref(),
        options.limit,
    ))
}

pub fn summarize_a2a_cockpit(
    fleet: &FleetSummary,
    ledger_tasks: &[TaskLedgerEntry],
    peer: Option<&str>,
    limit: Option<usize>,
) -> CockpitSummary {
    let peer_filter = peer.map(str::trim).filter(|s| !s.is_empty());
    let limit = normalize_limit(limit);
    let peers: Vec<_> = fleet
        .peers
        .iter()
        .filter(|peer| peer_filter.map(|name| peer.name == name).unwrap_or(true))
        .map(|peer| summarize_peer(peer, ledger_tasks))
        .collect();
    let registered: std::collections::BTreeSet<_> =
        peers.iter().map(|peer| peer.name.clone()).collect();
    let mut tasks: Vec<_> = ledger_tasks
        .iter()
        .filter(|entry| peer_filter.map(|name| entry.peer == name).unwrap_or(true))
        .map(|entry| summarize_task(entry, Some(&registered)))
        .collect();
    tasks.sort_by(|left, right| {
        task_urgency(&right.status)
            .cmp(&task_urgency(&left.status))
            .then_with(|| right.updated_at.cmp(&left.updated_at))
    });
    let limited_tasks = tasks.iter().take(limit).cloned().collect::<Vec<_>>();
    let counts = summarize_counts(&peers, &tasks);
    let next_actions = summarize_next_actions(&peers, &tasks, limit);
    CockpitSummary {
        generated_at: fleet.generated_at.clone(),
        registry_path: fleet.registry_path.clone(),
        tasks_path: fleet.tasks_path.clone(),
        peer: peer_filter.map(str::to_string),
        counts,
        peers,
        tasks: limited_tasks,
        next_actions,
    }
}

async fn inspect_peer(
    name: String,
    entry: PeerRegistryEntry,
    last_task: Option<FleetTaskSummary>,
    options: &FleetOptions,
    registry_timeout_ms: Option<u64>,
) -> FleetPeerSummary {
    let mut base = base_peer_summary(&name, &entry, last_task);
    let resolved = match resolve_peer(
        Some(&name),
        ResolvePeerOptions {
            registry_path: options.registry_path.clone(),
            timeout_ms: options.timeout_ms,
            token: None,
            max_attempts: Some(1),
        },
    ) {
        Ok(peer) => peer,
        Err(error) => {
            base.status = "unreachable".into();
            base.error = Some(sanitize_error(&error.to_string()));
            return base;
        }
    };
    let mut config = resolved.config;
    if options.timeout_ms.is_none() && entry.timeout_ms.is_none() && registry_timeout_ms.is_none() {
        config.timeout_ms = config.timeout_ms.min(DEFAULT_A2A_FLEET_PROBE_TIMEOUT_MS);
    }
    config.max_attempts = 1;
    match discover_agent_card(&config).await {
        Ok(card) => {
            base.status = "online".into();
            if let Some(display) = card.get("name").and_then(|v| v.as_str()) {
                let trimmed = display.trim();
                if !trimmed.is_empty() {
                    base.display_name = Some(trimmed.to_string());
                }
            }
            if let Some(iface) = card
                .get("supportedInterfaces")
                .and_then(|v| v.as_array())
                .and_then(|items| items.first())
            {
                if let Some(binding) = iface.get("protocolBinding").and_then(|v| v.as_str()) {
                    base.protocol_binding = Some(binding.to_string());
                }
                if let Some(version) = iface.get("protocolVersion").and_then(|v| v.as_str()) {
                    base.protocol_version = Some(version.to_string());
                }
            }
            base
        }
        Err(error) => {
            base.status = "unreachable".into();
            base.error = Some(sanitize_error(&format!("{error:#}")));
            base
        }
    }
}

fn base_peer_summary(
    name: &str,
    entry: &PeerRegistryEntry,
    last_task: Option<FleetTaskSummary>,
) -> FleetPeerSummary {
    FleetPeerSummary {
        name: name.to_string(),
        display_name: entry.display_name.clone(),
        url: entry.url.clone(),
        agent_card_url: entry.agent_card_url.clone(),
        status: "unreachable".into(),
        error: None,
        auth: if let Some(env) = &entry.token_env {
            Some(format!("env:{env}"))
        } else if entry.token_file.is_some() {
            Some("file".into())
        } else {
            None
        },
        protocol_binding: entry.protocol_binding.clone(),
        protocol_version: entry.protocol_version.clone(),
        model: string_metadata(entry, "model"),
        cwd: string_metadata(entry, "cwd"),
        last_task,
    }
}

fn fleet_task_summary(entry: &TaskLedgerEntry) -> FleetTaskSummary {
    FleetTaskSummary {
        id: entry.task_id.clone(),
        ledger_id: entry.id.clone(),
        state: entry.state.clone(),
        text: entry.text.clone(),
        response_text: entry.response_text.clone(),
        work_graph: entry.work_graph.clone(),
        updated_at: entry.updated_at.clone(),
    }
}

fn summarize_peer(peer: &FleetPeerSummary, ledger_tasks: &[TaskLedgerEntry]) -> CockpitPeerSummary {
    let tasks: Vec<_> = ledger_tasks
        .iter()
        .filter(|entry| entry.peer == peer.name)
        .map(|entry| summarize_task(entry, None))
        .collect();
    let last_task = tasks.first().map(|task| CockpitLastTask {
        id: task.task_id.clone(),
        state: task.state.clone(),
        status: task.status.clone(),
        updated_at: task.updated_at.clone(),
        text: task.text.clone(),
    });
    CockpitPeerSummary {
        name: peer.name.clone(),
        display_name: peer.display_name.clone(),
        url: peer.url.clone(),
        status: peer.status.clone(),
        error: peer.error.clone(),
        auth: peer.auth.clone(),
        model: peer.model.clone(),
        cwd: peer.cwd.clone(),
        task_counts: CockpitPeerTaskCounts {
            tasks: tasks.len(),
            running_tasks: tasks.iter().filter(|t| t.status == "running").count(),
            action_required_tasks: tasks.iter().filter(|t| t.status == "waiting").count(),
            failed_tasks: tasks.iter().filter(|t| t.status == "failed").count(),
            completed_tasks: tasks.iter().filter(|t| t.status == "completed").count(),
        },
        last_task,
    }
}

fn summarize_task(
    entry: &TaskLedgerEntry,
    registered_peer_names: Option<&std::collections::BTreeSet<String>>,
) -> CockpitTaskSummary {
    let status = classify_task_state(&entry.state);
    let orphaned = registered_peer_names.is_some_and(|names| !names.contains(&entry.peer));
    let next_command = if orphaned {
        None
    } else {
        task_command(entry, &status)
    };
    CockpitTaskSummary {
        ledger_id: entry.id.clone(),
        peer: entry.peer.clone(),
        peer_display_name: entry.peer_display_name.clone(),
        orphaned_peer: orphaned.then_some(true),
        task_id: entry.task_id.clone(),
        state: entry.state.clone(),
        status: status.clone(),
        requires_input: is_action_required_state(&entry.state),
        terminal: is_terminal_state(&entry.state),
        final_state: is_final_state(&entry.state),
        text: entry.text.clone(),
        response_text: entry.response_text.clone(),
        updated_at: entry.updated_at.clone(),
        completed_at: entry.completed_at.clone(),
        work_graph: entry.work_graph.clone(),
        next_command,
    }
}

fn summarize_counts(peers: &[CockpitPeerSummary], tasks: &[CockpitTaskSummary]) -> CockpitCounts {
    CockpitCounts {
        peers: peers.len(),
        online_peers: peers.iter().filter(|peer| peer.status == "online").count(),
        unreachable_peers: peers
            .iter()
            .filter(|peer| peer.status == "unreachable")
            .count(),
        tasks: tasks.len(),
        running_tasks: tasks.iter().filter(|task| task.status == "running").count(),
        action_required_tasks: tasks.iter().filter(|task| task.status == "waiting").count(),
        failed_tasks: tasks.iter().filter(|task| task.status == "failed").count(),
        completed_tasks: tasks
            .iter()
            .filter(|task| task.status == "completed")
            .count(),
    }
}

fn summarize_next_actions(
    peers: &[CockpitPeerSummary],
    tasks: &[CockpitTaskSummary],
    limit: usize,
) -> Vec<CockpitNextAction> {
    let mut actions: Vec<_> = tasks.iter().filter_map(next_action_for_task).collect();
    if actions.is_empty() {
        actions = peers
            .iter()
            .filter(|peer| peer.status == "online" && peer.task_counts.running_tasks == 0)
            .map(|peer| CockpitNextAction {
                id: format!("delegate:{}", peer.name),
                label: format!("Delegate fresh work to {}", peer.name),
                command: format!(
                    "deixic-code a2a delegate {} <objective> --wait --work-graph",
                    shell_quote(&peer.name)
                ),
                severity: "info".into(),
                peer: peer.name.clone(),
                task_id: None,
                reason: "Peer is reachable and has no active local A2A task in the ledger.".into(),
            })
            .collect();
    }
    actions.into_iter().take(limit).collect()
}

fn next_action_for_task(task: &CockpitTaskSummary) -> Option<CockpitNextAction> {
    if task.orphaned_peer == Some(true) {
        return None;
    }
    match task.status.as_str() {
        "waiting" => Some(CockpitNextAction {
            id: format!("reply:{}:{}", task.peer, task.task_id),
            label: format!("Reply to {} task {}", task.peer, task.task_id),
            command: format!(
                "deixic-code a2a reply {} {} <response> --wait --work-graph",
                shell_quote(&task.peer),
                shell_quote(&task.task_id)
            ),
            severity: "critical".into(),
            peer: task.peer.clone(),
            task_id: Some(task.task_id.clone()),
            reason: "Peer returned an input-required or auth-required A2A state.".into(),
        }),
        "running" => Some(CockpitNextAction {
            id: format!("wait:{}:{}", task.peer, task.task_id),
            label: format!("Wait for {} task {}", task.peer, task.task_id),
            command: task.next_command.clone().unwrap_or_else(|| {
                format!(
                    "deixic-code a2a wait {} {} --work-graph",
                    shell_quote(&task.peer),
                    shell_quote(&task.task_id)
                )
            }),
            severity: "info".into(),
            peer: task.peer.clone(),
            task_id: Some(task.task_id.clone()),
            reason: "Task is still non-terminal in the local A2A ledger.".into(),
        }),
        "failed" => Some(CockpitNextAction {
            id: format!("refresh:{}:{}", task.peer, task.task_id),
            label: format!("Refresh failed {} task {}", task.peer, task.task_id),
            command: format!(
                "deixic-code a2a tasks {} --refresh --work-graph",
                shell_quote(&task.peer)
            ),
            severity: "warning".into(),
            peer: task.peer.clone(),
            task_id: Some(task.task_id.clone()),
            reason: "Task reached a failed, rejected, or canceled final state.".into(),
        }),
        _ => None,
    }
}

fn task_command(entry: &TaskLedgerEntry, status: &str) -> Option<String> {
    match status {
        "waiting" => Some(format!(
            "deixic-code a2a reply {} {} <response> --wait --work-graph",
            shell_quote(&entry.peer),
            shell_quote(&entry.task_id)
        )),
        "running" => Some(format!(
            "deixic-code a2a wait {} {} --work-graph",
            shell_quote(&entry.peer),
            shell_quote(&entry.task_id)
        )),
        _ => None,
    }
}

pub fn classify_task_state(state: &str) -> String {
    if is_action_required_state(state) {
        "waiting".into()
    } else if is_completed_state(state) {
        "completed".into()
    } else if is_failed_state(state) {
        "failed".into()
    } else if !is_final_state(state) {
        "running".into()
    } else {
        "unknown".into()
    }
}

fn task_urgency(status: &str) -> u8 {
    match status {
        "waiting" => 4,
        "failed" => 3,
        "running" => 2,
        "unknown" => 1,
        _ => 0,
    }
}

fn normalize_limit(limit: Option<usize>) -> usize {
    match limit {
        None => DEFAULT_COCKPIT_LIMIT,
        Some(0) => DEFAULT_COCKPIT_LIMIT,
        Some(limit) => limit.min(50),
    }
}

fn string_metadata(entry: &PeerRegistryEntry, key: &str) -> Option<String> {
    entry
        .metadata
        .as_ref()
        .and_then(|meta| meta.get(key))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | ':' | '-'))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn sanitize_error(message: &str) -> String {
    message
        .replace("Bearer ", "Bearer [redacted] ")
        .chars()
        .take(240)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::a2a_cli::ledger::TaskLedgerEntry;

    #[test]
    fn cockpit_surfaces_waiting_tasks_as_next_actions() {
        let fleet = FleetSummary {
            generated_at: "2026-07-21T00:00:00.000Z".into(),
            registry_path: "/tmp/peers.json".into(),
            tasks_path: "/tmp/tasks.json".into(),
            peers: vec![FleetPeerSummary {
                name: "peer-a".into(),
                display_name: Some("Peer A".into()),
                url: "http://127.0.0.1:1".into(),
                agent_card_url: None,
                status: "online".into(),
                error: None,
                auth: None,
                protocol_binding: None,
                protocol_version: None,
                model: None,
                cwd: None,
                last_task: None,
            }],
        };
        let tasks = vec![TaskLedgerEntry {
            id: "ledger-1".into(),
            kind: "delegation".into(),
            peer: "peer-a".into(),
            peer_display_name: None,
            task_id: "task-1".into(),
            context_id: None,
            message_id: None,
            text: "need input".into(),
            role: None,
            cwd: None,
            state: "input-required".into(),
            response_text: None,
            metadata: None,
            work_graph: None,
            transcript: vec![],
            created_at: "2026-07-21T00:00:00.000Z".into(),
            updated_at: "2026-07-21T00:00:01.000Z".into(),
            completed_at: None,
            extensions: Default::default(),
        }];
        let cockpit = summarize_a2a_cockpit(&fleet, &tasks, None, Some(5));
        assert_eq!(cockpit.counts.action_required_tasks, 1);
        assert_eq!(cockpit.next_actions[0].severity, "critical");
        assert!(cockpit.next_actions[0].command.contains("reply"));
    }
}
