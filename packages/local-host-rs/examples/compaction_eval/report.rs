use super::host::agent::TokenUsage;
use super::suite::Suite;
use anyhow::{Result, ensure};
use serde::Serialize;
use std::collections::HashSet;

#[derive(Default, Serialize)]
pub struct Measurement {
    pub response_count: usize,
    pub usage_observations: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub provider_reported_cost_usd: Option<f64>,
    pub complete: bool,
}

impl Measurement {
    pub fn add(&mut self, usage: Option<&TokenUsage>) {
        let first = self.response_count == 0;
        self.response_count += 1;
        if let Some(usage) = usage {
            self.usage_observations += 1;
            self.input_tokens = self.input_tokens.saturating_add(usage.input_tokens);
            self.output_tokens = self.output_tokens.saturating_add(usage.output_tokens);
            self.cache_read_tokens = self
                .cache_read_tokens
                .saturating_add(usage.cache_read_tokens);
            self.cache_write_tokens = self
                .cache_write_tokens
                .saturating_add(usage.cache_write_tokens);
            let cost = usage.cost.filter(|v| v.is_finite() && *v >= 0.0);
            self.provider_reported_cost_usd = if first {
                cost
            } else {
                self.provider_reported_cost_usd
                    .zip(cost)
                    .map(|(a, b)| a + b)
                    .filter(|v| v.is_finite())
            };
        } else {
            self.provider_reported_cost_usd = None;
        }
    }
}

#[derive(Serialize)]
pub struct Trial {
    pub case_id: String,
    pub compacted: bool,
    pub verified: bool,
    pub terminal: bool,
    pub failure: Option<String>,
    pub elapsed_seconds: f64,
    pub retries_started: usize,
    pub forced_compaction_applied: bool,
    pub answer: String,
    pub measurement: Measurement,
}

#[derive(Serialize)]
pub struct Arm {
    pub attempts: usize,
    pub verified: usize,
    pub success_rate: f64,
    pub elapsed_seconds: f64,
    pub retries_started: usize,
    pub input_tokens_observed: u64,
    pub output_tokens_observed: u64,
    pub cache_read_tokens_observed: u64,
    pub cache_write_tokens_observed: u64,
    pub usage_complete: bool,
    pub provider_reported_cost_usd: Option<f64>,
    /// Includes failed attempts and compaction overhead in the numerator.
    pub cost_per_verified_task_usd: Option<f64>,
}

fn arm(rows: &[&Trial]) -> Arm {
    let verified = rows.iter().filter(|r| r.verified).count();
    let costs: Option<Vec<f64>> = rows
        .iter()
        .map(|r| {
            r.measurement
                .complete
                .then_some(r.measurement.provider_reported_cost_usd)
                .flatten()
        })
        .collect();
    let cost = costs
        .map(|c| c.iter().sum::<f64>())
        .filter(|c| c.is_finite());
    Arm {
        attempts: rows.len(),
        verified,
        success_rate: verified as f64 / rows.len() as f64,
        elapsed_seconds: rows.iter().map(|r| r.elapsed_seconds).sum(),
        retries_started: rows.iter().map(|r| r.retries_started).sum(),
        input_tokens_observed: rows.iter().map(|r| r.measurement.input_tokens).sum(),
        output_tokens_observed: rows.iter().map(|r| r.measurement.output_tokens).sum(),
        cache_read_tokens_observed: rows.iter().map(|r| r.measurement.cache_read_tokens).sum(),
        cache_write_tokens_observed: rows.iter().map(|r| r.measurement.cache_write_tokens).sum(),
        usage_complete: rows.iter().all(|r| {
            r.measurement.complete
                && r.measurement.response_count > 0
                && r.measurement.usage_observations == r.measurement.response_count
        }),
        provider_reported_cost_usd: cost,
        cost_per_verified_task_usd: cost.filter(|_| verified > 0).map(|c| c / verified as f64),
    }
}

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
