//! Provider-scoped request capabilities shared by transport and model inspection.
use crate::{AiProvider, provider_model_name};
use serde::Serialize;

pub const ASTRA_CONTEXT_TOKENS: u32 = 1_050_000;
pub const ASTRA_OUTPUT_TOKENS: u32 = 128_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OpenAiWireProtocol {
    #[serde(rename = "openai-chat")]
    OpenAiChat,
    #[serde(rename = "openai-responses")]
    OpenAiResponses,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenAiRequestCapabilities {
    pub protocol: OpenAiWireProtocol,
    pub temperature: bool,
    pub reasoning_budget_levels: [&'static str; 3],
    pub maximum_reasoning_effort: &'static str,
    pub context_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
}

/// Thinking wire mode supported by a direct Anthropic model family.
///
/// This is deliberately a transport classification. It does not describe how
/// the model is displayed in the picker or how a user's thinking preference is
/// labelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AnthropicThinkingMode {
    /// Legacy extended thinking uses `type: enabled` and a token budget.
    Extended,
    /// Current Claude models use `type: adaptive` without a budget field.
    Adaptive,
    /// The model always thinks; omit the `thinking` request object entirely.
    AlwaysOn,
}

/// Provider-scoped request capabilities for direct Anthropic routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnthropicRequestCapabilities {
    pub thinking: AnthropicThinkingMode,
    pub temperature: bool,
    /// Whether the model accepts the `xhigh` effort level.
    ///
    /// Anthropic documents `xhigh` on Claude Fable 5.1, Mythos 5.1, Fable 5,
    /// Mythos 5, Opus 5.5, Opus 5, Opus 4.8, Opus 4.7, and Sonnet 5. Mythos
    /// Preview, Opus 4.6, and Sonnet 4.6 support `max` without it, so this is
    /// a per-family capability rather than a threshold on the effort ladder.
    pub supports_xhigh: bool,
}

/// Whether a provider route accepts explicit prompt-cache markers for this model.
///
/// Direct Anthropic requests own this wire contract. Bedrock exposes the same
/// capability only for the documented Claude model families; other Bedrock
/// models may use implicit caching and reject explicit `cachePoint` blocks.
#[must_use]
pub fn supports_explicit_prompt_caching(provider: AiProvider, model: &str) -> bool {
    match provider {
        AiProvider::Anthropic => true,
        AiProvider::Bedrock => {
            let normalized = provider_model_name(model).trim().to_ascii_lowercase();
            let Some(start) = normalized.find("anthropic.") else {
                return false;
            };
            let model_id = &normalized[start..];
            [
                "anthropic.claude-fable-5",
                "anthropic.claude-mythos-5",
                "anthropic.claude-mythos-preview",
                "anthropic.claude-opus-5",
                "anthropic.claude-opus-4-8",
                "anthropic.claude-opus-4-7",
                "anthropic.claude-opus-4-6-v1",
                "anthropic.claude-opus-4-5-20251101-v1:0",
                "anthropic.claude-sonnet-5",
                "anthropic.claude-sonnet-4-6",
                "anthropic.claude-sonnet-4-5-20250929-v1:0",
                "anthropic.claude-3-7-sonnet-20250219-v1:0",
                "anthropic.claude-3-5-sonnet-20241022-v2:0",
                "anthropic.claude-haiku-4-5-20251001-v1:0",
            ]
            .iter()
            .any(|family| {
                model_id == *family
                    || model_id
                        .strip_prefix(family)
                        .is_some_and(|suffix| suffix.starts_with('-') || suffix.starts_with(':'))
            })
        }
        _ => false,
    }
}

impl AnthropicRequestCapabilities {
    /// Map the existing token-budget control to a supported modern effort
    /// level. Anthropic's adaptive and always-on models accept `low`,
    /// `medium`, `high`, and `max`, and most of them also accept `xhigh`;
    /// legacy extended-thinking models retain their `budget_tokens` contract.
    ///
    /// The budget boundaries mirror `ThinkingLevel::to_config`. A model
    /// without `xhigh` maps the `XHigh` budget up to `max`, which is the
    /// nearest level it does accept; `normalize_thinking` then reports `Max`
    /// so the picker never offers a level the route would reject.
    #[must_use]
    pub fn effort_for_budget(&self, budget_tokens: u32) -> Option<&'static str> {
        if !matches!(
            self.thinking,
            AnthropicThinkingMode::Adaptive | AnthropicThinkingMode::AlwaysOn
        ) {
            return None;
        }

