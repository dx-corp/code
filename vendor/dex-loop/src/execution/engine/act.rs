//! `act`: the one way the model changes something, in two calls. A preview
//! names the operation and its arguments, runs nothing, and returns the plan
//! with a `plan_digest`. A submit repeats the operation and arguments with
//! that digest, after a preview of them in the same thread; it is unwrapped
//! into the operation itself when proposed, as
//! a `tools.call` is, so policy, the operation's own governance class,
//! approval, receipts and surfaces see the operation and never `act`.
//!
//! A host opts in with [`Tools::act`]. With it on, `tools.call` refuses a
//! change and points to `act`, so a found change always has a preview. The
//! host's [`Tools::act_operations`] are the changes `act` reaches without a
//! search: its description names them, and they are not declared on their
//! own.

use super::*;
use crate::{ActOperation, Outcome, Output};

/// The engine-owned change tool. Declared when the host opts in.
pub const ACT: &str = "act";

const PREVIEW: &str = "preview";
const SUBMIT: &str = "submit";

/// The digest a submit must carry: the operation and its exact arguments.
pub fn plan_digest(op: &ToolName, args: &serde_json::Value) -> String {
    crate::args_digest(&serde_json::json!({"op": op.as_str(), "args": args}))
}

/// `act`'s description: how to use it, then the operations it reaches
/// without a search.
fn description(operations: &[ActOperation]) -> String {
    let mut description = String::from(
        "Make a change through one operation: one listed here, or one that search found. \
         First call with phase \"preview\", op (the operation's name) and args (its input): \
         nothing runs, and the result shows what will run, whether a person must approve it, \
         and a plan_digest. Then call with phase \"submit\", the same op and args unchanged, \
         and that plan_digest. The change runs as that operation, with its own authorization, \
         approval and receipt. Changed arguments need a new preview. A preview whose args do \
         not fit returns the operation's description and input schema; preview again with \
         args that fit. Reads do not go through act.",
    );
    if !operations.is_empty() {
        description.push_str("\n\nOperations, with no search first:");
        for operation in operations {
            description.push_str(&format!(
                "\n- {}: {}",
                model_tool_name(operation.tool.as_str()),
                operation.summary
            ));
        }
    }
    description
}

fn spec(operations: &[ActOperation]) -> ToolSpec {
    ToolSpec {
        name: ToolName::new(ACT),
        label: "Proposing a change".into(),
        description: description(operations),
        schema: serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["op", "args", "phase"],
            "properties": {
                "op": {"type": "string", "minLength": 1, "description": "The operation's name"},
                "args": {"type": "object", "description": "The operation's input, matching its schema"},
                "phase": {"type": "string", "enum": [PREVIEW, SUBMIT]},
                "plan_digest": {"type": "string", "description": "From the preview; required to submit"}
            }
        }),
        // The `act` call itself never changes anything: a preview only
        // reads specs, and a submit runs as the operation it names.
        read_only: true,
        core: true,
        governance: GovernanceClass::Plain,
        executor: ExecutorKind::InProcess,
    }
}

/// Whether this thread's history holds a successful `act` result containing
/// `needle`.
fn act_history_has(ctx: &Context, needle: &str) -> bool {
    ctx.history().iter().any(|entry| {
        matches!(
            &entry.message,
            crate::Message::Tool {
                name,
                outcome: Outcome::Succeeded,
                output: Output::Text(text),
                ..
            } if name.as_str() == ACT && text.contains(needle)
        )
    })
}

/// Whether this thread's history holds a successful preview of `digest`.
/// A digest is deterministic, so the preview, not the digest alone, is what
/// lets a submit run; a compaction that drops the preview asks for another.
fn previewed(ctx: &Context, digest: &str) -> bool {
    act_history_has(ctx, &format!("\"plan_digest\":\"{digest}\""))
}

fn governance_note(class: GovernanceClass) -> &'static str {
    match class {
        GovernanceClass::Plain => "runs on submit",
        GovernanceClass::Guardrails => "runs on submit after its guardrail checks",
        GovernanceClass::Guardian => "runs on submit after an independent review",
        GovernanceClass::Approval => "a person approves it after submit, before it runs",
    }
}

