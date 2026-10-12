//! Native `maestro a2a` command surface.
//!
//! Local peer operations and Platform agent-registry integration without
//! booting the TypeScript agent runtime:
//! - `offer` / `pair` / `create` — pairing code generation
//! - `accept` — decode pairing code into `~/.maestro/a2a/peers.json`
//! - `peers` / `list` — list registered peers
//! - `discover` / `register` — Platform agent registry
//! - `fleet` / `cockpit` — local peer probes + task dashboard
//! - `card` — fetch Agent Card from a registered peer
//! - `send` / `reply` / `delegate` / `coordinate` — message + ledger
//! - `control` / `graph` — Platform delegation control surface
//! - `tasks` / `wait` / `telemetry` — ledger + offline telemetry inspect

mod agent_registry;
mod capability_market;
mod client;
mod fleet_cockpit;
mod ledger;
mod maestro_peer;
mod pairing;
pub mod peer_message;
mod registry;
mod telemetry;

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use agent_registry::{
    AGENT_STATUS_IDLE, ControlA2ADelegationTaskInput, DelegateAgentInput,
    GetA2ADelegationGraphInput, HeartbeatAgentInput, ListA2APeersInput,
    PlatformAgentDiscoveryEvidence, PlatformAgentRegistryA2APeerCandidate, RegisterAgentInput,
    UpdateAgentInput, agent_registry_not_configured_message,
    control_a2a_delegation_task_with_platform, delegate_agent_with_platform,
    get_a2a_delegation_graph_with_platform, heartbeat_agent_with_platform,
    is_agent_already_exists_error, list_a2a_peer_candidates_with_evidence,
    normalize_a2a_control_mode, register_agent_with_platform, update_agent_with_platform,
};
use capability_market::{A2ACapabilityMarketRequest, select_a2a_capability_peer};
pub use client::{
    A2AServiceConfig, A2ATask, SendMessageInput, discover_agent_card, extract_task_text, get_task,
    is_action_required_state, is_completed_state, is_failed_state, is_final_state,
    is_terminal_state, send_message, wait_for_task,
};
use fleet_cockpit::{CockpitOptions, FleetOptions, build_a2a_cockpit, inspect_a2a_fleet};
pub use ledger::{
    OrbDelegationEntry, OrbDelegationObservation, OrbDelegationObserver, OrbDelegationStartInput,
    OrbDelegationStartOutcome, OrbDelegationState, OrbObservedState, OrbRecoveryReport,
    RecordTaskStartInput, TaskLedgerEntry, TaskLedgerFile, TranscriptEntry, get_task_ledger_path,
    list_task_entries, load_task_ledger, reconcile_orb_delegation, record_orb_delegation_start,
    record_task_start, recover_orb_delegations, recover_orb_delegations_from_path,
    update_task_in_ledger, upsert_orb_delegation,
};
use maestro_peer::{
    BuildMaestroA2APeerProjectionInput, build_maestro_a2a_peer_projection,
    default_maestro_a2a_capabilities,
};
use pairing::{
    base_url_from_agent_card_url, create_pairing_payload, create_pairing_payload_from_agent_card,
    decode_pairing_code, encode_pairing_code, resolve_agent_card_url,
};
use peer_message::{PeerMessageInput, peer_context_id, start_peer_message, wait_for_peer_message};
use registry::{
    PeerRegistryEntry, ResolvePeerOptions, UpsertPeerOptions, list_peers, load_peer_registry,
    normalize_peer_name, resolve_peer, save_peer_registry, upsert_peer_from_pairing_payload,
};
use telemetry::{inspect_a2a_telemetry, load_a2a_telemetry_events};

const DEFAULT_WAIT_MS: u64 = 300_000;
const DEFAULT_WAIT_INTERVAL_MS: u64 = 5_000;
const OFFER_DISCOVER_TIMEOUT_MS: u64 = 2_500;

/// Dispatch `maestro a2a <subcommand> ...`.
///
/// `args` is the token stream after the `a2a` command name.
pub async fn run_a2a(args: &[String]) -> Result<i32> {
    let subcommand = args
        .first()
        .map(|s| canonical_subcommand(s.as_str()))
        .unwrap_or("help");

    match subcommand {
        "help" | "--help" | "-h" => {
            println!("{}", a2a_help());
            Ok(0)
        }
        "offer" | "pair" | "create" => run_offer(&args[1..]).await,
        "accept" => run_accept(&args[1..]).await,
        "peers" | "list" => run_peers(&args[1..]),
        "discover" => run_discover(&args[1..]).await,
        "register" => run_register(&args[1..]).await,
        "fleet" => run_fleet(&args[1..]).await,
        "cockpit" | "dashboard" => run_cockpit(&args[1..]).await,
        "card" => run_card(&args[1..]).await,
        "send" => run_send(&args[1..]).await,
        "delegate" | "delegation" => run_delegate(&args[1..]).await,
        "control" => run_control(&args[1..]).await,
        "graph" => run_graph(&args[1..]).await,
        "reply" | "continue" => run_reply(&args[1..]).await,
        "coordinate" => run_coordinate(&args[1..]).await,
        "tasks" => run_tasks(&args[1..]).await,
        "telemetry" => run_telemetry(&args[1..]),
        "wait" => run_wait(&args[1..]).await,
        other => {
            eprintln!("Unknown a2a subcommand: {other}");
            eprintln!();
            println!("{}", a2a_help());
            Ok(1)
        }
    }
}

fn a2a_help() -> &'static str {
    "Usage:
  maestro a2a offer --url <base-url> [--name <display-name>] [--peer-id <id>]
  maestro a2a accept <pairing-code> [--name <peer>] [--default] [--token-env ENV] [--session-id <id>]
  maestro a2a peers [--registry <path>]
  maestro a2a discover [--capability <capability>] [--skill <skill-id>] [--import] [--json]
  maestro a2a register --url <base-url> [--agent-id <id>] [--workspace-id <id>] [--json]
  maestro a2a fleet [--json]
  maestro a2a cockpit [--peer <peer>] [--json]
  maestro a2a card <peer> [--registry <path>]
  maestro a2a send <peer> <text> [--wait] [--tasks <path>] [--registry <path>]
  maestro a2a delegate <peer> <text> [--role <role>] [--cwd <path>] [--wait] [--work-graph]
  maestro a2a delegate --discover --skill <skill-id> <text> [--capability <capability>]
  maestro a2a delegate --platform --from-agent-id <agent-id> [--to-agent-id <id>|--capability <c>] --skill <skill-id> <text>
  maestro a2a control <delegation-id> --mode steer|followup|collect|interrupt|cancel [message]
  maestro a2a graph <delegation-id> [--root <root-delegation-id>] [--json]
  maestro a2a coordinate [peer] [--reply <text>] [--wait] [--json] [--work-graph]
  maestro a2a reply <peer> <task-id> <text> [--wait] [--tasks <path>]
  maestro a2a tasks [peer] [--json] [--refresh] [--tasks <path>]
  maestro a2a telemetry --events <path> --swarm-id <id> [--json]
  maestro a2a wait <peer> <task-id> [--max-wait-ms N] [--interval-ms N]

Native surface covers local peer pairing/registry, Agent Card fetch, send/reply,
fleet/cockpit probes, Platform discover/register/delegate/control/graph, and
offline telemetry inspection against ~/.maestro/a2a/{peers,tasks}.json.

Pairing codes carry Agent Card and transport coordinates only. Configure auth
with --token-env or --token-file when accepting a peer; bearer tokens are never
embedded."
}

fn canonical_subcommand(raw: &str) -> &str {
    match raw {
        "pair" | "create" => "offer",
        "list" => "peers",
        "dashboard" => "cockpit",
        "delegation" => "delegate",
        "continue" => "reply",
        other => other,
    }
}

async fn run_offer(args: &[String]) -> Result<i32> {
    let flags = FlagSet::parse(args);
    let base_url = flags.string("--url").or_else(|| flags.string("--base-url"));
    let agent_card_url_input = flags
        .string("--agent-card-url")
        .or(base_url)
        .or_else(|| {
            env_first(&[
                "MAESTRO_A2A_PUBLIC_URL",
                "MAESTRO_CONTROL_PUBLIC_URL",
                "MAESTRO_A2A_URL",
                "MAESTRO_CONTROL_URL",
            ])
        })
        .context("Provide --url or set MAESTRO_A2A_PUBLIC_URL.")?;
    let agent_card_url = resolve_agent_card_url(&agent_card_url_input)?;
    let transport_url = base_url_from_agent_card_url(&agent_card_url)?;
    let ttl_ms = flags.minutes_ms("--ttl-minutes").unwrap_or(30 * 60 * 1000);
    let peer_id = flags.string("--peer-id");
    let display_name = flags.string("--name");

    let agent_card = {
        let config = A2AServiceConfig {
            base_url: transport_url.clone(),
            token: None,
            organization_id: None,
            workspace_id: None,
            agent_id: Some("maestro".into()),
            session_id: None,
            actor_id: None,
            timeout_ms: OFFER_DISCOVER_TIMEOUT_MS,
            max_attempts: 1,
        };
        match discover_agent_card(&config).await {
            Ok(card) => Some(card),
            Err(error) => {
                if display_name.is_none() {
                    bail!(
                        "Could not fetch Agent Card at {agent_card_url}: {error:#}. Pass --name to create an offline pairing code."
                    );
                }
                None
            }
        }
    };

    let payload = if let Some(card) = agent_card {
        create_pairing_payload_from_agent_card(
            &card,
            &agent_card_url,
            display_name.as_deref(),
            peer_id.as_deref(),
            ttl_ms,
        )?
    } else {
        create_pairing_payload(
            display_name.as_deref().unwrap_or("Deixic Code A2A Peer"),
            &agent_card_url,
            &transport_url,
            peer_id.as_deref(),
            ttl_ms,
        )?
    };
    let code = encode_pairing_code(&payload)?;
    println!("{code}");
    eprintln!(
        "Pairing code for {}; expires {}. No token or bearer secret is embedded.",
        payload.display_name, payload.expires_at
    );
    Ok(0)
}

