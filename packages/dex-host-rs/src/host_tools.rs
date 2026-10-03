//! The `Tools` port over a Maestro `NativeExecutionHost`.
//!
//! Every tool the host registers (bash, file edits, search, MCP, inline
//! tools) is offered to the dex-loop kernel as it is. The host still owns
//! execution, hooks, sandbox and the action firewall; this module only
//! translates. Approval and read-only classification call the native actor's
//! own functions (`maestro_runtime::agent::loop_policy`), so a call is gated
//! the same way on both loops.
//!
//! A call that would have prompted under the native actor does not run. Its
//! result is a `needs_confirmation` preview; the model asks the person with
//! `user.ask` bound to that preview, and once they choose Confirm it calls
//! again with `confirmation` set to the preview's call id. That is the
//! kernel's one human gate (see `dex-tools`' `confirm` module, which this
//! mirrors for Maestro's tools).

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use dex_loop::{
    ApprovalId, CallId, CancellationToken, Context, ExecutorKind, GovernanceClass, PrincipalId,
    ProposedCall, ThreadId, ToolName, ToolResult, ToolSpec, Tools, Verdict, args_digest,
};
use maestro_runtime::agent::loop_policy;
use maestro_runtime::agent::native_host::{
    ApprovalMode, NativeExecutionHostHandle, NativeFirewallVerdict, NativeHookResult,
    NativeToolExecutionOptions,
};
use maestro_runtime::agent::workflow_state::{WorkflowStateTracker, apply_workflow_state_hooks};
use maestro_runtime_contracts::contracts::ToolOutcome;
use serde_json::{Map, Value, json};

/// The kernel's question tool. The engine parks on it (`ExecutorKind::User`)
/// and binds a `confirmation` reference to the preview it names. Here it only
/// confirms a previewed action: Maestro's own `ask_user` stays the way to ask
/// an open question, which the person answers in their next message.
pub const USER_ASK: &str = "user.ask";

/// The argument that carries a confirmed preview's call id.
pub const CONFIRMATION_FIELD: &str = "confirmation";

/// Longest string argument value shown in a preview.
const PREVIEW_VALUE_CHARS: usize = 400;

/// Refusal for a gated call in a turn nobody is watching.
pub const HEADLESS_GATED: &str =
    "this action needs a person's confirmation and nobody is attached to this turn";

fn user_ask() -> ToolSpec {
    ToolSpec {
        name: ToolName::new(USER_ASK),
        label: "Confirm an action".into(),
        description: "Ask the person to confirm one previewed action and wait for their choice. Only for a needs_confirmation preview; ask open questions with ask_user.".into(),
        schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["question", CONFIRMATION_FIELD],
            "properties": {
                "question": {"type": "string", "minLength": 1, "maxLength": 2000},
                CONFIRMATION_FIELD: {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["proposal_call_id"],
                    "properties": {
                        "proposal_call_id": {"type": "string", "minLength": 1, "maxLength": 256},
                    },
                },
            },
        }),
        read_only: true,
        core: true,
        governance: GovernanceClass::Plain,
        executor: ExecutorKind::User,
    }
}

/// Adds the optional confirmation reference to a mutation's input schema, so
/// a strict schema still admits it.
fn declare_confirmation(schema: &mut Value) {
    let Some(root) = schema.as_object_mut() else {
        return;
    };
    let properties = root
        .entry("properties")
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(properties) = properties.as_object_mut() {
        properties.insert(
            CONFIRMATION_FIELD.to_owned(),
            json!({
                "type": "string",
                "description": "Only after the person chose Confirm in a user.ask question bound to this action's preview: that preview's proposal_call_id. Single use."
            }),
        );
    }
}

