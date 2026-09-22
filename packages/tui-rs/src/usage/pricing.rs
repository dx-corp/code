//! Model pricing configuration
//!
//! Provides per-model token pricing for cost estimation.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

/// OpenRouter spells a Claude release suffix with a dot
/// (`anthropic/claude-opus-5.5`) where the direct Anthropic id uses a dash
/// (`claude-opus-5-5`). Without this, `claude-opus-5.5` matches only the
/// shorter `claude-opus-5` tier and bills at the wrong rate. Ids from other
/// providers keep their dots, so `gpt-5.6` is untouched.
fn normalize_claude_release_suffix(model: &str) -> Cow<'_, str> {
    let name = model.rsplit('/').next().unwrap_or(model);
    if name.starts_with("claude-") && name.contains('.') {
        Cow::Owned(model.replace('.', "-"))
    } else {
        Cow::Borrowed(model)
    }
}

/// Pricing tier for a model
#[derive(Debug, Clone, Copy)]
pub struct PricingTier {
    /// Cost per 1M input tokens in USD
    pub input_per_million: f64,
    /// Cost per 1M output tokens in USD
    pub output_per_million: f64,
    /// Cost per 1M cached read tokens (typically discounted)
    pub cache_read_per_million: f64,
    /// Cost per 1M cached write tokens
    pub cache_write_per_million: f64,
}

impl PricingTier {
    /// Create a new pricing tier
    #[must_use]
    pub const fn new(input: f64, output: f64, cache_read: f64, cache_write: f64) -> Self {
        Self {
            input_per_million: input,
            output_per_million: output,
            cache_read_per_million: cache_read,
            cache_write_per_million: cache_write,
        }
    }

    /// Create tier with just input/output pricing (cache = 0)
    #[must_use]
    pub const fn simple(input: f64, output: f64) -> Self {
        Self::new(input, output, 0.0, 0.0)
    }

    /// Calculate cost for given token counts
    #[must_use]
    pub fn calculate_cost(
        &self,
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
    ) -> f64 {
        let input_cost = (input_tokens as f64 / 1_000_000.0) * self.input_per_million;
        let output_cost = (output_tokens as f64 / 1_000_000.0) * self.output_per_million;
        let cache_read_cost =
            (cache_read_tokens as f64 / 1_000_000.0) * self.cache_read_per_million;
        let cache_write_cost =
            (cache_write_tokens as f64 / 1_000_000.0) * self.cache_write_per_million;

        input_cost + output_cost + cache_read_cost + cache_write_cost
    }
}

impl Default for PricingTier {
    fn default() -> Self {
        // Default to Claude Sonnet 4 pricing
        Self::new(3.0, 15.0, 0.30, 3.75)
    }
}

/// A tier built from the bundled catalog's published rates, memoized so
/// `get_tier` can hand back a reference.
///
/// 49 catalogued models had no `add_tier` call and billed at the default
/// $3/$15 Sonnet 4 rate, including every current Gemini and the GPT-5 family.
/// Rates now come from the same generated snapshot as context limits instead
/// of being typed in a second time.
fn catalog_tier(model: &str) -> Option<&'static PricingTier> {
    static TIERS: LazyLock<Mutex<HashMap<String, &'static PricingTier>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    let mut cache = TIERS.lock().ok()?;
    if let Some(tier) = cache.get(model) {
        return Some(*tier);
    }
    let rates = maestro_local_host::model_catalog::bundled_rates(model)?;
    // Leaked once per distinct id seen in a process so the reference can
    // outlive the lock. Bounded by the size of the snapshot.
    let tier: &'static PricingTier = Box::leak(Box::new(PricingTier::new(
        rates.input_per_million,
        rates.output_per_million,
        rates.cache_read_per_million,
        rates.cache_write_per_million,
    )));
    cache.insert(model.to_owned(), tier);
    Some(tier)
}

