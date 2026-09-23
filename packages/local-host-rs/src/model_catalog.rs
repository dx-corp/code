//! Shared model capability metadata for CLI and TUI surfaces.
//!
//! The catalog follows the "always-fresh snapshot" approach:
//!
//! 1. A bundled snapshot (`model_catalog_data.json`, embedded via
//!    `include_str!`) is regenerated from <https://models.dev/api.json> and
//!    OpenRouter's public <https://openrouter.ai/api/v1/models> catalog by
//!    `scripts/fetch-model-catalog.mjs` and committed, so offline builds and
//!    fresh installs always have current data with zero build-time network.
//! 2. At runtime a non-blocking background task refreshes a cached copy at
//!    `~/.composer/models-catalog.json` at most every [`REFRESH_TTL_SECS`].
//!    The cache overlays the bundled snapshot only when it is at least as
//!    fresh as the bundled `generated_at`; stale or malformed caches lose to
//!    the bundled snapshot, so fresh installs always win.
//! 3. Unknown model ids are not in the catalog at all and keep passing
//!    through to the provider registry unchanged. OpenRouter routes remain
//!    valid without a catalog row.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::model_facts_generated::model_facts;
use std::sync::LazyLock;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::ai::{ProviderProtocol, ProviderRegistry, provider_model_name};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelProtocol {
    Anthropic,
    #[serde(rename = "openai-chat")]
    OpenAiChat,
    #[serde(rename = "openai-responses")]
    OpenAiResponses,
    Google,
    CodexAppServer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationState {
    Catalog,
    Verified,
    Unavailable,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelVerification {
    pub state: VerificationState,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl ModelVerification {
    #[must_use]
    pub fn catalog() -> Self {
        Self {
            state: VerificationState::Catalog,
            source: "builtin-catalog".to_owned(),
            detail: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCapabilities {
    pub protocol: ModelProtocol,
    pub tools: bool,
    pub vision: bool,
    pub reasoning: bool,
    pub streaming: bool,
    pub context_tokens: u32,
    /// Per-response output ceiling (reasoning tokens included) from the
    /// upstream catalog's `limit.output`. `None` when the source has no
    /// output-limit data for the model. Deserializes from older catalog
    /// snapshots that predate the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub description: String,
    pub capabilities: ModelCapabilities,
    pub verification: ModelVerification,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedModelInspection {
    pub provider: String,
    pub protocol: String,
    pub base_url: Option<String>,
    pub auth_configured: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_capabilities: Option<crate::ai::OpenAiRequestCapabilities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<ModelCapabilities>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInspection {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catalog: Option<ModelInfo>,
    pub resolved: ResolvedModelInspection,
    pub sources: BTreeMap<String, String>,
}

/// Upstream community catalog (MIT-licensed) that feeds native-provider rows
/// in both the bundled snapshot and the runtime refresh.
const MODELS_DEV_API_URL: &str = "https://models.dev/api.json";
/// Official OpenRouter catalog. This is the source of truth for current
/// OpenRouter model ids; models.dev is a lagging subset.
const OPENROUTER_MODELS_API_URL: &str = "https://openrouter.ai/api/v1/models";
/// How long a cached refresh check stays valid before another background
/// refresh is spawned. OpenRouter publishes new routes daily; keep this
/// shorter than the models.dev-only era so a long-lived TUI picks them up.
const REFRESH_TTL_SECS: u64 = 60 * 60;
/// Hard timeout for the background refresh request; must never stall startup.
const REFRESH_TIMEOUT_SECS: u64 = 15;
/// Providers mirrored from models.dev into the catalog, in the order they
/// are merged. OpenRouter is fetched from its own official catalog.
pub(crate) const CATALOG_PROVIDERS: &[&str] = &["anthropic", "google", "openai", "xai"];
/// Providers whose defaults are shown in the model selector. Vertex AI uses
/// the Google catalog rows mirrored by [`available_models`], but is kept out
/// of [`CATALOG_PROVIDERS`] because models.dev has no separate Vertex entry.
pub const MODEL_SELECTOR_PROVIDERS: &[&str] = &[
    "anthropic",
    "google",
    "vertex-ai",
    "openai",
    "xai",
    "openrouter",
    "llamacpp",
];

const BUNDLED_CATALOG_JSON: &str = include_str!("model_catalog_data.json");
const VENDOR_CORRECTIONS_JSON: &str = include_str!("../../../config/vendor-corrections.json");

#[derive(Deserialize)]
struct VendorCorrections {
    #[serde(rename = "schemaVersion")]
    schema_version: String,
    corrections: Vec<VendorCorrection>,
}

#[derive(Deserialize)]
struct VendorCorrection {
    id: String,
    field: String,
    value: u32,
    source: String,
}

/// Committed snapshot regenerated by `scripts/fetch-model-catalog.mjs`.
#[derive(Debug, Clone, Deserialize)]
struct BundledCatalog {
    generated_at: u64,
    models: Vec<ModelInfo>,
}

/// Runtime overlay persisted at `~/.composer/models-catalog.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CachedCatalog {
    /// Last refresh attempt (success or failure), unix epoch seconds.
    #[serde(default)]
    checked_at: u64,
    /// Last successful fetch, unix epoch seconds. Compared against the bundled
    /// snapshot's `generated_at` for the staleness guard.
    #[serde(default)]
    fetched_at: u64,
    /// Raw HTTP `Last-Modified` header from the fetch, kept for debugging.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_modified: Option<String>,
    #[serde(default)]
    models: Vec<ModelInfo>,
}

static BUNDLED_CATALOG: LazyLock<BundledCatalog> = LazyLock::new(|| {
    serde_json::from_str(BUNDLED_CATALOG_JSON).expect("bundled model catalog JSON must parse")
});

static VENDOR_CORRECTIONS: LazyLock<VendorCorrections> = LazyLock::new(|| {
    let parsed: VendorCorrections = serde_json::from_str(VENDOR_CORRECTIONS_JSON)
        .expect("checked-in vendor corrections JSON must parse");
    assert_eq!(parsed.schema_version, "maestro.vendor-corrections.v1");
    parsed
});

/// The same checked-in corrections used by the snapshot generator also apply
/// to persisted caches and live refreshes. An upstream refresh must not undo
/// a vendor-verified context or output limit.
fn apply_vendor_corrections(models: &mut [ModelInfo]) -> anyhow::Result<()> {
    for correction in &VENDOR_CORRECTIONS.corrections {
        for model in models.iter_mut().filter(|model| model.id == correction.id) {
            let changed = match correction.field.as_str() {
                "capabilities.context_tokens"
                    if model.capabilities.context_tokens != correction.value =>
                {
                    model.capabilities.context_tokens = correction.value;
                    true
                }
                "capabilities.context_tokens" => false,
                "capabilities.output_tokens"
                    if model.capabilities.output_tokens != Some(correction.value) =>
                {
                    model.capabilities.output_tokens = Some(correction.value);
                    true
                }
                "capabilities.output_tokens" => false,
                other => anyhow::bail!("unsupported vendor correction field: {other}"),
            };
            if changed {
                model.verification = ModelVerification {
                    state: VerificationState::Verified,
                    source: correction.source.clone(),
                    detail: None,
                };
            }
        }
    }
    Ok(())
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Hand-maintained default model per catalog provider. Ids must exist in the
/// bundled snapshot; regeneration keeps dead ids out of the catalog itself.
#[must_use]
pub fn default_model_for_provider(provider: &str) -> Option<&'static str> {
    match provider.trim().to_ascii_lowercase().as_str() {
        "anthropic" | "claude" => Some("claude-sonnet-4-6"),
        "openai" => Some("gpt-5.6"),
        "openai-codex" | "codex" => Some("gpt-5.6"),
        "google" | "gemini" | "vertex-ai" | "vertex" => Some("gemini-2.5-pro"),
        "xai" | "grok" => Some("grok-4.5"),
        "openrouter" => Some("openai/gpt-4o-mini"),
        "llamacpp" | "llama.cpp" | "llama-cpp" => Some("Qwen3.8-27B"),
        _ => None,
    }
}

/// Model catalog used by `maestro models` and the model selector: the runtime
/// cache overlaid on the bundled snapshot when the cache wins the staleness
/// guard, else the bundled snapshot. Never fails and never blocks on network.
#[must_use]
pub fn available_models() -> Vec<ModelInfo> {
    maybe_spawn_background_refresh();
    let cache = catalog_cache_path().and_then(|path| load_cache(&path));
    let mut models = select_models(&BUNDLED_CATALOG, cache.as_ref()).to_vec();
    apply_vendor_corrections(&mut models).expect("checked-in vendor corrections must be supported");
    ensure_astra_model(&mut models);
    for model in &mut models {
        if matches!(
            model.capabilities.protocol,
            ModelProtocol::OpenAiChat | ModelProtocol::OpenAiResponses
        ) {
            let request = crate::ai::openai_request_capabilities(Some(&model.provider), &model.id);
            model.capabilities.protocol = match request.protocol {
                crate::ai::OpenAiWireProtocol::OpenAiChat => ModelProtocol::OpenAiChat,
                crate::ai::OpenAiWireProtocol::OpenAiResponses => ModelProtocol::OpenAiResponses,
            };
            if let Some(context) = request.context_tokens {
                model.capabilities.context_tokens = context;
            }
            if let Some(output) = request.output_tokens {
                model.capabilities.output_tokens = Some(output);
            }
        }
    }
    append_managed_models(&mut models);
    append_builtin_local_models(&mut models);
    mirror_vertex_models(&mut models);
    models
}

/// Return the provider-preserving route for model consumers that start a run.
#[must_use]
pub fn model_route(model: &ModelInfo) -> String {
    match model.provider.as_str() {
        "evalops" | "google" | "vertex-ai" | "llamacpp" | "lmstudio" | "ollama" | "openrouter" => {
            format!("{}/{}", model.provider, model.id)
        }
        _ => model.id.clone(),
    }
}

pub struct ManagedModel {
    pub id: &'static str,
    pub name: &'static str,
    pub provider: &'static str,
    pub credential_name: &'static str,
    pub context_tokens: u32,
}

/// Shipped managed route metadata. Gateway admission and pricing remain server-owned.
pub const MANAGED_MODELS: &[ManagedModel] = &[
    ManagedModel {
        id: "accounts/fireworks/models/glm-5p3",
        name: "GLM-5.3",
        provider: "fireworks",
        credential_name: crate::credential_mode::DEFAULT_MANAGED_CREDENTIAL_NAME,
        context_tokens: 1_048_576,
    },
    ManagedModel {
        id: "accounts/fireworks/models/glm-5p3-flash",
        name: "GLM-5.3 Flash",
        provider: "fireworks",
        credential_name: crate::credential_mode::DEFAULT_MANAGED_CREDENTIAL_NAME,
        context_tokens: 1_048_576,
    },
    ManagedModel {
        id: "accounts/fireworks/models/deepseek-v4-flash-0731",
        name: "DeepSeek V4 Flash 0731",
        provider: "fireworks",
        credential_name: crate::credential_mode::DEFAULT_MANAGED_CREDENTIAL_NAME,
        context_tokens: 1_048_576,
    },
    ManagedModel {
        id: "accounts/fireworks/models/kimi-k3",
        name: "Kimi K3",
        provider: "fireworks",
        credential_name: crate::credential_mode::DEFAULT_MANAGED_CREDENTIAL_NAME,
        context_tokens: 1_048_576,
    },
    ManagedModel {
        id: "accounts/fireworks/models/deepseek-v4p1-flash",
        name: "DeepSeek V4.1 Flash",
        provider: "fireworks",
        credential_name: "deixic-llm-gateway-glm53",
        context_tokens: 1_048_576,
    },
    ManagedModel {
        id: "accounts/fireworks/models/qwen3p8-max",
        name: "Qwen 3.8 Max",
        provider: "fireworks",
        credential_name: "deixic-llm-gateway-glm53",
        context_tokens: 131_072,
    },
    ManagedModel {
        id: "gemini-3.8-flash",
        name: "Gemini 3.8 Flash",
        provider: "google",
        credential_name: "deixic-llm-gateway-gemini38",
        context_tokens: 1_048_576,
    },
];

// Managed routes are product-owned typed contracts, independent of community cache refreshes.
fn append_managed_models(models: &mut Vec<ModelInfo>) {
    for price in MANAGED_MODELS {
        models.retain(|model| !(model.provider == "evalops" && model.id == price.id));
        models.push(ModelInfo {
            id: price.id.to_owned(),
            name: price.name.to_owned(),
            provider: "evalops".to_owned(),
            description: format!("{} · Model credits · Text and tools", price.provider),
            capabilities: ModelCapabilities {
                protocol: ModelProtocol::OpenAiChat,
                tools: true,
                // The managed prepaid contract currently admits text and tools only.
                vision: false,
                // Reasoning controls are not part of the prepaid request contract.
                reasoning: false,
                streaming: true,
                context_tokens: price.context_tokens,
                // Keep the existing 16k default; the gateway enforces its 32k ceiling.
                output_tokens: None,
            },
            verification: ModelVerification {
                state: VerificationState::Catalog,
                source: "managed-model-contract".to_owned(),
                detail: Some(
                    "Availability requires a configured managed route and model credits."
                        .to_owned(),
                ),
            },
        });
    }
}

// models.dev can lag native OpenAI launches. Keep the documented model
// available across bundled installs and successful community-catalog refreshes.
fn ensure_astra_model(models: &mut Vec<ModelInfo>) {
    if let Some(model) = models
        .iter_mut()
        .find(|model| model.provider == "openai" && model.id == "gpt-6-astra")
    {
        // A cache written before Astra support used the generic chat protocol.
        model.capabilities.protocol = ModelProtocol::OpenAiResponses;
        return;
    }
    // The bundled snapshot is the fallback authority, including provenance and
    // display metadata. A second hard-coded row drifts when the catalog refreshes.
    if let Some(model) = bundled_models()
        .iter()
        .find(|model| model.provider == "openai" && model.id == "gpt-6-astra")
    {
        models.push(model.clone());
    }
}

fn append_builtin_local_models(models: &mut Vec<ModelInfo>) {
    if models
        .iter()
        .any(|model| model.provider == "llamacpp" && model.id == "Qwen3.8-27B")
    {
        return;
    }
    models.push(ModelInfo {
        id: "Qwen3.8-27B".to_owned(),
        name: "Qwen3.8-27B (llama.cpp)".to_owned(),
        provider: "llamacpp".to_owned(),
        description: "Local Qwen3.8 model served by llama.cpp".to_owned(),
        capabilities: ModelCapabilities {
            protocol: ModelProtocol::OpenAiChat,
            tools: true,
            vision: true,
            reasoning: true,
            streaming: true,
            context_tokens: 262_144,
            output_tokens: Some(32_768),
        },
        verification: ModelVerification {
            state: VerificationState::Catalog,
            source: "builtin-local-catalog".to_owned(),
            detail: None,
        },
    });
}

/// Vertex AI serves the same Gemini model family as Google's public API. Keep
/// a distinct provider row so qualified Vertex routes are cataloged without
/// requiring models.dev to expose a second copy of every Gemini model.
fn mirror_vertex_models(models: &mut Vec<ModelInfo>) {
    let vertex_model_ids: HashSet<&str> = models
        .iter()
        .filter(|model| model.provider == "vertex-ai")
        .map(|model| model.id.as_str())
        .collect();
    let mirrored = models
        .iter()
        .filter(|model| model.provider == "google" && model.id.starts_with("gemini-"))
        .filter(|model| !vertex_model_ids.contains(model.id.as_str()))
        .cloned()
        .map(|mut model| {
            model.provider = "vertex-ai".to_owned();
            model
        })
        .collect::<Vec<_>>();
    models.extend(mirrored);
}

/// Inspect the exact native model/provider resolution while retaining per-field
/// provenance and never returning credential material.
pub fn inspect_model(id: &str) -> anyhow::Result<ModelInspection> {
    inspect_model_with_env(id, &std::env::vars().collect())
}

fn inspect_model_with_env(
    id: &str,
    env: &HashMap<String, String>,
) -> anyhow::Result<ModelInspection> {
    let catalog = find_model(id);
    let resolved = ProviderRegistry::resolve(id, env)?;
    let mut sources = BTreeMap::new();
    let cache_wins = catalog_cache_path()
        .and_then(|path| load_cache(&path))
        .is_some_and(|cache| {
            !cache.models.is_empty() && cache.fetched_at >= BUNDLED_CATALOG.generated_at
        });
    sources.insert(
        "catalog".to_string(),
        if catalog.is_none() {
            "uncataloged"
        } else if cache_wins {
            "runtime-cache"
        } else {
            "bundled"
        }
        .to_string(),
    );
    sources.insert(
        "provider".to_string(),
        if id.contains('/') {
            "model-id-prefix"
        } else {
            "model-family-inference"
        }
        .to_string(),
    );
    sources.insert(
        "auth".to_string(),
        resolved
            .auth_source
            .as_ref()
            .map_or_else(|| "none".to_string(), |name| format!("environment:{name}")),
    );
    let base_url_source = resolved
        .provider
        .base_url_env
        .iter()
        .find(|name| {
            env.get(**name)
                .is_some_and(|value| !value.trim().is_empty())
        })
        .map_or_else(
            || {
                if resolved.provider.default_base_url.is_some() {
                    "builtin".to_string()
                } else {
                    "none".to_string()
                }
            },
            |name| format!("environment:{name}"),
        );
    sources.insert("baseUrl".to_string(), base_url_source);
    sources.insert(
        "capabilities".to_string(),
        if catalog.is_some() {
            "catalog"
        } else {
            "unavailable"
        }
        .to_string(),
    );

    Ok(ModelInspection {
        id: id.to_string(),
        resolved: ResolvedModelInspection {
            provider: resolved.provider.id.to_string(),
            protocol: protocol_name(resolved.provider.protocol).to_string(),
            base_url: resolved.base_url.as_deref().map(redact_endpoint),
            auth_configured: resolved.credential.is_some(),
            request_capabilities: matches!(
                resolved.provider.protocol,
                ProviderProtocol::OpenAi | ProviderProtocol::OpenAiCompatible
            )
            .then(|| crate::ai::openai_request_capabilities(Some(resolved.provider.id), id)),
            capabilities: catalog.as_ref().map(|catalog| catalog.capabilities.clone()),
        },
        catalog,
        sources,
    })
}

fn redact_endpoint(value: &str) -> String {
    let Ok(mut url) = url::Url::parse(value) else {
        return "[configured endpoint]".to_string();
    };
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_path("/");
    url.set_query(None);
    url.set_fragment(None);
    url.to_string()
}

/// Bundled snapshot only; used where the runtime overlay must not leak in.
#[must_use]
pub fn bundled_models() -> &'static [ModelInfo] {
    &BUNDLED_CATALOG.models
}

/// Per-million-token USD rates for one model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelRates {
    pub input_per_million: f64,
    pub output_per_million: f64,
    pub cache_read_per_million: f64,
    pub cache_write_per_million: f64,
}

/// Published rates for `model_id`, from the bundled snapshot.
///
/// Read from the generated binding (`model_facts_generated.rs`, emitted from
/// the same snapshot by `tools/model-catalog/generate-bindings.mjs`) rather
/// than added to [`ModelInfo`], which derives `Eq`: an f64 field would remove
/// that, and every consumer of `ModelInfo` equality would have to change for a
/// field only cost reporting wants.
///
/// A model with no vendor rate at all (self-hosted open weights, a router
/// meta-route) has no rates. A cache rate the catalog omits reads as 0.0.
///
/// Id matching is the binding's rule, shared with the TypeScript and Python
/// bindings: the qualified route first, then the bare model name, then a
/// Claude dotted release suffix respelled with a dash. See
/// [`model_facts_candidates`](crate::model_facts_generated::model_facts_candidates).
#[must_use]
pub fn bundled_rates(model_id: &str) -> Option<ModelRates> {
    let facts = model_facts(model_id)?;
    Some(ModelRates {
        input_per_million: facts.input_per_million?,
        output_per_million: facts.output_per_million?,
        cache_read_per_million: facts.cache_read_per_million.unwrap_or(0.0),
        cache_write_per_million: facts.cache_write_per_million.unwrap_or(0.0),
    })
}

/// Context and output limits for `model_id`, from the bundled snapshot.
///
/// Deliberately reads the generated binding rather than [`find_model`]:
/// callers are on hot paths. `find_model` goes through `available_models`,
/// which checks for a background refresh, reads the on-disk cache, and clones
/// every model in the catalog on each call.
///
/// Id matching is the binding's rule; see [`bundled_rates`].
#[must_use]
pub fn bundled_limits(model_id: &str) -> Option<(u32, Option<u32>)> {
    model_facts(model_id).map(|facts| (facts.context_window, facts.max_output_tokens))
}

/// Cache wins only when it carries models and is at least as fresh as the
/// bundled snapshot; anything else falls back to the bundled snapshot.
fn select_models<'a>(
    bundled: &'a BundledCatalog,
    cache: Option<&'a CachedCatalog>,
) -> &'a [ModelInfo] {
    match cache {
        Some(cache) if !cache.models.is_empty() && cache.fetched_at >= bundled.generated_at => {
            &cache.models
        }
        _ => &bundled.models,
    }
}

fn catalog_cache_path() -> Option<PathBuf> {
    crate::path_utils::legacy_composer_home_dir().map(|dir| dir.join("models-catalog.json"))
}

fn load_cache(path: &Path) -> Option<CachedCatalog> {
    let contents = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&contents).ok()
}

/// Spawn a background refresh when the cache is missing or its last check is
/// older than the TTL. No-ops without a tokio runtime. A long-lived process
/// may refresh again after the TTL; startup paths never block or fail on it.
fn maybe_spawn_background_refresh() {
    if crate::safety::vendor_network_disabled() {
        return;
    }
    static LAST_REFRESH_STARTED: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);

    let now = now_epoch_secs();
    let last = LAST_REFRESH_STARTED.load(Ordering::SeqCst);
    if last != 0 && now.saturating_sub(last) <= REFRESH_TTL_SECS {
        return;
    }
    let stale = catalog_cache_path().is_none_or(|path| {
        load_cache(&path)
            .is_none_or(|cache| now.saturating_sub(cache.checked_at) > REFRESH_TTL_SECS)
    });
    if !stale {
        return;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    if LAST_REFRESH_STARTED
        .compare_exchange(last, now, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    handle.spawn(async {
        let _ = refresh_catalog_cache().await;
    });
}

/// Fetch models.dev, apply the same mapping as the fetcher script, and
/// atomically replace the cache. On failure only `checked_at` is bumped so
/// the old data stays in use without hammering the upstream on every start.
async fn refresh_catalog_cache() -> anyhow::Result<()> {
    crate::safety::require_vendor_network()?;
    let Some(path) = catalog_cache_path() else {
        return Ok(());
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(REFRESH_TIMEOUT_SECS))
        .user_agent(concat!("maestro-tui/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let result = fetch_remote_catalog(&client).await;
    let checked_at = now_epoch_secs();
    match result {
        Ok((models, last_modified)) => {
            let cache = CachedCatalog {
                checked_at,
                fetched_at: checked_at,
                last_modified,
                models,
            };
            write_cache_atomic(&path, &cache).await?;
        }
        Err(error) => {
            let mut cache = load_cache(&path).unwrap_or_default();
            cache.checked_at = checked_at;
            let _ = write_cache_atomic(&path, &cache).await;
            return Err(error);
        }
    }
    Ok(())
}

async fn fetch_remote_catalog(
    client: &reqwest::Client,
) -> anyhow::Result<(Vec<ModelInfo>, Option<String>)> {
    let native = fetch_catalog_json(client, MODELS_DEV_API_URL).await;
    let openrouter = fetch_catalog_json(client, OPENROUTER_MODELS_API_URL).await;
    match (native, openrouter) {
        (Err(native_error), Err(openrouter_error)) => {
            anyhow::bail!(
                "catalog refresh failed: models.dev: {native_error}; openrouter: {openrouter_error}"
            )
        }
        (native, openrouter) => {
            let limits = native
                .as_ref()
                .ok()
                .map(|(payload, _)| DevTokenLimits::from_models_dev(payload));
            let (mut models, last_modified) = match native {
                Ok((payload, last_modified)) => match map_models_dev_catalog(&payload) {
                    Ok(models) if !models.is_empty() => (models, last_modified),
                    _ => (bundled_native_models(), None),
                },
                Err(_) => (bundled_native_models(), None),
            };
            let openrouter_models = match openrouter {
                Ok((payload, _)) => {
                    match map_openrouter_catalog_with_limits(&payload, limits.as_ref()) {
                        Ok(models) if !models.is_empty() => models,
                        _ => bundled_openrouter_models(),
                    }
                }
                Err(_) => bundled_openrouter_models(),
            };
            models.extend(openrouter_models);
            apply_vendor_corrections(&mut models)?;
            sort_catalog_models(&mut models);
            if models.is_empty() {
                anyhow::bail!("catalog refresh produced an empty catalog");
            }
            Ok((models, last_modified))
        }
    }
}

async fn fetch_catalog_json(
    client: &reqwest::Client,
    url: &str,
) -> anyhow::Result<(serde_json::Value, Option<String>)> {
    let response = client.get(url).send().await?;
    let response = response.error_for_status()?;
    let last_modified = response
        .headers()
        .get(reqwest::header::LAST_MODIFIED)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let payload: serde_json::Value = response.json().await?;
    Ok((payload, last_modified))
}

fn bundled_native_models() -> Vec<ModelInfo> {
    bundled_models()
        .iter()
        .filter(|model| model.provider != "openrouter")
        .cloned()
        .collect()
}

fn bundled_openrouter_models() -> Vec<ModelInfo> {
    bundled_models()
        .iter()
        .filter(|model| model.provider == "openrouter")
        .cloned()
        .collect()
}

fn sort_catalog_models(models: &mut [ModelInfo]) {
    models.sort_by(|left, right| {
        left.provider
            .cmp(&right.provider)
            .then_with(|| left.id.cmp(&right.id))
    });
}

/// Write the cache via a sibling temp file + rename so readers never observe
/// a partially written catalog.
async fn write_cache_atomic(path: &Path, cache: &CachedCatalog) -> anyhow::Result<()> {
    let path = path.to_path_buf();
    let contents = serde_json::to_vec_pretty(cache)?;
    tokio::task::spawn_blocking(move || crate::path_utils::atomic_private_write(&path, &contents))
        .await??;
    Ok(())
}

/// Positive output ceiling that is distinct from an aggregator's context copy
/// or exact 90% fallback. Keep in sync with the snapshot generator.
fn distinct_output_tokens(context: Option<u32>, output: Option<u32>) -> Option<u32> {
    let output = output.filter(|tokens| *tokens > 0)?;
    match context.filter(|tokens| *tokens > 0) {
        Some(context) if u64::from(output) * 100 >= u64::from(context) * 99 => None,
        Some(context) if u64::from(output) == (u64::from(context) * 9 + 5) / 10 => None,
        _ => Some(output),
    }
}

fn parse_limit_tokens(model: &serde_json::Value, field: &str) -> Option<u32> {
    model
        .get("limit")
        .and_then(|limit| limit.get(field))
        .and_then(serde_json::Value::as_u64)
        .and_then(|tokens| u32::try_from(tokens).ok())
        .filter(|tokens| *tokens > 0)
}

/// models.dev `limit.context` / `limit.output` keyed for OpenRouter vendor ids.
///
/// Vendor-native rows are inserted first so a distinct Google/OpenAI/Moonshot
/// output wins over an OpenRouter row that copied the context window.
type DevTokenWindow = (Option<u32>, Option<u32>);

#[derive(Debug, Default)]
struct DevTokenLimits {
    entries: HashMap<String, Vec<DevTokenWindow>>,
}

impl DevTokenLimits {
    fn from_models_dev(payload: &serde_json::Value) -> Self {
        let mut limits = Self::default();
        let Some(providers) = payload.as_object() else {
            return limits;
        };
        for (provider, body) in providers {
            if provider == "openrouter" {
                continue;
            }
            limits.index_provider(provider, body);
        }
        if let Some(body) = providers.get("openrouter") {
            limits.index_provider("openrouter", body);
        }
        limits
    }

    fn index_provider(&mut self, provider: &str, body: &serde_json::Value) {
        let Some(models) = body.get("models").and_then(serde_json::Value::as_object) else {
            return;
        };
        for (id, model) in models {
            let context = parse_limit_tokens(model, "context");
            let output = parse_limit_tokens(model, "output");
            if provider == "openrouter" {
                self.push(id, context, output);
            } else {
                self.push(&format!("{provider}/{id}"), context, output);
            }
        }
    }

    fn push(&mut self, key: &str, context: Option<u32>, output: Option<u32>) {
        self.entries
            .entry(key.to_owned())
            .or_default()
            .push((context, output));
    }

    fn distinct_output(
        &self,
        openrouter_id: &str,
        openrouter_context: u32,
        openrouter_output: Option<u32>,
    ) -> Option<u32> {
        if let Some(output) = distinct_output_tokens(Some(openrouter_context), openrouter_output) {
            return Some(output);
        }
        for (_context, output) in self.candidates(openrouter_id) {
            if let Some(resolved) = distinct_output_tokens(Some(openrouter_context), output) {
                return Some(resolved);
            }
        }
        None
    }

    fn candidates(&self, openrouter_id: &str) -> Vec<DevTokenWindow> {
        self.entries
            .get(openrouter_id)
            .into_iter()
            .flatten()
            .copied()
            .collect()
    }
}

/// Map a models.dev `api.json` payload into catalog entries, applying the
/// same rules as `scripts/fetch-model-catalog.mjs`: keep tool-capable,
/// non-deprecated models with a positive context limit for the providers
/// Maestro routes natively.
fn map_models_dev_catalog(payload: &serde_json::Value) -> anyhow::Result<Vec<ModelInfo>> {
    let mut models = Vec::new();
    for provider in CATALOG_PROVIDERS {
        let provider_models = payload
            .get(*provider)
            .and_then(|provider| provider.get("models"))
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| anyhow::anyhow!("models.dev payload is missing provider {provider}"))?;
        for (id, model) in provider_models {
            if model.get("tool_call").and_then(serde_json::Value::as_bool) != Some(true) {
                continue;
            }
            if model.get("status").and_then(serde_json::Value::as_str) == Some("deprecated") {
                continue;
            }
            let context_tokens = model
                .get("limit")
                .and_then(|limit| limit.get("context"))
                .and_then(serde_json::Value::as_u64)
                .and_then(|context| u32::try_from(context).ok())
                .unwrap_or(0);
            if context_tokens == 0 {
                continue;
            }
            models.push(map_models_dev_model(provider, id, model, context_tokens));
        }
    }
    models.sort_by(|left, right| {
        left.provider
            .cmp(&right.provider)
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(models)
}

fn map_models_dev_model(
    provider: &str,
    id: &str,
    model: &serde_json::Value,
    context_tokens: u32,
) -> ModelInfo {
    let name = model
        .get("name")
        .and_then(serde_json::Value::as_str)
        .filter(|name| !name.is_empty())
        .unwrap_or(id)
        .to_owned();
    let description = model
        .get("description")
        .and_then(serde_json::Value::as_str)
        .filter(|description| !description.is_empty())
        .unwrap_or(&name)
        .to_owned();
    let vision = model
        .get("modalities")
        .and_then(|modalities| modalities.get("input"))
        .and_then(serde_json::Value::as_array)
        .is_some_and(|inputs| inputs.iter().any(|input| input.as_str() == Some("image")));
    // Keep `limit.output` only when it is a real output cap, not a copy of
    // the context window.
    let output_tokens =
        distinct_output_tokens(Some(context_tokens), parse_limit_tokens(model, "output"));
    ModelInfo {
        id: id.to_owned(),
        name,
        provider: provider.to_owned(),
        description,
        capabilities: ModelCapabilities {
            protocol: catalog_protocol(provider, id),
            tools: true,
            vision,
            reasoning: model.get("reasoning").and_then(serde_json::Value::as_bool) == Some(true),
            streaming: true,
            context_tokens,
            output_tokens,
        },
        verification: ModelVerification {
            state: VerificationState::Catalog,
            source: "models.dev".to_owned(),
            detail: None,
        },
    }
}

fn catalog_protocol(provider: &str, model_id: &str) -> ModelProtocol {
    match provider {
        "anthropic" => ModelProtocol::Anthropic,
        "google" => ModelProtocol::Google,
        _ => match crate::ai::openai_request_capabilities(Some(provider), model_id).protocol {
            crate::ai::OpenAiWireProtocol::OpenAiChat => ModelProtocol::OpenAiChat,
            crate::ai::OpenAiWireProtocol::OpenAiResponses => ModelProtocol::OpenAiResponses,
        },
    }
}

/// Map OpenRouter's `GET /api/v1/models` payload. Interactive routes are kept;
/// `:batch` variants belong to OpenRouter's async Batch API and cannot drive
/// Maestro's streaming agent loop.
#[cfg(test)]
fn map_openrouter_catalog(payload: &serde_json::Value) -> anyhow::Result<Vec<ModelInfo>> {
    map_openrouter_catalog_with_limits(payload, None)
}

fn map_openrouter_catalog_with_limits(
    payload: &serde_json::Value,
    limits: Option<&DevTokenLimits>,
) -> anyhow::Result<Vec<ModelInfo>> {
    let models = payload
        .get("data")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("OpenRouter payload is missing a data array"))?;
    let mut mapped = Vec::new();
    for model in models {
        if let Some(info) = map_openrouter_model(model, limits) {
            mapped.push(info);
        }
    }
    Ok(mapped)
}

fn map_openrouter_model(
    model: &serde_json::Value,
    limits: Option<&DevTokenLimits>,
) -> Option<ModelInfo> {
    let id = model
        .get("id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())?;
    if id.ends_with(":batch") {
        return None;
    }
    let top_provider = model.get("top_provider");
    let context_tokens = model
        .get("context_length")
        .or_else(|| top_provider.and_then(|provider| provider.get("context_length")))
        .and_then(serde_json::Value::as_u64)
        .and_then(|context| u32::try_from(context).ok())
        .filter(|context| *context > 0)?;
    let advertised_output = top_provider
        .and_then(|provider| provider.get("max_completion_tokens"))
        .or_else(|| model.get("max_completion_tokens"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|output| u32::try_from(output).ok())
        .filter(|output| *output > 0);
    let output_tokens = limits.map_or_else(
        || distinct_output_tokens(Some(context_tokens), advertised_output),
        |limits| limits.distinct_output(id, context_tokens, advertised_output),
    );
    let name = model
        .get("name")
        .and_then(serde_json::Value::as_str)
        .filter(|name| !name.is_empty())
        .unwrap_or(id)
        .to_owned();
    let description = model
        .get("description")
        .and_then(serde_json::Value::as_str)
        .filter(|description| !description.is_empty())
        .unwrap_or(&name)
        .to_owned();
    let vision = model
        .get("architecture")
        .and_then(|architecture| architecture.get("input_modalities"))
        .and_then(serde_json::Value::as_array)
        .is_some_and(|inputs| inputs.iter().any(|input| input.as_str() == Some("image")));
    let reasoning = openrouter_supported_parameter(model, "reasoning")
        || openrouter_supported_parameter(model, "include_reasoning")
        || model
            .get("reasoning")
            .is_some_and(serde_json::Value::is_object);
    Some(ModelInfo {
        id: id.to_owned(),
        name,
        provider: "openrouter".to_owned(),
        description,
        capabilities: ModelCapabilities {
            protocol: ModelProtocol::OpenAiChat,
            tools: openrouter_supported_parameter(model, "tools")
                || openrouter_supported_parameter(model, "tool_choice"),
            vision,
            reasoning,
            streaming: true,
            context_tokens,
            output_tokens,
        },
        verification: ModelVerification {
            state: VerificationState::Catalog,
            source: "openrouter".to_owned(),
            detail: None,
        },
    })
}

fn openrouter_supported_parameter(model: &serde_json::Value, parameter: &str) -> bool {
    model
        .get("supported_parameters")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|parameters| {
            parameters
                .iter()
                .any(|entry| entry.as_str() == Some(parameter))
        })
}

fn find_openrouter_model<'a>(models: &'a [ModelInfo], id: &str) -> Option<&'a ModelInfo> {
    models
        .iter()
        .find(|model| model.provider == "openrouter" && model.id == id)
}

#[must_use]
pub fn find_model(id: &str) -> Option<ModelInfo> {
    if let Some(model) = crate::local_models::find_discovered_model(id) {
        return Some(model);
    }
    let (provider, bare_id) = id
        .split_once('/')
        .map_or((None, id), |(provider, model)| (Some(provider), model));
    let models = available_models();
    if let Some(provider) = provider {
        if let Some(descriptor) = ProviderRegistry::descriptor(provider) {
            // Exact provider match first.
            if let Some(model) = models
                .iter()
                .find(|model| model.id == bare_id && model.provider == descriptor.id)
            {
                return Some(model.clone());
            }
            // openai-codex is a ChatGPT-subscription transport for OpenAI frontier
            // models; catalog rows are stored under provider "openai".
            if descriptor.id == "openai-codex" {
                return models
                    .into_iter()
                    .find(|model| model.id == bare_id && model.provider == "openai");
            }
            // `anthropic/claude-sonnet-4.5` is an OpenRouter model id, not a
            // native Anthropic catalog row. Prefer the native hit above.
            if descriptor.id != "openrouter" {
                if let Some(model) = find_openrouter_model(&models, id) {
                    return Some(model.clone());
                }
            }
            return None;
        }
        // Unknown first segment (`qwen/...`) is still a valid OpenRouter id.
        return find_openrouter_model(&models, id).cloned();
    }
    models.into_iter().find(|model| model.id == bare_id)
}

/// Fallback per-request output token budget used when the catalog has no
/// distinct output limit for the active model. Explicit configuration
/// (`set_max_tokens`, env overrides such as `MAESTRO_PRINT_MAX_TOKENS`)
/// always wins over this default.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 16_384;

/// Tokens reserved for prompt + tools so a default `max_tokens` cannot consume
/// the entire context window.
pub const MIN_PROMPT_TOKEN_HEADROOM: u32 = 8_192;

/// Catalog-declared output token limit (models.dev `limit.output`) for a
/// model id, when known.
///
/// Provider-qualified runtime models are resolved first so discovered limits
/// retain their provider provenance. Managed gateway ids (`evalops/…`,
/// `maestro-managed/…`) then fall back to the bare catalog id via
/// [`provider_model_name`], so `evalops/gpt-5.6` resolves to the `gpt-5.6` row.
/// The returned value is the model's full output ceiling: it is a cap, not a
/// reservation, and unused headroom costs nothing under per-token billing, so
/// it is passed through untruncated. `None` when the model is not cataloged
/// or the catalog has no output limit for it.
#[must_use]
pub fn catalog_output_token_limit(model_id: &str) -> Option<u32> {
    if let Some(output) = user_configured_token_limits(model_id)
        .and_then(|limits| limits.output_tokens)
        .filter(|output| *output > 0)
    {
        return Some(output);
    }
    find_model(model_id)
        .or_else(|| find_model(&provider_model_name(model_id)))
        .and_then(|model| model.capabilities.output_tokens)
}

/// Default per-request output token budget for a model id.
///
/// Uses the catalog output ceiling only when it is strictly smaller than the
/// context window. OpenRouter and other catalogs often publish
/// `max_completion_tokens` equal to the full context; requesting that as
/// `max_tokens` leaves no room for the prompt and the provider returns 400.
/// A discovered local model with only a live context limit uses at most half
/// that context, capped at [`DEFAULT_MAX_OUTPUT_TOKENS`]. Models without
/// either limit use [`DEFAULT_MAX_OUTPUT_TOKENS`].
#[must_use]
pub fn default_max_output_tokens(model_id: &str) -> u32 {
    let user = user_configured_token_limits(model_id);
    let catalog = find_model(model_id).or_else(|| find_model(&provider_model_name(model_id)));
    let discovered = crate::local_models::find_discovered_model(model_id);
    let context = user.and_then(|limits| limits.context_tokens).or_else(|| {
        catalog
            .as_ref()
            .or(discovered.as_ref())
            .map(|model| model.capabilities.context_tokens)
            .filter(|tokens| *tokens > 0)
    });
    let user_output = user.and_then(|limits| limits.output_tokens);
    let distinct_output = user_output
        .filter(|output| context.is_none_or(|ctx| *output < ctx))
        .or_else(|| {
            catalog
                .as_ref()
                .and_then(|model| model.capabilities.output_tokens)
                .filter(|output| context.is_none_or(|ctx| *output < ctx))
        });
    let budget = if let Some(output) = distinct_output {
        output
    } else if let Some(tokens) = discovered
        .map(|model| model.capabilities.context_tokens)
        .filter(|tokens| *tokens > 0)
    {
        (tokens / 2).clamp(1, DEFAULT_MAX_OUTPUT_TOKENS)
    } else {
        DEFAULT_MAX_OUTPUT_TOKENS
    };
    clamp_output_budget_to_context(budget, context)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UserModelLimits {
    context_tokens: Option<u32>,
    output_tokens: Option<u32>,
}

fn user_configured_token_limits(model_id: &str) -> Option<UserModelLimits> {
    let mut found = None;
    for path in user_provider_config_paths() {
        if let Some(limits) = limits_from_provider_config(&path, model_id) {
            found = Some(limits);
        }
    }
    found
}

fn user_provider_config_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(home) = crate::path_utils::maestro_home_dir() {
        paths.push(home.join("config.json"));
        paths.push(home.join("local.json"));
    }
    if let Ok(cwd) = std::env::current_dir() {
        paths.push(cwd.join(".maestro").join("config.json"));
        paths.push(cwd.join(".maestro").join("config.local.json"));
    }
    paths
}

fn limits_from_provider_config(path: &Path, model_id: &str) -> Option<UserModelLimits> {
    let raw = fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let providers = value.get("providers")?.as_array()?;
    let mut found = None;
    for provider in providers {
        let provider_id = provider
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let Some(models) = provider.get("models").and_then(serde_json::Value::as_array) else {
            continue;
        };
        for model in models {
            let Some(id) = model.get("id").and_then(serde_json::Value::as_str) else {
                continue;
            };
            if !user_model_id_matches(model_id, provider_id, id) {
                continue;
            }
            found = Some(UserModelLimits {
                context_tokens: json_positive_u32(model, &["contextWindow", "context_window"]),
                output_tokens: json_positive_u32(model, &["maxTokens", "max_tokens"]),
            });
        }
    }
    found.filter(|limits| limits.context_tokens.is_some() || limits.output_tokens.is_some())
}

fn user_model_id_matches(requested: &str, provider_id: &str, configured_id: &str) -> bool {
    let requested = requested.trim();
    let configured_id = configured_id.trim();
    if requested.eq_ignore_ascii_case(configured_id) {
        return true;
    }
    if !provider_id.is_empty() {
        let qualified = format!("{provider_id}/{configured_id}");
        if requested.eq_ignore_ascii_case(&qualified) {
            return true;
        }
    }
    requested
        .rsplit_once('/')
        .is_some_and(|(_, tail)| tail.eq_ignore_ascii_case(configured_id))
        || requested
            .to_ascii_lowercase()
            .ends_with(&format!("/{}", configured_id.to_ascii_lowercase()))
}

fn json_positive_u32(value: &serde_json::Value, keys: &[&str]) -> Option<u32> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(serde_json::Value::as_u64)
            .and_then(|tokens| u32::try_from(tokens).ok())
            .filter(|tokens| *tokens > 0)
    })
}

fn clamp_output_budget_to_context(budget: u32, context: Option<u32>) -> u32 {
    let Some(context) = context.filter(|tokens| *tokens > 0) else {
        return budget.max(1);
    };
    // Never reserve more than half the window. An 8k local model with
    // MIN_PROMPT_TOKEN_HEADROOM == 8192 would otherwise keep 1 output token.
    let reserved = MIN_PROMPT_TOKEN_HEADROOM
        .min(context / 2)
        .min(context.saturating_sub(1));
    budget.min(context.saturating_sub(reserved)).max(1)
}

#[must_use]
pub fn has_provider_mismatch(id: &str) -> bool {
    let Some((provider, bare_id)) = id.split_once('/') else {
        return false;
    };
    let Some(descriptor) = ProviderRegistry::descriptor(provider) else {
        // Unknown provider prefix is not a catalog mismatch; resolution
        // fails elsewhere if the prefix is truly invalid.
        return false;
    };
    // Local OpenAI-compatible runtimes intentionally serve arbitrary model
    // ids, including ids that also exist under a hosted catalog provider.
    // Their explicit provider-qualified route is never a catalog mismatch.
    if crate::local_models::LOCAL_RUNTIME_IDS.contains(&descriptor.id) {
        return false;
    }
    let models = available_models();
    // Prefer an exact provider+id hit (openai-codex/gpt-5.6 vs openai/gpt-5.6).
    if models
        .iter()
        .any(|model| model.id == bare_id && model.provider == descriptor.id)
    {
        return false;
    }
    // openai-codex reuses OpenAI catalog ids (subscription transport).
    if descriptor.id == "openai-codex"
        && models
            .iter()
            .any(|model| model.id == bare_id && model.provider == "openai")
    {
        return false;
    }
    // No catalog entry for this provider+id pair: only flag mismatch when
    // the bare id is known under a *different* provider.
    models
        .iter()
        .find(|model| model.id == bare_id)
        .is_some_and(|model| model.provider != descriptor.id)
}

/// Verify registry routing and credential presence without network access.
#[must_use]
pub fn verify_model_offline(model_id: &str) -> ModelVerification {
    let env = std::env::vars().collect();
    match ProviderRegistry::resolve(model_id, &env) {
        Ok(provider) if provider.credential.is_some() || !provider.provider.requires_auth() => {
            let authless = !provider.provider.requires_auth();
            ModelVerification {
                state: VerificationState::Verified,
                source: if authless {
                    "provider-registry".to_owned()
                } else {
                    "environment".to_owned()
                },
                detail: provider.auth_source,
            }
        }
        Ok(provider) => ModelVerification {
            state: VerificationState::Unavailable,
            source: "environment".to_owned(),
            detail: Some(format!(
                "{} credentials not found ({})",
                provider.provider.id,
                provider.provider.auth_env.join(", ")
            )),
        },
        Err(error) => ModelVerification {
            state: VerificationState::Unknown,
            source: "provider-registry".to_owned(),
            detail: Some(error.to_string()),
        },
    }
}

#[must_use]
pub fn protocol_name(protocol: ProviderProtocol) -> &'static str {
    match protocol {
        ProviderProtocol::Anthropic => "anthropic",
        ProviderProtocol::OpenAi => "openai",
        ProviderProtocol::OpenAiCompatible => "openai-compatible",
        ProviderProtocol::Google => "google",
        ProviderProtocol::VertexAi => "vertex-ai",
        ProviderProtocol::Codex => "codex-app-server",
        ProviderProtocol::AzureOpenAi => "azure-openai",
        ProviderProtocol::Bedrock => "bedrock",
        ProviderProtocol::Managed => "managed",
    }
}

