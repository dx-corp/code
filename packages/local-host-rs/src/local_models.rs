//! Local OpenAI-compatible runtime discovery.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock, RwLock, mpsc};
use std::time::{Duration, Instant};

use crate::ai::ProviderRegistry;
use crate::model_catalog::{
    ModelCapabilities, ModelInfo, ModelProtocol, ModelVerification, VerificationState,
    available_models,
};

/// Canonical local provider IDs in stable display/probe order.
pub const LOCAL_RUNTIME_IDS: [&str; 3] = ["llamacpp", "lmstudio", "ollama"];
const DISCOVERY_TOTAL_TIMEOUT: Duration = Duration::from_millis(750);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalRuntimeEndpoint {
    pub provider: &'static str,
    pub display_name: &'static str,
    pub base_url: String,
}

/// Resolve local runtime endpoints through the canonical provider registry so
/// discovery, setup, and request routing share the same override precedence.
#[must_use]
pub fn local_runtime_endpoints(env: &HashMap<String, String>) -> Vec<LocalRuntimeEndpoint> {
    if crate::safety::vendor_network_disabled() {
        return Vec::new();
    }
    LOCAL_RUNTIME_IDS
        .into_iter()
        .filter_map(|provider| {
            let resolved = ProviderRegistry::resolve(provider, env).ok()?;
            let base_url = resolved.base_url?;
            let display_name = match provider {
                "llamacpp" => "llama.cpp",
                "lmstudio" => "LM Studio",
                "ollama" => "Ollama",
                _ => return None,
            };
            Some(LocalRuntimeEndpoint {
                provider,
                display_name,
                base_url,
            })
        })
        .collect()
}

