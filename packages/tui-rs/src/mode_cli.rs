//! Native `maestro modes` command.

use anyhow::{Result, bail};
use serde_json::{Map, Value, json};

use crate::swarm::{
    AgentMode, DispatchSource, ModelProvider, ModelTier, ReasoningEffort, SubagentType,
    model_for_tier, resolve_subagent_dispatch,
};

#[derive(Clone, Copy)]
struct ModeConfig {
    mode: AgentMode,
    name: &'static str,
    description: &'static str,
    primary: ModelTier,
    fallback: ModelTier,
    reasoning: ReasoningEffort,
    thinking: bool,
    thinking_budget: u32,
    extended_context: bool,
    retries: u8,
    cost_multiplier: f64,
    speed_hint: u8,
    visible: bool,
    rollout_owner: Option<&'static str>,
}

macro_rules! mode {
    (@owner) => {
        None
    };
    (@owner $rollout_owner:expr) => {
        Some($rollout_owner)
    };
    ($mode:expr, $name:expr, $description:expr, $primary:expr, $fallback:expr, $reasoning:expr, $thinking:expr, $thinking_budget:expr, $extended_context:expr, $retries:expr, $cost_multiplier:expr, $speed_hint:expr, $visible:expr $(, $rollout_owner:expr)? $(,)?) => {
        ModeConfig {
            mode: $mode,
            name: $name,
            description: $description,
            primary: $primary,
            fallback: $fallback,
            reasoning: $reasoning,
            thinking: $thinking,
            thinking_budget: $thinking_budget,
            extended_context: $extended_context,
            retries: $retries,
            cost_multiplier: $cost_multiplier,
            speed_hint: $speed_hint,
            visible: $visible,
            rollout_owner: mode!(@owner $($rollout_owner)?),
        }
    };
}

const MODES: [ModeConfig; 10] = [
    mode!(
        AgentMode::Low,
        "Low",
        "Bounded, obvious, and reversible work",
        ModelTier::Haiku,
        ModelTier::Haiku,
        ReasoningEffort::Low,
        false,
        2_000,
        false,
        1,
        0.1,
        10,
        true,
    ),
    mode!(
        AgentMode::Medium,
        "Medium",
        "Ordinary repository work with moderate uncertainty",
        ModelTier::Opus,
        ModelTier::Haiku,
        ReasoningEffort::Medium,
        true,
        16_000,
        true,
        2,
        1.0,
        5,
        true,
    ),
    mode!(
        AgentMode::High,
        "High",
        "Ambiguous or cross-cutting work where misses are expensive",
        ModelTier::Opus,
        ModelTier::Sonnet,
        ReasoningEffort::XHigh,
        true,
        20_000,
        true,
        2,
        1.25,
        4,
        true,
    ),
    mode!(
        AgentMode::Ultra,
        "Ultra",
        "Migrations, architecture, and discovery-heavy work",
        ModelTier::Opus,
        ModelTier::Opus,
        ReasoningEffort::XHigh,
        true,
        32_000,
        true,
        3,
        1.5,
        3,
        true,
    ),
    mode!(
        AgentMode::Smart,
        "Smart",
        "Best quality, uses opus for complex tasks",
        ModelTier::Opus,
        ModelTier::Sonnet,
        ReasoningEffort::Medium,
        true,
        16_000,
        true,
        3,
        1.0,
        5,
        true,
    ),
    mode!(
        AgentMode::Rush,
        "Rush",
        "Fast responses, uses sonnet for speed",
        ModelTier::Sonnet,
        ModelTier::Haiku,
        ReasoningEffort::Low,
        false,
        4_000,
        false,
        2,
        0.5,
        8,
        true,
    ),
    mode!(
        AgentMode::Free,
        "Free",
        "Most cost-effective, uses haiku",
        ModelTier::Haiku,
        ModelTier::Haiku,
        ReasoningEffort::Low,
        false,
        2_000,
        false,
        1,
        0.1,
        10,
        true,
    ),
    mode!(
        AgentMode::Custom,
        "Custom",
        "User-defined configuration",
        ModelTier::Sonnet,
        ModelTier::Haiku,
        ReasoningEffort::Medium,
        true,
        8_000,
        true,
        2,
        0.7,
        6,
        true,
    ),
    mode!(
        AgentMode::Frontier,
        "Frontier",
        "Experimental high-capability orchestration mode",
        ModelTier::Opus,
        ModelTier::Sonnet,
        ReasoningEffort::High,
        true,
        20_000,
        true,
        3,
        1.25,
        4,
        false,
        "agent-runtime",
    ),
    mode!(
        AgentMode::Replay,
        "Replay",
        "Deterministic scripted scenario replay",
        ModelTier::Haiku,
        ModelTier::Haiku,
        ReasoningEffort::Low,
        false,
        0,
        false,
        0,
        0.0,
        10,
        false,
        "agent-evals",
    ),
];

