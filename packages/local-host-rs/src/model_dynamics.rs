//! TUI model catalog integration for the runtime-owned model preference values.

// The value types and loop state live in `maestro-runtime`; the TUI keeps the
// catalog-aware normalization and boost compatibility checks here.
pub use maestro_runtime::agent::model_dynamics::same_model_route;
pub use maestro_runtime::agent::model_dynamics::thinking_level;
pub use maestro_runtime::agent::{
    BoostStatus, ModelChoice, ModelDynamicsConfig, TaskDifficulty, ThinkingLevel,
};

pub fn configured_thinking(
    config: &crate::config::ComposerConfig,
    model: &str,
    dynamics: &ModelDynamicsConfig,
) -> ThinkingLevel {
    let default = match config.model_reasoning_effort {
        Some(crate::config::ReasoningEffort::Minimal) => ThinkingLevel::Minimal,
        Some(crate::config::ReasoningEffort::Low) => ThinkingLevel::Low,
        Some(crate::config::ReasoningEffort::Medium) => ThinkingLevel::Medium,
        Some(crate::config::ReasoningEffort::High) => ThinkingLevel::High,
        None => ThinkingLevel::Off,
    };
    let requested = dynamics.effort_for_model(model).unwrap_or(default);
    normalize_thinking(model, requested)
}

pub fn next_cycle_route<'a>(
    routes: &'a [String],
    current: &str,
    backward: bool,
) -> Option<&'a str> {
    if routes.is_empty() {
        return None;
    }
    let index = routes
        .iter()
        .position(|route| same_model_route(route, current));
    let next = match index {
        Some(index) if backward => (index + routes.len() - 1) % routes.len(),
        Some(index) => (index + 1) % routes.len(),
        None if backward => routes.len() - 1,
        None => 0,
    };
    (!same_model_route(&routes[next], current)).then_some(routes[next].as_str())
}

pub fn boost_choice(current: &ModelChoice, config: &ModelDynamicsConfig) -> Option<ModelChoice> {
    let info = crate::model_catalog::find_model(&current.model)?;
    let next = if let Some(choice) = &config.boost {
        ModelChoice {
            model: choice.model.clone(),
            thinking: normalize_thinking(&choice.model, choice.thinking),
        }
    } else {
        if !info.capabilities.reasoning {
            return None;
        }
        let thinking = if crate::codex_auth::resolve_model_route(&current.model).uses_app_server() {
            if current.thinking == ThinkingLevel::Max {
                return None;
            }
            ThinkingLevel::Max
        } else {
            match current.thinking {
                ThinkingLevel::Off
                | ThinkingLevel::Minimal
                | ThinkingLevel::Low
                | ThinkingLevel::Medium => ThinkingLevel::High,
                ThinkingLevel::High => ThinkingLevel::XHigh,
                ThinkingLevel::XHigh => ThinkingLevel::Max,
                ThinkingLevel::Max => return None,
            }
        };
        ModelChoice {
            model: current.model.clone(),
            thinking,
        }
    };
    if next.model != current.model {
        let target = crate::model_catalog::find_model(&next.model)?;
        if info.capabilities.protocol != target.capabilities.protocol
            || info.capabilities.context_tokens != target.capabilities.context_tokens
            || info.capabilities.vision != target.capabilities.vision
            || info.capabilities.tools != target.capabilities.tools
            || crate::codex_auth::resolve_model_route(&current.model).uses_app_server()
        {
            return None;
        }
    }
    if next.model == current.model && next.thinking.to_config().1 <= current.thinking.to_config().1
    {
        return None;
    }
    if matches!(
        info.capabilities.protocol,
        crate::model_catalog::ModelProtocol::OpenAiChat
            | crate::model_catalog::ModelProtocol::OpenAiResponses
    ) && !crate::codex_auth::resolve_model_route(&current.model).uses_app_server()
        && next.model == current.model
        && current.thinking != ThinkingLevel::Off
    {
        let caps = crate::ai::openai_request_capabilities(Some(&info.provider), &current.model);
        if caps.reasoning_effort(current.thinking.to_config().1)
            == caps.reasoning_effort(next.thinking.to_config().1)
        {
            return None;
        }
    }
    Some(next)
}

