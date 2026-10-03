//! `/context` breakdown: estimate how the current session's context window is
//! spent, split by category (system prompt, tool schemas, tool results,
//! conversation, other/overhead), with an optional compact budget waterfall.
//!
//! Token counts use the selected model's bundled tokenizer when available and
//! clearly identify heuristic estimates otherwise. The input is the live
//! TUI transcript (`crate::state::Message`), which mirrors the agent history:
//! regular user/assistant text, thinking blocks, and tool calls with their
//! outputs. UI-only messages (system notices, side questions) never reach the
//! model and are excluded.

use crate::state::Message;
use maestro_context::token_counting::{self, CountConfidence};
use maestro_context::token_estimation;

const WATERFALL_WIDTH: usize = 20;

/// Token breakdown of the current session context, by category.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContextBreakdown {
    /// Base system prompt sent with every request.
    pub system_prompt: u64,
    /// Tool call inputs and tool result outputs.
    pub tool_results: u64,
    /// Tool definitions included in the selected request surface.
    pub tool_schemas: u64,
    /// User and assistant text, including compaction summaries.
    pub conversation: u64,
    /// Everything else: thinking/reasoning blocks and framing overhead.
    pub other: u64,
}

impl ContextBreakdown {
    /// Estimate the breakdown from the system prompt and the live transcript.
    #[cfg(test)]
    #[must_use]
    pub fn compute(system_prompt: &str, messages: &[Message]) -> Self {
        Self::compute_for_model(system_prompt, messages, None)
    }

    /// Count with the selected model tokenizer when Maestro bundles one.
    #[must_use]
    pub fn compute_for_model(
        system_prompt: &str,
        messages: &[Message],
        model: Option<&str>,
    ) -> Self {
        let count = |text: &str| token_counting::count_tokens(text, model);
        let mut breakdown = Self {
            system_prompt: count(system_prompt),
            ..Self::default()
        };

        for message in messages {
            // Only messages that are part of the model-facing history count:
            // regular turns plus compaction summaries (which are replayed to
            // the model). System notices and side questions are UI-only.
            let in_model_context =
                message.counts_toward_compaction_index() || message.is_compaction_boundary();
            if !in_model_context {
                continue;
            }

            breakdown.conversation += count(&message.content);
            breakdown.other += count(&message.thinking);

            for call in &message.tool_calls {
                // Mirrors the compactor's ToolUse/ToolResult estimation: tool
                // name + serialized input for the call, content for the result.
                let args = serde_json::to_string(&call.args).unwrap_or_default();
                breakdown.tool_results += count(&call.tool) + count(&args) + count(&call.output);
            }
        }

        breakdown
    }

