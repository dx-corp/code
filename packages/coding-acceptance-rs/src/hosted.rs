//! Pure hosted coding_task projection. The worker supplies owner-checkpointed
//! facts and persists returned transitions; this module performs no effects.

use super::{computer::*, computer_evidence::*, *};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostedCodingAction {
    Begin {
        mission_id: String,
        work_id: String,
        contract: CodingAcceptanceContract,
    },
    Readiness {
        #[serde(default)]
        requirement_id: Option<String>,
        #[serde(default)]
        execution_id: Option<String>,
    },
    Validate {
        role: CodingValidationRole,
    },
    Status {},
    Handoff {
        item: CodingHandoffItem,
    },
    Complete {},
}

pub struct HostedCodingContext<'a> {
    pub contract: &'a CodingAcceptanceContract,
    pub work_id: &'a str,
    pub session_id: &'a str,
    pub evidence: &'a ComputerCodingEvidence,
    pub readiness_bindings: &'a BTreeMap<String, String>,
    pub children: &'a [(CodingAcceptanceChildRecord, CodingValidationReport)],
    pub handoff_items: &'a [CodingHandoffItem],
}

pub enum HostedCodingProjection {
    Response(Value),
    ReadinessBinding {
        requirement_id: String,
        execution_id: String,
    },
    Validate(CodingValidationRole),
    Handoff(CodingHandoffItem),
    Complete(Box<CodingCompletionSubmission>),
}

/// Hosted metadata exposes the same coding_task actions as native Code. Its
/// readiness binding refers to an already settled Computer result, not caller
/// status, exit codes, output bytes, or fabricated validator records.
pub fn hosted_coding_task_schema() -> Value {
    json!({"type":"object","oneOf":[
        {"type":"object","properties":{"action":{"const":"begin"},"mission_id":{"type":"string","minLength":1},"work_id":{"type":"string","minLength":1},"contract":{"type":"object"}},"required":["action","mission_id","work_id","contract"],"additionalProperties":false},
        {"type":"object","properties":{"action":{"const":"readiness"},"requirement_id":{"type":"string","enum":["build","test","start","authentication","observation"]},"execution_id":{"type":"string","minLength":1}},"required":["action"],"additionalProperties":false},
        {"type":"object","properties":{"action":{"const":"validate"},"role":{"enum":["review","behavior"]}},"required":["action","role"],"additionalProperties":false},
        {"type":"object","properties":{"action":{"enum":["status","complete"]}},"required":["action"],"additionalProperties":false},
        {"type":"object","properties":{"action":{"const":"handoff"},"item":{"type":"object"}},"required":["action","item"],"additionalProperties":false}
    ]})
}

fn inspection<'a>(
    context: &'a HostedCodingContext<'_>,
) -> Result<&'a ComputerCodingExecution, &'static str> {
    context
        .evidence
        .executions
        .iter()
        .rev()
        .find(|execution| matches!(execution.request, CodingWorkspaceRequest::Inspect { .. }))
        .filter(|execution| execution.response.clean)
        .ok_or("coding_task requires a clean owner Computer inspection")
}

fn readiness_execution<'a>(
    context: &'a HostedCodingContext<'_>,
    id: &str,
    revision: &str,
) -> Result<&'a ComputerCodingExecution, &'static str> {
    let record = context
        .evidence
        .executions
        .iter()
        .find(|e| e.response.tool_execution_id == id)
        .ok_or("unknown owner readiness execution")?;
    record.response.validate_for(&record.request)?;
    let readiness = record
        .response
        .readiness
        .as_ref()
        .ok_or("execution is not readiness")?;
    if !matches!(record.request, CodingWorkspaceRequest::Readiness { .. })
        || record.response.revision != revision
        || !record.response.clean
        || readiness.exit_code != 0
        || readiness.timed_out
        || readiness.truncated
    {
        return Err("readiness did not succeed at the inspected revision");
    }
    Ok(record)
}