/// Normalize a generic UI level to the direct adapter's actual supported value.
/// App-server capabilities are resolved live by the Codex turn adapter.
pub fn normalize_thinking(model: &str, requested: ThinkingLevel) -> ThinkingLevel {
    if crate::codex_auth::resolve_model_route(model).uses_app_server() {
        return requested;
    }
    let Some(info) = crate::model_catalog::find_model(model) else {
        return requested;
    };
    if !info.capabilities.reasoning {
        return ThinkingLevel::Off;
    }
    if info.capabilities.protocol == crate::model_catalog::ModelProtocol::Anthropic {
        let caps = crate::ai::anthropic_request_capabilities(Some(&info.provider), model);
        if requested == ThinkingLevel::Off {
            return if caps.thinking == crate::ai::AnthropicThinkingMode::AlwaysOn {
                ThinkingLevel::High
            } else {
                requested
            };
        }
        return match caps.effort_for_budget(requested.to_config().1) {
            Some("low") => ThinkingLevel::Low,
            Some("medium") => ThinkingLevel::Medium,
            Some("high") => ThinkingLevel::High,
            Some("xhigh") => ThinkingLevel::XHigh,
            Some("max") => ThinkingLevel::Max,
            _ => requested,
        };
    }
    if !matches!(
        info.capabilities.protocol,
        crate::model_catalog::ModelProtocol::OpenAiChat
            | crate::model_catalog::ModelProtocol::OpenAiResponses
    ) || requested == ThinkingLevel::Off
    {
        return requested;
    }
    let caps = crate::ai::openai_request_capabilities(Some(&info.provider), model);
    match caps.reasoning_effort(requested.to_config().1) {
        "low" => ThinkingLevel::Low,
        "medium" => ThinkingLevel::Medium,
        "high" => ThinkingLevel::High,
        _ => requested,
    }
}

/// Cycle distinct effective levels, skipping budgets the provider normalizes
/// to the same effort. Unknown/custom models retain the existing six levels.
pub fn next_thinking_level(model: &str, current: ThinkingLevel) -> ThinkingLevel {
    let mut levels = Vec::new();
    for requested in [
        ThinkingLevel::Off,
        ThinkingLevel::Minimal,
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
        ThinkingLevel::XHigh,
        ThinkingLevel::Max,
    ] {
        let level = normalize_thinking(model, requested);
        if !levels.contains(&level) {
            levels.push(level);
        }
    }
    levels.sort_by_key(|level| level.to_config().1);
    let current = normalize_thinking(model, current);
    let index = levels
        .iter()
        .position(|level| *level == current)
        .unwrap_or(0);
    levels[(index + 1) % levels.len()]
}

#[cfg(test)]
mod selection_tests {
    use super::*;
    #[test]
    fn cycle_scope_preserves_order_direction_and_provider_identity() {
        let routes = vec![
            "openai/gpt-5.6".into(),
            "anthropic/claude-fable-5-1".into(),
            "openai-codex/gpt-5.6".into(),
        ];
        assert_eq!(
            next_cycle_route(&routes, "gpt-5.6", false),
            Some(routes[1].as_str())
        );
        assert_eq!(
            next_cycle_route(&routes, &routes[0], true),
            Some(routes[2].as_str())
        );
        assert_eq!(
            next_cycle_route(&routes, &routes[2], false),
            Some(routes[0].as_str())
        );
        assert_eq!(
            next_cycle_route(&routes, "unknown", true),
            Some(routes[2].as_str())
        );
        assert_eq!(next_cycle_route(&routes[..1], "gpt-5.6", false), None);
        assert_eq!(next_cycle_route(&[], "gpt-5.6", false), None);
    }

    #[test]
    fn configured_startup_effort_and_scoped_effort_are_normalized() {
        let mut config = crate::config::ComposerConfig::default();
        let mut dynamics = ModelDynamicsConfig::default();
        assert_eq!(
            configured_thinking(&config, "gpt-5.6", &dynamics),
            ThinkingLevel::Off
        );
        config.model_reasoning_effort = Some(crate::config::ReasoningEffort::High);
        assert_eq!(
            configured_thinking(&config, "gpt-5.6", &dynamics),
            ThinkingLevel::High
        );
        dynamics.cycle.push(ModelChoice {
            model: "openai/gpt-5.6".into(),
            thinking: ThinkingLevel::Low,
        });
        assert_eq!(
            configured_thinking(&config, "gpt-5.6", &dynamics),
            ThinkingLevel::Low
        );
        assert_eq!(dynamics.effort_for_model("openai-codex/gpt-5.6"), None);
        assert_eq!(
            configured_thinking(&config, "claude-fable-5-1", &dynamics),
            ThinkingLevel::High
        );
    }

    #[test]
    fn always_on_claude_effort_matches_the_wire_and_cycles_in_order() {
        let model = "anthropic/claude-fable-5-1";
        assert_eq!(
            normalize_thinking(model, ThinkingLevel::Off),
            ThinkingLevel::High
        );
        assert_eq!(
            normalize_thinking(model, ThinkingLevel::Minimal),
            ThinkingLevel::Low
        );
        for (current, next) in [
            (ThinkingLevel::Low, ThinkingLevel::Medium),
            (ThinkingLevel::Medium, ThinkingLevel::High),
            (ThinkingLevel::High, ThinkingLevel::Max),
            (ThinkingLevel::Max, ThinkingLevel::Low),
        ] {
            assert_eq!(next_thinking_level(model, current), next);
        }
        assert_eq!(
            normalize_thinking("anthropic/claude-sonnet-4-6", ThinkingLevel::Off),
            ThinkingLevel::Off
        );
    }

