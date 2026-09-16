//! Durable, runtime-local tool-operation lifecycle values.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ExecutionReceipt;

pub const TOOL_OPERATION_CUSTOM_TYPE: &str = "runtime_tool_operation_v1";
pub const MAX_TOOL_OPERATION_PROGRESS_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOperationPhase {
    Planned,
    EffectPending,
    OutcomeReady,
    Completed,
}

impl ToolOperationPhase {
    const fn next(self) -> Option<Self> {
        match self {
            Self::Planned => Some(Self::EffectPending),
            Self::EffectPending => Some(Self::OutcomeReady),
            Self::OutcomeReady => Some(Self::Completed),
            Self::Completed => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolReplayPolicy {
    Safe,
    Never,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolOperationOutcome {
    pub content: String,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<ExecutionReceipt>,
}

impl ToolOperationOutcome {
    #[must_use]
    pub fn new(
        content: impl Into<String>,
        is_error: bool,
        receipt: Option<ExecutionReceipt>,
    ) -> Self {
        Self {
            content: content.into(),
            is_error,
            receipt,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolOperationRecord {
    pub call_id: String,
    pub tool_name: String,
    pub admitted_arguments: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    pub replay_policy: ToolReplayPolicy,
    pub phase: ToolOperationPhase,
    pub planned_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<ToolOperationOutcome>,
}

impl ToolOperationRecord {
    pub fn planned(
        call_id: impl Into<String>,
        tool_name: impl Into<String>,
        admitted_arguments: Value,
        idempotency_key: Option<String>,
        replay_policy: ToolReplayPolicy,
        timestamp_ms: u64,
    ) -> Result<Self, ToolOperationError> {
        let record = Self {
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            admitted_arguments,
            idempotency_key,
            replay_policy,
            phase: ToolOperationPhase::Planned,
            planned_at_ms: timestamp_ms,
            updated_at_ms: timestamp_ms,
            progress: None,
            outcome: None,
        };
        record.validate()?;
        Ok(record)
    }

    pub fn effect_pending(mut self, timestamp_ms: u64) -> Result<Self, ToolOperationError> {
        self.advance(ToolOperationPhase::EffectPending, timestamp_ms)?;
        Ok(self)
    }

    pub fn outcome_ready(
        mut self,
        outcome: ToolOperationOutcome,
        timestamp_ms: u64,
    ) -> Result<Self, ToolOperationError> {
        if self.phase.next() != Some(ToolOperationPhase::OutcomeReady) {
            return Err(ToolOperationError::InvalidTransition {
                call_id: self.call_id,
                from: self.phase,
                to: ToolOperationPhase::OutcomeReady,
            });
        }
        self.set_updated_at(timestamp_ms)?;
        self.phase = ToolOperationPhase::OutcomeReady;
        self.progress = None;
        self.outcome = Some(outcome);
        self.validate()?;
        Ok(self)
    }

    pub fn completed(mut self, timestamp_ms: u64) -> Result<Self, ToolOperationError> {
        self.advance(ToolOperationPhase::Completed, timestamp_ms)?;
        Ok(self)
    }

    pub fn with_progress(
        mut self,
        progress: Value,
        timestamp_ms: u64,
    ) -> Result<Self, ToolOperationError> {
        let bytes = serde_json::to_vec(&progress)
            .map_err(|error| ToolOperationError::InvalidRecord(error.to_string()))?
            .len();
        if bytes > MAX_TOOL_OPERATION_PROGRESS_BYTES {
            return Err(ToolOperationError::ProgressTooLarge {
                call_id: self.call_id,
                bytes,
            });
        }
        if !matches!(self.phase, ToolOperationPhase::EffectPending) {
            return Err(ToolOperationError::InvalidRecord(
                "progress is only valid while an effect is pending".into(),
            ));
        }
        self.set_updated_at(timestamp_ms)?;
        self.progress = Some(progress);
        Ok(self)
    }

    fn advance(
        &mut self,
        phase: ToolOperationPhase,
        timestamp_ms: u64,
    ) -> Result<(), ToolOperationError> {
        if self.phase.next() != Some(phase) {
            return Err(ToolOperationError::InvalidTransition {
                call_id: self.call_id.clone(),
                from: self.phase,
                to: phase,
            });
        }
        self.set_updated_at(timestamp_ms)?;
        self.phase = phase;
        if phase != ToolOperationPhase::EffectPending {
            self.progress = None;
        }
        self.validate()
    }

    fn set_updated_at(&mut self, timestamp_ms: u64) -> Result<(), ToolOperationError> {
        if timestamp_ms < self.updated_at_ms {
            return Err(ToolOperationError::TimestampRegression {
                call_id: self.call_id.clone(),
            });
        }
        self.updated_at_ms = timestamp_ms;
        Ok(())
    }

    fn validate(&self) -> Result<(), ToolOperationError> {
        if self.call_id.trim().is_empty() || self.tool_name.trim().is_empty() {
            return Err(ToolOperationError::InvalidRecord(
                "callId and toolName must not be empty".into(),
            ));
        }
        if self
            .idempotency_key
            .as_ref()
            .is_some_and(|key| key.trim().is_empty())
        {
            return Err(ToolOperationError::InvalidRecord(
                "idempotencyKey must not be empty when present".into(),
            ));
        }
        if self.updated_at_ms < self.planned_at_ms {
            return Err(ToolOperationError::TimestampRegression {
                call_id: self.call_id.clone(),
            });
        }
        if let Some(progress) = &self.progress {
            let bytes = serde_json::to_vec(progress)
                .map_err(|error| ToolOperationError::InvalidRecord(error.to_string()))?
                .len();
            if bytes > MAX_TOOL_OPERATION_PROGRESS_BYTES {
                return Err(ToolOperationError::ProgressTooLarge {
                    call_id: self.call_id.clone(),
                    bytes,
                });
            }
            if self.phase != ToolOperationPhase::EffectPending {
                return Err(ToolOperationError::InvalidRecord(
                    "progress is only valid while an effect is pending".into(),
                ));
            }
        }
        let outcome_required = matches!(
            self.phase,
            ToolOperationPhase::OutcomeReady | ToolOperationPhase::Completed
        );
        if outcome_required != self.outcome.is_some() {
            return Err(ToolOperationError::InvalidRecord(format!(
                "phase {:?} has invalid outcome presence",
                self.phase
            )));
        }
        Ok(())
    }

    #[cfg(test)]
    fn with_phase_for_test(mut self, phase: ToolOperationPhase, timestamp_ms: u64) -> Self {
        self.phase = phase;
        self.updated_at_ms = timestamp_ms;
        if matches!(
            phase,
            ToolOperationPhase::OutcomeReady | ToolOperationPhase::Completed
        ) {
            self.outcome = Some(ToolOperationOutcome::new("test", false, None));
        }
        self
    }
}

#[derive(Clone, Debug)]
pub enum ToolOperationRecovery {
    Replay(ToolOperationRecord),
    UnknownOutcome(ToolOperationRecord),
    Materialize(ToolOperationRecord),
}

impl ToolOperationRecovery {
    #[must_use]
    pub fn record(&self) -> &ToolOperationRecord {
        match self {
            Self::Replay(record) | Self::UnknownOutcome(record) | Self::Materialize(record) => {
                record
            }
        }
    }

    #[must_use]
    pub fn call_id(&self) -> &str {
        &self.record().call_id
    }

    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Replay(_) => "replay",
            Self::UnknownOutcome(_) => "unknown_outcome",
            Self::Materialize(_) => "materialize",
        }
    }
}

#[derive(Default)]
pub struct ToolOperationLedger {
    latest: BTreeMap<String, ToolOperationRecord>,
}

impl ToolOperationLedger {
    pub fn apply(&mut self, record: ToolOperationRecord) -> Result<(), ToolOperationError> {
        record.validate()?;
        let Some(current) = self.latest.get(&record.call_id) else {
            if record.phase != ToolOperationPhase::Planned {
                return Err(ToolOperationError::InvalidTransition {
                    call_id: record.call_id,
                    from: ToolOperationPhase::Planned,
                    to: record.phase,
                });
            }
            self.latest.insert(record.call_id.clone(), record);
            return Ok(());
        };
        if serde_json::to_value(current).ok() == serde_json::to_value(&record).ok() {
            return Ok(());
        }
        if current.tool_name != record.tool_name
            || current.admitted_arguments != record.admitted_arguments
            || current.idempotency_key != record.idempotency_key
            || current.replay_policy != record.replay_policy
            || current.planned_at_ms != record.planned_at_ms
        {
            return Err(ToolOperationError::IdentityChanged {
                call_id: record.call_id,
            });
        }
        if record.updated_at_ms < current.updated_at_ms {
            return Err(ToolOperationError::TimestampRegression {
                call_id: record.call_id,
            });
        }
        if record.phase < current.phase {
            return Err(ToolOperationError::PhaseRegression {
                call_id: record.call_id,
                from: current.phase,
                to: record.phase,
            });
        }
        if current.outcome.is_some()
            && serde_json::to_value(&current.outcome).ok()
                != serde_json::to_value(&record.outcome).ok()
        {
            return Err(ToolOperationError::OutcomeChanged {
                call_id: record.call_id,
            });
        }
        if record.phase != current.phase && current.phase.next() != Some(record.phase) {
            return Err(ToolOperationError::InvalidTransition {
                call_id: record.call_id,
                from: current.phase,
                to: record.phase,
            });
        }
        if record.phase == current.phase && record.phase != ToolOperationPhase::EffectPending {
            return Err(ToolOperationError::InvalidTransition {
                call_id: record.call_id,
                from: current.phase,
                to: record.phase,
            });
        }
        self.latest.insert(record.call_id.clone(), record);
        Ok(())
    }

    #[must_use]
    pub fn latest(&self, call_id: &str) -> Option<&ToolOperationRecord> {
        self.latest.get(call_id)
    }

    #[must_use]
    pub fn recovery_actions(&self) -> Vec<ToolOperationRecovery> {
        self.latest
            .values()
            .filter_map(|record| match (record.phase, record.replay_policy) {
                (ToolOperationPhase::EffectPending, ToolReplayPolicy::Safe) => {
                    Some(ToolOperationRecovery::Replay(record.clone()))
                }
                (ToolOperationPhase::EffectPending, ToolReplayPolicy::Never) => {
                    Some(ToolOperationRecovery::UnknownOutcome(record.clone()))
                }
                (ToolOperationPhase::OutcomeReady, _) => {
                    Some(ToolOperationRecovery::Materialize(record.clone()))
                }
                (ToolOperationPhase::Planned | ToolOperationPhase::Completed, _) => None,
            })
            .collect()
    }

    pub fn latest_records(&self) -> impl Iterator<Item = &ToolOperationRecord> {
        self.latest.values()
    }
}

#[derive(Debug)]
pub enum ToolOperationError {
    InvalidRecord(String),
    InvalidTransition {
        call_id: String,
        from: ToolOperationPhase,
        to: ToolOperationPhase,
    },
    PhaseRegression {
        call_id: String,
        from: ToolOperationPhase,
        to: ToolOperationPhase,
    },
    IdentityChanged {
        call_id: String,
    },
    OutcomeChanged {
        call_id: String,
    },
    TimestampRegression {
        call_id: String,
    },
    ProgressTooLarge {
        call_id: String,
        bytes: usize,
    },
}

impl fmt::Display for ToolOperationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRecord(reason) => write!(formatter, "invalid tool operation: {reason}"),
            Self::InvalidTransition { call_id, from, to } => write!(
                formatter,
                "invalid tool operation transition for {call_id}: {from:?} -> {to:?}"
            ),
            Self::PhaseRegression { call_id, from, to } => write!(
                formatter,
                "tool operation phase regressed for {call_id}: {from:?} -> {to:?}"
            ),
            Self::IdentityChanged { call_id } => {
                write!(formatter, "tool operation identity changed for {call_id}")
            }
            Self::OutcomeChanged { call_id } => {
                write!(formatter, "tool operation outcome changed for {call_id}")
            }
            Self::TimestampRegression { call_id } => {
                write!(
                    formatter,
                    "tool operation timestamp regressed for {call_id}"
                )
            }
            Self::ProgressTooLarge { call_id, bytes } => write!(
                formatter,
                "tool operation progress for {call_id} is {bytes} bytes; maximum is {MAX_TOOL_OPERATION_PROGRESS_BYTES}"
            ),
        }
    }
}

impl std::error::Error for ToolOperationError {}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn planned(call_id: &str, replay_policy: ToolReplayPolicy) -> ToolOperationRecord {
        ToolOperationRecord::planned(
            call_id,
            "write",
            json!({"path":"README.md","content":"hello"}),
            Some(format!("idem-{call_id}")),
            replay_policy,
            10,
        )
        .unwrap()
    }

    #[test]
    fn lifecycle_requires_every_forward_transition() {
        let mut ledger = ToolOperationLedger::default();
        let planned = planned("call-1", ToolReplayPolicy::Never);
        ledger.apply(planned.clone()).unwrap();

        let pending = planned.effect_pending(20).unwrap();
        ledger.apply(pending.clone()).unwrap();
        let ready = pending
            .outcome_ready(ToolOperationOutcome::new("written", false, None), 30)
            .unwrap();
        ledger.apply(ready.clone()).unwrap();
        ledger.apply(ready.completed(40).unwrap()).unwrap();

        assert_eq!(
            ledger.latest("call-1").unwrap().phase,
            ToolOperationPhase::Completed
        );
        assert!(ledger.recovery_actions().is_empty());
    }

    #[test]
    fn lifecycle_rejects_skips_regressions_and_identity_drift() {
        let mut ledger = ToolOperationLedger::default();
        let initial = planned("call-1", ToolReplayPolicy::Never);
        ledger.apply(initial.clone()).unwrap();

        let skipped = initial
            .clone()
            .with_phase_for_test(ToolOperationPhase::OutcomeReady, 20);
        assert!(matches!(
            ledger.apply(skipped),
            Err(ToolOperationError::InvalidTransition { .. })
        ));

        let pending = initial.effect_pending(20).unwrap();
        ledger.apply(pending.clone()).unwrap();
        let regressed = planned("call-1", ToolReplayPolicy::Never)
            .with_phase_for_test(ToolOperationPhase::Planned, 21);
        assert!(matches!(
            ledger.apply(regressed),
            Err(ToolOperationError::PhaseRegression { .. })
        ));

        let mut drifted = pending;
        drifted.tool_name = "bash".into();
        drifted.updated_at_ms = 21;
        assert!(matches!(
            ledger.apply(drifted),
            Err(ToolOperationError::IdentityChanged { .. })
        ));
    }

    #[test]
    fn progress_replaces_prior_snapshot_and_is_bounded() {
        let mut ledger = ToolOperationLedger::default();
        let pending = planned("call-progress", ToolReplayPolicy::Never)
            .effect_pending(20)
            .unwrap();
        ledger
            .apply(planned("call-progress", ToolReplayPolicy::Never))
            .unwrap();
        ledger.apply(pending.clone()).unwrap();
        ledger
            .apply(
                pending
                    .clone()
                    .with_progress(json!({"percent":10}), 21)
                    .unwrap(),
            )
            .unwrap();
        ledger
            .apply(pending.with_progress(json!({"percent":90}), 22).unwrap())
            .unwrap();
        assert_eq!(
            ledger.latest("call-progress").unwrap().progress,
            Some(json!({"percent":90}))
        );

        let oversized = "x".repeat(MAX_TOOL_OPERATION_PROGRESS_BYTES + 1);
        assert!(matches!(
            planned("call-large", ToolReplayPolicy::Never)
                .effect_pending(20)
                .unwrap()
                .with_progress(json!(oversized), 21),
            Err(ToolOperationError::ProgressTooLarge { .. })
        ));
    }

    #[test]
    fn recovery_is_stable_and_never_infers_replay_safety() {
        let mut ledger = ToolOperationLedger::default();
        let safe = planned("call-safe", ToolReplayPolicy::Safe);
        ledger.apply(safe.clone()).unwrap();
        ledger.apply(safe.effect_pending(20).unwrap()).unwrap();

        let never = planned("call-never", ToolReplayPolicy::Never);
        ledger.apply(never.clone()).unwrap();
        ledger.apply(never.effect_pending(20).unwrap()).unwrap();

        let ready = planned("call-ready", ToolReplayPolicy::Never);
        ledger.apply(ready.clone()).unwrap();
        let ready = ready.effect_pending(20).unwrap();
        ledger.apply(ready.clone()).unwrap();
        let ready = ready
            .outcome_ready(ToolOperationOutcome::new("done", false, None), 30)
            .unwrap();
        ledger.apply(ready).unwrap();

        assert_eq!(
            ledger
                .recovery_actions()
                .into_iter()
                .map(|action| (action.call_id().to_owned(), action.kind()))
                .collect::<Vec<_>>(),
            vec![
                ("call-never".into(), "unknown_outcome"),
                ("call-ready".into(), "materialize"),
                ("call-safe".into(), "replay"),
            ]
        );
    }

    #[test]
    fn duplicate_records_are_idempotent_but_outcomes_are_immutable() {
        let mut ledger = ToolOperationLedger::default();
        let planned = planned("call-1", ToolReplayPolicy::Never);
        ledger.apply(planned.clone()).unwrap();
        ledger.apply(planned.clone()).unwrap();
        let pending = planned.effect_pending(20).unwrap();
        ledger.apply(pending.clone()).unwrap();
        let ready = pending
            .outcome_ready(ToolOperationOutcome::new("first", false, None), 30)
            .unwrap();
        ledger.apply(ready.clone()).unwrap();
        ledger.apply(ready.clone()).unwrap();

        let mut changed = ready;
        changed.outcome = Some(ToolOperationOutcome::new("second", false, None));
        changed.updated_at_ms = 31;
        assert!(matches!(
            ledger.apply(changed),
            Err(ToolOperationError::OutcomeChanged { .. })
        ));
    }
}