/// Model pricing database
#[derive(Debug, Clone)]
pub struct ModelPricing {
    /// Map of model name patterns to pricing tiers
    tiers: HashMap<String, PricingTier>,
    /// Default tier for unknown models
    default_tier: PricingTier,
}

impl ModelPricing {
    /// Create a new model pricing database
    #[must_use]
    pub fn new() -> Self {
        Self {
            tiers: HashMap::new(),
            default_tier: PricingTier::default(),
        }
    }

    /// Add a pricing tier for a model pattern
    pub fn add_tier(&mut self, pattern: impl Into<String>, tier: PricingTier) {
        self.tiers.insert(pattern.into(), tier);
    }

    /// Set the default tier for unknown models
    pub fn set_default(&mut self, tier: PricingTier) {
        self.default_tier = tier;
    }

    /// Get pricing for a model (matches by prefix)
    #[must_use]
    pub fn get_tier(&self, model: &str) -> &PricingTier {
        // The bundled catalog is authoritative for any model whose rates
        // upstream publishes; the patterns below cover the rest.
        if let Some(tier) = catalog_tier(model) {
            return tier;
        }

        let normalized = normalize_claude_release_suffix(model);
        let model = normalized.as_ref();

        // Try exact match first
        if let Some(tier) = self.tiers.get(model) {
            return tier;
        }

        // Try prefix matching (longest match wins to prefer specific tiers)
        let mut best_match: Option<(&str, &PricingTier)> = None;
        for (pattern, tier) in &self.tiers {
            if model.starts_with(pattern.as_str()) || pattern.starts_with(model) {
                match best_match {
                    None => best_match = Some((pattern.as_str(), tier)),
                    Some((best_pattern, _)) if pattern.len() > best_pattern.len() => {
                        best_match = Some((pattern.as_str(), tier));
                    }
                    _ => {}
                }
            }
        }
        if let Some((_, tier)) = best_match {
            return tier;
        }

        // Check for common model families (e.g., provider/model-id strings)
        let model_lower = model.to_lowercase();
        let mut best_family_match: Option<(&str, &PricingTier)> = None;
        for (pattern, tier) in &self.tiers {
            let pattern_lower = pattern.to_lowercase();
            if model_lower.contains(&pattern_lower) || pattern_lower.contains(&model_lower) {
                match best_family_match {
                    None => best_family_match = Some((pattern.as_str(), tier)),
                    Some((best_pattern, _)) if pattern.len() > best_pattern.len() => {
                        best_family_match = Some((pattern.as_str(), tier));
                    }
                    _ => {}
                }
            }
        }
        if let Some((_, tier)) = best_family_match {
            return tier;
        }

        &self.default_tier
    }

    /// Calculate cost for a model with given token counts
    #[must_use]
    pub fn calculate_cost(
        &self,
        model: &str,
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
    ) -> f64 {
        self.get_tier(model).calculate_cost(
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_write_tokens,
        )
    }
}

