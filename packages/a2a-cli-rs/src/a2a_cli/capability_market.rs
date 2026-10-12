//! A2A capability market ranking for Platform peer discovery.
//!
//! Ports `src/platform/a2a-capability-market.ts`.

use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

use super::agent_registry::{PlatformAgentA2ASkill, PlatformAgentRegistryA2APeerCandidate};

pub const A2A_CAPABILITY_MARKET_VERSION: &str = "evalops.maestro.a2a-capability-market.v1";

#[derive(Debug, Clone, Default)]
pub struct A2ACapabilityMarketRequest {
    pub skill_id: Option<String>,
    pub task_class: Option<String>,
    pub required_context_grants: Option<Vec<String>>,
    pub required_artifact_kinds: Option<Vec<String>>,
    pub prefer_internal_endpoint: bool,
    pub now_ms: Option<i64>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct A2ACapabilityMarketRank {
    pub version: &'static str,
    pub candidate: PlatformAgentRegistryA2APeerCandidate,
    pub selected_skill: Option<PlatformAgentA2ASkill>,
    pub score: f64,
    pub reasons: Vec<String>,
    pub blockers: Vec<String>,
}

pub fn rank_a2a_capability_peers(
    candidates: &[PlatformAgentRegistryA2APeerCandidate],
    request: &A2ACapabilityMarketRequest,
) -> Vec<A2ACapabilityMarketRank> {
    let mut ranks: Vec<_> = candidates
        .iter()
        .map(|candidate| rank_candidate(candidate, request))
        .filter(|rank| rank.blockers.is_empty())
        .collect();
    ranks.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                let left_id = left
                    .candidate
                    .agent
                    .id
                    .as_deref()
                    .unwrap_or(left.candidate.endpoint_url.as_str());
                let right_id = right
                    .candidate
                    .agent
                    .id
                    .as_deref()
                    .unwrap_or(right.candidate.endpoint_url.as_str());
                left_id.cmp(right_id)
            })
    });
    ranks
}

pub fn select_a2a_capability_peer(
    candidates: &[PlatformAgentRegistryA2APeerCandidate],
    request: &A2ACapabilityMarketRequest,
) -> Option<A2ACapabilityMarketRank> {
    rank_a2a_capability_peers(candidates, request)
        .into_iter()
        .next()
}

fn rank_candidate(
    candidate: &PlatformAgentRegistryA2APeerCandidate,
    request: &A2ACapabilityMarketRequest,
) -> A2ACapabilityMarketRank {
    let now_ms = request.now_ms.unwrap_or_else(current_time_ms);
    let task_class = request
        .task_class
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let skill_rank = select_skill_rank(candidate, request, task_class);
    let mut score = skill_rank.score;
    let mut reasons = skill_rank.reasons;
    let blockers = skill_rank.blockers;

    let status = status_score(candidate.agent.status.as_deref());
    score += status.0;
    reasons.push(status.1.into());

    let heartbeat = heartbeat_score(candidate.agent.last_heartbeat_at.as_deref(), now_ms);
    score += heartbeat.0;
    if let Some(reason) = heartbeat.1 {
        reasons.push(reason.into());
    }
    if candidate.endpoint_kind.as_deref() == Some("internal") && request.prefer_internal_endpoint {
        score += 8.0;
        reasons.push("internal_endpoint".into());
    }
    if candidate.push_notifications == Some(true) {
        score += 5.0;
        reasons.push("push_notifications".into());
    }

    A2ACapabilityMarketRank {
        version: A2A_CAPABILITY_MARKET_VERSION,
        candidate: candidate.clone(),
        selected_skill: skill_rank.selected_skill,
        score: if blockers.is_empty() {
            score
        } else {
            f64::NEG_INFINITY
        },
        reasons,
        blockers,
    }
}

struct SkillRank {
    selected_skill: Option<PlatformAgentA2ASkill>,
    score: f64,
    reasons: Vec<String>,
    blockers: Vec<String>,
}

