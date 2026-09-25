//! Accurate token counter (OpenAI clade) with bytes/4 fallback.
//!
//! Mirrors `src/agent/token-counter.ts`. The native TUI keeps the fast,
//! offline-capable `token_estimation::estimate_tokens` (bytes/4) as its default
//! and exposes [`count_tokens`] for callers that know the model and need
//! real accuracy (e.g. compaction thresholds, context-overflow preflight).
//!
//! The BPE instances are constructed lazily and cached for the process
//! lifetime. If a tokenizer cannot be loaded (e.g. missing data file), the
//! call falls back to the bytes/4 heuristic rather than panicking.

use std::sync::OnceLock;

use tiktoken_rs::CoreBPE;

use crate::token_estimation;

/// BPE encoding families we can count accurately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenEncoding {
    O200k,
    Cl100k,
}

/// Provenance attached to a token count shown to users or used for budgeting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CountConfidence {
    /// Counted by a tokenizer bundled for the selected model family.
    Measured,
    /// Estimated with the shared bytes-per-token heuristic.
    Estimated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenCount {
    pub tokens: u64,
    pub confidence: CountConfidence,
}

/// Inputs whose stability determines whether a provider prompt cache can be reused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheIdentity<'a> {
    pub model: &'a str,
    pub system_prompt_sha256: &'a str,
    pub thinking: &'a str,
    pub skills_sha256: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheReuse {
    Reusable,
    /// Prepared identity matches and no sourced retention hint applies.
    CompatiblePrefix,
    /// Compatible, and the gap is inside a sourced retention hint. Not an observed hit.
    PredictedReuse,
    /// Provider-reported cache read. Not a prediction.
    ObservedRead,
    /// Provider-reported cache write. Lifetime starts at the provider event.
    ObservedWrite,
    ModelChanged,
    SystemPromptChanged,
    ThinkingChanged,
    SkillsChanged,
    ToolsChanged,
    /// The gap is past a sourced retention hint. Not proof the entry is gone.
    LikelyExpired,
    Unsupported,
    /// No sourced lifetime or read rate. Callers must not assume five minutes or 0.1×.
    Unknown,
}

impl CacheReuse {
    pub fn explanation(self) -> &'static str {
        match self {
            Self::Reusable | Self::CompatiblePrefix => {
                "Compatible prefix: model, instructions, thinking, and tools match. This is not an observed read or a predicted hit."
            }
            Self::PredictedReuse => {
                "Predicted reuse: the prefix is compatible and the gap is inside the provider retention hint. A preparation timestamp is not a provider cache-creation time."
            }
            Self::ObservedRead => {
                "Observed read: the provider reported cache-read tokens for this prefix."
            }
            Self::ObservedWrite => {
                "Observed write: the provider reported cache-write tokens. The lifetime starts at that event, not at preparation or stream completion."
            }
            Self::ModelChanged => "Model changed.",
            Self::SystemPromptChanged => "System prompt or instructions changed.",
            Self::ThinkingChanged => "Thinking settings changed.",
            Self::SkillsChanged => "Skills changed.",
            Self::ToolsChanged => "Tool schemas changed.",
            Self::LikelyExpired => {
                "Expired hint: the gap since preparation, or since the observed cache event when one was recorded, is past the provider retention hint. This does not confirm the entry is gone, and requesting a quote does not extend it."
            }
            Self::Unsupported => "Unsupported: this route has no prompt-cache markers.",
            Self::Unknown => "Unknown cache behavior. No lifetime or read rate is assumed.",
        }
    }
}

/// Provider-reported cache usage. `None` is missing, not zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheObservation {
    pub read_tokens: Option<u64>,
    pub write_tokens: Option<u64>,
    /// Provider event time. Not preparation time and not stream completion.
    /// For Anthropic this is request start: the ephemeral TTL clock starts there.
    /// Production audit still calls [`RequestCacheSnapshot::compare`], which passes
    /// no observation. Wiring reported usage into this field is deferred.
    pub event_seconds: Option<u64>,
}

/// One earlier route's prepared topology. Digests only; no prompt text.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PriorRouteRecord {
    pub topology: maestro_ai::cache_topology::CacheTopology,
    #[serde(default)]
    pub tools_system_materialized: bool,
}

