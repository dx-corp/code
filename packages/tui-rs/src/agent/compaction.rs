//! Terminal compatibility exports and transcript parity tests.
#[cfg(test)]
use maestro_ai::Message;
pub use maestro_local_host::agent::compaction::*;
#[cfg(test)]
mod tests {
    use maestro_ai::{ContentBlock, MessageContent, Role};

    use super::*;

    #[test]
    fn host_resolves_catalog_window_before_core_construction() {
        let catalog_tokens = crate::model_catalog::find_model("gpt-5.5")
            .map(|entry| u64::from(entry.capabilities.context_tokens))
            .expect("gpt-5.5 catalog entry");
        let config = CompactionConfig::for_model("gpt-5.5", None);
        assert_eq!(config.max_context_tokens, catalog_tokens);

        let luna = CompactionConfig::for_model("gpt-5.6-luna", None);
        assert_eq!(luna.max_context_tokens, 1_050_000);
        assert_eq!(luna.target_tokens, 525_000);

        let overridden = CompactionConfig::for_model("gpt-5.5", Some(96_000));
        assert_eq!(overridden.max_context_tokens, 96_000);
    }
    /// Fixture transcript shared by the counter-parity tests below, built twice:
    /// once as the agent history the compactor sees (`crate::ai::Message`) and
    /// once as the TUI transcript `/context` reads (`crate::state::Message`).
    /// The same strings appear in both, so any divergence in the totals is a
    /// divergence between the two token counters and nothing else.
    fn parity_fixture() -> (Vec<Message>, Vec<crate::state::Message>) {
        use crate::state::{
            Message as UiMessage, MessageKind, MessageRole, ToolCallState, ToolCallStatus,
        };
        use std::time::SystemTime;

        // Dense source text: bytes/4 and the o200k tokenizer disagree sharply
        // here, which is what makes the parity assertion meaningful.
        let user_text = "Refactor `fn add(a: usize, b: usize) -> usize { a + b }` \
             into a generic over `core::ops::Add`, and keep the doctest.";
        let assistant_text =
            "Done. `impl<T: Add<Output = T>> Sum<T> for Pair<T>` now covers the generic case.";
        let thinking_text = "The doctest asserts add(2,2)==4; a generic impl must keep that true.";
        let tool_name = "bash";
        let tool_args = serde_json::json!({"command": "cargo test -p maestro-tui add_generic"});
        let tool_output = "running 1 test\ntest add_generic ... ok\n\ntest result: ok. 1 passed";

        let agent = vec![
            Message {
                role: Role::User,
                content: MessageContent::Text(user_text.to_string()),
            },
            Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(vec![
                    ContentBlock::Text {
                        text: assistant_text.to_string(),
                    },
                    ContentBlock::Thinking {
                        thinking: thinking_text.to_string(),
                        signature: None,
                    },
                    ContentBlock::ToolUse {
                        id: "call-1".to_string(),
                        name: tool_name.to_string(),
                        input: tool_args.clone(),
                        gemini_context: None,
                    },
                ]),
            },
            Message {
                role: Role::User,
                content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: "call-1".to_string(),
                    content: tool_output.to_string(),
                    is_error: Some(false),
                }]),
            },
        ];

        let ui_message = |role: MessageRole, content: &str| UiMessage {
            id: String::new(),
            role,
            kind: MessageKind::Regular,
            content: content.to_string(),
            thinking: String::new(),
            streaming: false,
            tool_calls: Vec::new(),
            usage: None,
            timestamp: SystemTime::UNIX_EPOCH,
            thinking_expanded: false,
        };
        let mut assistant = ui_message(MessageRole::Assistant, assistant_text);
        assistant.thinking = thinking_text.to_string();
        assistant.tool_calls.push(ToolCallState {
            parent_call_id: None,
            duration_ms: None,
            call_id: "call-1".to_string(),
            tool: tool_name.to_string(),
            args: tool_args,
            status: ToolCallStatus::Completed,
            output: tool_output.to_string(),
        });
        let ui = vec![ui_message(MessageRole::User, user_text), assistant];

        (agent, ui)
    }

    #[test]
    fn auto_compaction_counts_agree_with_context_breakdown() {
        use crate::app::context_breakdown::ContextBreakdown;

        // An OpenAI-clade model, so `token_counting` has a bundled tokenizer
        // and both counts are `CountConfidence::Measured`.
        let model = "gpt-4o";
        let (agent, ui) = parity_fixture();

        let compactor = ContextCompactor::new(CompactionConfig {
            model: Some(model.to_string()),
            ..Default::default()
        });
        let gate_tokens = compactor.estimate_tokens(&agent);
        // The empty system prompt keeps the breakdown to the same content the
        // compactor sees; the compactor never counts the system prompt.
        let breakdown_tokens = ContextBreakdown::compute_for_model("", &ui, Some(model)).total();

        assert!(breakdown_tokens > 0);
        let drift = gate_tokens.abs_diff(breakdown_tokens) as f64 / breakdown_tokens as f64;
        assert!(
            drift <= 0.05,
            "compaction gate counted {gate_tokens} tokens, /context counted \
             {breakdown_tokens}: {:.1}% apart",
            drift * 100.0
        );

        // Prove the gate is no longer reading the bytes/4 heuristic. If it
        // were, this fixture would count materially lower.
        let heuristic_compactor = ContextCompactor::new(CompactionConfig {
            model: None,
            ..Default::default()
        });
        let heuristic = heuristic_compactor.estimate_tokens(&agent);
        assert_ne!(
            gate_tokens, heuristic,
            "gate count matches the bytes/4 heuristic; the tokenizer is not being used"
        );
    }

    #[test]
    fn auto_compact_threshold_uses_the_measured_count() {
        let model = "gpt-4o";
        let (agent, _) = parity_fixture();

        let heuristic_compactor = ContextCompactor::new(CompactionConfig {
            model: None,
            ..Default::default()
        });
        let heuristic = heuristic_compactor.estimate_tokens(&agent);
        let measured_compactor = ContextCompactor::new(CompactionConfig {
            model: Some(model.to_string()),
            ..Default::default()
        });
        let measured = measured_compactor.estimate_tokens(&agent);
        assert!(
            measured > heuristic,
            "fixture must tokenize higher than bytes/4 for this test to bite \
             (measured {measured}, heuristic {heuristic})"
        );

        // Pick a window whose 85% threshold sits between the two counts: the
        // heuristic says "no compaction needed", the tokenizer says "compact".
        let threshold = u64::midpoint(heuristic, measured);
        let max_context_tokens = (threshold as f64 / 0.85).ceil() as u64;

        let measured_compactor = ContextCompactor::new(CompactionConfig {
            model: Some(model.to_string()),
            max_context_tokens,
            ..Default::default()
        });
        let heuristic_compactor = ContextCompactor::new(CompactionConfig {
            model: None,
            max_context_tokens,
            ..Default::default()
        });

        assert!(measured_compactor.should_auto_compact(&agent));
        assert!(!heuristic_compactor.should_auto_compact(&agent));
    }
}