const GLOBAL_FLAGS_WITH_VALUES: [&str; 24] = [
    "--mode",
    "--model",
    "-m",
    "--task-budget",
    "--models",
    "--models-file",
    "--api-key",
    "--port",
    "--system-prompt",
    "--append-system-prompt",
    "--session",
    "--approval-mode",
    "--auth",
    "--sandbox",
    "--output-schema",
    "--output-last-message",
    "--tools",
    "--composer",
    "--format",
    "--output-dir",
    "--junit",
    "--replay",
    "--record-scenario",
    "--profile",
];

const GLOBAL_BOOLEAN_FLAGS: [&str; 13] = [
    "--headless",
    "--continue",
    "-c",
    "--no-session",
    "--safe-mode",
    "--force",
    "--stream-json",
    "--full-auto",
    "--read-only",
    "--readonly",
    "--read-only-mode",
    "--redact-secrets",
    "--live-mcp",
];

pub async fn run_modes(args: &[String]) -> Result<i32> {
    let mut provider = ModelProvider::OpenAiCodex;
    let mut json_output = false;
    let mut include_hidden = false;
    let mut legacy_all_format = false;
    let mut positionals = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--json" => json_output = true,
            "--include-hidden" | "--all" => include_hidden = true,
            "--list-modes-all" => {
                include_hidden = true;
                legacy_all_format = true;
            }
            "--help" | "-h" => positionals.push("help"),
            "--provider" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| anyhow::anyhow!("--provider requires a value"))?;
                provider = parse_provider(value)?;
            }
            value if value.starts_with("--provider=") => {
                provider = parse_provider(value.trim_start_matches("--provider="))?;
            }
            "--config" => {
                // Config overrides are repeatable global options and do not affect modes output.
                index += usize::from(args.get(index + 1).is_some());
            }
            "--worktree" => {
                if args
                    .get(index + 1)
                    .is_some_and(|value| !value.starts_with('-'))
                {
                    index += 1;
                }
            }
            value if GLOBAL_FLAGS_WITH_VALUES.contains(&value) => {
                index += usize::from(args.get(index + 1).is_some());
            }
            value
                if GLOBAL_FLAGS_WITH_VALUES
                    .iter()
                    .any(|flag| value.starts_with(&format!("{flag}=")))
                    || value.starts_with("--config=")
                    || value.starts_with("--worktree=") => {}
            value if GLOBAL_BOOLEAN_FLAGS.contains(&value) => {}
            value if value.starts_with('-') => bail!(
                "{}",
                crate::localization::cli_locale()
                    .format("Unknown modes option: {0}", &[(value).to_string()])
            ),
            value => positionals.push(value),
        }
        index += 1;
    }

    let command = positionals.first().copied().unwrap_or("list");
    if matches!(command, "help" | "--help" | "-h") {
        println!("{}", usage());
        return Ok(0);
    }
    if command == "list" {
        if include_hidden {
            crate::telemetry::record_staged_rollout_surface_usage(
                "hidden_flag_used",
                "cli:--list-modes-all",
                "cli_flag",
                Some("agent-runtime"),
                "cli:modes:list",
            )
            .await;
        }
        print_list(include_hidden, legacy_all_format);
        return Ok(0);
    }

    let requested_mode_name = if command == "describe" {
        positionals.get(1).copied()
    } else {
        Some(command)
    };
    let Some(config) = requested_mode_name.and_then(find_mode) else {
        let suffix = requested_mode_name
            .map(|name| format!(": {name}"))
            .unwrap_or_default();
        eprintln!(
            "{}",
            crate::localization::cli_locale()
                .format("Unknown mode{0}", std::slice::from_ref(&(suffix)))
        );
        println!("{}", usage());
        return Ok(1);
    };

    let description = describe_mode(config, provider);
    if !config.visible {
        crate::telemetry::record_staged_rollout_surface_usage(
            "hidden_mode_used",
            &format!("mode:{}", mode_name(config.mode)),
            "mode",
            config.rollout_owner,
            "cli:modes:describe",
        )
        .await;
    }
    if json_output {
        println!("{}", serde_json::to_string_pretty(&description)?);
    } else {
        println!("{}", render_description(config, provider));
    }
    Ok(0)
}

