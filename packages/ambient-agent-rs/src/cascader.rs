//! Cascader (Model Routing)
//!
//! Routes tasks to appropriate models based on complexity, type, and cost.
//! Research shows up to 94% cost reduction possible with proper routing.

use crate::types::*;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::LazyLock;

pub const DEFAULT_OPENROUTER_FRONTIER_MODEL: &str = "~anthropic/claude-opus-latest";
/// Direct-Anthropic frontier model, from the central default-model table.
///
/// Was `claude-opus-4-1-20250805`. Anthropic's model list no longer carries
/// Opus 4.1 — its oldest listed Opus is 4.5 — and the id is absent from the
/// bundled catalog, so every ambient task on the direct Anthropic route named
/// a retired snapshot. `frontier_model_is_catalogued` guards this now.
pub const DEFAULT_ANTHROPIC_FRONTIER_MODEL: &str =
    maestro_runtime_contracts::DefaultModel::AnthropicFlagship.id();
pub const DEFAULT_FRONTIER_PROVIDER: &str = "openrouter";

/// Bundled model catalog, read for published per-million-token rates.
///
/// Read directly rather than through `maestro-local-host`: adding a crate
/// dependency here would need matching Bazel wiring, and this only needs two
/// numbers per model.
const CATALOG_JSON: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../local-host-rs/src/model_catalog_data.json"
));

/// Which models an operator permits for each complexity band.
///
/// Economy routing picks the cheapest model *within a band*, never outside it.
/// Price alone is not evidence a model can do the work:
/// `docs/AGENT_PROFILES.md` records that automatic promotion is not wired up
/// and, when it is, must require verified outcomes. Until then a human decides
/// which models are eligible and economics only decides between them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EconomyAllowlist {
    pub light: Vec<String>,
    pub medium: Vec<String>,
    pub heavy: Vec<String>,
}

impl EconomyAllowlist {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.light.is_empty() && self.medium.is_empty() && self.heavy.is_empty()
    }

    fn band(&self, complexity: Complexity) -> &[String] {
        match complexity {
            Complexity::Trivial | Complexity::Simple => &self.light,
            Complexity::Medium => &self.medium,
            Complexity::Complex | Complexity::High => &self.heavy,
        }
    }
}

/// Published rates for `model_id`, scaled to USD per 1,000 tokens.
///
/// The catalog states USD per million tokens; `ModelTier` is per thousand.
fn catalog_rates_per_1k(model_id: &str) -> Option<(f64, f64)> {
    static RATES: LazyLock<HashMap<String, (f64, f64)>> = LazyLock::new(|| {
        let parsed: serde_json::Value = match serde_json::from_str(CATALOG_JSON) {
            Ok(value) => value,
            Err(_) => return HashMap::new(),
        };
        parsed
            .get("models")
            .and_then(serde_json::Value::as_array)
            .map(|models| {
                models
                    .iter()
                    .filter_map(|model| {
                        let id = model.get("id")?.as_str()?.to_owned();
                        let cost = model.get("cost")?;
                        let input = cost.get("input")?.as_f64()?;
                        let output = cost.get("output")?.as_f64()?;
                        Some((id, (input / 1000.0, output / 1000.0)))
                    })
                    .collect()
            })
            .unwrap_or_default()
    });

    let trimmed = model_id.trim();
    RATES.get(trimmed).copied().or_else(|| {
        let bare = trimmed.rsplit('/').next()?;
        RATES.get(bare).copied()
    })
}

