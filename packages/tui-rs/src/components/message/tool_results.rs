//! Layout and previews of tool calls within a conversation.

use super::conversation_theme;
use crate::components::auto_chart::AutoChart;
use crate::state::{Message, ToolCallState, ToolCallStatus};
use crate::tool_output::{clamp_tool_output, format_tool_output_truncation, tool_output_limits};
use crate::tool_summary::summarize_tool_use;
use maestro_presentation::components::tool_result::{ToolPhase, ToolResult};
use ratatui::{
    style::Style,
    text::{Line, Span},
};
use std::collections::HashSet;
use unicode_width::UnicodeWidthStr;

#[cfg(test)]
mod tests;

pub(super) fn tool_phase(status: ToolCallStatus) -> ToolPhase {
    match status {
        ToolCallStatus::Pending => ToolPhase::Pending,
        ToolCallStatus::Running => ToolPhase::Running,
        ToolCallStatus::Completed => ToolPhase::Completed,
        ToolCallStatus::Failed => ToolPhase::Failed,
        ToolCallStatus::Cancelled => ToolPhase::Cancelled,
        ToolCallStatus::Blocked => ToolPhase::Blocked,
    }
}

pub(super) fn visible_script_child(
    call: &ToolCallState,
    expanded: Option<&HashSet<String>>,
) -> bool {
    call.parent_call_id
        .as_ref()
        .is_none_or(|parent| expanded.is_some_and(|set| set.contains(parent)))
        || matches!(
            call.status,
            ToolCallStatus::Pending
                | ToolCallStatus::Failed
                | ToolCallStatus::Blocked
                | ToolCallStatus::Cancelled
        )
}

pub(super) fn latest_chart_call<'a>(
    message: &'a Message,
    expanded_tools: Option<&HashSet<String>>,
    enabled: bool,
    width: u16,
) -> Option<(&'a str, &'a AutoChart)> {
    if !enabled || width < AutoChart::MIN_WIDTH {
        return None;
    }
    message
        .tool_calls
        .iter()
        .rev()
        .filter(|call| call.status == ToolCallStatus::Completed)
        .find_map(|call| {
            let chart = call.auto_chart.as_deref()?;
            if visible_script_child(call, expanded_tools) {
                return Some((call.call_id.as_str(), chart));
            }
            let parent_id = call.parent_call_id.as_deref()?;
            let parent = message.tool_calls.iter().find(|parent| {
                parent.call_id == parent_id
                    && parent.tool == "codemode"
                    && parent.status == ToolCallStatus::Completed
                    && visible_script_child(parent, expanded_tools)
            })?;
            Some((parent.call_id.as_str(), chart))
        })
}

pub(super) fn tool_result_lines(
    tc: &ToolCallState,
    expanded: bool,
    width: u16,
) -> Vec<Line<'static>> {
    let summary = if tc.tool == "codemode" {
        "Script".to_owned()
    } else if tc.status == ToolCallStatus::Completed {
        summarize_tool_use(&tc.tool, &tc.args)
    } else {
        crate::tool_summary::summarize_tool_intent(&tc.tool, &tc.args)
    };
    let arguments = if tc.tool == "codemode" && !expanded {
        String::new()
    } else {
        get_tool_args_preview(&tc.tool, &tc.args, width.saturating_sub(20) as usize)
    };
    // An expanded call replaces its chart, so readings that exist only in
    // structured details stay inspectable through the output.
    let output = match tc
        .auto_chart
        .as_deref()
        .filter(|_| expanded)
        .and_then(AutoChart::structured_readings)
    {
        Some(readings) => std::borrow::Cow::Owned(
            crate::components::auto_chart::chart_readings_output(&tc.output, &readings),
        ),
        None => std::borrow::Cow::Borrowed(tc.output.as_str()),
    };
    let clamp = clamp_tool_output(&output, tool_output_limits());
    let banner = format_tool_output_truncation(&clamp);
    ToolResult {
        phase: tool_phase(tc.status),
        summary: &summary,
        arguments: &arguments,
        output: &clamp.text,
        expanded,
        detail: &format!(
            "· {} #{}{}",
            tc.tool,
            tc.call_id,
            tc.duration_ms
                .map(|ms| format!(" · {ms} ms"))
                .unwrap_or_default()
        ),
        truncation: banner.as_deref(),
        theme: conversation_theme(),
    }
    .lines(width)
}

/// Get a preview of tool arguments based on tool type
pub(super) fn get_tool_args_preview(
    tool: &str,
    args: &serde_json::Value,
    max_len: usize,
) -> String {
    let preview = match tool.to_lowercase().as_str() {
        "bash" => args
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "read" => args
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "write" => args
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "edit" => args
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "glob" => args
            .get("pattern")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "grep" => args
            .get("pattern")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "task" => args
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "webfetch" | "websearch" => args
            .get("url")
            .or_else(|| args.get("query"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        _ => {
            // Generic: show first string value
            args.as_object()
                .and_then(|obj| {
                    obj.values()
                        .find_map(|v| v.as_str())
                        .map(std::string::ToString::to_string)
                })
                .unwrap_or_default()
        }
    };

    if preview.width() <= max_len {
        return preview;
    }
    let suffix = ".".repeat(max_len.min(3));
    let budget = max_len.saturating_sub(suffix.len());
    let mut clipped = String::new();
    let mut width = 0;
    let span = Span::raw(preview);
    for grapheme in span.styled_graphemes(Style::default()) {
        let next = grapheme.symbol.width();
        if width + next > budget {
            break;
        }
        clipped.push_str(grapheme.symbol);
        width += next;
    }
    clipped + &suffix
}
