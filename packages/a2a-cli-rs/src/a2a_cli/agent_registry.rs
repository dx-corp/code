//! Platform Agent Registry Connect client (`agents.v1.AgentService/*`).
//!
//! Ports `src/platform/agent-registry-client.ts` for the native a2a CLI.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::operating_plane_client::normalize_base_url;

const DEFAULT_TIMEOUT_MS: u64 = 2_500;
const DEFAULT_MAX_ATTEMPTS: usize = 2;
const CONNECT_PROTOCOL_VERSION: &str = "1";

const HEARTBEAT_PATH: &str = "/agents.v1.AgentService/Heartbeat";
const REGISTER_PATH: &str = "/agents.v1.AgentService/Register";
const DELEGATE_PATH: &str = "/agents.v1.AgentService/Delegate";
const LIST_AGENTS_PATH: &str = "/agents.v1.AgentService/List";
const RESOLVE_DELEGATION_PATH: &str = "/agents.v1.AgentService/ResolveDelegation";
const UPDATE_PATH: &str = "/agents.v1.AgentService/Update";
const CONTROL_A2A_DELEGATION_TASK_PATH: &str = "/agents.v1.AgentService/ControlA2ADelegationTask";
const GET_A2A_DELEGATION_GRAPH_PATH: &str = "/agents.v1.AgentService/GetA2ADelegationGraph";

const BASE_URL_ENV_VARS: &[&str] = &[
    "MAESTRO_AGENT_REGISTRY_SERVICE_URL",
    "AGENT_REGISTRY_SERVICE_URL",
    "MAESTRO_AGENT_REGISTRY_URL",
    "AGENT_REGISTRY_BASE_URL",
    "PLATFORM_AGENT_REGISTRY_URL",
    "MAESTRO_PLATFORM_BASE_URL",
    "MAESTRO_EVALOPS_BASE_URL",
    "EVALOPS_BASE_URL",
];

const TOKEN_ENV_VARS: &[&str] = &[
    "MAESTRO_AGENT_REGISTRY_SERVICE_TOKEN",
    "AGENT_REGISTRY_SERVICE_TOKEN",
    "MAESTRO_AGENT_REGISTRY_TOKEN",
    "AGENT_REGISTRY_TOKEN",
    "MAESTRO_EVALOPS_ACCESS_TOKEN",
    "EVALOPS_TOKEN",
];

const ORGANIZATION_ENV_VARS: &[&str] = &[
    "MAESTRO_AGENT_REGISTRY_ORG_ID",
    "AGENT_REGISTRY_ORGANIZATION_ID",
    "AGENT_REGISTRY_ORG_ID",
    "MAESTRO_EVALOPS_ORG_ID",
    "EVALOPS_ORGANIZATION_ID",
    "EVALOPS_ORG_ID",
    "MAESTRO_ENTERPRISE_ORG_ID",
];

const WORKSPACE_ENV_VARS: &[&str] = &[
    "MAESTRO_AGENT_REGISTRY_WORKSPACE_ID",
    "AGENT_REGISTRY_WORKSPACE_ID",
    "MAESTRO_REMOTE_RUNNER_WORKSPACE_ID",
    "MAESTRO_WORKSPACE_ID",
    "MAESTRO_EVALOPS_WORKSPACE_ID",
    "EVALOPS_WORKSPACE_ID",
];

const TIMEOUT_ENV_VARS: &[&str] = &[
    "MAESTRO_AGENT_REGISTRY_TIMEOUT_MS",
    "AGENT_REGISTRY_SERVICE_TIMEOUT_MS",
];

const MAX_ATTEMPTS_ENV_VARS: &[&str] = &[
    "MAESTRO_AGENT_REGISTRY_MAX_ATTEMPTS",
    "AGENT_REGISTRY_SERVICE_MAX_ATTEMPTS",
];

const BASE_URL_SUFFIXES: &[&str] = &[
    HEARTBEAT_PATH,
    REGISTER_PATH,
    DELEGATE_PATH,
    LIST_AGENTS_PATH,
    RESOLVE_DELEGATION_PATH,
    UPDATE_PATH,
    CONTROL_A2A_DELEGATION_TASK_PATH,
    GET_A2A_DELEGATION_GRAPH_PATH,
    "/agents.v1.AgentService",
];

#[allow(dead_code)]
pub const AGENT_STATUS_ACTIVE: &str = "AGENT_STATUS_ACTIVE";
pub const AGENT_STATUS_IDLE: &str = "AGENT_STATUS_IDLE";