/// Build cascade tiers from an operator allowlist, priced from the catalog.
///
/// A model the catalog does not price is skipped and named in the returned
/// warnings: routing on a guessed rate is worse than not routing on it.
#[must_use]
pub fn tiers_from_allowlist(allowlist: &EconomyAllowlist) -> (Vec<ModelTier>, Vec<String>) {
    let mut tiers = Vec::new();
    let mut skipped = Vec::new();
    // The band's ceiling: eligibility is `tier.max_complexity >= task`, so a
    // band must advertise the hardest work it is allowed to take.
    for (band, complexity) in [
        ("light", Complexity::Simple),
        ("medium", Complexity::Medium),
        ("heavy", Complexity::High),
    ] {
        for model in allowlist.band(complexity) {
            let Some((input, output)) = catalog_rates_per_1k(model) else {
                skipped.push(format!(
                    "{model} (band {band}): no published rate in the catalog"
                ));
                continue;
            };
            tiers.push(ModelTier {
                name: format!("{band}:{model}"),
                model: model.clone(),
                cost_per_1k_input: input,
                cost_per_1k_output: output,
                // A band entry is an operator statement that the model may
                // serve that band, so it inherits the band's capabilities.
                capabilities: band_capabilities(complexity),
                max_complexity: complexity,
            });
        }
    }
    (tiers, skipped)
}

fn band_capabilities(complexity: Complexity) -> Vec<String> {
    let light = ["typo-fix", "simple-refactor", "doc-update"];
    let medium = ["feature-impl", "bug-fix", "refactor", "test-write"];
    let heavy = ["architecture", "complex-debug", "security-fix"];
    let selected: Vec<&str> = match complexity {
        Complexity::Trivial | Complexity::Simple => light.to_vec(),
        Complexity::Medium => [light.as_slice(), medium.as_slice()].concat(),
        Complexity::Complex | Complexity::High => {
            [light.as_slice(), medium.as_slice(), heavy.as_slice()].concat()
        }
    };
    selected.into_iter().map(str::to_owned).collect()
}

/// Default model tiers
fn default_tiers() -> Vec<ModelTier> {
    vec![ModelTier {
        name: "frontier".to_string(),
        model: ambient_frontier_model(),
        // OpenRouter currently lists Claude Opus latest/4.7 at $5/M input and
        // $25/M output tokens. Keep these estimates conservative and visible.
        cost_per_1k_input: 0.005,
        cost_per_1k_output: 0.025,
        capabilities: vec![
            "typo-fix".to_string(),
            "simple-refactor".to_string(),
            "doc-update".to_string(),
            "feature-impl".to_string(),
            "bug-fix".to_string(),
            "refactor".to_string(),
            "test-write".to_string(),
            "architecture".to_string(),
            "complex-debug".to_string(),
            "security-fix".to_string(),
        ],
        max_complexity: Complexity::High,
    }]
}

fn ambient_frontier_model() -> String {
    std::env::var("MAESTRO_AMBIENT_FRONTIER_MODEL")
        .or_else(|_| std::env::var("AMBIENT_FRONTIER_MODEL"))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(default_frontier_model_for_configured_provider)
}

fn default_frontier_model_for_configured_provider() -> String {
    match std::env::var("MAESTRO_AMBIENT_LLM_API").ok().as_deref() {
        Some("anthropic") | Some("anthropic-messages") => {
            DEFAULT_ANTHROPIC_FRONTIER_MODEL.to_string()
        }
        _ => DEFAULT_OPENROUTER_FRONTIER_MODEL.to_string(),
    }
}

/// Static task type to capability mapping to avoid recreation on every call
static TASK_TYPE_CAPABILITIES: LazyLock<HashMap<TaskType, Vec<&'static str>>> =
    LazyLock::new(|| {
        let mut map = HashMap::new();
        map.insert(TaskType::Implement, vec!["feature-impl", "architecture"]);
        map.insert(
            TaskType::Fix,
            vec!["bug-fix", "simple-refactor", "complex-debug"],
        );
        map.insert(
            TaskType::Refactor,
            vec!["simple-refactor", "refactor", "architecture"],
        );
        map.insert(TaskType::Test, vec!["test-write", "feature-impl"]);
        map.insert(TaskType::Document, vec!["doc-update"]);
        map.insert(TaskType::Security, vec!["security-fix", "complex-debug"]);
        map
    });

/// Routing result
#[derive(Debug, Clone)]
pub struct RoutingResult {
    pub model: String,
    pub tier: ModelTier,
    pub reason: String,
    pub estimated_cost: f64,
    /// Populated when the shadow-routing evidence gate explicitly selected a
    /// candidate. `None` preserves the legacy routing contract.
    pub shadow_decision_id: Option<String>,
}