#[cfg(test)]
mod tests {
    /// Every centrally declared default model must exist in the catalog.
    ///
    /// Each of these ids used to live in the crate that consumed it, with
    /// nothing checking it. Over one audit the Google subagent tiers named
    /// three retired Gemini 2.0 previews, the ambient Anthropic frontier named
    /// a retired Opus 4.1 snapshot, and the Anthropic provider preset still
    /// defaulted to Opus 4.6.
    #[test]
    fn default_models_are_catalogued() {
        use maestro_runtime_contracts::DefaultModel;

        let ids: std::collections::HashSet<&str> =
            bundled_models().iter().map(|m| m.id.as_str()).collect();
        let mut missing = Vec::new();
        for slot in DefaultModel::ALL {
            if !ids.contains(slot.id()) {
                missing.push(format!("{slot:?} -> {}", slot.id()));
            }
        }
        assert!(
            missing.is_empty(),
            "default models absent from the bundled catalog: {missing:?}"
        );
    }

    /// Anthropic's published 1M-context list, pinned against the snapshot.
    ///
    /// models.dev is the snapshot's upstream and it is not infallible: it
    /// lists Claude Sonnet 4.5 with a 1,000,000-token window, while
    /// Anthropic's context-windows doc names the models that have 1M and then
    /// says "Other Claude models, including Claude Sonnet 4.5, have a
    /// 200k-token context window". A too-large window is the dangerous
    /// direction, because the overflow detector compacts at a fraction of it
    /// and the API rejects the oversized request instead.
    ///
    /// scripts/fetch-model-catalog.mjs carries the correction; this asserts it
    /// Every catalogued model must carry a published price.
    ///
    /// A row with no cost is not free, but it is spent as if it were:
    /// conductor's computeCostDetails returns a zero breakdown when it finds
    /// no pricing entry, so an unpriced model reports no spend at all.
    /// `gemma-4-26b-a4b-it` reached the catalog that way, with models.dev
    /// carrying no cost while OpenRouter published $0.09/$0.30 for the same
    /// weights.
    ///
    /// The exceptions are OpenRouter's routing models. They publish
    /// `"prompt": "-1"`, meaning the price is whatever the model they route to
    /// charges, so no fixed number exists to record. They are named here one
    /// by one rather than matched by prefix, so a new `openrouter/` model that
    /// really is unpriced has to be looked at instead of being absorbed.
    #[test]
    fn every_catalogued_model_has_a_published_price() {
        const PRICED_AT_ROUTING_TIME: [&str; 5] = [
            "openrouter/auto",
            "openrouter/auto-beta",
            "openrouter/bodybuilder",
            "openrouter/fusion",
            "openrouter/pareto-code",
        ];

        let mut unpriced = Vec::new();
        for model in bundled_models() {
            if PRICED_AT_ROUTING_TIME.contains(&model.id.as_str()) {
                continue;
            }
            if bundled_rates(&model.id).is_none() {
                unpriced.push(model.id.clone());
            }
        }
        assert!(
            unpriced.is_empty(),
            "catalogued models with no published price bill as free: {unpriced:?}"
        );
    }

