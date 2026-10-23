//! Estimates from the request built by the runtime, never from all registered tools.
use crate::token_counter::TokenCounter;
use maestro_ai::{ContentBlock, Message, MessageContent, RequestConfig};

#[derive(Debug, Clone, Default)]
pub struct RequestContextUsage {
    pub model: String,
    pub system: u64,
    pub conversation: u64,
    pub tool_results: u64,
    pub other: u64,
    pub tools: Vec<(String, u64)>,
}

/// A primary response measured against its own sealed request, before auxiliary usage.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ContextCalibration {
    pub request_id: String,
    pub generation: u64,
    pub estimated_input_tokens: u64,
    pub observed_input_tokens: u64,
}

impl ContextCalibration {
    pub fn from_usage(
        request_id: String,
        generation: u64,
        estimated_input_tokens: u64,
        input_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
    ) -> Option<Self> {
        let observed_input_tokens = input_tokens
            .checked_add(cache_read_tokens)?
            .checked_add(cache_write_tokens)?;
        Some(Self {
            request_id,
            generation,
            estimated_input_tokens,
            observed_input_tokens,
        })
    }
}

/// Content-free aggregates for the existing turn telemetry pipeline.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextEstimationMeasurements {
    pub responses: u64,
    pub estimated_input_tokens: u64,
    pub observed_input_tokens: u64,
    pub absolute_error_tokens: u64,
    pub underestimated_responses: u64,
}

impl ContextEstimationMeasurements {
    pub fn record(&mut self, observation: &ContextCalibration) {
        self.responses = self.responses.saturating_add(1);
        self.estimated_input_tokens = self
            .estimated_input_tokens
            .saturating_add(observation.estimated_input_tokens);
        self.observed_input_tokens = self
            .observed_input_tokens
            .saturating_add(observation.observed_input_tokens);
        self.absolute_error_tokens = self.absolute_error_tokens.saturating_add(
            observation
                .estimated_input_tokens
                .abs_diff(observation.observed_input_tokens),
        );
        self.underestimated_responses = self.underestimated_responses.saturating_add(u64::from(
            observation.estimated_input_tokens < observation.observed_input_tokens,
        ));
    }
}

impl RequestContextUsage {
    /// Includes stable instructions, tools, history and the sealed volatile tail.
    pub fn total(&self) -> Option<u64> {
        [
            self.system,
            self.conversation,
            self.tool_results,
            self.other,
        ]
        .into_iter()
        .chain(self.tools.iter().map(|(_, tokens)| *tokens))
        .try_fold(0_u64, u64::checked_add)
    }

    pub fn from_request(
        messages: &[Message],
        config: &RequestConfig,
        counter: &TokenCounter,
    ) -> Self {
        let count = |text: &str| counter.count(text);
        let mut usage = Self {
            model: config.model.clone(),
            system: config.system.as_deref().map_or(0, count),
            tools: config
                .tools
                .iter()
                .map(|tool| {
                    (
                        tool.name.clone(),
                        count(&serde_json::to_string(tool).unwrap_or_default()),
                    )
                })
                .collect(),
            ..Self::default()
        };
        for message in messages {
            match &message.content {
                MessageContent::Text(text) => usage.conversation += count(text),
                MessageContent::Blocks(blocks) => {
                    for block in blocks {
                        match block {
                            ContentBlock::Text { text }
                            | ContentBlock::AttributedText { text, .. } => {
                                usage.conversation += count(text)
                            }
                            ContentBlock::Thinking { thinking, .. } => {
                                usage.other += count(thinking);
                            }
                            ContentBlock::ToolUse { name, input, .. } => {
                                usage.tool_results += count(name) + count(&input.to_string());
                            }
                            ContentBlock::ToolResult { content, .. } => {
                                usage.tool_results += count(content);
                            }
                            ContentBlock::Image { .. } => {
                                usage.other += crate::token_estimation::IMAGE_TOKEN_ESTIMATE;
                            }
                        }
                    }
                }
            }
        }
        if let Some(tail) = config
            .cache_topology
            .as_ref()
            .and_then(|prepared| prepared.volatile_tail())
        {
            usage.conversation += count(tail);
        }
        usage
            .tools
            .sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        usage
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maestro_ai::Tool;

    #[test]
    fn calibration_counts_cache_buckets_and_rejects_overflow() {
        let observation =
            ContextCalibration::from_usage("request-2".into(), 2, 100, 20, 300, 40).unwrap();
        assert_eq!(observation.observed_input_tokens, 360);
        assert_eq!(observation.generation, 2);
        assert_eq!(observation.request_id, "request-2");
        assert!(
            ContextCalibration::from_usage("overflow".into(), 1, 100, u64::MAX, 1, 0).is_none()
        );
        let mut measurements = ContextEstimationMeasurements::default();
        measurements.record(&observation);
        assert_eq!(measurements.responses, 1);
        assert_eq!(measurements.absolute_error_tokens, 260);
        assert_eq!(measurements.underestimated_responses, 1);
    }

    use std::sync::Arc;

    #[test]
    fn volatile_tail_is_counted_without_becoming_system_context() {
        let mut config = RequestConfig::default();
        let counter = TokenCounter::new(Some(config.model.clone()));
        let before = RequestContextUsage::from_request(&[], &config, &counter);
        config.cache_topology = Some(
            maestro_ai::cache_topology::PreparedPrompt::prepare(&[], &config, "scope".into(), None)
                .unwrap()
                .with_volatile_tail(Some("current plan ".repeat(100))),
        );
        let after = RequestContextUsage::from_request(&[], &config, &counter);
        assert_eq!(before.system, after.system);
        assert!(after.conversation > before.conversation);
    }

    #[test]
    fn reports_only_schemas_in_the_prepared_request() {
        let tool = Tool::new("fixture_integration", "large optional schema")
            .with_schema(serde_json::json!({"description": "schema ".repeat(4000)}));
        let mut config = RequestConfig {
            tools: Arc::new(vec![tool]),
            ..Default::default()
        };
        let counter = TokenCounter::new(Some(config.model.clone()));
        let with_tool = RequestContextUsage::from_request(&[], &config, &counter);
        assert_eq!(with_tool.tools.len(), 1);
        assert!(with_tool.tools[0].1 > 1000);
        config.tools = Arc::new(Vec::new());
        let without_tool = RequestContextUsage::from_request(&[], &config, &counter);
        assert!(without_tool.tools.is_empty());
        assert_eq!(without_tool.system, with_tool.system);
    }
}
