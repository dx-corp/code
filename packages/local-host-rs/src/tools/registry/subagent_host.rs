//! Preserve selected child-host dependencies when executor scope changes.

use super::*;

impl ToolExecutor {
    /// Keep child construction and role filtering on the parent agent's host.
    pub(crate) fn with_tool_host(
        mut self,
        tool_host: Arc<dyn crate::agent::tool_host::ToolHost>,
    ) -> Self {
        self.subagents = Arc::new((*self.subagents).clone().with_tool_host(tool_host));
        self
    }

    /// Supply child construction while retaining the shared lifecycle owner.
    pub fn with_child_agent_factory(
        mut self,
        factory: Arc<dyn crate::subagents::factory::ChildAgentFactory>,
    ) -> Self {
        self.subagents = Arc::new((*self.subagents).clone().with_child_agent_factory(factory));
        self
    }

    pub(crate) fn with_subagent_parent_scope(mut self, parent_scope_id: String) -> Self {
        let factory = self.subagents.child_agent_factory();
        let tool_host = self.subagents.tool_host();
        self.subagents = Arc::new(
            SubagentManager::with_parent_scope(PathBuf::from(&self.cwd), parent_scope_id)
                .with_tool_host(tool_host)
                .with_child_agent_factory(factory),
        );
        self
    }
}
