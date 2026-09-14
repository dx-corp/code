use crate::{Config, MAX_JSON_BODY_BYTES, model_config_path, read_json_value};
use maestro_local_host::ai::ProviderRegistry;
use maestro_local_host::model_catalog as shared_catalog;
use maestro_local_host::model_catalog::ModelProtocol;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::env;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ModelInfo {
    pub(crate) id: String,
    pub(crate) provider: String,
    pub(crate) name: String,
    pub(crate) api: String,
    pub(crate) context_window: u32,
    pub(crate) max_tokens: u32,
    pub(crate) reasoning: bool,
    pub(crate) cost: ModelCost,
    pub(crate) capabilities: ModelCapabilities,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ModelCost {
    pub(crate) input: f64,
    pub(crate) output: f64,
    pub(crate) cache_read: f64,
    pub(crate) cache_write: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ModelCapabilities {
    pub(crate) streaming: bool,
    pub(crate) tools: bool,
    pub(crate) vision: bool,
    pub(crate) reasoning: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct ModelRegistry {
    pub(crate) models: Vec<ModelInfo>,
    pub(crate) aliases: HashMap<String, String>,
}

pub(crate) async fn available_models(config: &Config) -> ModelRegistry {
    let mut registry = ModelRegistry {
        models: builtin_models(),
        aliases: HashMap::new(),
    };

    if let Some(catalog) = fetch_llm_gateway_model_catalog(config).await {
        merge_llm_gateway_model_catalog(&mut registry, &catalog);
    } else if let Some(catalog) = fetch_openrouter_public_catalog(config).await {
        merge_llm_gateway_model_catalog(&mut registry, &catalog);
    }

    if let Some(config) = read_json_value(&model_config_path()).await {
        merge_configured_models(&mut registry, &config);
    }
    registry.models.sort_by(|left, right| {
        left.provider
            .cmp(&right.provider)
            .then(left.id.cmp(&right.id))
    });
    registry
}

const OPENROUTER_MODELS_API_URL: &str = "https://openrouter.ai/api/v1/models";

async fn fetch_openrouter_public_catalog(config: &Config) -> Option<Value> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(config.llm_gateway_timeout_ms))
        .user_agent(concat!(
            "maestro-runtime-gateway/",
            env!("CARGO_PKG_VERSION")
        ))
        .build()
        .ok()?;
    let response = client
        .get(OPENROUTER_MODELS_API_URL)
        .header("accept", "application/json")
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body = response.bytes().await.ok()?;
    if body.len() > MAX_JSON_BODY_BYTES {
        return None;
    }
    serde_json::from_slice(&body).ok()
}

async fn fetch_llm_gateway_model_catalog(config: &Config) -> Option<Value> {
    let url = config.llm_gateway_models_url.as_deref()?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(config.llm_gateway_timeout_ms))
        .build()
        .ok()?;
    let mut request = client.get(url).header("accept", "application/json");
    if let Some(token) = config.llm_gateway_token.as_deref() {
        request = request.bearer_auth(token);
    }
    if let Some(org_id) = config.llm_gateway_org_id.as_deref() {
        request = request.header("x-organization-id", org_id);
    }

    let response = request.send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body = response.bytes().await.ok()?;
    if body.len() > MAX_JSON_BODY_BYTES {
        return None;
    }
    serde_json::from_slice(&body).ok()
}

pub(crate) fn merge_llm_gateway_model_catalog(registry: &mut ModelRegistry, catalog: &Value) {
    if let Some(models) = catalog.get("models").and_then(Value::as_array) {
        merge_gateway_model_array(registry, None, models);
    }

    if let Some(data) = catalog.get("data").and_then(Value::as_array) {
        if data.iter().any(|entry| entry.get("models").is_some()) {
            merge_gateway_provider_array(registry, data);
        } else {
            merge_openrouter_model_array(registry, data);
        }
    }

    if let Some(providers) = catalog.get("external_providers").and_then(Value::as_array) {
        merge_gateway_provider_array(registry, providers);
    }
}

