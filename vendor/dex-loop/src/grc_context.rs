//! Retained GRC context admission. Until compaction preserves exact revision and
//! permission pins, a summary cannot reopen this budget. Reservations come from
//! the actual transcript, including every call in a parallel proposed step.
use crate::{Context, Message, Output, ProposedCall, ToolResult};

pub const GRC_GRAPH_TOOL_NAME: &str = "cerebro.grc_graph";
pub const GRC_CONTEXT_BYTES: usize = 12_000;
pub const GRC_RESULT_BYTES: usize = 4_000;
const STOP: &str = "not executed: GRC context is partial; retained context cannot admit another graph read. Continue in a fresh bounded context with owner revision and permission checks.";

pub(crate) fn admission(ctx: &Context, call: &ProposedCall) -> Result<(), &'static str> {
    admission_messages(ctx.history().iter().map(|e| &e.message), call)
}
fn admission_messages<'a>(
    history: impl Iterator<Item = &'a Message>,
    call: &ProposedCall,
) -> Result<(), &'static str> {
    let mut first = None;
    let mut retained = 0usize;
    for message in history {
        match message {
            Message::Summary { .. } => return Err(STOP),
            Message::Tool { call, name, .. } if name.as_str() == GRC_GRAPH_TOOL_NAME => {
                if first.is_none() {
                    first = Some(call);
                }
                retained = retained.saturating_add(message.size());
            }
            Message::Assistant { calls, .. } => {
                for proposed in calls {
                    if proposed.tool.as_str() == GRC_GRAPH_TOOL_NAME && first.is_none() {
                        first = Some(&proposed.id);
                    }
                }
            }
            _ => {}
        }
    }
    if first == Some(&call.id) && retained.saturating_add(GRC_RESULT_BYTES) <= GRC_CONTEXT_BYTES {
        Ok(())
    } else {
        Err(STOP)
    }
}

pub(crate) fn finish(ctx: &Context, call: &ProposedCall, result: ToolResult) -> ToolResult {
    if call.tool.as_str() != GRC_GRAPH_TOOL_NAME {
        return result;
    }
    let retained = ctx
        .history()
        .iter()
        .filter(
            |e| matches!(&e.message,Message::Tool{name,..} if name.as_str()==GRC_GRAPH_TOOL_NAME),
        )
        .fold(0usize, |sum, e| sum.saturating_add(e.message.size()));
    let remaining = GRC_CONTEXT_BYTES.saturating_sub(retained);
    let mut result = match &result.output {
        Output::Text(text) if text.len() <= GRC_RESULT_BYTES && text.len() <= remaining => result,
        _ => ToolResult::error(
            "GRC result unavailable: bounded inline context required; coverage is partial.",
        ),
    };
    if let Output::Text(text) = &mut result.output
        && text.len() > remaining
    {
        *text = String::new();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CallId, Outcome, PrincipalId, ToolName};
    fn call(id: &str) -> ProposedCall {
        ProposedCall::new(
            CallId::new(id),
            ToolName::new(GRC_GRAPH_TOOL_NAME),
            serde_json::json!({}),
            PrincipalId::new("alice"),
        )
    }
    fn proposed(calls: Vec<ProposedCall>) -> Message {
        Message::Assistant {
            text: String::new(),
            calls,
            reasoning: None,
            served: None,
        }
    }
    #[test]
    fn parallel_calls_reserve_only_first_and_replay_keeps_same_reservation() {
        let a = call("a");
        let b = call("b");
        let history = [proposed(vec![a.clone(), b.clone()])];
        assert!(admission_messages(history.iter(), &a).is_ok());
        assert!(admission_messages(history.iter(), &b).is_err());
        assert!(admission_messages(history.iter(), &a).is_ok());
    }
    #[test]
    fn oversized_or_referenced_result_cannot_enter_transcript() {
        let ctx = Context::new(crate::ThreadId {
            org: "org".into(),
            workspace: "ws".into(),
            thread: "thread".into(),
        });
        let oversized = ToolResult {
            outcome: Outcome::Succeeded,
            output: Output::Text("x".repeat(GRC_CONTEXT_BYTES + 1)),
            receipt: None,
        };
        let bounded = finish(&ctx, &call("a"), oversized);
        assert_eq!(bounded.outcome, Outcome::Failed);
        assert!(matches!(bounded.output,Output::Text(text) if text.len()<GRC_RESULT_BYTES));
        let referenced = ToolResult {
            outcome: Outcome::Succeeded,
            output: Output::Ref(crate::OutputRef::new("blob")),
            receipt: None,
        };
        assert_eq!(
            finish(&ctx, &call("a"), referenced).outcome,
            Outcome::Failed
        );
    }
    #[test]
    fn actual_retained_result_and_compaction_do_not_reset_admission() {
        let b = call("b");
        let history = [
            Message::Tool {
                call: CallId::new("a"),
                name: ToolName::new(GRC_GRAPH_TOOL_NAME),
                outcome: Outcome::Succeeded,
                output: Output::Text("owner pins".into()),
            },
            proposed(vec![b.clone()]),
        ];
        assert!(admission_messages(history.iter(), &b).is_err());
        let compacted = [
            Message::Summary {
                text: "previous work".into(),
            },
            proposed(vec![b.clone()]),
        ];
        assert!(admission_messages(compacted.iter(), &b).is_err());
    }
}