/// Statistics about routing
#[derive(Debug, Clone, Default)]
pub struct CascaderStats {
    pub total_routings: u64,
    pub routings_by_tier: HashMap<String, u64>,
    pub total_cost_saved: f64,
    pub average_cost: f64,
}

/// Task context for routing decisions
#[derive(Debug, Clone)]
pub struct TaskContext {
    pub complexity: Complexity,
    pub task_type: TaskType,
    pub estimated_tokens: Option<u64>,
    pub previous_attempts: u32,
}

/// Cascader routes tasks to appropriate models
pub struct Cascader {
    config: CascaderConfig,
    cost_history: Vec<CostTracker>,
    stats: CascaderStats,
}

impl Cascader {
    /// Create a new Cascader
    /// Economy routing over an operator allowlist.
    ///
    /// Returns the cascader plus any allowlist entries that were dropped for
    /// want of a published rate. An empty or entirely unpriced allowlist keeps
    /// the single-frontier default, so opting in cannot silently leave routing
    /// with nothing to choose from.
    #[must_use]
    pub fn with_economy_allowlist(allowlist: &EconomyAllowlist) -> (Self, Vec<String>) {
        let (tiers, skipped) = tiers_from_allowlist(allowlist);
        if tiers.is_empty() {
            return (Self::new(None), skipped);
        }
        (
            Self::new(Some(CascaderConfig {
                tiers,
                fallback_to_higher: true,
                max_retries: 2,
            })),
            skipped,
        )
    }

    pub fn new(config: Option<CascaderConfig>) -> Self {
        let config = config.unwrap_or_else(|| CascaderConfig {
            tiers: default_tiers(),
            fallback_to_higher: true,
            max_retries: 2,
        });

        let mut stats = CascaderStats::default();
        for tier in &config.tiers {
            stats.routings_by_tier.insert(tier.name.clone(), 0);
        }

        Self {
            config,
            cost_history: vec![],
            stats,
        }
    }

    /// Route a task to the most appropriate model
    pub fn route(&mut self, task: &Task, context: &TaskContext) -> RoutingResult {
        self.stats.total_routings += 1;

        let needed = TASK_TYPE_CAPABILITIES
            .get(&context.task_type)
            .cloned()
            .unwrap_or_default();

        // Find eligible tiers
        let mut eligible: Vec<_> = self
            .config
            .tiers
            .iter()
            .filter(|tier| {
                tier.max_complexity >= context.complexity
                    && tier
                        .capabilities
                        .iter()
                        .any(|c| needed.contains(&c.as_str()))
            })
            .collect();

        // Sort by cost (using total_cmp to avoid NaN panics)
        eligible.sort_by(|a, b| {
            let cost_a = a.cost_per_1k_input + a.cost_per_1k_output;
            let cost_b = b.cost_per_1k_input + b.cost_per_1k_output;
            cost_a.total_cmp(&cost_b)
        });

        // Get default tier safely (first tier, or create a default if empty)
        let default_tier = || {
            self.config
                .tiers
                .first()
                .cloned()
                .unwrap_or_else(|| ModelTier {
                    name: "fallback".to_string(),
                    model: ambient_frontier_model(),
                    cost_per_1k_input: 0.005,
                    cost_per_1k_output: 0.025,
                    capabilities: vec![
                        "feature-impl".to_string(),
                        "architecture".to_string(),
                        "security-fix".to_string(),
                    ],
                    max_complexity: Complexity::High,
                })
        };

        let (selected, reason) = if let Some(tier) = eligible.first() {
            (
                (*tier).clone(),
                format!(
                    "Cheapest tier with {:?} capability for {:?} complexity",
                    context.task_type, context.complexity
                ),
            )
        } else if self.config.fallback_to_higher {
            (
                self.config
                    .tiers
                    .last()
                    .cloned()
                    .unwrap_or_else(default_tier),
                "Fallback to advanced tier".to_string(),
            )
        } else {
            (
                default_tier(),
                "Default to configured frontier tier".to_string(),
            )
        };

        // Escalate on retry
        let (selected, reason) = if context.previous_attempts > 0 {
            let current_idx = self
                .config
                .tiers
                .iter()
                .position(|t| t.name == selected.name)
                .unwrap_or(0);
            if current_idx < self.config.tiers.len() - 1 {
                (
                    self.config.tiers[current_idx + 1].clone(),
                    format!(
                        "Escalated after {} failed attempt(s)",
                        context.previous_attempts
                    ),
                )
            } else {
                (selected, reason)
            }
        } else {
            (selected, reason)
        };

        // Update stats
        *self
            .stats
            .routings_by_tier
            .entry(selected.name.clone())
            .or_insert(0) += 1;

        // Estimate cost
        let tokens = context
            .estimated_tokens
            .unwrap_or(self.estimate_tokens(task, context));
        let estimated_cost = (tokens as f64 * selected.cost_per_1k_input) / 1000.0
            + (tokens as f64 * 0.3 * selected.cost_per_1k_output) / 1000.0;

        RoutingResult {
            model: selected.model.clone(),
            tier: selected,
            reason,
            estimated_cost,
            shadow_decision_id: None,
        }
    }