/// Append the OpenAI-compatible model metadata path to a runtime base URL.
pub fn local_metadata_url(base_url: &str) -> anyhow::Result<String> {
    let mut parsed = url::Url::parse(base_url)?;
    anyhow::ensure!(
        matches!(parsed.scheme(), "http" | "https") && parsed.host_str().is_some(),
        "runtime URL must use HTTP or HTTPS with a host"
    );
    let base_path = parsed.path().trim_end_matches('/');
    parsed.set_path(&format!("{base_path}/models"));
    Ok(parsed.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalDiscoveryBatch {
    pub generation: u64,
    pub models: Vec<ModelInfo>,
    pub runtimes: Vec<LocalRuntimeDiscovery>,
}

/// Discovery evidence only; this never grants inference or tool permission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalRuntimeDiscovery {
    pub provider: &'static str,
    pub display_name: &'static str,
    pub status: LocalRuntimeStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalRuntimeStatus {
    Ready { models: usize },
    Empty,
    Unreachable,
    TimedOut,
    HttpError(u16),
    InvalidEndpoint,
    InvalidResponse,
}

struct EndpointDiscovery {
    models: Vec<ModelInfo>,
    runtime: LocalRuntimeDiscovery,
}

/// Replace the runtime model metadata available to agent-side model resolution.
///
/// This is populated only after the UI accepts a discovery generation, keeping
/// its snapshot aligned with the model selector.
pub fn replace_discovered_models(
    _generation: u64,
    models: &[ModelInfo],
    active_route: Option<&str>,
) {
    let mut snapshot = discovered_models()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let active_key = active_route
        .and_then(canonical_local_route)
        .map(|(provider, model)| (provider.to_owned(), model.to_owned()));
    snapshot.models = replacement_discovered_models(&snapshot.models, models, active_key);
}

fn replacement_discovered_models(
    previous: &HashMap<(String, String), ModelInfo>,
    models: &[ModelInfo],
    active_key: Option<(String, String)>,
) -> HashMap<(String, String), ModelInfo> {
    let retained_active = active_key
        .as_ref()
        .and_then(|key| previous.get(key))
        .cloned();
    let mut next_models = models
        .iter()
        .map(|model| ((model.provider.clone(), model.id.clone()), model.clone()))
        .collect::<HashMap<_, _>>();
    if let (Some(active_key), Some(mut active_model)) = (active_key, retained_active) {
        next_models.entry(active_key).or_insert_with(|| {
            active_model.verification.state = VerificationState::Unavailable;
            active_model.verification.detail = Some(
                "Not reported by the local runtime on the latest refresh; using last-known limits"
                    .to_owned(),
            );
            active_model
        });
    }
    next_models
}

/// Resolve the latest accepted runtime metadata for a selected model route.
#[must_use]
pub fn find_discovered_model(route: &str) -> Option<ModelInfo> {
    let snapshot = discovered_models()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (provider, model) = canonical_local_route(route)?;
    snapshot
        .models
        .get(&(provider.to_owned(), model.to_owned()))
        .cloned()
}

fn canonical_local_route(route: &str) -> Option<(&'static str, &str)> {
    let (provider, model) = route.split_once('/')?;
    let descriptor = ProviderRegistry::descriptor(provider)?;
    LOCAL_RUNTIME_IDS
        .contains(&descriptor.id)
        .then_some((descriptor.id, model))
}

/// Whether a provider-qualified route targets a supported local runtime.
#[must_use]
pub fn is_local_model_route(route: &str) -> bool {
    canonical_local_route(route).is_some()
}

/// Discover one selected local model without blocking the async caller.
///
/// Non-interactive runs use this before constructing their agent so their
/// first request has the same live limits as the interactive selector.
pub async fn discover_local_model(route: &str) -> anyhow::Result<Option<ModelInfo>> {
    let Some((provider, model_id)) = canonical_local_route(route) else {
        return Ok(None);
    };
    let endpoint = local_runtime_endpoints(&std::env::vars().collect())
        .into_iter()
        .find(|endpoint| endpoint.provider == provider);
    let Some(endpoint) = endpoint else {
        return Ok(None);
    };
    let model_id = model_id.to_owned();
    tokio::task::spawn_blocking(move || {
        let client = discovery_client();
        discover_endpoint(&client, &endpoint)
            .models
            .into_iter()
            .find(|model| model.id == model_id)
    })
    .await
    .map_err(|error| anyhow::anyhow!("local model discovery task failed: {error}"))
}

#[derive(Debug, Default)]
struct DiscoveredModelSnapshot {
    models: HashMap<(String, String), ModelInfo>,
}

fn discovered_models() -> &'static RwLock<DiscoveredModelSnapshot> {
    static MODELS: OnceLock<RwLock<DiscoveredModelSnapshot>> = OnceLock::new();
    MODELS.get_or_init(|| RwLock::new(DiscoveredModelSnapshot::default()))
}

#[derive(Debug, Default)]
struct DiscoveryState {
    in_flight: bool,
    last_started: Option<Instant>,
}

#[derive(Clone, Debug)]
pub struct LocalDiscoveryHandle {
    tx: mpsc::SyncSender<()>,
    state: Arc<Mutex<DiscoveryState>>,
}

impl LocalDiscoveryHandle {
    /// Queue a discovery pass unless one is already queued or running.
    pub fn refresh(&self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.in_flight {
            return;
        }
        state.in_flight = true;
        state.last_started = Some(Instant::now());
        if self.tx.try_send(()).is_err() {
            state.in_flight = false;
        }
    }

    /// Queue a discovery pass only when the previous pass is older than the
    /// caller's freshness window.
    pub fn refresh_if_stale(&self, max_age: Duration) {
        let stale = self.state.lock().is_ok_and(|state| {
            !state.in_flight
                && state
                    .last_started
                    .is_none_or(|started| started.elapsed() >= max_age)
        });
        if stale {
            self.refresh();
        }
    }
}

#[must_use]
pub fn spawn_local_model_discovery() -> (LocalDiscoveryHandle, mpsc::Receiver<LocalDiscoveryBatch>)
{
    let env = std::env::vars().collect();
    spawn_local_model_discovery_with_endpoints(local_runtime_endpoints(&env))
}

fn spawn_local_model_discovery_with_endpoints(
    endpoints: Vec<LocalRuntimeEndpoint>,
) -> (LocalDiscoveryHandle, mpsc::Receiver<LocalDiscoveryBatch>) {
    spawn_local_model_discovery_with_client_factory(endpoints, discovery_client)
}

fn spawn_local_model_discovery_with_client_factory(
    endpoints: Vec<LocalRuntimeEndpoint>,
    make_client: impl FnOnce() -> reqwest::blocking::Client + Send + 'static,
) -> (LocalDiscoveryHandle, mpsc::Receiver<LocalDiscoveryBatch>) {
    let (tx, rx) = mpsc::sync_channel(1);
    let (event_tx, event_rx) = mpsc::sync_channel(2);
    let state = Arc::new(Mutex::new(DiscoveryState::default()));
    let worker_state = Arc::clone(&state);
    std::thread::Builder::new()
        .name("maestro-local-model-discovery".to_owned())
        .spawn(move || {
            let client = make_client();
            let mut generation = 0_u64;
            while rx.recv().is_ok() {
                let (models, runtimes) = discover_endpoints_with_client(&client, &endpoints);
                generation = generation.saturating_add(1);
                if let Ok(mut state) = worker_state.lock() {
                    state.in_flight = false;
                }
                if event_tx
                    .send(LocalDiscoveryBatch {
                        generation,
                        models,
                        runtimes,
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .expect("local model discovery thread should start");
    (LocalDiscoveryHandle { tx, state }, event_rx)
}

fn discovery_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_millis(150))
        .timeout(DISCOVERY_TOTAL_TIMEOUT)
        .build()
        .expect("local discovery HTTP client should build")
}

fn discover_endpoints_with_client(
    client: &reqwest::blocking::Client,
    endpoints: &[LocalRuntimeEndpoint],
) -> (Vec<ModelInfo>, Vec<LocalRuntimeDiscovery>) {
    let results = std::thread::scope(|scope| {
        endpoints
            .iter()
            .map(|endpoint| {
                (
                    endpoint,
                    scope.spawn(|| discover_endpoint(client, endpoint)),
                )
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|(endpoint, handle)| {
                handle.join().unwrap_or_else(|_| {
                    endpoint_failure(endpoint, LocalRuntimeStatus::InvalidResponse)
                })
            })
            .collect::<Vec<_>>()
    });
    let mut seen = HashSet::new();
    let mut models = Vec::new();
    let mut runtimes = Vec::new();
    for result in results {
        models.extend(
            result
                .models
                .into_iter()
                .filter(|model| seen.insert((model.provider.clone(), model.id.clone()))),
        );
        runtimes.push(result.runtime);
    }
    (models, runtimes)
}

fn endpoint_failure(
    endpoint: &LocalRuntimeEndpoint,
    status: LocalRuntimeStatus,
) -> EndpointDiscovery {
    EndpointDiscovery {
        models: Vec::new(),
        runtime: LocalRuntimeDiscovery {
            provider: endpoint.provider,
            display_name: endpoint.display_name,
            status,
        },
    }
}

fn discover_endpoint(
    client: &reqwest::blocking::Client,
    endpoint: &LocalRuntimeEndpoint,
) -> EndpointDiscovery {
    let started = Instant::now();
    let Ok(url) = local_metadata_url(&endpoint.base_url) else {
        return endpoint_failure(endpoint, LocalRuntimeStatus::InvalidEndpoint);
    };
    let response = match client.get(url).send() {
        Ok(response) => response,
        Err(error) => {
            return endpoint_failure(
                endpoint,
                if error.is_timeout() {
                    LocalRuntimeStatus::TimedOut
                } else {
                    LocalRuntimeStatus::Unreachable
                },
            );
        }
    };
    if !response.status().is_success() {
        return endpoint_failure(
            endpoint,
            LocalRuntimeStatus::HttpError(response.status().as_u16()),
        );
    }
    let payload: serde_json::Value = match response.json() {
        Ok(payload) => payload,
        Err(error) => {
            return endpoint_failure(
                endpoint,
                if error.is_timeout() {
                    LocalRuntimeStatus::TimedOut
                } else {
                    LocalRuntimeStatus::InvalidResponse
                },
            );
        }
    };
    let Some(entries) = payload.get("data").and_then(serde_json::Value::as_array) else {
        return endpoint_failure(endpoint, LocalRuntimeStatus::InvalidResponse);
    };
    let mut models = parse_models_response(endpoint.provider, &payload);
    if !entries.is_empty() && models.is_empty() {
        return endpoint_failure(endpoint, LocalRuntimeStatus::InvalidResponse);
    }
    if endpoint.provider == "ollama" && !models.is_empty() {
        let remaining = DISCOVERY_TOTAL_TIMEOUT.saturating_sub(started.elapsed());
        if !remaining.is_zero() {
            let running = ollama_running_contexts(client, &endpoint.base_url, remaining);
            apply_ollama_running_contexts(&mut models, &running);
        }
    }
    let status = if models.is_empty() {
        LocalRuntimeStatus::Empty
    } else {
        LocalRuntimeStatus::Ready {
            models: models.len(),
        }
    };
    EndpointDiscovery {
        models,
        runtime: LocalRuntimeDiscovery {
            provider: endpoint.provider,
            display_name: endpoint.display_name,
            status,
        },
    }
}

fn ollama_running_contexts(
    client: &reqwest::blocking::Client,
    base_url: &str,
    timeout: Duration,
) -> HashMap<String, u32> {
    let Ok(mut url) = url::Url::parse(base_url) else {
        return HashMap::new();
    };
    url.set_path("/api/ps");
    url.set_query(None);
    let Ok(payload) = client
        .get(url)
        .timeout(timeout)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .and_then(reqwest::blocking::Response::json::<serde_json::Value>)
    else {
        return HashMap::new();
    };
    ollama_running_contexts_from_payload(&payload)
}

fn apply_ollama_running_contexts(models: &mut [ModelInfo], running: &HashMap<String, u32>) {
    for model in models {
        model.capabilities.context_tokens = running.get(&model.id).copied().unwrap_or_default();
    }
}

fn ollama_running_contexts_from_payload(payload: &serde_json::Value) -> HashMap<String, u32> {
    payload
        .get("models")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let id = entry
                .get("model")
                .or_else(|| entry.get("name"))?
                .as_str()?
                .trim();
            let context_tokens = entry
                .get("context_length")?
                .as_u64()
                .and_then(|tokens| u32::try_from(tokens).ok())?;
            (!id.is_empty() && context_tokens > 0).then(|| (id.to_owned(), context_tokens))
        })
        .collect()
}

fn parse_models_response(provider: &str, payload: &serde_json::Value) -> Vec<ModelInfo> {
    let Some(entries) = payload.get("data").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    let catalog = available_models();
    let mut seen = HashSet::new();
    entries
        .iter()
        .filter_map(|entry| {
            let id = entry.get("id").and_then(serde_json::Value::as_str)?.trim();
            if id.is_empty() || !seen.insert(id.to_owned()) {
                return None;
            }
            let runtime_context_tokens = entry
                .get("max_context_length")
                .or_else(|| entry.get("context_length"))
                .or_else(|| entry.get("meta").and_then(|meta| meta.get("n_ctx")))
                .and_then(serde_json::Value::as_u64)
                .and_then(|tokens| u32::try_from(tokens).ok())
                .unwrap_or(0);
            let runtime_output_tokens = entry
                .get("max_output_tokens")
                .or_else(|| {
                    entry
                        .get("meta")
                        .and_then(|meta| meta.get("max_output_tokens"))
                })
                .and_then(serde_json::Value::as_u64)
                .and_then(|tokens| u32::try_from(tokens).ok())
                .filter(|tokens| *tokens > 0);
            let exact_catalog_model = catalog
                .iter()
                .find(|model| model.provider == provider && model.id == id);
            let mut bare_catalog_matches = catalog.iter().filter(|model| model.id == id);
            let first_bare_catalog_match = bare_catalog_matches.next();
            let equivalent_bare_catalog_model = first_bare_catalog_match.filter(|first| {
                bare_catalog_matches.all(|model| model.capabilities == first.capabilities)
            });
            let catalog_model = exact_catalog_model.or(equivalent_bare_catalog_model);
            let catalog_capabilities = catalog_model.map(|model| model.capabilities.clone());
            let mut capabilities = catalog_capabilities.clone().unwrap_or(ModelCapabilities {
                protocol: ModelProtocol::OpenAiChat,
                tools: false,
                vision: false,
                reasoning: false,
                streaming: true,
                context_tokens: 0,
                output_tokens: None,
            });
            // The catalog row contributes capability ceilings, but a model
            // discovered behind a local OpenAI-compatible runtime must keep
            // using that runtime's Chat Completions transport.
            capabilities.protocol = ModelProtocol::OpenAiChat;
            // Catalog context is a model ceiling, not the server's active
            // allocation. Treat an omitted live n_ctx as unknown so runtime
            // compaction falls back conservatively instead of assuming the
            // catalog maximum.
            capabilities.context_tokens = runtime_context_tokens;
            if let Some(runtime_output_tokens) = runtime_output_tokens {
                capabilities.output_tokens = Some(runtime_output_tokens);
            } else {
                // A catalog output ceiling does not reserve prompt headroom
                // inside the live server context. Without live output
                // metadata, leave completion budgeting to the runtime
                // fallback rather than inheriting an unsafe cap.
                capabilities.output_tokens = None;
            }
            if runtime_context_tokens > 0 {
                capabilities.output_tokens = capabilities
                    .output_tokens
                    .map(|tokens| tokens.min(runtime_context_tokens));
            }
            Some(ModelInfo {
                id: id.to_owned(),
                name: id.to_owned(),
                provider: provider.to_owned(),
                description: format!("Local model served by {provider}"),
                capabilities,
                verification: ModelVerification {
                    state: VerificationState::Verified,
                    source: "local-runtime".to_owned(),
                    detail: Some(if catalog_capabilities.is_some() {
                        "Capabilities from the built-in catalog".to_owned()
                    } else {
                        "Capabilities are not in the catalog".to_owned()
                    }),
                },
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Condvar, Mutex, mpsc};
    use std::time::Duration;

    use super::*;

    #[test]
    fn runtime_endpoints_follow_provider_registry() {
        let env = HashMap::from([
            (
                "LLAMA_CPP_BASE_URL".to_owned(),
                "http://127.0.0.1:9080/v1/".to_owned(),
            ),
            (
                "LM_STUDIO_BASE_URL".to_owned(),
                "http://127.0.0.1:9234/v1/".to_owned(),
            ),
            (
                "OLLAMA_BASE_URL".to_owned(),
                "http://127.0.0.1:91434/v1/".to_owned(),
            ),
        ]);

        let endpoints = local_runtime_endpoints(&env);
        assert_eq!(endpoints.len(), 3);
        assert_eq!(
            endpoints
                .iter()
                .map(|endpoint| (
                    endpoint.provider,
                    endpoint.display_name,
                    endpoint.base_url.as_str(),
                ))
                .collect::<Vec<_>>(),
            vec![
                ("llamacpp", "llama.cpp", "http://127.0.0.1:9080/v1"),
                ("lmstudio", "LM Studio", "http://127.0.0.1:9234/v1"),
                ("ollama", "Ollama", "http://127.0.0.1:91434/v1"),
            ]
        );
        assert_eq!(
            local_metadata_url("http://127.0.0.1:8080/v1/").unwrap(),
            "http://127.0.0.1:8080/v1/models"
        );
    }

    #[test]
    fn discovered_routes_canonicalize_supported_local_provider_aliases() {
        assert_eq!(
            canonical_local_route("lm-studio/my-model"),
            Some(("lmstudio", "my-model"))
        );
        assert_eq!(
            canonical_local_route("llama.cpp/custom-model"),
            Some(("llamacpp", "custom-model"))
        );
        assert_eq!(canonical_local_route("openai/gpt-5.5"), None);
    }

    #[test]
    fn parser_deduplicates_models_and_uses_runtime_context_metadata() {
        let payload = serde_json::json!({"data": [
            {
                "id": "Qwen/Qwen3.8-27B",
                "object": "model",
                "owned_by": "llamacpp",
                "meta": {"n_ctx": 131_072, "n_ctx_train": 262_144, "max_output_tokens": 32_768}
            },
            {"id": "Qwen/Qwen3.8-27B", "object": "model"},
            {"id": "  ", "object": "model"}
        ]});

        let models = parse_models_response("llamacpp", &payload);

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "Qwen/Qwen3.8-27B");
        assert_eq!(models[0].provider, "llamacpp");
        assert_eq!(models[0].capabilities.context_tokens, 131_072);
        assert_eq!(models[0].capabilities.output_tokens, Some(32_768));
        assert_eq!(
            models[0].verification.state,
            crate::model_catalog::VerificationState::Verified
        );
        assert_eq!(models[0].verification.source, "local-runtime");
        assert_eq!(
            crate::model_catalog::model_route(&models[0]),
            "llamacpp/Qwen/Qwen3.8-27B"
        );
    }

    #[test]
    fn parser_reads_direct_openai_compatible_context_metadata() {
        let payload = serde_json::json!({"data": [{
            "id": "lm-studio-model",
            "max_context_length": 65_536
        }]});

        let models = parse_models_response("lmstudio", &payload);

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].capabilities.context_tokens, 65_536);
    }

    #[test]
    fn ollama_running_models_report_the_allocated_context() {
        let payload = serde_json::json!({"models": [{
            "name": "qwen3.6:27b",
            "model": "qwen3.6:27b",
            "context_length": 8_192
        }]});

        assert_eq!(
            ollama_running_contexts_from_payload(&payload),
            HashMap::from([("qwen3.6:27b".to_owned(), 8_192)])
        );

        let mut discovered = parse_models_response(
            "ollama",
            &serde_json::json!({"data": [
                {"id": "qwen3.6:27b"},
                {"id": "not-running"}
            ]}),
        );
        apply_ollama_running_contexts(
            &mut discovered,
            &ollama_running_contexts_from_payload(&payload),
        );
        assert_eq!(discovered[0].capabilities.context_tokens, 8_192);
        assert_eq!(discovered[1].capabilities.context_tokens, 0);
    }

    #[test]
    fn parser_rejects_nonstandard_or_empty_model_lists() {
        for payload in [
            serde_json::json!({}),
            serde_json::json!({"data": null}),
            serde_json::json!({"data": [{"object": "model"}]}),
        ] {
            assert!(parse_models_response("ollama", &payload).is_empty());
        }
    }

    #[test]
    fn parser_enriches_an_exact_local_catalog_model() {
        let payload = serde_json::json!({"data": [{"id": "Qwen3.8-27B"}]});

        let models = parse_models_response("llamacpp", &payload);

        assert_eq!(models.len(), 1);
        assert!(models[0].capabilities.tools);
        assert!(models[0].capabilities.vision);
        assert!(models[0].capabilities.reasoning);
        assert_eq!(models[0].capabilities.context_tokens, 0);
        assert_eq!(models[0].capabilities.output_tokens, None);
        assert_eq!(
            models[0].verification.detail.as_deref(),
            Some("Capabilities from the built-in catalog")
        );
    }

    #[test]
    fn parser_does_not_treat_training_context_as_the_live_server_limit() {
        let payload = serde_json::json!({"data": [{
            "id": "runtime-only-model",
            "meta": {"n_ctx_train": 262_144}
        }]});

        let models = parse_models_response("llamacpp", &payload);

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].capabilities.context_tokens, 0);
    }

    #[test]
    fn parser_enriches_a_unique_bare_catalog_model_without_changing_local_protocol() {
        let payload = serde_json::json!({"data": [{"id": "gpt-5.5"}]});

        let models = parse_models_response("lmstudio", &payload);

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].provider, "lmstudio");
        assert_eq!(
            models[0].capabilities.protocol,
            crate::model_catalog::ModelProtocol::OpenAiChat
        );
        assert_eq!(models[0].capabilities.context_tokens, 0);
        assert_eq!(models[0].capabilities.output_tokens, None);
        assert_eq!(
            models[0].verification.detail.as_deref(),
            Some("Capabilities from the built-in catalog")
        );
    }

    #[test]
    fn parser_clears_inherited_output_without_live_output_metadata() {
        let payload = serde_json::json!({"data": [{
            "id": "gpt-5.5",
            "meta": {"n_ctx": 8_192}
        }]});

        let models = parse_models_response("lmstudio", &payload);

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].capabilities.context_tokens, 8_192);
        assert_eq!(models[0].capabilities.output_tokens, None);
    }

    #[test]
    fn agent_snapshot_retains_only_the_missing_active_routes_live_limits() {
        let payload = serde_json::json!({"data": [{
            "id": "active-runtime-model",
            "meta": {"n_ctx": 8_192}
        }]});
        let active = parse_models_response("llamacpp", &payload)
            .into_iter()
            .next()
            .expect("runtime model");
        let active_key = (active.provider.clone(), active.id.clone());
        let previous = HashMap::from([(active_key.clone(), active)]);

        let retained = replacement_discovered_models(&previous, &[], Some(active_key.clone()));
        let active = retained.get(&active_key).expect("active limits retained");
        assert_eq!(active.capabilities.context_tokens, 8_192);
        assert_eq!(active.verification.state, VerificationState::Unavailable);
        assert!(
            active
                .verification
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("last-known limits"))
        );

        let cleared = replacement_discovered_models(
            &previous,
            &[],
            Some(("ollama".to_owned(), "different-model".to_owned())),
        );
        assert!(cleared.is_empty(), "inactive stale routes must be removed");
    }

    #[test]
    fn parser_enriches_equivalent_mirrored_catalog_models() {
        let payload = serde_json::json!({"data": [{"id": "gemini-2.5-pro"}]});

        let models = parse_models_response("ollama", &payload);

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].provider, "ollama");
        assert_eq!(models[0].capabilities.context_tokens, 0);
        assert_eq!(models[0].capabilities.output_tokens, None);
        assert_eq!(models[0].capabilities.protocol, ModelProtocol::OpenAiChat);
    }

    fn serve_models_after(delay: Duration, body: &'static str) -> String {
        serve_models_after_requests(delay, body, 1)
    }

    fn serve_models_after_requests(delay: Duration, body: &'static str, requests: usize) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for _ in 0..requests {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 2048];
                let _ = stream.read(&mut request);
                std::thread::sleep(delay);
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        });
        format!("http://{address}/v1")
    }

    fn serve_models_after_parallel_probe(
        provider: &'static str,
        body: &'static str,
        arrivals: mpsc::Sender<&'static str>,
        release: Arc<(Mutex<bool>, Condvar)>,
    ) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request);
            arrivals.send(provider).unwrap();

            let (released, wake) = &*release;
            let mut released = released.lock().unwrap();
            while !*released {
                released = wake.wait(released).unwrap();
            }
            drop(released);

            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
        });
        (format!("http://{address}/v1"), server)
    }

    #[test]
    fn discovery_probes_endpoints_concurrently_and_isolates_malformed_results() {
        let fast = serve_models_after(
            Duration::from_millis(5),
            r#"{"data":[{"id":"fast-model"}]}"#,
        );
        let (arrival_tx, arrival_rx) = mpsc::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let (slow_a, slow_a_server) = serve_models_after_parallel_probe(
            "lmstudio",
            r#"{"data":[{"id":"slow-a"}]}"#,
            arrival_tx.clone(),
            Arc::clone(&release),
        );
        let (slow_b, slow_b_server) = serve_models_after_parallel_probe(
            "ollama",
            "not-json",
            arrival_tx,
            Arc::clone(&release),
        );
        let endpoints = vec![
            LocalRuntimeEndpoint {
                provider: "llamacpp",
                display_name: "llama.cpp",
                base_url: fast,
            },
            LocalRuntimeEndpoint {
                provider: "lmstudio",
                display_name: "LM Studio",
                base_url: slow_a,
            },
            LocalRuntimeEndpoint {
                provider: "ollama",
                display_name: "Ollama",
                base_url: slow_b,
            },
        ];

        // The production client times out a local probe quickly. Give this
        // rendezvous fixture enough transport time to distinguish a serial
        // dispatcher from parallel requests without making a timing claim.
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_millis(150))
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let discovery =
            std::thread::spawn(move || discover_endpoints_with_client(&client, &endpoints));
        let mut arrivals = Vec::new();
        for _ in 0..2 {
            if let Ok(provider) = arrival_rx.recv_timeout(Duration::from_secs(2)) {
                arrivals.push(provider);
            }
        }
        {
            let (released, wake) = &*release;
            *released.lock().unwrap() = true;
            wake.notify_all();
        }
        let (models, runtimes) = discovery.join().unwrap();
        assert_eq!(runtimes[2].status, LocalRuntimeStatus::InvalidResponse);
        slow_a_server.join().unwrap();
        slow_b_server.join().unwrap();

        arrivals.sort_unstable();
        assert_eq!(
            arrivals,
            vec!["lmstudio", "ollama"],
            "both slow probes must connect before either fixture is released"
        );
        assert_eq!(
            models
                .iter()
                .map(|model| (model.provider.as_str(), model.id.as_str()))
                .collect::<Vec<_>>(),
            vec![("llamacpp", "fast-model"), ("lmstudio", "slow-a")]
        );
    }

    #[test]
    fn discovery_distinguishes_empty_invalid_and_unreachable_runtimes() {
        let client = discovery_client();
        for (body, expected) in [
            (r#"{"data":[]}"#, LocalRuntimeStatus::Empty),
            (
                r#"{"data":[{"id":"qwen"}]}"#,
                LocalRuntimeStatus::Ready { models: 1 },
            ),
            (
                r#"{"error":"unavailable"}"#,
                LocalRuntimeStatus::InvalidResponse,
            ),
            (r#"{"data":[{}]}"#, LocalRuntimeStatus::InvalidResponse),
            ("not-json", LocalRuntimeStatus::InvalidResponse),
        ] {
            let endpoint = LocalRuntimeEndpoint {
                provider: "llamacpp",
                display_name: "llama.cpp",
                base_url: serve_models_after(Duration::ZERO, body),
            };
            let result = discover_endpoint(&client, &endpoint);
            assert_eq!(result.runtime.status, expected);
            assert_eq!(result.runtime.provider, "llamacpp");
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let mut endpoint = LocalRuntimeEndpoint {
            provider: "lmstudio",
            display_name: "LM Studio",
            base_url,
        };
        assert_eq!(
            discover_endpoint(&client, &endpoint).runtime.status,
            LocalRuntimeStatus::Unreachable
        );
        endpoint.base_url = "invalid URL".to_owned();
        assert_eq!(
            discover_endpoint(&client, &endpoint).runtime.status,
            LocalRuntimeStatus::InvalidEndpoint
        );
    }

    #[test]
    fn discovery_timeout_is_distinct_from_an_empty_model_list() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let (release, gate) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            gate.recv().unwrap();
        });
        let endpoint = LocalRuntimeEndpoint {
            provider: "llamacpp",
            display_name: "llama.cpp",
            base_url,
        };
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let result = discover_endpoint(&client, &endpoint);
        release.send(()).unwrap();
        server.join().unwrap();
        assert_eq!(result.runtime.status, LocalRuntimeStatus::TimedOut);
        assert!(result.models.is_empty());
    }

    #[test]
    fn discovery_reports_http_error_without_server_body_or_credentials() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request);
            stream.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 6\r\nConnection: close\r\n\r\nsecret").unwrap();
        });
        let endpoint = LocalRuntimeEndpoint {
            provider: "llamacpp",
            display_name: "llama.cpp",
            base_url,
        };
        let result = discover_endpoint(&discovery_client(), &endpoint);
        assert_eq!(result.runtime.status, LocalRuntimeStatus::HttpError(401));
        assert!(result.models.is_empty());
        assert!(!format!("{:?}", result.runtime).contains("secret"));
        server.join().unwrap();
    }

    #[test]
    fn discovery_actor_coalesces_refreshes_and_increments_completed_generations() {
        let first =
            serve_models_after_requests(Duration::ZERO, r#"{"data":[{"id":"model-a"}]}"#, 2);
        let endpoints = vec![LocalRuntimeEndpoint {
            provider: "llamacpp",
            display_name: "llama.cpp",
            base_url: first,
        }];
        // This test bounds discovery and coalescing, not platform certificate
        // loading during HTTP client construction. Production still builds its
        // client asynchronously inside the actor.
        let client = discovery_client();
        // Hold actor startup until both refreshes have been submitted, so
        // coalescing does not depend on the HTTP server's scheduling or a sleep.
        let (release, startup) = std::sync::mpsc::channel();
        let (handle, events) =
            spawn_local_model_discovery_with_client_factory(endpoints, move || {
                startup.recv().expect("release discovery actor startup");
                client
            });

        handle.refresh();
        handle.refresh();
        release.send(()).expect("start queued discovery");
        let first = events.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(first.generation, 1);
        assert_eq!(first.models[0].id, "model-a");

        handle.refresh();
        let second = events.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(second.generation, 2);
        assert_eq!(second.models[0].id, "model-a");
        assert!(
            events.try_recv().is_err(),
            "the duplicate refresh was coalesced"
        );
    }
}
