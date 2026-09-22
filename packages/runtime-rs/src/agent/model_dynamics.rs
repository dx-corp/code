//! Transport-neutral task model preferences used by the native loop.
//!
//! The application may use a richer model catalog when it composes a host, but
//! the loop only needs the persisted choice shape and the local effort state.
//! Keeping these values here avoids making the runtime depend on the TUI
//! session or catalog modules.

use serde::{Deserialize, Serialize};

pub use maestro_runtime_contracts::ThinkingLevel;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskDifficulty {
    Light,
    #[default]
    Medium,
    Heavy,
}

impl TaskDifficulty {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "light" => Ok(Self::Light),
            "medium" => Ok(Self::Medium),
            "heavy" => Ok(Self::Heavy),
            _ => Err("difficulty must be light, medium, or heavy".to_owned()),
        }
    }

    #[must_use]
    pub fn cap(self, inherited: ThinkingLevel) -> ThinkingLevel {
        let limit = match self {
            Self::Light => ThinkingLevel::Low,
            Self::Medium => ThinkingLevel::Medium,
            Self::Heavy => ThinkingLevel::High,
        };
        if inherited.to_config().1 > limit.to_config().1 {
            limit
        } else {
            inherited
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelChoice {
    pub model: String,
    #[serde(default)]
    pub thinking: ThinkingLevel,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelDynamicsConfig {
    /// Ordered interactive model scope with a preferred effort for each route.
    pub cycle: Vec<ModelChoice>,
    pub light: Option<ModelChoice>,
    pub medium: Option<ModelChoice>,
    pub heavy: Option<ModelChoice>,
    pub boost: Option<ModelChoice>,
    /// Optional tool-free summarizer; the active conversation model is unchanged.
    pub summary_model: Option<String>,
    pub fallbacks: Vec<ModelChoice>,
    pub auto_boost: bool,
}

impl ModelDynamicsConfig {
    pub fn effort_for_model(&self, model: &str) -> Option<ThinkingLevel> {
        self.cycle
            .iter()
            .find(|choice| same_model_route(&choice.model, model))
            .map(|choice| choice.thinking)
    }

    #[must_use]
    pub fn choice(&self, difficulty: TaskDifficulty) -> Option<&ModelChoice> {
        match difficulty {
            TaskDifficulty::Light => self.light.as_ref(),
            TaskDifficulty::Medium => self.medium.as_ref(),
            TaskDifficulty::Heavy => self.heavy.as_ref(),
        }
    }

    #[must_use]
    pub fn resolve_child(
        &self,
        difficulty: TaskDifficulty,
        model: Option<&str>,
        thinking: Option<ThinkingLevel>,
        parent: &ModelChoice,
    ) -> ModelChoice {
        let tier = self.choice(difficulty);
        ModelChoice {
            model: model
                .map(str::to_owned)
                .or_else(|| tier.map(|choice| choice.model.clone()))
                .unwrap_or_else(|| parent.model.clone()),
            thinking: thinking.unwrap_or_else(|| {
                if let Some(model) = model {
                    if model == parent.model {
                        difficulty.cap(parent.thinking)
                    } else {
                        ThinkingLevel::Medium
                    }
                } else {
                    tier.map(|choice| choice.thinking)
                        .unwrap_or_else(|| difficulty.cap(parent.thinking))
                }
            }),
        }
    }
}

#[must_use]
pub fn thinking_level(enabled: bool, budget: u32) -> ThinkingLevel {
    if !enabled {
        return ThinkingLevel::Off;
    }
    match budget {
        0..=1024 => ThinkingLevel::Minimal,
        1025..=4096 => ThinkingLevel::Low,
        4097..=10_000 => ThinkingLevel::Medium,
        10_001..=20_000 => ThinkingLevel::High,
        20_001..=32_000 => ThinkingLevel::XHigh,
        _ => ThinkingLevel::Max,
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoostStatus {
    #[default]
    Idle,
    Suggested,
    Pending,
    Active,
}

/// Mutable state shared by the native loop and its model-dynamics extension.
#[derive(Debug, Default)]
pub(crate) struct DynamicsState {
    pub status: BoostStatus,
    pub requested: bool,
    pub used: bool,
    pub available: bool,
    pub fallback_models: std::collections::HashSet<String>,
    pub fallback_attempts: usize,
}

/// Choose a configured boost without consulting a provider catalog.  Host
/// compatibility and authorization checks run before the choice is applied.
#[must_use]
pub fn boost_choice(current: &ModelChoice, config: &ModelDynamicsConfig) -> Option<ModelChoice> {
    let next = if let Some(choice) = &config.boost {
        choice.clone()
    } else {
        let thinking = match current.thinking {
            ThinkingLevel::Off
            | ThinkingLevel::Minimal
            | ThinkingLevel::Low
            | ThinkingLevel::Medium => ThinkingLevel::High,
            ThinkingLevel::High => ThinkingLevel::XHigh,
            ThinkingLevel::XHigh => ThinkingLevel::Max,
            ThinkingLevel::Max => return None,
        };
        ModelChoice {
            model: current.model.clone(),
            thinking,
        }
    };
    if next.model == current.model && next.thinking.to_config().1 <= current.thinking.to_config().1
    {
        None
    } else {
        Some(next)
    }
}

#[must_use]
pub fn normalize_thinking(_model: &str, requested: ThinkingLevel) -> ThinkingLevel {
    requested
}

#[must_use]
pub fn next_thinking_level(model: &str, current: ThinkingLevel) -> ThinkingLevel {
    let current = normalize_thinking(model, current);
    let levels = ThinkingLevel::ALL;
    let index = levels
        .iter()
        .position(|level| *level == current)
        .unwrap_or(0);
    levels[(index + 1) % levels.len()]
}

/// Compare routes without confusing models served by different providers.
pub fn same_model_route(left: &str, right: &str) -> bool {
    let identity = |model: &str| {
        crate::ai::ProviderRegistry::resolve_descriptor(model)
            .ok()
            .map(|provider| format!("{}/{}", provider.id, crate::ai::provider_model_name(model)))
    };
    left == right
        || identity(left)
            .zip(identity(right))
            .is_some_and(|(a, b)| a == b)
}