    /// Record an outcome
    pub fn record_outcome(
        &mut self,
        tier: &ModelTier,
        success: bool,
        actual_tokens: u64,
        actual_cost: f64,
    ) {
        self.cost_history.push(CostTracker {
            task_type: "unknown".to_string(),
            model_used: tier.model.clone(),
            tokens: actual_tokens,
            cost_usd: actual_cost,
            success,
            timestamp: Utc::now(),
        });

        // Calculate cost saved vs highest-cost configured tier.
        if let Some(advanced) = self.config.tiers.iter().max_by(|a, b| {
            let cost_a = a.cost_per_1k_input + a.cost_per_1k_output;
            let cost_b = b.cost_per_1k_input + b.cost_per_1k_output;
            cost_a.total_cmp(&cost_b)
        }) {
            if tier.name != advanced.name {
                let advanced_cost = (actual_tokens as f64 * advanced.cost_per_1k_input) / 1000.0
                    + (actual_tokens as f64 * 0.3 * advanced.cost_per_1k_output) / 1000.0;
                self.stats.total_cost_saved += advanced_cost - actual_cost;
            }
        }

        // Update average
        let total: f64 = self.cost_history.iter().map(|h| h.cost_usd).sum();
        self.stats.average_cost = total / self.cost_history.len() as f64;

        // Keep bounded
        if self.cost_history.len() > 1000 {
            self.cost_history = self.cost_history.split_off(500);
        }
    }

    /// Estimate tokens for a task
    fn estimate_tokens(&self, _task: &Task, context: &TaskContext) -> u64 {
        let base = match context.complexity {
            Complexity::Trivial => 500,
            Complexity::Simple => 2000,
            Complexity::Medium => 8000,
            Complexity::Complex => 25000,
            Complexity::High => 50000,
        };

        let multiplier = match context.task_type {
            TaskType::Implement => 1.5,
            TaskType::Fix => 1.0,
            TaskType::Refactor => 1.3,
            TaskType::Test => 1.2,
            TaskType::Document => 0.5,
            TaskType::Security => 1.8,
        };

        (base as f64 * multiplier) as u64
    }

    /// Get statistics
    pub fn get_stats(&self) -> CascaderStats {
        self.stats.clone()
    }