        Some(if budget_tokens > 32_000 {
            "max"
        } else if budget_tokens > 20_000 {
            if self.supports_xhigh { "xhigh" } else { "max" }
        } else if budget_tokens > 10_000 {
            "high"
        } else if budget_tokens > 4_096 {
            "medium"
        } else {
            "low"
        })
    }
}

/// Resolve the Anthropic wire contract from the provider route and model id.
///
/// Model family matching is kept here so callers use the same typed
/// capabilities when inspecting a model and when constructing its request.
/// Non-Anthropic routes intentionally receive the conservative legacy mode;
/// this helper must not infer provider behaviour from a display name routed by
/// another provider.
#[must_use]
pub fn anthropic_request_capabilities(
    provider: Option<&str>,
    model: &str,
) -> AnthropicRequestCapabilities {
    let model_id = anthropic_model_id(provider, model);
    let normalized = model_id.as_deref().unwrap_or_default();

    let thinking = if is_model_family(normalized, "claude-fable-5")
        || is_model_family(normalized, "claude-mythos-5")
        || is_model_family(normalized, "claude-mythos-preview")
    {
        AnthropicThinkingMode::AlwaysOn
    } else if is_model_family(normalized, "claude-opus-5")
        || is_model_family(normalized, "claude-sonnet-5")
        || is_model_family(normalized, "claude-opus-4-8")
        || is_model_family(normalized, "claude-opus-4-7")
        || is_model_family(normalized, "claude-opus-4-6")
        || is_model_family(normalized, "claude-sonnet-4-6")
        || is_model_family(normalized, "claude-opus-latest")
        || is_model_family(normalized, "claude-sonnet-latest")
    {
        AnthropicThinkingMode::Adaptive
    } else {
        AnthropicThinkingMode::Extended
    };

    let temperature = model_id.as_deref().is_none_or(|model| {
        !is_anthropic_opus_4_family_for_capabilities(model)
            && !is_model_family(model, "claude-fable-5")
            && !is_model_family(model, "claude-mythos-5")
            && !is_model_family(model, "claude-mythos-preview")
            && !is_model_family(model, "claude-opus-5")
            && !is_model_family(model, "claude-sonnet-5")
            && !is_model_family(model, "claude-sonnet-latest")
    });

    let supports_xhigh = is_model_family(normalized, "claude-fable-5")
        || is_model_family(normalized, "claude-mythos-5")
        || is_model_family(normalized, "claude-opus-5")
        || is_model_family(normalized, "claude-opus-4-8")
        || is_model_family(normalized, "claude-opus-4-7")
        || is_model_family(normalized, "claude-sonnet-5");

    AnthropicRequestCapabilities {
        thinking,
        temperature,
        supports_xhigh,
    }
}

fn anthropic_model_id(provider: Option<&str>, model: &str) -> Option<String> {
    let stripped = strip_managed_model_prefix(model.trim());
    let inferred_provider = stripped
        .split_once('/')
        .map(|(name, _)| name.trim())
        .or_else(|| {
            stripped
                .get(..7)
                .filter(|prefix| prefix.eq_ignore_ascii_case("claude-"))
                .map(|_| "anthropic")
        });
    let provider = provider.or(inferred_provider);
    let is_anthropic = provider.is_some_and(|name| {
        name.eq_ignore_ascii_case("anthropic") || name.eq_ignore_ascii_case("claude")
    });
    if !is_anthropic {
        return None;
    }

    let normalized = provider_model_name(stripped).trim().to_ascii_lowercase();
    let normalized = normalized
        .strip_prefix("anthropic/")
        .or_else(|| normalized.strip_prefix("claude/"))
        .unwrap_or(&normalized);
    normalized
        .starts_with("claude-")
        .then(|| normalized.replace('.', "-"))
}

fn is_model_family(model: &str, family: &str) -> bool {
    model == family
        || model
            .strip_prefix(family)
            .is_some_and(|suffix| suffix.starts_with('-'))
}

fn is_anthropic_opus_4_family_for_capabilities(model: &str) -> bool {
    is_model_family(model, "claude-opus-4") || is_model_family(model, "claude-opus-latest")
}

