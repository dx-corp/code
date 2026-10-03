//! Validate immutable tool ownership before registering agent tools.

use super::*;

pub(super) fn validate_tools_with_host(
    host: &NativeExecutionHostHandle,
    allowed_tools: Option<&HashSet<String>>,
    external_tool_definitions: &[ToolDefinition],
) -> Result<()> {
    if let Some(allowed_tools) = allowed_tools {
        for name in allowed_tools {
            let normalized = name.to_ascii_lowercase();
            if normalized != agent_codemode::TOOL_NAME
                && (!host.has_native_tool(&normalized) || host.is_reserved_tool(name))
            {
                return Err(anyhow::anyhow!("Unknown allowed tool `{name}`"));
            }
        }
    }
    let native_names = host
        .tool_definitions()
        .iter()
        .map(|definition| definition.tool.name.to_ascii_lowercase())
        .collect::<HashSet<_>>();
    let mut external_names = HashSet::new();
    for definition in external_tool_definitions {
        let name = definition.tool.name.trim().to_ascii_lowercase();
        if name.is_empty() {
            return Err(anyhow::anyhow!("External tool name must not be empty"));
        }
        if name == agent_codemode::TOOL_NAME
            || native_names.contains(&name)
            || host.is_mcp_tool(&name)
            || host.is_reserved_tool(&name)
        {
            return Err(anyhow::anyhow!(
                "External tool name `{name}` collides with a host, MCP, or reserved tool"
            ));
        }
        if !external_names.insert(name.clone()) {
            return Err(anyhow::anyhow!(
                "Ambiguous external tool name `{name}` has multiple owners"
            ));
        }
    }
    Ok(())
}
