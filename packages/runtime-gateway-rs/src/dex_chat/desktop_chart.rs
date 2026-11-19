//! Bounded chart presentation for native desktop tool results.

use maestro_dex_host::dex_loop::{Output, OutputBlock};
use maestro_local_host::tool_chart::ToolChart;
use serde_json::{Value, json};

const MAX_CHART_BYTES: usize = 12 * 1024;

pub(super) fn project(
    call_id: &str,
    tool_name: &str,
    args: Option<&Value>,
    output: &Output,
) -> Option<Value> {
    let code_mode = tool_name == "codemode";
    if !code_mode && !is_posthog_mcp(tool_name) {
        return None;
    }
    let raw = match output {
        Output::Text(text) => text,
        Output::Blocks(blocks) if code_mode => {
            return blocks.iter().rev().find_map(|block| match block {
                OutputBlock::Text { text } => project_text(call_id, code_mode, args, text.as_str()),
                OutputBlock::Image { .. } => None,
            });
        }
        _ => return None,
    };
    project_text(call_id, code_mode, args, raw)
}

fn project_text(call_id: &str, code_mode: bool, args: Option<&Value>, raw: &str) -> Option<Value> {
    if raw.len() > MAX_CHART_BYTES {
        return None;
    }
    let chart = ToolChart::from_result(raw, None)?;
    let date_only = chart
        .axis
        .iter()
        .all(|time| time.time() == chrono::NaiveTime::MIN);
    let time_format = if date_only {
        "%Y-%m-%d"
    } else {
        "%Y-%m-%d %H:%M:%S%.f"
    };
    let axis: Vec<_> = chart
        .axis
        .iter()
        .map(|time| {
            [
                json!(time.timestamp_micros()),
                json!(time.format(time_format).to_string()),
            ]
        })
        .collect();
    let series: Vec<_> = chart
        .series
        .iter()
        .map(|series| json!({ "name": series.name, "values": series.values }))
        .collect();
    let projection = json!({
        "callId": call_id,
        "source": if code_mode { "codemode" } else { "posthog" },
        "axis": axis,
        "dateOnly": date_only,
        "utcAxis": chart.utc_axis,
        "series": series,
        "omittedOpenBuckets": chart.omitted_open_buckets,
        "omittedSeries": chart.omitted_series,
        "args": args,
        "raw": raw,
    });
    (projection.to_string().len() <= MAX_CHART_BYTES).then_some(projection)
}

fn is_posthog_mcp(tool_name: &str) -> bool {
    tool_name
        .strip_prefix("mcp__")
        .and_then(|name| name.split_once("__"))
        .is_some_and(|(server, _)| {
            server
                .split(['_', '-', ':'])
                .any(|segment| segment.eq_ignore_ascii_case("posthog"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRENDS: &str = "Date|turns\n2026-11-15|5\n2026-11-16|N/A\n2026-11-17|11\n2026-11-18 (partial)|2\n\nNote: rows marked \"(partial)\" cover an interval that is still in progress.";

    #[test]
    fn projects_posthog_tool_and_code_mode_with_shared_terminal_semantics() {
        let direct = Output::Text(TRENDS.to_owned());
        let script = Output::Blocks(vec![OutputBlock::Text {
            text: TRENDS.to_owned(),
        }]);
        for (tool, output) in [
            ("mcp__plugin_posthog_posthog__posthog_exec", &direct),
            ("codemode", &script),
        ] {
            let args = json!({"query": "turns"});
            let chart = project("call-1", tool, Some(&args), output).unwrap();
            assert_eq!(chart["series"][0]["values"], json!([5.0, null, 11.0]));
            assert_eq!(chart["axis"][2][1], "2026-11-17");
            assert_eq!(chart["omittedOpenBuckets"], 1);
            assert_eq!(chart["args"], args);
            assert_eq!(chart["raw"], TRENDS);
        }
    }

    #[test]
    fn unavailable_and_unrelated_results_have_no_projection() {
        let table = Output::Text(TRENDS.to_owned());
        assert!(project("a", "shell", None, &table).is_none());
        let preview = Output::Text(format!(
            "{TRENDS}\n[SQL result preview: Showing 3 of 4 rows.]"
        ));
        assert!(project("a", "mcp__posthog__exec", None, &preview).is_none());
        let large_args = json!({"code": "x".repeat(MAX_CHART_BYTES)});
        assert!(project("a", "codemode", Some(&large_args), &table).is_none());
    }
}