async fn run_accept(args: &[String]) -> Result<i32> {
    let flags = FlagSet::parse(args);
    let code = flags
        .first_positional()
        .context("Usage: deixic-code a2a accept <code>")?;
    let payload = decode_pairing_code(code, false)?;
    let result = upsert_peer_from_pairing_payload(
        &payload,
        UpsertPeerOptions {
            name: flags.string("--name"),
            make_default: flags.boolean("--default"),
            token_env: flags.string("--token-env"),
            token_file: flags.string("--token-file"),
            session_id: flags.string("--session-id"),
            workspace_id: flags.string("--workspace-id"),
            organization_id: flags.string("--organization-id"),
            registry_path: flags.string("--registry"),
        },
    )?;
    println!(
        "Registered A2A peer {} at {}",
        result.name, result.entry.url
    );
    println!("Registry: {}", result.path.display());
    if result.entry.token_env.is_none() && result.entry.token_file.is_none() {
        println!(
            "No token source configured; add --token-env or --token-file if the peer requires Authorization."
        );
    }
    Ok(0)
}

fn run_peers(args: &[String]) -> Result<i32> {
    let flags = FlagSet::parse(args);
    let (path, registry) = list_peers(flags.string("--registry").as_deref())?;
    println!("A2A peers ({})", path.display());
    let mut entries: Vec<_> = registry.peers.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    if entries.is_empty() {
        println!("  No peers registered. Run deixic-code a2a accept <code>.");
        return Ok(0);
    }
    for (name, peer) in entries {
        let marker = if registry.default_peer.as_deref() == Some(name.as_str()) {
            "*"
        } else {
            " "
        };
        let token_source = if let Some(env) = &peer.token_env {
            format!(" token=env:{env}")
        } else if peer.token_file.is_some() {
            " token=file".to_string()
        } else {
            String::new()
        };
        let display = peer
            .display_name
            .as_ref()
            .map(|d| format!(" ({d})"))
            .unwrap_or_default();
        println!("{marker} {name} {}{token_source}{display}", peer.url);
    }
    Ok(0)
}

