//! Declared wrapper tools (`Tools::wrappers`): which wrapper a name is,
//! what it routes to, which catalog tools are reachable undeclared, and the
//! texts that name the tool the model finds tools with.

use super::*;

impl<L, M, T, E, S, C> Engine<L, M, T, E, S, C>
where
    L: Log,
    M: Model,
    T: Tools,
    E: Effects,
    S: Sanitizer,
    C: Compactor,
{
    /// The declared wrapper named `name`.
    pub(super) fn wrapper(&self, name: &ToolName) -> Option<&crate::ToolWrapper> {
        self.tools
            .wrappers()
            .iter()
            .find(|wrapper| &wrapper.spec.name == name)
    }

    /// Whether a declared wrapper routes to `name`, which makes it callable.
    pub(super) fn routed_by_wrapper(&self, name: &ToolName) -> bool {
        self.tools
            .wrappers()
            .iter()
            .any(|wrapper| wrapper.routes.iter().any(|route| &route.tool == name))
    }

    /// Whether the model may call catalog tool `spec` this step: it is core,
    /// a search exposed it in this thread, a declared wrapper routes to it,
    /// or `act` reaches it without a search.
    pub(super) fn reachable(&self, ctx: &Context, spec: &ToolSpec) -> bool {
        spec.core
            || ctx.exposed_tools().contains(&spec.name)
            || self.routed_by_wrapper(&spec.name)
            || self.act_operation(&spec.name)
    }

    /// A call still named as `tools.call` or a wrapper after unwrapping: its
    /// arguments named no catalog tool, so it is refused with what to fix.
    pub(super) fn unrouted(&self, name: &ToolName) -> bool {
        name.as_str() == TOOLS_CALL || self.wrapper(name).is_some()
    }

    /// Whether a declared wrapper routes to `tools.search`, and so is
    /// declared in its place.
    pub(super) fn search_wrapped(&self) -> bool {
        self.searcher() != TOOLS_SEARCH
    }

    /// The name the model finds tools with: the wrapper routing to
    /// `tools.search`, or `tools.search` itself.
    fn searcher(&self) -> &str {
        self.tools
            .wrappers()
            .iter()
            .find(|wrapper| {
                wrapper
                    .routes
                    .iter()
                    .any(|route| route.tool.as_str() == TOOLS_SEARCH)
            })
            .map_or(TOOLS_SEARCH, |wrapper| wrapper.spec.name.as_str())
    }

    /// `tools.call`, naming the tool the model finds tools with.
    pub(super) fn call_spec(&self) -> ToolSpec {
        call_spec(self.searcher())
    }

    pub(super) fn unknown_tool(&self, name: &ToolName) -> ToolResult {
        ToolResult::error(format!(
            "unknown tool: {name}; use {} to find tools",
            model_tool_name(self.searcher()),
        ))
    }
}