    #[test]
    fn shift_tab_cycles_distinct_provider_levels() {
        let model = "openai/gpt-4o";
        assert_eq!(
            normalize_thinking(model, ThinkingLevel::High),
            ThinkingLevel::Off
        );
        assert_eq!(
            next_thinking_level(model, ThinkingLevel::Off),
            ThinkingLevel::Off
        );

        // o1 supports low/medium/high; normalized Minimal and Max must not
        // trap the cycle at Low or High.
        let model = "openrouter/openai/o1";
        assert_eq!(
            next_thinking_level(model, ThinkingLevel::Off),
            ThinkingLevel::Low
        );
        assert_eq!(
            next_thinking_level(model, ThinkingLevel::Low),
            ThinkingLevel::Medium
        );
        assert_eq!(
            next_thinking_level(model, ThinkingLevel::Medium),
            ThinkingLevel::High
        );
        assert_eq!(
            next_thinking_level(model, ThinkingLevel::High),
            ThinkingLevel::Off
        );
    }
    #[test]
    fn explicit_parent_model_does_not_inherit_another_tiers_effort() {
        let parent = ModelChoice {
            model: "parent".into(),
            thinking: ThinkingLevel::Off,
        };
        let config = ModelDynamicsConfig {
            heavy: Some(ModelChoice {
                model: "other".into(),
                thinking: ThinkingLevel::High,
            }),
            ..Default::default()
        };
        assert_eq!(
            config.resolve_child(TaskDifficulty::Heavy, Some("parent"), None, &parent),
            parent
        );
        assert_eq!(
            config
                .resolve_child(TaskDifficulty::Heavy, Some("third"), None, &parent)
                .thinking,
            ThinkingLevel::Medium
        );
    }
    #[test]
    fn settings_roundtrip_and_reject_unknown_difficulty() {
        let config: ModelDynamicsConfig =
            toml::from_str("[heavy]\nmodel = 'openai/gpt-5.5'\nthinking = 'max'").unwrap();
        assert_eq!(config.heavy.unwrap().thinking, ThinkingLevel::Max);
        assert!(TaskDifficulty::parse("automatic").is_err());
        assert!(toml::from_str::<ModelDynamicsConfig>("auto_boost = 'true'").is_err());
    }
    #[test]
    fn boosts_do_not_advertise_equivalent_wire_settings() {
        let config = ModelDynamicsConfig::default();
        let current = ModelChoice {
            model: "openrouter/openai/o1".into(),
            thinking: ThinkingLevel::High,
        };
        assert!(boost_choice(&current, &config).is_none());
        let medium = ModelChoice {
            thinking: ThinkingLevel::Medium,
            ..current
        };
        assert_eq!(
            boost_choice(&medium, &config).unwrap().thinking,
            ThinkingLevel::High
        );
        assert_eq!(
            normalize_thinking("openrouter/openai/o1", ThinkingLevel::Low),
            ThinkingLevel::Low
        );
    }
    #[test]
    fn xhigh_normalizes_to_max_on_models_without_it() {
        // Anthropic documents xhigh on these families.
        for model in [
            "claude-opus-5-5",
            "claude-opus-5",
            "claude-opus-4-8",
            "claude-sonnet-5",
            "claude-fable-5-1",
        ] {
            assert_eq!(
                normalize_thinking(model, ThinkingLevel::XHigh),
                ThinkingLevel::XHigh,
                "{model}"
            );
        }

        // These accept max but not xhigh, so the picker must not offer XHigh.
        for model in ["claude-opus-4-6", "claude-sonnet-4-6"] {
            assert_eq!(
                normalize_thinking(model, ThinkingLevel::XHigh),
                ThinkingLevel::Max,
                "{model}"
            );
        }
    }

    #[test]
    fn boost_steps_through_xhigh_before_max() {
        let config = ModelDynamicsConfig::default();
        let high = ModelChoice {
            model: "claude-opus-5-5".into(),
            thinking: ThinkingLevel::High,
        };
        let boosted = boost_choice(&high, &config).expect("high boosts");
        assert_eq!(boosted.thinking, ThinkingLevel::XHigh);

        let xhigh = ModelChoice {
            model: "claude-opus-5-5".into(),
            thinking: ThinkingLevel::XHigh,
        };
        assert_eq!(
            boost_choice(&xhigh, &config)
                .expect("xhigh boosts")
                .thinking,
            ThinkingLevel::Max
        );
    }

    #[test]
    fn codex_effort_keeps_live_adapter_authority() {
        let model = "openai-codex/gpt-6-astra";
        assert_eq!(
            normalize_thinking(model, ThinkingLevel::Max),
            ThinkingLevel::Max
        );
        let current = ModelChoice {
            model: model.into(),
            thinking: ThinkingLevel::High,
        };
        assert_eq!(
            boost_choice(&current, &ModelDynamicsConfig::default())
                .unwrap()
                .thinking,
            ThinkingLevel::Max
        );
    }
}