/// Diagnostic request identity, persisted by the existing session owner.
/// This predicts reuse; it never substitutes for provider-reported cache usage.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RequestCacheSnapshot {
    pub model: String,
    pub system_sha256: String,
    pub thinking_sha256: String,
    pub tools_sha256: String,
    pub prepared_at_seconds: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_topology: Option<maestro_ai::cache_topology::CacheTopology>,
    /// Routed provider id. Absent on legacy snapshots; inference then uses the model id only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The tools/system breakpoint was placed on this request, not merely eligible.
    #[serde(default)]
    pub tools_system_materialized: bool,
    /// Other routes' prepared topologies for this session. Empty on legacy snapshots.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prior_routes: Vec<PriorRouteRecord>,
    /// Capability record that chose the markers. Absent on legacy snapshots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundary_record_id: Option<String>,
    /// Canonical history indexes the plan marked. Empty when the plan was not finalized.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history_boundaries: Vec<usize>,
}

impl RequestCacheSnapshot {
    pub fn from_request(config: &maestro_ai::RequestConfig, prepared_at_seconds: u64) -> Self {
        use sha2::{Digest, Sha256};
        let hash = |bytes: &[u8]| format!("{:x}", Sha256::digest(bytes));
        Self {
            model: config.model.clone(),
            system_sha256: hash(config.system.as_deref().unwrap_or_default().as_bytes()),
            thinking_sha256: hash(
                serde_json::to_string(&config.thinking)
                    .expect("thinking settings serialize")
                    .as_bytes(),
            ),
            tools_sha256: hash(
                serde_json::to_string(config.tools.as_ref())
                    .expect("tool schemas serialize")
                    .as_bytes(),
            ),
            prepared_at_seconds,
            cache_topology: config
                .cache_topology
                .as_ref()
                .map(|prepared| prepared.topology().clone()),
            provider: None,
            tools_system_materialized: config.cache_topology.as_ref().is_some_and(|prepared| {
                let plan = prepared.boundary();
                plan.is_final() && (plan.mark_system || plan.mark_tools)
            }),
            prior_routes: Vec::new(),
            boundary_record_id: config.cache_topology.as_ref().and_then(|prepared| {
                prepared
                    .boundary()
                    .is_final()
                    .then(|| prepared.boundary().record_id.to_string())
            }),
            history_boundaries: config
                .cache_topology
                .as_ref()
                .filter(|prepared| prepared.boundary().is_final())
                .map(|prepared| prepared.boundary().history_indexes.clone())
                .unwrap_or_default(),
        }
    }

    /// Keep other routes when the model changes. Same-model topology stays on `cache_topology`.
    /// At most two prior routes: this is the boundary handoff, not a placement registry.
    #[must_use]
    pub fn rolled_prior_routes(&self, next_model_digest: &str) -> Vec<PriorRouteRecord> {
        let mut kept = Vec::new();
        if let Some(topology) = &self.cache_topology {
            if topology.shape.model != next_model_digest {
                kept.push(PriorRouteRecord {
                    topology: topology.clone(),
                    tools_system_materialized: self.tools_system_materialized,
                });
            }
        }
        for prior in &self.prior_routes {
            if prior.topology.shape.model != next_model_digest
                && kept
                    .iter()
                    .all(|existing| existing.topology.shape.model != prior.topology.shape.model)
            {
                kept.push(prior.clone());
            }
        }
        kept.truncate(2);
        kept
    }

    pub fn compare(&self, previous: &Self) -> CacheReuse {
        self.compare_observed(previous, None)
    }

    /// Diagnostic only. Does not store a new expiry and does not treat preparation as cache creation.
    pub fn compare_observed(
        &self,
        previous: &Self,
        observed: Option<&CacheObservation>,
    ) -> CacheReuse {
        if self.model != previous.model {
            return CacheReuse::ModelChanged;
        }
        if self.system_sha256 != previous.system_sha256 {
            return CacheReuse::SystemPromptChanged;
        }
        if self.thinking_sha256 != previous.thinking_sha256 {
            return CacheReuse::ThinkingChanged;
        }
        if self.tools_sha256 != previous.tools_sha256 {
            return CacheReuse::ToolsChanged;
        }
        let provider = self.provider.as_deref().or(previous.provider.as_deref());
        let capability = maestro_ai::cache_capability(provider, &self.model);
        if capability.behavior == maestro_ai::CacheBehavior::Unsupported {
            return CacheReuse::Unsupported;
        }
        if let Some(observed) = observed {
            if observed.read_tokens.is_some_and(|tokens| tokens > 0) {
                return CacheReuse::ObservedRead;
            }
            if observed.write_tokens.is_some_and(|tokens| tokens > 0) {
                return CacheReuse::ObservedWrite;
            }
        }
        let Some(hint) = capability.retention.hint_seconds() else {
            return if capability.behavior == maestro_ai::CacheBehavior::Unknown {
                CacheReuse::Unknown
            } else {
                CacheReuse::CompatiblePrefix
            };
        };
        let anchor = observed
            .and_then(|observation| observation.event_seconds)
            .unwrap_or(previous.prepared_at_seconds);
        let idle = self.prepared_at_seconds.saturating_sub(anchor);
        if idle >= hint {
            CacheReuse::LikelyExpired
        } else {
            CacheReuse::PredictedReuse
        }
    }
}