pub fn project_hosted_coding_action(
    action: HostedCodingAction,
    context: &HostedCodingContext<'_>,
) -> Result<HostedCodingProjection, &'static str> {
    context.contract.validate()?;
    if context.work_id != context.evidence.fence.work_id
        || context.session_id != context.evidence.fence.maestro_session_id
    {
        return Err("coding_task owner scope does not match this session and work");
    }
    match action {
        HostedCodingAction::Begin {
            mission_id,
            work_id,
            contract,
        } => {
            if mission_id.trim().is_empty()
                || work_id != context.work_id
                || contract != *context.contract
            {
                return Err("coding_task begin does not match the admitted work contract");
            }
            let baseline = context
                .evidence
                .executions
                .iter()
                .find(|e| matches!(e.request, CodingWorkspaceRequest::Baseline { .. }))
                .ok_or("coding_task begin requires an owner Computer baseline")?;
            baseline.response.validate_for(&baseline.request)?;
            Ok(HostedCodingProjection::Response(
                json!({"missionId":mission_id,"workId":work_id,"contract":contract,"baseRevision":baseline.response.base_revision,"repositoryId":baseline.response.project_resource_id}),
            ))
        }
        HostedCodingAction::Readiness {
            requirement_id: Some(requirement_id),
            execution_id: Some(execution_id),
        } => {
            if !context
                .contract
                .readiness_requirements
                .contains(&requirement_id)
            {
                return Err("readiness category is not admitted");
            }
            readiness_execution(
                context,
                &execution_id,
                &inspection(context)?.response.revision,
            )?;
            Ok(HostedCodingProjection::ReadinessBinding {
                requirement_id,
                execution_id,
            })
        }
        HostedCodingAction::Readiness {
            requirement_id: None,
            execution_id: None,
        }
        | HostedCodingAction::Status {} => {
            let inspected = inspection(context)?;
            let missing: Vec<_> = context
                .contract
                .readiness_requirements
                .iter()
                .filter(|id| {
                    context.readiness_bindings.get(*id).is_none_or(|execution| {
                        readiness_execution(context, execution, &inspected.response.revision)
                            .is_err()
                    })
                })
                .collect();
            Ok(HostedCodingProjection::Response(
                json!({"contract":context.contract,"revision":inspected.response.revision,"readinessBindings":context.readiness_bindings,"missingReadiness":missing,"handoffItems":context.handoff_items}),
            ))
        }
        HostedCodingAction::Readiness { .. } => {
            Err("readiness requires both requirement_id and execution_id")
        }
        HostedCodingAction::Validate { role } => {
            let revision = &inspection(context)?.response.revision;
            for requirement in &context.contract.readiness_requirements {
                readiness_execution(
                    context,
                    context
                        .readiness_bindings
                        .get(requirement)
                        .ok_or("required readiness has no owner execution")?,
                    revision,
                )?;
            }
            Ok(HostedCodingProjection::Validate(role))
        }
        HostedCodingAction::Handoff { item } => {
            if item.id.is_empty()
                || item.evidence_refs.is_empty()
                || (matches!(
                    item.disposition,
                    CodingHandoffDisposition::Deferred | CodingHandoffDisposition::Dismissed
                ) && !context.contract.authorized_dispositions.contains(&item.id))
            {
                return Err("handoff item is missing evidence or an authorized disposition");
            }
            Ok(HostedCodingProjection::Handoff(item))
        }
        HostedCodingAction::Complete {} => complete(context)
            .map(Box::new)
            .map(HostedCodingProjection::Complete),
    }
}