fn parse_provider(value: &str) -> Result<ModelProvider> {
    match value {
        "anthropic" => Ok(ModelProvider::Anthropic),
        "openai" => Ok(ModelProvider::OpenAi),
        "openai-codex" => Ok(ModelProvider::OpenAiCodex),
        "google" => Ok(ModelProvider::Google),
        _ => bail!("{}", crate::localization::cli_locale().format("Unknown provider \"{0}\". Supported providers: anthropic, openai, openai-codex, google", &[(value).to_string()])),
    }
}

fn find_mode(value: &str) -> Option<&'static ModeConfig> {
    MODES
        .iter()
        .find(|config| mode_name(config.mode) == value.trim().to_lowercase())
}

fn mode_name(mode: AgentMode) -> &'static str {
    match mode {
        AgentMode::Low => "low",
        AgentMode::Medium => "medium",
        AgentMode::High => "high",
        AgentMode::Ultra => "ultra",
        AgentMode::Smart => "smart",
        AgentMode::Rush => "rush",
        AgentMode::Free => "free",
        AgentMode::Custom => "custom",
        AgentMode::Frontier => "frontier",
        AgentMode::Replay => "replay",
    }
}

fn tier_name(tier: ModelTier) -> &'static str {
    match tier {
        ModelTier::Opus => "opus",
        ModelTier::Sonnet => "sonnet",
        ModelTier::Haiku => "haiku",
    }
}

fn provider_name(provider: ModelProvider) -> &'static str {
    match provider {
        ModelProvider::Anthropic => "anthropic",
        ModelProvider::OpenAi => "openai",
        ModelProvider::OpenAiCodex => "openai-codex",
        ModelProvider::Google => "google",
    }
}

fn provider_family(provider: ModelProvider) -> &'static str {
    match provider {
        ModelProvider::Anthropic => "anthropic",
        ModelProvider::OpenAi | ModelProvider::OpenAiCodex => "openai",
        ModelProvider::Google => "google",
    }
}

/// The read-only oracle and cross-check reviewers must come from a different
/// provider family than the primary model, so a second opinion never shares
/// the primary's blind spots. Keep the level's preferred provider when it
/// already differs; otherwise switch to the other major family.
fn cross_provider(preferred: ModelProvider, primary: ModelProvider) -> ModelProvider {
    if provider_family(preferred) != provider_family(primary) {
        return preferred;
    }
    match primary {
        ModelProvider::Anthropic => ModelProvider::OpenAiCodex,
        ModelProvider::OpenAi | ModelProvider::OpenAiCodex | ModelProvider::Google => {
            ModelProvider::Anthropic
        }
    }
}

fn effort_name(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::XHigh => "xhigh",
    }
}

