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

    /// `args` of a proposed call to `name` as the engine runs them, with
    /// what it interpreted (`crate::arguments`). A call a wrapper routed
    /// (`offered` is the wrapper's schema) leaves out fields only another
    /// route takes; every call accepts obvious variants of its tool's
    /// declared fields and values. A read also takes the one correction of
    /// arguments with exactly one candidate per problem; a change never does,
    /// so a suggestion for it is refused with the suggestion instead. Calls
    /// the engine handles itself, unknown tools and arguments the layer
    /// refuses keep the model's arguments, so dispatch reports what to fix.
    pub(super) fn interpret_arguments(
        &self,
        name: &ToolName,
        args: serde_json::Value,
        offered: Option<&serde_json::Value>,
    ) -> (serde_json::Value, Vec<String>) {
        if [TOOLS_SEARCH, TOOLS_CALL, CODEMODE, ACT].contains(&name.as_str())
            || self.wrapper(name).is_some()
        {
            return (args, Vec::new());
        }
        let Some(spec) = self.tools.spec(name) else {
            return (args, Vec::new());
        };
        let Some(object) = args.as_object() else {
            return (args, Vec::new());
        };
        let result = match offered {
            Some(offered) => crate::arguments::route(&spec.schema, offered, object),
            None => crate::arguments::normalize(&spec.schema, object),
        };
        let normalized = match result {
            Ok(normalized) => normalized,
            Err(refused) => match refused.corrected() {
                Some(corrected) if spec.read_only => corrected,
                _ => return (args, Vec::new()),
            },
        };
        if normalized.interpreted.is_empty() {
            return (args, Vec::new());
        }
        let notes = normalized
            .interpreted
            .iter()
            .map(ToString::to_string)
            .collect();
        (serde_json::Value::Object(normalized.args), notes)
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
    pub(super) fn searcher(&self) -> &str {
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
        call_spec(self.searcher(), self.tools.act())
    }

    pub(super) fn unknown_tool(&self, name: &ToolName) -> ToolResult {
        ToolResult::error(format!(
            "unknown tool: {name}; use {} to find tools",
            model_tool_name(self.searcher()),
        ))
    }
}

/// What the model is told `tools.call` does.
///
/// The description says what the engine refuses, so the model does not learn
/// it one refused step at a time (production, 7 days to 2026-11-18: 312
/// calls stayed named `tools.call`, every one refused before running). A
/// call that names a catalog tool is recorded as that tool, so the ones
/// still named `tools.call` are exactly the refusals: a name no catalog tool
/// has (a declared wrapper, or a script's `tools.<name>` form), arguments
/// that are not an object, or, with `act` on, a change.
pub(super) fn call_description(searcher: &str, act: bool) -> String {
    let run = model_tool_name(CODEMODE);
    let changes = if act {
        format!(
            " A tool that changes something is refused here: preview it with {ACT}, then \
             submit the plan."
        )
    } else {
        String::new()
    };
    format!(
        "Call a tool that {search} found and that is not one of your declared tools; \
                 request a declared tool directly. name: the tool's name exactly as the search \
                 result lists it, with no tools. prefix (tools.<name> exists only inside a {run} \
                 script); arguments: an object matching that tool's input_schema from the search \
                 result, {{}} when it takes none. The call runs as that tool, with its own \
                 authorization and receipt.{changes} An argument that does not fit the schema is \
                 returned with the field and the reason; correct it and call again.",
        search = model_tool_name(searcher),
    )
}

#[cfg(test)]
mod call_description_tests {
    use super::*;

    /// The call of a found tool says, beside the call, each thing the engine
    /// refuses in it: a declared tool's name, the script-only `tools.<name>`
    /// form, missing arguments and, where `act` is on, a change.
    #[test]
    fn the_found_tool_call_names_what_it_refuses() {
        let plain = call_description("search", false);
        for rule in [
            "Call a tool that search found and that is not one of your declared tools",
            "request a declared tool directly",
            "with no tools. prefix (tools.<name> exists only inside a run script)",
            "{} when it takes none",
        ] {
            assert!(plain.contains(rule), "lost {rule:?}: {plain}");
        }
        assert!(!plain.contains("preview it with"), "{plain}");
        let with_act = call_description(TOOLS_SEARCH, true);
        assert!(
            with_act.starts_with("Call a tool that tools_search found"),
            "{with_act}"
        );
        assert!(
            with_act.contains(
                "A tool that changes something is refused here: preview it with act, then submit \
                 the plan."
            ),
            "{with_act}"
        );
        // Sent on every step: keep the whole description small.
        assert!(with_act.len() / 4 <= 190, "{} bytes", with_act.len());
    }
}