fn merge_openrouter_model_array(registry: &mut ModelRegistry, models: &[Value]) {
    for model in models {
        let Some(info) = model_info_from_openrouter_value(model) else {
            continue;
        };
        upsert_model(&mut registry.models, info);
    }
}

fn merge_gateway_provider_array(registry: &mut ModelRegistry, providers: &[Value]) {
    for provider in providers {
        let Some(provider_id) = provider.get("id").and_then(Value::as_str).map(str::trim) else {
            continue;
        };
        if provider_id.is_empty() {
            continue;
        }
        let provider_id = canonical_provider(provider_id);
        if let Some(models) = provider.get("models").and_then(Value::as_array) {
            merge_gateway_model_array(registry, Some(provider_id), models);
        }
    }
}

fn merge_gateway_model_array(
    registry: &mut ModelRegistry,
    provider_id: Option<&str>,
    models: &[Value],
) {
    for model in models {
        let Some(info) = model_info_from_gateway_value(provider_id, model) else {
            continue;
        };
        upsert_model(&mut registry.models, info);
    }
}

fn model_info_from_openrouter_value(model: &Value) -> Option<ModelInfo> {
    let id = model.get("id").and_then(Value::as_str).map(str::trim)?;
    if id.is_empty() {
        return None;
    }
    let provider = "openrouter";
    let name = model
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(id);
    let top_provider = model.get("top_provider");
    let reasoning =
        supported_parameter(model, "reasoning") || supported_parameter(model, "include_reasoning");
    let tools = supported_parameter(model, "tools") || supported_parameter(model, "tool_choice");
    let vision = openrouter_input_modalities_include(model, "image");

    Some(ModelInfo {
        id: id.to_string(),
        provider: provider.to_string(),
        name: name.to_string(),
        api: default_api_for_provider_model(provider, id).to_string(),
        context_window: value_u32(
            model
                .get("context_length")
                .or_else(|| top_provider.and_then(|provider| provider.get("context_length"))),
        )
        .unwrap_or(0),
        max_tokens: value_u32(
            top_provider
                .and_then(|provider| provider.get("max_completion_tokens"))
                .or_else(|| model.get("max_completion_tokens"))
                .or_else(|| model.get("maxTokens")),
        )
        .unwrap_or(0),
        reasoning,
        cost: openrouter_model_cost_from_value(model.get("pricing")),
        capabilities: ModelCapabilities {
            streaming: true,
            tools,
            vision,
            reasoning,
        },
    })
}

fn model_info_from_gateway_value(provider_id: Option<&str>, model: &Value) -> Option<ModelInfo> {
    let id = model.get("id").and_then(Value::as_str).map(str::trim)?;
    if id.is_empty() {
        return None;
    }
    let provider = model
        .get("provider")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|provider| !provider.is_empty())
        .or(provider_id)
        .unwrap_or("llm-gateway");
    let provider = canonical_provider(provider);
    let name = model
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(id);
    let capabilities = model.get("capabilities");
    let limit = model.get("limit");
    let reasoning = value_bool(model.get("supports_reasoning"))
        .or_else(|| value_bool(model.get("reasoning")))
        .unwrap_or(false);
    let vision =
        value_bool(capabilities.and_then(|capabilities| capabilities.get("supports_vision")))
            .or_else(|| gateway_modalities_include(model, "image"))
            .unwrap_or(false);
    let tools =
        value_bool(capabilities.and_then(|capabilities| capabilities.get("supports_functions")))
            .or_else(|| value_bool(model.get("tool_call")))
            .or_else(|| value_bool(model.get("toolUse")))
            .or_else(|| value_bool(model.get("tools")))
            .unwrap_or(true);
    let streaming =
        value_bool(capabilities.and_then(|capabilities| capabilities.get("supports_streaming")))
            .unwrap_or(true);

    Some(ModelInfo {
        id: id.to_string(),
        provider: provider.to_string(),
        name: name.to_string(),
        api: default_api_for_provider_model(provider, id).to_string(),
        context_window: value_u32(
            capabilities
                .and_then(|capabilities| capabilities.get("context_length"))
                .or_else(|| model.get("contextWindow"))
                .or_else(|| limit.and_then(|limit| limit.get("context"))),
        )
        .unwrap_or(0),
        max_tokens: value_u32(
            capabilities
                .and_then(|capabilities| capabilities.get("max_tokens"))
                .or_else(|| model.get("maxTokens"))
                .or_else(|| limit.and_then(|limit| limit.get("output"))),
        )
        .unwrap_or(0),
        reasoning,
        cost: model_cost_from_value(model.get("pricing").or_else(|| model.get("cost"))),
        capabilities: ModelCapabilities {
            streaming,
            tools,
            vision,
            reasoning,
        },
    })
}