fn subagent_name(kind: SubagentType) -> &'static str {
    match kind {
        SubagentType::Explorer => "explorer",
        SubagentType::Planner => "planner",
        SubagentType::Coder => "coder",
        SubagentType::Reviewer => "reviewer",
        SubagentType::TestRunner => "test-runner",
        SubagentType::Researcher => "researcher",
        SubagentType::BrowserQa => "browser-qa",
        SubagentType::Minimal => "minimal",
        SubagentType::Custom => "custom",
    }
}

fn subagent_metadata(kind: SubagentType) -> (&'static str, &'static str) {
    match kind {
        SubagentType::Explorer => (
            "Explorer",
            "Read-only codebase exploration - can search and read files",
        ),
        SubagentType::Planner => ("Planner", "Planning mode - can read files and manage todos"),
        SubagentType::Coder => (
            "Coder",
            "Full coding capabilities - can read, write, and execute",
        ),
        SubagentType::Reviewer => (
            "Reviewer",
            "Code review mode - can read files and search web",
        ),
        SubagentType::TestRunner => (
            "Test Runner",
            "Test execution mode - can run checks and inspect results",
        ),
        SubagentType::Researcher => (
            "Researcher",
            "Research mode - focused on web search and analysis",
        ),
        SubagentType::BrowserQa => (
            "Browser QA",
            "Product QA mode - explores web surfaces and captures repro evidence",
        ),
        SubagentType::Minimal => (
            "Minimal",
            "Minimal capabilities - only basic read operations",
        ),
        SubagentType::Custom => ("Custom", "Custom subagent configuration"),
    }
}

const SUBAGENTS: [SubagentType; 9] = [
    SubagentType::Explorer,
    SubagentType::Planner,
    SubagentType::Coder,
    SubagentType::Reviewer,
    SubagentType::TestRunner,
    SubagentType::Researcher,
    SubagentType::BrowserQa,
    SubagentType::Minimal,
    SubagentType::Custom,
];

fn dispatch_json(mode: AgentMode, kind: SubagentType, provider: ModelProvider) -> Value {
    let dispatch = resolve_subagent_dispatch(mode, kind, provider);
    let (display_name, description) = subagent_metadata(kind);
    let mut value = Map::new();
    value.insert("mode".into(), json!(mode_name(dispatch.mode)));
    value.insert("type".into(), json!(subagent_name(dispatch.subagent_type)));
    value.insert("provider".into(), json!(provider_name(dispatch.provider)));
    value.insert("model".into(), json!(dispatch.model));
    if let Some(tier) = dispatch.model_tier {
        value.insert("modelTier".into(), json!(tier_name(tier)));
    }
    value.insert(
        "reasoningEffort".into(),
        json!(effort_name(dispatch.reasoning_effort)),
    );
    value.insert(
        "source".into(),
        json!(match dispatch.source {
            DispatchSource::Mode => "mode",
            DispatchSource::Fallback => "fallback",
        }),
    );
    value.insert("displayName".into(), json!(display_name));
    value.insert("description".into(), json!(description));
    Value::Object(value)
}

fn describe_mode(config: &ModeConfig, provider: ModelProvider) -> Value {
    let mut value = Map::new();
    value.insert("mode".into(), json!(mode_name(config.mode)));
    value.insert("displayName".into(), json!(config.name));
    value.insert("description".into(), json!(config.description));
    value.insert("visible".into(), json!(config.visible));
    value.insert("primary".into(), json!({"tier": tier_name(config.primary), "provider": provider_name(provider), "model": model_for_tier(config.primary, provider)}));
    value.insert("fallback".into(), json!({"tier": tier_name(config.fallback), "provider": provider_name(provider), "model": model_for_tier(config.fallback, provider)}));
    value.insert(
        "reasoningEffort".into(),
        json!(effort_name(config.reasoning)),
    );
    value.insert(
        "thinking".into(),
        json!({"enabled": config.thinking, "budget": config.thinking_budget}),
    );
    value.insert(
        "context".into(),
        json!({"extended": config.extended_context}),
    );
    value.insert("retries".into(), json!(config.retries));
    value.insert("costMultiplier".into(), json!(config.cost_multiplier));
    value.insert("speedHint".into(), json!(config.speed_hint));
    value.insert(
        "subagents".into(),
        Value::Array(
            SUBAGENTS
                .iter()
                .map(|kind| dispatch_json(config.mode, *kind, provider))
                .collect(),
        ),
    );
    if let Some(profile) = profile_json(config.mode, provider) {
        value.insert("agentProfile".into(), profile);
    }
    Value::Object(value)
}