pub const CONTROL_MODE_STEER: &str = "A2A_DELEGATION_TASK_CONTROL_MODE_STEER";
pub const CONTROL_MODE_FOLLOWUP: &str = "A2A_DELEGATION_TASK_CONTROL_MODE_FOLLOWUP";
pub const CONTROL_MODE_COLLECT: &str = "A2A_DELEGATION_TASK_CONTROL_MODE_COLLECT";
pub const CONTROL_MODE_INTERRUPT: &str = "A2A_DELEGATION_TASK_CONTROL_MODE_INTERRUPT";
pub const CONTROL_MODE_CANCEL: &str = "A2A_DELEGATION_TASK_CONTROL_MODE_CANCEL";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRegistryConfig {
    pub base_url: String,
    pub token: String,
    pub organization_id: String,
    pub workspace_id: String,
    pub timeout_ms: u64,
    pub max_attempts: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlatformAgentA2ASkill {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_modes: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_modes: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_context_grants: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_policy_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_autonomy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_artifact_kinds: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub optional_artifact_kinds: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_task_classes: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denied_task_classes: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attributes: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlatformAgentA2APeerProjection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_endpoint_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub internal_endpoint_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_card_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_binding: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supported_extensions: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<PlatformAgentA2ASkill>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security_schemes: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_card_observed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_card_etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_card_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_notifications: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attributes: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlatformAgentCapacity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserved_delegation_count: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlatformAgentRegistryAgent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surfaces: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface_types: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_config_version: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_heartbeat_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a2a: Option<PlatformAgentA2APeerProjection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<PlatformAgentCapacity>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlatformAgentDiscoveryExclusion {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_agent_ids: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlatformAgentDiscoveryEvidence {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a2a_skill_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_a2a_dispatch: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eligible_for_delegation: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_count: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_count: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclusions: Option<Vec<PlatformAgentDiscoveryExclusion>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlatformAgentRegistryA2APeerCandidate {
    pub agent: PlatformAgentRegistryAgent,
    pub endpoint_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_card_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_binding: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<String>,
    #[serde(default)]
    pub skills: Vec<PlatformAgentA2ASkill>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supported_extensions: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_notifications: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlatformA2APeerCandidatesResult {
    pub candidates: Vec<PlatformAgentRegistryA2APeerCandidate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery_evidence: Option<PlatformAgentDiscoveryEvidence>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlatformDelegationRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_capability: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a2a_task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a2a_message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a2a_endpoint_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a2a_dispatch_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a2a_dispatch_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a2a_skill_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a2a_root_delegation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a2a_parent_delegation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a2a_delegation_chain: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a2a_resume_wait_contracts: Option<Vec<Value>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlatformA2ADelegationTaskControlResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancelled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_for_worker: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlatformControlA2ADelegationTaskResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<PlatformDelegationRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_task: Option<PlatformA2ADelegationTaskControlResult>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlatformA2ADelegationGraphNode {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<PlatformDelegationRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_count: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlatformA2ADelegationGraphEdge {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_delegation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_delegation_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlatformA2ADelegationGraphResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_delegation_id: Option<String>,
    #[serde(default)]
    pub nodes: Vec<PlatformA2ADelegationGraphNode>,
    #[serde(default)]
    pub edges: Vec<PlatformA2ADelegationGraphEdge>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing_parent_delegation_ids: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default)]
pub struct ListAgentsInput {
    pub workspace_id: Option<String>,
    pub agent_type: Option<String>,
    pub capability: Option<String>,
    pub surface: Option<String>,
    pub status: Option<String>,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
    pub a2a_skill_id: Option<String>,
    pub task_class: Option<String>,
    pub require_a2a_dispatch: Option<bool>,
    pub eligible_for_delegation: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct ListA2APeersInput {
    pub workspace_id: Option<String>,
    pub capability: Option<String>,
    pub surface: Option<String>,
    pub status: Option<String>,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
    pub skill_id: Option<String>,
    pub prefer_internal_endpoint: bool,
}

#[derive(Debug, Clone)]
pub struct RegisterAgentInput {
    pub workspace_id: Option<String>,
    pub id: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub agent_type: String,
    pub capabilities: Vec<String>,
    pub surfaces: Option<Vec<String>>,
    pub surface_types: Option<Vec<String>>,
    pub owner_id: Option<String>,
    pub a2a: Option<PlatformAgentA2APeerProjection>,
}

#[derive(Debug, Clone)]
pub struct UpdateAgentInput {
    pub workspace_id: Option<String>,
    pub id: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub capabilities: Option<Vec<String>>,
    pub surfaces: Option<Vec<String>>,
    pub surface_types: Option<Vec<String>>,
    pub a2a: Option<PlatformAgentA2APeerProjection>,
}

#[derive(Debug, Clone)]
pub struct HeartbeatAgentInput {
    pub workspace_id: Option<String>,
    pub agent_id: String,
    pub status: Option<String>,
    pub surface: Option<String>,
    pub surface_type: Option<String>,
    pub a2a: Option<PlatformAgentA2APeerProjection>,
}

#[derive(Debug, Clone)]
pub struct DelegateAgentInput {
    pub workspace_id: Option<String>,
    pub from_agent_id: String,
    pub to_agent_id: Option<String>,
    pub required_capability: Option<String>,
    pub a2a_skill_id: Option<String>,
    pub objective_id: Option<String>,
    pub workflow_run_id: Option<String>,
    pub workflow_step_id: Option<String>,
    pub context_payload: Option<Value>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ControlA2ADelegationTaskInput {
    pub workspace_id: Option<String>,
    pub delegation_id: String,
    pub mode: String,
    pub message: Option<String>,
    pub idempotency_key: Option<String>,
    pub target_run_id: Option<String>,
    pub child_run_id: Option<String>,
    pub subagent_lane_id: Option<String>,
    pub work_item_id: Option<String>,
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Default)]
pub struct GetA2ADelegationGraphInput {
    pub workspace_id: Option<String>,
    pub root_delegation_id: Option<String>,
    pub delegation_id: Option<String>,
    pub max_depth: Option<u64>,
    pub limit: Option<u64>,
}

pub fn agent_registry_not_configured_message() -> &'static str {
    "Agent Registry service is not configured. Set AGENT_REGISTRY_SERVICE_URL, \
     AGENT_REGISTRY_SERVICE_TOKEN, AGENT_REGISTRY_ORGANIZATION_ID, and \
     AGENT_REGISTRY_WORKSPACE_ID."
}

#[allow(dead_code)]
pub fn resolve_agent_registry_config() -> Option<AgentRegistryConfig> {
    resolve_agent_registry_config_with_workspace(None)
}

pub fn resolve_agent_registry_config_with_workspace(
    workspace_id: Option<&str>,
) -> Option<AgentRegistryConfig> {
    let base_url = get_env_value(BASE_URL_ENV_VARS)?;
    let token = get_env_value(TOKEN_ENV_VARS)?;
    let organization_id = get_env_value(ORGANIZATION_ENV_VARS)?;
    let workspace_id = resolve_workspace_scope(workspace_id, get_env_value(WORKSPACE_ENV_VARS))?;
    Some(AgentRegistryConfig {
        base_url: normalize_base_url(&base_url, BASE_URL_SUFFIXES),
        token,
        organization_id,
        workspace_id,
        timeout_ms: parse_positive_int(
            get_env_value(TIMEOUT_ENV_VARS).as_deref(),
            DEFAULT_TIMEOUT_MS,
        ),
        max_attempts: parse_positive_int(
            get_env_value(MAX_ATTEMPTS_ENV_VARS).as_deref(),
            DEFAULT_MAX_ATTEMPTS as u64,
        ) as usize,
    })
}

pub fn is_agent_already_exists_error(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}");
    message.contains("409")
        || message.to_ascii_lowercase().contains("already exists")
        || message.to_ascii_lowercase().contains("already_exists")
}

pub fn normalize_a2a_control_mode(value: &str) -> Result<String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "steer" | "a2a_delegation_task_control_mode_steer" => Ok(CONTROL_MODE_STEER.into()),
        "followup" | "follow-up" | "a2a_delegation_task_control_mode_followup" => {
            Ok(CONTROL_MODE_FOLLOWUP.into())
        }
        "collect" | "a2a_delegation_task_control_mode_collect" => Ok(CONTROL_MODE_COLLECT.into()),
        "interrupt" | "a2a_delegation_task_control_mode_interrupt" => {
            Ok(CONTROL_MODE_INTERRUPT.into())
        }
        "cancel" | "a2a_delegation_task_control_mode_cancel" => Ok(CONTROL_MODE_CANCEL.into()),
        other => bail!("Unsupported A2A control mode: {other}"),
    }
}

pub async fn register_agent_with_platform(
    input: RegisterAgentInput,
    config: Option<AgentRegistryConfig>,
) -> Result<Option<PlatformAgentRegistryAgent>> {
    let config = match resolve_config(config, input.workspace_id.as_deref()) {
        Some(config) => config,
        None => return Ok(None),
    };
    let mut body = Map::new();
    insert_string(
        &mut body,
        "workspaceId",
        trim_opt(input.workspace_id.as_deref()).or(Some(config.workspace_id.as_str())),
    );
    insert_string(&mut body, "id", input.id.as_deref());
    body.insert("name".into(), Value::String(input.name));
    insert_string(&mut body, "description", input.description.as_deref());
    body.insert("agentType".into(), Value::String(input.agent_type));
    body.insert(
        "capabilities".into(),
        Value::Array(input.capabilities.into_iter().map(Value::String).collect()),
    );
    insert_string_list(&mut body, "surfaces", input.surfaces.as_ref());
    insert_string_list(&mut body, "surfaceTypes", input.surface_types.as_ref());
    insert_string(&mut body, "ownerId", input.owner_id.as_deref());
    if let Some(a2a) = input.a2a {
        body.insert("a2a".into(), encode_a2a_peer_projection(&a2a));
    }
    let payload = post_agent_registry(&config, REGISTER_PATH, Value::Object(body)).await?;
    Ok(Some(normalize_agent(object_value(&payload, &["agent"]))))
}

pub async fn update_agent_with_platform(
    input: UpdateAgentInput,
    config: Option<AgentRegistryConfig>,
) -> Result<Option<PlatformAgentRegistryAgent>> {
    let config = match resolve_config(config, input.workspace_id.as_deref()) {
        Some(config) => config,
        None => return Ok(None),
    };
    let mut body = Map::new();
    body.insert("id".into(), Value::String(input.id));
    insert_string(&mut body, "name", input.name.as_deref());
    insert_string(&mut body, "description", input.description.as_deref());
    insert_string_list(&mut body, "capabilities", input.capabilities.as_ref());
    insert_string_list(&mut body, "surfaces", input.surfaces.as_ref());
    insert_string_list(&mut body, "surfaceTypes", input.surface_types.as_ref());
    if let Some(a2a) = input.a2a {
        body.insert("a2a".into(), encode_a2a_peer_projection(&a2a));
    }
    let payload = post_agent_registry(&config, UPDATE_PATH, Value::Object(body)).await?;
    Ok(Some(normalize_agent(object_value(&payload, &["agent"]))))
}

pub async fn heartbeat_agent_with_platform(
    input: HeartbeatAgentInput,
    config: Option<AgentRegistryConfig>,
) -> Result<Option<String>> {
    let config = match resolve_config(config, input.workspace_id.as_deref()) {
        Some(config) => config,
        None => return Ok(None),
    };
    let mut body = Map::new();
    body.insert("agentId".into(), Value::String(input.agent_id));
    insert_string(&mut body, "status", input.status.as_deref());
    insert_string(&mut body, "surface", input.surface.as_deref());
    insert_string(&mut body, "surfaceType", input.surface_type.as_deref());
    if let Some(a2a) = input.a2a {
        body.insert("a2a".into(), encode_a2a_peer_projection(&a2a));
    }
    let payload = post_agent_registry(&config, HEARTBEAT_PATH, Value::Object(body)).await?;
    Ok(first_string(
        &payload,
        &["nextHeartbeatBy", "next_heartbeat_by"],
    ))
}

pub async fn list_agents_with_platform(
    input: ListAgentsInput,
    config: Option<AgentRegistryConfig>,
) -> Result<
    Option<(
        Vec<PlatformAgentRegistryAgent>,
        Option<PlatformAgentDiscoveryEvidence>,
    )>,
> {
    let config = match resolve_config(config, input.workspace_id.as_deref()) {
        Some(config) => config,
        None => return Ok(None),
    };
    let mut body = Map::new();
    insert_string(
        &mut body,
        "workspaceId",
        trim_opt(input.workspace_id.as_deref()).or(Some(config.workspace_id.as_str())),
    );
    insert_string(&mut body, "agentType", input.agent_type.as_deref());
    insert_string(&mut body, "capability", input.capability.as_deref());
    insert_string(&mut body, "surface", input.surface.as_deref());
    insert_string(&mut body, "status", input.status.as_deref());
    if let Some(limit) = input.limit {
        body.insert("limit".into(), json!(limit));
    }
    if let Some(offset) = input.offset {
        body.insert("offset".into(), json!(offset));
    }
    insert_string(&mut body, "a2aSkillId", input.a2a_skill_id.as_deref());
    insert_string(&mut body, "taskClass", input.task_class.as_deref());
    if let Some(require) = input.require_a2a_dispatch {
        body.insert("requireA2aDispatch".into(), Value::Bool(require));
    }
    if let Some(eligible) = input.eligible_for_delegation {
        body.insert("eligibleForDelegation".into(), Value::Bool(eligible));
    }
    let payload = post_agent_registry(&config, LIST_AGENTS_PATH, Value::Object(body)).await?;
    let agents = payload
        .get("agents")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_object().map(|o| normalize_agent(Some(o))))
                .filter(|agent| agent.id.is_some() || agent.name.is_some() || agent.a2a.is_some())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let evidence = object_value(&payload, &["discoveryEvidence", "discovery_evidence"])
        .map(normalize_discovery_evidence);
    Ok(Some((agents, evidence)))
}

pub async fn list_a2a_peer_candidates_with_evidence(
    input: ListA2APeersInput,
    config: Option<AgentRegistryConfig>,
) -> Result<Option<PlatformA2APeerCandidatesResult>> {
    let listed = list_agents_with_platform(
        ListAgentsInput {
            workspace_id: input.workspace_id.clone(),
            capability: input.capability.clone(),
            surface: input.surface.clone(),
            status: input.status.clone(),
            limit: input.limit,
            offset: input.offset,
            a2a_skill_id: input.skill_id.clone(),
            require_a2a_dispatch: Some(true),
            eligible_for_delegation: Some(true),
            ..Default::default()
        },
        config,
    )
    .await?;
    let Some((agents, discovery_evidence)) = listed else {
        return Ok(None);
    };
    let candidates = agents
        .into_iter()
        .filter_map(|agent| {
            let a2a = agent.a2a.clone()?;
            let use_internal =
                input.prefer_internal_endpoint && a2a.internal_endpoint_url.is_some();
            let endpoint_url = if use_internal {
                a2a.internal_endpoint_url.clone()
            } else {
                a2a.public_endpoint_url
                    .clone()
                    .or_else(|| a2a.internal_endpoint_url.clone())
            }?;
            let endpoint_kind = if use_internal
                || a2a
                    .internal_endpoint_url
                    .as_ref()
                    .is_some_and(|url| url == &endpoint_url)
            {
                "internal"
            } else {
                "public"
            };
            let skills = a2a.skills.clone().unwrap_or_default();
            if let Some(skill_id) = &input.skill_id {
                if !skills.iter().any(|skill| skill.id == *skill_id) {
                    return None;
                }
            }
            Some(PlatformAgentRegistryA2APeerCandidate {
                agent,
                endpoint_url,
                endpoint_kind: Some(endpoint_kind.into()),
                agent_card_url: a2a.agent_card_url.clone(),
                protocol_binding: a2a.protocol_binding.clone(),
                protocol_version: a2a.protocol_version.clone(),
                skills,
                supported_extensions: a2a.supported_extensions.clone(),
                push_notifications: a2a.push_notifications,
            })
        })
        .collect();
    Ok(Some(PlatformA2APeerCandidatesResult {
        candidates,
        discovery_evidence,
    }))
}

pub async fn delegate_agent_with_platform(
    input: DelegateAgentInput,
    config: Option<AgentRegistryConfig>,
) -> Result<Option<PlatformDelegationRecord>> {
    let config = match resolve_config(config, input.workspace_id.as_deref()) {
        Some(config) => config,
        None => return Ok(None),
    };
    let mut body = Map::new();
    insert_string(
        &mut body,
        "workspaceId",
        trim_opt(input.workspace_id.as_deref()).or(Some(config.workspace_id.as_str())),
    );
    body.insert("fromAgentId".into(), Value::String(input.from_agent_id));
    insert_string(&mut body, "toAgentId", input.to_agent_id.as_deref());
    insert_string(
        &mut body,
        "requiredCapability",
        input.required_capability.as_deref(),
    );
    insert_string(&mut body, "a2aSkillId", input.a2a_skill_id.as_deref());
    insert_string(&mut body, "objectiveId", input.objective_id.as_deref());
    insert_string(&mut body, "workflowRunId", input.workflow_run_id.as_deref());
    insert_string(
        &mut body,
        "workflowStepId",
        input.workflow_step_id.as_deref(),
    );
    if let Some(payload) = input.context_payload {
        body.insert(
            "contextPayload".into(),
            Value::String(BASE64.encode(payload.to_string().as_bytes())),
        );
    }
    insert_string(&mut body, "reason", input.reason.as_deref());
    let response = post_agent_registry(&config, DELEGATE_PATH, Value::Object(body)).await?;
    Ok(Some(normalize_delegation(object_value(
        &response,
        &["delegation"],
    ))))
}

pub async fn control_a2a_delegation_task_with_platform(
    input: ControlA2ADelegationTaskInput,
    config: Option<AgentRegistryConfig>,
) -> Result<Option<PlatformControlA2ADelegationTaskResult>> {
    let config = match resolve_config(config, input.workspace_id.as_deref()) {
        Some(config) => config,
        None => return Ok(None),
    };
    let mut body = Map::new();
    body.insert("delegationId".into(), Value::String(input.delegation_id));
    body.insert("mode".into(), Value::String(input.mode));
    insert_string(&mut body, "message", input.message.as_deref());
    insert_string(
        &mut body,
        "idempotencyKey",
        input.idempotency_key.as_deref(),
    );
    insert_string(&mut body, "targetRunId", input.target_run_id.as_deref());
    insert_string(&mut body, "childRunId", input.child_run_id.as_deref());
    insert_string(
        &mut body,
        "subagentLaneId",
        input.subagent_lane_id.as_deref(),
    );
    insert_string(&mut body, "workItemId", input.work_item_id.as_deref());
    if let Some(metadata) = input.metadata {
        body.insert("metadata".into(), metadata);
    }
    let response = post_agent_registry(
        &config,
        CONTROL_A2A_DELEGATION_TASK_PATH,
        Value::Object(body),
    )
    .await?;
    Ok(Some(PlatformControlA2ADelegationTaskResult {
        delegation: Some(normalize_delegation(object_value(
            &response,
            &["delegation"],
        )))
        .filter(|d| d.id.is_some() || d.status.is_some()),
        remote_task: object_value(&response, &["remoteTask", "remote_task"])
            .map(normalize_remote_task),
    }))
}

pub async fn get_a2a_delegation_graph_with_platform(
    input: GetA2ADelegationGraphInput,
    config: Option<AgentRegistryConfig>,
) -> Result<Option<PlatformA2ADelegationGraphResult>> {
    let config = match resolve_config(config, input.workspace_id.as_deref()) {
        Some(config) => config,
        None => return Ok(None),
    };
    let mut body = Map::new();
    insert_string(
        &mut body,
        "workspaceId",
        trim_opt(input.workspace_id.as_deref()).or(Some(config.workspace_id.as_str())),
    );
    insert_string(
        &mut body,
        "rootDelegationId",
        input.root_delegation_id.as_deref(),
    );
    insert_string(&mut body, "delegationId", input.delegation_id.as_deref());
    if let Some(max_depth) = input.max_depth {
        body.insert("maxDepth".into(), json!(max_depth));
    }
    if let Some(limit) = input.limit {
        body.insert("limit".into(), json!(limit));
    }
    let response =
        post_agent_registry(&config, GET_A2A_DELEGATION_GRAPH_PATH, Value::Object(body)).await?;
    let nodes = response
        .get("nodes")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_object().map(normalize_graph_node))
                .collect()
        })
        .unwrap_or_default();
    let edges = response
        .get("edges")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_object().map(normalize_graph_edge))
                .collect()
        })
        .unwrap_or_default();
    Ok(Some(PlatformA2ADelegationGraphResult {
        root_delegation_id: first_string(&response, &["rootDelegationId", "root_delegation_id"]),
        nodes,
        edges,
        total: first_number(&response, &["total"]),
        truncated: first_bool(&response, &["truncated"]),
        missing_parent_delegation_ids: string_list(
            &response,
            &[
                "missingParentDelegationIds",
                "missing_parent_delegation_ids",
            ],
        ),
    }))
}