async fn run_discover(args: &[String]) -> Result<i32> {
    let flags = FlagSet::parse(args);
    let discovery = list_a2a_peer_candidates_with_evidence(
        ListA2APeersInput {
            workspace_id: flags.string("--workspace-id"),
            capability: flags.string("--capability"),
            surface: flags.string("--surface"),
            status: flags.string("--status"),
            limit: flags.number("--limit"),
            offset: flags.number("--offset"),
            skill_id: flags.string("--skill"),
            prefer_internal_endpoint: flags.boolean("--prefer-internal"),
        },
        None,
    )
    .await?
    .with_context(agent_registry_not_configured_message)?;
    let imported = if flags.boolean("--import") {
        import_discovered_peers(
            &discovery.candidates,
            discovery.discovery_evidence.as_ref(),
            flags.string("--registry").as_deref(),
            flags.boolean("--default"),
        )?
    } else {
        Vec::new()
    };
    if flags.boolean("--json") {
        let payload = json!({
            "peers": discovery.candidates.iter().map(discovered_peer_json).collect::<Vec<_>>(),
            "imported": imported,
            "discoveryEvidence": discovery.discovery_evidence,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(0);
    }
    println!("Platform A2A peers");
    print_discovery_evidence(discovery.discovery_evidence.as_ref());
    if discovery.candidates.is_empty() {
        println!("  No Platform agents expose A2A peer endpoints.");
        return Ok(0);
    }
    for candidate in &discovery.candidates {
        let label = candidate
            .agent
            .name
            .clone()
            .or_else(|| candidate.agent.id.clone())
            .unwrap_or_else(|| candidate.endpoint_url.clone());
        let skill_summary = candidate
            .skills
            .iter()
            .map(|skill| skill.id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let status = candidate
            .agent
            .status
            .as_deref()
            .map(|s| format!(" {s}"))
            .unwrap_or_default();
        println!("{label} {}{status}", candidate.endpoint_url);
        let mut details = Vec::new();
        if let Some(id) = &candidate.agent.id {
            details.push(format!("agent={id}"));
        }
        if let Some(binding) = &candidate.protocol_binding {
            details.push(format!("binding={binding}"));
        }
        if let Some(version) = &candidate.protocol_version {
            details.push(format!("version={version}"));
        }
        if !details.is_empty() {
            println!("  {}", details.join(" "));
        }
        if !skill_summary.is_empty() {
            println!("  skills={skill_summary}");
        }
    }
    if !imported.is_empty() {
        println!("Imported {} peer(s).", imported.len());
        if let Some(path) = imported.first().map(|entry| &entry.path) {
            println!("Registry: {path}");
        }
    }
    Ok(0)
}

async fn run_register(args: &[String]) -> Result<i32> {
    let flags = FlagSet::parse(args);
    let heartbeat_only = flags.boolean("--heartbeat-only");
    let update_only = flags.boolean("--update-only");
    let should_heartbeat = !flags.boolean("--no-heartbeat");
    if heartbeat_only && !should_heartbeat {
        bail!("--heartbeat-only cannot be combined with --no-heartbeat.");
    }
    let agent_id = flags.string("--agent-id").or_else(|| {
        env_first(&[
            "MAESTRO_A2A_AGENT_ID",
            "MAESTRO_AGENT_ID",
            "EVALOPS_AGENT_ID",
        ])
    });
    let name = flags
        .string("--name")
        .or_else(|| env_first(&["MAESTRO_A2A_AGENT_NAME", "MAESTRO_AGENT_NAME"]))
        .unwrap_or_else(|| "Deixic Code A2A Peer".into());
    let description = flags
        .string("--description")
        .or_else(|| env_first(&["MAESTRO_A2A_AGENT_DESCRIPTION", "MAESTRO_AGENT_DESCRIPTION"]))
        .unwrap_or_else(|| {
            "Deixic Code peer exposing governed Codex subagent lanes through A2A.".into()
        });
    let workspace_id = flags.string("--workspace-id");
    let default_capabilities = default_maestro_a2a_capabilities();
    let capabilities = flags.string_list("--capabilities", &default_capabilities);
    let default_surfaces = vec!["a2a".into(), "maestro".into()];
    let surfaces = flags.string_list("--surface", &default_surfaces);
    let default_surface_types = vec!["SURFACE_MAESTRO".into()];
    let surface_types = flags.string_list("--surface-types", &default_surface_types);
    let public_endpoint_url = if heartbeat_only {
        None
    } else {
        Some(
            flags
                .string("--public-url")
                .or_else(|| flags.string("--url"))
                .or_else(|| {
                    env_first(&[
                        "MAESTRO_A2A_PUBLIC_URL",
                        "MAESTRO_CONTROL_PUBLIC_URL",
                        "MAESTRO_A2A_URL",
                        "MAESTRO_CONTROL_URL",
                    ])
                })
                .context("Provide --url or set MAESTRO_A2A_PUBLIC_URL.")?,
        )
    };
    let a2a = public_endpoint_url.as_ref().map(|url| {
        let mut attributes = BTreeMap::new();
        attributes.insert("publishedBy".into(), "deixic-code a2a register".into());
        build_maestro_a2a_peer_projection(BuildMaestroA2APeerProjectionInput {
            public_endpoint_url: url.clone(),
            internal_endpoint_url: flags.string("--internal-url").or_else(|| {
                env_first(&["MAESTRO_A2A_INTERNAL_URL", "MAESTRO_CONTROL_INTERNAL_URL"])
            }),
            agent_card_url: flags.string("--agent-card-url"),
            protocol_version: flags.string("--protocol-version"),
            agent_card_etag: flags.string("--agent-card-etag"),
            agent_card_hash: flags.string("--agent-card-hash"),
            push_notifications: None,
            security_schemes: Some({
                let default_schemes = vec!["evalops-agent-token".into()];
                flags.string_list("--security-schemes", &default_schemes)
            }),
            attributes: Some(attributes),
        })
    });

    let mut operation = "registered";
    let mut agent = None;
    if heartbeat_only {
        if agent_id.is_none() {
            bail!("Usage: deixic-code a2a register --heartbeat-only --agent-id <id>");
        }
        operation = "heartbeat";
    } else if update_only {
        let agent_id = agent_id.clone().context(
            "Usage: deixic-code a2a register --update-only --agent-id <id> --url <base-url>",
        )?;
        let updated = update_agent_with_platform(
            UpdateAgentInput {
                workspace_id: workspace_id.clone(),
                id: agent_id,
                name: Some(name.clone()),
                description: Some(description.clone()),
                capabilities: Some(capabilities.clone()),
                surfaces: Some(surfaces.clone()),
                surface_types: Some(surface_types.clone()),
                a2a: a2a.clone(),
            },
            None,
        )
        .await?
        .with_context(agent_registry_not_configured_message)?;
        operation = "updated";
        agent = Some(updated);
    } else {
        match register_agent_with_platform(
            RegisterAgentInput {
                workspace_id: workspace_id.clone(),
                id: agent_id.clone(),
                name: name.clone(),
                description: Some(description.clone()),
                agent_type: flags.string("--type").unwrap_or_else(|| "maestro".into()),
                capabilities: capabilities.clone(),
                surfaces: Some(surfaces.clone()),
                surface_types: Some(surface_types.clone()),
                owner_id: flags.string("--owner-id"),
                a2a: a2a.clone(),
            },
            None,
        )
        .await
        {
            Ok(Some(registered)) => agent = Some(registered),
            Ok(None) => bail!("{}", agent_registry_not_configured_message()),
            Err(error) if agent_id.is_some() && is_agent_already_exists_error(&error) => {
                let updated = update_agent_with_platform(
                    UpdateAgentInput {
                        workspace_id: workspace_id.clone(),
                        id: agent_id.clone().expect("checked"),
                        name: Some(name.clone()),
                        description: Some(description.clone()),
                        capabilities: Some(capabilities.clone()),
                        surfaces: Some(surfaces.clone()),
                        surface_types: Some(surface_types.clone()),
                        a2a: a2a.clone(),
                    },
                    None,
                )
                .await?
                .with_context(agent_registry_not_configured_message)?;
                operation = "updated";
                agent = Some(updated);
            }
            Err(error) => return Err(error),
        }
    }

    let resolved_agent_id = agent
        .as_ref()
        .and_then(|entry| entry.id.clone())
        .or(agent_id)
        .context("Agent Registry did not return an agent id.")?;
    let heartbeat = if should_heartbeat {
        Some(
            heartbeat_agent_with_platform(
                HeartbeatAgentInput {
                    workspace_id: workspace_id.clone(),
                    agent_id: resolved_agent_id.clone(),
                    status: Some(
                        flags
                            .string("--status")
                            .unwrap_or_else(|| AGENT_STATUS_IDLE.into()),
                    ),
                    surface: surfaces.first().cloned(),
                    surface_type: surface_types.first().cloned(),
                    a2a: a2a.clone(),
                },
                None,
            )
            .await?
            .with_context(agent_registry_not_configured_message)?,
        )
    } else {
        None
    };

    if flags.boolean("--json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "operation": operation,
                "agentId": resolved_agent_id,
                "agent": agent,
                "heartbeat": heartbeat.as_ref().map(|next| json!({ "nextHeartbeatBy": next })),
                "a2a": a2a,
            }))?
        );
        return Ok(0);
    }

    if operation == "heartbeat" {
        println!(
            "Sent Platform A2A heartbeat for {resolved_agent_id}{}",
            a2a.as_ref()
                .and_then(|projection| projection.public_endpoint_url.as_deref())
                .map(|url| format!(" at {url}"))
                .unwrap_or_default()
        );
    } else {
        let verb = if operation == "registered" {
            "Registered"
        } else {
            "Updated"
        };
        let endpoint = a2a
            .as_ref()
            .and_then(|projection| projection.public_endpoint_url.as_deref())
            .or(public_endpoint_url.as_deref())
            .unwrap_or("(no endpoint)");
        println!("{verb} Platform A2A peer {resolved_agent_id} at {endpoint}");
    }
    let skills = a2a
        .as_ref()
        .and_then(|projection| projection.skills.as_ref())
        .map(|skills| {
            skills
                .iter()
                .map(|skill| skill.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "none".into());
    println!("Skills: {skills}");
    if let Some(next) = heartbeat.as_ref().filter(|value| !value.is_empty()) {
        println!("Next heartbeat by: {next}");
    }
    Ok(0)
}

async fn run_fleet(args: &[String]) -> Result<i32> {
    let flags = FlagSet::parse(args);
    let fleet = inspect_a2a_fleet(FleetOptions {
        registry_path: flags.string("--registry"),
        tasks_path: flags.string("--tasks"),
        timeout_ms: flags.number("--timeout-ms"),
    })
    .await?;
    if flags.boolean("--json") {
        println!("{}", serde_json::to_string_pretty(&fleet)?);
        return Ok(0);
    }
    println!("A2A fleet ({})", fleet.registry_path);
    if fleet.peers.is_empty() {
        println!("  No peers registered. Run deixic-code a2a accept <code>.");
        return Ok(0);
    }
    for peer in &fleet.peers {
        let status = if peer.status == "online" {
            "online"
        } else {
            "down"
        };
        let label = peer
            .display_name
            .as_ref()
            .map(|display| format!("{} ({display})", peer.name))
            .unwrap_or_else(|| peer.name.clone());
        println!("{status} {label} {}", peer.url);
        let mut details = Vec::new();
        if let Some(model) = &peer.model {
            details.push(format!("model={model}"));
        }
        if let Some(cwd) = &peer.cwd {
            details.push(format!("cwd={cwd}"));
        }
        if let Some(auth) = &peer.auth {
            details.push(format!("auth={auth}"));
        }
        if !details.is_empty() {
            println!("  {}", details.join(" "));
        }
        if let Some(last) = &peer.last_task {
            println!("  last={} {} {}", last.id, last.state, last.text);
        }
        if let Some(error) = &peer.error {
            println!("  error={error}");
        }
    }
    Ok(0)
}

async fn run_cockpit(args: &[String]) -> Result<i32> {
    let flags = FlagSet::parse(args);
    let cockpit = build_a2a_cockpit(CockpitOptions {
        registry_path: flags.string("--registry"),
        tasks_path: flags.string("--tasks"),
        timeout_ms: flags.number("--timeout-ms"),
        peer: flags.string("--peer"),
        limit: flags.number("--limit").map(|n| n as usize),
    })
    .await?;
    if flags.boolean("--json") {
        println!("{}", serde_json::to_string_pretty(&cockpit)?);
        return Ok(0);
    }
    println!("A2A cockpit ({})", cockpit.registry_path);
    println!("  tasks={}", cockpit.tasks_path);
    println!(
        "{}/{} peers online · {} running · {} waiting · {} failed · {} completed",
        cockpit.counts.online_peers,
        cockpit.counts.peers,
        cockpit.counts.running_tasks,
        cockpit.counts.action_required_tasks,
        cockpit.counts.failed_tasks,
        cockpit.counts.completed_tasks
    );
    println!("\nPeers");
    if cockpit.peers.is_empty() {
        println!("  No peers registered. Run deixic-code a2a accept <code>.");
    } else {
        for peer in &cockpit.peers {
            let status = if peer.status == "online" {
                "online"
            } else {
                "down"
            };
            let mut task_summary = Vec::new();
            if peer.task_counts.running_tasks > 0 {
                task_summary.push(format!("{} running", peer.task_counts.running_tasks));
            }
            if peer.task_counts.action_required_tasks > 0 {
                task_summary.push(format!(
                    "{} waiting",
                    peer.task_counts.action_required_tasks
                ));
            }
            if peer.task_counts.failed_tasks > 0 {
                task_summary.push(format!("{} failed", peer.task_counts.failed_tasks));
            }
            let summary = if task_summary.is_empty() {
                String::new()
            } else {
                format!(" ({})", task_summary.join(", "))
            };
            println!("{status} {} {}{summary}", peer.name, peer.url);
            if let Some(last) = &peer.last_task {
                println!("  last={} {} {}", last.id, last.state, last.text);
            }
            if let Some(error) = &peer.error {
                println!("  error={error}");
            }
        }
    }
    println!("\nTasks");
    if cockpit.tasks.is_empty() {
        println!("  No delegated tasks recorded yet.");
    } else {
        for task in &cockpit.tasks {
            let peer_label = if task.orphaned_peer == Some(true) {
                format!("{} (orphaned peer)", task.peer)
            } else {
                task.peer.clone()
            };
            println!(
                "{peer_label} {} {} {}",
                task.task_id, task.status, task.updated_at
            );
            println!("  {}", task.text);
            if let Some(next) = &task.next_command {
                println!("  next: {next}");
            }
        }
    }
    if !cockpit.next_actions.is_empty() {
        println!("\nNext actions");
        for action in &cockpit.next_actions {
            println!("[{}] {}", action.severity, action.label);
            println!("  {}", action.command);
        }
    }
    Ok(0)
}

async fn run_card(args: &[String]) -> Result<i32> {
    let flags = FlagSet::parse(args);
    let peer_name = flags.first_positional();
    let peer = resolve_peer(
        peer_name,
        ResolvePeerOptions {
            registry_path: flags.string("--registry"),
            timeout_ms: flags.number("--timeout-ms"),
            token: None,
            max_attempts: None,
        },
    )?;
    let card = discover_agent_card(&peer.config).await?;
    println!("{}", serde_json::to_string_pretty(&card)?);
    Ok(0)
}

async fn run_send(args: &[String]) -> Result<i32> {
    let flags = FlagSet::parse(args);
    let peer_name = flags
        .first_positional()
        .context("Usage: deixic-code a2a send <peer> <text>")?;
    let text = flags.remaining_positionals_from(1).join(" ");
    let text = text.trim();
    if text.is_empty() {
        bail!("Usage: deixic-code a2a send <peer> <text>");
    }
    let wait = flags.boolean("--wait");
    let pending = start_peer_message(PeerMessageInput {
        peer: Some(peer_name.to_string()),
        text: text.to_string(),
        request_kind: "maestro-peer-message".into(),
        ledger_kind: "message".into(),
        metadata: Map::new(),
        registry_path: flags.string("--registry"),
        tasks_path: flags.string("--tasks"),
        timeout_ms: flags.number("--timeout-ms"),
    })
    .await?;
    if let Some(warning) = &pending.ledger_warning {
        eprintln!("A2A task ledger warning: {warning}");
    }

    let task = if wait {
        let completed = wait_for_peer_message(
            &pending,
            flags.number("--max-wait-ms").unwrap_or(DEFAULT_WAIT_MS),
            flags
                .number("--interval-ms")
                .unwrap_or(DEFAULT_WAIT_INTERVAL_MS),
        )
        .await?;
        if let Some(warning) = completed.ledger_warning {
            eprintln!("A2A task ledger warning: {warning}");
        }
        completed.task
    } else {
        pending.task
    };

    print_task(&task);
    Ok(0)
}

async fn run_delegate(args: &[String]) -> Result<i32> {
    let flags = FlagSet::parse(args);
    if flags.boolean("--platform") {
        return run_platform_delegate(&flags).await;
    }
    let discover = flags.boolean("--discover");
    let peer_name = if discover {
        None
    } else {
        Some(
            flags
                .first_positional()
                .context("Usage: deixic-code a2a delegate <peer> <text>")?
                .to_string(),
        )
    };
    let text = if discover {
        flags.remaining_positionals_from(0).join(" ")
    } else {
        flags.remaining_positionals_from(1).join(" ")
    };
    let text = text.trim();
    if text.is_empty() {
        if discover {
            bail!("Usage: deixic-code a2a delegate --discover --skill <skill-id> <text>");
        }
        bail!("Usage: deixic-code a2a delegate <peer> <text>");
    }
    let (peer, discovery_selection) = if discover {
        resolve_discovered_delegate_peer(&flags).await?
    } else {
        (
            resolve_peer(
                peer_name.as_deref(),
                ResolvePeerOptions {
                    registry_path: flags.string("--registry"),
                    timeout_ms: flags.number("--timeout-ms"),
                    token: None,
                    max_attempts: None,
                },
            )?,
            None,
        )
    };
    let wait = flags.boolean("--wait");
    let role = flags.string("--role");
    let cwd = flags.string("--cwd").or_else(|| {
        std::env::current_dir()
            .ok()
            .map(|p| p.display().to_string())
    });
    let message_id = format!("maestro-a2a-message-{}", uuid::Uuid::new_v4());
    let context_id = peer_context_id(&peer.config);
    let skill_id = flags.string("--skill");
    let skill = select_peer_skill(peer.entry.skills.as_ref(), skill_id.as_deref());
    let mut metadata = Map::new();
    metadata.insert("requestKind".into(), json!("maestro-peer-delegation"));
    metadata.insert("relayPeer".into(), json!(peer.name));
    if let Some(role) = &role {
        metadata.insert("delegationRole".into(), json!(role));
    }
    if let Some(cwd) = &cwd {
        metadata.insert("delegationCwd".into(), json!(cwd));
    }
    if discover {
        metadata.insert("discoverySource".into(), json!("platform-agent-registry"));
    }
    if let Some(skill_id) = &skill_id {
        metadata.insert("a2aSkillId".into(), json!(skill_id));
        let mut subagent_request = Map::new();
        subagent_request.insert("skillId".into(), json!(skill_id));
        if let Some(skill) = &skill {
            if let Some(name) = skill.get("name") {
                subagent_request.insert("skillName".into(), name.clone());
            }
            if let Some(description) = skill.get("description") {
                subagent_request.insert("description".into(), description.clone());
            }
        }
        if let Some(role) = &role {
            subagent_request.insert("role".into(), json!(role));
        }
        if let Some(cwd) = &cwd {
            subagent_request.insert("cwd".into(), json!(cwd));
        }
        metadata.insert(
            "evalops.subagentRequest".into(),
            Value::Object(subagent_request),
        );
    }
    if let Some(selection) = &discovery_selection {
        metadata.insert("evalops.a2aDiscovery".into(), selection.clone());
    }

    let mut ledger_metadata = Map::new();
    ledger_metadata.insert("requestKind".into(), json!("maestro-peer-delegation"));
    ledger_metadata.insert("relayPeer".into(), json!(peer.name));
    if let Some(role) = &role {
        ledger_metadata.insert("delegationRole".into(), json!(role));
    }
    if let Some(cwd) = &cwd {
        ledger_metadata.insert("delegationCwd".into(), json!(cwd));
    }
    if discover {
        ledger_metadata.insert("discoverySource".into(), json!("platform-agent-registry"));
    }
    if let Some(skill_id) = &skill_id {
        ledger_metadata.insert("a2aSkillId".into(), json!(skill_id));
    }

    let sent = send_message(
        &peer.config,
        SendMessageInput {
            text: text.to_string(),
            message_id: message_id.clone(),
            context_id: Some(context_id.clone()),
            task_id: None,
            metadata: Some(Value::Object(metadata)),
            return_immediately: true,
        },
    )
    .await?;
    println!("Delegated to {} as task {}", peer.name, sent.task.id);
    if let Err(error) = record_task_start(RecordTaskStartInput {
        path: flags.string("--tasks").as_deref(),
        peer: &peer.name,
        peer_display_name: peer.entry.display_name.as_deref(),
        task: &sent.task,
        text,
        message_id: Some(&message_id),
        context_id: Some(&context_id),
        kind: "delegation",
        metadata: Some(Value::Object(ledger_metadata)),
    }) {
        eprintln!("A2A task ledger warning: could not record delegated task locally: {error:#}");
    }
    let task = if wait {
        let task = wait_for_task(
            &peer.config,
            &sent.task.id,
            flags.number("--max-wait-ms").unwrap_or(DEFAULT_WAIT_MS),
            flags
                .number("--interval-ms")
                .unwrap_or(DEFAULT_WAIT_INTERVAL_MS),
        )
        .await?;
        if let Err(error) =
            update_task_in_ledger(flags.string("--tasks").as_deref(), &peer.name, &task)
        {
            eprintln!(
                "A2A task ledger warning: could not sync delegated task result locally: {error:#}"
            );
        }
        task
    } else {
        sent.task
    };
    print_task(&task);
    Ok(0)
}

async fn run_platform_delegate(flags: &FlagSet) -> Result<i32> {
    let text = flags.positionals.join(" ");
    let text = text.trim();
    if text.is_empty() {
        bail!(
            "Usage: deixic-code a2a delegate --platform --from-agent-id <agent-id> \
             [--to-agent-id <agent-id>|--capability <capability>] --skill <skill-id> <text>"
        );
    }
    let from_agent_id = flags
        .string("--from-agent-id")
        .or_else(|| {
            env_first(&[
                "MAESTRO_A2A_AGENT_ID",
                "MAESTRO_AGENT_ID",
                "MAESTRO_EVALOPS_AGENT_ID",
                "EVALOPS_AGENT_ID",
            ])
        })
        .context("Provide --from-agent-id or set MAESTRO_A2A_AGENT_ID/MAESTRO_AGENT_ID.")?;
    let to_agent_id = flags.string("--to-agent-id");
    let required_capability = flags.string("--capability");
    let skill_id = flags.string("--skill");
    if to_agent_id.is_none() && required_capability.is_none() && skill_id.is_none() {
        bail!("Provide --to-agent-id, --capability, or --skill for Platform routing.");
    }
    let role = flags.string("--role");
    let cwd = flags.string("--cwd").or_else(|| {
        std::env::current_dir()
            .ok()
            .map(|p| p.display().to_string())
    });
    let requested_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let mut context = Map::new();
    context.insert("requestKind".into(), json!("maestro-peer-delegation"));
    context.insert("transport".into(), json!("platform-a2a"));
    context.insert("prompt".into(), json!(text));
    context.insert("source".into(), json!("maestro-cli"));
    context.insert("requestedAt".into(), json!(requested_at));
    if let Some(role) = &role {
        context.insert("role".into(), json!(role));
    }
    if let Some(cwd) = &cwd {
        context.insert("cwd".into(), json!(cwd));
    }
    if let Some(skill_id) = &skill_id {
        context.insert("a2aSkillId".into(), json!(skill_id));
    }
    if let Some(capability) = &required_capability {
        context.insert("requiredCapability".into(), json!(capability));
    }
    let reason = flags.string("--reason").unwrap_or_else(|| {
        let target = skill_id
            .as_deref()
            .or(required_capability.as_deref())
            .unwrap_or("a2a peer");
        let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
        format!(
            "deixic-code a2a delegate {target}: {}",
            compact.chars().take(120).collect::<String>()
        )
    });
    let result = delegate_agent_with_platform(
        DelegateAgentInput {
            workspace_id: flags.string("--workspace-id"),
            from_agent_id: from_agent_id.clone(),
            to_agent_id: to_agent_id.clone(),
            required_capability: required_capability.clone(),
            a2a_skill_id: skill_id.clone(),
            objective_id: flags.string("--objective-id"),
            workflow_run_id: flags.string("--workflow-run-id"),
            workflow_step_id: flags.string("--workflow-step-id"),
            context_payload: Some(Value::Object(context)),
            reason: Some(reason),
        },
        None,
    )
    .await?
    .with_context(agent_registry_not_configured_message)?;
    if flags.boolean("--json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "delegation": result }))?
        );
        return Ok(0);
    }
    println!(
        "Platform A2A delegation {}: {}",
        result.id.as_deref().unwrap_or("(submitted)"),
        result.status.as_deref().unwrap_or("submitted")
    );
    println!("From: {from_agent_id}");
    if let Some(to) = result.to_agent_id.as_ref().or(to_agent_id.as_ref()) {
        println!("To: {to}");
    }
    if let Some(task_id) = &result.a2a_task_id {
        println!("Remote task: {task_id}");
    }
    if let Some(endpoint) = &result.a2a_endpoint_url {
        println!("Endpoint: {endpoint}");
    }
    if let Some(status) = &result.a2a_dispatch_status {
        println!("Dispatch: {status}");
    }
    if let Some(waits) = &result.a2a_resume_wait_contracts {
        if !waits.is_empty() {
            println!("Resume waits: {}", waits.len());
        }
    }
    Ok(0)
}

