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
    /// Whether the route accepts a reasoning-effort field at all. A model
    /// without an effort ladder (a non-reasoning GPT-4 model, a Grok model
    /// that reasons but exposes no control) must not be sent one.
    pub reasoning_effort_supported: bool,
    pub reasoning_budget_levels: [&'static str; 3],
    /// The lowest effort the model accepts. The `pro` tiers reject anything
    /// below it: OpenAI documents `gpt-5-pro` as `high` only and the later
    /// `-pro` tiers as `medium` and up, and returns 400 for a lower value.
    pub minimum_reasoning_effort: &'static str,
    pub maximum_reasoning_effort: &'static str,
    pub context_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
}

/// Effort levels in ladder order, shared by every OpenAI-compatible route.
const EFFORT_LADDER: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];

fn effort_rank(effort: &str) -> usize {
    EFFORT_LADDER
        .iter()
        .position(|level| *level == effort)
        .unwrap_or(usize::MAX)
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

/// Direct Anthropic families whose thinking is always on: the request carries
/// no `thinking` object and depth is steered with `output_config.effort`.
/// Anthropic's models overview lists Fable 5.1 and Opus 5.5 as "Adaptive
/// (always on)"; Fable 5 and Mythos 5 are documented alongside them on the
/// thinking overview. Sending `thinking: {"type": "disabled"}` to one of these
/// returns 400 at every effort level.
///
/// Whether thinking can be turned off is not derivable from the bundled
/// catalog: models.dev carries no such field, and Anthropic's Models API
/// `capabilities.thinking.types` reports only `adaptive` and `enabled`. This
/// list is therefore hand-maintained; `every_current_anthropic_model_has_the_documented_capabilities`
/// pins it row by row and `capability_matrix_covers_every_anthropic_model_in_the_bundled_catalog`
/// forces a new catalog model to be classified here before it ships.
const ALWAYS_ON_THINKING_FAMILIES: &[&str] = &[
    "claude-opus-5-5",
    "claude-fable-5",
    "claude-mythos-5",
    "claude-mythos-preview",
];

/// Direct Anthropic families on the adaptive thinking wire (`type: adaptive`
/// plus `output_config.effort`) that still accept `type: disabled`. The
/// catalog does derive this half: every one of these carries `max` in its
/// effort ladder, and no extended-thinking model does
/// (`anthropic_capabilities_follow_the_catalog_effort_ladder`).
const ADAPTIVE_THINKING_FAMILIES: &[&str] = &[
    "claude-opus-5",
    "claude-sonnet-5",
    "claude-opus-4-8",
    "claude-opus-4-7",
    "claude-opus-4-6",
    "claude-sonnet-4-6",
    "claude-opus-latest",
    "claude-sonnet-latest",
];

/// Direct Anthropic families that are sent no `temperature`. models.dev
/// lists `temperature: false` for Fable 5/5.1, Opus 4.7/4.8/5/5.5 and Sonnet
/// 5. Opus 4.5 and 4.6 are listed as accepting it and are omitted here on
/// purpose (see `is_anthropic_opus_4_family_for_capabilities`): omitting a
/// sampling parameter cannot fail a request, sending one a model rejects
/// returns 400.
const NO_TEMPERATURE_FAMILIES: &[&str] = &[
    "claude-fable-5",
    "claude-mythos-5",
    "claude-mythos-preview",
    "claude-opus-5",
    "claude-sonnet-5",
    "claude-sonnet-latest",
];

/// Direct Anthropic families that accept `xhigh`. Derivable from the catalog
/// (`reasoning_efforts` contains `xhigh`) and pinned against it by
/// `anthropic_capabilities_follow_the_catalog_effort_ladder`.
const XHIGH_FAMILIES: &[&str] = &[
    "claude-fable-5",
    "claude-mythos-5",
    "claude-opus-5",
    "claude-opus-4-8",
    "claude-opus-4-7",
    "claude-sonnet-5",
];

/// Families named in the lists above that the bundled catalog does not carry,
/// each with the reason. `anthropic_family_lists_name_only_catalogued_or_exempted_models`
/// fails on any other uncatalogued family, so a retired or misspelled id
/// cannot sit in a list unnoticed.
#[cfg(test)]
const UNCATALOGUED_ANTHROPIC_FAMILIES: &[(&str, &str)] = &[
    (
        "claude-mythos-5",
        "documented by Anthropic (effort levels and thinking overview name Mythos 5 and Mythos 5.1) \
         but models.dev lists claude-mythos-5 only under its Azure providers, not the direct \
         Anthropic provider the catalog is built from",
    ),
    (
        "claude-mythos-preview",
        "documented by Anthropic (thinking overview: supports both thinking modes; effort levels: \
         accepts max) but absent from every models.dev provider",
    ),
    (
        "claude-opus-latest",
        "Anthropic alias resolved at request time; the catalog carries snapshot ids only",
    ),
    (
        "claude-sonnet-latest",
        "Anthropic alias resolved at request time; the catalog carries snapshot ids only",
    ),
];

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
                "anthropic.claude-opus-5-5",
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

    // Always-on is checked first: `claude-opus-5-5` is also in the
    // `claude-opus-5` family, and that prefix match is how Opus 5.5 shipped
    // classified Adaptive and returned 400 on every thinking-off request.
    let thinking = if in_any_family(normalized, ALWAYS_ON_THINKING_FAMILIES) {
        AnthropicThinkingMode::AlwaysOn
    } else if in_any_family(normalized, ADAPTIVE_THINKING_FAMILIES) {
        AnthropicThinkingMode::Adaptive
    } else {
        AnthropicThinkingMode::Extended
    };

    let temperature = model_id.as_deref().is_none_or(|model| {
        !is_anthropic_opus_4_family_for_capabilities(model)
            && !in_any_family(model, NO_TEMPERATURE_FAMILIES)
    });

    let supports_xhigh = in_any_family(normalized, XHIGH_FAMILIES);

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

fn in_any_family(model: &str, families: &[&str]) -> bool {
    families.iter().any(|family| is_model_family(model, family))
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
    let name = stripped.rsplit('/').next().unwrap_or(stripped);
    let lower = name.to_ascii_lowercase();
    let direct_openai = !local && provider.is_some_and(|p| p.eq_ignore_ascii_case("openai"));
    let direct_xai = !local
        && provider
            .is_some_and(|p| p.eq_ignore_ascii_case("xai") || p.eq_ignore_ascii_case("grok"));
    // Chat-protocol OpenAI reasoning models. models.dev lists
    // `temperature: false` for gpt-6-luna, gpt-6-sol and gpt-realtime-2.1;
    // OpenAI rejects the parameter on its reasoning models with 400
    // "Unsupported parameter: 'temperature'".
    let openai_chat_reasoning =
        direct_openai && (lower.starts_with("gpt-6") || lower.starts_with("gpt-realtime"));
    let reasoning_effort_supported = if direct_openai {
        // GPT-4-generation chat models have no effort ladder and reject the
        // field. Every other direct OpenAI model, including an unrecognised
        // name, keeps the field: the catalog guard pins the known rows.
        !(lower.starts_with("gpt-4")
            || lower.starts_with("gpt-3.5")
            || lower.starts_with("chatgpt-"))
    } else if direct_xai {
        // xAI documents `reasoning_effort` on grok-4.5 and later; the
        // bundled catalog lists no ladder for grok-build and the
        // grok-4.20 snapshots.
        ["grok-4.3", "grok-4.5", "grok-4.6", "grok-4.7"]
            .iter()
            .any(|family| is_model_family(&lower, family))
    } else {
        true
    };
    let minimum_reasoning_effort = if direct_openai && is_model_family(&lower, "gpt-5-pro") {
        "high"
    } else if direct_openai && lower.ends_with("-pro") && lower.starts_with("gpt-5.") {
        "medium"
    } else {
        "minimal"
    };
    OpenAiRequestCapabilities {
        protocol: if responses {
            OpenAiWireProtocol::OpenAiResponses
        } else {
            OpenAiWireProtocol::OpenAiChat
        },
        temperature: !responses && !astra && !openai_chat_reasoning,
        reasoning_effort_supported,
        minimum_reasoning_effort,
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
            // Ladder tops from the bundled catalog's `reasoning_efforts`
            // (models.dev): the GPT-5.6 and GPT-6 families end at `max`, the
            // GPT-5.2 to 5.5 families and the realtime model at `xhigh`,
            // everything earlier at `high`. Pinned model by model against the
            // catalog by `openai_and_xai_efforts_follow_the_catalog_ladder`.
            if is_model_family(&lower, "gpt-5.6")
                || lower.starts_with("gpt-6")
                || is_model_family(&lower, "gpt-5.7")
            {
                "max"
            } else if ["gpt-5.2", "gpt-5.3", "gpt-5.4", "gpt-5.5"]
                .iter()
                .any(|family| is_model_family(&lower, family))
                || lower.starts_with("gpt-realtime")
            {
                "xhigh"
            } else {
                "high"
            }
        } else if direct_xai {
            // docs.x.ai/docs/guides/reasoning: `xhigh` is available on
            // grok-4.6 and later.
            if is_model_family(&lower, "grok-4.6") || is_model_family(&lower, "grok-4.7") {
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
    ///
    /// The result never falls below `minimum_reasoning_effort`: a `pro`
    /// tier asked for `low` is sent the lowest level it accepts rather than
    /// a value the provider returns 400 for.
    #[must_use]
    pub fn reasoning_effort(&self, budget_tokens: u32) -> &str {
        let effort = if budget_tokens > 20000 {
            self.maximum_reasoning_effort
        } else {
            self.reasoning_budget_levels[if budget_tokens > 10000 {
                2
            } else if budget_tokens > 4096 {
                1
            } else {
                0
            }]
        };
        if effort_rank(effort) < effort_rank(self.minimum_reasoning_effort) {
            self.minimum_reasoning_effort
        } else {
            effort
        }
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
            ("gpt-5.6", "max"),
            ("gpt-5.7", "max"),
            ("gpt-5.6-luna", "max"),
            ("gpt-6-astra", "max"),
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

    /// One row per current Anthropic model: the three request capabilities
    /// `anthropic_request_capabilities` decides, in one place.
    ///
    /// Those three answers come from four separate hand-maintained family
    /// lists in that function: the always-on set, the adaptive set, the
    /// temperature exclusions, and `supports_xhigh`. Shipping one model means
    /// editing up to four of them, and nothing previously checked that a model
    /// appeared in all the right ones. Claude Opus 5.5 shipped missing from
    /// the always-on set, which made every request with thinking off return
    /// 400.
    ///
    /// `thinking` and `xhigh` are the values Anthropic documents. `temperature`
    /// is what Maestro actually sends, which is not the same thing for three
    /// models; see the note on the rows below.
    const ANTHROPIC_CAPABILITY_MATRIX: &[(&str, AnthropicThinkingMode, bool, bool)] = &[
        // model, thinking, sends temperature, accepts xhigh
        (
            "claude-opus-5-5",
            AnthropicThinkingMode::AlwaysOn,
            false,
            true,
        ),
        (
            "claude-opus-5",
            AnthropicThinkingMode::Adaptive,
            false,
            true,
        ),
        (
            "claude-fable-5-1",
            AnthropicThinkingMode::AlwaysOn,
            false,
            true,
        ),
        (
            "claude-fable-5",
            AnthropicThinkingMode::AlwaysOn,
            false,
            true,
        ),
        (
            "claude-sonnet-5",
            AnthropicThinkingMode::Adaptive,
            false,
            true,
        ),
        (
            "claude-opus-4-8",
            AnthropicThinkingMode::Adaptive,
            false,
            true,
        ),
        (
            "claude-opus-4-7",
            AnthropicThinkingMode::Adaptive,
            false,
            true,
        ),
        // Upstream lists temperature as supported on Opus 4.6 and Opus 4.5,
        // but Maestro omits it for the whole claude-opus-4 family
        // (is_anthropic_opus_4_family_for_capabilities). Omitting a sampling
        // parameter cannot fail a request, so this is pinned as the current
        // deliberate behaviour rather than silently changed here.
        (
            "claude-opus-4-6",
            AnthropicThinkingMode::Adaptive,
            false,
            false,
        ),
        (
            "claude-opus-4-5",
            AnthropicThinkingMode::Extended,
            false,
            false,
        ),
        (
            "claude-sonnet-4-6",
            AnthropicThinkingMode::Adaptive,
            true,
            false,
        ),
        (
            "claude-sonnet-4-5",
            AnthropicThinkingMode::Extended,
            true,
            false,
        ),
        (
            "claude-haiku-4-5",
            AnthropicThinkingMode::Extended,
            true,
            false,
        ),
    ];

    #[test]
    fn every_current_anthropic_model_has_the_documented_capabilities() {
        for &(model, thinking, temperature, xhigh) in ANTHROPIC_CAPABILITY_MATRIX {
            let caps = anthropic_request_capabilities(Some("anthropic"), model);
            assert_eq!(caps.thinking, thinking, "{model} thinking");
            assert_eq!(caps.temperature, temperature, "{model} temperature");
            assert_eq!(caps.supports_xhigh, xhigh, "{model} xhigh");
        }
    }

    #[test]
    fn capability_matrix_covers_every_anthropic_model_in_the_bundled_catalog() {
        // The bundled snapshot is regenerated from models.dev by
        // scripts/fetch-model-catalog.mjs. When a new Anthropic model lands in
        // it, this fails until the model is given a row above, which is the
        // one place that forces every family list to be revisited.
        let catalog: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../local-host-rs/src/model_catalog_data.json"
        )))
        .expect("bundled catalog parses");

        let mut missing: Vec<String> = Vec::new();
        for model in catalog["models"].as_array().expect("models array") {
            if model["provider"].as_str() != Some("anthropic") {
                continue;
            }
            let id = model["id"].as_str().expect("model id");
            // A dated snapshot resolves through its dateless family row.
            let covered = ANTHROPIC_CAPABILITY_MATRIX
                .iter()
                .any(|&(family, ..)| is_model_family(id, family));
            if !covered {
                missing.push(id.to_owned());
            }
        }
        assert!(
            missing.is_empty(),
            "Anthropic models in the bundled catalog with no capability row: {missing:?}"
        );
    }

    #[test]
    fn dated_snapshots_inherit_their_family_capabilities() {
        for (dated, family) in [
            ("claude-opus-5-5-20260922", "claude-opus-5-5"),
            ("claude-opus-4-5-20251101", "claude-opus-4-5"),
            ("claude-haiku-4-5-20251001", "claude-haiku-4-5"),
            ("claude-sonnet-4-5-20250929", "claude-sonnet-4-5"),
        ] {
            assert_eq!(
                anthropic_request_capabilities(Some("anthropic"), dated),
                anthropic_request_capabilities(Some("anthropic"), family),
                "{dated} must resolve like {family}"
            );
        }
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

    fn bundled_catalog() -> serde_json::Value {
        serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../local-host-rs/src/model_catalog_data.json"
        )))
        .expect("bundled catalog parses")
    }

    fn catalog_rows(provider: &str) -> Vec<(String, Vec<String>, bool, serde_json::Value)> {
        bundled_catalog()["models"]
            .as_array()
            .expect("models array")
            .iter()
            .filter(|model| model["provider"].as_str() == Some(provider))
            .map(|model| {
                let ladder = model["reasoning_efforts"]
                    .as_array()
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(|value| value.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default();
                (
                    model["id"].as_str().expect("model id").to_owned(),
                    ladder,
                    model["capabilities"]["reasoning"]
                        .as_bool()
                        .unwrap_or(false),
                    model["capabilities"].clone(),
                )
            })
            .collect()
    }

    /// Every thinking budget the UI can send. `Off` carries no effort.
    fn thinking_budgets() -> Vec<u32> {
        maestro_runtime_contracts::ThinkingLevel::ALL
            .iter()
            .map(|level| level.to_config().1)
            .filter(|budget| *budget > 0)
            .collect()
    }

    /// The bundled catalog derives two of the three Anthropic request
    /// capabilities. For every catalogued Anthropic model: `xhigh` is
    /// accepted exactly when the effort ladder lists it; the adaptive wire
    /// (`Adaptive` or `AlwaysOn`) is used exactly when the ladder lists
    /// `max`, which no extended-thinking model does; and every effort the
    /// budget mapping can produce is on the ladder. The third capability,
    /// whether thinking may be disabled, has no catalog field; see
    /// `ALWAYS_ON_THINKING_FAMILIES`.
    #[test]
    fn anthropic_capabilities_follow_the_catalog_effort_ladder() {
        let rows = catalog_rows("anthropic");
        assert!(
            rows.len() >= 10,
            "unexpectedly few Anthropic rows: {}",
            rows.len()
        );
        let mut problems = Vec::new();
        for (id, ladder, _, _) in &rows {
            let caps = anthropic_request_capabilities(Some("anthropic"), id);
            let has = |level: &str| ladder.iter().any(|value| value == level);
            if caps.supports_xhigh != has("xhigh") {
                problems.push(format!(
                    "{id}: supports_xhigh={} but catalog ladder {ladder:?}",
                    caps.supports_xhigh
                ));
            }
            let adaptive_wire = caps.thinking != AnthropicThinkingMode::Extended;
            if adaptive_wire != has("max") {
                problems.push(format!(
                    "{id}: thinking={:?} but catalog ladder {ladder:?}",
                    caps.thinking
                ));
            }
            for budget in thinking_budgets() {
                if let Some(effort) = caps.effort_for_budget(budget) {
                    if !has(effort) {
                        problems.push(format!(
                            "{id}: budget {budget} maps to {effort}, not on catalog ladder {ladder:?}"
                        ));
                    }
                }
            }
        }
        assert!(problems.is_empty(), "{problems:#?}");
    }

    /// Every family named in the Anthropic capability lists must be a
    /// catalogued model (or a family of one), or carry an exemption with a
    /// reason. A retired or misspelled id in a list is otherwise invisible:
    /// the coverage test only checks catalog -> matrix, not matrix -> catalog.
    #[test]
    fn anthropic_family_lists_name_only_catalogued_or_exempted_models() {
        let catalogued: Vec<String> = catalog_rows("anthropic")
            .into_iter()
            .map(|(id, ..)| id)
            .collect();
        let mut unknown = Vec::new();
        for family in ALWAYS_ON_THINKING_FAMILIES
            .iter()
            .chain(ADAPTIVE_THINKING_FAMILIES)
            .chain(NO_TEMPERATURE_FAMILIES)
            .chain(XHIGH_FAMILIES)
        {
            let in_catalog = catalogued.iter().any(|id| is_model_family(id, family));
            let exempted = UNCATALOGUED_ANTHROPIC_FAMILIES
                .iter()
                .any(|(exempt, _)| exempt == family);
            if !in_catalog && !exempted {
                unknown.push(*family);
            }
        }
        assert!(
            unknown.is_empty(),
            "families with no catalog row and no exemption: {unknown:?}"
        );
        // An exemption for a family the catalog now carries is stale.
        for (family, reason) in UNCATALOGUED_ANTHROPIC_FAMILIES {
            assert!(!reason.is_empty(), "{family}: exemption needs a reason");
            assert!(
                !catalogued.iter().any(|id| is_model_family(id, family)),
                "{family} is catalogued now; drop its exemption"
            );
        }
    }

    /// For every catalogued OpenAI and xAI model the effort field is sent
    /// only when the catalog lists a ladder, the ceiling equals the top of
    /// that ladder, and every effort the budget mapping can produce is on it.
    /// `gpt-5-pro` (ladder `[high]`) and the later `-pro` tiers (`[medium,
    /// high, xhigh]`) are the rows that previously failed: a Low or Minimal
    /// thinking level sent `low`, which OpenAI rejects with 400.
    #[test]
    fn openai_and_xai_efforts_follow_the_catalog_ladder() {
        let mut problems = Vec::new();
        let mut checked = 0;
        for provider in ["openai", "xai"] {
            for (id, ladder, reasoning, _) in catalog_rows(provider) {
                checked += 1;
                let caps = openai_request_capabilities(Some(provider), &id);
                if ladder.is_empty() {
                    if caps.reasoning_effort_supported {
                        problems.push(format!(
                            "{provider}/{id}: sends reasoning effort but the catalog lists no \
                             ladder (reasoning={reasoning})"
                        ));
                    }
                    continue;
                }
                if !caps.reasoning_effort_supported {
                    problems.push(format!(
                        "{provider}/{id}: effort disabled but catalog ladder {ladder:?}"
                    ));
                    continue;
                }
                if ladder.last().map(String::as_str) != Some(caps.maximum_reasoning_effort) {
                    problems.push(format!(
                        "{provider}/{id}: maximum {} but catalog ladder {ladder:?}",
                        caps.maximum_reasoning_effort
                    ));
                }
                for budget in thinking_budgets() {
                    let effort = caps.reasoning_effort(budget);
                    if !ladder.iter().any(|value| value == effort) {
                        problems.push(format!(
                            "{provider}/{id}: budget {budget} maps to {effort}, not on catalog \
                             ladder {ladder:?}"
                        ));
                    }
                }
            }
        }
        assert!(checked >= 30, "unexpectedly few OpenAI/xAI rows: {checked}");
        assert!(problems.is_empty(), "{problems:#?}");
    }

    /// `temperature` is sent only to models the catalog says accept it. The
    /// field arrives with the next daily refresh (scripts/fetch-model-catalog.mjs
    /// now carries models.dev `temperature`); rows without it are skipped, and
    /// `temperature_guard_rejects_a_disagreeing_row` proves the check bites.
    #[test]
    fn temperature_follows_the_catalog_when_the_snapshot_carries_it() {
        let mut problems = Vec::new();
        for provider in ["anthropic", "openai", "xai"] {
            for (id, _, _, capabilities) in catalog_rows(provider) {
                if let Some(problem) = temperature_disagreement(provider, &id, &capabilities) {
                    problems.push(problem);
                }
            }
        }
        assert!(problems.is_empty(), "{problems:#?}");
    }

    /// One row's verdict: Some(problem) when Maestro would send `temperature`
    /// to a model whose catalog row says it is not accepted.
    fn temperature_disagreement(
        provider: &str,
        id: &str,
        capabilities: &serde_json::Value,
    ) -> Option<String> {
        let accepts = capabilities.get("temperature")?.as_bool()?;
        let sends = match provider {
            "anthropic" => anthropic_request_capabilities(Some(provider), id).temperature,
            _ => openai_request_capabilities(Some(provider), id).temperature,
        };
        (sends && !accepts)
            .then(|| format!("{provider}/{id}: sends temperature, catalog says unsupported"))
    }

    #[test]
    fn temperature_guard_rejects_a_disagreeing_row() {
        let rejects = serde_json::json!({ "temperature": false });
        let accepts = serde_json::json!({ "temperature": true });
        let unknown = serde_json::json!({});
        // gpt-4o is sent temperature; a row saying it rejects it must fail.
        assert!(temperature_disagreement("openai", "gpt-4o", &rejects).is_some());
        assert!(temperature_disagreement("openai", "gpt-4o", &accepts).is_none());
        assert!(temperature_disagreement("openai", "gpt-4o", &unknown).is_none());
        // gpt-6-luna is not sent temperature, so either verdict passes.
        assert!(temperature_disagreement("openai", "gpt-6-luna", &rejects).is_none());
        assert!(temperature_disagreement("anthropic", "claude-sonnet-4-6", &rejects).is_some());
        assert!(temperature_disagreement("anthropic", "claude-opus-5-5", &rejects).is_none());
    }

    #[test]
    fn pro_tiers_never_send_an_effort_below_their_floor() {
        let pro = openai_request_capabilities(Some("openai"), "gpt-5-pro");
        for budget in [1_024, 4_096, 10_000, 20_000, 32_000, 50_000] {
            assert_eq!(
                pro.reasoning_effort(budget),
                "high",
                "gpt-5-pro budget {budget}"
            );
        }
        let later = openai_request_capabilities(Some("openai"), "gpt-5.5-pro");
        assert_eq!(later.reasoning_effort(1_024), "medium");
        assert_eq!(later.reasoning_effort(4_096), "medium");
        assert_eq!(later.reasoning_effort(10_000), "medium");
        assert_eq!(later.reasoning_effort(20_000), "high");
        assert_eq!(later.reasoning_effort(50_000), "xhigh");
        // Non-pro models keep the full ladder.
        assert_eq!(
            openai_request_capabilities(Some("openai"), "gpt-5.5").reasoning_effort(4_096),
            "low"
        );
    }

    #[test]
    fn chat_protocol_reasoning_models_are_not_sent_temperature_or_denied_effort() {
        for model in ["gpt-6-luna", "gpt-6-sol", "gpt-realtime-2.1"] {
            let caps = openai_request_capabilities(Some("openai"), model);
            assert_eq!(caps.protocol, OpenAiWireProtocol::OpenAiChat, "{model}");
            assert!(!caps.temperature, "{model} must not be sent temperature");
            assert!(caps.reasoning_effort_supported, "{model}");
        }
        for model in ["gpt-4o", "gpt-4.1", "gpt-4o-mini"] {
            let caps = openai_request_capabilities(Some("openai"), model);
            assert!(caps.temperature, "{model}");
            assert!(
                !caps.reasoning_effort_supported,
                "{model} has no effort ladder"
            );
        }
        for (model, supported, maximum) in [
            ("grok-4.7", true, "xhigh"),
            ("grok-4.5", true, "high"),
            ("grok-build-0.1", false, "high"),
            ("grok-4.20-0309-reasoning", false, "high"),
        ] {
            let caps = openai_request_capabilities(Some("xai"), model);
            assert_eq!(caps.reasoning_effort_supported, supported, "{model}");
            assert_eq!(caps.maximum_reasoning_effort, maximum, "{model}");
        }
        // Routes other than direct OpenAI and xAI keep the previous contract.
        assert!(
            openai_request_capabilities(Some("openrouter"), "openai/gpt-4o")
                .reasoning_effort_supported
        );
        assert!(
            openai_request_capabilities(Some("deepseek"), "deepseek-chat")
                .reasoning_effort_supported
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
