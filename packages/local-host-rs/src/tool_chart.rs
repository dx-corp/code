//! Parse bounded tool results into time-series data shared by the terminal and desktop.

mod mcp;

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use serde_json::Value;
use std::collections::BTreeSet;

const MAX_TEXT_BYTES: usize = 64 * 1024;
const MAX_ROWS: usize = 512;
const MAX_COLUMNS: usize = 32;
pub const MAX_SERIES: usize = 5;
const MAX_LABEL_BYTES: usize = 256;

#[derive(Debug)]
pub struct ToolChart {
    pub axis: Vec<DateTime<Utc>>,
    pub utc_axis: bool,
    /// Every charted column, including those beyond the plotted `MAX_SERIES`.
    pub series: Vec<ChartSeries>,
    pub omitted_open_buckets: usize,
    /// Rows came only from structured details, so the text output may not show them.
    pub structured: bool,
}

#[derive(Debug)]
pub struct ChartSeries {
    pub name: String,
    pub values: Vec<Option<f64>>,
    pub min: f64,
    pub max: f64,
}

struct PipeChart {
    chart: ToolChart,
    open_buckets: BTreeSet<DateTime<Utc>>,
}

impl ToolChart {
    pub fn from_result(output: &str, details: Option<&Value>) -> Option<Self> {
        if details.is_some_and(mcp::is_unavailable_result) {
            return None;
        }
        let structured = details.and_then(|value| value.get("structuredContent"));
        let content = details.and_then(|value| mcp::content_text(value.get("content")?));
        let visible = [content.as_deref(), Some(output)].into_iter().flatten();
        if let Some(text) = visible
            .clone()
            .find(|text| mcp::has_partial_bucket_note(text))
            .or_else(|| visible.clone().find(|text| mcp::is_trends_text(text)))
        {
            return Self::from_trends_text(text, structured, 2);
        }
        structured
            .and_then(|value| Self::from_value(value, 2))
            .map(|chart| Self {
                structured: true,
                ..chart
            })
            .or_else(|| Self::from_text(content.as_deref()?, 2))
            .or_else(|| Self::from_text(output, 3))
    }

    fn from_text(output: &str, depth: usize) -> Option<Self> {
        if output.len() > MAX_TEXT_BYTES || mcp::is_incomplete_text(output) {
            return None;
        }
        let text = output.trim();
        let json = text
            .strip_prefix("```json")
            .and_then(|body| body.strip_suffix("```"))
            .map_or(text, str::trim);
        if let Some(value) = json
            .starts_with(['[', '{'])
            .then(|| serde_json::from_str(json).ok())
            .flatten()
        {
            return Self::from_value(&value, depth);
        }

        if mcp::is_trends_text(text) || mcp::has_partial_bucket_note(text) {
            return Self::from_trends_text(text, None, depth);
        }
        let named = mcp::chart_data(text).and_then(|value| Self::from_value(&value, 0));
        match Self::from_pipe_table(text) {
            Some(table) => table.reconcile(named),
            None => named,
        }
    }

    fn from_trends_text(text: &str, structured: Option<&Value>, depth: usize) -> Option<Self> {
        if text.len() > MAX_TEXT_BYTES || mcp::is_incomplete_text(text) {
            return None;
        }
        let table = Self::from_pipe_table(text.trim())?;
        let structured = structured.and_then(|value| Self::from_value(value, depth));
        let named = mcp::chart_data(text).and_then(|value| Self::from_value(&value, 0));
        // Validate both numeric representations against the visible buckets when present.
        if let (Some(_), Some(named)) = (&structured, &named) {
            table.reconciled_series(named)?;
        }
        table.reconcile(structured.or(named))
    }

    fn from_pipe_table(text: &str) -> Option<PipeChart> {
        // SQL text results use a header followed by pipe-separated rows.
        // Require rectangular data and recognize only known query footers.
        if !text.contains('|') || text.contains("...truncated") {
            return None;
        }
        let (table, has_partial_bucket) = mcp::pipe_table_body(text)?;
        let mut lines = table.trim().lines();
        let columns: Vec<_> = lines
            .next()?
            .split('|')
            .map(str::trim)
            .map(Value::from)
            .collect();
        if !(2..=MAX_COLUMNS).contains(&columns.len()) {
            return None;
        }
        let trend_table = columns[0].as_str() == Some("Date");
        let mut rows = Vec::new();
        let mut open_buckets = BTreeSet::new();
        for line in lines {
            let cells: Vec<_> = line.split('|').map(str::trim).collect();
            if cells.len() != columns.len() || rows.len() + open_buckets.len() == MAX_ROWS {
                return None;
            }
            if let Some(time) = (trend_table && has_partial_bucket)
                .then(|| cells[0].strip_suffix(" (partial)"))
                .flatten()
            {
                if !open_buckets.insert(parse_time(time)?.0) {
                    return None;
                }
                continue;
            }
            let row = cells
                .into_iter()
                .map(|cell| {
                    if cell == "(null)" || (trend_table && cell == "N/A") {
                        Value::Null
                    } else if let Some(number) = cell
                        .parse::<f64>()
                        .ok()
                        .and_then(serde_json::Number::from_f64)
                    {
                        Value::Number(number)
                    } else {
                        Value::from(cell)
                    }
                })
                .collect();
            rows.push(Value::Array(row));
        }
        let mut chart = Self::from_table(&rows, Some(&Value::Array(columns)))?;
        if chart.axis.iter().any(|time| open_buckets.contains(time)) {
            return None;
        }
        chart.omitted_open_buckets = open_buckets.len();
        Some(PipeChart {
            chart,
            open_buckets,
        })
    }