    /// The routing exceptions must stay real models, not a stale allowlist.
    #[test]
    fn every_routing_time_exception_is_still_catalogued() {
        const PRICED_AT_ROUTING_TIME: [&str; 5] = [
            "openrouter/auto",
            "openrouter/auto-beta",
            "openrouter/bodybuilder",
            "openrouter/fusion",
            "openrouter/pareto-code",
        ];

        let ids: std::collections::HashSet<&str> =
            bundled_models().iter().map(|m| m.id.as_str()).collect();
        let stale: Vec<&str> = PRICED_AT_ROUTING_TIME
            .into_iter()
            .filter(|id| !ids.contains(id))
            .collect();
        assert!(
            stale.is_empty(),
            "routing-time exceptions no longer in the catalog: {stale:?}"
        );
    }

    /// survives a regeneration.
    #[test]
    fn anthropic_context_windows_match_the_published_list() {
        const ONE_MILLION: [&str; 9] = [
            "claude-fable-5",
            "claude-fable-5-1",
            "claude-opus-4-6",
            "claude-opus-4-7",
            "claude-opus-4-8",
            "claude-opus-5",
            "claude-opus-5-5",
            "claude-sonnet-4-6",
            "claude-sonnet-5",
        ];

        let mut wrong = Vec::new();
        for model in bundled_models() {
            if model.provider != "anthropic" {
                continue;
            }
            let base = model
                .id
                .split_once("-2025")
                .or_else(|| model.id.split_once("-2026"))
                .map_or(model.id.as_str(), |(head, _)| head);
            let expected = if ONE_MILLION.contains(&base) {
                1_000_000
            } else {
                200_000
            };
            if model.capabilities.context_tokens != expected {
                wrong.push(format!(
                    "{}: {} declared, {expected} published",
                    model.id, model.capabilities.context_tokens
                ));
            }
        }
        assert!(
            wrong.is_empty(),
            "context windows disagree with Anthropic's published list: {wrong:?}"
        );
    }