fn gateway_modalities_include(model: &Value, mode: &str) -> Option<bool> {
    let input = model.get("modalities")?.get("input")?.as_array()?;
    Some(input.iter().any(|entry| entry.as_str() == Some(mode)))
}

fn openrouter_input_modalities_include(model: &Value, mode: &str) -> bool {
    model
        .get("architecture")
        .and_then(|architecture| architecture.get("input_modalities"))
        .and_then(Value::as_array)
        .map(|modalities| modalities.iter().any(|entry| entry.as_str() == Some(mode)))
        .unwrap_or(false)
}

fn supported_parameter(model: &Value, parameter: &str) -> bool {
    model
        .get("supported_parameters")
        .and_then(Value::as_array)
        .map(|parameters| {
            parameters
                .iter()
                .any(|entry| entry.as_str() == Some(parameter))
        })
        .unwrap_or(false)
}

fn default_api_for_provider_model(provider: &str, _model_id: &str) -> &'static str {
    match provider {
        "anthropic" => "anthropic-messages",
        "openai-codex" => "openai-codex-app-server",
        "openai" | "azure-openai" | "azure" => "openai-responses",
        "openrouter" => "openai-completions",
        "google" | "google-ai" | "gemini" | "vertex-ai" | "vertex" => "google",
        "bedrock" | "aws-bedrock" => "bedrock",
        _ => "openai-responses",
    }
}

pub(crate) fn merge_configured_models(registry: &mut ModelRegistry, config: &Value) {
    if let Some(aliases) = config.get("aliases").and_then(Value::as_object) {
        registry
            .aliases
            .extend(aliases.iter().filter_map(|(alias, target)| {
                target
                    .as_str()
                    .map(|target| (alias.to_string(), target.trim().to_string()))
            }));
    }

    let Some(providers) = config.get("providers").and_then(Value::as_array) else {
        return;
    };

    for provider in providers {
        if provider.get("enabled").and_then(Value::as_bool) == Some(false) {
            continue;
        }
        let Some(provider_id) = provider.get("id").and_then(Value::as_str).map(str::trim) else {
            continue;
        };
        if provider_id.is_empty() {
            continue;
        }
        let provider_id = canonical_provider(provider_id);
        let provider_api = provider.get("api").and_then(Value::as_str).map(str::trim);
        let Some(models) = provider.get("models").and_then(Value::as_array) else {
            continue;
        };

        for model in models {
            let Some(id) = model.get("id").and_then(Value::as_str).map(str::trim) else {
                continue;
            };
            if id.is_empty() {
                continue;
            }
            let name = model
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .unwrap_or(id);
            let api = model
                .get("api")
                .and_then(Value::as_str)
                .map(str::trim)
                .or(provider_api)
                .unwrap_or(if provider_id == "vertex-ai" {
                    "google"
                } else {
                    "openai-responses"
                });
            let reasoning = model
                .get("reasoning")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let input_modes = model
                .get("input")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let vision = input_modes
                .iter()
                .any(|mode| mode.as_str() == Some("image"));
            let tools = model
                .get("toolUse")
                .or_else(|| model.get("tools"))
                .and_then(Value::as_bool)
                .unwrap_or(true);

            let info = ModelInfo {
                id: id.to_string(),
                provider: provider_id.to_string(),
                name: name.to_string(),
                api: api.to_string(),
                context_window: value_u32(model.get("contextWindow")).unwrap_or(0),
                max_tokens: value_u32(model.get("maxTokens")).unwrap_or(0),
                reasoning,
                cost: model_cost_from_value(model.get("cost")),
                capabilities: ModelCapabilities {
                    streaming: true,
                    tools,
                    vision,
                    reasoning,
                },
            };
            upsert_model(&mut registry.models, info);
        }
    }
}

