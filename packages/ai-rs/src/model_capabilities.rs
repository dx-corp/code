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

/// OpenAI prompt-caching guide. Re-check before adding a wire field.
pub const OPENAI_PROMPT_CACHING_SOURCE: &str =
    "https://developers.openai.com/api/docs/guides/prompt-caching (checked 2026-09-24)";
/// Anthropic prompt-caching guide. Re-check before adding a wire field.
pub const ANTHROPIC_PROMPT_CACHING_SOURCE: &str =
    "https://platform.claude.com/docs/en/build-with-claude/prompt-caching (checked 2026-09-24)";

/// How a route caches. Unknown does not inherit another route's lifetime or rates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheBehavior {
    Unknown,
    Unsupported,
    Implicit,
    Explicit,
}

/// Where a caller is allowed to place a boundary. Empty means "do not send a marker".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheBoundaryKind {
    /// Anthropic `cache_control` on tools, system, or a non-thinking message block.
    AnthropicCacheControl,
    /// OpenAI Responses `prompt_cache_breakpoint` on a content block. Not valid on top-level `instructions`.
    OpenAiContentBreakpoint,
    /// The provider chooses breakpoints. Callers must not send a marker parameter.
    ProviderChosen,
}

/// How far a later request can see a breakpoint that was actually written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheLookback {
    Unknown,
    /// Anthropic checks at most this many blocks per breakpoint, counting the breakpoint.
    Blocks(u32),
    /// OpenAI explicit lookup: the first N and the latest M explicit breakpoints.
    ExplicitBreakpoints {
        first: u32,
        latest: u32,
    },
}

/// Retention the source actually states. A missing hint is not five minutes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheRetention {
    Unknown,
    Unsupported,
    /// Anthropic default ephemeral lifetime. The clock starts when the caching request starts.
    AnthropicEphemeral5m,
    /// Anthropic `ttl: "1h"`. Twice the base input price. Not selected by the boundary planner.
    AnthropicEphemeral1h,
    /// GPT-5.6+ `prompt_cache_options.ttl` of `"30m"`, also the default. Minimum lifetime after write or reuse.
    OpenAiTtl30m,
    /// Earlier OpenAI models: `in_memory` is typically 5–10 minutes of inactivity, up to an hour.
    OpenAiInMemory,
    /// Earlier OpenAI `prompt_cache_retention: "24h"`. Up to 24 hours. Not a single cutoff.
    OpenAiExtended24h,
    /// Earlier OpenAI default depends on the organization's zero-data-retention policy, which this process does not know.
    OpenAiOrganizationDependent,
}

impl CacheRetention {
    /// A single sourced duration, in seconds, suitable only as a diagnostic hint.
    /// `None` means the source does not give one number. Callers must not substitute 300.
    #[must_use]
    pub fn hint_seconds(self) -> Option<u64> {
        match self {
            Self::AnthropicEphemeral5m => Some(300),
            Self::AnthropicEphemeral1h => Some(3_600),
            Self::OpenAiTtl30m => Some(1_800),
            Self::Unknown
            | Self::Unsupported
            | Self::OpenAiInMemory
            | Self::OpenAiExtended24h
            | Self::OpenAiOrganizationDependent => None,
        }
    }
}

/// How reported usage relates to cached and uncached input. Missing stays missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheUsageInterpretation {
    Unknown,
    /// `input_tokens` excludes cache read and cache creation. Total input is the sum of the three.
    AnthropicSeparateCacheFields,
    /// GPT-5.6+: `input_tokens` includes `cached_tokens` and `cache_write_tokens`. No 128-token rounding.
    OpenAiInclusiveExact,
    /// Earlier OpenAI: `cached_tokens` omits hidden system tokens and rounds down to a multiple of 128.
    OpenAiCachedTokensRounded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheWireProtocol {
    Unknown,
    None,
    AnthropicMessages,
    BedrockConverse,
    OpenAiResponses,
    OpenAiChat,
}