impl Default for ModelPricing {
    fn default() -> Self {
        let mut pricing = Self::new();

        // Anthropic Claude models. Rates are per million tokens from
        // platform.claude.com/docs/en/about-claude/pricing, mirrored by the
        // bundled models.dev snapshot in model_catalog_data.json.
        // Claude Opus 5.5 ($4/$20; cache reads are 5% of input, not 10%)
        pricing.add_tier("claude-opus-5-5", PricingTier::new(4.0, 20.0, 0.20, 5.0));
        // Claude Opus 5 ($5/$25 per M tokens)
        pricing.add_tier("claude-opus-5", PricingTier::new(5.0, 25.0, 0.50, 6.25));
        // Claude Opus 4.8, 4.7, 4.6, and 4.5 ($5/$25 per M tokens)
        pricing.add_tier("claude-opus-4-8", PricingTier::new(5.0, 25.0, 0.50, 6.25));
        pricing.add_tier("claude-opus-4-7", PricingTier::new(5.0, 25.0, 0.50, 6.25));
        pricing.add_tier("claude-opus-4-6", PricingTier::new(5.0, 25.0, 0.50, 6.25));
        pricing.add_tier("claude-opus-4-5", PricingTier::new(5.0, 25.0, 0.50, 6.25));
        // Claude Opus 4.0 ($15/$75 per M tokens)
        pricing.add_tier("claude-opus-4", PricingTier::new(15.0, 75.0, 1.50, 18.75));
        pricing.add_tier("claude-4-opus", PricingTier::new(15.0, 75.0, 1.50, 18.75));

        // Claude Fable 5.1 and Fable 5 ($10/$50; Fable 5.1 cache reads are
        // 2.5% of input, Fable 5 reads are 10%)
        pricing.add_tier(
            "claude-fable-5-1",
            PricingTier::new(10.0, 50.0, 0.25, 12.50),
        );
        pricing.add_tier("claude-fable-5", PricingTier::new(10.0, 50.0, 1.0, 12.50));

        // Claude Sonnet 5 ($2/$10 per M tokens)
        pricing.add_tier("claude-sonnet-5", PricingTier::new(2.0, 10.0, 0.20, 2.50));

        // Claude Sonnet 4.6 and 4.5 ($3/$15 per M tokens)
        pricing.add_tier("claude-sonnet-4-6", PricingTier::new(3.0, 15.0, 0.30, 3.75));
        pricing.add_tier("claude-sonnet-4-5", PricingTier::new(3.0, 15.0, 0.30, 3.75));

        // Claude Sonnet 4
        pricing.add_tier("claude-sonnet-4", PricingTier::new(3.0, 15.0, 0.30, 3.75));
        pricing.add_tier("claude-4-sonnet", PricingTier::new(3.0, 15.0, 0.30, 3.75));

        // Claude Haiku 4.5 ($1/$5 per M tokens)
        pricing.add_tier("claude-haiku-4-5", PricingTier::new(1.0, 5.0, 0.10, 1.25));

        // Claude 3.5 Sonnet
        pricing.add_tier("claude-3-5-sonnet", PricingTier::new(3.0, 15.0, 0.30, 3.75));
        pricing.add_tier("claude-3.5-sonnet", PricingTier::new(3.0, 15.0, 0.30, 3.75));

        // Claude 3.5 Haiku
        pricing.add_tier("claude-3-5-haiku", PricingTier::new(0.80, 4.0, 0.08, 1.0));
        pricing.add_tier("claude-3.5-haiku", PricingTier::new(0.80, 4.0, 0.08, 1.0));

        // Claude 3 Opus
        pricing.add_tier("claude-3-opus", PricingTier::new(15.0, 75.0, 1.50, 18.75));

        // Claude 3 Sonnet
        pricing.add_tier("claude-3-sonnet", PricingTier::new(3.0, 15.0, 0.30, 3.75));

        // Claude 3 Haiku
        pricing.add_tier("claude-3-haiku", PricingTier::new(0.25, 1.25, 0.03, 0.30));

        // OpenAI GPT models
        pricing.add_tier("gpt-4o", PricingTier::simple(2.50, 10.0));
        pricing.add_tier("gpt-4o-mini", PricingTier::simple(0.15, 0.60));
        pricing.add_tier("gpt-4-turbo", PricingTier::simple(10.0, 30.0));
        pricing.add_tier("gpt-4", PricingTier::simple(30.0, 60.0));
        pricing.add_tier("gpt-3.5-turbo", PricingTier::simple(0.50, 1.50));
        pricing.add_tier("o1-preview", PricingTier::simple(15.0, 60.0));
        pricing.add_tier("o1-mini", PricingTier::simple(1.10, 4.40));
        pricing.add_tier("o1", PricingTier::simple(15.0, 60.0));
        pricing.add_tier("o3-mini", PricingTier::simple(1.10, 4.40));
        pricing.add_tier("o3", PricingTier::simple(2.0, 8.0));

        // Google Gemini models
        pricing.add_tier("gemini-2.0-flash", PricingTier::simple(0.10, 0.40));
        pricing.add_tier("gemini-1.5-pro", PricingTier::simple(1.25, 5.0));
        pricing.add_tier("gemini-1.5-flash", PricingTier::simple(0.075, 0.30));
        pricing.add_tier("gemini-pro", PricingTier::simple(0.50, 1.50));

        // Groq (inference provider - typically cheaper)
        pricing.add_tier("llama-3.3-70b", PricingTier::simple(0.59, 0.79));
        pricing.add_tier("llama-3.1-70b", PricingTier::simple(0.59, 0.79));
        pricing.add_tier("llama-3.1-8b", PricingTier::simple(0.05, 0.08));
        pricing.add_tier("mixtral-8x7b", PricingTier::simple(0.24, 0.24));

        // DeepSeek
        pricing.add_tier("deepseek-chat", PricingTier::simple(0.14, 0.28));
        pricing.add_tier("deepseek-coder", PricingTier::simple(0.14, 0.28));
        pricing.add_tier("deepseek-reasoner", PricingTier::simple(0.55, 2.19));

        pricing
    }
}