/// The arguments without `confirmation`: what the host executes.
fn strip(args: &Value) -> Value {
    match args {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| key.as_str() != CONFIRMATION_FIELD)
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn truncate(value: &Value) -> Value {
    match value {
        Value::String(text) if text.chars().count() > PREVIEW_VALUE_CHARS => {
            let cut: String = text.chars().take(PREVIEW_VALUE_CHARS).collect();
            Value::String(format!(
                "{cut}... [{} characters in all]",
                text.chars().count()
            ))
        }
        Value::Array(items) => Value::Array(items.iter().map(truncate).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), truncate(value)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The `needs_confirmation` document a gated call returns. `Context` records
/// it as a preview only when `args_digest` matches the call's arguments.
fn preview(spec: &ToolSpec, call: &ProposedCall, reason: Option<&str>) -> String {
    let arguments = strip(&call.args);
    json!({
        "status": "needs_confirmation",
        "tool": spec.name.as_str(),
        "action": spec.label,
                "reason": reason,
        "arguments": truncate(&arguments),
        "args_digest": args_digest(&arguments),
        "proposal_call_id": call.id.as_str(),
        "instructions": format!(
            "Nothing was done. Call {USER_ASK} with a plain question showing the action ({}) and its key details, and confirmation={{\"proposal_call_id\":\"{}\"}}. The person must choose Confirm in that question. Ordinary chat replies do not authorize execution. After Confirm, call this tool with exactly the same arguments and confirmation=\"{}\". Changed arguments need a new preview and question; every decision is single use.",
            spec.label, call.id, call.id
        ),
    })
    .to_string()
}

fn approval_id(call: &CallId) -> ApprovalId {
    ApprovalId::new(format!("approval-{call}"))
}

/// A `user.ask` call must carry an exact-action reference naming an
/// unexecuted preview, by the same person, not already asked about. Unlike `dex-tools`,
/// a read can be gated too (the firewall holds some reads), so a read's
/// preview is confirmable.
fn question_binding(ctx: &Context, ask: &ProposedCall, tools: &HostTools) -> Result<(), String> {
    let reference = ask
        .args
        .get(CONFIRMATION_FIELD)
        .ok_or("user.ask only confirms a previewed action; ask open questions with ask_user")?;
    let id = reference
        .get("proposal_call_id")
        .and_then(Value::as_str)
        .ok_or("confirmation requires proposal_call_id")?;
    let prior = ctx
        .action_preview(&CallId::new(id))
        .ok_or("confirmation proposal is unavailable")?;
    let spec = tools
        .spec(&prior.tool)
        .ok_or("confirmation tool is unavailable")?;
    if !ctx.is_unexecuted_preview(&prior.id) {
        return Err("confirmation requires a preview before execution".into());
    }
    if prior.principal != ask.principal {
        return Err("confirmation principal does not match".into());
    }
    if ctx.confirmation_question_exists(&prior.id) {
        return Err("confirmation proposal already has a question; create a fresh preview".into());
    }
    if ctx.action_question_text(&prior.id, &spec.label).is_none() {
        return Err(
            "confirmation details cannot fit the chat question; split or reduce the action".into(),
        );
    }
    Ok(())
}

fn render(outcome: &ToolOutcome) -> ToolResult {
    match outcome {
        ToolOutcome::Succeeded { output } => ToolResult::text(output.as_str()),
        ToolOutcome::Failed {
            error,
            partial_output,
        } => match partial_output {
            Some(partial) if !partial.as_str().is_empty() => {
                ToolResult::error(format!("{}\n\n{}", error.message(), partial.as_str()))
            }
            _ => ToolResult::error(error.message()),
        },
        ToolOutcome::Denied { reason } => {
            ToolResult::error(format!("denied: {}", reason.message()))
        }
        ToolOutcome::Cancelled { .. } => ToolResult::error("cancelled"),
        ToolOutcome::Indeterminate { reason } => ToolResult::unknown(reason.clone()),
    }
}

/// The `Tools` port over one Maestro execution host.
#[derive(Clone)]
pub struct HostTools {
    host: NativeExecutionHostHandle,
    mode: ApprovalMode,
    catalog: Arc<[ToolSpec]>,
    workflow: Arc<Mutex<WorkflowStateTracker>>,
}

impl HostTools {
    /// Offers every tool `host` registers, gated as the native actor gates
    /// them under `mode`.
    pub fn new(host: NativeExecutionHostHandle, mode: ApprovalMode) -> Self {
        let mut catalog = Vec::new();
        for definition in host.tool_definitions() {
            let name = definition.tool.name.clone();
            if name == USER_ASK {
                continue;
            }
            let annotations = host.tool_annotations(&name);
            let read_only = loop_policy::parallel_read_only(
                &name,
                definition.requires_approval,
                annotations.as_ref(),
                host.is_explicit_inline_read_only_tool(&name),
            );
            let mut schema = definition.tool.input_schema.clone();
            declare_confirmation(&mut schema);
            catalog.push(ToolSpec {
                name: ToolName::new(&name),
                label: name.clone(),
                description: definition.tool.description.clone(),
                schema,
                read_only,
                core: true,
                governance: if definition.requires_approval {
                    GovernanceClass::Approval
                } else {
                    GovernanceClass::Plain
                },
                executor: ExecutorKind::InProcess,
            });
        }
        catalog.push(user_ask());
        Self {
            host,
            mode,
            catalog: Arc::from(catalog),
            workflow: Arc::new(Mutex::new(WorkflowStateTracker::default())),
        }
    }

    fn firewall(&self, name: &str, args: &Value) -> NativeFirewallVerdict {
        let snapshot = self
            .workflow
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .snapshot();
        loop_policy::firewall_verdict(
            &self.host,
            name,
            args,
            &snapshot,
            self.host.tool_annotations(name).as_ref(),
            false,
        )
    }
}

impl Tools for HostTools {
    fn catalog(&self) -> &[ToolSpec] {
        &self.catalog
    }

    async fn search(&self, _principal: &PrincipalId, query: &str) -> Vec<ToolName> {
        let query = query.to_ascii_lowercase();
        if query.trim().is_empty() {
            return Vec::new();
        }
        self.catalog
            .iter()
            .filter(|spec| {
                spec.name.as_str().to_ascii_lowercase().contains(&query)
                    || spec.description.to_ascii_lowercase().contains(&query)
            })
            .map(|spec| spec.name.clone())
            .collect()
    }

    async fn policy(&self, ctx: &Context, call: &ProposedCall) -> Verdict {
        let Some(spec) = self.spec(&call.tool) else {
            return Verdict::Deny(format!("unknown tool: {}", call.tool));
        };
        if call.tool.as_str() == USER_ASK {
            return match question_binding(ctx, call, self) {
                Ok(()) => Verdict::Allow,
                Err(reason) => Verdict::Deny(reason),
            };
        }
        let name = call.tool.as_str();
        let args = strip(&call.args);
        let missing = self.host.missing_required(name, &args);
        if !missing.is_empty() {
            return Verdict::Deny(format!(
                "missing required arguments: {}",
                missing.join(", ")
            ));
        }
        let firewall = self.firewall(name, &args);
        if let NativeFirewallVerdict::Block { reason } = &firewall {
            return Verdict::Deny(reason.clone());
        }
        let required =
            loop_policy::approval_required(self.mode, false, &firewall, &self.host, name, &args);
        if !required {
            return Verdict::Allow;
        }
        if ctx.approval_mode() == dex_loop::ApprovalMode::Headless {
            return Verdict::Deny(HEADLESS_GATED.into());
        }
        if ctx.confirmed_action(call) {
            return Verdict::Confirmed {
                approval: approval_id(&call.id),
                summary: format!(
                    "decision=policy_approved_after_action_confirmation; {}",
                    spec.label
                ),
            };
        }
        let reason = match &firewall {
            NativeFirewallVerdict::RequireApproval { reason } => Some(reason.as_str()),
            _ => None,
        };
        Verdict::NeedsConfirmation {
            preview: preview(spec, call, reason),
        }
    }

    async fn run(
        &self,
        _thread: &ThreadId,
        call: &ProposedCall,
        cancel: &CancellationToken,
    ) -> ToolResult {
        let name = call.tool.as_str();
        let call_id = call.id.as_str();
        let mut args = strip(&call.args);
        match self.host.hook_pre_tool_use(name, call_id, &args).await {
            NativeHookResult::Block { reason } => {
                return ToolResult::error(format!("blocked by hook: {reason}"));
            }
            NativeHookResult::ModifyInput { new_input } => args = new_input,
            NativeHookResult::Continue | NativeHookResult::InjectContext { .. } => {}
        }
        let started = Instant::now();
        let execution = self
            .host
            .execute_tool(
                name,
                &args,
                None,
                call_id,
                NativeToolExecutionOptions {
                    cancel: cancel.clone(),
                    approved_inline_env: None,
                },
            )
            .await;
        let result = render(&execution.outcome);
        let is_error = result.outcome != dex_loop::Outcome::Succeeded;
        // A PII tracking error leaves the tracker as it was; the firewall
        // keeps judging later calls against the last good snapshot.
        let _ = apply_workflow_state_hooks(
            name,
            call_id,
            &args,
            &mut self.workflow.lock().unwrap_or_else(PoisonError::into_inner),
            is_error,
        );
        let output = match &result.output {
            dex_loop::Output::Text(text) => text.as_str(),
            _ => "",
        };
        let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if let NativeHookResult::Block { reason } = self
            .host
            .hook_post_tool_use(name, call_id, &args, output, is_error, elapsed)
            .await
        {
            return ToolResult::error(format!("{output}\n\nblocked by hook: {reason}"));
        }
        result
    }
}