    /// Total estimated tokens across all categories.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.system_prompt + self.tool_results + self.tool_schemas + self.conversation + self.other
    }

    /// Canonical category keys keep recommendation selection independent of locale.
    fn category_values(&self) -> [(&'static str, u64); 5] {
        [
            ("System prompt", self.system_prompt),
            ("Tool schemas", self.tool_schemas),
            ("Tool results", self.tool_results),
            ("Conversation", self.conversation),
            ("Other / overhead", self.other),
        ]
    }

    /// Category rows in display order: `(label, tokens, share of total in %)`.
    #[must_use]
    pub fn categories(&self) -> Vec<(&'static str, u64, f64)> {
        let total = self.total();
        self.category_values()
            .into_iter()
            .map(|(key, tokens)| {
                (
                    maestro_ui::localization::tr(key),
                    tokens,
                    share(tokens, total),
                )
            })
            .collect()
    }

    /// One suggestion derived from counted categories, never from guessed content.
    pub fn advice(&self, context_window: Option<u64>) -> String {
        let total = self.total();
        if total == 0 {
            return maestro_ui::localization::tr("No conversation context yet.").into();
        }
        let (category, tokens) = self
            .category_values()
            .into_iter()
            .max_by_key(|(_, tokens)| *tokens)
            .expect("fixed categories");
        let pressure = context_window
            .filter(|window| *window > 0)
            .is_some_and(|window| token_estimation::usage_percentage(total, window) >= 70.0);
        let action = match category {
            "Tool schemas" => {
                maestro_ui::localization::tr("Use `/tools` to review the enabled tool surface.")
            }
            "System prompt" => maestro_ui::localization::tr(
                "Use `/harness list` to review supplemental instructions.",
            ),
            _ if pressure => maestro_ui::localization::tr(
                "Use `/compact` to shorten older history while retaining recent turns.",
            ),
            _ if context_window.is_none_or(|window| window == 0) => maestro_ui::localization::tr(
                "Keep future tool output bounded until context capacity is known.",
            ),
            _ => maestro_ui::localization::tr(
                "There is no need to compact now; keep future tool output bounded.",
            ),
        };
        let unknown = if context_window.is_none_or(|window| window == 0) {
            maestro_ui::localization::tr(" Context capacity is unknown.")
        } else {
            ""
        };
        maestro_ui::localization::format(
            "**Next step:** {0} is the largest category ({1}%). {2}{3}",
            &[
                maestro_ui::localization::tr(category).to_string(),
                format!("{:.1}", share(tokens, total)),
                (action).to_string(),
                (unknown).to_string(),
            ],
        )
    }

    /// Render the breakdown as a chat message with counts, percentages, and a
    /// progress bar against the model's context window (when known).
    #[cfg(test)]
    #[must_use]
    pub fn render(&self, model: Option<&str>, context_window: Option<u64>) -> String {
        self.render_with_budget(model, context_window, None, None)
    }

    /// Render with an optional response reserve and remaining headroom budget.
    ///
    /// The legacy [`Self::render`] output stays unchanged when neither budget
    /// input is supplied. When a budget input is supplied and the context
    /// window is known, the progress bar becomes a proportional waterfall.
    #[must_use]
    pub fn render_with_budget(
        &self,
        model: Option<&str>,
        context_window: Option<u64>,
        response_reserve: Option<u64>,
        remaining_headroom: Option<u64>,
    ) -> String {
        let total = self.total();
        let known_window = context_window.filter(|window| *window > 0);
        let remaining_headroom = remaining_headroom.or_else(|| {
            response_reserve.and_then(|reserve| {
                known_window.map(|window| window.saturating_sub(total.saturating_add(reserve)))
            })
        });
        let mut lines = vec![
            maestro_ui::localization::tr("## Context Breakdown").to_string(),
            String::new(),
        ];

        if let Some(model) = model {
            lines.push(maestro_ui::localization::format(
                "**Model:** {0}",
                &[(model).to_string()],
            ));
        }
        let confidence = token_counting::count_tokens_with_metadata("", model).confidence;
        lines.push(maestro_ui::localization::format(
            "**Token count:** {0}",
            &[(match confidence {
                CountConfidence::Measured => {
                    maestro_ui::localization::tr("measured with the model tokenizer")
                }
                CountConfidence::Estimated => {
                    maestro_ui::localization::tr("estimated (model tokenizer unavailable)")
                }
            })
            .to_string()],
        ));
        lines.push(
            maestro_ui::localization::tr("**Prompt cache:** reuse requires the same model, system prompt, thinking level, and skills; provider caches may expire after long idle periods.")
                .to_string(),
        );
        match context_window {
            Some(window) => lines.push(maestro_ui::localization::format(
                "**Context window:** {0}",
                &[(format_tokens(window)).clone()],
            )),
            None => {
                lines.push(maestro_ui::localization::tr("**Context window:** unknown").to_string());
            }
        }
        lines.push(String::new());

        for (label, tokens, pct) in self.categories() {
            lines.push(format!(
                "- **{label}:** {} ({pct:.1}%)",
                format_tokens(tokens)
            ));
        }
        if let Some(tokens) = response_reserve {
            lines.push(maestro_ui::localization::format(
                "- **Response reserve:** {0}",
                &[format_tokens(tokens)],
            ));
        }
        if let Some(tokens) = remaining_headroom {
            lines.push(maestro_ui::localization::format(
                "- **Remaining:** {0}",
                &[format_tokens(tokens)],
            ));
        }
        lines.push(String::new());

        match context_window {
            Some(window) if window > 0 => {
                let used_pct = token_estimation::usage_percentage(total, window);
                lines.push(maestro_ui::localization::format(
                    "**Total:** {0} of {1} ({2}%)",
                    &[
                        format_tokens(total),
                        format_tokens(window),
                        (format!("{used_pct:.1}")).to_string(),
                    ],
                ));
                if response_reserve.is_some() || remaining_headroom.is_some() {
                    let segments = self.waterfall_segments(response_reserve, remaining_headroom);
                    lines.push(maestro_ui::localization::format(
                        "**Waterfall:** {0}",
                        &[proportional_waterfall(&segments, WATERFALL_WIDTH)],
                    ));
                    lines.push(
                        maestro_ui::localization::tr(
                            "S system · D schemas · R results · C conversation",
                        )
                        .to_string(),
                    );
                    lines.push(
                        maestro_ui::localization::tr("O other · P reserve · . remaining")
                            .to_string(),
                    );
                } else {
                    lines.push(progress_bar(used_pct / 100.0, WATERFALL_WIDTH));
                }
            }
            _ => {
                lines.push(maestro_ui::localization::format(
                    "**Total:** {0} (estimated)",
                    &[format_tokens(total)],
                ));
            }
        }

        lines.push(String::new());
        lines.push(self.advice(context_window));
        lines.join("\n")
    }

    fn waterfall_segments(
        &self,
        response_reserve: Option<u64>,
        remaining_headroom: Option<u64>,
    ) -> Vec<WaterfallSegment> {
        let mut segments = vec![
            WaterfallSegment {
                marker: 'S',
                tokens: self.system_prompt,
            },
            WaterfallSegment {
                marker: 'D',
                tokens: self.tool_schemas,
            },
            WaterfallSegment {
                marker: 'R',
                tokens: self.tool_results,
            },
            WaterfallSegment {
                marker: 'C',
                tokens: self.conversation,
            },
            WaterfallSegment {
                marker: 'O',
                tokens: self.other,
            },
        ];
        if let Some(tokens) = response_reserve {
            segments.push(WaterfallSegment {
                marker: 'P',
                tokens,
            });
        }
        if let Some(tokens) = remaining_headroom {
            segments.push(WaterfallSegment {
                marker: '.',
                tokens,
            });
        }
        segments
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WaterfallSegment {
    marker: char,
    tokens: u64,
}

/// Share of `part` in `total` as a percentage; `0.0` when `total` is zero.
fn share(part: u64, total: u64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    (part as f64 / total as f64) * 100.0
}

/// Simple text progress bar, e.g. `[████████░░░░░░░░░░░░] 40%`.
fn progress_bar(fraction: f64, width: usize) -> String {
    let fraction = fraction.clamp(0.0, 1.0);
    let filled = (fraction * width as f64).round() as usize;
    let empty = width.saturating_sub(filled);
    format!(
        "[{}{}] {:.0}%",
        "█".repeat(filled),
        "░".repeat(empty),
        fraction * 100.0
    )
}

/// Render a bounded proportional bar, using largest-remainder allocation so
/// every cell is assigned without letting zero-sized categories appear.
fn proportional_waterfall(segments: &[WaterfallSegment], width: usize) -> String {
    if width == 0 {
        return "[]".to_string();
    }

    let total: u128 = segments
        .iter()
        .map(|segment| u128::from(segment.tokens))
        .sum();
    if total == 0 {
        return format!("[{}]", ".".repeat(width));
    }

    let mut cells = Vec::with_capacity(segments.len());
    let mut remainders = Vec::with_capacity(segments.len());
    let mut allocated = 0usize;
    for (index, segment) in segments.iter().enumerate() {
        let scaled = u128::from(segment.tokens) * width as u128;
        let segment_cells = usize::try_from(scaled / total).unwrap_or(width);
        cells.push(segment_cells);
        allocated = allocated.saturating_add(segment_cells);
        if segment.tokens > 0 {
            remainders.push((scaled % total, index));
        }
    }

    remainders.sort_by(
        |(left_remainder, left_index), (right_remainder, right_index)| {
            right_remainder
                .cmp(left_remainder)
                .then_with(|| left_index.cmp(right_index))
        },
    );
    for (_, index) in remainders.into_iter().take(width.saturating_sub(allocated)) {
        cells[index] += 1;
    }

    let mut bar = String::with_capacity(width);
    for (segment, segment_cells) in segments.iter().zip(cells) {
        bar.extend(std::iter::repeat_n(segment.marker, segment_cells));
    }
    format!("[{bar}]")
}

/// Human-readable token count, matching the usage tracker's K/M style.
fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}K", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{MessageKind, MessageRole, ToolCallState, ToolCallStatus};
    use maestro_context::token_estimation::estimate_tokens;
    use std::time::SystemTime;

    fn message(role: MessageRole, kind: MessageKind, content: &str) -> Message {
        Message {
            id: uuid::Uuid::new_v4().to_string(),
            role,
            kind,
            content: content.to_string(),
            thinking: String::new(),
            streaming: false,
            tool_calls: Vec::new(),
            usage: None,
            timestamp: SystemTime::now(),
            thinking_expanded: false,
        }
    }

    fn tool_call(tool: &str, args: serde_json::Value, output: &str) -> ToolCallState {
        ToolCallState {
            parent_call_id: None,
            duration_ms: None,
            call_id: "call-1".to_string(),
            tool: tool.to_string(),
            args,
            status: ToolCallStatus::Completed,
            output: output.to_string(),
        }
    }

    #[test]
    fn empty_session_counts_only_system_prompt() {
        let breakdown = ContextBreakdown::compute("You are a helpful assistant.", &[]);

        assert_eq!(
            breakdown.system_prompt,
            estimate_tokens("You are a helpful assistant.")
        );
        assert_eq!(breakdown.tool_results, 0);
        assert_eq!(breakdown.conversation, 0);
        assert_eq!(breakdown.other, 0);
        assert_eq!(breakdown.total(), breakdown.system_prompt);
    }

    #[test]
    fn fully_empty_session_is_zero() {
        let breakdown = ContextBreakdown::compute("", &[]);
        assert_eq!(breakdown.total(), 0);
        // Percentages must not divide by zero.
        for (_, _, pct) in breakdown.categories() {
            assert!(pct.abs() < f64::EPSILON);
        }
    }

    #[test]
    fn fixture_history_splits_into_categories() {
        let mut assistant = message(MessageRole::Assistant, MessageKind::Regular, "Sure thing");
        assistant.thinking = "Let me think about this".to_string();
        assistant.tool_calls.push(tool_call(
            "read",
            serde_json::json!({"path": "/tmp/a"}),
            "file contents here",
        ));
        let messages = vec![
            message(
                MessageRole::User,
                MessageKind::Regular,
                "Please read a file",
            ),
            assistant,
            // UI-only kinds must be excluded.
            message(MessageRole::Assistant, MessageKind::System, "local notice"),
            message(
                MessageRole::User,
                MessageKind::SideQuestion,
                "side question",
            ),
        ];

        let breakdown = ContextBreakdown::compute("sys", &messages);

        assert_eq!(breakdown.system_prompt, estimate_tokens("sys"));
        assert_eq!(
            breakdown.conversation,
            estimate_tokens("Please read a file") + estimate_tokens("Sure thing")
        );
        assert_eq!(breakdown.other, estimate_tokens("Let me think about this"));
        let args = serde_json::to_string(&serde_json::json!({"path": "/tmp/a"})).unwrap();
        assert_eq!(
            breakdown.tool_results,
            estimate_tokens("read")
                + estimate_tokens(&args)
                + estimate_tokens("file contents here")
        );
        assert_eq!(
            breakdown.total(),
            breakdown.system_prompt
                + breakdown.tool_results
                + breakdown.conversation
                + breakdown.other
        );
    }

    #[test]
    fn compaction_summary_counts_as_conversation() {
        let messages = vec![message(
            MessageRole::Assistant,
            MessageKind::CompactionBoundary,
            "## Conversation Summary\n\nStuff happened.",
        )];
        let breakdown = ContextBreakdown::compute("", &messages);
        assert_eq!(
            breakdown.conversation,
            estimate_tokens("## Conversation Summary\n\nStuff happened.")
        );
    }

    #[test]
    fn percentages_sum_to_hundred() {
        let mut assistant = message(MessageRole::Assistant, MessageKind::Regular, "answer");
        assistant.thinking = "thinking".to_string();
        assistant.tool_calls.push(tool_call(
            "bash",
            serde_json::json!({"command": "ls"}),
            "out",
        ));
        let messages = vec![
            message(MessageRole::User, MessageKind::Regular, "question"),
            assistant,
        ];

        let breakdown = ContextBreakdown::compute("system prompt", &messages);
        let sum: f64 = breakdown.categories().iter().map(|(_, _, pct)| pct).sum();
        assert!(
            (sum - 100.0).abs() < 0.001,
            "category percentages should sum to 100, got {sum}"
        );
    }

    #[test]
    fn render_includes_bar_against_window() {
        let messages = vec![message(
            MessageRole::User,
            MessageKind::Regular,
            &"x".repeat(400),
        )];
        let breakdown = ContextBreakdown::compute("sys", &messages);
        let rendered = breakdown.render(Some("claude-sonnet-4-5-20250514"), Some(200_000));

        assert!(rendered.contains("## Context Breakdown"));
        assert!(rendered.contains("**Model:** claude-sonnet-4-5-20250514"));
        assert!(rendered.contains("**Context window:** 200.0K"));
        assert!(rendered.contains("System prompt"));
        assert!(rendered.contains("Tool results"));
        assert!(rendered.contains("Conversation"));
        assert!(rendered.contains("Other / overhead"));
        assert!(rendered.contains('█') || rendered.contains('░'));
        assert!(rendered.contains("% used") || rendered.contains('%'));
    }

    #[test]
    fn render_with_budget_includes_rows_and_all_waterfall_categories() {
        let breakdown = ContextBreakdown {
            system_prompt: 100,
            tool_schemas: 100,
            tool_results: 200,
            conversation: 300,
            other: 100,
        };
        let rendered =
            breakdown.render_with_budget(Some("fixture"), Some(1_000), Some(100), Some(100));

        assert!(rendered.contains("- **Response reserve:** 100"));
        assert!(rendered.contains("- **Remaining:** 100"));
        let waterfall = rendered
            .lines()
            .find(|line| line.starts_with("**Waterfall:**"))
            .expect("budget render should include a waterfall");
        let bar = waterfall
            .strip_prefix("**Waterfall:** ")
            .expect("waterfall label should be present");
        assert_eq!(bar.chars().count(), WATERFALL_WIDTH + 2);
        for marker in ['S', 'D', 'R', 'C', 'O', 'P', '.'] {
            assert!(bar.contains(marker), "waterfall should contain {marker}");
        }
        assert!(rendered.contains("S system"));
        assert!(rendered.contains("D schemas"));
        assert!(rendered.contains("R results"));
        assert!(rendered.contains("C conversation"));
        assert!(rendered.contains("O other"));
        assert!(rendered.contains("P reserve"));
        assert!(rendered.contains(". remaining"));
    }

    #[test]
    fn render_with_budget_derives_remaining_from_known_window() {
        let breakdown = ContextBreakdown {
            conversation: 300,
            ..Default::default()
        };
        let rendered = breakdown.render_with_budget(None, Some(1_000), Some(200), None);

        assert!(rendered.contains("- **Response reserve:** 200"));
        assert!(rendered.contains("- **Remaining:** 500"));
        assert!(rendered.contains("**Waterfall:**"));
    }

    #[test]
    fn render_with_budget_keeps_unknown_window_without_waterfall() {
        let rendered =
            ContextBreakdown::default().render_with_budget(None, None, Some(100), Some(50));

        assert!(rendered.contains("**Context window:** unknown"));
        assert!(rendered.contains("- **Response reserve:** 100"));
        assert!(rendered.contains("- **Remaining:** 50"));
        assert!(!rendered.contains("**Waterfall:**"));
        assert!(!rendered.contains('['));
    }

    #[test]
    fn render_without_window_omits_bar() {
        let breakdown = ContextBreakdown::compute("sys", &[]);
        let rendered = breakdown.render(None, None);
        assert!(rendered.contains("**Context window:** unknown"));
        assert!(rendered.contains("**Total:**"));
        assert!(!rendered.contains('['));
    }

    #[test]
    fn progress_bar_edges() {
        assert_eq!(progress_bar(0.0, 10), "[░░░░░░░░░░] 0%");
        assert_eq!(progress_bar(1.0, 10), "[██████████] 100%");
        // Over-100% usage clamps instead of overflowing the bar.
        assert_eq!(progress_bar(1.5, 10), "[██████████] 100%");
    }

    #[test]
    fn proportional_waterfall_stays_bounded_for_narrow_widths() {
        let segments = [
            WaterfallSegment {
                marker: 'S',
                tokens: 1,
            },
            WaterfallSegment {
                marker: '.',
                tokens: 1,
            },
        ];
        assert_eq!(proportional_waterfall(&segments, 1), "[S]");
        assert_eq!(proportional_waterfall(&segments, 0), "[]");
    }
}