/// Global default pricing database
pub static DEFAULT_PRICING: std::sync::LazyLock<ModelPricing> =
    std::sync::LazyLock::new(ModelPricing::default);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pricing_tier_calculation() {
        let tier = PricingTier::new(3.0, 15.0, 0.30, 3.75);

        // 1000 input tokens = $0.003
        let cost = tier.calculate_cost(1000, 0, 0, 0);
        assert!((cost - 0.003).abs() < 0.0001);

        // 1000 output tokens = $0.015
        let cost = tier.calculate_cost(0, 1000, 0, 0);
        assert!((cost - 0.015).abs() < 0.0001);

        // Combined
        let cost = tier.calculate_cost(1000, 500, 0, 0);
        assert!((cost - 0.0105).abs() < 0.0001);
    }

    #[test]
    fn test_model_pricing_lookup() {
        let pricing = ModelPricing::default();

        // Exact match
        let tier = pricing.get_tier("claude-3-5-sonnet");
        assert!((tier.input_per_million - 3.0).abs() < 0.01);

        // Prefix match
        let tier = pricing.get_tier("claude-3-5-sonnet-20241022");
        assert!((tier.input_per_million - 3.0).abs() < 0.01);

        // GPT model exact match
        let tier = pricing.get_tier("gpt-4o");
        assert!((tier.input_per_million - 2.50).abs() < 0.01);

        // GPT model with suffix (falls through to default since "gpt-4o-2024" doesn't start with "gpt-4o")
        // This tests that the lookup handles model versions
        let tier = pricing.get_tier("gpt-4-turbo");
        assert!((tier.input_per_million - 10.0).abs() < 0.01);
    }

    #[test]
    fn test_haiku_vs_opus_pricing() {
        let pricing = ModelPricing::default();

        let haiku = pricing.get_tier("claude-3-haiku");
        let opus = pricing.get_tier("claude-3-opus");

        // Opus should be significantly more expensive
        assert!(opus.input_per_million > haiku.input_per_million * 10.0);
    }

    #[test]
    fn test_cost_calculation() {
        let pricing = ModelPricing::default();

        // Typical conversation: 2000 input, 500 output with Sonnet 4
        let cost = pricing.calculate_cost("claude-sonnet-4", 2000, 500, 0, 0);
        // 2000 * 3.0/1M + 500 * 15.0/1M = 0.006 + 0.0075 = 0.0135
        assert!((cost - 0.0135).abs() < 0.0001);
    }

    #[test]
    fn test_cache_pricing() {
        let pricing = ModelPricing::default();

        // With cache hits, cost should be lower
        let no_cache = pricing.calculate_cost("claude-sonnet-4", 10000, 1000, 0, 0);
        let with_cache = pricing.calculate_cost("claude-sonnet-4", 2000, 1000, 8000, 0);

        // Cache reads at 0.30/M vs input at 3.0/M
        assert!(with_cache < no_cache);
    }

    #[test]
    fn test_opus_4_6_pricing_is_specific() {
        let pricing = ModelPricing::default();

        // Exact model ID
        let tier = pricing.get_tier("claude-opus-4-6");
        assert!((tier.input_per_million - 5.0).abs() < 0.01);
        assert!((tier.output_per_million - 25.0).abs() < 0.01);

        // Versioned suffix should match the 4.5 tier (not generic 4.x pricing)
        let tier = pricing.get_tier("claude-opus-4-5-20251101");
        assert!((tier.input_per_million - 5.0).abs() < 0.01);

        // provider/model-id format should match the specific tier
        let tier = pricing.get_tier("anthropic/claude-opus-4-6");
        assert!((tier.input_per_million - 5.0).abs() < 0.01);

        // Extra suffix variants should still match the specific tier
        let tier = pricing.get_tier("claude-opus-4-6-thinking");
        assert!((tier.input_per_million - 5.0).abs() < 0.01);

        // The generic Opus 4 tier should remain $15/$75
        let tier = pricing.get_tier("claude-opus-4-20250514");
        assert!((tier.input_per_million - 15.0).abs() < 0.01);
    }

    #[test]
    fn test_current_anthropic_lineup_pricing() {
        let pricing = ModelPricing::default();

        // Opus 5.5 must not fall back to the Opus 5 tier.
        let tier = pricing.get_tier("claude-opus-5-5");
        assert!((tier.input_per_million - 4.0).abs() < 0.01);
        assert!((tier.output_per_million - 20.0).abs() < 0.01);
        assert!((tier.cache_read_per_million - 0.20).abs() < 0.01);
        assert!((tier.cache_write_per_million - 5.0).abs() < 0.01);

        // Routed and suffixed ids resolve to the same tier.
        for model in [
            "anthropic/claude-opus-5-5",
            "claude-opus-5-5-thinking",
            "anthropic/claude-opus-5.5",
        ] {
            let tier = pricing.get_tier(model);
            assert!(
                (tier.input_per_million - 4.0).abs() < 0.01,
                "{model} got ${}/M instead of $4/M",
                tier.input_per_million
            );
        }

        let tier = pricing.get_tier("claude-opus-5");
        assert!((tier.input_per_million - 5.0).abs() < 0.01);
        assert!((tier.output_per_million - 25.0).abs() < 0.01);

        let tier = pricing.get_tier("claude-sonnet-5");
        assert!((tier.input_per_million - 2.0).abs() < 0.01);
        assert!((tier.output_per_million - 10.0).abs() < 0.01);

        let tier = pricing.get_tier("claude-fable-5-1");
        assert!((tier.input_per_million - 10.0).abs() < 0.01);
        assert!((tier.cache_read_per_million - 0.25).abs() < 0.01);

        let tier = pricing.get_tier("claude-haiku-4-5-20251001");
        assert!((tier.input_per_million - 1.0).abs() < 0.01);
        assert!((tier.output_per_million - 5.0).abs() < 0.01);

        let tier = pricing.get_tier("claude-opus-4-8");
        assert!((tier.input_per_million - 5.0).abs() < 0.01);
    }

    #[test]
    fn catalogued_models_bill_at_their_published_rates() {
        let pricing = ModelPricing::default();

        // None of these had an add_tier call; every one billed at the $3/$15
        // Sonnet 4 default before the catalog became authoritative.
        for (model, input, output) in [
            ("gemini-3.6-flash", 0.75, 3.75),
            ("gpt-5", 1.25, 10.0),
            ("gpt-6-astra", 10.0, 50.0),
        ] {
            let tier = pricing.get_tier(model);
            assert!(
                (tier.input_per_million - input).abs() < 0.001,
                "{model} input ${}/M, expected ${input}/M",
                tier.input_per_million
            );
            assert!(
                (tier.output_per_million - output).abs() < 0.001,
                "{model} output ${}/M, expected ${output}/M",
                tier.output_per_million
            );
        }

        // A routed row resolves to the same rates as its direct row.
        let direct = pricing.get_tier("claude-opus-5-5");
        let routed = pricing.get_tier("anthropic/claude-opus-5.5");
        assert!((direct.input_per_million - 4.0).abs() < 0.001);
        assert!((routed.input_per_million - direct.input_per_million).abs() < 0.001);
        assert!((routed.output_per_million - direct.output_per_million).abs() < 0.001);
        assert!((routed.cache_read_per_million - direct.cache_read_per_million).abs() < 0.001);
    }

    #[test]
    fn every_catalogued_model_has_published_rates() {
        // The generated snapshot is the single source; this fails when a model
        // lands in it without cost, which is what would silently put it back
        // on the $3/$15 default.
        let catalog: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../local-host-rs/src/model_catalog_data.json"
        )))
        .expect("bundled catalog parses");

        let mut missing = Vec::new();
        let mut open_weights = Vec::new();
        for model in catalog["models"].as_array().expect("models array") {
            let provider = model["provider"].as_str().unwrap_or_default();
            if !matches!(provider, "anthropic" | "openai" | "google" | "xai") {
                continue;
            }
            let id = model["id"].as_str().unwrap_or_default().to_owned();
            if model.get("cost").is_some() {
                continue;
            }
            // An open-weights model is self-hosted and has no vendor rate, so
            // upstream publishes none. Anything else is a real gap.
            if model["open_weights"].as_bool() == Some(true) {
                open_weights.push(id);
            } else {
                missing.push(id);
            }
        }
        assert!(
            missing.is_empty(),
            "priced models in the bundled catalog with no published rates: {missing:?}"
        );
        // Keep the exemption honest: if it ever covers everything, the check
        // above has stopped checking anything.
        assert!(
            open_weights.len() < 5,
            "unexpectedly many unpriced open-weights models, is the cost mapping broken? {open_weights:?}"
        );
    }

    #[test]
    fn test_longest_prefix_wins() {
        // Verifies that longest-match-wins is deterministic regardless of
        // HashMap iteration order. Run 50 times to catch nondeterminism.
        for _ in 0..50 {
            let pricing = ModelPricing::default();

            // "claude-opus-4-6" (len 16) must beat "claude-opus-4" (len 13)
            let tier = pricing.get_tier("claude-opus-4-6");
            assert!(
                (tier.input_per_million - 5.0).abs() < 0.01,
                "Opus 4.6 got ${}/M instead of $5/M — prefix match is nondeterministic",
                tier.input_per_million
            );

            // "claude-opus-4-5-20251101" should match "claude-opus-4-5" (len 15)
            // not "claude-opus-4" (len 13)
            let tier = pricing.get_tier("claude-opus-4-5-20251101");
            assert!(
                (tier.input_per_million - 5.0).abs() < 0.01,
                "Opus 4.5 versioned got ${}/M instead of $5/M",
                tier.input_per_million
            );

            // "claude-3-5-haiku-20250101" should match "claude-3-5-haiku" (len 16)
            // not "claude-3-5" which doesn't exist, but shouldn't match
            // "claude-3-haiku" (len 14) either
            let tier = pricing.get_tier("claude-3-5-haiku-20250101");
            assert!(
                (tier.input_per_million - 0.80).abs() < 0.01,
                "Haiku 3.5 versioned got ${}/M instead of $0.80/M",
                tier.input_per_million
            );
        }
    }
}
