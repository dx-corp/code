//! Typed task-Computer operations and references to their settled owner records.
//!
//! These values travel through the existing ToolExecution request/result. A
//! runtime copy is not authority: Platform compares it to the execution it
//! observed and checkpointed under its run lease before accepting completion.

use serde::{Deserialize, Serialize};

pub const CODING_WORKSPACE_SCHEMA: &str = "dex.computer_coding_workspace.v1";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(
    tag = "action",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum CodingWorkspaceRequest {
    Baseline {
        #[serde(default, rename = "idempotency_key")]
        idempotency_key: String,
    },
    Inspect {
        expected_base_revision: String,
    },
    Readiness {
        #[serde(default, rename = "idempotency_key")]
        idempotency_key: String,
        expected_revision: String,
        command: String,
        timeout_ms: u64,
    },
    Outputs {
        expected_base_revision: String,
        expected_revision: String,
        paths: Vec<String>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CodingWorkspaceBaselineKind {
    AcceptedSnapshot,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingWorkspaceResponse {
    pub schema: String,
    pub project_resource_id: String,
    pub accepted_snapshot_id: String,
    pub source_workspace_id: String,
    pub task_computer_id: String,
    pub run_id: String,
    pub tool_execution_id: String,
    pub owner_lease_epoch: u64,
    pub base_revision: String,
    pub revision: String,
    pub baseline_kind: CodingWorkspaceBaselineKind,
    pub clean: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness: Option<CodingWorkspaceReadiness>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<CodingWorkspaceOutput>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingWorkspaceReadiness {
    pub command: String,
    pub revision_before: String,
    pub revision_after: String,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub timed_out: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingWorkspaceOutput {
    pub path: String,
    pub sha256: String,
    pub content: String,
    pub git_blob_id: String,
}

/// References only. The worker resolves these against its own settled records.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ComputerCodingProof {
    pub baseline_execution_id: String,
    pub inspection_execution_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outputs_execution_id: Option<String>,
    pub readiness_execution_ids: Vec<String>,
}

fn idempotency_valid(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && !value
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
}

fn revision_valid(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn paths_valid(paths: &[String]) -> bool {
    !paths.is_empty()
        && paths.len() <= 8
        && paths
            .iter()
            .all(|path| super::valid_output_path(path) && path != ".dex-source.json")
        && paths
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            == paths.len()
}

impl CodingWorkspaceRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        let valid = match self {
            Self::Baseline { idempotency_key } => idempotency_valid(idempotency_key),
            Self::Inspect {
                expected_base_revision,
            } => revision_valid(expected_base_revision),
            Self::Readiness {
                idempotency_key,
                expected_revision,
                command,
                timeout_ms,
            } => {
                idempotency_valid(idempotency_key)
                    && revision_valid(expected_revision)
                    && !command.trim().is_empty()
                    && command.len() <= 16_384
                    && !command.contains('\0')
                    && (1..=120_000).contains(timeout_ms)
            }
            Self::Outputs {
                expected_base_revision,
                expected_revision,
                paths,
            } => {
                revision_valid(expected_base_revision)
                    && revision_valid(expected_revision)
                    && expected_base_revision != expected_revision
                    && paths_valid(paths)
            }
        };
        valid
            .then_some(())
            .ok_or("invalid coding workspace request")
    }
}

impl CodingWorkspaceResponse {
    pub fn validate(&self) -> Result<(), &'static str> {
        use sha2::{Digest, Sha256};
        if self.owner_lease_epoch == 0
            || self.schema != CODING_WORKSPACE_SCHEMA
            || [
                &self.project_resource_id,
                &self.accepted_snapshot_id,
                &self.source_workspace_id,
                &self.task_computer_id,
                &self.run_id,
                &self.tool_execution_id,
            ]
            .iter()
            .any(|value| {
                value.trim().is_empty() || value.len() > 1024 || value.chars().any(char::is_control)
            })
            || !revision_valid(&self.base_revision)
            || !revision_valid(&self.revision)
        {
            return Err("invalid coding workspace identity");
        }
        if let Some(readiness) = &self.readiness
            && (readiness.command.trim().is_empty()
                || readiness.command.len() > 16_384
                || !revision_valid(&readiness.revision_before)
                || !revision_valid(&readiness.revision_after)
                || readiness
                    .stdout
                    .len()
                    .saturating_add(readiness.stderr.len())
                    > 16_384)
        {
            return Err("invalid coding readiness result");
        }
        if !self.outputs.is_empty() {
            let paths: Vec<_> = self
                .outputs
                .iter()
                .map(|output| output.path.clone())
                .collect();
            if !paths_valid(&paths)
                || self
                    .outputs
                    .iter()
                    .map(|output| output.content.len())
                    .sum::<usize>()
                    > 16_384
            {
                return Err("invalid coding output manifest");
            }
            for output in &self.outputs {
                if output.content.is_empty()
                    || output.content.len() > 8192
                    || !revision_valid(&output.git_blob_id)
                    || output.sha256 != format!("{:x}", Sha256::digest(output.content.as_bytes()))
                {
                    return Err("invalid coding output content");
                }
            }
        }
        Ok(())
    }

    /// Validate the owner response shape against the operation actually admitted.
    /// Tenant/task/run equality remains the caller's owner-linkage check.
    pub fn validate_for(&self, request: &CodingWorkspaceRequest) -> Result<(), &'static str> {
        request.validate()?;
        self.validate()?;
        let valid = match request {
            CodingWorkspaceRequest::Baseline { .. } => {
                self.clean
                    && self.base_revision == self.revision
                    && self.outputs.is_empty()
                    && self.readiness.is_none()
            }
            CodingWorkspaceRequest::Inspect {
                expected_base_revision,
            } => {
                &self.base_revision == expected_base_revision
                    && self.outputs.is_empty()
                    && self.readiness.is_none()
            }
            CodingWorkspaceRequest::Readiness {
                expected_revision,
                command,
                ..
            } => {
                self.clean
                    && &self.revision == expected_revision
                    && self.outputs.is_empty()
                    && self.readiness.as_ref().is_some_and(|readiness| {
                        &readiness.command == command
                            && &readiness.revision_before == expected_revision
                            && &readiness.revision_after == expected_revision
                    })
            }
            CodingWorkspaceRequest::Outputs {
                expected_base_revision,
                expected_revision,
                paths,
            } => {
                self.clean
                    && &self.base_revision == expected_base_revision
                    && &self.revision == expected_revision
                    && self.readiness.is_none()
                    && self.outputs.len() == paths.len()
                    && self
                        .outputs
                        .iter()
                        .map(|output| &output.path)
                        .collect::<std::collections::BTreeSet<_>>()
                        == paths.iter().collect::<std::collections::BTreeSet<_>>()
            }
        };
        valid
            .then_some(())
            .ok_or("coding workspace result does not match admitted operation")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn response() -> CodingWorkspaceResponse {
        CodingWorkspaceResponse {
            schema: CODING_WORKSPACE_SCHEMA.into(),
            project_resource_id: "project".into(),
            accepted_snapshot_id: "snapshot".into(),
            source_workspace_id: "source".into(),
            task_computer_id: "task".into(),
            run_id: "run".into(),
            tool_execution_id: "execution".into(),
            owner_lease_epoch: 1,
            base_revision: "a".repeat(40),
            revision: "b".repeat(40),
            baseline_kind: CodingWorkspaceBaselineKind::AcceptedSnapshot,
            clean: true,
            readiness: None,
            outputs: vec![],
        }
    }
    #[test]
    fn coding_workspace_result_rejects_wrong_revision_content_and_shape() {
        let mut no_lease = response();
        no_lease.owner_lease_epoch = 0;
        assert!(no_lease.validate().is_err());
        use sha2::{Digest, Sha256};
        let request = CodingWorkspaceRequest::Outputs {
            expected_base_revision: "a".repeat(40),
            expected_revision: "b".repeat(40),
            paths: vec!["answer.py".into()],
        };
        let mut result = response();
        assert!(result.validate_for(&request).is_err());
        result.outputs.push(CodingWorkspaceOutput {
            path: "answer.py".into(),
            content: "answer\n".into(),
            sha256: format!("{:x}", Sha256::digest(b"answer\n")),
            git_blob_id: "c".repeat(40),
        });
        assert!(result.validate_for(&request).is_ok());
        let mut wrong = result.clone();
        wrong.revision = "d".repeat(40);
        assert!(wrong.validate_for(&request).is_err());
        let mut wrong = result.clone();
        wrong.outputs[0].content.push('!');
        assert!(wrong.validate_for(&request).is_err());
        let mut wrong = result.clone();
        wrong.outputs[0].path = "unexpected.py".into();
        assert!(wrong.validate_for(&request).is_err());
        let mut wrong = result.clone();
        wrong.clean = false;
        assert!(wrong.validate_for(&request).is_err());
        let mut wrong = result.clone();
        wrong.outputs.push(wrong.outputs[0].clone());
        assert!(wrong.validate_for(&request).is_err());
        let mut wrong = result.clone();
        wrong.outputs[0].content = "x".repeat(8193);
        assert!(wrong.validate_for(&request).is_err());
        let mut wrong = result;
        wrong.schema = "caller-observation".into();
        assert!(wrong.validate_for(&request).is_err());
        assert!(
            CodingWorkspaceRequest::Readiness {
                idempotency_key: "fixture".into(),
                expected_revision: "b".repeat(40),
                command: "test".into(),
                timeout_ms: 120001
            }
            .validate()
            .is_err()
        );
    }
    #[test]
    fn coding_workspace_mutations_require_owner_idempotency_before_validation() {
        let mut request: CodingWorkspaceRequest =
            serde_json::from_value(serde_json::json!({"action":"baseline"})).unwrap();
        assert!(request.validate().is_err());
        if let CodingWorkspaceRequest::Baseline { idempotency_key } = &mut request {
            *idempotency_key = "client-tool:owner-execution".into();
        }
        assert!(request.validate().is_ok());
        assert_eq!(
            serde_json::to_value(request).unwrap()["idempotency_key"],
            "client-tool:owner-execution"
        );
    }
    #[test]
    fn coding_workspace_requests_reject_caller_scope_and_unknown_fields() {
        assert!(
            serde_json::from_value::<CodingWorkspaceRequest>(
                serde_json::json!({"action":"baseline","taskComputerId":"forged"})
            )
            .is_err()
        );
        assert!(serde_json::from_value::<CodingWorkspaceRequest>(serde_json::json!({"action":"readiness","expectedRevision":"commit","command":"test","timeoutMs":5000,"cwd":"/host"})).is_err());
        let request: CodingWorkspaceRequest = serde_json::from_value(serde_json::json!({"action":"outputs","expectedBaseRevision":"base","expectedRevision":"commit","paths":["answer.py"]})).unwrap();
        assert!(
            matches!(request, CodingWorkspaceRequest::Outputs { expected_base_revision, .. } if expected_base_revision == "base")
        );
    }
}