fn resolve_config(
    config: Option<AgentRegistryConfig>,
    workspace_id: Option<&str>,
) -> Option<AgentRegistryConfig> {
    match config {
        Some(mut config) => {
            let workspace = resolve_workspace_scope(
                workspace_id,
                trim_opt(Some(config.workspace_id.as_str())).map(str::to_string),
            )?;
            config.workspace_id = workspace;
            Some(config)
        }
        None => resolve_agent_registry_config_with_workspace(workspace_id),
    }
}

fn resolve_workspace_scope(
    requested_workspace_id: Option<&str>,
    configured_workspace_id: Option<String>,
) -> Option<String> {
    trim_opt(requested_workspace_id)
        .map(str::to_string)
        .or(configured_workspace_id)
}

async fn post_agent_registry(
    config: &AgentRegistryConfig,
    path: &str,
    body: Value,
) -> Result<Value> {
    let client = Client::builder()
        .timeout(Duration::from_millis(config.timeout_ms.max(1)))
        .build()
        .context("failed to create agent registry HTTP client")?;
    let url = format!("{}{}", config.base_url.trim_end_matches('/'), path);
    let max_attempts = config.max_attempts.max(1);
    let mut last_error = None;
    for attempt in 0..max_attempts {
        let response = client
            .post(&url)
            .bearer_auth(&config.token)
            .header("Content-Type", "application/json")
            .header("Connect-Protocol-Version", CONNECT_PROTOCOL_VERSION)
            .header("X-Organization-ID", &config.organization_id)
            .header("X-Workspace-ID", &config.workspace_id)
            .json(&body)
            .send()
            .await;
        match response {
            Ok(response) => {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                if retryable_status(status) && attempt + 1 < max_attempts {
                    last_error = Some(anyhow::anyhow!(
                        "agent registry service returned {}: {}",
                        status.as_u16(),
                        text
                    ));
                    tokio::time::sleep(Duration::from_millis(100 * (1 << attempt) as u64)).await;
                    continue;
                }
                if !status.is_success() {
                    bail!(
                        "agent registry service returned {}: {}",
                        status.as_u16(),
                        if text.trim().is_empty() {
                            status.canonical_reason().unwrap_or("error")
                        } else {
                            text.trim()
                        }
                    );
                }
                if text.trim().is_empty() {
                    return Ok(json!({}));
                }
                return serde_json::from_str(&text)
                    .context("agent registry service returned invalid JSON");
            }
            Err(error) if attempt + 1 < max_attempts => {
                last_error = Some(error.into());
                tokio::time::sleep(Duration::from_millis(100 * (1 << attempt) as u64)).await;
            }
            Err(error) => {
                return Err(error).context("agent registry service request failed");
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("agent registry service request failed")))
}

fn retryable_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 429) || status.is_server_error()
}