    /// Get cost savings percentage
    pub fn get_cost_savings_percent(&self) -> f64 {
        if self.cost_history.is_empty() {
            return 0.0;
        }

        let advanced = match self.config.tiers.iter().max_by(|a, b| {
            let cost_a = a.cost_per_1k_input + a.cost_per_1k_output;
            let cost_b = b.cost_per_1k_input + b.cost_per_1k_output;
            cost_a.total_cmp(&cost_b)
        }) {
            Some(t) => t,
            None => return 0.0,
        };

        let actual_total: f64 = self.cost_history.iter().map(|h| h.cost_usd).sum();
        let advanced_total: f64 = self
            .cost_history
            .iter()
            .map(|h| {
                (h.tokens as f64 * advanced.cost_per_1k_input) / 1000.0
                    + (h.tokens as f64 * 0.3 * advanced.cost_per_1k_output) / 1000.0
            })
            .sum();

        if advanced_total == 0.0 {
            0.0
        } else {
            ((advanced_total - actual_total) / advanced_total) * 100.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn task(task_type: TaskType) -> Task {
        Task {
            id: "task-1".to_string(),
            task_type,
            prompt: "Do the work".to_string(),
            files: vec![],
            depends_on: vec![],
            priority: 100,
            estimated_tokens: Some(4_000),
        }
    }

    fn allowlist(light: &[&str], medium: &[&str], heavy: &[&str]) -> EconomyAllowlist {
        EconomyAllowlist {
            light: light.iter().map(|m| (*m).to_owned()).collect(),
            medium: medium.iter().map(|m| (*m).to_owned()).collect(),
            heavy: heavy.iter().map(|m| (*m).to_owned()).collect(),
        }
    }

    #[test]
    fn economy_tiers_are_priced_from_the_catalog() {
        let (tiers, skipped) = tiers_from_allowlist(&allowlist(
            &["claude-haiku-4-5"],
            &["claude-sonnet-5"],
            &["claude-opus-5-5"],
        ));
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(tiers.len(), 3);

        // The catalog states USD per million tokens; ModelTier is per 1,000.
        // Opus 5.5 is $4/$20 per million.
        let opus = tiers
            .iter()
            .find(|tier| tier.model == "claude-opus-5-5")
            .expect("opus tier");
        assert!((opus.cost_per_1k_input - 0.004).abs() < 1e-9);
        assert!((opus.cost_per_1k_output - 0.020).abs() < 1e-9);

        // Haiku 4.5 is $1/$5 per million, so it must price below Opus.
        let haiku = tiers
            .iter()
            .find(|tier| tier.model == "claude-haiku-4-5")
            .expect("haiku tier");
        assert!(haiku.cost_per_1k_input < opus.cost_per_1k_input);
    }

    #[test]
    fn economy_routing_picks_the_cheapest_model_allowed_for_the_band() {
        // Both models are allowed for light work; price decides between them.
        let (mut cascader, skipped) = Cascader::with_economy_allowlist(&allowlist(
            &["claude-opus-5-5", "claude-haiku-4-5"],
            &[],
            &[],
        ));
        assert!(skipped.is_empty(), "{skipped:?}");

        let routing = cascader.route(
            &task(TaskType::Document),
            &TaskContext {
                complexity: Complexity::Trivial,
                task_type: TaskType::Document,
                estimated_tokens: Some(2_000),
                previous_attempts: 0,
            },
        );
        assert_eq!(routing.tier.model, "claude-haiku-4-5");
    }

    #[test]
    fn allowlisted_models_the_catalog_cannot_price_are_reported_not_guessed() {
        let (tiers, skipped) =
            tiers_from_allowlist(&allowlist(&["not-a-real-model-xyz"], &[], &[]));
        assert!(tiers.is_empty());
        assert_eq!(skipped.len(), 1);
        assert!(
            skipped[0].contains("not-a-real-model-xyz"),
            "the skipped entry must name the model: {skipped:?}"
        );
    }

    #[test]
    fn opting_in_with_nothing_priceable_keeps_the_frontier_default() {
        let _guard = ENV_LOCK.lock().unwrap();
        // An allowlist that yields no priced tier must not leave routing with
        // an empty candidate set.
        let (cascader, skipped) =
            Cascader::with_economy_allowlist(&allowlist(&["not-a-real-model-xyz"], &[], &[]));
        assert_eq!(skipped.len(), 1);
        assert_eq!(cascader.config.tiers.len(), default_tiers().len());

        let (untouched, skipped) = Cascader::with_economy_allowlist(&EconomyAllowlist::default());
        assert!(skipped.is_empty());
        assert_eq!(untouched.config.tiers.len(), default_tiers().len());
    }

    #[test]
    fn frontier_model_is_catalogued() {
        // The direct-Anthropic default was claude-opus-4-1-20250805, a
        // retired snapshot absent from the catalog, and nothing checked it.
        // This is the same failure the Google subagent tiers had in #10149.
        // Inlined rather than a module constant so this does not collide
        // with the economy-allowlist work, which introduces one.
        let catalog: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../local-host-rs/src/model_catalog_data.json"
        )))
        .expect("bundled catalog parses");
        let ids: std::collections::HashSet<&str> = catalog["models"]
            .as_array()
            .expect("models array")
            .iter()
            .filter_map(|model| model["id"].as_str())
            .collect();
        assert!(
            ids.contains(DEFAULT_ANTHROPIC_FRONTIER_MODEL),
            "{DEFAULT_ANTHROPIC_FRONTIER_MODEL} is not in the bundled catalog"
        );

