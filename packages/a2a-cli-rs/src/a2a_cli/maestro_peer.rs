//! Maestro A2A peer projection for Platform agent-registry registration.
//!
//! Ports `src/platform/a2a-maestro-peer.ts` with a static skill catalog that
//! mirrors the governed subagent lanes.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

use super::agent_registry::{PlatformAgentA2APeerProjection, PlatformAgentA2ASkill};

pub const MAESTRO_A2A_PROTOCOL_VERSION: &str = "1.0";
pub const MAESTRO_A2A_PROTOCOL_BINDING: &str = "HTTP+JSON";
pub const MAESTRO_A2A_AGENT_CARD_PATH: &str = "/.well-known/agent-card.json";
pub const EVALOPS_A2A_OPERATING_PLANE_EXTENSION_URI: &str =
    "https://evalops.com/a2a/extensions/operating-plane/v1";

#[derive(Debug, Clone, Default)]
pub struct BuildMaestroA2APeerProjectionInput {
    pub public_endpoint_url: String,
    pub internal_endpoint_url: Option<String>,
    pub agent_card_url: Option<String>,
    pub protocol_version: Option<String>,
    pub agent_card_etag: Option<String>,
    pub agent_card_hash: Option<String>,
    pub push_notifications: Option<bool>,
    pub security_schemes: Option<Vec<String>>,
    pub attributes: Option<BTreeMap<String, String>>,
}

pub fn default_maestro_a2a_capabilities() -> Vec<String> {
    unique_strings(vec![
        "maestro:a2a".into(),
        "maestro:cli".into(),
        "maestro:subagents".into(),
        "code:write".into(),
        "code:review".into(),
        "test:run".into(),
        "browser:qa".into(),
        "repo:explore".into(),
        "release:shepherd".into(),
    ])
}

pub fn build_maestro_a2a_peer_projection(
    input: BuildMaestroA2APeerProjectionInput,
) -> PlatformAgentA2APeerProjection {
    let public_endpoint_url = normalize_endpoint(&input.public_endpoint_url);
    let internal_endpoint_url = input
        .internal_endpoint_url
        .as_deref()
        .map(normalize_endpoint);
    let agent_card_url = input
        .agent_card_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{public_endpoint_url}{MAESTRO_A2A_AGENT_CARD_PATH}"));
    let security_schemes = input
        .security_schemes
        .filter(|items| !items.is_empty())
        .map(unique_strings)
        .unwrap_or_else(|| vec!["evalops-agent-token".into()]);

    let mut attributes = Map::new();
    attributes.insert("runtime".into(), Value::String("maestro".into()));
    attributes.insert(
        "controlPlane".into(),
        Value::String("rust-control-plane".into()),
    );
    attributes.insert(
        "operatingPlaneExtension".into(),
        Value::String(EVALOPS_A2A_OPERATING_PLANE_EXTENSION_URI.into()),
    );
    if let Some(extra) = input.attributes {
        for (key, value) in extra {
            attributes.insert(key, Value::String(value));
        }
    }

    PlatformAgentA2APeerProjection {
        public_endpoint_url: Some(public_endpoint_url),
        internal_endpoint_url,
        agent_card_url: Some(agent_card_url),
        protocol_binding: Some(MAESTRO_A2A_PROTOCOL_BINDING.into()),
        protocol_version: Some(
            input
                .protocol_version
                .unwrap_or_else(|| MAESTRO_A2A_PROTOCOL_VERSION.into()),
        ),
        supported_extensions: Some(vec![EVALOPS_A2A_OPERATING_PLANE_EXTENSION_URI.into()]),
        skills: Some(maestro_a2a_agent_skills()),
        security_schemes: Some(security_schemes),
        agent_card_observed_at: None,
        agent_card_etag: input.agent_card_etag,
        agent_card_hash: input.agent_card_hash,
        push_notifications: Some(input.push_notifications.unwrap_or(true)),
        attributes: Some(attributes),
    }
}

pub fn maestro_a2a_agent_skills() -> Vec<PlatformAgentA2ASkill> {
    let mut skills = vec![PlatformAgentA2ASkill {
        id: "maestro-tui-turn".into(),
        name: Some("Deixic Code TUI turn".into()),
        description: Some(
            "Run a prompt through the local Deixic Code native TUI agent runner.".into(),
        ),
        tags: Some(vec![
            "maestro".into(),
            "tui".into(),
            "codex".into(),
            "a2a".into(),
            "fleet".into(),
        ]),
        input_modes: Some(vec!["text/plain".into()]),
        output_modes: Some(vec!["text/plain".into(), "application/json".into()]),
        attributes: Some(map_string_values(&[
            ("evalopsSkillKind", "maestro-turn"),
            (
                "operatingPlaneExtension",
                EVALOPS_A2A_OPERATING_PLANE_EXTENSION_URI,
            ),
        ])),
        ..Default::default()
    }];
    skills.extend(subagent_lane_skills());
    skills
}

