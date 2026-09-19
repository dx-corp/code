//! Typed, transport-neutral coding completion contracts. The runtime supplies
//! observations; the work owner supplies the admitted contract and child records.
//! Child provenance attests which runtime produced a report, not its correctness.

#![forbid(unsafe_code)]

pub mod computer;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

pub const CODING_ACCEPTANCE_METADATA_KEY: &str = "codingAcceptance";
pub const CODING_ACCEPTANCE_RESULT_METADATA_KEY: &str = "codingAcceptanceResult";
pub const CODING_ACCEPTANCE_CHILD_RECORDS_KEY: &str = "codingAcceptanceChildRecords";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingAcceptanceContract {
    pub task_id: String,
    pub repository_id: String,
    pub generation: u64,
    pub required_assertion_ids: Vec<String>,
    pub require_review: bool,
    pub require_behavior: bool,
    pub readiness_requirements: Vec<String>,
    /// Exact assertion/readiness IDs the admitting owner permits to be skipped.
    #[serde(default)]
    pub authorized_skips: Vec<String>,
    /// Exact handoff item IDs the admitting owner permits to be deferred/dismissed.
    #[serde(default)]
    pub authorized_dispositions: Vec<String>,
    /// Exact UTF-8 source files whose blob content must differ from the admitted
    /// baseline (or be newly added). Mode-only changes do not satisfy this option.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_paths: Vec<String>,
}

