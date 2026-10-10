//! Bounded repeated case reads; history and pending calls reserve the retained lane.
use crate::{Context, Message, ProposedCall, ToolResult};
pub const CONTEXT_POCKET_TOOL_NAME: &str = "dex.get_context_pocket";
pub const CONTEXT_POCKET_RESULT_BYTES: usize = 4096;
pub const CONTEXT_POCKET_BYTES: usize = 16384;

const STOP: &str = "Case context is partial; its retained byte allowance is exhausted or compacted. Continue in a fresh bounded context.";
fn encoded_size(call: &ProposedCall, outcome: crate::Outcome, output: &crate::Output) -> usize {
    let crate::Output::Text(text) = output else {
        return usize::MAX;
    };
    let text = if outcome == crate::Outcome::Succeeded {
        text.clone()
    } else {
        format!("error: {text}")
    };
    // Responses encodes arguments as a JSON string and repeats the call ID.
    // Charge both frames, array delimiters and a conservative Messages wrapper
    // allowance. The model builder also checks the final provider frames,
    // including provider metadata that is unavailable at tool completion.
    serde_json::to_vec(&serde_json::json!([
        {"type":"function_call", "call_id":call.id.as_str(),
         "name":crate::model_tool_name(call.tool.as_str()), "arguments":call.args.to_string()},
        {"type":"function_call_output", "call_id":call.id.as_str(), "output":text}
    ]))
    .map_or(usize::MAX, |bytes| bytes.len().saturating_add(128))
}
fn completed(ctx: &Context) -> std::collections::BTreeMap<crate::CallId, usize> {
    let mut sizes = std::collections::BTreeMap::new();
    for record in ctx
        .tool_evidence()
        .iter()
        .filter(|e| e.call.tool.as_str() == CONTEXT_POCKET_TOOL_NAME)
    {
        sizes.insert(
            record.call.id.clone(),
            if record.result.outcome == crate::Outcome::Succeeded {
                encoded_size(&record.call, record.result.outcome, &record.result.output)
            } else {
                0
            },
        );
    }
    // Older grouped history remains accountable even after the evidence ring evicts a result.
    for entry in ctx.history() {
        if let Message::Tool {
            call,
            name,
            outcome,
            output,
        } = &entry.message
            && name.as_str() == CONTEXT_POCKET_TOOL_NAME
            && !sizes.contains_key(call)
        {
            let proposal = ctx.history().iter().find_map(|entry| match &entry.message {
                Message::Assistant { calls, .. } => {
                    calls.iter().find(|proposal| &proposal.id == call)
                }
                _ => None,
            });
            sizes.insert(
                call.clone(),
                if *outcome == crate::Outcome::Succeeded {
                    proposal.map_or(usize::MAX, |proposal| {
                        encoded_size(proposal, *outcome, output)
                    })
                } else {
                    0
                },
            );
        }
    }
    sizes
}
pub(crate) fn admission(ctx: &Context, call: &ProposedCall) -> Result<(), &'static str> {
    admit(
        ctx.history().iter().map(|e| &e.message),
        &completed(ctx),
        call,
    )
}
#[cfg(test)]
fn admission_messages<'a>(
    history: impl Iterator<Item = &'a Message> + Clone,
    call: &ProposedCall,
) -> Result<(), &'static str> {
    let mut sizes = std::collections::BTreeMap::new();
    for message in history.clone() {
        if let Message::Tool {
            call,
            name,
            outcome,
            output,
        } = message
            && name.as_str() == CONTEXT_POCKET_TOOL_NAME
        {
            let proposal = ProposedCall::new(
                call.clone(),
                name.clone(),
                serde_json::json!({}),
                crate::PrincipalId::new(""),
            );
            sizes.insert(
                call.clone(),
                if *outcome == crate::Outcome::Succeeded {
                    encoded_size(&proposal, *outcome, output)
                } else {
                    0
                },
            );
        }
    }
    admit(history, &sizes, call)
}
fn admit<'a>(
    history: impl Iterator<Item = &'a Message>,
    sizes: &std::collections::BTreeMap<crate::CallId, usize>,
    call: &ProposedCall,
) -> Result<(), &'static str> {
    let history: Vec<_> = history.collect();
    if history.iter().any(|m| matches!(m, Message::Summary { .. })) {
        return Err(STOP);
    }
    let mut used = sizes
        .values()
        .fold(0usize, |sum, size| sum.saturating_add(*size));
    for message in history {
        if let Message::Assistant { calls, .. } = message {
            for proposed in calls.iter().filter(|p| {
                p.tool.as_str() == CONTEXT_POCKET_TOOL_NAME && !sizes.contains_key(&p.id)
            }) {
                used = used.saturating_add(CONTEXT_POCKET_RESULT_BYTES);
                if proposed.id == call.id {
                    return if used <= CONTEXT_POCKET_BYTES {
                        Ok(())
                    } else {
                        Err(STOP)
                    };
                }
            }
        }
    }
    if used.saturating_add(CONTEXT_POCKET_RESULT_BYTES) <= CONTEXT_POCKET_BYTES {
        Ok(())
    } else {
        Err(STOP)
    }
}
pub(crate) fn finish(ctx: &Context, call: &ProposedCall, result: ToolResult) -> ToolResult {
    if call.tool.as_str() != CONTEXT_POCKET_TOOL_NAME {
        return result;
    }
    if result.outcome != crate::Outcome::Succeeded {
        return if encoded_size(call, result.outcome, &result.output) <= CONTEXT_POCKET_RESULT_BYTES
        {
            result
        } else {
            ToolResult::error("Case owner read unavailable; coverage remains partial.")
        };
    }
    let retained = completed(ctx)
        .values()
        .fold(0usize, |sum, size| sum.saturating_add(*size));
    let remaining = CONTEXT_POCKET_BYTES.saturating_sub(retained);
    let size = encoded_size(call, result.outcome, &result.output);
    if size <= CONTEXT_POCKET_RESULT_BYTES && size <= remaining {
        return result;
    }
    let mut error = ToolResult::error(
        "Case result unavailable: bounded inline context required; coverage remains partial.",
    );
    // Repeated refusals cannot manufacture more content after the allowance is exhausted.
    if encoded_size(call, error.outcome, &error.output) > remaining {
        error.output = crate::Output::Text(String::new());
    }
    error
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CallId, Outcome, Output, PrincipalId, ThreadId, ToolName};
    fn call(id: &str) -> ProposedCall {
        ProposedCall::new(
            CallId::new(id),
            ToolName::new(CONTEXT_POCKET_TOOL_NAME),
            serde_json::json!({"root_id":"root"}),
            PrincipalId::new("alice"),
        )
    }
    #[test]
    fn brief_then_expansion_can_reserve_remaining_context() {
        let ctx = Context::new(ThreadId {
            org: "org".into(),
            workspace: "ws".into(),
            thread: "t".into(),
        });
        assert!(admission(&ctx, &call("brief")).is_ok());
    }
    #[test]
    fn oversized_or_referenced_pocket_cannot_enter_transcript() {
        let ctx = Context::new(ThreadId {
            org: "org".into(),
            workspace: "ws".into(),
            thread: "t".into(),
        });
        let r = ToolResult {
            outcome: Outcome::Succeeded,
            output: Output::Text("x".repeat(CONTEXT_POCKET_BYTES)),
            receipt: None,
        };
        assert_eq!(finish(&ctx, &call("a"), r).outcome, Outcome::Failed);
        let r = ToolResult {
            outcome: Outcome::Succeeded,
            output: Output::Ref(crate::OutputRef::new("blob")),
            receipt: None,
        };
        assert_eq!(finish(&ctx, &call("a"), r).outcome, Outcome::Failed);
    }
    #[test]
    fn parallel_and_sequential_reads_share_one_actual_retained_budget() {
        fn proposed(calls: Vec<ProposedCall>) -> Message {
            Message::Assistant {
                text: String::new(),
                calls,
                reasoning: None,
                served: None,
            }
        }
        let calls: Vec<_> = (0..5).map(|n| call(&n.to_string())).collect();
        let parallel = [proposed(calls.clone())];
        assert!(admission_messages(parallel.iter(), &calls[3]).is_ok());
        assert!(admission_messages(parallel.iter(), &calls[4]).is_err());
        let sequential = [
            Message::Tool {
                call: calls[0].id.clone(),
                name: calls[0].tool.clone(),
                outcome: Outcome::Succeeded,
                output: Output::Text("pinned brief".into()),
            },
            proposed(vec![calls[1].clone()]),
        ];
        assert!(admission_messages(sequential.iter(), &calls[1]).is_ok());
        let compacted = [
            Message::Summary {
                text: "historical case".into(),
            },
            proposed(vec![calls[1].clone()]),
        ];
        assert!(admission_messages(compacted.iter(), &calls[1]).is_err());
    }
    #[test]
    fn escaped_native_pocket_cannot_undercharge_its_call_and_result() {
        let ctx = Context::new(ThreadId {
            org: "org".into(),
            workspace: "ws".into(),
            thread: "t".into(),
        });
        let mut packet: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../platform/cerebro/evals/context_pockets/native_brief.json"
        ))
        .unwrap();
        packet["items"][0]["title"] = serde_json::json!("\"".repeat(590));
        let call = ProposedCall::new(
            CallId::new("call0"),
            ToolName::new(CONTEXT_POCKET_TOOL_NAME),
            serde_json::json!({"root_id":"case"}),
            PrincipalId::new("alice"),
        );
        assert_eq!(
            finish(&ctx, &call, ToolResult::text(packet.to_string())).outcome,
            Outcome::Failed
        );
    }
    #[test]
    fn lost_case_authority_cannot_be_hidden_by_evidence_eviction_or_summary() {
        let mut ctx = Context::new(ThreadId {
            org: "org".into(),
            workspace: "ws".into(),
            thread: "t".into(),
        });
        for n in 0..=crate::TOOL_EVIDENCE_LIMIT {
            let mut c = call(&n.to_string());
            if n > 0 {
                c.tool = ToolName::new("other.read");
            }
            ctx.observe(
                crate::Cursor((n * 2 + 1) as i64),
                &crate::Event::ModelStepCompleted {
                    step: n as u32,
                    text: String::new(),
                    calls: vec![c.clone()],
                    reasoning: None,
                    served: None,
                    timing: None,
                },
            );
            ctx.observe(
                crate::Cursor((n * 2 + 2) as i64),
                &crate::Event::ToolFinished {
                    call: c.id,
                    outcome: Outcome::Succeeded,
                    output: Output::Text("owner data".into()),
                    receipt: None,
                    summary: None,
                },
            );
        }
        assert!(ctx.context_pocket_authority_lost());
    }
}
