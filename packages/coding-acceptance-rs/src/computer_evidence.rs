//! Completion checks over Computer results retained by the existing worker.

use super::computer::{CodingWorkspaceRequest, CodingWorkspaceResponse, ComputerCodingProof};
use super::{CodingAcceptanceContract, CodingCompletionSubmission};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const COMPUTER_CODING_EVIDENCE_KEY: &str = "codingComputerEvidence";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ComputerCodingFence {
    pub organization_id: String,
    pub workspace_id: String,
    pub actor_id: String,
    pub work_id: String,
    pub runtime_run_id: String,
    pub maestro_run_id: String,
    pub maestro_session_id: String,
    pub session_epoch: u64,
    pub runtime_generation: u64,
    pub task_id: String,
    pub task_computer_id: String,
    pub project_resource_id: String,
    pub accepted_snapshot_id: String,
    pub source_workspace_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ComputerCodingExecution {
    pub request: CodingWorkspaceRequest,
    pub response: CodingWorkspaceResponse,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ComputerCodingEvidence {
    pub fence: ComputerCodingFence,
    pub executions: Vec<ComputerCodingExecution>,
}

/// Resolve against owner records, never against execution IDs in model output.
pub fn validate_computer_completion(
    evidence: &ComputerCodingEvidence,
    proof: &ComputerCodingProof,
    contract: &CodingAcceptanceContract,
    submission: &CodingCompletionSubmission,
) -> Result<(), &'static str> {
    if evidence.executions.is_empty()
        || evidence.executions.len() > 32
        || submission.contract_digest != contract.digest()
        || submission.implementation_session_id != evidence.fence.maestro_session_id
        || submission.work_id != evidence.fence.work_id
    {
        return Err("coding Computer completion does not match its owner fence");
    }
    let mut records = BTreeMap::new();
    for execution in &evidence.executions {
        execution.response.validate_for(&execution.request)?;
        let response = &execution.response;
        if response.owner_lease_epoch != evidence.fence.runtime_generation
            || response.run_id != evidence.fence.maestro_run_id
            || response.task_computer_id != evidence.fence.task_computer_id
            || response.project_resource_id != evidence.fence.project_resource_id
            || response.accepted_snapshot_id != evidence.fence.accepted_snapshot_id
            || response.source_workspace_id != evidence.fence.source_workspace_id
            || records
                .insert(response.tool_execution_id.as_str(), execution)
                .is_some()
        {
            return Err("coding Computer execution does not match admitted source");
        }
    }
    let mut ids: Vec<&str> = vec![&proof.baseline_execution_id, &proof.inspection_execution_id];
    ids.extend(proof.outputs_execution_id.as_deref());
    ids.extend(proof.readiness_execution_ids.iter().map(String::as_str));
    if ids.iter().any(|id| id.is_empty()) || ids.iter().collect::<BTreeSet<_>>().len() != ids.len()
    {
        return Err("coding Computer proof repeats or omits execution identities");
    }
    let baseline = records
        .get(proof.baseline_execution_id.as_str())
        .ok_or("missing coding baseline execution")?;
    let inspection = records
        .get(proof.inspection_execution_id.as_str())
        .ok_or("missing coding inspection execution")?;
    if !matches!(baseline.request, CodingWorkspaceRequest::Baseline { .. })
        || !matches!(inspection.request, CodingWorkspaceRequest::Inspect { .. })
        || !inspection.response.clean
        || inspection.response.base_revision != baseline.response.base_revision
        || inspection.response.revision != submission.revision
    {
        return Err("coding Computer revision is not the inspected admitted revision");
    }
    if contract.output_paths.is_empty() {
        if proof.outputs_execution_id.is_some() || !submission.outputs.is_empty() {
            return Err("unrequested coding Computer output");
        }
    } else {
        let output_id = proof
            .outputs_execution_id
            .as_deref()
            .ok_or("missing coding output execution")?;
        let output = records
            .get(output_id)
            .ok_or("unknown coding output execution")?;
        if !matches!(output.request, CodingWorkspaceRequest::Outputs { .. })
            || output.response.base_revision != baseline.response.base_revision
            || output.response.revision != submission.revision
        {
            return Err("coding Computer output revision mismatch");
        }
        let captured: BTreeMap<_, _> = output
            .response
            .outputs
            .iter()
            .map(|file| (&file.path, &file.content))
            .collect();
        let submitted: BTreeMap<_, _> = submission
            .outputs
            .iter()
            .map(|file| (&file.path, &file.content))
            .collect();
        if captured != submitted
            || submitted.len() != submission.outputs.len()
            || captured.keys().copied().collect::<BTreeSet<_>>()
                != contract.output_paths.iter().collect::<BTreeSet<_>>()
        {
            return Err("coding Computer output bytes do not match the owner capture");
        }
    }
    if proof.readiness_execution_ids.len() != submission.commands.len()
        || submission.commands.is_empty()
    {
        return Err("coding Computer command proof is incomplete");
    }
    for (execution_id, command) in proof
        .readiness_execution_ids
        .iter()
        .zip(&submission.commands)
    {
        let execution = records
            .get(execution_id.as_str())
            .ok_or("unknown coding readiness execution")?;
        let readiness = execution
            .response
            .readiness
            .as_ref()
            .ok_or("missing coding readiness result")?;
        if !matches!(execution.request, CodingWorkspaceRequest::Readiness { .. })
            || execution.response.base_revision != baseline.response.base_revision
            || execution.response.revision != submission.revision
            || readiness.command != command.command
            || readiness.exit_code != 0
            || readiness.timed_out
            || readiness.truncated
            || command.exit_code != Some(0)
            || !command
                .evidence_refs
                .contains(&format!("receipt:tool-execution:{execution_id}"))
        {
            return Err("coding Computer readiness is not a successful owner execution");
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingValidatorReservation {
    pub tool_execution_id: String,
    pub role: super::CodingValidationRole,
    pub inspection_execution_id: String,
    pub readiness_execution_ids: Vec<String>,
    pub contract_digest: String,
    pub revision: String,
    pub parent_session_id: String,
    pub runtime_run_id: String,
    pub session_epoch: u64,
    pub runtime_generation: u64,
}

impl ComputerCodingEvidence {
    /// Preserve an owner's first result across response loss and lease reclaim.
    pub fn record(&mut self, execution: ComputerCodingExecution) -> Result<bool, &'static str> {
        execution.response.validate_for(&execution.request)?;
        let response = &execution.response;
        if response.owner_lease_epoch != self.fence.runtime_generation
            || response.run_id != self.fence.maestro_run_id
            || response.task_computer_id != self.fence.task_computer_id
            || response.project_resource_id != self.fence.project_resource_id
            || response.accepted_snapshot_id != self.fence.accepted_snapshot_id
            || response.source_workspace_id != self.fence.source_workspace_id
        {
            return Err("coding Computer result belongs to a different task or source");
        }
        if let Some(existing) = self
            .executions
            .iter()
            .find(|old| old.response.tool_execution_id == response.tool_execution_id)
        {
            return if existing == &execution {
                Ok(false)
            } else {
                Err("coding Computer execution replay conflicts")
            };
        }
        if self.executions.len() >= 32 {
            return Err("coding Computer evidence limit reached");
        }
        self.executions.push(execution);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::super::computer::{
        CODING_WORKSPACE_SCHEMA, CodingWorkspaceBaselineKind, CodingWorkspaceOutput,
        CodingWorkspaceReadiness,
    };
    use super::*;
    use sha2::{Digest, Sha256};

    fn fixture() -> (
        ComputerCodingEvidence,
        ComputerCodingProof,
        CodingAcceptanceContract,
        CodingCompletionSubmission,
    ) {
        let (mut contract, mut submission, _) = super::super::tests::fixture();
        contract.output_paths = vec!["answer.py".into()];
        submission.contract_digest = contract.digest();
        submission.outputs = vec![super::super::CodingOutputFile {
            path: "answer.py".into(),
            content: "answer\n".into(),
        }];
        submission.commands[0].evidence_refs = vec!["receipt:tool-execution:ready".into()];
        let base = "b".repeat(40);
        let revision = submission.revision.clone();
        let response = |id: &str| CodingWorkspaceResponse {
            owner_lease_epoch: 1,
            schema: CODING_WORKSPACE_SCHEMA.into(),
            project_resource_id: "project".into(),
            accepted_snapshot_id: "snapshot".into(),
            source_workspace_id: "source".into(),
            task_computer_id: "computer".into(),
            run_id: "maestro-run".into(),
            tool_execution_id: id.into(),
            base_revision: base.clone(),
            revision: revision.clone(),
            baseline_kind: CodingWorkspaceBaselineKind::AcceptedSnapshot,
            clean: true,
            readiness: None,
            outputs: vec![],
        };
        let mut baseline = response("baseline");
        baseline.revision = base.clone();
        let mut readiness = response("ready");
        readiness.readiness = Some(CodingWorkspaceReadiness {
            command: "cargo test".into(),
            revision_before: revision.clone(),
            revision_after: revision.clone(),
            exit_code: 0,
            stdout: "passed".into(),
            stderr: String::new(),
            truncated: false,
            timed_out: false,
        });
        let mut outputs = response("outputs");
        outputs.outputs = vec![CodingWorkspaceOutput {
            path: "answer.py".into(),
            content: "answer\n".into(),
            sha256: format!("{:x}", Sha256::digest(b"answer\n")),
            git_blob_id: "c".repeat(40),
        }];
        let evidence = ComputerCodingEvidence {
            fence: ComputerCodingFence {
                organization_id: "org-1".into(),
                workspace_id: "workspace-1".into(),
                actor_id: "actor".into(),
                work_id: submission.work_id.clone(),
                runtime_run_id: "runtime".into(),
                maestro_run_id: "maestro-run".into(),
                maestro_session_id: submission.implementation_session_id.clone(),
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
                        idempotency_key: "baseline-key".into(),
                    },
                    response: baseline,
                },
                ComputerCodingExecution {
                    request: CodingWorkspaceRequest::Inspect {
                        expected_base_revision: base.clone(),
                    },
                    response: response("inspection"),
                },
                ComputerCodingExecution {
                    request: CodingWorkspaceRequest::Readiness {
                        idempotency_key: "ready-key".into(),
                        expected_revision: revision.clone(),
                        command: "cargo test".into(),
                        timeout_ms: 5000,
                    },
                    response: readiness,
                },
                ComputerCodingExecution {
                    request: CodingWorkspaceRequest::Outputs {
                        expected_base_revision: base,
                        expected_revision: revision,
                        paths: vec!["answer.py".into()],
                    },
                    response: outputs,
                },
            ],
        };
        let proof = ComputerCodingProof {
            baseline_execution_id: "baseline".into(),
            inspection_execution_id: "inspection".into(),
            outputs_execution_id: Some("outputs".into()),
            readiness_execution_ids: vec!["ready".into()],
        };
        (evidence, proof, contract, submission)
    }

    #[test]
    fn computer_completion_requires_exact_owner_bytes_commands_and_scope() {
        let (evidence, proof, contract, submission) = fixture();
        assert!(validate_computer_completion(&evidence, &proof, &contract, &submission).is_ok());
        let mut wrong = submission.clone();
        wrong.outputs[0].content = "forged\n".into();
        assert!(validate_computer_completion(&evidence, &proof, &contract, &wrong).is_err());
        let mut wrong = submission.clone();
        wrong.commands[0].evidence_refs = vec!["unrelated-receipt".into()];
        assert!(validate_computer_completion(&evidence, &proof, &contract, &wrong).is_err());
        let mut wrong = submission.clone();
        wrong.implementation_session_id = "other-session".into();
        assert!(validate_computer_completion(&evidence, &proof, &contract, &wrong).is_err());
        let mut wrong = evidence.clone();
        wrong.executions[3].response.accepted_snapshot_id = "other-snapshot".into();
        assert!(validate_computer_completion(&wrong, &proof, &contract, &submission).is_err());
        let mut wrong = evidence.clone();
        wrong.executions[2]
            .response
            .readiness
            .as_mut()
            .unwrap()
            .timed_out = true;
        assert!(validate_computer_completion(&wrong, &proof, &contract, &submission).is_err());
        let mut wrong = evidence.clone();
        wrong.executions[0].response.owner_lease_epoch += 1;
        assert!(validate_computer_completion(&wrong, &proof, &contract, &submission).is_err());
        let mut wrong = proof.clone();
        wrong.readiness_execution_ids.clear();
        assert!(validate_computer_completion(&evidence, &wrong, &contract, &submission).is_err());
    }

    #[test]
    fn computer_owner_evidence_replay_cannot_replace_first_result() {
        let (mut evidence, _, _, _) = fixture();
        let last = evidence.executions.last().unwrap().clone();
        assert!(!evidence.record(last.clone()).unwrap());
        let mut wrong = last;
        wrong.response.outputs[0].git_blob_id = "d".repeat(40);
        assert!(evidence.record(wrong).is_err());
    }
}