impl CodingAcceptanceContract {
    pub fn digest(&self) -> String {
        digest(self)
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if !valid_id(&self.task_id)
            || !valid_id(&self.repository_id)
            || self.required_assertion_ids.is_empty()
            || self.readiness_requirements.is_empty()
            || !self.require_review
            || !self.require_behavior
            || self.output_paths.len() > 8
            || (!self.output_paths.is_empty() && !unique_ids(&self.output_paths))
            || self
                .output_paths
                .iter()
                .any(|path| !valid_output_path(path))
            || self.generation == 0
            || !unique_ids(&self.required_assertion_ids)
            || !unique_ids(&self.readiness_requirements)
            || self.readiness_requirements.iter().any(|id| {
                !matches!(
                    id.as_str(),
                    "build" | "test" | "start" | "authentication" | "observation"
                )
            })
            || !unique_ids(&self.authorized_skips)
            || !unique_ids(&self.authorized_dispositions)
            || self.authorized_skips.iter().any(|id| {
                !self.required_assertion_ids.contains(id)
                    && !self.readiness_requirements.contains(id)
            })
        {
            return Err("invalid coding acceptance contract");
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CodingVerificationStatus {
    Passed,
    Failed,
    Blocked,
    Skipped,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CodingValidationRole {
    Review,
    Behavior,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingCommandResult {
    pub command: String,
    /// None means the command did not reach a terminal exit status.
    pub exit_code: Option<i32>,
    pub evidence_refs: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingAssertionResult {
    pub assertion_id: String,
    pub status: CodingVerificationStatus,
    pub evidence_refs: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingValidationReport {
    pub child_id: String,
    pub session_id: String,
    pub revision: String,
    pub status: CodingVerificationStatus,
    pub assertions: Vec<CodingAssertionResult>,
    pub evidence_refs: Vec<String>,
}

impl CodingValidationReport {
    pub fn digest(&self) -> String {
        digest(self)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CodingHandoffDisposition {
    Open,
    Resolved,
    Deferred,
    Dismissed,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingHandoffItem {
    pub id: String,
    pub disposition: CodingHandoffDisposition,
    pub evidence_refs: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingCompletionSubmission {
    pub task_id: String,
    pub work_id: String,
    pub repository_id: String,
    pub contract_digest: String,
    pub generation: u64,
    pub revision: String,
    pub implementation_session_id: String,
    pub commands: Vec<CodingCommandResult>,
    pub readiness: Vec<CodingAssertionResult>,
    pub review: Option<CodingValidationReport>,
    pub behavior: Option<CodingValidationReport>,
    pub handoff_items: Vec<CodingHandoffItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<CodingOutputFile>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingOutputFile {
    pub path: String,
    pub content: String,
}

pub fn valid_output_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 512
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && path
            .split('/')
            .all(|part| !matches!(part, "" | "." | ".." | ".git"))
}

impl CodingCompletionSubmission {
    pub fn digest(&self) -> String {
        digest(self)
    }
}

/// Populated from actual completed child executions by the runtime owner and
/// persisted with its fenced checkpoint. Do not construct from submission IDs.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingAcceptanceChildRecord {
    pub organization_id: String,
    pub workspace_id: String,
    pub work_id: String,
    pub parent_session_id: String,
    pub child_id: String,
    pub session_id: String,
    pub role: CodingValidationRole,
    pub revision: String,
    pub completed_successfully: bool,
    pub report_digest: String,
}

pub struct CodingAcceptanceScope<'a> {
    pub organization_id: &'a str,
    pub workspace_id: &'a str,
    pub work_id: &'a str,
    pub implementation_session_id: &'a str,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingAcceptanceDecision {
    pub accepted: bool,
    pub contract_digest: String,
    pub submission_digest: Option<String>,
    pub reasons: Vec<String>,
}

/// Deterministic decision: the same admitted contract, exact submission, and
/// owner-held child records produce the same replayable result.
pub fn evaluate_coding_acceptance(
    contract: &CodingAcceptanceContract,
    submission: Option<&CodingCompletionSubmission>,
    scope: &CodingAcceptanceScope<'_>,
    children: &[CodingAcceptanceChildRecord],
) -> CodingAcceptanceDecision {
    let mut decision = CodingAcceptanceDecision {
        accepted: false,
        contract_digest: contract.digest(),
        submission_digest: submission.map(digest),
        reasons: Vec::new(),
    };
    if let Err(reason) = contract.validate() {
        decision.reasons.push(reason.into());
    }
    let Some(submission) = submission else {
        decision
            .reasons
            .push("missing coding completion submission".into());
        return decision;
    };
    let actual: BTreeSet<_> = submission
        .outputs
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    let expected: BTreeSet<_> = contract.output_paths.iter().map(String::as_str).collect();
    if actual != expected
        || actual.len() != submission.outputs.len()
        || submission
            .outputs
            .iter()
            .any(|file| !valid_output_path(&file.path) || file.content.is_empty())
        || submission
            .outputs
            .iter()
            .map(|file| file.content.len())
            .sum::<usize>()
            > 65_536
    {
        decision
            .reasons
            .push("coding output manifest mismatch or size exceeded".into());
    }

    if submission.task_id != contract.task_id
        || submission.repository_id != contract.repository_id
        || submission.work_id != scope.work_id
        || submission.generation != contract.generation
        || submission.contract_digest != decision.contract_digest
        || submission.implementation_session_id != scope.implementation_session_id
        || !valid_revision(&submission.revision)
        || submission.implementation_session_id.trim().is_empty()
    {
        decision
            .reasons
            .push("coding completion identity mismatch".into());
    }
    if submission.commands.is_empty()
        || submission.commands.iter().any(|command| {
            command.command.trim().is_empty()
                || command.exit_code != Some(0)
                || !has_evidence(&command.evidence_refs)
        })
    {
        decision
            .reasons
            .push("verification commands lack successful exit evidence".into());
    }
    check_assertions(
        &contract.readiness_requirements,
        &submission.readiness,
        &contract.authorized_skips,
        &mut decision.reasons,
    );
    for (role, required, report) in [
        (
            CodingValidationRole::Review,
            contract.require_review,
            submission.review.as_ref(),
        ),
        (
            CodingValidationRole::Behavior,
            contract.require_behavior,
            submission.behavior.as_ref(),
        ),
    ] {
        let Some(report) = report else {
            if required {
                decision.reasons.push(format!("missing {role:?} report"));
            }
            continue;
        };
        if report.status != CodingVerificationStatus::Passed
            || report.revision != submission.revision
            || report.session_id == submission.implementation_session_id
            || report.session_id.trim().is_empty()
            || report.child_id.trim().is_empty()
            || !has_evidence(&report.evidence_refs)
        {
            decision.reasons.push(format!("invalid {role:?} report"));
        }
        let report_digest = report.digest();
        let matching_children = children
            .iter()
            .filter(|child| {
                child.organization_id == scope.organization_id
                    && child.workspace_id == scope.workspace_id
                    && child.work_id == scope.work_id
                    && child.parent_session_id == scope.implementation_session_id
                    && child.child_id == report.child_id
                    && child.session_id == report.session_id
                    && child.role == role
                    && child.revision == submission.revision
                    && child.completed_successfully
                    && child.report_digest == report_digest
            })
            .count();
        if matching_children != 1 {
            decision
                .reasons
                .push(format!("missing authoritative {role:?} child evidence"));
        }
        check_assertions(
            &contract.required_assertion_ids,
            &report.assertions,
            &contract.authorized_skips,
            &mut decision.reasons,
        );
    }
    if let (Some(review), Some(behavior)) = (&submission.review, &submission.behavior)
        && (review.child_id == behavior.child_id || review.session_id == behavior.session_id)
    {
        decision
            .reasons
            .push("review and behavior require distinct child executions".into());
    }
    let item_ids: Vec<_> = submission
        .handoff_items
        .iter()
        .map(|item| item.id.clone())
        .collect();
    if !unique_ids(&item_ids) {
        decision
            .reasons
            .push("invalid handoff item identities".into());
    }
    for item in &submission.handoff_items {
        if !has_evidence(&item.evidence_refs)
            || match item.disposition {
                CodingHandoffDisposition::Open => true,
                CodingHandoffDisposition::Resolved => false,
                CodingHandoffDisposition::Deferred | CodingHandoffDisposition::Dismissed => {
                    !contract.authorized_dispositions.contains(&item.id)
                }
            }
        {
            decision
                .reasons
                .push(format!("unresolved handoff item: {}", item.id));
        }
    }
    decision.accepted = decision.reasons.is_empty();
    decision
}

fn check_assertions(
    required: &[String],
    reports: &[CodingAssertionResult],
    authorized_skips: &[String],
    reasons: &mut Vec<String>,
) {
    let ids: Vec<_> = reports
        .iter()
        .map(|report| report.assertion_id.clone())
        .collect();
    if !unique_ids(&ids) {
        reasons.push("invalid assertion identities".into());
    }
    for id in required {
        if !ids.contains(id) {
            reasons.push(format!("missing assertion: {id}"));
        }
    }
    for report in reports {
        let passes = report.status == CodingVerificationStatus::Passed
            || (report.status == CodingVerificationStatus::Skipped
                && authorized_skips.contains(&report.assertion_id));
        if !passes || !has_evidence(&report.evidence_refs) {
            reasons.push(format!(
                "assertion lacks accepted evidence: {}",
                report.assertion_id
            ));
        }
    }
}

fn valid_id(id: &str) -> bool {
    !id.trim().is_empty() && id.len() <= 512 && !id.chars().any(char::is_control)
}

/// Return whether a revision is a full hexadecimal SHA-1 or SHA-256 Git object ID.
pub fn valid_revision(revision: &str) -> bool {
    matches!(revision.len(), 40 | 64) && revision.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn unique_ids(ids: &[String]) -> bool {
    ids.len() <= 128
        && ids.iter().all(|id| valid_id(id))
        && ids.iter().collect::<BTreeSet<_>>().len() == ids.len()
}

fn has_evidence(refs: &[String]) -> bool {
    !refs.is_empty() && unique_ids(refs)
}

fn digest(value: &impl Serialize) -> String {
    // These concrete types have no map keys or fallible custom serializers.
    let bytes = serde_json::to_vec(value).expect("coding acceptance types serialize");
    format!(
        "sha256:{}",
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> CodingAcceptanceScope<'static> {
        CodingAcceptanceScope {
            organization_id: "org-1",
            workspace_id: "workspace-1",
            work_id: "work-1",
            implementation_session_id: "implementation-1",
        }
    }

    fn fixture() -> (
        CodingAcceptanceContract,
        CodingCompletionSubmission,
        Vec<CodingAcceptanceChildRecord>,
    ) {
        let contract = CodingAcceptanceContract {
            task_id: "task-1".into(),
            repository_id: "repo-1".into(),
            generation: 1,
            required_assertion_ids: vec!["user-flow".into()],
            require_review: true,
            require_behavior: true,
            readiness_requirements: vec!["test".into()],
            authorized_skips: vec![],
            authorized_dispositions: vec![],
            output_paths: vec![],
        };
        let assertion = |id: &str| CodingAssertionResult {
            assertion_id: id.into(),
            status: CodingVerificationStatus::Passed,
            evidence_refs: vec!["evidence-1".into()],
        };
        let report = |child: &str, assertions| CodingValidationReport {
            child_id: child.into(),
            session_id: format!("session-{child}"),
            revision: "a".repeat(40),
            status: CodingVerificationStatus::Passed,
            assertions,
            evidence_refs: vec!["transcript-1".into()],
        };
        let review = report("review-1", vec![assertion("user-flow")]);
        let behavior = report("behavior-1", vec![assertion("user-flow")]);
        let children = [
            (CodingValidationRole::Review, &review),
            (CodingValidationRole::Behavior, &behavior),
        ]
        .into_iter()
        .map(|(role, report)| CodingAcceptanceChildRecord {
            organization_id: "org-1".into(),
            workspace_id: "workspace-1".into(),
            work_id: "work-1".into(),
            parent_session_id: "implementation-1".into(),
            child_id: report.child_id.clone(),
            session_id: report.session_id.clone(),
            role,
            revision: report.revision.clone(),
            completed_successfully: true,
            report_digest: report.digest(),
        })
        .collect();
        let submission = CodingCompletionSubmission {
            task_id: contract.task_id.clone(),
            work_id: "work-1".into(),
            repository_id: contract.repository_id.clone(),
            contract_digest: contract.digest(),
            generation: 1,
            revision: "a".repeat(40),
            implementation_session_id: "implementation-1".into(),
            commands: vec![CodingCommandResult {
                command: "cargo test".into(),
                exit_code: Some(0),
                evidence_refs: vec!["command-1".into()],
            }],
            readiness: vec![assertion("test")],
            review: Some(review),
            behavior: Some(behavior),
            handoff_items: vec![],
            outputs: vec![],
        };
        (contract, submission, children)
    }

    #[test]
    fn coding_acceptance_accepts_exact_evidence_and_replay_is_identical() {
        let (contract, submission, children) = fixture();
        let decision =
            evaluate_coding_acceptance(&contract, Some(&submission), &scope(), &children);
        assert!(decision.accepted, "{:?}", decision.reasons);
        assert_eq!(
            decision,
            evaluate_coding_acceptance(&contract, Some(&submission), &scope(), &children)
        );
    }

    #[test]
    fn coding_acceptance_requires_submission_and_authoritative_child_proof() {
        let (contract, submission, _) = fixture();
        assert!(!evaluate_coding_acceptance(&contract, None, &scope(), &[]).accepted);
        let decision = evaluate_coding_acceptance(&contract, Some(&submission), &scope(), &[]);
        assert!(!decision.accepted);
        assert!(
            decision
                .reasons
                .iter()
                .any(|reason| reason.contains("authoritative"))
        );
    }

    #[test]
    fn coding_acceptance_rejects_an_empty_review_even_with_real_child_provenance() {
        let (contract, mut submission, mut children) = fixture();
        let review = submission.review.as_mut().unwrap();
        review.assertions.clear();
        children[0].report_digest = review.digest();
        let decision =
            evaluate_coding_acceptance(&contract, Some(&submission), &scope(), &children);
        assert!(!decision.accepted);
        assert!(
            decision
                .reasons
                .iter()
                .any(|reason| reason.contains("user-flow"))
        );
    }

    #[test]
    fn coding_acceptance_rejects_cross_tenant_and_cross_work_child_records() {
        let (contract, submission, children) = fixture();
        for field in ["organization", "workspace", "work", "parent", "digest"] {
            let mut forged = children.clone();
            match field {
                "organization" => forged[0].organization_id = "other".into(),
                "workspace" => forged[0].workspace_id = "other".into(),
                "work" => forged[0].work_id = "other".into(),
                "parent" => forged[0].parent_session_id = "other".into(),
                _ => forged[0].report_digest = "other".into(),
            }
            assert!(
                !evaluate_coding_acceptance(&contract, Some(&submission), &scope(), &forged)
                    .accepted,
                "{field}"
            );
        }
    }

    #[test]
    fn coding_acceptance_rejects_revision_drift_and_changed_contract() {
        let (mut contract, mut submission, children) = fixture();
        submission.revision = "b".repeat(40);
        assert!(
            !evaluate_coding_acceptance(&contract, Some(&submission), &scope(), &children).accepted
        );
        submission.revision = "a".repeat(40);
        contract.generation += 1;
        assert!(
            !evaluate_coding_acceptance(&contract, Some(&submission), &scope(), &children).accepted
        );
    }

    #[test]
    fn coding_acceptance_requires_owner_authorization_for_skips() {
        let (mut contract, mut submission, children) = fixture();
        submission.readiness[0].status = CodingVerificationStatus::Skipped;
        assert!(
            !evaluate_coding_acceptance(&contract, Some(&submission), &scope(), &children).accepted
        );
        contract.authorized_skips.push("test".into());
        submission.contract_digest = contract.digest();
        assert!(
            evaluate_coding_acceptance(&contract, Some(&submission), &scope(), &children).accepted
        );
    }

    #[test]
    fn coding_acceptance_unresolved_items_survive_until_authorized_or_resolved() {
        let (mut contract, mut submission, children) = fixture();
        submission.handoff_items.push(CodingHandoffItem {
            id: "auth-test".into(),
            disposition: CodingHandoffDisposition::Open,
            evidence_refs: vec!["issue-1".into()],
        });
        assert!(
            !evaluate_coding_acceptance(&contract, Some(&submission), &scope(), &children).accepted
        );
        submission.handoff_items[0].disposition = CodingHandoffDisposition::Dismissed;
        assert!(
            !evaluate_coding_acceptance(&contract, Some(&submission), &scope(), &children).accepted
        );
        contract.authorized_dispositions.push("auth-test".into());
        submission.contract_digest = contract.digest();
        assert!(
            evaluate_coding_acceptance(&contract, Some(&submission), &scope(), &children).accepted
        );
    }

    #[test]
    fn coding_acceptance_rejects_missing_readiness_failed_commands_and_self_review() {
        let (contract, submission, children) = fixture();
        let mut missing = submission.clone();
        missing.readiness.clear();
        assert!(
            !evaluate_coding_acceptance(&contract, Some(&missing), &scope(), &children).accepted
        );
        let mut failed = submission.clone();
        failed.commands[0].exit_code = Some(1);
        assert!(
            !evaluate_coding_acceptance(&contract, Some(&failed), &scope(), &children).accepted
        );
        let mut same = submission;
        same.review.as_mut().unwrap().session_id = "implementation-1".into();
        assert!(!evaluate_coding_acceptance(&contract, Some(&same), &scope(), &children).accepted);
    }

    #[test]
    fn coding_acceptance_contract_cannot_disable_verification_or_exceed_bounds() {
        let (mut contract, _, _) = fixture();
        contract.require_review = false;
        assert!(contract.validate().is_err());
        contract.require_review = true;
        contract.required_assertion_ids.clear();
        assert!(contract.validate().is_err());
        contract.required_assertion_ids.push("x".repeat(513));
        assert!(contract.validate().is_err());
    }

    #[test]
    fn coding_acceptance_rejects_readiness_the_runtime_cannot_execute() {
        let (mut contract, _, _) = fixture();
        contract.readiness_requirements = vec!["unknown-check".into()];
        assert!(contract.validate().is_err());
    }
    #[test]
    fn coding_output_manifest_is_exact_and_bounded() {
        let (mut contract, mut submission, children) = fixture();
        contract.output_paths = vec!["sum.py".into()];
        submission.contract_digest = contract.digest();
        let scope = CodingAcceptanceScope {
            organization_id: "org-1",
            workspace_id: "workspace-1",
            work_id: "work-1",
            implementation_session_id: "session-impl",
        };
        assert!(
            evaluate_coding_acceptance(&contract, Some(&submission), &scope, &children)
                .reasons
                .iter()
                .any(|r| r.contains("output manifest"))
        );
        submission.outputs = vec![CodingOutputFile {
            path: "other.py".into(),
            content: "pass\n".into(),
        }];
        assert!(
            evaluate_coding_acceptance(&contract, Some(&submission), &scope, &children)
                .reasons
                .iter()
                .any(|r| r.contains("output manifest"))
        );
        submission.outputs[0].path = "sum.py".into();
        assert!(
            !evaluate_coding_acceptance(&contract, Some(&submission), &scope, &children)
                .reasons
                .iter()
                .any(|r| r.contains("output manifest"))
        );
        submission.outputs[0].content = "x".repeat(65_537);
        assert!(
            evaluate_coding_acceptance(&contract, Some(&submission), &scope, &children)
                .reasons
                .iter()
                .any(|r| r.contains("output manifest"))
        );
    }
}
