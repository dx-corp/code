use super::ContextCompactor;

impl ContextCompactor {
    /// Whether an exact provider count is worth its extra request. Probe only
    /// for heuristic tokenizers and only once the sealed request is within 80%
    /// of the configured compaction trigger.
    #[must_use]
    pub fn should_calibrate_request(&self, estimated_request_tokens: u64) -> bool {
        !self.counter.is_measured()
            && estimated_request_tokens >= self.compaction_trigger_tokens().saturating_mul(4) / 5
    }

    /// Apply an exact provider count to all subsequent heuristic budgeting in
    /// this compactor. Returns false for measured local tokenizers.
    pub fn calibrate_counter(&self, estimated_tokens: u64, observed_tokens: u64) -> bool {
        self.counter.calibrate(estimated_tokens, observed_tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compaction::CompactionConfig;

    #[test]
    fn exact_count_probe_waits_until_a_heuristic_request_nears_the_trigger() {
        let compactor = ContextCompactor::new(CompactionConfig {
            max_context_tokens: 1_000,
            auto_compact_enabled: true,
            auto_compact_threshold: 0.85,
            model: Some("claude-sonnet-4-5".to_owned()),
            ..Default::default()
        });

        assert!(!compactor.should_calibrate_request(679));
        assert!(compactor.should_calibrate_request(680));
        assert!(compactor.calibrate_counter(680, 850));
    }
}
