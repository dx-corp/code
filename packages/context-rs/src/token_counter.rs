//! Memoized token counting shared by compaction and request accounting.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;

use crate::token_counting::{self, CountConfidence};
use crate::token_estimation;

/// Upper bound on memoized token counts, so a long session cannot grow the
/// table without limit. The table is cleared wholesale when it is reached.
const TOKEN_COUNT_CACHE_MAX_ENTRIES: usize = 4_096;

/// The token counter behind context accounting and compaction decisions.
///
/// Compaction used to count with the bytes/4 heuristic in
/// [`crate::token_estimation`] while `/context` counted with
/// [`crate::token_counting::count_tokens`], which uses the model's bundled
/// tokenizer when one exists. The two disagree by the tokenizer's error, so
/// the auto-compaction gate fired at a different point than the usage
/// percentage the user was shown. This type makes both read the same counter
/// for the same model.
///
/// Counts are memoized on `(byte length, hash of the text)` because the gate
/// re-counts the whole transcript on every turn and byte-pair encoding is
/// linear in the input size. When no tokenizer is bundled for the model, the
/// counter calls the heuristic directly and does not touch the table: the
/// heuristic is a length division and memoizing it would cost more than it
/// saves.
pub struct TokenCounter {
    model: Option<String>,
    measured: bool,
    cache: Mutex<HashMap<(usize, u64), u64>>,
    calibration_scale: Mutex<f64>,
}

impl TokenCounter {
    /// Build a counter for `model`. `None` selects the bytes/4 heuristic.
    #[must_use]
    pub fn new(model: Option<String>) -> Self {
        // Probing with the empty string resolves both "is a tokenizer bundled
        // for this model family" and "did the tokenizer data actually load",
        // which is the same decision `count_tokens` makes per call.
        let measured = matches!(
            token_counting::count_tokens_with_metadata("", model.as_deref()).confidence,
            CountConfidence::Measured
        );
        Self {
            model,
            measured,
            cache: Mutex::new(HashMap::new()),
            calibration_scale: Mutex::new(1.0),
        }
    }

    /// A counter with no model: always the shared bytes/4 heuristic.
    #[must_use]
    pub fn heuristic() -> Self {
        Self::new(None)
    }

    /// Whether counts come from a real tokenizer rather than the heuristic.
    #[must_use]
    pub fn is_measured(&self) -> bool {
        self.measured
    }

    /// Count the tokens in `text` for the configured model.
    #[must_use]
    pub fn count(&self, text: &str) -> u64 {
        let base = if !self.measured {
            token_estimation::estimate_tokens(text)
        } else {
            let key = (text.len(), content_hash(text));
            if let Ok(cache) = self.cache.lock() {
                if let Some(hit) = cache.get(&key) {
                    return *hit;
                }
            }
            let counted = token_counting::count_tokens(text, self.model.as_deref());
            if let Ok(mut cache) = self.cache.lock() {
                if cache.len() >= TOKEN_COUNT_CACHE_MAX_ENTRIES {
                    cache.clear();
                }
                cache.insert(key, counted);
            }
            counted
        };
        if self.measured || base == 0 {
            return base;
        }
        let scale = self.calibration_scale.lock().map_or(1.0, |scale| *scale);
        ((base as f64) * scale).round().clamp(0.0, u64::MAX as f64) as u64
    }

    /// Recalibrate a heuristic counter from one exact provider observation.
    /// Measured local tokenizers remain authoritative and ignore calibration.
    pub fn calibrate(&self, estimated_tokens: u64, observed_tokens: u64) -> bool {
        if self.measured || estimated_tokens == 0 || observed_tokens == 0 {
            return false;
        }
        let Ok(mut scale) = self.calibration_scale.lock() else {
            return false;
        };
        let correction = observed_tokens as f64 / estimated_tokens as f64;
        *scale = (*scale * correction).clamp(0.5, 2.5);
        true
    }
}

impl std::fmt::Debug for TokenCounter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenCounter")
            .field("model", &self.model)
            .field("measured", &self.measured)
            .field(
                "calibration_scale",
                &self.calibration_scale.lock().map(|scale| *scale).ok(),
            )
            .finish_non_exhaustive()
    }
}

fn content_hash(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heuristic_counter_recalibrates_from_an_observed_request() {
        let counter = TokenCounter::heuristic();
        let text = "fn main() { println!(\"dense code tokens\"); }".repeat(40);
        let estimated = counter.count(&text);

        assert!(counter.calibrate(estimated, estimated.saturating_mul(5) / 4));
        assert_eq!(counter.count(&text), estimated.saturating_mul(5) / 4);
    }

    #[test]
    fn measured_counter_ignores_remote_recalibration() {
        let counter = TokenCounter::new(Some("gpt-4o".to_owned()));
        assert!(counter.is_measured());
        let before = counter.count("hello world");
        assert!(!counter.calibrate(before, before.saturating_mul(2)));
        assert_eq!(counter.count("hello world"), before);
    }
}
