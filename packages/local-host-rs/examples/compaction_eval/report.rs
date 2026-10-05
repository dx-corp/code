use super::suite::Suite;
use anyhow::{Result, ensure};
use serde::Serialize;
use std::collections::HashSet;

#[path = "../behavior_eval/measurement.rs"]
mod measurement;
use measurement::Arm;
pub(crate) use measurement::arm;
pub use measurement::{Measurement, Trial};

#[derive(Serialize)]
pub struct Report {
    pub schema: &'static str,
    pub comparison_valid: bool,
    pub original: Arm,
    pub compacted: Arm,
    pub compacted_only: usize,
    pub original_only: usize,
    pub difference_percentage_points: f64,
    pub claim: &'static str,
    pub promotion_allowed: bool,
}

pub fn paired(suite: &Suite, trials: &[Trial]) -> Result<Report> {
    ensure!(!suite.cases.is_empty(), "empty paired denominator");
    ensure!(
        trials.len() == suite.cases.len() * 2,
        "incomplete paired denominator"
    );
    let ids: HashSet<_> = suite.cases.iter().map(|c| c.id.as_str()).collect();
    let mut seen = HashSet::new();
    for trial in trials {
        ensure!(
            ids.contains(trial.case_id.as_str()) && seen.insert((&trial.case_id, trial.compacted)),
            "extra or duplicate trial"
        );
        ensure!(
            trial.terminal || trial.failure.is_some(),
            "nonterminal trial without a recorded failure"
        );
        ensure!(
            !trial.verified || (trial.terminal && trial.failure.is_none()),
            "unverified terminal claimed success"
        );
        ensure!(
            !trial.terminal || !trial.compacted || trial.forced_compaction_applied,
            "missing forced compaction"
        );
        ensure!(
            trial.compacted || !trial.forced_compaction_applied,
            "original arm was compacted"
        );
    }
    let original = trials.iter().filter(|r| !r.compacted).collect::<Vec<_>>();
    let compacted = trials.iter().filter(|r| r.compacted).collect::<Vec<_>>();
    let mut wins = 0;
    let mut losses = 0;
    for control in &original {
        let candidate = compacted
            .iter()
            .find(|r| r.case_id == control.case_id)
            .expect("validated pair");
        wins += usize::from(candidate.verified && !control.verified);
        losses += usize::from(control.verified && !candidate.verified);
    }
    Ok(Report {
        schema: "maestro.compaction-eval-report.v1",
        comparison_valid: trials
            .iter()
            .all(|r| r.failure.as_deref().is_none_or(|f| f == "timeout")),
        original: arm(&original),
        compacted: arm(&compacted),
        compacted_only: wins,
        original_only: losses,
        difference_percentage_points: 100.0 * (wins as f64 - losses as f64)
            / suite.cases.len() as f64,
        claim: "synthetic_context_behavior_only",
        promotion_allowed: false,
    })
}