/// Explain cache reuse before resuming a session. Content is compared only by
/// caller-provided hashes, so prompts and skill text never enter telemetry.
#[must_use]
pub fn cache_reuse(
    previous: &CacheIdentity<'_>,
    current: &CacheIdentity<'_>,
    idle_seconds: u64,
    expiry_hint_seconds: u64,
) -> CacheReuse {
    if previous.model != current.model {
        CacheReuse::ModelChanged
    } else if previous.system_prompt_sha256 != current.system_prompt_sha256 {
        CacheReuse::SystemPromptChanged
    } else if previous.thinking != current.thinking {
        CacheReuse::ThinkingChanged
    } else if previous.skills_sha256 != current.skills_sha256 {
        CacheReuse::SkillsChanged
    } else if idle_seconds >= expiry_hint_seconds {
        CacheReuse::LikelyExpired
    } else {
        CacheReuse::Reusable
    }
}

fn o200k() -> Option<&'static CoreBPE> {
    static BPE: OnceLock<Option<CoreBPE>> = OnceLock::new();
    BPE.get_or_init(|| tiktoken_rs::o200k_base().ok()).as_ref()
}

fn cl100k() -> Option<&'static CoreBPE> {
    static BPE: OnceLock<Option<CoreBPE>> = OnceLock::new();
    BPE.get_or_init(|| tiktoken_rs::cl100k_base().ok()).as_ref()
}

/// Resolve the tokenizer encoding for a model id, or `None` when no accurate
/// tokenizer is bundled (Anthropic, Google, unknown).
#[must_use]
pub fn encoding_for_model(model: &str) -> Option<TokenEncoding> {
    let m = model.to_lowercase();
    // GPT-4o family and o-series / GPT-5 use o200k_base.
    if m.contains("gpt-4o")
        || m.contains("gpt-5")
        || m.contains("o1")
        || m.contains("o3")
        || m.contains("o4")
    {
        return Some(TokenEncoding::O200k);
    }
    // GPT-4 / GPT-3.5 / embeddings use cl100k_base.
    if m.contains("gpt-4") || m.contains("gpt-3.5") || m.contains("text-embedding") {
        return Some(TokenEncoding::Cl100k);
    }
    None
}

/// Count tokens accurately for OpenAI-clade models; fall back to the shared
/// bytes/4 heuristic otherwise (or when `model` is `None`/unknown, or when the
/// BPE data could not be loaded).
#[must_use]
pub fn count_tokens(text: &str, model: Option<&str>) -> u64 {
    count_tokens_with_metadata(text, model).tokens
}