fn encode_a2a_peer_projection(a2a: &PlatformAgentA2APeerProjection) -> Value {
    let mut map = Map::new();
    insert_string(
        &mut map,
        "publicEndpointUrl",
        a2a.public_endpoint_url.as_deref(),
    );
    insert_string(
        &mut map,
        "internalEndpointUrl",
        a2a.internal_endpoint_url.as_deref(),
    );
    insert_string(&mut map, "agentCardUrl", a2a.agent_card_url.as_deref());
    insert_string(&mut map, "protocolBinding", a2a.protocol_binding.as_deref());
    insert_string(&mut map, "protocolVersion", a2a.protocol_version.as_deref());
    insert_string_list(
        &mut map,
        "supportedExtensions",
        a2a.supported_extensions.as_ref(),
    );
    if let Some(skills) = &a2a.skills {
        map.insert(
            "skills".into(),
            Value::Array(skills.iter().map(encode_skill).collect()),
        );
    }
    insert_string_list(&mut map, "securitySchemes", a2a.security_schemes.as_ref());
    insert_string(
        &mut map,
        "agentCardObservedAt",
        a2a.agent_card_observed_at.as_deref(),
    );
    insert_string(&mut map, "agentCardEtag", a2a.agent_card_etag.as_deref());
    insert_string(&mut map, "agentCardHash", a2a.agent_card_hash.as_deref());
    if let Some(push) = a2a.push_notifications {
        map.insert("pushNotifications".into(), Value::Bool(push));
    }
    if let Some(attributes) = &a2a.attributes {
        map.insert("attributes".into(), Value::Object(attributes.clone()));
    }
    Value::Object(map)
}

