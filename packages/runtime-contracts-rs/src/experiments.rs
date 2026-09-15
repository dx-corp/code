//! Closed, content-free contracts for the opt-in native tool-profile trial.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub const EXPERIMENT_ID: &str = "native-tool-profile";
pub const EXPERIMENT_VERSION: u16 = 1;
pub const CONSENT_VERSION: u16 = 1;
pub const FOLLOW_UP_DAYS: u16 = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentArm {
    Control,
    Minimal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExperimentAssignment {
    pub experiment_id: String,
    pub experiment_version: u16,
    pub allocation_version: u16,
    pub consent_revision: u32,
    /// Domain-separated pseudonym scoped to this organization/workspace.
    pub unit_id: String,
    pub assignment_id: Uuid,
    pub arm: ExperimentArm,
    pub probability_bps: u16,
}

fn digest(parts: &[&str]) -> [u8; 32] {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    hash.finalize().into()
}

impl ExperimentAssignment {
    pub fn derive(seed: &str, organization: &str, workspace: &str, revision: u32) -> Self {
        let unit = digest(&["maestro.experiment.unit.v1", organization, workspace, seed]);
        let unit_id = unit
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let allocation = digest(&[
            "maestro.experiment.allocation.v1",
            EXPERIMENT_ID,
            "1",
            &unit_id,
        ]);
        let identity = digest(&[
            "maestro.experiment.assignment.v1",
            &unit_id,
            EXPERIMENT_ID,
            "1",
            &revision.to_string(),
        ]);
        let mut bytes = [0_u8; 16];
        bytes.copy_from_slice(&identity[..16]);
        Self {
            experiment_id: EXPERIMENT_ID.to_owned(),
            experiment_version: EXPERIMENT_VERSION,
            allocation_version: 1,
            consent_revision: revision,
            unit_id,
            assignment_id: Uuid::from_bytes(bytes),
            arm: if allocation[0] & 1 == 0 {
                ExperimentArm::Control
            } else {
                ExperimentArm::Minimal
            },
            probability_bps: 5000,
        }
    }

    pub fn is_valid(&self) -> bool {
        self.experiment_id == EXPERIMENT_ID
            && self.experiment_version == EXPERIMENT_VERSION
            && self.allocation_version == 1
            && self.consent_revision > 0
            && self.probability_bps == 5000
            && self.unit_id.len() == 64
            && self
                .unit_id
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            && !self.assignment_id.is_nil()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnrollmentType {
    ExperimentEnrollment,
}

/// Enrollment is content-free. Tenant scope comes only from authenticated ingress.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExperimentEnrollment {
    #[serde(rename = "type")]
    pub event_type: EnrollmentType,
    pub schema_version: u16,
    pub event_id: Uuid,
    pub timestamp: String,
    pub assignment: ExperimentAssignment,
    pub follow_up_days: u16,
}

impl ExperimentEnrollment {
    pub fn is_valid(&self) -> bool {
        self.schema_version == 1
            && self.assignment.is_valid()
            && self.event_id == self.assignment.assignment_id
            && self.follow_up_days == FOLLOW_UP_DAYS
            && !self.timestamp.is_empty()
            && self.timestamp.len() <= 40
    }
}

/// Assigned and locally applied do not imply a gateway-verified exposure.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExperimentObservation {
    pub assignment: ExperimentAssignment,
    pub locally_applied: bool,
    pub runtime_version: String,
}

impl ExperimentObservation {
    pub fn is_valid(&self) -> bool {
        self.assignment.is_valid()
            && !self.runtime_version.is_empty()
            && self.runtime_version.len() <= 64
            && self
                .runtime_version
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-+".contains(&b))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assignment_is_stable_scoped_and_revision_preserves_arm() {
        let first = ExperimentAssignment::derive("seed-a", "org-a", "workspace-a", 1);
        assert_eq!(
            first,
            ExperimentAssignment::derive("seed-a", "org-a", "workspace-a", 1)
        );
        assert_ne!(
            first.unit_id,
            ExperimentAssignment::derive("seed-a", "org-b", "workspace-a", 1).unit_id
        );
        let renewed = ExperimentAssignment::derive("seed-a", "org-a", "workspace-a", 2);
        assert_eq!(first.unit_id, renewed.unit_id);
        assert_eq!(first.arm, renewed.arm);
        assert_ne!(first.assignment_id, renewed.assignment_id);
        assert!(first.is_valid());
        assert_eq!(
            first.unit_id,
            "bbbc44a40840ef8bfe746c4f14f593227b21ced4f74138e0cc94e93510676d8b"
        );
        assert_eq!(
            first.assignment_id.to_string(),
            "2663318f-da85-4859-5b53-73beb7b62c37"
        );
    }

    #[test]
    fn assignment_schema_rejects_unreviewed_fields_and_allocations() {
        let assignment = ExperimentAssignment::derive("seed-a", "org-a", "workspace-a", 1);
        let mut wire = serde_json::to_value(&assignment).unwrap();
        wire["prompt"] = "private".into();
        assert!(serde_json::from_value::<ExperimentAssignment>(wire).is_err());
        let mut invalid = assignment;
        invalid.probability_bps = 10000;
        assert!(!invalid.is_valid());
    }
}