        // The OpenRouter default is deliberately a floating alias
        // (`~anthropic/claude-opus-latest`). OpenRouter resolves it to the
        // current Opus, so it is self-updating and is not a catalog row; it is
        // asserted for shape rather than membership.
        assert!(DEFAULT_OPENROUTER_FRONTIER_MODEL.starts_with("~anthropic/"));
    }

    #[test]
    fn default_routes_ambient_work_to_openrouter_opus_latest() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("MAESTRO_AMBIENT_FRONTIER_MODEL");
        std::env::remove_var("AMBIENT_FRONTIER_MODEL");
        std::env::remove_var("MAESTRO_AMBIENT_LLM_API");
        let mut cascader = Cascader::new(None);

        let documentation = cascader.route(
            &task(TaskType::Document),
            &TaskContext {
                complexity: Complexity::Trivial,
                task_type: TaskType::Document,
                estimated_tokens: Some(2_000),
                previous_attempts: 0,
            },
        );
        let security = cascader.route(
            &task(TaskType::Security),
            &TaskContext {
                complexity: Complexity::High,
                task_type: TaskType::Security,
                estimated_tokens: Some(20_000),
                previous_attempts: 0,
            },
        );

        assert_eq!(documentation.tier.name, "frontier");
        assert_eq!(documentation.model, DEFAULT_OPENROUTER_FRONTIER_MODEL);
        assert_eq!(security.tier.name, "frontier");
        assert_eq!(security.model, DEFAULT_OPENROUTER_FRONTIER_MODEL);
    }

    #[test]
    fn anthropic_api_default_uses_native_anthropic_model_id() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("MAESTRO_AMBIENT_FRONTIER_MODEL");
        std::env::remove_var("AMBIENT_FRONTIER_MODEL");
        std::env::set_var("MAESTRO_AMBIENT_LLM_API", "anthropic");

        let mut cascader = Cascader::new(None);
        let routed = cascader.route(
            &task(TaskType::Implement),
            &TaskContext {
                complexity: Complexity::High,
                task_type: TaskType::Implement,
                estimated_tokens: Some(4_000),
                previous_attempts: 0,
            },
        );

        assert_eq!(routed.tier.name, "frontier");
        assert_eq!(routed.model, DEFAULT_ANTHROPIC_FRONTIER_MODEL);
        std::env::remove_var("MAESTRO_AMBIENT_LLM_API");
    }

    #[test]
    fn non_fallback_path_uses_configured_default_tier() {
        let mut cascader = Cascader::new(Some(CascaderConfig {
            tiers: vec![ModelTier {
                name: "frontier".to_string(),
                model: "test-frontier".to_string(),
                cost_per_1k_input: 0.001,
                cost_per_1k_output: 0.002,
                capabilities: vec!["doc-update".to_string()],
                max_complexity: Complexity::Trivial,
            }],
            fallback_to_higher: false,
            max_retries: 0,
        }));

        let routed = cascader.route(
            &task(TaskType::Security),
            &TaskContext {
                complexity: Complexity::High,
                task_type: TaskType::Security,
                estimated_tokens: Some(4_000),
                previous_attempts: 0,
            },
        );

        assert_eq!(routed.tier.name, "frontier");
        assert_eq!(routed.model, "test-frontier");
        assert_eq!(routed.reason, "Default to configured frontier tier");
    }
}
