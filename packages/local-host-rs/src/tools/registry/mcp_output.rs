//! Human-facing MCP rendering; script data retains the full owner envelope.

use crate::mcp::McpContent;

/// Text handed to the model for one MCP tool result.
///
/// Joins the text content blocks (falling back to a pretty-printed dump of
/// non-text content) and strips terminal control characters. The Native agent
/// owns the later model-facing clamp because only that layer knows whether the
/// current tool allowlist lets the model retrieve a spill file with `read`.
pub(super) fn mcp_model_output(content: &[McpContent]) -> String {
    let text_output = content
        .iter()
        .filter_map(|content| match content {
            McpContent::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let output = if text_output.is_empty() {
        serde_json::to_string_pretty(content)
            .unwrap_or_else(|_| "MCP tool returned non-text content".to_string())
    } else {
        text_output
    };
    crate::output_sanitize::sanitize_control_chars(&output)
}
