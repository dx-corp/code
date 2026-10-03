//! Cost provenance for native coding usage. Persist applied catalog rates so
//! later catalog refreshes cannot silently change a recorded estimate.

use maestro_runtime::TokenUsage;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum UsageCostSource {
    ProviderReported,
    ModelPriced,
    // Old numeric records do not identify whether the provider supplied them.
    LegacyRecorded,
    #[serde(other)]
    Unpriced,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UsageCost {
    pub(crate) cost: Option<f64>,
    pub(crate) cost_source: UsageCostSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pricing_version: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pricing_rates: Option<PricingRates>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PricingRates {
    input_per_million: f64,
    output_per_million: f64,
    cache_read_per_million: f64,
    cache_write_per_million: f64,
}

pub(crate) fn price_usage(provider: &str, model: &str, usage: &TokenUsage) -> UsageCost {
    let unpriced = || UsageCost {
        cost: None,
        cost_source: UsageCostSource::Unpriced,
        pricing_version: None,
        pricing_rates: None,
    };
    if let Some(cost) = usage.cost.filter(|cost| cost.is_finite()) {
        return UsageCost {
            cost: Some(cost),
            cost_source: UsageCostSource::ProviderReported,
            pricing_version: None,
            pricing_rates: None,
        };
    }

    // The Codex adapter currently retains inclusive input-token totals,
    // unlike the provider adapters' uncached input category. Until its typed
    // contract is normalized, cached Codex usage cannot be estimated safely.
    if provider == "openai-codex" && (usage.cache_read_tokens > 0 || usage.cache_write_tokens > 0) {
        return unpriced();
    }

    let qualified_model = format!("{provider}/{model}");
    let Some(rates) = maestro_local_host::model_catalog::bundled_rates(&qualified_model) else {
        return unpriced();
    };
    let Some(facts) = maestro_local_host::model_facts_generated::model_facts(&qualified_model)
    else {
        return unpriced();
    };
    // bundled_rates uses zero for absent cache rates. That compatibility
    // default is not evidence that an observed cache category was free.
    if (usage.cache_read_tokens > 0 && facts.cache_read_per_million.is_none())
        || (usage.cache_write_tokens > 0 && facts.cache_write_per_million.is_none())
    {
        return unpriced();
    }
    let cost = (usage.input_tokens as f64 * rates.input_per_million
        + usage.output_tokens as f64 * rates.output_per_million
        + usage.cache_read_tokens as f64 * rates.cache_read_per_million
        + usage.cache_write_tokens as f64 * rates.cache_write_per_million)
        / 1_000_000.0;
    if !cost.is_finite() {
        return unpriced();
    }
    UsageCost {
        cost: Some(cost),
        cost_source: UsageCostSource::ModelPriced,
        pricing_version: Some(maestro_local_host::model_catalog::bundled_catalog_version()),
        pricing_rates: Some(PricingRates {
            input_per_million: rates.input_per_million,
            output_per_million: rates.output_per_million,
            cache_read_per_million: rates.cache_read_per_million,
            cache_write_per_million: rates.cache_write_per_million,
        }),
    }
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UsageCostCounts {
    provider_reported: u64,
    model_priced: u64,
    legacy_recorded: u64,
    unpriced: u64,
}

impl UsageCostCounts {
    pub(crate) fn add(&mut self, source: UsageCostSource) {
        match source {
            UsageCostSource::ProviderReported => self.provider_reported += 1,
            UsageCostSource::ModelPriced => self.model_priced += 1,
            UsageCostSource::LegacyRecorded => self.legacy_recorded += 1,
            UsageCostSource::Unpriced => self.unpriced += 1,
        }
    }

    pub(crate) fn known(&self) -> u64 {
        self.provider_reported + self.model_priced + self.legacy_recorded
    }

    pub(crate) fn unpriced(&self) -> u64 {
        self.unpriced
    }
}

#[cfg(test)]
#[path = "usage_cost_tests.rs"]
mod usage_cost_tests;
