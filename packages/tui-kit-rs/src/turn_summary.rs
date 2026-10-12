//! Compact, idle-only summaries for completed interactive turns.

use std::time::Duration;

/// Facts collected while one user turn is running.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnSummary {
    tool_calls: usize,
    successful_tools: usize,
    failed_tools: usize,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    duration: Duration,
}

impl TurnSummary {
    /// Add one provider response's usage to the aggregate turn summary.
    pub fn add_usage(&mut self, usage: &crate::headless::TokenUsage) {
        self.input_tokens = self.input_tokens.saturating_add(usage.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(usage.output_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(usage.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(usage.cache_write_tokens);
    }

    /// Set the tool counts collected from the completed transcript slice.
    pub fn set_tool_counts(
        &mut self,
        tool_calls: usize,
        successful_tools: usize,
        failed_tools: usize,
    ) {
        self.tool_calls = tool_calls;
        self.successful_tools = successful_tools;
        self.failed_tools = failed_tools;
    }

    /// Set the wall-clock duration of the completed turn.
    pub fn set_duration(&mut self, duration: Duration) {
        self.duration = duration;
    }

    /// Render the concise status-bar recap shown after the app becomes idle.
    #[must_use]
    pub fn status_line(&self) -> String {
        let tools = match self.tool_calls {
            0 => "no tools".to_string(),
            1 => format!(
                "1 tool ({} {})",
                self.successful_tools,
                if self.failed_tools == 0 {
                    "ok"
                } else {
                    "failed"
                }
            ),
            count => format!(
                "{count} tools ({} ok, {} failed)",
                self.successful_tools, self.failed_tools
            ),
        };

        let mut parts = vec!["Turn complete".to_string(), tools];
        if self.input_tokens > 0 || self.output_tokens > 0 {
            parts.push(format!(
                "{} in / {} out",
                format_tokens(self.input_tokens),
                format_tokens(self.output_tokens)
            ));
        }
        if self.cache_read_tokens > 0 || self.cache_write_tokens > 0 {
            parts.push(format!(
                "cache {} read / {} write",
                format_tokens(self.cache_read_tokens),
                format_tokens(self.cache_write_tokens)
            ));
        }
        if !self.duration.is_zero() {
            parts.push(format_duration(self.duration));
        }
        parts.join(" · ")
    }
}

fn format_tokens(tokens: u64) -> String {
    match tokens {
        1_000_000.. => format!("{:.1}m", tokens as f64 / 1_000_000.0),
        1_000.. => format!("{:.1}k", tokens as f64 / 1_000.0),
        value => value.to_string(),
    }
}

fn format_duration(duration: Duration) -> String {
    if duration.as_secs() > 0 {
        format!("{:.1}s", duration.as_secs_f64())
    } else {
        format!("{}ms", duration.as_millis())
    }
}

#[cfg(test)]
mod tests {
    use super::TurnSummary;
    use std::time::Duration;

    #[test]
    fn status_line_includes_tools_usage_cache_and_duration() {
        let mut summary = TurnSummary::default();
        summary.set_tool_counts(2, 1, 1);
        summary.add_usage(&crate::headless::TokenUsage {
            input_tokens: 1_200,
            output_tokens: 300,
            cache_read_tokens: 40,
            cache_write_tokens: 20,
            cost: None,
            total_tokens: None,
            model_id: None,
            provider: None,
        });
        summary.set_duration(Duration::from_millis(2_500));

        assert_eq!(
            summary.status_line(),
            "Turn complete · 2 tools (1 ok, 1 failed) · 1.2k in / 300 out · cache 40 read / 20 write · 2.5s"
        );
    }

    #[test]
    fn empty_turn_has_a_safe_compact_summary() {
        assert_eq!(
            TurnSummary::default().status_line(),
            "Turn complete · no tools"
        );
    }
}
