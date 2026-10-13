//! Child host inheritance and role-specific narrowing of its tool catalog.

use super::*;

impl SubagentManager {
    pub(crate) fn with_tool_host(
        mut self,
        tool_host: Arc<dyn crate::agent::tool_host::ToolHost>,
    ) -> Self {
        self.child_factory = Arc::new(LocalChildAgentFactory::new(Arc::clone(&tool_host)));
        self.tool_host = tool_host;
        self
    }

    pub(crate) fn tool_host(&self) -> Arc<dyn crate::agent::tool_host::ToolHost> {
        Arc::clone(&self.tool_host)
    }
}

const READ_ONLY_CHILD_TOOLS: [&str; 21] = [
    "read",
    "glob",
    "grep",
    "list",
    "diff",
    "status",
    "parallel_ripgrep",
    "search",
    "web_fetch",
    "websearch",
    "codesearch",
    "read_image",
    "screenshot",
    "vscode_get_diagnostics",
    "jetbrains_get_diagnostics",
    "vscode_get_definition",
    "jetbrains_get_definition",
    "vscode_find_references",
    "jetbrains_find_references",
    "mcp_list_resources",
    "mcp_read_resource",
];

pub(super) fn child_allowed_tools_for_role(
    tool_host: &dyn crate::agent::tool_host::ToolHost,
    role: SubagentRole,
    profile_tools: Option<&[String]>,
) -> HashSet<String> {
    let mut tools = tool_host
        .registry_tool_definitions()
        .into_iter()
        .filter(|definition| {
            let name = definition.tool.name.to_ascii_lowercase();
            let globally_allowed = !SUBAGENT_TOOL_NAMES.contains(&name.as_str())
                && !matches!(
                    name.as_str(),
                    "get_goal" | "update_goal" | "todo" | "background_tasks" | "coding_task"
                );
            let role_allowed = matches!(role, SubagentRole::Code)
                || READ_ONLY_CHILD_TOOLS.contains(&name.as_str());
            globally_allowed && role_allowed
        })
        .map(|definition| definition.tool.name.to_ascii_lowercase())
        .collect::<HashSet<_>>();

    if let Some(profile_tools) = profile_tools {
        let requested = profile_tools
            .iter()
            .map(|name| name.to_ascii_lowercase())
            .collect::<HashSet<_>>();
        tools.retain(|name| requested.contains(name));
    }
    tools
}