/// Model-and-provider scoped cache contract. Rates are sourced milli-units of the
/// uncached input price (100 = 0.1×). `None` is not a default of 100 or of 1.25×.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheCapability {
    pub behavior: CacheBehavior,
    pub boundary_kinds: &'static [CacheBoundaryKind],
    pub minimum_cacheable_tokens: Option<u32>,
    pub max_explicit_markers: Option<u8>,
    pub lookback: CacheLookback,
    pub read_rate_millis: Option<u32>,
    pub write_rate_millis: Option<u32>,
    pub retention: CacheRetention,
    pub wire: CacheWireProtocol,
    pub usage: CacheUsageInterpretation,
    /// Attribution for this record. Not a provider semver.
    pub record_id: &'static str,
    pub source: &'static str,
}

const NO_BOUNDARIES: &[CacheBoundaryKind] = &[];
const ANTHROPIC_BOUNDARIES: &[CacheBoundaryKind] = &[CacheBoundaryKind::AnthropicCacheControl];
const OPENAI_EXPLICIT_BOUNDARIES: &[CacheBoundaryKind] =
    &[CacheBoundaryKind::OpenAiContentBreakpoint];
const PROVIDER_CHOSEN_BOUNDARIES: &[CacheBoundaryKind] = &[CacheBoundaryKind::ProviderChosen];

/// Family, minimum cacheable tokens, cache-read milli-rate. Longer families come first
/// so `claude-opus-4` does not swallow `claude-opus-4-5`.
const ANTHROPIC_CACHE_TABLE: &[(&str, u32, u32)] = &[
    ("claude-fable-5-1", 512, 25),
    ("claude-mythos-5-1", 512, 25),
    ("claude-opus-5-5", 512, 50),
    ("claude-opus-5", 512, 100),
    ("claude-fable-5", 512, 100),
    ("claude-mythos-preview", 2_048, 100),
    ("claude-mythos-5", 512, 100),
    ("claude-opus-4-8", 1_024, 100),
    ("claude-opus-4-7", 2_048, 100),
    ("claude-opus-4-6", 4_096, 100),
    ("claude-opus-4-5", 4_096, 100),
    ("claude-sonnet-5", 1_024, 100),
    ("claude-sonnet-4-6", 1_024, 100),
    ("claude-sonnet-4-5", 1_024, 100),
    ("claude-haiku-4-5", 4_096, 100),
    ("claude-haiku-3-5", 2_048, 100),
];

fn unknown_capability(record_id: &'static str, source: &'static str) -> CacheCapability {
    CacheCapability {
        behavior: CacheBehavior::Unknown,
        boundary_kinds: NO_BOUNDARIES,
        minimum_cacheable_tokens: None,
        max_explicit_markers: None,
        lookback: CacheLookback::Unknown,
        read_rate_millis: None,
        write_rate_millis: None,
        retention: CacheRetention::Unknown,
        wire: CacheWireProtocol::Unknown,
        usage: CacheUsageInterpretation::Unknown,
        record_id,
        source,
    }
}

fn normalized_route(provider: Option<&str>, model: &str) -> (Option<String>, String) {
    let stripped = strip_managed_model_prefix(model.trim());
    let embedded = stripped
        .split_once('/')
        .map(|(name, _)| name.trim().to_ascii_lowercase());
    let provider = provider
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| name.to_ascii_lowercase())
        .or(embedded);
    (provider, provider_model_name(stripped).to_ascii_lowercase())
}

fn openai_explicit_breakpoint_family(model: &str) -> bool {
    // GPT-5.6 and later, per the OpenAI guide's model table. GPT-5.5 and earlier are implicit only.
    let name = model.rsplit('/').next().unwrap_or(model);
    let Some(rest) = name.strip_prefix("gpt-") else {
        return false;
    };
    let major_end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    if major_end == 0 {
        return false;
    }
    let Ok(major) = rest[..major_end].parse::<u32>() else {
        return false;
    };
    if major > 5 {
        return true;
    }
    if major < 5 {
        return false;
    }
    let after = &rest[major_end..];
    let Some(minor_src) = after.strip_prefix('.') else {
        return false;
    };
    let minor_end = minor_src
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(minor_src.len());
    minor_src[..minor_end]
        .parse::<u32>()
        .ok()
        .is_some_and(|minor| minor >= 6)
}