async fn run_control(args: &[String]) -> Result<i32> {
    let flags = FlagSet::parse(args);
    let delegation_id = flags
        .string("--delegation-id")
        .or_else(|| flags.first_positional().map(str::to_string))
        .context("Usage: deixic-code a2a control <delegation-id> --mode <mode> [message]")?;
    let mode_raw = flags
        .string("--mode")
        .or_else(|| {
            if flags.string("--delegation-id").is_some() {
                flags.first_positional().map(str::to_string)
            } else {
                flags.positional(1).map(str::to_string)
            }
        })
        .context("Provide --mode steer|followup|collect|interrupt|cancel")?;
    let mode = normalize_a2a_control_mode(&mode_raw)?;
    let message = flags.string("--message").or_else(|| {
        let start = if flags.string("--delegation-id").is_some() {
            usize::from(flags.string("--mode").is_none())
        } else if flags.string("--mode").is_some() {
            1
        } else {
            2
        };
        let joined = flags.remaining_positionals_from(start).join(" ");
        let trimmed = joined.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    });
    let result = control_a2a_delegation_task_with_platform(
        ControlA2ADelegationTaskInput {
            workspace_id: flags.string("--workspace-id"),
            delegation_id,
            mode,
            message,
            idempotency_key: flags.string("--idempotency-key"),
            target_run_id: flags.string("--target-run-id"),
            child_run_id: flags.string("--child-run-id"),
            subagent_lane_id: flags.string("--subagent-lane-id"),
            work_item_id: flags.string("--work-item-id"),
            metadata: Some(json!({
                "source": "maestro-cli",
                "requestedAt": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            })),
        },
        None,
    )
    .await?
    .with_context(agent_registry_not_configured_message)?;
    println!(
        "Control {}: {}",
        result
            .remote_task
            .as_ref()
            .and_then(|task| task.control_id.as_deref())
            .unwrap_or("(queued)"),
        result
            .remote_task
            .as_ref()
            .and_then(|task| task.state.as_deref())
            .unwrap_or("submitted")
    );
    if let Some(task_id) = result
        .remote_task
        .as_ref()
        .and_then(|task| task.task_id.as_deref())
    {
        println!("Task: {task_id}");
    }
    if let Some(delegation_id) = result
        .delegation
        .as_ref()
        .and_then(|delegation| delegation.id.as_deref())
    {
        println!("Delegation: {delegation_id}");
    }
    Ok(0)
}