fn encode_skill(skill: &PlatformAgentA2ASkill) -> Value {
    let mut map = Map::new();
    map.insert("id".into(), Value::String(skill.id.clone()));
    insert_string(&mut map, "name", skill.name.as_deref());
    insert_string(&mut map, "description", skill.description.as_deref());
    insert_string_list(&mut map, "tags", skill.tags.as_ref());
    insert_string_list(&mut map, "inputModes", skill.input_modes.as_ref());
    insert_string_list(&mut map, "outputModes", skill.output_modes.as_ref());
    insert_string_list(
        &mut map,
        "requiredContextGrants",
        skill.required_context_grants.as_ref(),
    );
    insert_string(
        &mut map,
        "approvalPolicyRef",
        skill.approval_policy_ref.as_deref(),
    );
    insert_string(&mut map, "maxAutonomy", skill.max_autonomy.as_deref());
    insert_string_list(
        &mut map,
        "requiredArtifactKinds",
        skill.required_artifact_kinds.as_ref(),
    );
    insert_string_list(
        &mut map,
        "optionalArtifactKinds",
        skill.optional_artifact_kinds.as_ref(),
    );
    insert_string_list(
        &mut map,
        "allowedTaskClasses",
        skill.allowed_task_classes.as_ref(),
    );
    insert_string_list(
        &mut map,
        "deniedTaskClasses",
        skill.denied_task_classes.as_ref(),
    );
    if let Some(attributes) = &skill.attributes {
        map.insert("attributes".into(), Value::Object(attributes.clone()));
    }
    if let Some(metadata) = &skill.metadata {
        map.insert("metadata".into(), Value::Object(metadata.clone()));
    }
    Value::Object(map)
}