    fn from_value(value: &Value, depth: usize) -> Option<Self> {
        if mcp::is_unavailable_result(value) {
            return None;
        }
        if let Some(text) = value
            .get("content")
            .filter(|_| depth > 0)
            .and_then(mcp::content_text)
            .filter(|text| mcp::is_trends_text(text) || mcp::has_partial_bucket_note(text))
        {
            return Self::from_trends_text(&text, value.get("structuredContent"), depth - 1);
        }
        let table = ["results", "rows", "data"]
            .into_iter()
            .find_map(|key| value.get(key).and_then(Value::as_array));
        if let Some(chart) = table
            .or_else(|| value.as_array())
            .and_then(|rows| Self::from_table(rows, value.get("columns")))
            .or_else(|| mcp::from_series(value))
        {
            return Some(chart);
        }

        (depth > 0).then_some(())?;
        ["structuredContent", "data", "result", "results", "output"]
            .into_iter()
            .filter_map(|key| value.get(key))
            .find_map(|inner| Self::from_value(inner, depth - 1))
            .or_else(|| Self::from_content(value.get("content")?, depth - 1))
    }

    fn from_content(content: &Value, depth: usize) -> Option<Self> {
        let text = mcp::content_text(content)?;
        Self::from_text(&text, depth)
    }

    fn from_table(rows: &[Value], columns: Option<&Value>) -> Option<Self> {
        if !(2..=MAX_ROWS).contains(&rows.len()) {
            return None;
        }
        let objects = rows[0].is_object();
        let names: Vec<&str> = if objects {
            let mut names = BTreeSet::new();
            for row in rows {
                let row = row.as_object()?;
                if row.len() > MAX_COLUMNS {
                    return None;
                }
                names.extend(row.keys().map(String::as_str));
                if names.len() > MAX_COLUMNS {
                    return None;
                }
            }
            names.into_iter().collect()
        } else {
            let columns = columns?.as_array()?;
            if columns.len() > MAX_COLUMNS {
                return None;
            }
            columns.iter().map(Value::as_str).collect::<Option<_>>()?
        };
        if !(2..=MAX_COLUMNS).contains(&names.len())
            || names
                .iter()
                .enumerate()
                .any(|(i, name)| names[..i].contains(name))
        {
            return None;
        }

        let cells: Vec<Vec<_>> = rows
            .iter()
            .map(|row| {
                if objects {
                    let row = row.as_object()?;
                    Some(names.iter().map(|name| row.get(*name)).collect())
                } else {
                    let row = row.as_array()?;
                    (row.len() == names.len()).then(|| row.iter().map(Some).collect())
                }
            })
            .collect::<Option<_>>()?;
        let mut axes = (0..names.len()).filter_map(|column| {
            let times: Vec<_> = cells
                .iter()
                .map(|row| parse_time(row[column]?.as_str()?))
                .collect::<Option<_>>()?;
            Some((column, times))
        });
        let (time_column, times) = axes.next()?;
        if axes.next().is_some() {
            return None;
        }

        let mut order: Vec<_> = (0..times.len()).collect();
        order.sort_unstable_by_key(|index| times[*index].0);
        let axis: Vec<_> = order.iter().map(|index| times[*index].0).collect();
        if axis.windows(2).any(|pair| pair[0] == pair[1]) {
            return None;
        }

        let series = names
            .into_iter()
            .enumerate()
            .filter(|(column, _)| *column != time_column)
            .filter_map(|(column, name)| {
                let values: Vec<_> = order
                    .iter()
                    .map(|index| cells[*index][column].map_or(Some(None), chart_number))
                    .collect::<Option<_>>()?;
                ChartSeries::from_values(name, values)
            })
            .collect();
        Self::from_ordered_series(axis, times.iter().all(|(_, explicit)| *explicit), series)
    }