/// Count tokens and retain whether the value was measured or estimated.
#[must_use]
pub fn count_tokens_with_metadata(text: &str, model: Option<&str>) -> TokenCount {
    let Some(encoding) = model.and_then(encoding_for_model) else {
        return TokenCount {
            tokens: token_estimation::estimate_tokens(text),
            confidence: CountConfidence::Estimated,
        };
    };
    let bpe = match encoding {
        TokenEncoding::O200k => o200k(),
        TokenEncoding::Cl100k => cl100k(),
    };
    match bpe {
        Some(bpe) => TokenCount {
            tokens: bpe.encode_ordinary(text).len() as u64,
            confidence: CountConfidence::Measured,
        },
        None => TokenCount {
            tokens: token_estimation::estimate_tokens(text),
            confidence: CountConfidence::Estimated,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_cache_snapshot_roundtrip_explains_real_request_changes() {
        let mut config = maestro_ai::RequestConfig::default();
        let initial = RequestCacheSnapshot::from_request(&config, 10);
        let restored: RequestCacheSnapshot =
            serde_json::from_str(&serde_json::to_string(&initial).unwrap()).unwrap();
        assert_eq!(
            RequestCacheSnapshot::from_request(&config, 11).compare(&restored),
            CacheReuse::PredictedReuse
        );
        assert!(
            CacheReuse::PredictedReuse
                .explanation()
                .contains("not a provider cache-creation time")
        );
        assert_eq!(
            RequestCacheSnapshot::from_request(&config, 311).compare(&restored),
            CacheReuse::LikelyExpired
        );
        config.thinking = Some(maestro_ai::ThinkingConfig::enabled(2048));
        assert_eq!(
            RequestCacheSnapshot::from_request(&config, 11).compare(&restored),
            CacheReuse::ThinkingChanged
        );
        config.thinking = None;
        config.tools = std::sync::Arc::new(vec![maestro_ai::Tool::new("read", "Read a file")]);
        assert_eq!(
            RequestCacheSnapshot::from_request(&config, 11).compare(&restored),
            CacheReuse::ToolsChanged
        );
        config.system = Some("new instruction".into());
        assert_eq!(
            RequestCacheSnapshot::from_request(&config, 11).compare(&restored),
            CacheReuse::SystemPromptChanged
        );
        config.model = "changed-model".into();
        assert_eq!(
            RequestCacheSnapshot::from_request(&config, 11).compare(&restored),
            CacheReuse::ModelChanged
        );
        assert!(
            !serde_json::to_string(&RequestCacheSnapshot::from_request(&config, 11))
                .unwrap()
                .contains("new instruction")
        );
    }

    #[test]
    fn o200k_counts_known_values() {
        assert_eq!(count_tokens("Hello, world!", Some("gpt-4o")), 4);
        // bytes/4 would say ceil(35/4)=9; the real o200k count is 13.
        assert_eq!(
            count_tokens("function add(a, b) { return a + b; }", Some("gpt-4o")),
            13
        );
    }

    #[test]
    fn cl100k_counts_known_values() {
        assert_eq!(count_tokens("Hello, world!", Some("gpt-4")), 4);
    }

    #[test]
    fn encoding_for_model_maps_clades() {
        assert_eq!(encoding_for_model("gpt-4o"), Some(TokenEncoding::O200k));
        assert_eq!(encoding_for_model("o3-mini"), Some(TokenEncoding::O200k));
        assert_eq!(
            encoding_for_model("gpt-4-turbo"),
            Some(TokenEncoding::Cl100k)
        );
        assert_eq!(encoding_for_model("claude-sonnet-4-5"), None);
        assert_eq!(encoding_for_model("gemini-2.5-pro"), None);
    }

    #[test]
    fn falls_back_to_heuristic_for_non_openai() {
        let text = "Hello, world!";
        assert_eq!(
            count_tokens(text, Some("claude-sonnet-4-5")),
            token_estimation::estimate_tokens(text)
        );
        assert_eq!(
            count_tokens(text, None),
            token_estimation::estimate_tokens(text)
        );
    }

    #[test]
    fn count_reports_measurement_provenance() {
        assert_eq!(
            count_tokens_with_metadata("hello", Some("gpt-5")).confidence,
            CountConfidence::Measured
        );
        assert_eq!(
            count_tokens_with_metadata("hello", Some("claude-sonnet-4-5")).confidence,
            CountConfidence::Estimated
        );
    }

    #[test]
    fn cache_reuse_explains_invalidation_and_expiry() {
        let original = CacheIdentity {
            model: "gpt-5",
            system_prompt_sha256: "prompt-a",
            thinking: "medium",
            skills_sha256: "skills-a",
        };
        let changed_model = CacheIdentity {
            model: "gpt-5.1",
            ..original.clone()
        };
        assert_eq!(
            cache_reuse(&original, &changed_model, 1, 300),
            CacheReuse::ModelChanged
        );
        assert_eq!(
            cache_reuse(&original, &original, 301, 300),
            CacheReuse::LikelyExpired
        );
    }

    #[test]
    fn lifetime_hints_are_provider_specific_and_preparation_is_not_creation() {
        let mut config = maestro_ai::RequestConfig {
            model: "local-gguf".into(),
            ..Default::default()
        };
        let previous = RequestCacheSnapshot::from_request(&config, 10);
        let later = RequestCacheSnapshot::from_request(&config, 10_000);
        assert_eq!(later.compare(&previous), CacheReuse::Unknown);
        assert!(
            CacheReuse::Unknown
                .explanation()
                .contains("No lifetime or read rate")
        );
        assert_eq!(
            maestro_ai::cache_capability(None, "local-gguf").read_rate_millis,
            None
        );
        assert_eq!(
            maestro_ai::cache_capability(None, "local-gguf")
                .retention
                .hint_seconds(),
            None
        );

        config.model = "gpt-5.6".into();
        let mut prepared = RequestCacheSnapshot::from_request(&config, 0);
        prepared.provider = Some("openai".into());
        let mut within = RequestCacheSnapshot::from_request(&config, 1_000);
        within.provider = Some("openai".into());
        assert_eq!(within.compare(&prepared), CacheReuse::PredictedReuse);
        let mut past_hint = RequestCacheSnapshot::from_request(&config, 1_800);
        past_hint.provider = Some("openai".into());
        assert_eq!(past_hint.compare(&prepared), CacheReuse::LikelyExpired);
        // The same preparation gap is not expired when the provider event is recent.
        // Repeating the comparison does not move that event.
        let observed = CacheObservation {
            read_tokens: None,
            write_tokens: Some(100_000),
            event_seconds: Some(1_700),
        };
        assert_eq!(
            past_hint.compare_observed(&prepared, Some(&observed)),
            CacheReuse::ObservedWrite
        );
        assert_eq!(
            past_hint.compare_observed(&prepared, Some(&observed)),
            CacheReuse::ObservedWrite
        );
        let read = CacheObservation {
            read_tokens: Some(100_000),
            write_tokens: Some(0),
            event_seconds: Some(1_790),
        };
        assert_eq!(
            past_hint.compare_observed(&prepared, Some(&read)),
            CacheReuse::ObservedRead
        );
        let explicit_zero = CacheObservation {
            read_tokens: Some(0),
            write_tokens: Some(0),
            event_seconds: Some(1_790),
        };
        assert_eq!(
            past_hint.compare_observed(&prepared, Some(&explicit_zero)),
            CacheReuse::PredictedReuse
        );
        let missing = CacheObservation {
            read_tokens: None,
            write_tokens: None,
            event_seconds: Some(0),
        };
        assert_eq!(
            past_hint.compare_observed(&prepared, Some(&missing)),
            CacheReuse::LikelyExpired
        );

        config.model = "gpt-5.4".into();
        let mut earlier = RequestCacheSnapshot::from_request(&config, 0);
        earlier.provider = Some("openai".into());
        let mut much_later = RequestCacheSnapshot::from_request(&config, 86_400);
        much_later.provider = Some("openai".into());
        assert_eq!(much_later.compare(&earlier), CacheReuse::CompatiblePrefix);
        assert_eq!(
            much_later.compare(&earlier),
            much_later.compare(&earlier),
            "a repeated diagnostic must not extend the hint"
        );
    }

    #[test]
    fn legacy_cache_snapshot_without_capability_fields_stays_readable() {
        let legacy = r#"{"model":"claude-sonnet-4-5","system_sha256":"abc","thinking_sha256":"def","tools_sha256":"ghi","prepared_at_seconds":10}"#;
        let restored: RequestCacheSnapshot = serde_json::from_str(legacy).unwrap();
        assert!(restored.provider.is_none());
        assert!(!restored.tools_system_materialized);
        assert!(restored.prior_routes.is_empty());
        assert!(restored.boundary_record_id.is_none());
        assert!(restored.history_boundaries.is_empty());
        assert!(restored.cache_topology.is_none());
        let mut config = maestro_ai::RequestConfig {
            model: "gpt-5.6".into(),
            explicit_cache_boundaries: true,
            ..Default::default()
        };
        let messages = vec![maestro_ai::Message {
            role: maestro_ai::Role::User,
            content: maestro_ai::MessageContent::text("stable"),
        }];
        let mut prepared = maestro_ai::cache_topology::PreparedPrompt::prepare(
            &messages,
            &config,
            "session".into(),
            None,
        )
        .unwrap()
        .with_volatile_tail(Some("clock".into()));
        prepared
            .finalize_boundary(Some("openai"), "gpt-5.6", true, false, false, 1)
            .unwrap();
        config.cache_topology = Some(prepared);
        let snapshot = RequestCacheSnapshot::from_request(&config, 10);
        let round_trip: RequestCacheSnapshot =
            serde_json::from_str(&serde_json::to_string(&snapshot).unwrap()).unwrap();
        assert_eq!(
            round_trip.boundary_record_id.as_deref(),
            Some("openai-responses-gpt-5.6-explicit.2026-09-24")
        );
        assert_eq!(round_trip.history_boundaries, vec![0]);
        let topology = r#"{"version":1,"generation":1,"transition":"initial","shape":{"namespace":"n","model":"m","instructions":"i","tools":"t","thinking":"h","cache_policy":"c","history":[]}}"#;
        let parsed: maestro_ai::cache_topology::CacheTopology =
            serde_json::from_str(topology).unwrap();
        assert_eq!(parsed.generation, 1);
        assert_eq!(
            parsed.transition,
            maestro_ai::cache_topology::CacheTransition::Initial
        );
    }
}