#[cfg(test)]
mod advice_tests {
    use super::*;
    #[test]
    fn advice_distinguishes_history_pressure_from_fixed_prompt_cost() {
        let mut counts = ContextBreakdown {
            tool_results: 800,
            ..Default::default()
        };
        assert!(counts.advice(Some(1000)).contains("`/compact`"));
        counts.tool_schemas = 1600;
        assert!(counts.advice(Some(1000)).contains("`/tools`"));
        assert!(!counts.advice(Some(1000)).contains("`/compact`"));
        assert!(counts.advice(None).contains("capacity is unknown"));
        assert_eq!(
            ContextBreakdown::default().advice(None),
            "No conversation context yet."
        );
    }
    #[test]
    fn localized_advice_uses_canonical_category_and_preserves_model_identity() {
        for locale in maestro_ui::localization::Locale::ALL {
            maestro_ui::localization::with_locale(locale, || {
                let schemas = ContextBreakdown {
                    tool_schemas: 900,
                    ..Default::default()
                };
                let system = ContextBreakdown {
                    system_prompt: 900,
                    ..Default::default()
                };
                assert!(schemas.advice(Some(1000)).contains("`/tools`"));
                assert!(system.advice(Some(1000)).contains("`/harness list`"));
                let rendered = schemas.render_with_budget(
                    Some("model/keep-me"),
                    Some(1000),
                    Some(50),
                    Some(50),
                );
                assert!(rendered.contains("model/keep-me"));
                assert!(rendered.contains(locale.translate("Other / overhead")));
                assert!(
                    rendered
                        .contains(&locale.format("- **Response reserve:** {0}", &["50".into()]))
                );
            });
        }
    }
}