fn normalize_agent(record: Option<&Map<String, Value>>) -> PlatformAgentRegistryAgent {
    let Some(record) = record else {
        return PlatformAgentRegistryAgent::default();
    };
    PlatformAgentRegistryAgent {
        id: first_string_map(record, &["id"]),
        workspace_id: first_string_map(record, &["workspaceId", "workspace_id"]),
        name: first_string_map(record, &["name"]),
        description: first_string_map(record, &["description"]),
        agent_type: first_string_map(record, &["agentType", "agent_type"]),
        capabilities: string_list_map(record, &["capabilities"]),
        surfaces: string_list_map(record, &["surfaces"]),
        surface_types: string_list_map(record, &["surfaceTypes", "surface_types"]),
        status: first_string_map(record, &["status"]),
        active_config_version: first_number_map(
            record,
            &["activeConfigVersion", "active_config_version"],
        ),
        owner_id: first_string_map(record, &["ownerId", "owner_id"]),
        last_heartbeat_at: first_string_map(record, &["lastHeartbeatAt", "last_heartbeat_at"]),
        created_at: first_string_map(record, &["createdAt", "created_at"]),
        updated_at: first_string_map(record, &["updatedAt", "updated_at"]),
        a2a: object_value_map(record, &["a2a"]).map(normalize_a2a_projection),
        capacity: object_value_map(record, &["capacity"]).map(normalize_capacity),
    }
}

fn normalize_a2a_projection(record: &Map<String, Value>) -> PlatformAgentA2APeerProjection {
    let skills = record
        .get("skills")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_object().and_then(normalize_skill))
                .collect::<Vec<_>>()
        })
        .filter(|skills| !skills.is_empty());
    PlatformAgentA2APeerProjection {
        public_endpoint_url: first_string_map(
            record,
            &["publicEndpointUrl", "public_endpoint_url"],
        ),
        internal_endpoint_url: first_string_map(
            record,
            &["internalEndpointUrl", "internal_endpoint_url"],
        ),
        agent_card_url: first_string_map(record, &["agentCardUrl", "agent_card_url"]),
        protocol_binding: first_string_map(record, &["protocolBinding", "protocol_binding"]),
        protocol_version: first_string_map(record, &["protocolVersion", "protocol_version"]),
        supported_extensions: string_list_map(
            record,
            &["supportedExtensions", "supported_extensions"],
        ),
        skills,
        security_schemes: string_list_map(record, &["securitySchemes", "security_schemes"]),
        agent_card_observed_at: first_string_map(
            record,
            &["agentCardObservedAt", "agent_card_observed_at"],
        ),
        agent_card_etag: first_string_map(
            record,
            &["agentCardETag", "agentCardEtag", "agent_card_etag"],
        ),
        agent_card_hash: first_string_map(record, &["agentCardHash", "agent_card_hash"]),
        push_notifications: first_bool_map(record, &["pushNotifications", "push_notifications"]),
        attributes: object_value_map(record, &["attributes"]).cloned(),
    }
}

fn normalize_skill(record: &Map<String, Value>) -> Option<PlatformAgentA2ASkill> {
    let id = first_string_map(record, &["id"])?;
    Some(PlatformAgentA2ASkill {
        id,
        name: first_string_map(record, &["name"]),
        description: first_string_map(record, &["description"]),
        tags: string_list_map(record, &["tags"]),
        input_modes: string_list_map(record, &["inputModes", "input_modes"]),
        output_modes: string_list_map(record, &["outputModes", "output_modes"]),
        required_context_grants: string_list_map(
            record,
            &["requiredContextGrants", "required_context_grants"],
        ),
        approval_policy_ref: first_string_map(
            record,
            &["approvalPolicyRef", "approval_policy_ref"],
        ),
        max_autonomy: first_string_map(record, &["maxAutonomy", "max_autonomy"]),
        required_artifact_kinds: string_list_map(
            record,
            &["requiredArtifactKinds", "required_artifact_kinds"],
        ),
        optional_artifact_kinds: string_list_map(
            record,
            &["optionalArtifactKinds", "optional_artifact_kinds"],
        ),
        allowed_task_classes: string_list_map(
            record,
            &["allowedTaskClasses", "allowed_task_classes"],
        ),
        denied_task_classes: string_list_map(record, &["deniedTaskClasses", "denied_task_classes"]),
        attributes: object_value_map(record, &["attributes"]).cloned(),
        metadata: object_value_map(record, &["metadata"]).cloned(),
    })
}

fn normalize_capacity(record: &Map<String, Value>) -> PlatformAgentCapacity {
    PlatformAgentCapacity {
        current: first_number_map(record, &["current"]),
        max: first_number_map(record, &["max"]),
        remaining: first_number_map(record, &["remaining"]),
        reserved_delegation_count: first_number_map(
            record,
            &["reservedDelegationCount", "reserved_delegation_count"],
        ),
    }
}