fn profile_json(mode: AgentMode, provider: ModelProvider) -> Option<Value> {
    let (level, oracle_provider, oracle_effort, attempts, tool_calls, fallbacks) = match mode {
        AgentMode::Low | AgentMode::Rush | AgentMode::Free => (
            "low",
            ModelProvider::OpenAiCodex,
            ReasoningEffort::Medium,
            1,
            15,
            vec![],
        ),
        AgentMode::Medium | AgentMode::Smart | AgentMode::Custom => (
            "medium",
            ModelProvider::Anthropic,
            ReasoningEffort::Medium,
            2,
            30,
            vec!["low"],
        ),
        AgentMode::High => (
            "high",
            ModelProvider::Anthropic,
            ReasoningEffort::High,
            2,
            45,
            vec!["medium", "low"],
        ),
        AgentMode::Ultra | AgentMode::Frontier => (
            "ultra",
            ModelProvider::OpenAiCodex,
            ReasoningEffort::XHigh,
            3,
            60,
            vec!["high", "medium"],
        ),
        AgentMode::Replay => return None,
    };
    let oracle_provider = cross_provider(oracle_provider, provider);
    let profile_config = find_mode(level)?;
    let specialist =
        |specialist_provider: ModelProvider, tier: ModelTier, effort: ReasoningEffort| {
            json!({
                "provider": provider_name(specialist_provider),
                "model": model_for_tier(tier, specialist_provider),
                "reasoningEffort": effort_name(effort)
            })
        };
    let mut specialists = Map::new();
    match level {
        "low" => {
            specialists.insert(
                "explorer".into(),
                specialist(provider, ModelTier::Haiku, ReasoningEffort::Low),
            );
            specialists.insert(
                "coder".into(),
                specialist(provider, ModelTier::Haiku, ReasoningEffort::Low),
            );
            specialists.insert(
                "reviewer".into(),
                specialist(provider, ModelTier::Sonnet, ReasoningEffort::Low),
            );
        }
        "medium" => {
            specialists.insert(
                "explorer".into(),
                specialist(provider, ModelTier::Haiku, ReasoningEffort::Low),
            );
            specialists.insert(
                "planner".into(),
                specialist(provider, ModelTier::Sonnet, ReasoningEffort::Medium),
            );
            specialists.insert(
                "coder".into(),
                specialist(
                    ModelProvider::OpenAiCodex,
                    ModelTier::Opus,
                    ReasoningEffort::Medium,
                ),
            );
            specialists.insert(
                "reviewer".into(),
                specialist(provider, ModelTier::Sonnet, ReasoningEffort::Medium),
            );
        }
        "high" => {
            specialists.insert(
                "explorer".into(),
                specialist(provider, ModelTier::Sonnet, ReasoningEffort::Medium),
            );
            specialists.insert(
                "planner".into(),
                specialist(provider, ModelTier::Opus, ReasoningEffort::High),
            );
            specialists.insert(
                "coder".into(),
                specialist(
                    ModelProvider::OpenAiCodex,
                    ModelTier::Opus,
                    ReasoningEffort::XHigh,
                ),
            );
            specialists.insert(
                "reviewer".into(),
                specialist(
                    cross_provider(ModelProvider::Anthropic, provider),
                    ModelTier::Opus,
                    ReasoningEffort::High,
                ),
            );
        }
        "ultra" => {
            specialists.insert(
                "explorer".into(),
                specialist(provider, ModelTier::Sonnet, ReasoningEffort::High),
            );
            specialists.insert(
                "planner".into(),
                specialist(provider, ModelTier::Opus, ReasoningEffort::XHigh),
            );
            specialists.insert(
                "coder".into(),
                specialist(provider, ModelTier::Opus, ReasoningEffort::XHigh),
            );
            specialists.insert(
                "reviewer".into(),
                specialist(
                    cross_provider(ModelProvider::OpenAiCodex, provider),
                    ModelTier::Opus,
                    ReasoningEffort::XHigh,
                ),
            );
        }
        _ => unreachable!("profile level is canonical"),
    }
    Some(json!({
        "id": format!("{level}-v1"), "version": 1, "level": level,
        "description": profile_config.description,
        "primary": {"provider": provider_name(provider), "model": model_for_tier(profile_config.primary, provider), "reasoningEffort": effort_name(profile_config.reasoning)},
        "oracle": {"provider": provider_name(oracle_provider), "model": model_for_tier(ModelTier::Opus, oracle_provider), "reasoningEffort": effort_name(oracle_effort), "readOnly": true},
        "specialists": specialists, "fallbackLevels": fallbacks,
        "budgets": {"maxAttempts": attempts, "maxToolCalls": tool_calls}
    }))
}

