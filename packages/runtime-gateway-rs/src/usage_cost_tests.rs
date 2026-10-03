use super::*;
use serde_json::json;

fn usage(cost: Option<f64>) -> TokenUsage {
    TokenUsage {
        input_tokens: 1_000,
        output_tokens: 100,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        cost,
    }
}

#[test]
fn usage_cost_preserves_provider_zero_before_model_pricing() {
    let record = price_usage("openai", "gpt-4o", &usage(Some(0.0)));
    let value = serde_json::to_value(record).unwrap();
    assert_eq!(value["cost"], 0.0);
    assert_eq!(value["costSource"], "providerReported");
    assert!(value.get("pricingVersion").is_none());
}

#[test]
fn usage_cost_unknown_model_serializes_absence_as_unpriced() {
    let record = price_usage("local", "not-in-the-catalog", &usage(None));
    let value = serde_json::to_value(record).unwrap();
    assert_eq!(value["cost"], json!(null));
    assert_eq!(value["costSource"], "unpriced");
}

#[test]
fn usage_cost_prices_with_existing_catalog_and_pins_applied_rates() {
    let rates = maestro_local_host::model_catalog::bundled_rates("openai/gpt-4o").unwrap();
    let record = price_usage("openai", "gpt-4o", &usage(None));
    let value = serde_json::to_value(record).unwrap();
    let expected =
        (1_000.0 * rates.input_per_million + 100.0 * rates.output_per_million) / 1_000_000.0;
    assert_eq!(value["cost"], expected);
    assert_eq!(value["costSource"], "modelPriced");
    assert_eq!(
        value["pricingVersion"],
        maestro_local_host::model_catalog::bundled_catalog_version()
    );
    assert_eq!(
        value["pricingRates"]["inputPerMillion"],
        rates.input_per_million
    );
}

#[test]
fn usage_cost_does_not_price_missing_cache_rates_as_zero() {
    let mut tokens = usage(None);
    tokens.cache_write_tokens = 1;
    // This model has published input/output rates but no cache-write rate.
    let value = serde_json::to_value(price_usage("openai", "gpt-4o", &tokens)).unwrap();
    assert_eq!(value["costSource"], "unpriced");
    assert!(value["cost"].is_null());
}

#[test]
fn usage_cost_nonfinite_provider_cost_does_not_override_known_rates() {
    let value =
        serde_json::to_value(price_usage("openai", "gpt-4o", &usage(Some(f64::NAN)))).unwrap();
    assert_eq!(value["costSource"], "modelPriced");
    assert!(value["cost"].as_f64().unwrap().is_finite());
}

#[test]
fn usage_cost_assistant_message_does_not_fabricate_category_costs() {
    let message = crate::chat::composer_assistant_message("done", "", Some(usage(Some(0.0))));
    assert_eq!(message["usage"]["cost"]["total"], 0.0);
    assert_eq!(message["usage"]["costSource"], "providerReported");
    for category in ["input", "output", "cacheRead", "cacheWrite"] {
        assert!(message["usage"]["cost"][category].is_null());
    }
    let message = crate::chat::composer_assistant_message("done", "", Some(usage(None)));
    assert!(message["usage"]["cost"]["total"].is_null());
    assert_eq!(message["usage"]["costSource"], "unpriced");
}

#[test]
fn usage_cost_codex_inclusive_cached_input_is_not_double_priced() {
    let mut tokens = usage(None);
    tokens.cache_read_tokens = 700;
    let value = serde_json::to_value(price_usage("openai-codex", "gpt-4o", &tokens)).unwrap();
    assert!(value["cost"].is_null());
    assert_eq!(value["costSource"], "unpriced");
    tokens.cost = Some(0.01);
    let value = serde_json::to_value(price_usage("openai-codex", "gpt-4o", &tokens)).unwrap();
    assert_eq!(value["cost"], 0.01);
    assert_eq!(value["costSource"], "providerReported");
}
