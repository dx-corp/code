//! Shared accounting for native compaction and instruction behavior trials.
use maestro_runtime::agent::TokenUsage;
use serde::Serialize;

#[derive(Clone, Default, Serialize)]
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

#[derive(Clone, Serialize)]
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

pub(crate) fn arm(rows: &[&Trial]) -> Arm {
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