fn print_list(include_hidden: bool, legacy_all_format: bool) {
    println!(
        "{}",
        crate::localization::cli_locale().format("Agent modes:\n", &[])
    );
    for config in MODES
        .iter()
        .filter(|config| config.visible || include_hidden)
    {
        let hidden = if config.visible { "" } else { " [hidden]" };
        println!("{}{hidden}", mode_name(config.mode));
        println!("  {}", config.description);
        if legacy_all_format {
            println!(
                "  model: {}",
                model_for_tier(config.primary, ModelProvider::OpenAiCodex)
            );
        } else {
            println!(
                "{}",
                crate::localization::cli_locale().format(
                    "  describe: maestro modes describe {0}",
                    &[(mode_name(config.mode)).to_string()]
                )
            );
        }
    }
}

fn render_description(config: &ModeConfig, provider: ModelProvider) -> String {
    let mut lines = vec![
        crate::localization::cli_locale().format(
            "Mode: {0} ({1})",
            &[
                (config.name).to_string(),
                (mode_name(config.mode)).to_string(),
            ],
        ),
        config.description.into(),
        String::new(),
        crate::localization::cli_locale().format(
            "Visibility: {0}",
            &[(if config.visible { "visible" } else { "hidden" }).to_string()],
        ),
        crate::localization::cli_locale().format(
            "Primary: {0} -> {1}/{2}",
            &[
                (tier_name(config.primary)).to_string(),
                (provider_name(provider)).to_string(),
                (model_for_tier(config.primary, provider)).to_string(),
            ],
        ),
        crate::localization::cli_locale().format(
            "Fallback: {0} -> {1}/{2}",
            &[
                (tier_name(config.fallback)).to_string(),
                (provider_name(provider)).to_string(),
                (model_for_tier(config.fallback, provider)).to_string(),
            ],
        ),
        crate::localization::cli_locale().format(
            "Reasoning: {0}",
            &[(effort_name(config.reasoning)).to_string()],
        ),
        crate::localization::cli_locale().format(
            "Thinking: {0} (budget {1})",
            &[
                (if config.thinking {
                    "enabled"
                } else {
                    "disabled"
                })
                .to_string(),
                (config.thinking_budget).to_string(),
            ],
        ),
        crate::localization::cli_locale().format(
            "Context: {0}",
            &[(if config.extended_context {
                "extended"
            } else {
                "standard"
            })
            .to_string()],
        ),
        crate::localization::cli_locale().format("Retries: {0}", &[(config.retries).to_string()]),
    ];
    if let Some(profile) = profile_json(config.mode, provider) {
        lines.push(crate::localization::cli_locale().format(
            "Profile: {0}",
            &[profile["id"].as_str().unwrap_or("-").to_string()],
        ));
        lines.push(
            crate::localization::cli_locale().format(
                "Oracle: {0}/{1} ({2})",
                &[
                    profile["oracle"]["provider"]
                        .as_str()
                        .unwrap_or("-")
                        .to_string(),
                    profile["oracle"]["model"]
                        .as_str()
                        .unwrap_or("-")
                        .to_string(),
                    profile["oracle"]["reasoningEffort"]
                        .as_str()
                        .unwrap_or("-")
                        .to_string(),
                ],
            ),
        );
    }
    lines.extend([
        String::new(),
        crate::localization::cli_locale().format(
            "Subagent dispatch (provider: {0})",
            &[(provider_name(provider)).to_string()],
        ),
        crate::localization::cli_locale().format(
            "{0} {1} {2} {3} {4} Tier",
            &[
                format!("{:<12}", "Type"),
                format!("{:<8}", "Source"),
                format!("{:<14}", "Provider"),
                format!("{:<34}", "Model"),
                format!("{:<8}", "Effort"),
            ],
        ),
        format!(
            "{} {} {} {} {} {}",
            "-".repeat(12),
            "-".repeat(8),
            "-".repeat(14),
            "-".repeat(34),
            "-".repeat(8),
            "-".repeat(8)
        ),
    ]);
    for kind in SUBAGENTS {
        let dispatch = resolve_subagent_dispatch(config.mode, kind, provider);
        lines.push(format!(
            "{:<12} {:<8} {:<14} {:<34} {:<8} {}",
            subagent_name(kind),
            match dispatch.source {
                DispatchSource::Mode => "mode",
                DispatchSource::Fallback => "fallback",
            },
            provider_name(dispatch.provider),
            dispatch.model,
            effort_name(dispatch.reasoning_effort),
            dispatch.model_tier.map(tier_name).unwrap_or("-")
        ));
    }
    lines.join("\n")
}