fn normalize_discovery_evidence(record: &Map<String, Value>) -> PlatformAgentDiscoveryEvidence {
    PlatformAgentDiscoveryEvidence {
        schema: first_string_map(record, &["schema"]),
        decision: first_string_map(record, &["decision"]),
        reason: first_string_map(record, &["reason"]),
        organization_id: first_string_map(record, &["organizationId", "organization_id"]),
        workspace_id: first_string_map(record, &["workspaceId", "workspace_id"]),
        capability: first_string_map(record, &["capability"]),
        a2a_skill_id: first_string_map(record, &["a2aSkillId", "a2a_skill_id"]),
        task_class: first_string_map(record, &["taskClass", "task_class"]),
        require_a2a_dispatch: first_bool_map(
            record,
            &[
                "requireA2ADispatch",
                "requireA2aDispatch",
                "require_a2a_dispatch",
            ],
        ),
        eligible_for_delegation: first_bool_map(
            record,
            &["eligibleForDelegation", "eligible_for_delegation"],
        ),
        surface: first_string_map(record, &["surface"]),
        status: first_string_map(record, &["status"]),
        candidate_count: first_number_map(record, &["candidateCount", "candidate_count"]),
        matched_count: first_number_map(record, &["matchedCount", "matched_count"]),
        exclusions: record
            .get("exclusions")
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        let obj = item.as_object()?;
                        Some(PlatformAgentDiscoveryExclusion {
                            reason: first_string_map(obj, &["reason"]),
                            count: first_number_map(obj, &["count"]),
                            sample_agent_ids: string_list_map(
                                obj,
                                &["sampleAgentIds", "sample_agent_ids"],
                            ),
                        })
                    })
                    .collect()
            }),
    }
}

fn normalize_delegation(record: Option<&Map<String, Value>>) -> PlatformDelegationRecord {
    let Some(record) = record else {
        return PlatformDelegationRecord::default();
    };
    PlatformDelegationRecord {
        id: first_string_map(record, &["id"]),
        workspace_id: first_string_map(record, &["workspaceId", "workspace_id"]),
        from_agent_id: first_string_map(record, &["fromAgentId", "from_agent_id"]),
        to_agent_id: first_string_map(record, &["toAgentId", "to_agent_id"]),
        required_capability: first_string_map(
            record,
            &["requiredCapability", "required_capability"],
        ),
        status: first_string_map(record, &["status"]),
        reason: first_string_map(record, &["reason"]),
        a2a_task_id: first_string_map(record, &["a2aTaskId", "a2a_task_id"]),
        a2a_message_id: first_string_map(record, &["a2aMessageId", "a2a_message_id"]),
        a2a_endpoint_url: first_string_map(record, &["a2aEndpointUrl", "a2a_endpoint_url"]),
        a2a_dispatch_status: first_string_map(
            record,
            &["a2aDispatchStatus", "a2a_dispatch_status"],
        ),
        a2a_dispatch_error: first_string_map(record, &["a2aDispatchError", "a2a_dispatch_error"]),
        a2a_skill_id: first_string_map(record, &["a2aSkillId", "a2a_skill_id"]),
        a2a_root_delegation_id: first_string_map(
            record,
            &["a2aRootDelegationId", "a2a_root_delegation_id"],
        ),
        a2a_parent_delegation_id: first_string_map(
            record,
            &["a2aParentDelegationId", "a2a_parent_delegation_id"],
        ),
        a2a_delegation_chain: string_list_map(
            record,
            &["a2aDelegationChain", "a2a_delegation_chain"],
        ),
        a2a_resume_wait_contracts: record
            .get("a2aResumeWaitContracts")
            .or_else(|| record.get("a2a_resume_wait_contracts"))
            .and_then(|v| v.as_array())
            .cloned(),
    }
}

fn normalize_remote_task(record: &Map<String, Value>) -> PlatformA2ADelegationTaskControlResult {
    PlatformA2ADelegationTaskControlResult {
        task_id: first_string_map(record, &["taskId", "task_id"]),
        state: first_string_map(record, &["state"]),
        control_id: first_string_map(record, &["controlId", "control_id"]),
        control_mode: first_string_map(record, &["controlMode", "control_mode"]),
        cancelled: first_bool_map(record, &["cancelled"]),
        queued_for_worker: first_bool_map(record, &["queuedForWorker", "queued_for_worker"]),
    }
}

fn normalize_graph_node(record: &Map<String, Value>) -> PlatformA2ADelegationGraphNode {
    PlatformA2ADelegationGraphNode {
        delegation: object_value_map(record, &["delegation"])
            .map(|obj| normalize_delegation(Some(obj))),
        depth: first_number_map(record, &["depth"]),
        child_count: first_number_map(record, &["childCount", "child_count"]),
        terminal: first_bool_map(record, &["terminal"]),
    }
}

fn normalize_graph_edge(record: &Map<String, Value>) -> PlatformA2ADelegationGraphEdge {
    PlatformA2ADelegationGraphEdge {
        parent_delegation_id: first_string_map(
            record,
            &["parentDelegationId", "parent_delegation_id"],
        ),
        child_delegation_id: first_string_map(
            record,
            &["childDelegationId", "child_delegation_id"],
        ),
    }
}

fn get_env_value(names: &[&str]) -> Option<String> {
    for name in names {
        if let Ok(value) = std::env::var(name) {
            if let Some(trimmed) = trim_opt(Some(value.as_str())) {
                return Some(trimmed.to_owned());
            }
        }
    }
    None
}

fn parse_positive_int(value: Option<&str>, fallback: u64) -> u64 {
    value
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|parsed| *parsed > 0)
        .unwrap_or(fallback)
}

fn trim_opt(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|s| !s.is_empty())
}

fn insert_string(map: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    if let Some(value) = trim_opt(value) {
        map.insert(key.to_string(), Value::String(value.to_string()));
    }
}