/// Resolve the actual route before applying model-specific restrictions.
#[must_use]
pub fn openai_request_capabilities(
    provider: Option<&str>,
    model: &str,
) -> OpenAiRequestCapabilities {
    let stripped = strip_managed_model_prefix(model.trim());
    let provider = provider.or_else(|| stripped.split_once('/').map(|(p, _)| p));
    let local = provider.is_some_and(|p| {
        ["llamacpp", "lmstudio", "ollama"]
            .iter()
            .any(|v| p.eq_ignore_ascii_case(v))
    });
    let astra = !local
        && stripped
            .rsplit('/')
            .next()
            .is_some_and(|v| v.eq_ignore_ascii_case("gpt-6-astra"));
    let responses = uses_responses_api(provider, model);
    OpenAiRequestCapabilities {
        protocol: if responses {
            OpenAiWireProtocol::OpenAiResponses
        } else {
            OpenAiWireProtocol::OpenAiChat
        },
        temperature: !responses && !astra,
        reasoning_budget_levels: if provider == Some("llamacpp")
            && stripped
                .rsplit('/')
                .next()
                .is_some_and(|name| name.to_ascii_lowercase().starts_with("qwen3.8"))
        {
            ["low", "medium", "xhigh"]
        } else {
            ["low", "medium", "high"]
        },
        maximum_reasoning_effort: if !local && matches!(provider, Some("openai" | "openrouter")) {
            let name = stripped.rsplit('/').next().unwrap_or(stripped);
            if name.starts_with("gpt-5.6-") {
                "max"
            } else if [
                "gpt-5.2", "gpt-5.3", "gpt-5.4", "gpt-5.5", "gpt-5.6", "gpt-5.7",
            ]
            .iter()
            .any(|prefix| {
                name == *prefix
                    || name
                        .strip_prefix(prefix)
                        .is_some_and(|rest| rest.starts_with("-"))
            }) {
                "xhigh"
            } else {
                "high"
            }
        } else if provider == Some("llamacpp")
            && stripped
                .rsplit('/')
                .next()
                .is_some_and(|name| name.to_ascii_lowercase().starts_with("qwen3.8"))
        {
            "xhigh"
        } else {
            "high"
        },
        context_tokens: astra.then_some(ASTRA_CONTEXT_TOKENS),
        output_tokens: astra.then_some(ASTRA_OUTPUT_TOKENS),
    }
}

fn strip_managed_model_prefix(model: &str) -> &str {
    for prefix in ["evalops/", "maestro-managed/"] {
        if let Some(candidate) = model.get(..prefix.len()) {
            if candidate.eq_ignore_ascii_case(prefix) {
                return &model[prefix.len()..];
            }
        }
    }
    model
}

fn has_managed_model_prefix(model: &str) -> bool {
    let model = model.trim();
    ["evalops/", "maestro-managed/"].iter().any(|prefix| {
        model
            .get(..prefix.len())
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix))
    })
}

fn strip_provider_model_prefix<'a>(model: &'a str, provider: &str) -> &'a str {
    let Some((prefix, model_id)) = model.split_once('/') else {
        return model;
    };
    if prefix.eq_ignore_ascii_case(provider) && !model_id.trim().is_empty() {
        model_id.trim()
    } else {
        model
    }
}

fn uses_responses_api(provider: Option<&str>, model: &str) -> bool {
    let managed_namespace = has_managed_model_prefix(model);
    let model = strip_managed_model_prefix(model).trim();
    let inferred_provider = model.split_once('/').map(|(provider, _)| provider.trim());
    let provider = provider.or(inferred_provider);
    let is_native_local = provider.is_some_and(|provider| {
        ["llamacpp", "lmstudio", "ollama"]
            .iter()
            .any(|local| provider.eq_ignore_ascii_case(local))
    });
    if is_native_local {
        return false;
    }
    let is_openrouter =
        provider.is_some_and(|provider| provider.eq_ignore_ascii_case("openrouter"));
    let normalized = provider_model_name(model);
    let normalized = if is_openrouter && !managed_namespace {
        let routed_model = strip_provider_model_prefix(&normalized, "openrouter");
        provider_model_name(routed_model)
    } else {
        normalized
    };
    let normalized = normalized.to_ascii_lowercase();

    if is_openrouter {
        return normalized == "gpt-5.6";
    }

    // Direct OpenAI and managed OpenAI routes use the Responses families
    // already supported by the native client.
    normalized.contains("codex")
        || normalized.starts_with("gpt-5")
        || normalized == "gpt-6-astra"
        || normalized.starts_with("o3")
}