fn usage() -> &'static str {
    crate::localization::cli_locale().translate("Usage:\n  maestro modes list\n  maestro modes describe <mode> [--provider <provider>] [--json]\n\nExamples:\n  maestro modes describe smart\n  maestro modes describe frontier --provider openai --json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_high_profile_matches_cli_contract() {
        let value = describe_mode(find_mode("high").unwrap(), ModelProvider::OpenAiCodex);
        assert_eq!(value["mode"], "high");
        assert_eq!(value["agentProfile"]["id"], "high-v1");
        assert_eq!(value["agentProfile"]["primary"]["reasoningEffort"], "xhigh");
        assert_eq!(value["agentProfile"]["oracle"]["provider"], "anthropic");
        assert_eq!(value["agentProfile"]["oracle"]["readOnly"], true);
    }

    #[test]
    fn oracle_and_cross_check_reviewers_never_share_the_primary_provider_family() {
        let providers = [
            ModelProvider::Anthropic,
            ModelProvider::OpenAi,
            ModelProvider::OpenAiCodex,
            ModelProvider::Google,
        ];
        let family = |name: &str| match name {
            "anthropic" => "anthropic",
            "openai" | "openai-codex" => "openai",
            "google" => "google",
            other => panic!("unknown provider {other}"),
        };
        for level in ["low", "medium", "high", "ultra"] {
            for provider in providers {
                let value = describe_mode(find_mode(level).unwrap(), provider);
                let profile = &value["agentProfile"];
                let primary = family(profile["primary"]["provider"].as_str().unwrap());
                let oracle = family(profile["oracle"]["provider"].as_str().unwrap());
                assert_ne!(
                    oracle, primary,
                    "{level} oracle shares the {primary} family for primary {provider:?}"
                );
                if matches!(level, "high" | "ultra") {
                    let reviewer = family(
                        profile["specialists"]["reviewer"]["provider"]
                            .as_str()
                            .unwrap(),
                    );
                    assert_ne!(
                        reviewer, primary,
                        "{level} reviewer shares the {primary} family for primary {provider:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn cross_provider_keeps_a_different_preferred_family_and_switches_a_shared_one() {
        use ModelProvider::{Anthropic, Google, OpenAi, OpenAiCodex};
        let all = [Anthropic, OpenAi, OpenAiCodex, Google];
        for preferred in all {
            for primary in all {
                let chosen = cross_provider(preferred, primary);
                assert_ne!(
                    provider_family(chosen),
                    provider_family(primary),
                    "preferred {preferred:?} with primary {primary:?} chose {chosen:?}"
                );
                if provider_family(preferred) != provider_family(primary) {
                    assert_eq!(
                        chosen, preferred,
                        "a preferred provider outside the primary family is kept"
                    );
                }
            }
        }
        assert_eq!(cross_provider(Anthropic, Anthropic), OpenAiCodex);
        assert_eq!(cross_provider(OpenAiCodex, OpenAi), Anthropic);
        assert_eq!(cross_provider(OpenAi, OpenAiCodex), Anthropic);
        assert_eq!(cross_provider(Google, Google), Anthropic);
    }

    #[test]
    fn profile_second_opinions_are_pinned_for_representative_primaries() {
        let oracle = |level: &str, provider: ModelProvider| {
            describe_mode(find_mode(level).unwrap(), provider)["agentProfile"]["oracle"]["provider"]
                .as_str()
                .unwrap()
                .to_string()
        };
        let reviewer = |level: &str, provider: ModelProvider| {
            describe_mode(find_mode(level).unwrap(), provider)["agentProfile"]["specialists"]
                ["reviewer"]["provider"]
                .as_str()
                .unwrap()
                .to_string()
        };
        // Unchanged where the level's preference already differs from the primary.
        assert_eq!(oracle("high", ModelProvider::OpenAiCodex), "anthropic");
        assert_eq!(reviewer("high", ModelProvider::OpenAiCodex), "anthropic");
        assert_eq!(oracle("ultra", ModelProvider::Anthropic), "openai-codex");
        assert_eq!(reviewer("ultra", ModelProvider::Anthropic), "openai-codex");
        // Switched where the preference used to share the primary's family.
        assert_eq!(oracle("medium", ModelProvider::Anthropic), "openai-codex");
        assert_eq!(oracle("high", ModelProvider::Anthropic), "openai-codex");
        assert_eq!(reviewer("high", ModelProvider::Anthropic), "openai-codex");
        assert_eq!(oracle("low", ModelProvider::OpenAi), "anthropic");
        assert_eq!(oracle("ultra", ModelProvider::OpenAiCodex), "anthropic");
        assert_eq!(reviewer("ultra", ModelProvider::OpenAiCodex), "anthropic");
        // The Oracle stays read-only and on the top tier whichever family wins.
        for provider in [ModelProvider::Anthropic, ModelProvider::OpenAiCodex] {
            let value = describe_mode(find_mode("high").unwrap(), provider);
            assert_eq!(value["agentProfile"]["oracle"]["readOnly"], true);
            assert_eq!(value["agentProfile"]["oracle"]["reasoningEffort"], "high");
        }
    }

    #[test]
    fn hidden_modes_are_not_in_default_list_source() {
        assert!(!find_mode("frontier").unwrap().visible);
        assert!(!find_mode("replay").unwrap().visible);
        assert_eq!(MODES.iter().filter(|mode| mode.visible).count(), 8);
    }

    #[tokio::test]
    async fn modes_accept_global_flags_after_the_subcommand() {
        let args = [
            "describe",
            "high",
            "--profile",
            "local",
            "--config",
            "profile=local",
            "--no-session",
        ]
        .map(String::from);
        assert_eq!(run_modes(&args).await.unwrap(), 0);
    }
}