impl<L, M, T, E, S, C> Engine<L, M, T, E, S, C>
where
    L: Log,
    M: Model,
    T: Tools,
    E: Effects,
    S: Sanitizer,
    C: Compactor,
{
    /// `act`'s spec when the host declares it.
    pub(super) fn act_spec(&self) -> Option<ToolSpec> {
        self.tools.act().then(|| spec(self.tools.act_operations()))
    }

    /// Whether `act` reaches `name` without a search.
    pub(super) fn act_operation(&self, name: &ToolName) -> bool {
        self.tools.act()
            && self
                .tools
                .act_operations()
                .iter()
                .any(|operation| &operation.tool == name)
    }

    /// The act operations in this turn's catalog, for a script to call by
    /// their own names as it calls a wrapper's routed tools.
    pub(super) fn act_operation_specs(&self) -> impl Iterator<Item = &ToolSpec> {
        self.tools
            .act_operations()
            .iter()
            .filter(|_| self.tools.act())
            .filter_map(|operation| self.tools.spec(&operation.tool))
    }

    /// The catalog change `op` names, by registry or model name.
    fn act_target(&self, op: &str) -> Option<&ToolSpec> {
        self.tools
            .catalog()
            .iter()
            .find(|spec| spec.name.as_str() == op || model_tool_name(spec.name.as_str()) == op)
    }

    /// A submit whose digest matches, as the operation it names.
    pub(super) fn unwrap_act(
        &self,
        ctx: &Context,
        name: &ToolName,
        args: &serde_json::Value,
    ) -> Option<(ToolName, serde_json::Value)> {
        if name.as_str() != ACT || !self.tools.act() {
            return None;
        }
        if args.get("phase").and_then(serde_json::Value::as_str) != Some(SUBMIT) {
            return None;
        }
        let spec = self
            .act_target(args.get("op")?.as_str()?)
            .filter(|spec| !spec.read_only)?;
        let arguments = args.get("args").filter(|value| value.is_object())?;
        let digest = args.get("plan_digest")?.as_str()?;
        (digest == plan_digest(&spec.name, arguments) && previewed(ctx, digest))
            .then(|| (spec.name.clone(), arguments.clone()))
    }

    /// Whether a `tools.call` to `spec` must go through `act` instead.
    pub(super) fn redirects_to_act(&self, spec: &ToolSpec) -> bool {
        self.tools.act() && !spec.read_only
    }

    /// The refusal for a `tools.call` kept unwrapped because it names a
    /// change.
    pub(super) fn act_redirect(&self, call: &ProposedCall) -> Option<ToolResult> {
        let inner = call.args.get("name")?.as_str()?;
        let spec = self
            .act_target(inner)
            .filter(|spec| self.redirects_to_act(spec))?;
        Some(ToolResult::error(format!(
            "not executed: {} changes something; call {ACT} with phase \"preview\", op \"{}\" and these args, then submit the plan",
            model_tool_name(spec.name.as_str()),
            model_tool_name(spec.name.as_str()),
        )))
    }

    /// How a search result says to call what it found.
    pub(super) fn found_call_hint(&self) -> String {
        let call = format!(
            "Call each with {} {{\"name\": <name>, \"arguments\": <input matching its input_schema>}}",
            model_tool_name(TOOLS_CALL)
        );
        if self.tools.act() {
            format!(
                "{call} when it reads; when it changes something, preview it with {ACT} and then submit the plan."
            )
        } else {
            format!("{call}.")
        }
    }

    /// A preview, or a submit that could not be unwrapped.
    pub(super) async fn act_call(
        &self,
        ctx: &mut Context,
        call: &ProposedCall,
    ) -> Result<(), Fenced> {
        let spec = spec(self.tools.act_operations());
        self.emit(ctx, vec![started(call, &spec)]).await?;
        let result = self.act_result(ctx, call);
        self.finish(ctx, call, result).await
    }

    fn act_result(&self, ctx: &Context, call: &ProposedCall) -> ToolResult {
        if let Err(reason) = validate_args(&spec(&[]), &call.args) {
            return ToolResult::error(reason);
        }
        let op = call.args["op"].as_str().unwrap_or_default();
        let args = &call.args["args"];
        let Some(target) = self.act_target(op) else {
            return self.unknown_tool(&ToolName::new(op));
        };
        if target.read_only {
            return ToolResult::error(format!(
                "not executed: {} is a read; call it directly, not through {ACT}",
                model_tool_name(target.name.as_str())
            ));
        }
        let Some(target) = self.offered_spec(ctx, &target.name) else {
            return self.unknown_tool(&ToolName::new(op));
        };
        if let Err(reason) = validate_args(&target, args) {
            // An operation reached only through `act` was never declared and
            // no search returned it, so a misfit carries what a search result
            // would: its description and input schema.
            return ToolResult::error(format!(
                "{reason}; {}: {} Input schema: {}",
                model_tool_name(target.name.as_str()),
                target.description,
                target.schema
            ));
        }
        let digest = plan_digest(&target.name, args);
        if call.args["phase"] == SUBMIT {
            return ToolResult::error(
                "not executed: no preview in this conversation has this plan_digest for this op and args; preview them, then submit the returned plan unchanged",
            );
        }
        let name = model_tool_name(target.name.as_str());
        let plan = serde_json::json!({
            "preview": "nothing has run",
            "op": name,
            "label": target.label,
            "governance": format!("{:?}", target.governance),
            "on_submit": governance_note(target.governance),
            "plan_digest": digest,
            "submit": format!("call {ACT} with phase \"{SUBMIT}\", op \"{name}\", the same args unchanged, and this plan_digest"),
        });
        ToolResult::text(plan.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_description_names_each_operation_by_model_name_in_order() {
        let operations = [
            ActOperation {
                tool: ToolName::new("dex.make"),
                summary: "make it.".into(),
            },
            ActOperation {
                tool: ToolName::new("slack.react"),
                summary: "react.".into(),
            },
        ];
        let text = description(&operations);
        let make = text.find("\n- dex_make: make it.").expect("make named");
        let react = text.find("\n- slack_react: react.").expect("react named");
        assert!(make < react, "{text}");
        assert!(!description(&[]).contains("Operations"));
        // Only the description depends on the operations; the schema the
        // model fills does not.
        assert_eq!(spec(&operations).schema, spec(&[]).schema);
    }
}