fn anthropic_cache_row(model: &str) -> Option<(u32, u32)> {
    let normalized = anthropic_model_id(Some("anthropic"), model).or_else(|| {
        let lower = provider_model_name(model).to_ascii_lowercase();
        lower.starts_with("claude-").then_some(lower)
    })?;
    ANTHROPIC_CACHE_TABLE
        .iter()
        .find(|(family, _, _)| is_model_family(&normalized, family))
        .map(|(_, minimum, read)| (*minimum, *read))
}

fn anthropic_explicit_capability(
    model: &str,
    wire: CacheWireProtocol,
    record_id: &'static str,
    known_retention: bool,
) -> CacheCapability {
    let row = anthropic_cache_row(model);
    CacheCapability {
        behavior: CacheBehavior::Explicit,
        boundary_kinds: ANTHROPIC_BOUNDARIES,
        minimum_cacheable_tokens: row.map(|(minimum, _)| minimum),
        max_explicit_markers: Some(4),
        lookback: CacheLookback::Blocks(20),
        // Recognized families use the published multiplier. An unrecognized Claude id
        // stays explicit (the route already accepts markers) but does not inherit 0.1×.
        read_rate_millis: row.map(|(_, read)| read),
        write_rate_millis: row.map(|_| 1_250),
        retention: if known_retention {
            CacheRetention::AnthropicEphemeral5m
        } else {
            CacheRetention::Unknown
        },
        wire,
        usage: if wire == CacheWireProtocol::AnthropicMessages {
            CacheUsageInterpretation::AnthropicSeparateCacheFields
        } else {
            // Bedrock's usage field names are a different document and were not re-fetched here.
            CacheUsageInterpretation::Unknown
        },
        record_id,
        source: ANTHROPIC_PROMPT_CACHING_SOURCE,
    }
}

/// Sourced cache contract for this provider route and model. Aggregator routes stay
/// unknown even when the model id looks like a direct OpenAI or Anthropic model.
#[must_use]
pub fn cache_capability(provider: Option<&str>, model: &str) -> CacheCapability {
    // A managed or aggregator id does not identify the physical deployment.
    // Do not inherit the direct OpenAI or Anthropic contract from the suffix.
    if has_managed_model_prefix(model) {
        return unknown_capability(
            "managed-gateway-route.2026-09-24",
            "managed gateway routes do not identify the physical cache",
        );
    }
    let (provider_name, model_id) = normalized_route(provider, model);
    let provider = provider_name.as_deref();
    let is = |name: &str| provider.is_some_and(|provider| provider == name);

    if is("openrouter") || is("azure") || is("azure-openai") || is("vertex-ai") || is("google") {
        return unknown_capability(
            "aggregator-or-translated-route.2026-09-24",
            "physical cache placement is not the logical model id",
        );
    }
    if is("anthropic")
        || is("claude")
        || (provider.is_none() && anthropic_model_id(None, model).is_some())
    {
        return anthropic_explicit_capability(
            model,
            CacheWireProtocol::AnthropicMessages,
            "anthropic-messages-explicit.2026-09-24",
            true,
        );
    }
    if is("bedrock") {
        return if supports_explicit_prompt_caching(AiProvider::Bedrock, model) {
            anthropic_explicit_capability(
                &model_id,
                CacheWireProtocol::BedrockConverse,
                "bedrock-claude-explicit-markers.2026-09-24",
                false,
            )
        } else {
            unknown_capability(
                "bedrock-unlisted.2026-09-24",
                ANTHROPIC_PROMPT_CACHING_SOURCE,
            )
        };
    }
    if is("openai") {
        return openai_direct_capability(provider, model);
    }
    if is("llamacpp") {
        return CacheCapability {
            behavior: CacheBehavior::Implicit,
            boundary_kinds: PROVIDER_CHOSEN_BOUNDARIES,
            minimum_cacheable_tokens: None,
            max_explicit_markers: Some(0),
            lookback: CacheLookback::Unknown,
            read_rate_millis: None,
            write_rate_millis: None,
            retention: CacheRetention::Unknown,
            wire: CacheWireProtocol::OpenAiChat,
            usage: CacheUsageInterpretation::Unknown,
            record_id: "llamacpp-cache-prompt.2026-09-24",
            source: "existing llama.cpp cache_prompt request flag; no sourced lifetime",
        };
    }
    unknown_capability(
        "unknown-route.2026-09-24",
        "no sourced cache contract for this provider",
    )
}

