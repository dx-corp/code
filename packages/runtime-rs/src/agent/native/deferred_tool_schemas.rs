//! Deferred caller-tool schema projection.

use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ToolProfile {
    Fast,
    Minimal,
    All,
    Review,
    Explore,
}

impl ToolProfile {
    pub(super) fn from_env() -> Self {
        match std::env::var("MAESTRO_TOOL_PROFILE")
            .ok()
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("all" | "full") => Self::All,
            Some("minimal") => Self::Minimal,
            Some("review") => Self::Review,
            Some("explore") => Self::Explore,
            _ => Self::Fast,
        }
    }

    pub(super) fn includes(self, name: &str) -> bool {
        if self == Self::All {
            return true;
        }
        let name = name.to_ascii_lowercase();
        if name == agent_codemode::TOOL_NAME {
            return true;
        }
        let names: &[&str] = match self {
            Self::Minimal => &[
                "read",
                "bash",
                "edit",
                "write",
                "grep",
                "glob",
                "tool_search",
                "ask_user",
            ],
            Self::Fast => &[
                "bash",
                "read",
                "write",
                "edit",
                "glob",
                "grep",
                "find",
                "list",
                "search",
                "parallel_ripgrep",
                "diff",
                "status",
                "background_tasks",
                "todo",
                "ask_user",
                "get_goal",
                "update_goal",
                "get_harness_context",
                "propose_harness_refinement",
                "apply_harness_refinement",
                "reject_harness_refinement",
                "get_mailbox",
                "send_mailbox",
                "read_mailbox",
                "ack_mailbox",
                "compact_mailbox",
                "tool_search",
                "recall_output",
                "explore",
            ],
            Self::Review => &[
                "read",
                "grep",
                "find",
                "list",
                "search",
                "parallel_ripgrep",
                "diff",
                "status",
                "tool_search",
                "recall_output",
                "explore",
            ],
            Self::Explore => &[
                "read",
                "glob",
                "grep",
                "find",
                "list",
                "search",
                "parallel_ripgrep",
                "diff",
                "status",
                "tool_search",
                "recall_output",
                "explore",
            ],
            Self::All => &[],
        };
        names.contains(&name.as_str())
    }
}

fn is_rlm_context_tool(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "get_rlm_context"
            | "set_rlm_context"
            | "append_rlm_context"
            | "render_rlm_context"
            | "clear_rlm_context"
    )
}

pub(super) fn tool_search_profile_allows(
    profile: ToolProfile,
    name: &str,
    explicitly_allowed_tools: &HashSet<String>,
) -> bool {
    !matches!(profile, ToolProfile::Fast | ToolProfile::Minimal)
        || !is_rlm_context_tool(name)
        || explicitly_allowed_tools.contains(&name.to_ascii_lowercase())
}

/// Controls whether caller-owned tool schemas ship on the first request.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ExternalToolSchemaPolicy {
    /// Preserve the embedding contract by exposing caller tools immediately.
    #[default]
    Eager,
    /// Register caller tools for `tool_search` and expose schemas on demand.
    Deferred,
}

pub(super) fn initial_active_tool_names(
    profile: ToolProfile,
    tools: &HashMap<String, ToolDefinition>,
    external_tools: &HashSet<String>,
    explicit_allowed_tools: Option<&HashSet<String>>,
    external_tool_schema_policy: ExternalToolSchemaPolicy,
) -> HashSet<String> {
    tools
        .keys()
        .filter_map(|name| {
            let explicitly_allowed = explicit_allowed_tools
                .is_some_and(|allowed| allowed.contains(&name.to_ascii_lowercase()));
            let deferred_external = external_tool_schema_policy
                == ExternalToolSchemaPolicy::Deferred
                && external_tools.contains(name);
            (profile.includes(name) && (!deferred_external || profile == ToolProfile::All)
                || explicitly_allowed && !deferred_external)
                .then_some(name.clone())
        })
        .chain(
            (external_tool_schema_policy == ExternalToolSchemaPolicy::Eager)
                .then_some(external_tools)
                .into_iter()
                .flatten()
                .cloned(),
        )
        .collect()
}

pub(super) fn effective_tool_definitions(
    tools: &HashMap<String, ToolDefinition>,
    active_tool_names: &HashSet<String>,
    _external_tools: &HashSet<String>,
    goal_tools_visible: bool,
    include_ide_tools: bool,
) -> Vec<ToolDefinition> {
    let mut definitions = tools
        .values()
        .filter(|definition| {
            let name = definition.tool.name.as_str();
            active_tool_names.contains(&name.to_ascii_lowercase())
                && tool_is_visible_to_model(name, goal_tools_visible, include_ide_tools)
        })
        .cloned()
        .collect::<Vec<_>>();
    definitions.sort_unstable_by(|left, right| left.tool.name.cmp(&right.tool.name));
    for definition in &mut definitions {
        definition.tool = compact_tool_for_model(definition.tool.clone());
    }
    let has_codemode = definitions
        .iter()
        .any(|definition| definition.tool.name == agent_codemode::TOOL_NAME);
    if let Some(search) = definitions
        .iter_mut()
        .find(|definition| definition.tool.name.eq_ignore_ascii_case("tool_search"))
    {
        if has_codemode {
            search.tool.description.push_str("\nSearch the admitted catalog by query or exact names. Results are bounded; retrieve exact schemas with codemode getToolSchema and call tools.<name>(args). Discovery grants no execution authority.");
        } else {
            search.tool.description.push_str("\nSearch by query or exact names to load matching schemas on the next provider request.");
        }
    }
    definitions
}