fn subagent_lane_skills() -> Vec<PlatformAgentA2ASkill> {
    // lane_id, skill_id, description, required_caps, input_kinds, output_kinds, tags
    type LaneDef = (
        &'static str,
        &'static str,
        &'static str,
        &'static [&'static str],
        &'static [&'static str],
        &'static [&'static str],
        &'static [&'static str],
    );
    const LANES: &[LaneDef] = &[
        (
            "code-writer",
            "maestro-subagent-code-writer",
            "Governed code-writer subagent lane.",
            &["repo:read", "repo:write-scoped", "tool:execute-tests"],
            &["patch.summary"],
            &["test.report", "review.summary"],
            &["code.implementation", "code.refactor"],
        ),
        (
            "code-review",
            "maestro-subagent-code-review",
            "Governed code-review subagent lane.",
            &["repo:read", "pull-request:read", "artifact:read"],
            &["review.summary"],
            &["risk.finding", "test.plan"],
            &["code.review", "risk.analysis"],
        ),
        (
            "test-runner",
            "maestro-subagent-test-runner",
            "Governed test-runner subagent lane.",
            &["repo:read", "tool:execute-tests", "artifact:write"],
            &["test.report"],
            &["failure.triage", "coverage.summary"],
            &["test.execution", "ci.triage"],
        ),
        (
            "browser-qa",
            "maestro-subagent-browser-qa",
            "Governed browser-qa subagent lane.",
            &["browser:control", "artifact:write", "runtime:events:read"],
            &["qa.repro-report"],
            &[
                "screenshot",
                "repro.video",
                "browser.console",
                "network.error",
            ],
            &["product.qa", "browser.e2e", "ux.repro"],
        ),
        (
            "repo-explorer",
            "maestro-subagent-repo-explorer",
            "Governed repo-explorer subagent lane.",
            &["repo:read", "context:index:write"],
            &["repo.map"],
            &["context.index"],
            &["repo.inspect", "context.gathering"],
        ),
        (
            "release-shepherd",
            "maestro-subagent-release-shepherd",
            "Governed release-shepherd subagent lane.",
            &[
                "repo:read",
                "pull-request:write",
                "deploy:read",
                "artifact:write",
                "runtime:events:read",
            ],
            &["release.summary"],
            &["ci.summary", "deploy.status"],
            &["release.follow-through", "deployment.smoke"],
        ),
    ];

    LANES
        .iter()
        .map(
            |(
                lane_id,
                skill_id,
                description,
                grants,
                required_artifacts,
                optional_artifacts,
                allowed_classes,
            )| {
                PlatformAgentA2ASkill {
                    id: (*skill_id).into(),
                    name: Some(lane_id.replace('-', " ")),
                    description: Some((*description).into()),
                    tags: Some(vec!["maestro".into(), "subagent".into(), (*lane_id).into()]),
                    input_modes: Some(vec!["text/plain".into(), "application/json".into()]),
                    output_modes: Some(vec!["text/plain".into(), "application/json".into()]),
                    required_context_grants: Some(string_vec(grants)),
                    approval_policy_ref: Some(format!("maestro.subagent.{lane_id}.target-policy")),
                    max_autonomy: Some("bounded".into()),
                    required_artifact_kinds: Some(string_vec(required_artifacts)),
                    optional_artifact_kinds: Some(string_vec(optional_artifacts)),
                    allowed_task_classes: Some(string_vec(allowed_classes)),
                    denied_task_classes: Some(vec![
                        "credential.materialization".into(),
                        "secret.exfiltration".into(),
                        "unbounded.repository.write".into(),
                    ]),
                    attributes: Some(map_string_values(&[
                        ("evalopsSkillKind", "maestro-subagent"),
                        ("subagentLaneId", lane_id),
                        ("requestMetadataPath", "evalops.subagentRequest"),
                        (
                            "operatingPlaneExtension",
                            EVALOPS_A2A_OPERATING_PLANE_EXTENSION_URI,
                        ),
                    ])),
                    metadata: Some(map_string_values(&[
                        ("evalopsSkillKind", "maestro-subagent"),
                        ("subagentLaneId", lane_id),
                        ("requestMetadataPath", "evalops.subagentRequest"),
                        (
                            "operatingPlaneExtension",
                            EVALOPS_A2A_OPERATING_PLANE_EXTENSION_URI,
                        ),
                    ])),
                }
            },
        )
        .collect()
}

fn map_string_values(entries: &[(&str, &str)]) -> Map<String, Value> {
    let mut map = Map::new();
    for (key, value) in entries {
        map.insert((*key).into(), Value::String((*value).into()));
    }
    map
}

fn string_vec(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

fn normalize_endpoint(value: &str) -> String {
    value.trim().trim_end_matches('/').to_string()
}

fn unique_strings(values: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for value in values {
        let trimmed = value.trim().to_string();
        if trimmed.is_empty() || !seen.insert(trimmed.clone()) {
            continue;
        }
        out.push(trimmed);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_projection_with_default_skills() {
        let projection = build_maestro_a2a_peer_projection(BuildMaestroA2APeerProjectionInput {
            public_endpoint_url: "http://127.0.0.1:18787/".into(),
            attributes: Some(BTreeMap::from([(
                "publishedBy".into(),
                "deixic-code a2a register".into(),
            )])),
            ..Default::default()
        });
        assert_eq!(
            projection.public_endpoint_url.as_deref(),
            Some("http://127.0.0.1:18787")
        );
        assert_eq!(
            projection.agent_card_url.as_deref(),
            Some("http://127.0.0.1:18787/.well-known/agent-card.json")
        );
        let skills = projection.skills.unwrap();
        assert!(skills.iter().any(|skill| skill.id == "maestro-tui-turn"));
        assert!(
            skills
                .iter()
                .any(|skill| skill.id == "maestro-subagent-code-writer")
        );
        assert_eq!(
            projection
                .attributes
                .as_ref()
                .and_then(|attrs| attrs.get("publishedBy"))
                .and_then(|v| v.as_str()),
            Some("deixic-code a2a register")
        );
    }
}