impl OpenAiRequestCapabilities {
    /// Map the existing budget contract to a supported wire value.
    #[must_use]
    pub fn reasoning_effort(&self, budget_tokens: u32) -> &str {
        if budget_tokens > 20000 {
            return self.maximum_reasoning_effort;
        }
        self.reasoning_budget_levels[if budget_tokens > 10000 {
            2
        } else if budget_tokens > 4096 {
            1
        } else {
            0
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_prompt_cache_capability_is_provider_and_model_scoped() {
        for (provider, model) in [
            (AiProvider::Anthropic, "anthropic/claude-sonnet-4-6"),
            (
                AiProvider::Bedrock,
                "bedrock/anthropic.claude-sonnet-4-5-20250929-v1:0",
            ),
            (
                AiProvider::Bedrock,
                "bedrock/us.anthropic.claude-3-5-sonnet-20241022-v2:0",
            ),
        ] {
            assert!(
                supports_explicit_prompt_caching(provider, model),
                "{provider:?}/{model}"
            );
        }

        for (provider, model) in [
            (AiProvider::Bedrock, "bedrock/amazon.nova-pro-v1:0"),
            (
                AiProvider::Bedrock,
                "bedrock/anthropic.claude-3-haiku-20240307-v1:0",
            ),
            (AiProvider::OpenAI, "openai/gpt-5.6"),
        ] {
            assert!(
                !supports_explicit_prompt_caching(provider, model),
                "{provider:?}/{model}"
            );
        }
    }

    #[test]
    fn maximum_effort_is_provider_and_model_specific() {
        for (model, expected) in [
            ("gpt-5.4", "xhigh"),
            ("gpt-5.5", "xhigh"),
            ("gpt-5.6", "xhigh"),
            ("gpt-5.7", "xhigh"),
            ("gpt-5.6-luna", "max"),
            ("unknown", "high"),
        ] {
            let capabilities = openai_request_capabilities(Some("openai"), model);
            assert_eq!(capabilities.reasoning_effort(20_000), "high");
            assert_eq!(capabilities.reasoning_effort(50_000), expected);
        }
        assert_eq!(
            openai_request_capabilities(Some("ollama"), "gpt-5.6-luna").reasoning_effort(50_000),
            "high"
        );
    }
    #[test]
    fn routes_preserve_provider_capabilities() {
        for (provider, model, responses, temperature) in [
            ("openai", "gpt-6-astra", true, false),
            ("openrouter", "openai/gpt-6-astra", false, false),
            ("ollama", "gpt-6-astra", false, true),
            ("lmstudio", "gpt-6-astra", false, true),
            ("llamacpp", "gpt-6-astra", false, true),
            ("openrouter", "openai/gpt-5.6-terra", false, true),
        ] {
            let c = openai_request_capabilities(Some(provider), model);
            assert_eq!(
                c.protocol == OpenAiWireProtocol::OpenAiResponses,
                responses,
                "{provider}/{model}"
            );
            assert_eq!(c.temperature, temperature, "{provider}/{model}");
            assert_eq!(
                c.context_tokens.is_some(),
                model.ends_with("astra") && !temperature
            );
        }
    }
    #[test]
    fn local_reasoning_mapping_preserves_vendor_namespaces() {
        for model in ["qwen3.8", "Qwen/Qwen3.8-27B", "llamacpp/Qwen/Qwen3.8-27B"] {
            let local = openai_request_capabilities(Some("llamacpp"), model);
            assert_eq!(local.reasoning_effort(12_000), "xhigh", "{model}");
            // The CLI's Low setting is 4,096 tokens on every direct adapter.
            for budget in [3_000, 4_000, 4_096] {
                assert_eq!(local.reasoning_effort(budget), "low", "{model}/{budget}");
            }
            for budget in [4_097, 10_000] {
                assert_eq!(local.reasoning_effort(budget), "medium", "{model}/{budget}");
            }
            assert_eq!(
                openai_request_capabilities(Some("openrouter"), model).reasoning_effort(12_000),
                "high"
            );
        }
    }

    #[test]
    fn managed_and_explicit_routes_agree() {
        assert_eq!(
            openai_request_capabilities(None, "maestro-managed/openai/gpt-6-astra"),
            openai_request_capabilities(Some("openai"), "gpt-6-astra")
        );
    }

    #[test]
    fn xhigh_effort_is_gated_on_the_documented_model_families() {
        // ThinkingLevel::XHigh carries a 32,000-token budget.
        const XHIGH_BUDGET: u32 = 32_000;

        for model in [
            "claude-opus-5-5",
            "claude-opus-5",
            "claude-opus-4-8",
            "claude-opus-4-7",
            "claude-sonnet-5",
            "claude-fable-5-1",
            "claude-fable-5",
            "claude-mythos-5-1",
        ] {
            let caps = anthropic_request_capabilities(Some("anthropic"), model);
            assert!(caps.supports_xhigh, "{model} should accept xhigh");
            assert_eq!(
                caps.effort_for_budget(XHIGH_BUDGET),
                Some("xhigh"),
                "{model}"
            );
        }

        // These accept `max` but not `xhigh`, so the level maps up to `max`
        // rather than being sent as an effort the route would reject.
        for model in [
            "claude-opus-4-6",
            "claude-sonnet-4-6",
            "claude-mythos-preview",
        ] {
            let caps = anthropic_request_capabilities(Some("anthropic"), model);
            assert!(!caps.supports_xhigh, "{model} should not accept xhigh");
            assert_eq!(caps.effort_for_budget(XHIGH_BUDGET), Some("max"), "{model}");
        }

        // The other levels are unchanged by the new band.
        let caps = anthropic_request_capabilities(Some("anthropic"), "claude-opus-5-5");
        assert_eq!(caps.effort_for_budget(50_000), Some("max"));
        assert_eq!(caps.effort_for_budget(20_000), Some("high"));
        assert_eq!(caps.effort_for_budget(10_000), Some("medium"));
        assert_eq!(caps.effort_for_budget(4_096), Some("low"));

        // Legacy extended-thinking models keep their budget_tokens contract.
        assert_eq!(
            anthropic_request_capabilities(Some("anthropic"), "claude-opus-4-5")
                .effort_for_budget(XHIGH_BUDGET),
            None
        );
    }

    #[test]
    fn anthropic_thinking_modes_follow_documented_model_families() {
        for model in [
            "claude-fable-5",
            "claude-fable-5-1-20260901",
            "anthropic/claude-mythos-5.1",
            "claude-mythos-preview",
        ] {
            assert_eq!(
                anthropic_request_capabilities(Some("anthropic"), model).thinking,
                AnthropicThinkingMode::AlwaysOn,
                "{model}"
            );
        }

        for model in [
            "claude-opus-5",
            "claude-sonnet-5-20260901",
            "claude-opus-4.8",
            "claude-opus-4-7-20260520",
            "claude-opus-4-6",
            "claude-sonnet-4-6",
        ] {
            assert_eq!(
                anthropic_request_capabilities(Some("anthropic"), model).thinking,
                AnthropicThinkingMode::Adaptive,
                "{model}"
            );
        }

        for model in [
            "claude-opus-4-5-20251101",
            "claude-sonnet-4-5",
            "claude-haiku-4-5",
            "claude-3-opus-20240229",
        ] {
            assert_eq!(
                anthropic_request_capabilities(Some("anthropic"), model).thinking,
                AnthropicThinkingMode::Extended,
                "{model}"
            );
        }
    }

    #[test]
    fn anthropic_capabilities_are_route_scoped() {
        assert_eq!(
            anthropic_request_capabilities(None, "anthropic/claude-opus-4.7").thinking,
            AnthropicThinkingMode::Adaptive
        );
        assert_eq!(
            anthropic_request_capabilities(None, "claude-fable-5-1").thinking,
            AnthropicThinkingMode::AlwaysOn
        );
        assert_eq!(
            anthropic_request_capabilities(Some("openrouter"), "anthropic/claude-fable-5").thinking,
            AnthropicThinkingMode::Extended
        );
    }

    #[test]
    fn modern_anthropic_effort_preserves_budget_intent() {
        // An arbitrary budget rounds up to the next exposed effort level. Opus
        // 4.7 accepts xhigh, so the band above High stops at xhigh rather than
        // jumping to max.
        let capabilities = anthropic_request_capabilities(Some("anthropic"), "claude-opus-4-7");
        for (budget, effort) in [
            (4_096, "low"),
            (4_097, "medium"),
            (10_001, "high"),
            (20_001, "xhigh"),
            (32_000, "xhigh"),
            (32_001, "max"),
        ] {
            assert_eq!(
                capabilities.effort_for_budget(budget),
                Some(effort),
                "budget {budget}"
            );
        }

        // Opus 4.6 accepts max without xhigh, so the same band maps to max.
        let no_xhigh = anthropic_request_capabilities(Some("anthropic"), "claude-opus-4-6");
        for budget in [20_001, 32_000, 32_001] {
            assert_eq!(
                no_xhigh.effort_for_budget(budget),
                Some("max"),
                "budget {budget}"
            );
        }
        assert_eq!(no_xhigh.effort_for_budget(20_000), Some("high"));

        assert_eq!(
            anthropic_request_capabilities(Some("anthropic"), "claude-opus-4-5")
                .effort_for_budget(50_000),
            None
        );
    }
}