fn insert_string_list(map: &mut Map<String, Value>, key: &str, values: Option<&Vec<String>>) {
    if let Some(values) = values {
        let cleaned: Vec<Value> = values
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| Value::String(s.to_string()))
            .collect();
        if !cleaned.is_empty() {
            map.insert(key.to_string(), Value::Array(cleaned));
        }
    }
}

fn object_value<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Map<String, Value>> {
    value
        .as_object()
        .and_then(|obj| object_value_map(obj, keys))
}

fn object_value_map<'a>(
    record: &'a Map<String, Value>,
    keys: &[&str],
) -> Option<&'a Map<String, Value>> {
    for key in keys {
        if let Some(Value::Object(obj)) = record.get(*key) {
            return Some(obj);
        }
    }
    None
}

fn first_string(value: &Value, keys: &[&str]) -> Option<String> {
    value
        .as_object()
        .and_then(|obj| first_string_map(obj, keys))
}

fn first_string_map(record: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(Value::String(s)) = record.get(*key) {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn first_number(value: &Value, keys: &[&str]) -> Option<f64> {
    value
        .as_object()
        .and_then(|obj| first_number_map(obj, keys))
}

fn first_number_map(record: &Map<String, Value>, keys: &[&str]) -> Option<f64> {
    for key in keys {
        match record.get(*key) {
            Some(Value::Number(n)) => return n.as_f64(),
            Some(Value::String(s)) => {
                if let Ok(parsed) = s.trim().parse::<f64>() {
                    return Some(parsed);
                }
            }
            _ => {}
        }
    }
    None
}

fn first_bool(value: &Value, keys: &[&str]) -> Option<bool> {
    value.as_object().and_then(|obj| first_bool_map(obj, keys))
}

fn first_bool_map(record: &Map<String, Value>, keys: &[&str]) -> Option<bool> {
    for key in keys {
        if let Some(Value::Bool(b)) = record.get(*key) {
            return Some(*b);
        }
    }
    None
}

fn string_list(value: &Value, keys: &[&str]) -> Option<Vec<String>> {
    value.as_object().and_then(|obj| string_list_map(obj, keys))
}

fn string_list_map(record: &Map<String, Value>, keys: &[&str]) -> Option<Vec<String>> {
    for key in keys {
        if let Some(Value::Array(items)) = record.get(*key) {
            let strings: Vec<String> = items
                .iter()
                .filter_map(|item| item.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            if !strings.is_empty() {
                return Some(strings);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn normalizes_control_modes() {
        assert_eq!(
            normalize_a2a_control_mode("steer").unwrap(),
            CONTROL_MODE_STEER
        );
        assert_eq!(
            normalize_a2a_control_mode("follow-up").unwrap(),
            CONTROL_MODE_FOLLOWUP
        );
        assert!(normalize_a2a_control_mode("explode").is_err());
    }

    #[test]
    fn encodes_context_payload_as_base64() {
        let encoded = BASE64.encode(br#"{"prompt":"hello"}"#);
        assert_eq!(
            String::from_utf8(BASE64.decode(encoded.as_bytes()).unwrap()).unwrap(),
            r#"{"prompt":"hello"}"#
        );
    }

    #[tokio::test]
    async fn list_agents_posts_connect_headers_and_parses_candidates() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0_u8; 16384];
            let read = stream.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..read]);
            assert!(request.contains("POST /agents.v1.AgentService/List"));
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer reg-token")
            );
            assert!(
                request.contains("X-Organization-ID: org_1")
                    || request.contains("x-organization-id: org_1")
            );
            assert!(
                request.contains("X-Workspace-ID: ws_1")
                    || request.contains("x-workspace-id: ws_1")
            );
            assert!(
                request.contains("Connect-Protocol-Version: 1")
                    || request.contains("connect-protocol-version: 1")
            );
            assert!(
                request.contains(r#""workspaceId":"ws_1""#),
                "list requests must carry the configured workspace scope: {request}"
            );
            let body = r#"{
              "agents": [{
                "id": "agent-1",
                "name": "Peer One",
                "status": "AGENT_STATUS_IDLE",
                "lastHeartbeatAt": "2026-07-21T00:00:00.000Z",
                "a2a": {
                  "publicEndpointUrl": "http://127.0.0.1:18787",
                  "protocolBinding": "HTTP+JSON",
                  "protocolVersion": "1.0",
                  "pushNotifications": true,
                  "skills": [{"id": "maestro-tui-turn", "name": "TUI"}]
                }
              }],
              "discoveryEvidence": {
                "decision": "matched",
                "matchedCount": 1,
                "candidateCount": 1
              }
            }"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        let config = AgentRegistryConfig {
            base_url: format!("http://{address}"),
            token: "reg-token".into(),
            organization_id: "org_1".into(),
            workspace_id: "ws_1".into(),
            timeout_ms: 2_000,
            max_attempts: 1,
        };
        let result = list_a2a_peer_candidates_with_evidence(
            ListA2APeersInput {
                skill_id: Some("maestro-tui-turn".into()),
                ..Default::default()
            },
            Some(config),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result.candidates.len(), 1);
        assert_eq!(result.candidates[0].endpoint_url, "http://127.0.0.1:18787");
        assert_eq!(result.candidates[0].skills[0].id, "maestro-tui-turn");
        assert_eq!(
            result
                .discovery_evidence
                .as_ref()
                .and_then(|e| e.decision.as_deref()),
            Some("matched")
        );
        server.await.unwrap();
    }

    #[test]
    fn agent_registry_scope_requires_an_explicit_workspace() {
        assert_eq!(resolve_workspace_scope(None, None), None);
        assert_eq!(
            resolve_workspace_scope(Some("  ws_requested  "), None),
            Some("ws_requested".to_owned())
        );
        assert_eq!(
            resolve_workspace_scope(None, Some("ws_configured".to_owned())),
            Some("ws_configured".to_owned())
        );
    }

    #[test]
    fn supplied_agent_registry_config_cannot_be_unscoped() {
        let config = AgentRegistryConfig {
            base_url: "https://platform.example".to_owned(),
            token: "token".to_owned(),
            organization_id: "org_1".to_owned(),
            workspace_id: String::new(),
            timeout_ms: 1_000,
            max_attempts: 1,
        };
        assert!(resolve_config(Some(config), None).is_none());
    }
}