async fn run_graph(args: &[String]) -> Result<i32> {
    let flags = FlagSet::parse(args);
    let delegation_id = flags
        .string("--delegation-id")
        .or_else(|| flags.first_positional().map(str::to_string));
    let root_delegation_id = flags
        .string("--root-delegation-id")
        .or_else(|| flags.string("--root"));
    if delegation_id.is_none() && root_delegation_id.is_none() {
        bail!(
            "Usage: deixic-code a2a graph <delegation-id> [--root <root-delegation-id>] [--json]"
        );
    }
    let result = get_a2a_delegation_graph_with_platform(
        GetA2ADelegationGraphInput {
            workspace_id: flags.string("--workspace-id"),
            delegation_id,
            root_delegation_id,
            max_depth: flags.number("--max-depth"),
            limit: flags.number("--limit"),
        },
        None,
    )
    .await?
    .with_context(agent_registry_not_configured_message)?;
    if flags.boolean("--json") {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(0);
    }
    println!(
        "Platform A2A delegation graph {}",
        result.root_delegation_id.as_deref().unwrap_or("")
    );
    let mut summary = Vec::new();
    if let Some(total) = result.total {
        summary.push(format!("total={total}"));
    }
    if let Some(truncated) = result.truncated {
        summary.push(format!("truncated={truncated}"));
    }
    if let Some(missing) = &result.missing_parent_delegation_ids {
        if !missing.is_empty() {
            summary.push(format!("missing_parents={}", missing.len()));
        }
    }
    if !summary.is_empty() {
        println!("  {}", summary.join(" "));
    }
    if result.nodes.is_empty() {
        println!("  No delegation graph nodes returned.");
        return Ok(0);
    }
    for node in &result.nodes {
        let label = node
            .delegation
            .as_ref()
            .and_then(|delegation| delegation.id.clone())
            .or_else(|| node.depth.map(|depth| format!("depth-{depth}")))
            .unwrap_or_else(|| "delegation".into());
        let mut details = Vec::new();
        if let Some(depth) = node.depth {
            details.push(format!("depth={depth}"));
        }
        if let Some(status) = node
            .delegation
            .as_ref()
            .and_then(|delegation| delegation.status.clone())
        {
            details.push(status);
        }
        if node.terminal == Some(true) {
            details.push("terminal".into());
        }
        if let Some(children) = node.child_count {
            details.push(format!("children={children}"));
        }
        println!("{label} {}", details.join(" "));
        let task_id = node
            .delegation
            .as_ref()
            .and_then(|delegation| delegation.a2a_task_id.clone());
        let lineage = node
            .delegation
            .as_ref()
            .and_then(|delegation| delegation.a2a_delegation_chain.as_ref())
            .map(|chain| chain.join(" -> "));
        if task_id.is_some() || lineage.is_some() {
            let mut parts = Vec::new();
            if let Some(task_id) = task_id {
                parts.push(format!("task={task_id}"));
            }
            if let Some(lineage) = lineage {
                parts.push(format!("lineage={lineage}"));
            }
            println!("  {}", parts.join(" "));
        }
    }
    if !result.edges.is_empty() {
        println!("  edges={}", result.edges.len());
    }
    Ok(0)
}