    fn from_ordered_series(
        axis: Vec<DateTime<Utc>>,
        utc_axis: bool,
        series: Vec<ChartSeries>,
    ) -> Option<Self> {
        (!series.is_empty()).then_some(Self {
            axis,
            utc_axis,
            series,
            omitted_open_buckets: 0,
            structured: false,
        })
    }

    /// The series a chart draws: the first `MAX_SERIES`.
    pub fn plotted(&self) -> &[ChartSeries] {
        &self.series[..self.series.len().min(MAX_SERIES)]
    }

    /// Series kept in `series` but not drawn.
    pub fn omitted_series(&self) -> usize {
        self.series.len() - self.plotted().len()
    }
}

impl PipeChart {
    fn reconcile(mut self, precise: Option<ToolChart>) -> Option<ToolChart> {
        if let Some(precise) = precise {
            self.chart.series = self.reconciled_series(&precise)?;
        }
        Some(self.chart)
    }

    fn reconciled_series(&self, precise: &ToolChart) -> Option<Vec<ChartSeries>> {
        let chart = &self.chart;
        if precise.axis.iter().any(|time| {
            chart.axis.binary_search(time).is_err() && !self.open_buckets.contains(time)
        }) {
            return None;
        }
        // The table leaves out a metric with fewer than two completed readings,
        // while the precise series may still count its open bucket. Such a
        // metric is absent from the table for that reason alone; any other
        // missing metric is a disagreement.
        let mut left_out = 0usize;
        for full in &precise.series {
            if chart.series.iter().any(|visible| visible.name == full.name) {
                continue;
            }
            let completed = full
                .values
                .iter()
                .zip(&precise.axis)
                .filter(|(value, time)| value.is_some() && chart.axis.binary_search(time).is_ok())
                .count();
            if completed >= 2 {
                return None;
            }
            left_out += 1;
        }
        // Both charts keep every series, plotted or not.
        if chart.series.len() + left_out != precise.series.len() {
            return None;
        }
        let positions: Vec<_> = chart
            .axis
            .iter()
            .map(|time| precise.axis.binary_search(time).ok())
            .collect();
        chart
            .series
            .iter()
            .map(|visible| {
                let full = precise
                    .series
                    .iter()
                    .find(|entry| entry.name == visible.name)?;
                let values = visible
                    .values
                    .iter()
                    .zip(&positions)
                    .map(
                        |(cell, index)| match (*cell, index.and_then(|idx| full.values[idx])) {
                            (None, None) => Some(None),
                            (Some(visible), Some(full)) if same_displayed_value(visible, full) => {
                                Some(Some(full))
                            }
                            _ => None,
                        },
                    )
                    .collect::<Option<_>>()?;
                ChartSeries::from_values(&visible.name, values)
            })
            .collect()
    }
}

fn same_displayed_value(visible: f64, precise: f64) -> bool {
    // PostHog's Trends formatter rounds fractional cells to five decimal places.
    let tolerance = 0.000_005 + f64::EPSILON * visible.abs().max(precise.abs());
    (visible - precise).abs() <= tolerance
}

impl ChartSeries {
    fn from_values(name: &str, values: Vec<Option<f64>>) -> Option<Self> {
        if name.len() > MAX_LABEL_BYTES {
            return None;
        }
        let name = crate::output_sanitize::sanitize_control_chars(name)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let mut observed = values.iter().flatten().copied();
        let first = observed.next()?;
        let second = observed.next()?;
        let (min, max) = observed.fold((first.min(second), first.max(second)), |(min, max), n| {
            (min.min(n), max.max(n))
        });
        (!name.is_empty()).then_some(Self {
            name,
            values,
            min,
            max,
        })
    }
}

fn chart_number(value: &Value) -> Option<Option<f64>> {
    if value.is_null() {
        Some(None)
    } else {
        value.as_f64().filter(|n| n.is_finite()).map(Some)
    }
}

fn parse_time(raw: &str) -> Option<(DateTime<Utc>, bool)> {
    // Keep a timezone-free bucket on its wall-clock axis. Only zoned input earns a UTC label.
    if !(10..=40).contains(&raw.len()) {
        return None;
    }
    if raw.len() == 10 {
        return Some((
            NaiveDate::parse_from_str(raw, "%Y-%m-%d")
                .ok()?
                .and_time(chrono::NaiveTime::MIN)
                .and_utc(),
            false,
        ));
    }
    if let Ok(time) = DateTime::parse_from_rfc3339(raw) {
        return Some((time.to_utc(), true));
    }
    [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
    ]
    .into_iter()
    .find_map(|format| NaiveDateTime::parse_from_str(raw, format).ok())
    .map(|time| (time.and_utc(), false))
}