fn openai_direct_capability(provider: Option<&str>, model: &str) -> CacheCapability {
    let responses = uses_responses_api(provider, model);
    if responses && openai_explicit_breakpoint_family(model) {
        return CacheCapability {
            behavior: CacheBehavior::Explicit,
            boundary_kinds: OPENAI_EXPLICIT_BOUNDARIES,
            minimum_cacheable_tokens: Some(1_024),
            // The guide documents a lookup window (first 2 and latest 50), not a
            // hard marker cap. The planner applies its own budget separately.
            max_explicit_markers: None,
            lookback: CacheLookback::ExplicitBreakpoints {
                first: 2,
                latest: 50,
            },
            read_rate_millis: Some(100),
            write_rate_millis: Some(1_250),
            retention: CacheRetention::OpenAiTtl30m,
            wire: CacheWireProtocol::OpenAiResponses,
            usage: CacheUsageInterpretation::OpenAiInclusiveExact,
            record_id: "openai-responses-gpt-5.6-explicit.2026-09-24",
            source: OPENAI_PROMPT_CACHING_SOURCE,
        };
    }
    CacheCapability {
        behavior: CacheBehavior::Implicit,
        boundary_kinds: PROVIDER_CHOSEN_BOUNDARIES,
        minimum_cacheable_tokens: None,
        max_explicit_markers: Some(0),
        lookback: CacheLookback::Unknown,
        // The guide says earlier models have a model-dependent cached-input rate and no
        // extra write charge. Neither fact is a universal 0.1× read rate.
        read_rate_millis: None,
        write_rate_millis: Some(1_000),
        retention: CacheRetention::OpenAiOrganizationDependent,
        wire: if responses {
            CacheWireProtocol::OpenAiResponses
        } else {
            CacheWireProtocol::OpenAiChat
        },
        usage: CacheUsageInterpretation::OpenAiCachedTokensRounded,
        record_id: "openai-implicit.2026-09-24",
        source: OPENAI_PROMPT_CACHING_SOURCE,
    }
}