fn upsert_model(models: &mut Vec<ModelInfo>, model: ModelInfo) {
    if let Some(existing) = models
        .iter_mut()
        .find(|candidate| candidate.provider == model.provider && candidate.id == model.id)
    {
        *existing = model;
    } else {
        models.push(model);
    }
}

fn value_u32(value: Option<&Value>) -> Option<u32> {
    value?.as_u64().and_then(|value| u32::try_from(value).ok())
}

fn value_bool(value: Option<&Value>) -> Option<bool> {
    value?.as_bool()
}

fn model_cost_from_value(value: Option<&Value>) -> ModelCost {
    let Some(cost) = value.and_then(Value::as_object) else {
        return zero_model_cost();
    };
    ModelCost {
        input: cost.get("input").and_then(Value::as_f64).unwrap_or(0.0),
        output: cost.get("output").and_then(Value::as_f64).unwrap_or(0.0),
        cache_read: cost
            .get("cacheRead")
            .or_else(|| cost.get("cache_read"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        cache_write: cost
            .get("cacheWrite")
            .or_else(|| cost.get("cache_write"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
    }
}

fn openrouter_model_cost_from_value(value: Option<&Value>) -> ModelCost {
    let Some(cost) = value.and_then(Value::as_object) else {
        return zero_model_cost();
    };
    ModelCost {
        input: value_f64(cost.get("prompt")).unwrap_or(0.0),
        output: value_f64(cost.get("completion")).unwrap_or(0.0),
        cache_read: value_f64(cost.get("input_cache_read")).unwrap_or(0.0),
        cache_write: value_f64(cost.get("input_cache_write")).unwrap_or(0.0),
    }
}

fn value_f64(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

fn zero_model_cost() -> ModelCost {
    ModelCost {
        input: 0.0,
        output: 0.0,
        cache_read: 0.0,
        cache_write: 0.0,
    }
}

pub(crate) async fn default_model(config: &Config) -> ModelInfo {
    let registry = available_models(config).await;
    default_model_from_registry(&registry)
}

pub(crate) fn default_model_from_registry(registry: &ModelRegistry) -> ModelInfo {
    env::var("MAESTRO_DEFAULT_MODEL")
        .ok()
        .and_then(|model| resolve_model(&model, registry))
        .or_else(|| {
            let default_id = shared_catalog::default_model_for_provider("openai")?;
            resolve_model(&format!("openai-codex/{default_id}"), registry)
        })
        .or_else(|| registry.models.first().cloned())
        .unwrap_or_else(emergency_default_model)
}

pub(crate) fn emergency_default_model() -> ModelInfo {
    let models = builtin_models();
    let default_id = shared_catalog::default_model_for_provider("openai").unwrap_or("gpt-5.6");
    models
        .iter()
        .find(|model| model.provider == "openai-codex" && model.id == default_id)
        .or_else(|| models.first())
        .cloned()
        .expect("shared model catalog snapshot must not be empty")
}

pub(crate) fn resolve_model(input: &str, registry: &ModelRegistry) -> Option<ModelInfo> {
    let normalized = input.trim();
    if normalized.is_empty() {
        return None;
    }
    let candidate = registry
        .aliases
        .get(normalized)
        .map(String::as_str)
        .unwrap_or(normalized);
    let (provider, id) = candidate
        .split_once('/')
        .map(|(provider, id)| (Some(canonical_provider(provider)), id))
        .unwrap_or((None, candidate));

    let resolved = registry
        .models
        .iter()
        .find(|model| {
            model.id == id
                && provider
                    .map(|provider| provider == model.provider)
                    .unwrap_or(true)
        })
        .cloned();
    if resolved.is_some() {
        return resolved;
    }

    if let Some(model) = registry
        .models
        .iter()
        .find(|model| model.provider == "openrouter" && model.id == candidate)
    {
        return Some(model.clone());
    }

    // The OpenRouter catalog is intentionally live: model ids can be added,
    // aliased, or retired without a Maestro release. Preserve the exact
    // provider/model route even when the metadata endpoint is unavailable so
    // an uncatalogued OpenRouter model still reaches the native client. The
    // zero/false capabilities are honest unknowns; a live catalog overlay will
    // replace them with typed metadata when available.
    let provider = provider?;
    let descriptor = ProviderRegistry::descriptor(provider)?;
    if descriptor.id != "openrouter" || id.trim().is_empty() || id.chars().any(char::is_whitespace)
    {
        return None;
    }

    Some(ModelInfo {
        id: id.to_string(),
        provider: descriptor.id.to_string(),
        name: id.to_string(),
        api: "openai-completions".to_string(),
        context_window: 0,
        max_tokens: 0,
        reasoning: false,
        cost: zero_model_cost(),
        capabilities: ModelCapabilities {
            streaming: true,
            tools: false,
            vision: false,
            reasoning: false,
        },
    })
}

/// Static catalog seed shared with the TUI: the generated models.dev
/// snapshot bundled in `maestro_local_host::model_catalog` (regenerated by
/// `scripts/fetch-model-catalog.mjs`), mapped into the runtime gateway's
/// shape, plus an `openai-codex` app-server mirror of each Responses-API
/// OpenAI model so the Codex bridge keeps its own provider lane. Live data
/// still comes from the LLM-gateway overlay merged on top.
pub(crate) fn builtin_models() -> Vec<ModelInfo> {
    let shared = shared_catalog::bundled_models();
    let mut models: Vec<ModelInfo> = shared.iter().map(model_info_from_shared).collect();
    models.extend(
        shared
            .iter()
            .filter(|model| model.provider == "google" && model.id.starts_with("gemini-"))
            .map(|model| {
                let mut info = model_info_from_shared(model);
                info.provider = "vertex-ai".to_string();
                info
            }),
    );
    models.extend(
        shared
            .iter()
            .filter(|model| {
                model.provider == "openai"
                    && model.capabilities.protocol == ModelProtocol::OpenAiResponses
            })
            .map(|model| {
                let mut info = model_info_from_shared(model);
                info.provider = "openai-codex".to_string();
                info.api = "openai-codex-app-server".to_string();
                info
            }),
    );
    models
}

fn canonical_provider(provider: &str) -> &str {
    match provider {
        "vertex" => "vertex-ai",
        _ => provider,
    }
}

fn model_info_from_shared(model: &shared_catalog::ModelInfo) -> ModelInfo {
    ModelInfo {
        id: model.id.clone(),
        provider: model.provider.clone(),
        name: model.name.clone(),
        api: api_for_shared_protocol(model.capabilities.protocol).to_string(),
        context_window: model.capabilities.context_tokens,
        // The shared snapshot carries no output-token limit or pricing; the
        // gateway overlay fills those in when it has them.
        max_tokens: 0,
        reasoning: model.capabilities.reasoning,
        cost: zero_model_cost(),
        capabilities: ModelCapabilities {
            streaming: model.capabilities.streaming,
            tools: model.capabilities.tools,
            vision: model.capabilities.vision,
            reasoning: model.capabilities.reasoning,
        },
    }
}

fn api_for_shared_protocol(protocol: ModelProtocol) -> &'static str {
    match protocol {
        ModelProtocol::Anthropic => "anthropic-messages",
        ModelProtocol::OpenAiChat => "openai-completions",
        ModelProtocol::OpenAiResponses => "openai-responses",
        ModelProtocol::Google => "google",
        ModelProtocol::CodexAppServer => "openai-codex-app-server",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emergency_default_model_is_openai_codex_app_server() {
        let model = emergency_default_model();

        assert_eq!(model.provider, "openai-codex");
        assert_eq!(model.id, "gpt-5.6");
        assert_eq!(model.api, "openai-codex-app-server");
    }

    #[test]
    fn builtin_models_come_from_the_shared_snapshot() {
        let models = builtin_models();
        let shared = shared_catalog::bundled_models();

        assert_eq!(
            models
                .iter()
                .filter(|model| {
                    model.provider != "openai-codex" && model.provider != "vertex-ai"
                })
                .count(),
            shared.len(),
            "every shared snapshot model must appear under its own provider"
        );

        let shared_gemini_count = shared
            .iter()
            .filter(|model| model.provider == "google" && model.id.starts_with("gemini-"))
            .count();
        assert_eq!(
            models
                .iter()
                .filter(|model| model.provider == "vertex-ai")
                .count(),
            shared_gemini_count,
            "Vertex should expose the shared Gemini metadata under its own provider"
        );

        let codex = models
            .iter()
            .find(|model| model.provider == "openai-codex" && model.id == "gpt-5.6")
            .expect("codex app-server mirror of the default model");
        assert_eq!(codex.api, "openai-codex-app-server");
        assert!(codex.capabilities.reasoning);
        assert!(codex.capabilities.tools);
        assert_eq!(
            codex.context_window,
            shared
                .iter()
                .find(|model| model.provider == "openai" && model.id == "gpt-5.6")
                .expect("shared gpt-5.6")
                .capabilities
                .context_tokens,
        );
    }

    #[test]
    fn shared_default_models_are_present() {
        let models = builtin_models();

        for provider in ["anthropic", "openai", "google", "xai", "openrouter"] {
            let default =
                shared_catalog::default_model_for_provider(provider).expect("default model");
            assert!(
                models
                    .iter()
                    .any(|model| model.provider == provider && model.id == default),
                "default {default} for {provider} must be in the runtime-gateway catalog"
            );
        }
    }

    #[test]
    fn builtin_models_drop_deprecated_ids() {
        let models = builtin_models();

        for dead in [
            "gpt-5.1-codex-max",
            "gemini-2.0-flash",
            "claude-sonnet-4-5-20250514",
        ] {
            assert!(
                !models.iter().any(|model| model.id == dead),
                "deprecated id {dead} must not appear in the runtime-gateway catalog"
            );
        }
    }

    #[test]
    fn catalogued_openrouter_models_keep_live_metadata() {
        let registry = ModelRegistry {
            models: builtin_models(),
            aliases: HashMap::new(),
        };

        let model = resolve_model("openrouter/anthropic/claude-sonnet-4.5", &registry)
            .expect("bundled OpenRouter rows must resolve");
        assert_eq!(model.provider, "openrouter");
        assert_eq!(model.id, "anthropic/claude-sonnet-4.5");
        assert_eq!(model.api, "openai-completions");
        assert!(model.capabilities.tools);
        assert!(model.context_window > 0);

        let unprefixed = resolve_model("anthropic/claude-sonnet-4.5", &registry)
            .expect("OpenRouter vendor ids resolve when the native catalog misses them");
        assert_eq!(unprefixed.provider, "openrouter");
        assert_eq!(unprefixed.id, "anthropic/claude-sonnet-4.5");
    }

    #[test]
    fn uncatalogued_openrouter_models_resolve_as_chat_routes() {
        let registry = ModelRegistry {
            models: Vec::new(),
            aliases: HashMap::new(),
        };

        let model = resolve_model("openrouter/anthropic/claude-sonnet-4.5:free", &registry)
            .expect("OpenRouter model ids must not require a release-time catalog entry");

        assert_eq!(model.provider, "openrouter");
        assert_eq!(model.id, "anthropic/claude-sonnet-4.5:free");
        assert_eq!(model.api, "openai-completions");
        assert!(model.capabilities.streaming);
        assert!(!model.capabilities.tools);
        assert_eq!(model.context_window, 0);
    }

    #[test]
    fn uncatalogued_non_openrouter_models_still_fail_closed() {
        let registry = ModelRegistry {
            models: Vec::new(),
            aliases: HashMap::new(),
        };

        assert!(resolve_model("openai/future-custom-model", &registry).is_none());
        assert!(resolve_model("openrouter/", &registry).is_none());
        assert!(resolve_model("openrouter/model with spaces", &registry).is_none());
    }

    #[test]
    fn resolves_catalogued_vertex_gemini_routes() {
        let registry = ModelRegistry {
            models: builtin_models(),
            aliases: HashMap::new(),
        };

        for route in ["vertex-ai/gemini-2.5-pro", "vertex/gemini-2.5-pro"] {
            let model = resolve_model(route, &registry).expect("Vertex Gemini route");
            assert_eq!(model.provider, "vertex-ai");
            assert_eq!(model.id, "gemini-2.5-pro");
            assert_eq!(model.api, "google");
            assert!(model.capabilities.tools);
            assert!(model.capabilities.vision);
        }

        assert!(resolve_model("vertex-ai/future-gemini-model", &registry).is_none());
    }

    #[test]
    fn canonicalizes_vertex_aliases_when_ingesting_custom_models() {
        let config = serde_json::json!({
            "aliases": { "vertex-default": "vertex/gemini-custom" },
            "providers": [{
                "id": "vertex",
                "models": [{
                    "id": "gemini-custom",
                    "name": "Vertex Gemini Custom",
                    "input": ["text", "image"],
                    "contextWindow": 131072
                }]
            }]
        });
        let mut registry = ModelRegistry {
            models: Vec::new(),
            aliases: HashMap::new(),
        };

        merge_configured_models(&mut registry, &config);

        let model = resolve_model("vertex-default", &registry)
            .expect("configured Vertex aliases should resolve");
        assert_eq!(model.provider, "vertex-ai");
        assert_eq!(model.id, "gemini-custom");
        assert_eq!(model.api, "google");
        assert!(model.capabilities.vision);
        assert_eq!(model.context_window, 131072);
    }

    #[test]
    fn canonicalizes_vertex_provider_from_gateway_catalog() {
        let catalog = serde_json::json!({
            "external_providers": [{
                "id": "vertex",
                "models": [{
                    "id": "gemini-gateway",
                    "name": "Vertex Gemini Gateway"
                }]
            }]
        });
        let mut registry = ModelRegistry {
            models: Vec::new(),
            aliases: HashMap::new(),
        };

        merge_llm_gateway_model_catalog(&mut registry, &catalog);

        let model = resolve_model("vertex/gemini-gateway", &registry)
            .expect("gateway Vertex aliases should resolve");
        assert_eq!(model.provider, "vertex-ai");
        assert_eq!(model.id, "gemini-gateway");
        assert_eq!(model.api, "google");
    }

    #[test]
    fn configured_vertex_model_is_used_as_the_default() {
        let registry = ModelRegistry {
            models: builtin_models(),
            aliases: HashMap::new(),
        };
        let previous = env::var_os("MAESTRO_DEFAULT_MODEL");
        env::set_var("MAESTRO_DEFAULT_MODEL", "vertex-ai/gemini-2.5-pro");

        let model = default_model_from_registry(&registry);

        match previous {
            Some(value) => env::set_var("MAESTRO_DEFAULT_MODEL", value),
            None => env::remove_var("MAESTRO_DEFAULT_MODEL"),
        }
        assert_eq!(model.provider, "vertex-ai");
        assert_eq!(model.id, "gemini-2.5-pro");
    }
}
