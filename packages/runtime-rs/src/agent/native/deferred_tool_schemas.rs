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
            (profile.includes(name) || explicitly_allowed).then_some(name.clone())
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
    external_tools: &HashSet<String>,
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
    let mut deferred_external_tools = external_tools
        .iter()
        .filter(|name| !active_tool_names.contains(*name))
        .filter_map(|name| tools.get(name))
        .filter(|definition| {
            tool_is_visible_to_model(&definition.tool.name, goal_tools_visible, include_ide_tools)
        })
        .map(|definition| {
            let description = definition
                .tool
                .description
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let mut characters = description.chars();
            let compact = characters.by_ref().take(160).collect::<String>();
            let compact = if characters.next().is_some() {
                format!("{compact}…")
            } else {
                compact
            };
            (definition.tool.name.clone(), compact)
        })
        .collect::<Vec<_>>();
    deferred_external_tools.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    if deferred_external_tools.is_empty() {
        return definitions;
    }
    let Some(search) = definitions
        .iter_mut()
        .find(|definition| definition.tool.name.eq_ignore_ascii_case("tool_search"))
    else {
        return definitions;
    };
    search
        .tool
        .description
        .push_str("\nDeferred caller tools (activate by exact name):");
    for (name, description) in deferred_external_tools {
        search
            .tool
            .description
            .push_str(&format!("\n- {name}: {description}"));
    }
    definitions
}