fn select_skill_rank(
    candidate: &PlatformAgentRegistryA2APeerCandidate,
    request: &A2ACapabilityMarketRequest,
    task_class: Option<&str>,
) -> SkillRank {
    if let Some(skill_id) = request
        .skill_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return match candidate.skills.iter().find(|skill| skill.id == skill_id) {
            Some(skill) => rank_skill(skill, request, task_class),
            None => no_skill_rank(request, task_class),
        };
    }
    if candidate.skills.is_empty() {
        return no_skill_rank(request, task_class);
    }
    let skill_ranks: Vec<_> = candidate
        .skills
        .iter()
        .map(|skill| rank_skill(skill, request, task_class))
        .collect();
    if let Some(best) = skill_ranks
        .iter()
        .filter(|rank| rank.blockers.is_empty())
        .max_by(|a, b| {
            a.score
                .partial_cmp(&b.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    {
        return SkillRank {
            selected_skill: best.selected_skill.clone(),
            score: best.score,
            reasons: best.reasons.clone(),
            blockers: best.blockers.clone(),
        };
    }
    SkillRank {
        selected_skill: None,
        score: f64::NEG_INFINITY,
        reasons: vec![],
        blockers: unique(
            skill_ranks
                .into_iter()
                .flat_map(|rank| rank.blockers)
                .collect(),
        ),
    }
}

fn rank_skill(
    skill: &PlatformAgentA2ASkill,
    request: &A2ACapabilityMarketRequest,
    task_class: Option<&str>,
) -> SkillRank {
    let mut reasons = vec![format!("skill:{}", skill.id)];
    let mut blockers = Vec::new();
    let mut score = if request.skill_id.is_some() {
        35.0
    } else {
        15.0
    };

    if let Some(task_class) = task_class {
        let denied = normalized_set(skill.denied_task_classes.as_ref());
        if denied.contains(task_class) {
            blockers.push(format!("task_class_denied:{task_class}"));
        }
        let allowed = normalized_set(skill.allowed_task_classes.as_ref());
        if !allowed.is_empty() && !allowed.contains(task_class) {
            blockers.push(format!("task_class_not_allowed:{task_class}"));
        } else if allowed.contains(task_class) {
            score += 10.0;
            reasons.push(format!("task_class:{task_class}"));
        }
    }
    if !has_all(
        request.required_context_grants.as_ref(),
        skill.required_context_grants.as_ref(),
    ) {
        blockers.push("missing_context_grants".into());
    }
    if !has_all(
        request.required_artifact_kinds.as_ref(),
        skill.required_artifact_kinds.as_ref(),
    ) {
        blockers.push("missing_required_artifacts".into());
    }
    if skill.approval_policy_ref.is_some() {
        score += 4.0;
        reasons.push("approval_policy".into());
    }
    if skill
        .required_artifact_kinds
        .as_ref()
        .is_some_and(|items| !items.is_empty())
    {
        score += 3.0;
        reasons.push("artifact_contract".into());
    }

    SkillRank {
        selected_skill: Some(skill.clone()),
        score: if blockers.is_empty() {
            score
        } else {
            f64::NEG_INFINITY
        },
        reasons,
        blockers,
    }
}

fn no_skill_rank(request: &A2ACapabilityMarketRequest, task_class: Option<&str>) -> SkillRank {
    let mut blockers = Vec::new();
    if let Some(skill_id) = &request.skill_id {
        blockers.push(format!("missing_skill:{skill_id}"));
    }
    if let Some(task_class) = task_class {
        blockers.push(format!("missing_task_class:{task_class}"));
    }
    if has_requested_values(request.required_context_grants.as_ref()) {
        blockers.push("missing_context_grants".into());
    }
    if has_requested_values(request.required_artifact_kinds.as_ref()) {
        blockers.push("missing_required_artifacts".into());
    }
    SkillRank {
        selected_skill: None,
        score: 0.0,
        reasons: vec![],
        blockers,
    }
}

fn status_score(status: Option<&str>) -> (f64, &'static str) {
    let normalized = status.unwrap_or("").to_ascii_uppercase();
    if normalized.contains("IDLE") {
        return (30.0, "status_idle");
    }
    if normalized == "ACTIVE"
        || normalized == "AGENT_STATUS_ACTIVE"
        || normalized.contains("ONLINE")
        || normalized.contains("READY")
    {
        return (20.0, "status_online");
    }
    if normalized.contains("BUSY") {
        return (5.0, "status_busy");
    }
    (0.0, "status_unknown")
}

fn heartbeat_score(last_heartbeat_at: Option<&str>, now_ms: i64) -> (f64, Option<&'static str>) {
    let Some(last) = last_heartbeat_at else {
        return (0.0, None);
    };
    let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(last) else {
        return (0.0, None);
    };
    let age_ms = now_ms - parsed.timestamp_millis();
    if age_ms < 0 {
        return (0.0, None);
    }
    if age_ms <= 60_000 {
        return (15.0, Some("heartbeat_fresh"));
    }
    if age_ms <= 5 * 60_000 {
        return (8.0, Some("heartbeat_recent"));
    }
    (-10.0, Some("heartbeat_stale"))
}

fn normalized_set(values: Option<&Vec<String>>) -> BTreeSet<String> {
    values
        .into_iter()
        .flatten()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect()
}

fn has_all(required: Option<&Vec<String>>, available: Option<&Vec<String>>) -> bool {
    let available = normalized_set(available);
    normalized_set(required)
        .into_iter()
        .all(|value| available.contains(&value))
}

fn has_requested_values(values: Option<&Vec<String>>) -> bool {
    values
        .into_iter()
        .flatten()
        .any(|value| !value.trim().is_empty())
}

fn unique(values: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for value in values {
        if seen.insert(value.clone()) {
            out.push(value);
        }
    }
    out
}

fn current_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::a2a_cli::agent_registry::{
        PlatformAgentA2ASkill, PlatformAgentRegistryA2APeerCandidate, PlatformAgentRegistryAgent,
    };

    fn candidate(id: &str, skill: &str, status: &str) -> PlatformAgentRegistryA2APeerCandidate {
        PlatformAgentRegistryA2APeerCandidate {
            agent: PlatformAgentRegistryAgent {
                id: Some(id.into()),
                name: Some(id.into()),
                status: Some(status.into()),
                last_heartbeat_at: Some("2026-07-21T00:00:00.000Z".into()),
                ..Default::default()
            },
            endpoint_url: format!("http://{id}.local"),
            endpoint_kind: Some("public".into()),
            agent_card_url: None,
            protocol_binding: Some("HTTP+JSON".into()),
            protocol_version: Some("1.0".into()),
            skills: vec![PlatformAgentA2ASkill {
                id: skill.into(),
                name: Some(skill.into()),
                approval_policy_ref: Some("policy".into()),
                required_artifact_kinds: Some(vec!["patch.summary".into()]),
                ..Default::default()
            }],
            supported_extensions: None,
            push_notifications: Some(true),
        }
    }

    #[test]
    fn ranks_idle_skill_match_highest() {
        let candidates = vec![
            candidate("busy", "maestro-tui-turn", "AGENT_STATUS_BUSY"),
            candidate("idle", "maestro-tui-turn", "AGENT_STATUS_IDLE"),
        ];
        let selected = select_a2a_capability_peer(
            &candidates,
            &A2ACapabilityMarketRequest {
                skill_id: Some("maestro-tui-turn".into()),
                now_ms: Some(
                    chrono::DateTime::parse_from_rfc3339("2026-07-21T00:00:30.000Z")
                        .unwrap()
                        .timestamp_millis(),
                ),
                ..Default::default()
            },
        )
        .expect("selected");
        assert_eq!(selected.candidate.agent.id.as_deref(), Some("idle"));
        assert!(selected.score > 40.0);
        assert!(selected.reasons.iter().any(|r| r == "status_idle"));
    }

    #[test]
    fn blocks_missing_skill() {
        let candidates = vec![candidate("peer", "other-skill", "AGENT_STATUS_IDLE")];
        let ranks = rank_a2a_capability_peers(
            &candidates,
            &A2ACapabilityMarketRequest {
                skill_id: Some("maestro-tui-turn".into()),
                ..Default::default()
            },
        );
        assert!(ranks.is_empty());
    }
}