    /// The desktop built-in provider release derives its Claude rules from
    /// this snapshot via scripts/sync-desktop-claude-model-rules.mjs. If the
    /// two fall out of step the desktop app runs Claude models at the wrong
    /// context window, which is how Opus 4.6, 4.7 and 4.8 came to be capped at
    /// 200,000 tokens against a 1,000,000-token catalog value.
    #[test]
    fn desktop_provider_claude_rules_match_the_bundled_catalog() {
        let desktop: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../desktop/config/provider/zcode-builtin.json"
        )))
        .expect("desktop provider release parses");

        let rules = desktop["config"]["modelConfigRules"]["modelRules"]
            .as_array()
            .expect("modelRules array");

        let mut drift = Vec::new();
        for model in bundled_models() {
            if model.provider != "anthropic" {
                continue;
            }
            // Match the generated direct id exactly. A substring would also
            // select a later rule for a different model such as Opus 5.5.
            let declared = rules.iter().rfind(|rule| {
                rule["modelMatch"]
                    .as_str()
                    .and_then(|pattern| pattern.strip_prefix(".*"))
                    .and_then(|pattern| pattern.split_once("(?:"))
                    .is_some_and(|(id, _)| id == model.id.as_str())
            });
            let Some(rule) = declared else {
                drift.push(format!("{}: no rule", model.id));
                continue;
            };
            let context = rule["config"]["properties"]["contextWindow"].as_u64();
            if context != Some(u64::from(model.capabilities.context_tokens)) {
                drift.push(format!(
                    "{}: contextWindow {context:?}, catalog {}",
                    model.id, model.capabilities.context_tokens
                ));
            }
            if let Some(expected) = model.capabilities.output_tokens {
                let declared_output =
                    rule["config"]["optionSpecs"]["maxOutputTokens"]["max"].as_u64();
                if declared_output != Some(u64::from(expected)) {
                    drift.push(format!(
                        "{}: maxOutputTokens {declared_output:?}, catalog {expected}",
                        model.id
                    ));
                }
            }
        }
        assert!(
            drift.is_empty(),
            "desktop provider release disagrees with the bundled catalog; run \
             node scripts/sync-desktop-claude-model-rules.mjs: {drift:?}"
        );
    }

    use super::*;

    fn catalog_model(id: &str) -> ModelInfo {
        bundled_models()
            .iter()
            .find(|model| model.id == id)
            .unwrap_or_else(|| panic!("{id} must be in the bundled catalog"))
            .clone()
    }

    fn cache_with(fetched_at: u64, models: Vec<ModelInfo>) -> CachedCatalog {
        CachedCatalog {
            checked_at: fetched_at,
            fetched_at,
            last_modified: None,
            models,
        }
    }

    #[test]
    fn managed_model_routes_survive_cache_and_preserve_provider_and_limits() {
        let mut models = Vec::new();
        append_managed_models(&mut models);
        append_managed_models(&mut models);
        assert_eq!(models.len(), 7);
        for price in MANAGED_MODELS {
            let route = format!("evalops/{}", price.id);
            let model = find_model(&route).unwrap();
            assert_eq!(model_route(&model), route);
            assert_eq!(model.name, price.name);
            assert_eq!(default_max_output_tokens(&route), DEFAULT_MAX_OUTPUT_TOKENS);
            assert!(!model.capabilities.vision);
            assert!(!model.capabilities.reasoning);
            assert!(model.capabilities.tools && model.capabilities.streaming);
        }
    }

    #[test]
    fn astra_catalog_and_refresh_preserve_capabilities() {
        let model = bundled_models()
            .iter()
            .find(|model| model.provider == "openai" && model.id == "gpt-6-astra")
            .expect("bundled Astra");
        assert_eq!(model.capabilities.protocol, ModelProtocol::OpenAiResponses);
        assert_eq!(model.capabilities.context_tokens, 1_050_000);
        assert_eq!(model.capabilities.output_tokens, Some(128_000));
        assert!(
            model.capabilities.tools && model.capabilities.reasoning && model.capabilities.vision
        );
        let payload = serde_json::json!({
            "openai": {"models": {}}, "anthropic": {"models": {}},
            "google": {"models": {}}, "xai": {"models": {}}
        });
        let mut refreshed = map_models_dev_catalog(&payload).unwrap();
        ensure_astra_model(&mut refreshed);
        assert_eq!(
            serde_json::to_value(&refreshed[0]).unwrap(),
            serde_json::to_value(model).unwrap()
        );
        refreshed[0].capabilities.protocol = ModelProtocol::OpenAiChat;
        ensure_astra_model(&mut refreshed);
        assert_eq!(
            refreshed[0].capabilities.protocol,
            ModelProtocol::OpenAiResponses
        );
        assert_eq!(
            refreshed
                .iter()
                .filter(|model| model.id == "gpt-6-astra")
                .count(),
            1
        );
        assert_eq!(
            catalog_protocol("openai", "gpt-6-astra"),
            ModelProtocol::OpenAiResponses
        );
        assert_eq!(
            catalog_protocol("openrouter", "openai/gpt-6-astra"),
            ModelProtocol::OpenAiChat
        );
        let routed = bundled_models()
            .iter()
            .find(|model| model.provider == "openrouter" && model.id == "openai/gpt-6-astra")
            .expect("OpenRouter Astra");
        assert_eq!(model_route(routed), "openrouter/openai/gpt-6-astra");
        assert!(find_model("openai-codex/gpt-6-astra").is_some());
        assert!(!has_provider_mismatch("openai-codex/gpt-6-astra"));
        assert_eq!(catalog_output_token_limit("gpt-6-astra"), Some(128_000));
    }

    #[test]
    fn bundled_catalog_splits_native_and_openrouter_rows() {
        let native = bundled_native_models();
        let openrouter = bundled_openrouter_models();
        assert!(!native.is_empty());
        assert!(!openrouter.is_empty());
        assert!(native.iter().all(|model| model.provider != "openrouter"));
        assert!(
            openrouter
                .iter()
                .all(|model| model.provider == "openrouter")
        );
        assert_eq!(
            native.len() + openrouter.len(),
            bundled_models().len(),
            "native + OpenRouter rows must cover the bundled snapshot"
        );
    }

    #[test]
    fn bundled_snapshot_parses_and_is_healthy() {
        assert!(BUNDLED_CATALOG.generated_at > 0);
        assert!(bundled_models().len() >= 50);
        let openrouter_count = bundled_models()
            .iter()
            .filter(|model| model.provider == "openrouter")
            .count();
        assert!(
            openrouter_count >= 200,
            "bundled snapshot must include current OpenRouter models, found {openrouter_count}"
        );
        for model in bundled_models() {
            assert!(
                model.capabilities.context_tokens > 0,
                "{} must declare a context window",
                model.id
            );
            if model.provider == "openrouter" {
                assert!(
                    model.id.contains('/'),
                    "OpenRouter catalog id {} must keep its vendor namespace",
                    model.id
                );
                assert!(
                    !model.id.ends_with(":batch"),
                    "OpenRouter batch route {} is not an interactive catalog row",
                    model.id
                );
                assert_eq!(model.capabilities.protocol, ModelProtocol::OpenAiChat);
                // An OpenRouter row normally records OpenRouter as its source.
                // A vendor correction replaces the value and the provenance
                // together, so a Verified row carries the vendor page instead:
                // anthropic/claude-sonnet-4.5 is corrected to 200k against
                // OpenRouter's advertised 1M. What matters is that the row says
                // where its number came from, not that it always says the same
                // thing.
                match model.verification.state {
                    VerificationState::Verified => assert!(
                        model.verification.source.starts_with("https://"),
                        "corrected OpenRouter row {} must cite a vendor URL, got {}",
                        model.id,
                        model.verification.source
                    ),
                    _ => assert_eq!(model.verification.source, "openrouter"),
                }
                continue;
            }
            assert!(
                model.capabilities.tools,
                "{} must be tool-capable",
                model.id
            );
            assert!(
                CATALOG_PROVIDERS.contains(&model.provider.as_str()),
                "{} has unexpected provider {}",
                model.id,
                model.provider
            );
            assert!(
                !model.id.contains('/'),
                "catalog id {} must not contain a provider qualifier",
                model.id
            );
        }
    }

    #[test]
    fn authless_llamacpp_model_is_available_offline() {
        let verification = verify_model_offline("llamacpp/Qwen3.8-27B");

        assert_eq!(verification.state, VerificationState::Verified);
        assert_eq!(verification.source, "provider-registry");
        assert_eq!(verification.detail, None);
    }

    #[test]
    fn local_qwen_model_exposes_runtime_limits() {
        let model = find_model("llamacpp/Qwen3.8-27B").expect("local Qwen catalog row");

        assert_eq!(model.provider, "llamacpp");
        assert!(model.capabilities.tools);
        assert!(model.capabilities.vision);
        assert!(model.capabilities.reasoning);
        assert_eq!(model.capabilities.context_tokens, 262_144);
        assert_eq!(model.capabilities.output_tokens, Some(32_768));
        assert_eq!(default_max_output_tokens("llamacpp/Qwen3.8-27B"), 32_768);
    }

    #[test]
    fn bundled_snapshot_drops_deprecated_ids() {
        for dead in [
            "gpt-5.1-codex-max",
            "gemini-2.0-flash",
            "claude-sonnet-4-5-20250514",
        ] {
            assert!(
                find_model(dead).is_none(),
                "deprecated id {dead} must not survive regeneration"
            );
        }
    }

    #[test]
    fn default_models_exist_in_catalog() {
        for provider in ["anthropic", "openai", "google", "xai", "openrouter"] {
            let default = default_model_for_provider(provider).expect("default model");
            let model = bundled_models()
                .iter()
                .find(|model| model.id == default && model.provider == provider)
                .unwrap_or_else(|| panic!("{provider}/{default} must be in the bundled catalog"));
            assert_eq!(model.provider, provider);
        }
        assert_eq!(default_model_for_provider("openai"), Some("gpt-5.6"));
        assert_eq!(default_model_for_provider("openai-codex"), Some("gpt-5.6"));
        assert_eq!(default_model_for_provider("codex"), Some("gpt-5.6"));
        assert_eq!(default_model_for_provider("nope"), None);
        assert_eq!(
            default_model_for_provider("vertex-ai"),
            Some("gemini-2.5-pro")
        );
        assert_eq!(default_model_for_provider("vertex"), Some("gemini-2.5-pro"));
    }

    #[test]
    fn catalog_mirrors_google_gemini_models_for_vertex_routes() {
        let google = find_model("google/gemini-2.5-pro").expect("Google Gemini catalog row");
        let vertex = find_model("vertex-ai/gemini-2.5-pro").expect("Vertex Gemini catalog row");
        let vertex_alias = find_model("vertex/gemini-2.5-pro").expect("Vertex alias catalog row");

        assert_eq!(vertex.provider, "vertex-ai");
        assert_eq!(vertex_alias.provider, "vertex-ai");
        assert_eq!(vertex.id, google.id);
        assert_eq!(vertex.capabilities, google.capabilities);
        assert_eq!(vertex.verification, google.verification);
        assert!(!has_provider_mismatch("vertex-ai/gemini-2.5-pro"));
        assert!(!has_provider_mismatch("vertex/gemini-2.5-pro"));
        assert!(find_model("vertex-ai/future-gemini").is_none());
    }

    #[test]
    fn local_runtime_routes_do_not_fail_catalog_provider_mismatch_checks() {
        for route in [
            "llamacpp/gpt-5.5",
            "llama.cpp/gpt-5.5",
            "lmstudio/gpt-5.5",
            "lm-studio/gpt-5.5",
            "ollama/gpt-5.5",
        ] {
            assert!(!has_provider_mismatch(route), "local route {route}");
        }
        assert!(has_provider_mismatch("anthropic/gpt-5.5"));
    }

    #[test]
    fn model_inspection_reports_provenance_without_secret_values() {
        let mut env = std::collections::HashMap::new();
        env.insert(
            "OPENAI_API_KEY".to_string(),
            "super-secret-value".to_string(),
        );
        env.insert(
            "OPENAI_BASE_URL".to_string(),
            "https://user:password@gateway.example.test/v1/path-secret?token=secret".to_string(),
        );
        let inspection = inspect_model_with_env("openai/gpt-5.5", &env).unwrap();
        let json = serde_json::to_string(&inspection).unwrap();
        assert!(!json.contains("super-secret-value"));
        assert!(!json.contains("password"));
        assert!(!json.contains("path-secret"));
        assert!(!json.contains("token=secret"));
        assert_eq!(
            inspection.resolved.base_url.as_deref(),
            Some("https://gateway.example.test/")
        );
        assert_eq!(inspection.sources["auth"], "environment:OPENAI_API_KEY");
        assert_eq!(inspection.sources["baseUrl"], "environment:OPENAI_BASE_URL");
        assert_eq!(inspection.resolved.provider, "openai");
    }

    #[test]
    fn model_inspection_resolves_uncataloged_models() {
        let inspection =
            inspect_model_with_env("openai/future-custom-model", &HashMap::new()).unwrap();

        assert!(inspection.catalog.is_none());
        assert!(inspection.resolved.capabilities.is_none());
        assert_eq!(inspection.resolved.provider, "openai");
        assert_eq!(inspection.sources["catalog"], "uncataloged");
        assert_eq!(inspection.sources["capabilities"], "unavailable");
    }

    #[test]
    fn inspected_routes_share_the_transport_contract() {
        for (route, protocol, temperature) in [
            (
                "openai/gpt-6-astra",
                crate::ai::OpenAiWireProtocol::OpenAiResponses,
                false,
            ),
            (
                "openrouter/openai/gpt-6-astra",
                crate::ai::OpenAiWireProtocol::OpenAiChat,
                false,
            ),
            (
                "ollama/gpt-6-astra",
                crate::ai::OpenAiWireProtocol::OpenAiChat,
                true,
            ),
        ] {
            let inspected = inspect_model_with_env(route, &HashMap::new()).unwrap();
            let request = inspected.resolved.request_capabilities.unwrap();
            assert_eq!(request.protocol, protocol, "{route}");
            assert_eq!(request.temperature, temperature, "{route}");
        }
    }

    #[test]
    fn mapping_filters_deprecated_and_tool_less_models() {
        let payload = serde_json::json!({
            "anthropic": {"models": {
                "claude-good": {"name": "Good", "tool_call": true, "reasoning": true,
                    "limit": {"context": 200_000, "output": 64_000}, "modalities": {"input": ["text", "image"]}},
                "claude-no-output": {"name": "NoOutput", "tool_call": true,
                    "limit": {"context": 200_000}},
                "claude-old": {"name": "Old", "tool_call": true, "status": "deprecated",
                    "limit": {"context": 200_000}},
                "claude-no-tools": {"name": "NoTools", "tool_call": false,
                    "limit": {"context": 200_000}},
                "claude-no-context": {"name": "NoCtx", "tool_call": true,
                    "limit": {"context": 0}}
            }},
            "openai": {"models": {}},
            "google": {"models": {}},
            "xai": {"models": {}}
        });
        let models = map_models_dev_catalog(&payload).expect("mapping");
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "claude-good");
        assert!(models[0].capabilities.vision);
        assert!(models[0].capabilities.reasoning);
        assert_eq!(models[0].capabilities.protocol, ModelProtocol::Anthropic);
        assert_eq!(models[0].capabilities.output_tokens, Some(64_000));
        assert_eq!(models[1].id, "claude-no-output");
        assert_eq!(models[1].capabilities.output_tokens, None);
    }

    #[test]
    fn mapping_assigns_protocols() {
        let payload = serde_json::json!({
            "anthropic": {"models": {}},
            "google": {"models": {
                "gemini-9-pro": {"tool_call": true, "limit": {"context": 1_000_000}}
            }},
            "openai": {"models": {
                "gpt-5.5": {"tool_call": true, "limit": {"context": 400_000}},
                "gpt-5.3-codex": {"tool_call": true, "limit": {"context": 400_000}},
                "o3": {"tool_call": true, "limit": {"context": 200_000}},
                "gpt-4o": {"tool_call": true, "limit": {"context": 128_000}}
            }},
            "xai": {"models": {
                "grok-9": {"tool_call": true, "limit": {"context": 131_072}}
            }}
        });
        let models = map_models_dev_catalog(&payload).expect("mapping");
        let protocol_of = |id: &str| {
            models
                .iter()
                .find(|model| model.id == id)
                .map(|model| model.capabilities.protocol)
                .unwrap_or_else(|| panic!("{id} mapped"))
        };
        assert_eq!(protocol_of("gpt-5.5"), ModelProtocol::OpenAiResponses);
        assert_eq!(protocol_of("gpt-5.3-codex"), ModelProtocol::OpenAiResponses);
        assert_eq!(protocol_of("o3"), ModelProtocol::OpenAiResponses);
        assert_eq!(protocol_of("gpt-4o"), ModelProtocol::OpenAiChat);
        assert_eq!(protocol_of("grok-9"), ModelProtocol::OpenAiChat);
        assert_eq!(protocol_of("gemini-9-pro"), ModelProtocol::Google);
    }

    #[test]
    fn overlay_precedence_follows_staleness_guard() {
        let bundled = BundledCatalog {
            generated_at: 1_000,
            models: vec![catalog_model("gpt-5.5")],
        };
        let newer = cache_with(1_000, vec![catalog_model("gpt-4o")]);
        assert_eq!(select_models(&bundled, Some(&newer))[0].id, "gpt-4o");

        let older = cache_with(999, vec![catalog_model("gpt-4o")]);
        assert_eq!(select_models(&bundled, Some(&older))[0].id, "gpt-5.5");

        let empty = cache_with(5_000, Vec::new());
        assert_eq!(select_models(&bundled, Some(&empty))[0].id, "gpt-5.5");

        assert_eq!(select_models(&bundled, None)[0].id, "gpt-5.5");
    }

    #[test]
    fn malformed_cache_falls_back_to_bundled() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("models-catalog.json");
        std::fs::write(&path, "{ not json").expect("write malformed cache");
        assert!(load_cache(&path).is_none());

        std::fs::write(&path, "{\"checked_at\": 1}").expect("write partial cache");
        let cache = load_cache(&path).expect("partial cache still parses with defaults");
        assert!(cache.models.is_empty());
    }

    #[test]
    fn atomic_cache_write_replaces_existing_cache() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("nested").join("models-catalog.json");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime
            .block_on(write_cache_atomic(
                &path,
                &cache_with(21, vec![catalog_model("gpt-4o")]),
            ))
            .expect("initial write");
        runtime
            .block_on(write_cache_atomic(
                &path,
                &cache_with(42, vec![catalog_model("gpt-5.5")]),
            ))
            .expect("replacement write");

        let loaded = load_cache(&path).expect("cache loads");
        assert_eq!(loaded.fetched_at, 42);
        assert_eq!(loaded.models[0].id, "gpt-5.5");
        assert_eq!(
            std::fs::read_dir(path.parent().expect("cache parent"))
                .expect("read cache parent")
                .count(),
            1,
            "temporary file must be renamed away"
        );
    }

    #[test]
    fn local_qwen_model_route_preserves_its_provider() {
        let model = find_model("llamacpp/Qwen3.8-27B").expect("local Qwen catalog row");

        assert_eq!(model_route(&model), "llamacpp/Qwen3.8-27B");
    }

    #[test]
    fn catalog_output_token_limit_resolves_managed_and_qualified_ids() {
        assert_eq!(catalog_output_token_limit("gpt-5.6"), Some(128_000));
        assert_eq!(catalog_output_token_limit("openai/gpt-5.6"), Some(128_000));
        // Managed gateway prefixes strip to the underlying OpenAI catalog id.
        assert_eq!(catalog_output_token_limit("evalops/gpt-5.6"), Some(128_000));
        assert_eq!(
            catalog_output_token_limit("maestro-managed/gpt-5.6"),
            Some(128_000)
        );
        assert_eq!(catalog_output_token_limit("gpt-99-turbo"), None);
        assert_eq!(catalog_output_token_limit("openai/gpt-99-turbo"), None);
        assert_eq!(catalog_output_token_limit("evalops/gpt-99-turbo"), None);
    }

    #[test]
    fn default_max_output_tokens_prefers_the_full_catalog_limit() {
        // Distinct catalog output limits (smaller than context) pass through.
        // Uncataloged models and output==context rows use the fallback.
        assert_eq!(default_max_output_tokens("gpt-5.6"), 128_000);
        assert_eq!(default_max_output_tokens("evalops/gpt-5.6"), 128_000);
        assert_eq!(
            default_max_output_tokens("gpt-99-turbo"),
            DEFAULT_MAX_OUTPUT_TOKENS
        );
        assert_eq!(DEFAULT_MAX_OUTPUT_TOKENS, 16_384);
    }

    #[test]
    fn clamp_output_budget_keeps_half_the_window_on_small_contexts() {
        assert_eq!(clamp_output_budget_to_context(4_096, Some(8_192)), 4_096);
        assert_eq!(
            clamp_output_budget_to_context(DEFAULT_MAX_OUTPUT_TOKENS, Some(128_000)),
            DEFAULT_MAX_OUTPUT_TOKENS
        );
    }

    #[test]
    fn default_max_output_tokens_does_not_consume_the_full_context_window() {
        let model = find_model("openrouter/moonshotai/kimi-k2.7-code")
            .expect("openrouter kimi catalog row");
        assert_eq!(model.capabilities.context_tokens, 262_144);
        let budget = default_max_output_tokens("openrouter/moonshotai/kimi-k2.7-code");
        assert!(
            budget
                <= model
                    .capabilities
                    .context_tokens
                    .saturating_sub(MIN_PROMPT_TOKEN_HEADROOM),
            "runtime catalog overlays must still reserve prompt headroom"
        );
    }

    #[test]
    fn default_max_output_tokens_honors_user_config_json_max_tokens() {
        let _guard = crate::config::test_process_env_lock();
        let home = tempfile::tempdir().expect("maestro home");
        let previous = std::env::var_os("MAESTRO_HOME");
        std::env::set_var("MAESTRO_HOME", home.path());
        fs::write(
            home.path().join("config.json"),
            r#"{
              "providers": [{
                "id": "openrouter",
                "models": [{
                  "id": "moonshotai/kimi-k2.7-codex",
                  "contextWindow": 262144,
                  "maxTokens": 16384
                }]
              }]
            }"#,
        )
        .expect("write user config");
        let budget = default_max_output_tokens("openrouter/moonshotai/kimi-k2.7-codex");
        match previous {
            Some(value) => std::env::set_var("MAESTRO_HOME", value),
            None => std::env::remove_var("MAESTRO_HOME"),
        }
        assert_eq!(budget, 16_384);
    }

    #[test]
    fn distinct_output_tokens_rejects_context_copies() {
        assert_eq!(distinct_output_tokens(Some(262_144), Some(262_144)), None);
        assert_eq!(distinct_output_tokens(Some(65_536), Some(66_000)), None);
        assert_eq!(distinct_output_tokens(Some(262_144), Some(262_140)), None);
        assert_eq!(distinct_output_tokens(Some(1_000_000), Some(900_000)), None);
        assert_eq!(distinct_output_tokens(Some(262_143), Some(235_929)), None);
        assert_eq!(
            distinct_output_tokens(Some(200_000), Some(100_000)),
            Some(100_000)
        );
        assert_eq!(distinct_output_tokens(Some(200_000), None), None);
    }

    #[test]
    fn runtime_mappers_keep_vendor_corrected_context() {
        let native = serde_json::json!({
            "anthropic": {"models": {
                "claude-sonnet-4-5": {"tool_call": true, "limit": {"context": 1_000_000, "output": 64_000}},
                "claude-sonnet-4-5-20250929": {"tool_call": true, "limit": {"context": 1_000_000, "output": 64_000}}
            }},
            "google": {"models": {}}, "openai": {"models": {}}, "xai": {"models": {}}
        });
        let openrouter = serde_json::json!({"data": [{
            "id": "anthropic/claude-sonnet-4.5", "context_length": 1_000_000,
            "top_provider": {"max_completion_tokens": 900_000}
        }]});
        let mut models = map_models_dev_catalog(&native).expect("native mapping");
        models.extend(
            map_openrouter_catalog_with_limits(&openrouter, None).expect("OpenRouter mapping"),
        );
        apply_vendor_corrections(&mut models).expect("checked-in corrections");
        for id in [
            "claude-sonnet-4-5",
            "claude-sonnet-4-5-20250929",
            "anthropic/claude-sonnet-4.5",
        ] {
            let model = models
                .iter()
                .find(|model| model.id == id)
                .expect("corrected model");
            assert_eq!(model.capabilities.context_tokens, 200_000, "{id}");
            assert_eq!(
                model.verification.state,
                VerificationState::Verified,
                "{id}"
            );
        }
        let routed = models
            .iter()
            .find(|model| model.id == "anthropic/claude-sonnet-4.5")
            .unwrap();
        assert_eq!(routed.capabilities.output_tokens, None);
    }

    #[test]
    fn catalog_exposes_capabilities_separately_from_verification() {
        let model = find_model("openai/gpt-5.6").expect("catalog model");
        assert!(model.capabilities.tools);
        assert!(model.capabilities.reasoning);
        assert_eq!(model.capabilities.protocol, ModelProtocol::OpenAiResponses);
        assert_eq!(model.verification.state, VerificationState::Catalog);
        assert_eq!(model.capabilities.context_tokens, 1_050_000);
        assert_eq!(model.capabilities.output_tokens, Some(128_000));
    }

    #[test]
    fn catalog_rejects_provider_model_mismatches() {
        assert!(find_model("anthropic/gpt-4o").is_none());
        assert!(has_provider_mismatch("anthropic/gpt-4o"));
        assert!(find_model("anthropic/claude-sonnet-4-6").is_some());
        assert!(!has_provider_mismatch("anthropic/claude-sonnet-4-6"));
        assert!(find_model("claude/claude-sonnet-4-6").is_some());
        assert!(!has_provider_mismatch("openai/custom-model"));
        // openai-codex is subscription transport over OpenAI catalog ids.
        assert!(find_model("openai-codex/gpt-5.6").is_some());
        assert!(!has_provider_mismatch("openai-codex/gpt-5.6"));
    }

    #[test]
    fn openrouter_catalog_routes_keep_the_vendor_namespace() {
        let model = find_model("openrouter/anthropic/claude-sonnet-4.5")
            .expect("OpenRouter Claude route must be cataloged");
        assert_eq!(model.provider, "openrouter");
        assert_eq!(model.id, "anthropic/claude-sonnet-4.5");
        assert_eq!(
            model_route(&model),
            "openrouter/anthropic/claude-sonnet-4.5"
        );
        assert_eq!(model.capabilities.protocol, ModelProtocol::OpenAiChat);
        assert!(model.capabilities.tools);
        assert!(!has_provider_mismatch(
            "openrouter/anthropic/claude-sonnet-4.5"
        ));
        assert_eq!(
            find_model("anthropic/claude-sonnet-4.5").map(|model| model.provider),
            Some("openrouter".to_owned()),
            "an OpenRouter vendor id without the transport prefix still resolves"
        );
    }

    #[test]
    fn openrouter_mapping_skips_batch_and_keeps_chat_protocol() {
        let payload = serde_json::json!({
            "data": [
                {
                    "id": "anthropic/claude-sonnet-4.5",
                    "name": "Claude Sonnet 4.5",
                    "description": "Frontier Sonnet",
                    "context_length": 200_000,
                    "architecture": {"input_modalities": ["text", "image"]},
                    "supported_parameters": ["tools", "tool_choice", "reasoning"],
                    "reasoning": {"default_enabled": true},
                    "top_provider": {"max_completion_tokens": 64_000}
                },
                {
                    "id": "openai/gpt-5.6",
                    "name": "GPT-5.6",
                    "context_length": 400_000,
                    "supported_parameters": ["tools"]
                },
                {
                    "id": "anthropic/claude-sonnet-4.5:batch",
                    "name": "Claude Sonnet 4.5 Batch",
                    "context_length": 200_000,
                    "supported_parameters": ["tools"]
                },
                {
                    "id": "vendor/no-context",
                    "name": "Broken",
                    "supported_parameters": ["tools"]
                }
            ]
        });
        let models = map_openrouter_catalog(&payload).expect("mapping");
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "anthropic/claude-sonnet-4.5");
        assert!(models[0].capabilities.tools);
        assert!(models[0].capabilities.vision);
        assert!(models[0].capabilities.reasoning);
        assert_eq!(models[0].capabilities.output_tokens, Some(64_000));
        assert_eq!(models[1].id, "openai/gpt-5.6");
        assert_eq!(models[1].capabilities.protocol, ModelProtocol::OpenAiChat);
        assert_eq!(models[1].capabilities.output_tokens, None);
        assert!(!models.iter().any(|model| model.id.ends_with(":batch")));
    }

    #[test]
    fn openrouter_mapping_drops_output_equal_to_context_and_overlays_models_dev() {
        let openrouter = serde_json::json!({
            "data": [
                {
                    "id": "moonshotai/kimi-k2.7-code",
                    "name": "Kimi K2.7 Code",
                    "context_length": 262_144,
                    "top_provider": {"max_completion_tokens": 262_144},
                    "supported_parameters": ["tools"]
                },
                {
                    "id": "google/gemma-4-31b-it",
                    "name": "Gemma 4 31B",
                    "context_length": 262_144,
                    "top_provider": {"max_completion_tokens": 262_144},
                    "supported_parameters": ["tools"]
                },
                {
                    "id": "openai/o4-mini",
                    "name": "o4 Mini",
                    "context_length": 200_000,
                    "top_provider": {"max_completion_tokens": 100_000},
                    "supported_parameters": ["tools"]
                }
            ]
        });
        let models_dev = serde_json::json!({
            "google": {"models": {
                "gemma-4-31b-it": {"limit": {"context": 262_144, "output": 32_768}}
            }},
            "openai": {"models": {
                "o4-mini": {"limit": {"context": 200_000, "output": 100_000}, "status": "deprecated"}
            }},
            "moonshotai": {"models": {
                "kimi-k2.7-code": {"limit": {"context": 262_144, "output": 262_144}}
            }},
            "openrouter": {"models": {
                "google/gemma-4-31b-it": {"limit": {"context": 262_144, "output": 262_144}}
            }}
        });
        let limits = DevTokenLimits::from_models_dev(&models_dev);
        let models =
            map_openrouter_catalog_with_limits(&openrouter, Some(&limits)).expect("mapping");
        let by_id = |id: &str| {
            models
                .iter()
                .find(|model| model.id == id)
                .unwrap_or_else(|| panic!("{id}"))
        };
        assert_eq!(
            by_id("moonshotai/kimi-k2.7-code")
                .capabilities
                .context_tokens,
            262_144
        );
        assert_eq!(
            by_id("moonshotai/kimi-k2.7-code")
                .capabilities
                .output_tokens,
            None
        );
        assert_eq!(
            by_id("google/gemma-4-31b-it").capabilities.output_tokens,
            Some(32_768)
        );
        assert_eq!(
            by_id("openai/o4-mini").capabilities.output_tokens,
            Some(100_000)
        );
    }

    #[test]
    fn generated_model_facts_match_the_bundled_snapshot() {
        use crate::model_facts_generated::MODEL_FACTS;

        let parsed: serde_json::Value =
            serde_json::from_str(BUNDLED_CATALOG_JSON).expect("bundled snapshot parses");
        let models = parsed["models"].as_array().expect("models array");
        assert_eq!(
            models.len(),
            MODEL_FACTS.len(),
            "one generated row per catalogued model"
        );
        assert!(
            MODEL_FACTS.windows(2).all(|pair| pair[0].0 < pair[1].0),
            "generated rows must be in byte order for the binary search"
        );
        for model in models {
            let id = model["id"].as_str().expect("id");
            let index = MODEL_FACTS
                .binary_search_by(|(key, _)| key.cmp(&id))
                .unwrap_or_else(|_| panic!("{id} missing from the generated binding"));
            let facts = MODEL_FACTS[index].1;
            let caps = &model["capabilities"];
            assert_eq!(
                Some(u64::from(facts.context_window)),
                caps["context_tokens"].as_u64(),
                "{id} context"
            );
            assert_eq!(
                facts.max_output_tokens.map(u64::from),
                caps["output_tokens"].as_u64(),
                "{id} output"
            );
            let cost = &model["cost"];
            assert_eq!(
                facts.input_per_million,
                cost["input"].as_f64(),
                "{id} input"
            );
            assert_eq!(
                facts.output_per_million,
                cost["output"].as_f64(),
                "{id} output"
            );
            assert_eq!(
                facts.cache_read_per_million,
                cost["cache_read"].as_f64(),
                "{id} cache_read"
            );
            assert_eq!(
                facts.cache_write_per_million,
                cost["cache_write"].as_f64(),
                "{id} cache_write"
            );
        }
    }

    #[test]
    fn generated_model_facts_replay_the_resolution_fixture() {
        use crate::model_facts_generated::model_facts_candidates;

        // Computed by the generator's reference implementation; the
        // TypeScript and Python bindings replay the same file.
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("model_facts_resolution.generated.json"))
                .expect("resolution fixture parses");
        let probes = fixture["probes"].as_array().expect("probes array");
        assert!(
            probes.len() > 100,
            "fixture is unexpectedly small: {}",
            probes.len()
        );
        for entry in probes {
            let probe = entry["probe"].as_str().expect("probe");
            let expected: Vec<&str> = entry["candidates"]
                .as_array()
                .expect("candidates")
                .iter()
                .map(|value| value.as_str().expect("candidate"))
                .collect();
            assert_eq!(
                model_facts_candidates(probe),
                expected,
                "candidates for {probe:?}"
            );
            match (model_facts(probe), entry["resolved"].as_str()) {
                (None, None) => {}
                (Some(found), Some(id)) => {
                    assert_eq!(Some(found), model_facts(id), "{probe:?} resolves to {id}");
                }
                (found, expected) => {
                    panic!("{probe:?}: found {found:?}, fixture says {expected:?}")
                }
            }
        }
    }

    #[test]
    fn bundled_limits_and_rates_read_the_generated_binding() {
        assert_eq!(
            bundled_limits("anthropic/claude-opus-5.5"),
            Some((1_000_000, Some(128_000)))
        );
        assert_eq!(
            bundled_limits("claude-opus-5.5"),
            Some((1_000_000, Some(128_000)))
        );
        assert_eq!(
            bundled_limits("vertex-ai/gemini-3.7-flash"),
            Some((1_048_576, Some(65_536)))
        );
        // Sonnet 4.5 is 200k; models.dev says 1M and the snapshot carries the
        // documented correction.
        assert_eq!(
            bundled_limits("claude-sonnet-4-5").map(|(context, _)| context),
            Some(200_000)
        );
        // A bare name the catalog holds only as a route resolves to it.
        assert_eq!(bundled_limits("gpt-4"), bundled_limits("openai/gpt-4"));
        assert!(bundled_limits("gpt-4").is_some());
        assert_eq!(bundled_limits(""), None);
        assert_eq!(bundled_limits("anthropic/"), None);
        assert_eq!(bundled_limits("not-a-real-model"), None);

        // Rates are copied bit-for-bit from the generated table, so the
        // comparison is exact by construction; `to_bits` keeps clippy's
        // float_cmp lint out of it.
        let opus = bundled_rates("claude-opus-5-5").expect("opus rates");
        assert_eq!(opus.input_per_million.to_bits(), 4.0_f64.to_bits());
        assert_eq!(opus.cache_read_per_million.to_bits(), 0.2_f64.to_bits());
        // No vendor rate at all: no rates.
        assert_eq!(bundled_rates("openrouter/auto"), None);
        // A cache rate the catalog omits reads as 0.0.
        let pro = bundled_rates("gpt-5.5-pro").expect("pro rates");
        assert_eq!(pro.input_per_million.to_bits(), 30.0_f64.to_bits());
        assert_eq!(pro.cache_read_per_million.to_bits(), 0.0_f64.to_bits());
    }

    #[test]
    fn unknown_ids_pass_through_to_provider_registry() {
        // Not in the catalog, but routing must keep working unchanged.
        assert!(find_model("openai/gpt-99-turbo").is_none());
        assert!(!has_provider_mismatch("openai/gpt-99-turbo"));
        let env = std::collections::HashMap::new();
        let resolved = ProviderRegistry::resolve("openai/gpt-99-turbo", &env)
            .expect("unknown ids still resolve to their provider");
        assert_eq!(resolved.provider.id, "openai");
        let bare = ProviderRegistry::resolve("gpt-99-turbo", &env)
            .expect("bare unknown ids fall back to the default provider");
        assert_eq!(bare.provider.id, "openai");
    }
}