async fn run_coordinate(args: &[String]) -> Result<i32> {
    let flags = FlagSet::parse(args);
    if flags.positionals.len() > 1 {
        bail!("Usage: deixic-code a2a coordinate [peer] [--reply <text>] [--wait]");
    }
    let peer_name = flags.first_positional().map(str::to_string);
    if flags.flags.contains_key("--reply") && flags.string("--reply").is_none() {
        bail!("Usage: deixic-code a2a coordinate [peer] --reply <text> [--wait]");
    }
    if let Some(reply_text) = flags.string("--reply") {
        return run_coordinate_reply(&flags, peer_name.as_deref(), &reply_text).await;
    }
    refresh_task_ledger(
        flags.string("--tasks").as_deref(),
        flags.string("--registry").as_deref(),
        flags.number("--timeout-ms"),
        peer_name.as_deref(),
    )
    .await?;
    let ledger = load_task_ledger(flags.string("--tasks").as_deref())?;
    let tasks = actionable_task_entries(&ledger.tasks, peer_name.as_deref());
    let path = get_task_ledger_path(flags.string("--tasks").as_deref())?;
    if flags.boolean("--json") {
        let payload = json!({
            "path": path.display().to_string(),
            "tasks": tasks.iter().map(|entry| json!({
                "id": entry.id,
                "kind": entry.kind,
                "peer": entry.peer,
                "taskId": entry.task_id,
                "contextId": entry.context_id,
                "state": entry.state,
                "text": entry.text,
                "responseText": entry.response_text,
                "workGraph": entry.work_graph,
                "updatedAt": entry.updated_at,
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(0);
    }
    println!("A2A coordinate ({})", path.display());
    if tasks.is_empty() {
        println!("  No actionable A2A tasks require coordination.");
        return Ok(0);
    }
    for task in tasks {
        println!(
            "{} {} {} {}",
            task.peer, task.task_id, task.state, task.updated_at
        );
        println!("  {}", task.text);
        if let Some(response) = &task.response_text {
            println!("  {response}");
        }
        if flags.boolean("--work-graph") {
            if let Some(graph) = &task.work_graph {
                println!("  workGraph={}", serde_json::to_string(graph)?);
            }
        }
    }
    Ok(0)
}

async fn run_coordinate_reply(flags: &FlagSet, peer_name: Option<&str>, text: &str) -> Result<i32> {
    refresh_task_ledger(
        flags.string("--tasks").as_deref(),
        flags.string("--registry").as_deref(),
        flags.number("--timeout-ms"),
        peer_name,
    )
    .await?;
    let ledger = load_task_ledger(flags.string("--tasks").as_deref())?;
    let entry = select_coordinate_reply_task(&ledger.tasks, peer_name)?;
    let peer = resolve_peer(
        Some(&entry.peer),
        ResolvePeerOptions {
            registry_path: flags.string("--registry"),
            timeout_ms: flags.number("--timeout-ms"),
            token: None,
            max_attempts: None,
        },
    )?;
    let message_id = format!("maestro-a2a-message-{}", uuid::Uuid::new_v4());
    let mut metadata = Map::new();
    metadata.insert("requestKind".into(), json!("maestro-peer-coordinate-reply"));
    metadata.insert("relayPeer".into(), json!(peer.name));
    metadata.insert("referencedTaskId".into(), json!(entry.task_id));
    let sent = send_message(
        &peer.config,
        SendMessageInput {
            text: text.to_string(),
            message_id: message_id.clone(),
            context_id: entry.context_id.clone(),
            task_id: Some(entry.task_id.clone()),
            metadata: Some(Value::Object(metadata.clone())),
            return_immediately: true,
        },
    )
    .await?;
    let mut reply_task = sent.task.clone();
    reply_task.id = entry.task_id.clone();
    if reply_task.context_id.is_none() {
        reply_task.context_id = entry.context_id.clone();
    }
    if !flags.boolean("--json") {
        println!("Coordinated {} task {}", peer.name, entry.task_id);
    }
    if let Err(error) =
        update_task_in_ledger(flags.string("--tasks").as_deref(), &peer.name, &reply_task)
    {
        eprintln!("A2A task ledger warning: could not record coordinate reply locally: {error:#}");
    }
    let task = if flags.boolean("--wait") {
        let task = wait_for_task(
            &peer.config,
            &entry.task_id,
            flags.number("--max-wait-ms").unwrap_or(DEFAULT_WAIT_MS),
            flags
                .number("--interval-ms")
                .unwrap_or(DEFAULT_WAIT_INTERVAL_MS),
        )
        .await?;
        if let Err(error) =
            update_task_in_ledger(flags.string("--tasks").as_deref(), &peer.name, &task)
        {
            eprintln!(
                "A2A task ledger warning: could not sync coordinate task result locally: {error:#}"
            );
        }
        task
    } else {
        reply_task
    };
    if flags.boolean("--json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "peer": peer.name, "task": task }))?
        );
        return Ok(0);
    }
    print_task(&task);
    Ok(0)
}

fn run_telemetry(args: &[String]) -> Result<i32> {
    let flags = FlagSet::parse(args);
    let events_path = flags
        .string("--events")
        .context("Usage: deixic-code a2a telemetry --events <path> --swarm-id <id>")?;
    let swarm_id = flags
        .string("--swarm-id")
        .or_else(|| flags.first_positional().map(str::to_string))
        .context("Usage: deixic-code a2a telemetry --events <path> --swarm-id <id>")?;
    let events = load_a2a_telemetry_events(&events_path)?;
    let inspection = inspect_a2a_telemetry(&swarm_id, &events);
    if flags.boolean("--json") {
        println!("{}", serde_json::to_string_pretty(&inspection)?);
        return Ok(0);
    }
    println!(
        "A2A telemetry {swarm_id} {}",
        if inspection.complete {
            "complete"
        } else {
            "incomplete"
        }
    );
    println!(
        "  events={} lanes={} completed={} failed={} missing={}",
        inspection.counts.events,
        inspection.counts.lanes,
        inspection.counts.completed_lanes,
        inspection.counts.failed_lanes,
        inspection.counts.missing_telemetry_lanes
    );
    if inspection.counts.ordering_anomaly_lanes > 0 {
        println!(
            "  ordering_anomalies={}",
            inspection.counts.ordering_anomaly_lanes
        );
    }
    for lane in &inspection.lanes {
        println!(
            "{} {} {}",
            lane.lane_id,
            lane.status.as_deref().unwrap_or("(unknown)"),
            lane.peer.as_deref().unwrap_or("")
        );
        let mut details = Vec::new();
        if let Some(parent) = &lane.parent_task_id {
            details.push(format!("parent={parent}"));
        }
        if let Some(task) = &lane.a2a_task_id {
            details.push(format!("task={task}"));
        }
        if let Some(message) = &lane.a2a_message_id {
            details.push(format!("message={message}"));
        }
        if let Some(context) = &lane.context_id {
            details.push(format!("context={context}"));
        }
        if let Some(source) = &lane.source {
            details.push(format!("source={source}"));
        }
        if !details.is_empty() {
            println!("  {}", details.join(" "));
        }
        let mut timing = Vec::new();
        if let Some(ms) = lane.timing.selection_to_dispatch_ms {
            timing.push(format!("selected_to_dispatch={ms}ms"));
        }
        if let Some(ms) = lane.timing.observed_duration_ms {
            timing.push(format!("observed_duration={ms}ms"));
        }
        if let Some(ms) = lane.timing.reported_duration_ms {
            timing.push(format!("reported_duration={ms}ms"));
        }
        if !timing.is_empty() {
            println!("  {}", timing.join(" "));
        }
        if !lane.missing_event_types.is_empty() {
            println!("  missing: {}", lane.missing_event_types.join(", "));
        }
        if !lane.ordering_anomalies.is_empty() {
            println!("  anomalies: {}", lane.ordering_anomalies.join(", "));
        }
    }
    Ok(0)
}

async fn run_reply(args: &[String]) -> Result<i32> {
    let flags = FlagSet::parse(args);
    let peer_name = flags
        .first_positional()
        .context("Usage: deixic-code a2a reply <peer> <task-id> <text>")?;
    let task_id = flags
        .positional(1)
        .context("Usage: deixic-code a2a reply <peer> <task-id> <text>")?;
    let text = flags.remaining_positionals_from(2).join(" ");
    let text = text.trim();
    if text.is_empty() {
        bail!("Usage: deixic-code a2a reply <peer> <task-id> <text>");
    }
    let peer = resolve_peer(
        Some(peer_name),
        ResolvePeerOptions {
            registry_path: flags.string("--registry"),
            timeout_ms: flags.number("--timeout-ms"),
            token: None,
            max_attempts: None,
        },
    )?;
    let wait = flags.boolean("--wait");
    let message_id = format!("maestro-a2a-message-{}", uuid::Uuid::new_v4());
    let mut metadata = serde_json::Map::new();
    metadata.insert("requestKind".into(), json!("maestro-peer-reply"));
    metadata.insert("relayPeer".into(), json!(peer.name));

    let sent = send_message(
        &peer.config,
        SendMessageInput {
            text: text.to_string(),
            message_id,
            context_id: None,
            task_id: Some(task_id.to_string()),
            metadata: Some(serde_json::Value::Object(metadata)),
            return_immediately: true,
        },
    )
    .await?;

    if let Err(error) =
        update_task_in_ledger(flags.string("--tasks").as_deref(), &peer.name, &sent.task)
    {
        eprintln!("A2A task ledger warning: could not record reply locally: {error:#}");
    }

    let task = if wait {
        let task = wait_for_task(
            &peer.config,
            &sent.task.id,
            flags.number("--max-wait-ms").unwrap_or(DEFAULT_WAIT_MS),
            flags
                .number("--interval-ms")
                .unwrap_or(DEFAULT_WAIT_INTERVAL_MS),
        )
        .await?;
        if let Err(error) =
            update_task_in_ledger(flags.string("--tasks").as_deref(), &peer.name, &task)
        {
            eprintln!("A2A task ledger warning: could not sync reply result locally: {error:#}");
        }
        task
    } else {
        sent.task
    };
    print_task(&task);
    Ok(0)
}

async fn run_tasks(args: &[String]) -> Result<i32> {
    let flags = FlagSet::parse(args);
    let peer_filter = flags.first_positional().map(str::to_string);
    let tasks_path = flags.string("--tasks");
    if flags.boolean("--refresh") {
        refresh_task_ledger(
            tasks_path.as_deref(),
            flags.string("--registry").as_deref(),
            flags.number("--timeout-ms"),
            peer_filter.as_deref(),
        )
        .await?;
    }
    let ledger = load_task_ledger(tasks_path.as_deref())?;
    let tasks = list_task_entries(&ledger, peer_filter.as_deref());
    let path = get_task_ledger_path(tasks_path.as_deref())?;
    if flags.boolean("--json") {
        let payload = json!({
            "path": path.display().to_string(),
            "tasks": tasks.iter().map(|entry| json!({
                "id": entry.id,
                "kind": entry.kind,
                "peer": entry.peer,
                "taskId": entry.task_id,
                "state": entry.state,
                "text": entry.text,
                "responseText": entry.response_text,
                "workGraph": entry.work_graph,
                "updatedAt": entry.updated_at,
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(0);
    }
    println!("A2A tasks ({})", path.display());
    if tasks.is_empty() {
        println!("  No delegated tasks recorded yet.");
        return Ok(0);
    }
    for task in tasks {
        println!(
            "{} {} {} {}",
            task.peer, task.task_id, task.state, task.updated_at
        );
        println!("  {}", task.text);
        if let Some(response) = &task.response_text {
            println!("  {response}");
        }
        if flags.boolean("--work-graph") {
            if let Some(graph) = &task.work_graph {
                println!("  workGraph={}", serde_json::to_string(graph)?);
            }
        }
    }
    Ok(0)
}

async fn run_wait(args: &[String]) -> Result<i32> {
    let flags = FlagSet::parse(args);
    let peer_name = flags
        .first_positional()
        .context("Usage: deixic-code a2a wait <peer> <task-id>")?;
    let task_id = flags
        .positional(1)
        .context("Usage: deixic-code a2a wait <peer> <task-id>")?;
    let peer = resolve_peer(
        Some(peer_name),
        ResolvePeerOptions {
            registry_path: flags.string("--registry"),
            timeout_ms: flags.number("--timeout-ms"),
            token: None,
            max_attempts: None,
        },
    )?;
    let task = wait_for_task(
        &peer.config,
        task_id,
        flags.number("--max-wait-ms").unwrap_or(DEFAULT_WAIT_MS),
        flags
            .number("--interval-ms")
            .unwrap_or(DEFAULT_WAIT_INTERVAL_MS),
    )
    .await?;
    if let Err(error) = update_task_in_ledger(flags.string("--tasks").as_deref(), &peer.name, &task)
    {
        eprintln!("A2A task ledger warning: could not sync task result locally: {error:#}");
    }
    print_task(&task);
    Ok(0)
}

async fn refresh_task_ledger(
    tasks_path: Option<&str>,
    registry_path: Option<&str>,
    timeout_ms: Option<u64>,
    peer_filter: Option<&str>,
) -> Result<()> {
    let ledger = load_task_ledger(tasks_path)?;
    for entry in list_task_entries(&ledger, peer_filter) {
        if is_terminal_state(&entry.state) {
            continue;
        }
        let peer = resolve_peer(
            Some(&entry.peer),
            ResolvePeerOptions {
                registry_path: registry_path.map(str::to_string),
                timeout_ms,
                token: None,
                max_attempts: None,
            },
        )?;
        let task = get_task(&peer.config, &entry.task_id).await?;
        update_task_in_ledger(tasks_path, &entry.peer, &task)?;
    }
    Ok(())
}

fn print_task(task: &A2ATask) {
    println!("Task {}: {}", task.id, task.status.state);
    if let Some(text) = extract_task_text(task) {
        println!("{text}");
    }
}

fn env_first(names: &[&str]) -> Option<String> {
    for name in names {
        if let Ok(value) = std::env::var(name) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn actionable_task_entries<'a>(
    tasks: &'a [TaskLedgerEntry],
    peer: Option<&str>,
) -> Vec<&'a TaskLedgerEntry> {
    let mut entries: Vec<_> = tasks
        .iter()
        .filter(|entry| match peer {
            None => true,
            Some(name) => entry.peer == name,
        })
        .filter(|entry| is_action_required_state(&entry.state))
        .collect();
    entries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    entries
}

fn select_coordinate_reply_task<'a>(
    tasks: &'a [TaskLedgerEntry],
    peer: Option<&str>,
) -> Result<&'a TaskLedgerEntry> {
    let tasks = actionable_task_entries(tasks, peer);
    if tasks.len() > 1 {
        bail!(
            "Multiple actionable A2A tasks found; use `deixic-code a2a reply <peer> <task-id> <text>`."
        );
    }
    tasks
        .into_iter()
        .next()
        .context("No actionable A2A task is waiting for coordinator input.")
}

fn discovered_peer_json(candidate: &PlatformAgentRegistryA2APeerCandidate) -> Value {
    json!({
        "agentId": candidate.agent.id,
        "name": candidate.agent.name,
        "status": candidate.agent.status,
        "endpointUrl": candidate.endpoint_url,
        "endpointKind": candidate.endpoint_kind,
        "agentCardUrl": candidate.agent_card_url,
        "protocolBinding": candidate.protocol_binding,
        "protocolVersion": candidate.protocol_version,
        "skills": candidate.skills,
        "supportedExtensions": candidate.supported_extensions,
        "pushNotifications": candidate.push_notifications,
    })
}

fn print_discovery_evidence(evidence: Option<&PlatformAgentDiscoveryEvidence>) {
    let Some(evidence) = evidence else {
        return;
    };
    let mut summary = Vec::new();
    if let Some(decision) = &evidence.decision {
        summary.push(format!("decision={decision}"));
    }
    if let Some(reason) = &evidence.reason {
        summary.push(format!("reason={reason}"));
    }
    if let Some(matched) = evidence.matched_count {
        summary.push(format!("matched={matched}"));
    }
    if let Some(candidates) = evidence.candidate_count {
        summary.push(format!("candidates={candidates}"));
    }
    if let Some(skill) = &evidence.a2a_skill_id {
        summary.push(format!("skill={skill}"));
    }
    if let Some(capability) = &evidence.capability {
        summary.push(format!("capability={capability}"));
    }
    if !summary.is_empty() {
        println!("  discovery {}", summary.join(" "));
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ImportedPeer {
    name: String,
    path: String,
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_id: Option<String>,
}

fn import_discovered_peers(
    candidates: &[PlatformAgentRegistryA2APeerCandidate],
    discovery_evidence: Option<&PlatformAgentDiscoveryEvidence>,
    registry_path: Option<&str>,
    make_default: bool,
) -> Result<Vec<ImportedPeer>> {
    let (path, mut registry) = load_peer_registry(registry_path)?;
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let mut default_assigned = false;
    let mut imported_names = BTreeSet::new();
    let mut imported = Vec::new();
    for (index, candidate) in candidates.iter().enumerate() {
        let base_name = discovered_peer_name(candidate, index);
        let name = unique_discovered_peer_name(
            &base_name,
            candidate,
            &mut imported_names,
            &registry.peers,
        )?;
        let previous = registry.peers.get(&name).cloned();
        let mut metadata = Map::new();
        metadata.insert("source".into(), json!("platform-agent-registry"));
        if let Some(id) = &candidate.agent.id {
            metadata.insert("platformAgentId".into(), json!(id));
        }
        if let Some(agent_type) = &candidate.agent.agent_type {
            metadata.insert("platformAgentType".into(), json!(agent_type));
        }
        if let Some(status) = &candidate.agent.status {
            metadata.insert("platformAgentStatus".into(), json!(status));
        }
        if let Some(kind) = &candidate.endpoint_kind {
            metadata.insert("selectedEndpoint".into(), json!(kind));
        }
        if let Some(push) = candidate.push_notifications {
            metadata.insert("a2aPushNotifications".into(), json!(push));
        }
        if let Some(evidence) = discovery_evidence {
            if let Some(decision) = &evidence.decision {
                metadata.insert("platformDiscoveryDecision".into(), json!(decision));
            }
            if let Some(count) = evidence.candidate_count {
                metadata.insert("platformDiscoveryCandidateCount".into(), json!(count));
            }
            if let Some(count) = evidence.matched_count {
                metadata.insert("platformDiscoveryMatchedCount".into(), json!(count));
            }
        }
        let skills = if candidate.skills.is_empty() {
            previous.as_ref().and_then(|entry| entry.skills.clone())
        } else {
            Some(Value::Array(
                candidate
                    .skills
                    .iter()
                    .map(|skill| serde_json::to_value(skill).unwrap_or(Value::Null))
                    .collect(),
            ))
        };
        let mut capabilities = previous
            .as_ref()
            .and_then(|entry| entry.capabilities.clone())
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        if let Some(push) = candidate.push_notifications {
            capabilities.insert("pushNotifications".into(), json!(push));
        }
        let entry = PeerRegistryEntry {
            url: candidate.endpoint_url.clone(),
            display_name: candidate.agent.name.clone().or_else(|| {
                previous
                    .as_ref()
                    .and_then(|entry| entry.display_name.clone())
            }),
            agent_card_url: candidate.agent_card_url.clone().or_else(|| {
                previous
                    .as_ref()
                    .and_then(|entry| entry.agent_card_url.clone())
            }),
            protocol_binding: candidate.protocol_binding.clone().or_else(|| {
                previous
                    .as_ref()
                    .and_then(|entry| entry.protocol_binding.clone())
            }),
            protocol_version: candidate.protocol_version.clone().or_else(|| {
                previous
                    .as_ref()
                    .and_then(|entry| entry.protocol_version.clone())
            }),
            token_env: previous.as_ref().and_then(|entry| entry.token_env.clone()),
            token_file: previous.as_ref().and_then(|entry| entry.token_file.clone()),
            organization_id: previous
                .as_ref()
                .and_then(|entry| entry.organization_id.clone()),
            workspace_id: candidate.agent.workspace_id.clone().or_else(|| {
                previous
                    .as_ref()
                    .and_then(|entry| entry.workspace_id.clone())
            }),
            agent_id: candidate
                .agent
                .id
                .clone()
                .or_else(|| previous.as_ref().and_then(|entry| entry.agent_id.clone())),
            session_id: previous.as_ref().and_then(|entry| entry.session_id.clone()),
            actor_id: previous.as_ref().and_then(|entry| entry.actor_id.clone()),
            timeout_ms: previous.as_ref().and_then(|entry| entry.timeout_ms),
            max_attempts: previous.as_ref().and_then(|entry| entry.max_attempts),
            capabilities: if capabilities.is_empty() {
                None
            } else {
                Some(Value::Object(capabilities))
            },
            skills,
            key_fingerprint: previous
                .as_ref()
                .and_then(|entry| entry.key_fingerprint.clone()),
            metadata: Some(Value::Object(metadata)),
            created_at: previous
                .as_ref()
                .and_then(|entry| entry.created_at.clone())
                .or(Some(now.clone())),
            updated_at: Some(now.clone()),
        };
        imported.push(ImportedPeer {
            name: name.clone(),
            path: String::new(),
            url: entry.url.clone(),
            agent_id: entry.agent_id.clone(),
        });
        registry.peers.insert(name.clone(), entry);
        if (make_default || registry.default_peer.is_none()) && !default_assigned {
            registry.default_peer = Some(name);
            default_assigned = true;
        }
    }
    let saved = save_peer_registry(&registry, Some(path.display().to_string().as_str()))?;
    let path_display = saved.display().to_string();
    Ok(imported
        .into_iter()
        .map(|mut entry| {
            entry.path = path_display.clone();
            entry
        })
        .collect())
}

fn discovered_peer_name(candidate: &PlatformAgentRegistryA2APeerCandidate, index: usize) -> String {
    let raw = candidate
        .agent
        .id
        .clone()
        .or_else(|| candidate.agent.name.clone())
        .unwrap_or_else(|| format!("platform-a2a-peer-{}", index + 1));
    let sanitized = raw
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .chars()
        .take(80)
        .collect::<String>();
    if sanitized.is_empty() {
        format!("platform-a2a-peer-{}", index + 1)
    } else {
        sanitized
    }
}

fn unique_discovered_peer_name(
    base_name: &str,
    candidate: &PlatformAgentRegistryA2APeerCandidate,
    imported_names: &mut BTreeSet<String>,
    peers: &BTreeMap<String, PeerRegistryEntry>,
) -> Result<String> {
    for suffix in 1..=100 {
        let candidate_name = if suffix == 1 {
            base_name.to_string()
        } else {
            let suffix_text = format!("-{suffix}");
            let keep = 80usize.saturating_sub(suffix_text.len());
            format!("{}{suffix_text}", &base_name[..base_name.len().min(keep)])
        };
        let name = normalize_peer_name(&candidate_name)?;
        if imported_names.contains(&name) {
            continue;
        }
        match peers.get(&name) {
            None => {
                imported_names.insert(name.clone());
                return Ok(name);
            }
            Some(existing)
                if existing.agent_id.is_some()
                    && candidate.agent.id.is_some()
                    && existing.agent_id == candidate.agent.id =>
            {
                imported_names.insert(name.clone());
                return Ok(name);
            }
            Some(existing) if existing.url == candidate.endpoint_url => {
                imported_names.insert(name.clone());
                return Ok(name);
            }
            Some(_) => {}
        }
    }
    bail!("Could not derive a unique A2A peer name for {base_name}");
}

async fn resolve_discovered_delegate_peer(
    flags: &FlagSet,
) -> Result<(registry::ResolvedPeer, Option<Value>)> {
    let skill_id = flags
        .string("--skill")
        .context("Usage: deixic-code a2a delegate --discover --skill <skill-id> <text>")?;
    let discovery = list_a2a_peer_candidates_with_evidence(
        ListA2APeersInput {
            workspace_id: flags.string("--workspace-id"),
            capability: flags.string("--capability"),
            surface: flags.string("--surface").or_else(|| Some("a2a".into())),
            status: flags
                .string("--status")
                .or_else(|| Some(AGENT_STATUS_IDLE.into())),
            limit: flags.number("--limit").or(Some(10)),
            offset: flags.number("--offset"),
            skill_id: Some(skill_id.clone()),
            prefer_internal_endpoint: flags.boolean("--prefer-internal"),
        },
        None,
    )
    .await?
    .with_context(agent_registry_not_configured_message)?;
    if discovery.candidates.is_empty() {
        bail!("No Platform A2A peers advertise skill {skill_id}.");
    }
    let selected = select_a2a_capability_peer(
        &discovery.candidates,
        &A2ACapabilityMarketRequest {
            skill_id: Some(skill_id.clone()),
            prefer_internal_endpoint: flags.boolean("--prefer-internal"),
            ..Default::default()
        },
    )
    .with_context(|| format!("No Platform A2A peers advertise skill {skill_id}."))?;
    let imported = import_discovered_peers(
        std::slice::from_ref(&selected.candidate),
        discovery.discovery_evidence.as_ref(),
        flags.string("--registry").as_deref(),
        false,
    )?;
    let imported_peer = imported.first().context(format!(
        "Could not import Platform A2A peer for skill {skill_id}."
    ))?;
    eprintln!(
        "Selected Platform A2A peer {} ({}) for {skill_id}",
        imported_peer.name, imported_peer.url
    );
    eprintln!(
        "Capability score: {} ({})",
        selected.score,
        selected.reasons.join(", ")
    );
    let peer = resolve_peer(
        Some(&imported_peer.name),
        ResolvePeerOptions {
            registry_path: flags.string("--registry"),
            timeout_ms: flags.number("--timeout-ms"),
            token: None,
            max_attempts: None,
        },
    )?;
    let selection = json!({
        "source": "platform-agent-registry",
        "evidence": discovery.discovery_evidence,
        "candidateCount": discovery.discovery_evidence.as_ref().and_then(|e| e.candidate_count).unwrap_or(discovery.candidates.len() as f64),
        "matchedCount": discovery.discovery_evidence.as_ref().and_then(|e| e.matched_count).unwrap_or(discovery.candidates.len() as f64),
        "selectedAgentId": selected.candidate.agent.id,
        "selectedAgentName": selected.candidate.agent.name,
        "selectedEndpointUrl": selected.candidate.endpoint_url,
        "selectedEndpointKind": selected.candidate.endpoint_kind,
        "score": selected.score,
        "reasons": selected.reasons,
    });
    Ok((peer, Some(selection)))
}

fn select_peer_skill(skills: Option<&Value>, skill_id: Option<&str>) -> Option<Value> {
    let skill_id = skill_id?;
    let items = skills?.as_array()?;
    items
        .iter()
        .find(|skill| {
            skill
                .get("id")
                .and_then(|value| value.as_str())
                .is_some_and(|id| id == skill_id)
        })
        .cloned()
}

/// Minimal flag/positional parser for a2a CLI subcommands.
struct FlagSet {
    positionals: Vec<String>,
    flags: BTreeMap<String, String>,
    booleans: BTreeSet<String>,
}

impl FlagSet {
    fn parse(args: &[String]) -> Self {
        let mut positionals = Vec::new();
        let mut flags = BTreeMap::new();
        let mut booleans = BTreeSet::new();
        let mut index = 0;
        while index < args.len() {
            let arg = &args[index];
            if arg == "--" {
                positionals.extend(args[index + 1..].iter().cloned());
                break;
            }
            if let Some(rest) = arg.strip_prefix("--") {
                if let Some((key, value)) = rest.split_once('=') {
                    flags.insert(format!("--{key}"), value.to_string());
                    index += 1;
                    continue;
                }
                let flag = format!("--{rest}");
                let next = args.get(index + 1);
                let next_is_value = next.is_some_and(|n| !n.starts_with('-'));
                let known_boolean = matches!(
                    flag.as_str(),
                    "--default"
                        | "--json"
                        | "--wait"
                        | "--refresh"
                        | "--work-graph"
                        | "--import"
                        | "--prefer-internal"
                        | "--platform"
                        | "--discover"
                        | "--heartbeat-only"
                        | "--update-only"
                        | "--no-heartbeat"
                        | "--help"
                        | "-h"
                );
                if known_boolean || !next_is_value {
                    booleans.insert(flag);
                    index += 1;
                    continue;
                }
                if let Some(value) = next {
                    flags.insert(flag, value.clone());
                    index += 2;
                    continue;
                }
                booleans.insert(flag);
                index += 1;
            } else {
                positionals.push(arg.clone());
                index += 1;
            }
        }
        Self {
            positionals,
            flags,
            booleans,
        }
    }

    fn string(&self, name: &str) -> Option<String> {
        self.flags
            .get(name)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    fn string_list(&self, name: &str, fallback: &[String]) -> Vec<String> {
        match self.string(name) {
            None => fallback.to_vec(),
            Some(value) => {
                let parsed: Vec<String> = value
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect();
                if parsed.is_empty() {
                    fallback.to_vec()
                } else {
                    parsed
                }
            }
        }
    }

    fn boolean(&self, name: &str) -> bool {
        self.booleans.contains(name)
    }

    fn number(&self, name: &str) -> Option<u64> {
        self.string(name)?.parse().ok()
    }

    fn minutes_ms(&self, name: &str) -> Option<u64> {
        let minutes = self.string(name)?.parse::<f64>().ok()?;
        if minutes <= 0.0 {
            return None;
        }
        Some((minutes * 60_000.0) as u64)
    }

    fn first_positional(&self) -> Option<&str> {
        self.positionals.first().map(String::as_str)
    }

    fn positional(&self, index: usize) -> Option<&str> {
        self.positionals.get(index).map(String::as_str)
    }

    fn remaining_positionals_from(&self, index: usize) -> &[String] {
        if index >= self.positionals.len() {
            &[]
        } else {
            &self.positionals[index..]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pairing::{decode_pairing_code, encode_pairing_code};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn pairing_code_round_trips() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_secs();
        let issued = chrono::DateTime::from_timestamp(now as i64, 0)
            .expect("issued")
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let expires = chrono::DateTime::from_timestamp((now + 1800) as i64, 0)
            .expect("expires")
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let payload = pairing::PairingPayload {
            version: 1,
            display_name: "Mac mini Maestro".into(),
            agent_card_url: "http://127.0.0.1:18787/.well-known/agent-card.json".into(),
            transport_url: "http://127.0.0.1:18787".into(),
            protocol_binding: "HTTP+JSON".into(),
            protocol_version: "1.0".into(),
            issued_at: issued,
            expires_at: expires,
            peer_id: Some("mac-mini".into()),
            provider: None,
            capabilities: None,
            skills: None,
            key_fingerprint: None,
            relay_hints: None,
            metadata: None,
        };
        let code = encode_pairing_code(&payload).expect("encode");
        assert!(code.starts_with("maestro-pair-v1."));
        let decoded = decode_pairing_code(&code, true).expect("decode");
        assert_eq!(decoded.display_name, "Mac mini Maestro");
        assert_eq!(decoded.peer_id.as_deref(), Some("mac-mini"));
    }

    #[test]
    fn flag_set_parses_send_args() {
        let args = vec![
            "peer-a".into(),
            "hello".into(),
            "world".into(),
            "--wait".into(),
            "--tasks".into(),
            "/tmp/tasks.json".into(),
        ];
        let flags = FlagSet::parse(&args);
        assert_eq!(flags.first_positional(), Some("peer-a"));
        assert_eq!(flags.remaining_positionals_from(1).join(" "), "hello world");
        assert!(flags.boolean("--wait"));
        assert_eq!(flags.string("--tasks").as_deref(), Some("/tmp/tasks.json"));
    }

    #[test]
    fn configured_peer_session_is_the_outgoing_context() {
        let mut config = A2AServiceConfig {
            base_url: "https://peer.example.com".into(),
            token: None,
            organization_id: None,
            workspace_id: None,
            agent_id: None,
            session_id: Some(" chief-session ".into()),
            actor_id: None,
            timeout_ms: 1_000,
            max_attempts: 1,
        };

        assert_eq!(peer_context_id(&config), "chief-session");
        assert_eq!(peer_context_id(&config), "chief-session");

        config.session_id = None;
        assert_ne!(peer_context_id(&config), peer_context_id(&config));
    }
}