#[must_use]
pub fn allows_openai_explicit_breakpoint(provider: Option<&str>, model: &str) -> bool {
    let capability = cache_capability(provider, model);
    capability.behavior == CacheBehavior::Explicit
        && capability.wire == CacheWireProtocol::OpenAiResponses
        && capability
            .boundary_kinds
            .contains(&CacheBoundaryKind::OpenAiContentBreakpoint)
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
    fn cache_capability_records_are_sourced_and_do_not_lend_defaults() {
        let unknown = cache_capability(Some("openrouter"), "openai/gpt-5.6");
        assert_eq!(unknown.behavior, CacheBehavior::Unknown);
        assert!(unknown.boundary_kinds.is_empty());
        assert_eq!(unknown.read_rate_millis, None);
        assert_eq!(unknown.write_rate_millis, None);
        assert_eq!(unknown.retention.hint_seconds(), None);
        assert!(!allows_openai_explicit_breakpoint(
            Some("openrouter"),
            "openai/gpt-5.6"
        ));

        let bare = cache_capability(None, "gpt-5.6");
        assert_ne!(bare.behavior, CacheBehavior::Explicit);
        assert_eq!(bare.read_rate_millis, None);
        assert_eq!(bare.retention.hint_seconds(), None);

        let explicit = cache_capability(Some("openai"), "gpt-5.6-sol");
        assert_eq!(explicit.behavior, CacheBehavior::Explicit);
        assert_eq!(explicit.minimum_cacheable_tokens, Some(1_024));
        assert_eq!(explicit.max_explicit_markers, None);
        assert_eq!(
            explicit.lookback,
            CacheLookback::ExplicitBreakpoints {
                first: 2,
                latest: 50
            }
        );
        assert_eq!(explicit.read_rate_millis, Some(100));
        assert_eq!(explicit.write_rate_millis, Some(1_250));
        assert_eq!(explicit.retention, CacheRetention::OpenAiTtl30m);
        assert_eq!(
            explicit.usage,
            CacheUsageInterpretation::OpenAiInclusiveExact
        );
        assert!(explicit.source.contains("developers.openai.com"));
        assert!(allows_openai_explicit_breakpoint(
            Some("openai"),
            "gpt-6-astra"
        ));
        assert!(
            !allows_openai_explicit_breakpoint(Some("openai"), "maestro-managed/openai/gpt-5.6"),
            "a managed-gateway model id is not a confirmed physical OpenAI route"
        );
        assert_eq!(
            cache_capability(Some("openai"), "claude-opus-4").minimum_cacheable_tokens,
            None
        );
        assert_eq!(
            cache_capability(Some("anthropic"), "claude-sonnet-4").minimum_cacheable_tokens,
            None,
            "families absent from the sourced minimum table do not inherit a neighbor's number"
        );
        assert_eq!(
            cache_capability(Some("anthropic"), "claude-opus-4-1").read_rate_millis,
            None
        );

        let earlier = cache_capability(Some("openai"), "gpt-5.4");
        assert_eq!(earlier.behavior, CacheBehavior::Implicit);
        assert_eq!(earlier.max_explicit_markers, Some(0));
        assert_eq!(earlier.read_rate_millis, None);
        assert_eq!(earlier.write_rate_millis, Some(1_000));
        assert_eq!(earlier.retention.hint_seconds(), None);
        assert!(!allows_openai_explicit_breakpoint(Some("openai"), "gpt-5"));
        assert!(!allows_openai_explicit_breakpoint(
            Some("openai"),
            "gpt-4.1"
        ));

        let opus = cache_capability(Some("anthropic"), "claude-opus-5-5");
        assert_eq!(opus.minimum_cacheable_tokens, Some(512));
        assert_eq!(opus.read_rate_millis, Some(50));
        assert_eq!(opus.lookback, CacheLookback::Blocks(20));
        assert_eq!(opus.max_explicit_markers, Some(4));
        assert_eq!(opus.retention.hint_seconds(), Some(300));
        assert_eq!(
            cache_capability(Some("anthropic"), "claude-fable-5-1").read_rate_millis,
            Some(25)
        );
        assert_eq!(
            cache_capability(Some("anthropic"), "claude-haiku-4-5").minimum_cacheable_tokens,
            Some(4_096)
        );
        let unrecognized = cache_capability(Some("anthropic"), "claude-not-a-family");
        assert_eq!(unrecognized.behavior, CacheBehavior::Explicit);
        assert_eq!(unrecognized.read_rate_millis, None);
        assert_eq!(unrecognized.minimum_cacheable_tokens, None);

        let bedrock = cache_capability(
            Some("bedrock"),
            "bedrock/anthropic.claude-sonnet-4-5-20250929-v1:0",
        );
        assert_eq!(bedrock.behavior, CacheBehavior::Explicit);
        assert_eq!(bedrock.usage, CacheUsageInterpretation::Unknown);
        assert_eq!(bedrock.retention.hint_seconds(), None);
        assert_eq!(
            cache_capability(Some("bedrock"), "amazon.nova-pro-v1:0").behavior,
            CacheBehavior::Unknown
        );
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