fn complete(context: &HostedCodingContext<'_>) -> Result<CodingCompletionSubmission, &'static str> {
    let inspected = inspection(context)?;
    let revision = &inspected.response.revision;
    let baseline = context
        .evidence
        .executions
        .iter()
        .find(|e| matches!(e.request, CodingWorkspaceRequest::Baseline { .. }))
        .ok_or("missing owner baseline")?;
    let mut commands = Vec::new();
    let mut readiness = Vec::new();
    let mut readiness_execution_ids = Vec::new();
    for requirement in &context.contract.readiness_requirements {
        let id = context
            .readiness_bindings
            .get(requirement)
            .ok_or("required readiness has no owner execution")?;
        let record = readiness_execution(context, id, revision)?;
        let evidence_refs = vec![format!("receipt:tool-execution:{id}")];
        readiness.push(CodingAssertionResult {
            assertion_id: requirement.clone(),
            status: CodingVerificationStatus::Passed,
            evidence_refs: evidence_refs.clone(),
        });
        if !readiness_execution_ids.contains(id) {
            readiness_execution_ids.push(id.clone());
            commands.push(CodingCommandResult {
                command: record
                    .response
                    .readiness
                    .as_ref()
                    .ok_or("missing readiness")?
                    .command
                    .clone(),
                exit_code: Some(0),
                evidence_refs,
            });
        }
    }
    let child = |role| {
        context
            .children
            .iter()
            .rev()
            .find(|(record, report)| {
                record.role == role
                    && record.revision == *revision
                    && record.parent_session_id == context.session_id
                    && record.report_digest == report.digest()
            })
            .map(|(_, report)| report.clone())
            .ok_or("missing actual independent validator completion")
    };
    let output = if context.contract.output_paths.is_empty() {
        None
    } else {
        Some(
            context
                .evidence
                .executions
                .iter()
                .rev()
                .find(|e| {
                    matches!(e.request, CodingWorkspaceRequest::Outputs { .. })
                        && e.response.revision == *revision
                })
                .ok_or("missing immutable output capture")?,
        )
    };
    let proof = ComputerCodingProof {
        baseline_execution_id: baseline.response.tool_execution_id.clone(),
        inspection_execution_id: inspected.response.tool_execution_id.clone(),
        outputs_execution_id: output.map(|e| e.response.tool_execution_id.clone()),
        readiness_execution_ids,
    };
    let submission = CodingCompletionSubmission {
        task_id: context.contract.task_id.clone(),
        work_id: context.work_id.to_owned(),
        repository_id: context.contract.repository_id.clone(),
        contract_digest: context.contract.digest(),
        generation: context.contract.generation,
        revision: revision.clone(),
        implementation_session_id: context.session_id.to_owned(),
        commands,
        readiness,
        review: Some(child(CodingValidationRole::Review)?),
        behavior: Some(child(CodingValidationRole::Behavior)?),
        handoff_items: context.handoff_items.to_vec(),
        outputs: output
            .into_iter()
            .flat_map(|e| {
                e.response.outputs.iter().map(|file| CodingOutputFile {
                    path: file.path.clone(),
                    content: file.content.clone(),
                })
            })
            .collect(),
        computer_proof: Some(Box::new(proof.clone())),
    };
    validate_computer_completion(context.evidence, &proof, context.contract, &submission)?;
    let records: Vec<_> = context
        .children
        .iter()
        .map(|(record, _)| record.clone())
        .collect();
    let decision = evaluate_coding_acceptance(
        context.contract,
        Some(&submission),
        &CodingAcceptanceScope {
            organization_id: &context.evidence.fence.organization_id,
            workspace_id: &context.evidence.fence.workspace_id,
            work_id: context.work_id,
            implementation_session_id: context.session_id,
        },
        &records,
    );
    if !decision.accepted {
        return Err("coding completion was not accepted by the existing contract evaluator");
    }
    Ok(submission)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn contract() -> CodingAcceptanceContract {
        serde_json::from_value(json!({"taskId":"task","repositoryId":"project","generation":1,
            "requiredAssertionIds":["works"],"requireReview":true,"requireBehavior":true,"readinessRequirements":["test"]})).unwrap()
    }
    fn evidence() -> ComputerCodingEvidence {
        let base = "a".repeat(40);
        let revision = "b".repeat(40);
        let response = CodingWorkspaceResponse {
            owner_lease_epoch: 1,
            schema: CODING_WORKSPACE_SCHEMA.into(),
            project_resource_id: "project".into(),
            accepted_snapshot_id: "snapshot".into(),
            source_workspace_id: "source".into(),
            task_computer_id: "computer".into(),
            run_id: "run".into(),
            tool_execution_id: "baseline".into(),
            base_revision: base.clone(),
            revision: base.clone(),
            baseline_kind: CodingWorkspaceBaselineKind::AcceptedSnapshot,
            clean: true,
            readiness: None,
            outputs: vec![],
        };
        let mut inspected = response.clone();
        inspected.tool_execution_id = "inspect".into();
        inspected.revision = revision.clone();
        let mut ready = inspected.clone();
        ready.tool_execution_id = "ready".into();
        ready.readiness = Some(CodingWorkspaceReadiness {
            command: "test".into(),
            revision_before: revision.clone(),
            revision_after: revision.clone(),
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
            truncated: false,
            timed_out: false,
        });
        ComputerCodingEvidence {
            fence: ComputerCodingFence {
                organization_id: "org".into(),
                workspace_id: "workspace".into(),
                actor_id: "actor".into(),
                work_id: "work".into(),
                runtime_run_id: "runtime".into(),
                maestro_run_id: "run".into(),
                maestro_session_id: "parent".into(),
                session_epoch: 1,
                runtime_generation: 1,
                task_id: "task".into(),
                task_computer_id: "computer".into(),
                project_resource_id: "project".into(),
                accepted_snapshot_id: "snapshot".into(),
                source_workspace_id: "source".into(),
            },
            executions: vec![
                ComputerCodingExecution {
                    request: CodingWorkspaceRequest::Baseline {
                        idempotency_key: "baseline".into(),
                    },
                    response,
                },
                ComputerCodingExecution {
                    request: CodingWorkspaceRequest::Inspect {
                        expected_base_revision: base,
                    },
                    response: inspected,
                },
                ComputerCodingExecution {
                    request: CodingWorkspaceRequest::Readiness {
                        idempotency_key: "ready".into(),
                        expected_revision: revision,
                        command: "test".into(),
                        timeout_ms: 1000,
                    },
                    response: ready,
                },
            ],
        }
    }
    fn children() -> Vec<(CodingAcceptanceChildRecord, CodingValidationReport)> {
        [
            (CodingValidationRole::Review, "review"),
            (CodingValidationRole::Behavior, "behavior"),
        ]
        .into_iter()
        .map(|(role, id)| {
            let report = CodingValidationReport {
                child_id: id.into(),
                session_id: format!("session-{id}"),
                revision: "b".repeat(40),
                status: CodingVerificationStatus::Passed,
                assertions: vec![CodingAssertionResult {
                    assertion_id: "works".into(),
                    status: CodingVerificationStatus::Passed,
                    evidence_refs: vec!["observed".into()],
                }],
                evidence_refs: vec!["observed".into()],
            };
            let record = CodingAcceptanceChildRecord {
                organization_id: "org".into(),
                workspace_id: "workspace".into(),
                work_id: "work".into(),
                parent_session_id: "parent".into(),
                child_id: id.into(),
                session_id: report.session_id.clone(),
                role,
                revision: report.revision.clone(),
                completed_successfully: true,
                report_digest: report.digest(),
            };
            (record, report)
        })
        .collect()
    }
    #[test]
    fn hosted_completion_projects_only_owner_results_and_child_records() {
        let contract = contract();
        let evidence = evidence();
        let bindings = BTreeMap::from([("test".into(), "ready".into())]);
        let children = children();
        let context = HostedCodingContext {
            contract: &contract,
            work_id: "work",
            session_id: "parent",
            evidence: &evidence,
            readiness_bindings: &bindings,
            children: &children,
            handoff_items: &[],
        };
        let HostedCodingProjection::Complete(result) =
            project_hosted_coding_action(HostedCodingAction::Complete {}, &context).unwrap()
        else {
            panic!("completion expected")
        };
        assert_eq!(result.readiness[0].status, CodingVerificationStatus::Passed);
        assert_eq!(
            result.commands[0].evidence_refs,
            vec!["receipt:tool-execution:ready"]
        );
        assert_eq!(
            result.computer_proof.unwrap().inspection_execution_id,
            "inspect"
        );
    }
    #[test]
    fn no_caller_status_exit_or_validator_record_is_accepted() {
        for value in [
            json!({"action":"complete","review":{"childId":"forged"}}),
            json!({"action":"readiness","requirement_id":"test","execution_id":"ready","status":"passed"}),
            json!({"action":"readiness","exit_code":0}),
        ] {
            assert!(serde_json::from_value::<HostedCodingAction>(value).is_err());
        }
    }
    #[test]
    fn missing_children_failed_readiness_and_wrong_session_fail_closed() {
        let contract = contract();
        let bindings = BTreeMap::from([("test".into(), "ready".into())]);
        let children = children();
        for scenario in 0..3 {
            let mut evidence = evidence();
            if scenario == 1 {
                evidence.executions[2]
                    .response
                    .readiness
                    .as_mut()
                    .unwrap()
                    .exit_code = 1;
            }
            let context = HostedCodingContext {
                contract: &contract,
                work_id: "work",
                session_id: if scenario == 2 { "other" } else { "parent" },
                evidence: &evidence,
                readiness_bindings: &bindings,
                children: if scenario == 0 { &[] } else { &children },
                handoff_items: &[],
            };
            assert!(
                project_hosted_coding_action(HostedCodingAction::Complete {}, &context).is_err()
            );
        }
    }
    #[test]
    fn readiness_binding_must_name_an_admitted_category_and_real_execution() {
        let contract = contract();
        let evidence = evidence();
        let bindings = BTreeMap::new();
        let context = HostedCodingContext {
            contract: &contract,
            work_id: "work",
            session_id: "parent",
            evidence: &evidence,
            readiness_bindings: &bindings,
            children: &[],
            handoff_items: &[],
        };
        assert!(
            project_hosted_coding_action(
                HostedCodingAction::Readiness {
                    requirement_id: Some("test".into()),
                    execution_id: Some("ready".into())
                },
                &context
            )
            .is_ok()
        );
        for (requirement, id) in [("test", "inspect"), ("test", "forged"), ("build", "ready")] {
            assert!(
                project_hosted_coding_action(
                    HostedCodingAction::Readiness {
                        requirement_id: Some(requirement.into()),
                        execution_id: Some(id.into())
                    },
                    &context
                )
                .is_err()
            );
        }
    }
}