/// Bound newly discovered direct schemas independently of the eager profile.
/// The full admitted catalog remains available through the script envelope.
pub(super) fn discovery_budget_allows(
    tools: &HashMap<String, ToolDefinition>,
    initial: &HashSet<String>,
    active: &HashSet<String>,
    name: &str,
) -> bool {
    if active.contains(name) {
        return true;
    }
    let discovered = active.difference(initial).collect::<Vec<_>>();
    if discovered.len() >= 16 {
        return false;
    }
    let bytes = |name: &str| {
        tools
            .get(name)
            .and_then(|definition| serde_json::to_vec(&definition.tool).ok())
            .map_or(usize::MAX, |value| value.len())
    };
    let used = discovered
        .iter()
        .fold(0usize, |used, name| used.saturating_add(bytes(name)));
    used.saturating_add(bytes(name)) <= 65_536
}

impl NativeAgentRunner {
    pub(super) fn replace_governed_tools(
        &mut self,
        allowed_tools: &HashSet<String>,
        external_tool_definitions: Vec<ToolDefinition>,
    ) {
        let mut tools = self
            .tool_executor
            .tool_definitions()
            .into_iter()
            .filter(|definition| allowed_tools.contains(&definition.tool.name.to_ascii_lowercase()))
            .map(|definition| {
                (
                    definition.tool.name.to_ascii_lowercase(),
                    definition.clone(),
                )
            })
            .collect::<HashMap<_, _>>();
        codemode::register(&mut tools, Some(allowed_tools));
        classifier::register(
            &mut tools,
            Some(allowed_tools),
            self.client.is_some() && !self.model_route.uses_app_server(),
        );
        let external_tools = external_tool_definitions
            .iter()
            .map(|definition| definition.tool.name.to_ascii_lowercase())
            .collect::<HashSet<_>>();
        for definition in external_tool_definitions {
            tools.insert(definition.tool.name.to_ascii_lowercase(), definition);
        }
        self.active_tool_names = initial_active_tool_names(
            self.tool_profile,
            &tools,
            &external_tools,
            Some(allowed_tools),
            self.config.external_tool_schema_policy,
        );
        self.explicitly_allowed_tools = allowed_tools.clone();
        *self
            .discovery_catalog_cache
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        self.tools = tools;
        self.external_tools = external_tools;
        self.model_tool_cache = None;
        self.refresh_runtime_audit();
    }

    /// Start discovery lifetime at prompt admission, outside provider retries.
    pub(super) fn begin_tool_discovery_turn(&mut self) {
        // Resolve consent only at the safe user-turn boundary; retries keep this assignment.
        let assignment = self
            .tool_executor
            .experiment_assignment(&self.config.model)
            .filter(|a| a.is_valid());
        let selected = assignment
            .as_ref()
            .map_or(self.baseline_tool_profile, |a| match a.arm {
                maestro_runtime_contracts::experiments::ExperimentArm::Control => ToolProfile::Fast,
                maestro_runtime_contracts::experiments::ExperimentArm::Minimal => {
                    ToolProfile::Minimal
                }
            });
        *self
            .discovery_catalog_cache
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        // A fresh prompt clears discovery; retries and Continue retain it.
        self.tool_profile = selected;
        let mut initial = initial_active_tool_names(
            selected,
            &self.tools,
            &self.external_tools,
            Some(&self.explicitly_allowed_tools),
            self.config.external_tool_schema_policy,
        );
        // Conversational tools cannot be nested in codemode. Codex registers
        // this admitted direct tool once, including Review/Explore profiles.
        if self.model_route.uses_app_server() && self.tools.contains_key("ask_user") {
            initial.insert("ask_user".into());
        }
        if self.active_tool_names != initial || self.experiment_assignment != assignment {
            self.active_tool_names = initial;
            self.model_tool_cache = None;
            self.refresh_runtime_audit();
        }
        self.experiment_assignment = assignment;
    }
}

pub(super) fn validate_deferred_discovery(
    policy: ExternalToolSchemaPolicy,
    has_external: bool,
    has_tool: impl Fn(&str) -> bool,
) -> Result<()> {
    if policy == ExternalToolSchemaPolicy::Deferred && has_external {
        for name in ["tool_search", agent_codemode::TOOL_NAME] {
            if !has_tool(name) {
                bail!(
                    "Deferred external tool schemas require admitted `{name}` for bounded discovery and execution; use Eager for legacy embeddings"
                );
            }
        }
    }
    Ok(())
}
