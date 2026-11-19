//! Decode visible MCP time-series results without reading app-only metadata.

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
};

use super::{
    AutoChart, MAX_COLUMNS, MAX_LABEL_BYTES, MAX_ROWS, MAX_TEXT_BYTES, Series, chart_number,
    parse_time,
};
use serde_json::Value;

const CHART_DATA: &str = "Chart data: the JSON below repeats the values above as named series,";
const PARTIAL_BUCKET_NOTE: &str =
    "Note: rows marked \"(partial)\" cover an interval that is still in progress";
const WAREHOUSE_SUGGESTIONS: &str = "[Warehouse suggestions for views this query read.";

pub(super) fn is_unavailable_result(value: &Value) -> bool {
    ["isError", "truncated"]
        .into_iter()
        .any(|key| value.get(key).and_then(Value::as_bool) == Some(true))
}

pub(super) fn is_incomplete_text(text: &str) -> bool {
    text.contains("...truncated")
        || text.lines().any(|line| {
            let line = line.trim_start();
            line.starts_with("[SQL result preview")
                || line.starts_with("[Incomplete SQL result preview")
                || line.starts_with("[Access control]")
                || line.starts_with("[Data warehouse sync warnings")
                || (line.starts_with("[... ") && line.contains("bytes elided: MCP result exceeded"))
        })
}

pub(super) fn content_text(content: &Value) -> Option<Cow<'_, str>> {
    if let Some(text) = content.as_str() {
        return Some(Cow::Borrowed(text));
    }
    let blocks = content.as_array()?;
    if blocks.len() > MAX_COLUMNS {
        return None;
    }
    let mut text = String::new();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        let body = block.get("text")?.as_str()?;
        let separator = usize::from(!text.is_empty());
        if text
            .len()
            .saturating_add(body.len())
            .saturating_add(separator)
            > MAX_TEXT_BYTES
        {
            return None;
        }
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(body);
    }
    (!text.is_empty()).then_some(Cow::Owned(text))
}

/// PostHog's optimized Trends text labels comparison periods and in-progress buckets.
/// Strip only known footer sections so a ragged row cannot be mistaken for a notice.
pub(super) fn pipe_table_body(text: &str) -> Option<(&str, bool)> {
    let text = if let Some(comparison) = text.strip_prefix("Previous period:\n") {
        comparison.split_once("\n\nCurrent period:\n")?.1
    } else {
        text
    };
    let mut sections = text.split("\n\n");
    let table = sections.next()?;
    let mut partial_buckets = false;
    for section in sections {
        let section = section.trim();
        if section.starts_with(PARTIAL_BUCKET_NOTE) && !section.contains('\n') {
            partial_buckets = true;
        } else if section.starts_with(WAREHOUSE_SUGGESTIONS)
            && section.lines().skip(1).all(|line| line.starts_with("- "))
        {
            continue;
        } else if parse_chart_section(section).is_none() {
            return None;
        }
    }
    Some((table, partial_buckets))
}

pub(super) fn chart_data(text: &str) -> Option<Value> {
    text.split("\n\n").find_map(parse_chart_section)
}

pub(super) fn has_partial_bucket_note(text: &str) -> bool {
    text.lines()
        .any(|line| line.starts_with(PARTIAL_BUCKET_NOTE))
}

pub(super) fn is_trends_text(text: &str) -> bool {
    let text = text.trim_start();
    if let Some(previous) = text.strip_prefix("Previous period:\n") {
        return previous
            .split_once("\n\nCurrent period:\n")
            .is_some_and(|(_, current)| current.starts_with("Date|"));
    }
    text.starts_with("Date|")
}

fn parse_chart_section(section: &str) -> Option<Value> {
    let (preamble, json) = section.split_once('\n')?;
    preamble.starts_with(CHART_DATA).then_some(())?;
    let decoded: Value = serde_json::from_str(json).ok()?;
    decoded.get("series")?.as_array()?;
    Some(decoded)
}

/// Accept the explicit named-point payload and PostHog's Trends result arrays.
pub(super) fn from_series(value: &Value) -> Option<AutoChart> {
    let (items, named_points) = if let Some(series) = value.get("series") {
        (series.as_array()?, true)
    } else {
        (value.get("results")?.as_array()?, false)
    };
    if !(1..=MAX_COLUMNS).contains(&items.len()) {
        return None;
    }

    let mut axis = BTreeSet::new();
    let mut utc_axis = true;
    let mut columns = Vec::new();
    for item in items {
        if !named_points && item.get("compare_label").and_then(Value::as_str) == Some("previous") {
            continue;
        }
        let name = series_name(item, named_points)?;
        let data = item.get("data")?.as_array()?;
        if data.len() > MAX_ROWS {
            return None;
        }
        let pairs: Vec<_> = if named_points {
            data.iter()
                .map(|point| match point.as_array()?.as_slice() {
                    [time, number] => Some((time, number)),
                    _ => None,
                })
                .collect::<Option<_>>()?
        } else {
            let days = item.get("days")?.as_array()?;
            if days.len() != data.len() {
                return None;
            }
            days.iter().zip(data).collect()
        };
        let mut points = BTreeMap::new();
        let mut zoned = true;
        for (timestamp, number) in pairs {
            let (time, explicit_zone) = parse_time(timestamp.as_str()?)?;
            if points.insert(time, chart_number(number)?).is_some() {
                return None;
            }
            zoned &= explicit_zone;
        }
        if points.values().flatten().take(2).count() < 2 {
            continue;
        }
        axis.extend(points.keys().copied());
        if axis.len() > MAX_ROWS {
            return None;
        }
        utc_axis &= zoned;
        columns.push((name, points));
    }

    let axis: Vec<_> = axis.into_iter().collect();
    let series: Vec<_> = columns
        .into_iter()
        .filter_map(|(name, points)| {
            let values = axis
                .iter()
                .map(|time| points.get(time).copied().flatten())
                .collect();
            Series::from_values(&name, values)
        })
        .collect();
    if series
        .iter()
        .map(|entry| &entry.name)
        .collect::<BTreeSet<_>>()
        .len()
        != series.len()
    {
        return None;
    }
    AutoChart::from_ordered_series(axis, utc_axis, series)
}

fn series_name(value: &Value, named_points: bool) -> Option<String> {
    let fields: &[_] = if named_points {
        &["name"]
    } else {
        &["custom_name", "label", "name"]
    };
    let label = fields
        .iter()
        .filter_map(|field| value.get(*field)?.as_str())
        .find(|label| !label.trim().is_empty())?;
    if label.len() > MAX_LABEL_BYTES {
        return None;
    }
    let name = match value.get("breakdown_value").and_then(Value::as_str) {
        Some(breakdown) if !named_points && !breakdown.is_empty() && breakdown != label => {
            if label
                .len()
                .saturating_add(breakdown.len())
                .saturating_add(3)
                > MAX_LABEL_BYTES
            {
                return None;
            }
            format!("{label} ({breakdown})")
        }
        _ => label.to_string(),
    };
    Some(name)
}
