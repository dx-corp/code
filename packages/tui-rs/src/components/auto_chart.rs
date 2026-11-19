//! Derive a terminal preview from bounded, unambiguous time-series tool results.

mod mcp;

use crate::themes::Theme;
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use maestro_ui::{UiTheme, field_format::truncate_line_with_ellipsis, localization};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    symbols::Marker,
    text::{Line, Span},
    widgets::{
        Widget,
        canvas::{Canvas, Line as Segment, Points},
    },
};
use serde_json::Value;
use std::collections::BTreeSet;
use unicode_width::UnicodeWidthStr;

const MAX_TEXT_BYTES: usize = 64 * 1024;
const MAX_ROWS: usize = 512;
const MAX_COLUMNS: usize = 32;
const MAX_SERIES: usize = 5;
const MAX_LABEL_BYTES: usize = 256;
const PLOT_HEIGHT: u16 = 2;
const F64_EXACT_INTEGER_LIMIT: f64 = (1_u64 << 53) as f64;

#[derive(Debug)]
pub(crate) struct AutoChart {
    axis: Vec<DateTime<Utc>>,
    utc_axis: bool,
    series: Vec<Series>,
    omitted_series: usize,
    omitted_open_buckets: usize,
}

#[derive(Debug)]
struct Series {
    name: String,
    values: Vec<Option<f64>>,
    min: f64,
    max: f64,
}

struct PipeChart {
    chart: AutoChart,
    open_buckets: BTreeSet<DateTime<Utc>>,
}

impl AutoChart {
    pub(crate) const MIN_WIDTH: u16 = 44;

    pub(crate) fn from_result(output: &str, details: Option<&Value>) -> Option<Self> {
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
        // This crate is edition 2021, which has no let chains.
        if json.starts_with(['[', '{']) {
            if let Ok(value) = serde_json::from_str(json) {
                return Self::from_value(&value, depth);
            }
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
            if trend_table && has_partial_bucket {
                if let Some(time) = cells[0].strip_suffix(" (partial)") {
                    if !open_buckets.insert(parse_time(time)?.0) {
                        return None;
                    }
                    continue;
                }
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
            .and_then(mcp::content_text)
            .filter(|text| {
                depth > 0 && (mcp::is_trends_text(text) || mcp::has_partial_bucket_note(text))
            })
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
                Series::from_values(name, values)
            })
            .collect();
        Self::from_ordered_series(axis, times.iter().all(|(_, explicit)| *explicit), series)
    }

    fn from_ordered_series(
        axis: Vec<DateTime<Utc>>,
        utc_axis: bool,
        mut series: Vec<Series>,
    ) -> Option<Self> {
        let omitted_series = series.len().saturating_sub(MAX_SERIES);
        series.truncate(MAX_SERIES);
        (!series.is_empty()).then_some(Self {
            axis,
            utc_axis,
            series,
            omitted_series,
            omitted_open_buckets: 0,
        })
    }

    pub(crate) fn height(&self) -> u16 {
        1 + self.series.len() as u16 * (1 + PLOT_HEIGHT) + 1 + u16::from(self.omitted_series > 0)
    }

    pub(crate) fn lines(&self, width: u16, active_theme: &Theme) -> Vec<Line<'static>> {
        let theme = active_theme.ui_theme();
        let colors = series_colors(
            [
                "accent",
                "syntax_function",
                "syntax_number",
                "syntax_string",
                "syntax_type",
                "syntax_variable",
                "syntax_keyword",
            ]
            .into_iter()
            .filter_map(|name| active_theme.get_color(name)),
            theme.text,
        );
        let axis_width = usize::from(width.min(124));
        let plot_width = width.min(124).saturating_sub(6);
        let muted = Style::default().fg(theme.muted);
        let first = self.axis[0];
        let last = self.axis[self.axis.len() - 1];
        let offsets: Vec<_> = self.axis.iter().map(|time| seconds(*time, first)).collect();
        let duration = seconds(last, first);

        let mut lines = vec![self.title(usize::from(width), theme)];
        for (series, color) in self.series.iter().zip(colors) {
            lines.push(series.summary(usize::from(width), theme, color));
            lines.extend(series.plot(&offsets, duration, plot_width, color, theme.border));
        }

        let midnight = chrono::NaiveTime::MIN;
        let date_only = self.axis.iter().all(|time| time.time() == midnight);
        let (start, mut end) = if date_only {
            (
                first.format("%Y-%m-%d").to_string(),
                last.format("%Y-%m-%d").to_string(),
            )
        } else if first.date_naive() == last.date_naive() && duration < 1.0 {
            (
                first.format("%H:%M:%S%.f").to_string(),
                last.format("%H:%M:%S%.f").to_string(),
            )
        } else {
            (
                first.format("%m-%d %H:%M:%S").to_string(),
                last.format("%m-%d %H:%M:%S").to_string(),
            )
        };
        if self.utc_axis && !date_only {
            end.push_str(" UTC");
        }
        let endpoint_width = start.len() + end.len();
        let indent = 6.min(axis_width.saturating_sub(endpoint_width + 1));
        let gap = axis_width.saturating_sub(endpoint_width + indent).max(1);
        lines.push(Line::from(Span::styled(
            format!("{}{start}{}{end}", " ".repeat(indent), " ".repeat(gap)),
            muted,
        )));
        if self.omitted_series > 0 {
            lines.push(Line::from(Span::styled(
                format!(
                    "    {}",
                    maestro_ui::localization::format(
                        "{0} more series in tool output",
                        &[self.omitted_series.to_string()],
                    ),
                ),
                muted,
            )));
        }
        lines
    }

    fn title(&self, width: usize, theme: UiTheme) -> Line<'static> {
        let mut line = Line::from(Span::styled(
            format!("    {}", localization::tr("Auto chart")),
            Style::default()
                .fg(theme.focus)
                .add_modifier(Modifier::BOLD),
        ));
        let timestamps = format!(
            " · {}",
            localization::format("{0} timestamps", &[self.axis.len().to_string()])
        );
        let scales = if self.series.len() > 1 {
            format!(" · {}", localization::tr("independent scales"))
        } else {
            String::new()
        };
        let muted = Style::default().fg(theme.muted);
        if self.omitted_open_buckets > 0 {
            let omitted = format!(
                " · {}",
                localization::format(
                    "Open buckets omitted ({0})",
                    &[self.omitted_open_buckets.to_string()],
                ),
            );
            if line.width() + omitted.width() <= width {
                line.spans.push(Span::styled(omitted, muted));
            }
        }
        if line.width() + timestamps.width() + scales.width() <= width {
            line.spans.push(Span::styled(timestamps, muted));
        }
        if !scales.is_empty() && line.width() + scales.width() <= width {
            line.spans.push(Span::styled(scales, muted));
        }
        truncate_line_with_ellipsis(line, width)
    }
}

impl PipeChart {
    fn reconcile(mut self, precise: Option<AutoChart>) -> Option<AutoChart> {
        if let Some(precise) = precise {
            self.chart.series = self.reconciled_series(&precise)?;
        }
        Some(self.chart)
    }

    fn reconciled_series(&self, precise: &AutoChart) -> Option<Vec<Series>> {
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
        if chart.series.len() + chart.omitted_series + left_out
            != precise.series.len() + precise.omitted_series
        {
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
                Series::from_values(&visible.name, values)
            })
            .collect()
    }
}

fn same_displayed_value(visible: f64, precise: f64) -> bool {
    // PostHog's Trends formatter rounds fractional cells to five decimal places.
    let tolerance = 0.000_005 + f64::EPSILON * visible.abs().max(precise.abs());
    (visible - precise).abs() <= tolerance
}

impl Series {
    fn from_values(name: &str, values: Vec<Option<f64>>) -> Option<Self> {
        if name.len() > MAX_LABEL_BYTES {
            return None;
        }
        let name = maestro_local_host::output_sanitize::sanitize_control_chars(name)
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

    fn summary(&self, width: usize, theme: UiTheme, color: Color) -> Line<'static> {
        let label = Line::from(vec![
            Span::raw("    "),
            Span::styled(
                self.name.clone(),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
        ]);
        let latest = self.values.last().copied().flatten();
        let last_label = format!("  {} ", localization::tr("Last"));
        let last_value = latest.map_or_else(|| "—".to_string(), format_number);
        let last_width = last_label.width() + last_value.width();
        if label.width().min(12) + last_width > width {
            return truncate_line_with_ellipsis(label, width);
        }

        // Optional fields can use at most the space beyond a readable metric name.
        let stats_budget = width.saturating_sub(label.width().min(width / 2));
        let muted = Style::default().fg(theme.muted);
        let mut stats = vec![
            Span::styled(last_label, muted),
            Span::styled(last_value, Style::default().fg(theme.text)),
        ];
        let mut stats_width = last_width;
        let min = format_number(self.min);
        let max = format_number(self.max);
        let range = if min == max && self.min.abs().max(self.max.abs()) >= F64_EXACT_INTEGER_LIMIT {
            String::new()
        } else {
            format!("  {} {min}–{max}", localization::tr("Range"))
        };
        for fact in [range, self.change_text(latest)] {
            let fact_width = fact.width();
            if fact_width == 0 || stats_width + fact_width > stats_budget {
                break;
            }
            stats_width += fact_width;
            stats.push(Span::styled(fact, muted));
        }
        let mut summary = truncate_line_with_ellipsis(label, width - stats_width);
        summary.spans.extend(stats);
        summary
    }

    fn change_text(&self, latest: Option<f64>) -> String {
        let first = self.values.first().copied().flatten();
        let Some((first, last)) = first.zip(latest) else {
            return String::new();
        };
        let change = last - first;
        // Parsed integers at this boundary may already have lost low bits in f64.
        if first.abs().max(last.abs()).max(change.abs()) >= F64_EXACT_INTEGER_LIMIT {
            return String::new();
        }
        let sign = match change {
            number if number > 0.0 => "+",
            number if number < 0.0 => "−",
            _ => "",
        };
        format!(
            "  {} {sign}{}",
            localization::tr("Change"),
            format_change(change.abs()),
        )
    }

    fn plot(
        &self,
        offsets: &[f64],
        duration: f64,
        width: u16,
        color: Color,
        border: Color,
    ) -> Vec<Line<'static>> {
        let area = Rect::new(0, 0, width, PLOT_HEIGHT);
        let mut buffer = Buffer::empty(area);
        // Scaling before subtracting keeps the range finite even with opposite-sign extremes.
        let magnitude = self.max.abs().max(self.min.abs()).max(f64::MIN_POSITIVE);
        let low = self.min / magnitude;
        let span = self.max / magnitude - low;
        let points: Vec<_> = offsets
            .iter()
            .zip(&self.values)
            .map(|(&x, value)| {
                value.map(|n| {
                    (
                        x,
                        if span == 0.0 {
                            0.5
                        } else {
                            (n / magnitude - low) / span
                        },
                    )
                })
            })
            .collect();
        let samples: Vec<_> = points.iter().flatten().copied().collect();

        Canvas::default()
            .marker(Marker::Braille)
            .x_bounds([0.0, duration])
            .y_bounds([0.0, 1.0])
            .paint(|ctx| {
                for pair in points.windows(2) {
                    if let [Some((x1, y1)), Some((x2, y2))] = pair {
                        ctx.draw(&Segment {
                            x1: *x1,
                            y1: *y1,
                            x2: *x2,
                            y2: *y2,
                            color,
                        });
                    }
                }
                ctx.draw(&Points {
                    coords: &samples,
                    color,
                });
            })
            .render(area, &mut buffer);

        (0..PLOT_HEIGHT)
            .map(|y| {
                let dots: String = (0..width)
                    .filter_map(|x| buffer.cell((x, y)))
                    .map(|cell| cell.symbol())
                    .collect();
                Line::from(vec![
                    Span::styled("    │ ", Style::default().fg(border)),
                    Span::styled(dots, Style::default().fg(color)),
                ])
            })
            .collect()
    }
}

fn series_colors(colors: impl Iterator<Item = Color>, fallback: Color) -> [Color; MAX_SERIES] {
    let mut palette = [fallback; MAX_SERIES];
    let mut used = 0;
    for color in colors {
        if color != Color::Reset && !palette[..used].contains(&color) {
            palette[used] = color;
            used += 1;
            if used == palette.len() {
                break;
            }
        }
    }
    palette
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

fn seconds(time: DateTime<Utc>, start: DateTime<Utc>) -> f64 {
    (time - start).as_seconds_f64()
}

fn format_number(number: f64) -> String {
    if number == 0.0 {
        "0".into()
    } else if number.abs() >= F64_EXACT_INTEGER_LIMIT {
        format!("≈{number:.5e}")
    } else if (0.001..1_000_000.0).contains(&number.abs()) {
        number.to_string()
    } else {
        format!("{number:e}")
    }
}

fn format_change(number: f64) -> String {
    if number.fract() == 0.0 {
        return format_number(number);
    }
    // A subtraction can leave extra binary floating-point digits in a decimal result.
    if (0.001..1_000_000.0).contains(&number) {
        let places = (5.0 - number.log10().floor()).max(1.0) as usize;
        format!("{number:.places$}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
    } else {
        format!("{number:.5e}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn auto_chart_reads_sql_rows_and_mcp_structured_results() {
        let details = json!({"structuredContent": {
            "columns": ["day", "turns", "p95_s", "status"],
            "results": [
                ["2026-11-13", 462, 56, "ok"],
                ["2026-11-08", 6, -7, "ok"],
                ["2026-11-11", 60, null, "ok"]
            ]
        }});
        let chart = AutoChart::from_result("Query complete", Some(&details)).unwrap();
        assert_eq!(chart.series.len(), 2);
        assert_eq!(chart.series[0].name, "turns");
        assert_eq!(chart.series[0].values, [Some(6.0), Some(60.0), Some(462.0)]);
        assert_eq!(chart.series[1].values, [Some(-7.0), None, Some(56.0)]);
        let lines = chart.lines(64, &crate::themes::dark_theme());
        assert_eq!(lines.len(), usize::from(chart.height()));
        assert!(lines.iter().all(|line| line.width() <= 64));
        assert!(lines[1].to_string().contains("Last 462"));
        assert!(lines[2..4].iter().any(|line| {
            line.to_string()
                .chars()
                .any(|c| ('\u{2801}'..='\u{28ff}').contains(&c))
        }));
        assert!(lines.last().unwrap().to_string().contains("2026-11-13"));
    }

    #[test]
    fn auto_chart_reads_posthog_mcp_sql_text_blocks_without_losing_gaps() {
        let output = "day|turns|latency\n2026-11-19|12|(null)\n2026-11-18|8|40\n2026-11-17|4|58\n\n\
                      [Warehouse suggestions for views this query read. Mention them to the person; do not act on them unless the person asks.]\n- Example warehouse suggestion.";
        let details = json!({
            "server": "analytics",
            "tool": "exec",
            "content": [{"type": "text", "text": output}],
            "isError": false
        });
        let chart = AutoChart::from_result("Query complete", Some(&details)).unwrap();
        assert_eq!(chart.series[0].name, "turns");
        assert_eq!(chart.series[0].values, [Some(4.0), Some(8.0), Some(12.0)]);
        assert_eq!(chart.series[1].values, [Some(58.0), Some(40.0), None]);

        let mut incomplete = details.clone();
        incomplete["content"][0]["text"] = json!(format!(
            "{output}\n[SQL result preview: Showing 3 of 4 returned rows.]"
        ));
        assert!(AutoChart::from_result("Query complete", Some(&incomplete)).is_none());
        let mut failed = details;
        failed["isError"] = json!(true);
        assert!(AutoChart::from_result(output, Some(&failed)).is_none());
    }

    #[test]
    fn auto_chart_reads_current_posthog_trends_and_leaves_out_unfinished_buckets() {
        let output = "Previous period:\nDate|turns\n2026-11-17 08:00|100\n2026-11-17 09:00|200\n\n\
                      Current period:\nDate|turns\n2026-11-18 08:00|9\n2026-11-18 09:00|11\n\
                      2026-11-18 10:00 (partial)|2\n\n\
                      Note: rows marked \"(partial)\" cover an interval that is still in progress as of the query time, so their values are incomplete. Timezone: America/Los_Angeles.";
        let chart = AutoChart::from_result(output, None).unwrap();
        assert_eq!(chart.series[0].values, [Some(9.0), Some(11.0)]);
        let lines = chart.lines(64, &crate::themes::dark_theme());
        let axis = lines.last().unwrap().to_string();
        assert!(axis.contains("08:00"));
        assert!(axis.ends_with("09:00:00"));
        assert!(!axis.contains("UTC"));
    }

    #[test]
    fn auto_chart_reconciles_open_posthog_buckets_and_preserves_precision_and_gaps() {
        let text = "Date|turns\n2026-11-15|5.12346\n2026-11-16|N/A\n2026-11-17|11\n\
                    2026-11-18 (partial)|2\n\n\
                    Note: rows marked \"(partial)\" cover an interval that is still in progress. Timezone: UTC.";
        let data = json!({"results": [{
            "label": "turns", "days": ["2026-11-15", "2026-11-17", "2026-11-18"], "data": [5.123_456_78, 11, 2]
        }]});
        let envelope = |text: &str, structured: Value| json!({"structuredContent": structured, "content": [{"type":"text", "text": text}]});
        let details = envelope(text, data.clone());
        for chart in [
            AutoChart::from_result("Query complete", Some(&details)),
            AutoChart::from_result(&details.to_string(), None),
            AutoChart::from_result(text, Some(&json!({"structuredContent": data}))),
        ] {
            let chart = chart.unwrap();
            assert_eq!(
                chart.series[0].values,
                [Some(5.123_456_78), None, Some(11.0)]
            );
            let lines = chart.lines(80, &crate::themes::dark_theme());
            assert!(lines[0].to_string().contains("Open buckets omitted (1)"));
            assert!(lines[1].to_string().contains("Last 11"));
            assert!(lines.last().unwrap().to_string().ends_with("2026-11-17"));
        }

        for (field, value) in [
            ("label", json!("wrong metric")),
            ("days", json!(["2026-11-15", "2026-11-17", "2026-11-19"])),
            ("data", json!([5.9, 11, 2])),
        ] {
            let mut conflicting = details.clone();
            conflicting["structuredContent"]["results"][0][field] = value;
            assert!(
                AutoChart::from_result(text, Some(&conflicting)).is_none(),
                "{field}"
            );
        }
    }

    #[test]
    fn auto_chart_keeps_completed_metrics_when_another_has_one_completed_reading() {
        let text = "Date|turns|errors\n2026-11-16|4|(null)\n2026-11-17|11|3\n\
                    2026-11-18 (partial)|2|1\n\n\
                    Note: rows marked \"(partial)\" cover an interval that is still in progress. Timezone: UTC.";
        let data = json!({"results": [
            {"label": "turns", "days": ["2026-11-16", "2026-11-17", "2026-11-18"], "data": [4, 11, 2]},
            {"label": "errors", "days": ["2026-11-16", "2026-11-17", "2026-11-18"], "data": [null, 3, 1]}
        ]});
        let chart =
            AutoChart::from_result(text, Some(&json!({"structuredContent": data}))).unwrap();
        assert_eq!(chart.series.len(), 1);
        assert_eq!(chart.series[0].name, "turns");
        assert_eq!(chart.series[0].values, [Some(4.0), Some(11.0)]);

        // A metric the table drops although it has two completed readings
        // is still a disagreement.
        let mut conflicting = data.clone();
        conflicting["results"][1]["data"] = json!([7, 3, 1]);
        assert!(
            AutoChart::from_result(text, Some(&json!({"structuredContent": conflicting})))
                .is_none()
        );
    }

    #[test]
    fn auto_chart_reconciles_posthog_named_points_with_a_visible_table() {
        let text = "Date|turns\n2026-11-15|5.12346\n2026-11-16|N/A\n2026-11-17|11\n\n\
                    Chart data: the JSON below repeats the values above as named series, so the client can draw them.\n\
                    {\"series\":[{\"name\":\"turns\",\"data\":[[\"2026-11-15\",5.12345678],[\"2026-11-17\",11]]}]}";
        let chart = AutoChart::from_result(text, None).unwrap();
        assert_eq!(
            chart.series[0].values,
            [Some(5.123_456_78), None, Some(11.0)]
        );
        let conflicting = text.replace("5.12345678", "5.9");
        assert!(AutoChart::from_result(&conflicting, None).is_none());
    }

    #[test]
    fn auto_chart_reads_structured_posthog_trends_and_explicit_mcp_chart_data() {
        let details = json!({
            "structuredContent": { "results": [
                {"label": "turns", "days": ["2026-11-19", "2026-11-18", "2026-11-17"],
                 "data": [11, null, 5], "compare_label": "current"},
                {"label": "turns", "days": ["2026-11-12", "2026-11-11"],
                 "data": [8, 3], "compare_label": "previous"}
            ]}
        });
        let chart = AutoChart::from_result("Query complete", Some(&details)).unwrap();
        assert_eq!(chart.series.len(), 1);
        assert_eq!(chart.series[0].name, "turns");
        assert_eq!(chart.series[0].values, [Some(5.0), None, Some(11.0)]);

        let output = "PostHog query response\n\n\
                      Chart data: the JSON below repeats the values above as named series, so the client can draw them. Read and quote the output above.\n\
                      {\"series\":[{\"name\":\"events\",\"data\":[[\"2026-11-17\",5],[\"2026-11-19\",9]]},\
                      {\"name\":\"latency\",\"data\":[[\"2026-11-18\",14],[\"2026-11-19\",11]]}]}";
        let chart = AutoChart::from_result(output, None).unwrap();
        assert_eq!(chart.series[0].values, [Some(5.0), None, Some(9.0)]);
        assert_eq!(chart.series[1].values, [None, Some(14.0), Some(11.0)]);

        let wrapped = json!({"content": [{"type": "text", "text": output}]}).to_string();
        assert!(AutoChart::from_result(&wrapped, None).is_some());
        assert!(
            AutoChart::from_result(&output.replace("2026-11-19", "2026-11-17"), None).is_none()
        );
    }

    #[test]
    fn auto_chart_keeps_missing_values_as_gaps() {
        let output = r#"[
            {"at": "2026-11-18T00:00:00Z", "count": 1},
            {"at": "2026-11-18T00:05:00Z"},
            {"at": "2026-11-18T00:10:00Z", "count": 1}
        ]"#;
        let chart = AutoChart::from_result(output, None).unwrap();
        let rows = chart.series[0].plot(&[0.0, 300.0, 600.0], 600.0, 40, Color::Cyan, Color::Gray);
        let plotted_cells = rows
            .iter()
            .map(|line| {
                line.spans[1]
                    .content
                    .chars()
                    .filter(|c| ('\u{2801}'..='\u{28ff}').contains(c))
                    .count()
            })
            .sum::<usize>();
        assert_eq!(plotted_cells, 2);
    }

    #[test]
    fn auto_chart_summary_keeps_last_when_the_range_does_not_fit() {
        let table =
            "day|每分鐘平均執行數及其他附加指標|cap_hits\n2026-11-11|1.001|3\n2026-11-18|1.004|4";
        let chart = AutoChart::from_result(table, None).unwrap();
        let theme = crate::themes::dark_theme();
        for width in [44, 64, 124] {
            let lines = chart.lines(width, &theme);
            assert_eq!(lines.len(), usize::from(chart.height()));
            assert!(lines.iter().all(|line| line.width() <= usize::from(width)));
            assert!(lines[0].to_string().contains("independent scales"));
            assert!(lines[1].to_string().contains("Last 1.004"));
        }

        let narrow = chart.lines(44, &theme);
        assert!(!narrow[1].to_string().contains("Range"));
        let wide = chart.lines(124, &theme);
        assert!(wide[0].to_string().contains("2 timestamps"));
        assert!(wide[1].to_string().contains("Range 1.001–1.004"));
        assert!(wide[1].to_string().contains("Change +0.003"));
    }

    #[test]
    fn auto_chart_delta_uses_only_finite_values_at_both_time_bounds() {
        let table = "day|rise|fall|missing_first|missing_last|overflow\n\
                     2026-11-11|342|64|(null)|9|-1.7e308\n\
                     2026-11-14|(null)|52|5|7|0\n\
                     2026-11-18|357|45|4|(null)|1.7e308";
        let chart = AutoChart::from_result(table, None).unwrap();
        let lines = chart.lines(124, &crate::themes::dark_theme());
        let summary =
            |series: usize| lines[1 + series * (1 + usize::from(PLOT_HEIGHT))].to_string();
        assert!(summary(0).contains("Change +15"));
        assert!(summary(1).contains("Change −19"));
        assert!(!summary(2).contains("Change"));
        assert!(summary(3).contains("Last —"));
        assert!(!summary(3).contains("Change"));
        assert!(!summary(4).contains("Change"));
    }

    #[test]
    fn auto_chart_matches_distinct_theme_colors_to_labels_and_traces() {
        let table = "day|one|two|three|four|five\n2026-11-11|1|2|3|4|5\n2026-11-18|2|3|4|5|6";
        let chart = AutoChart::from_result(table, None).unwrap();
        let expected = [
            Color::Magenta,
            Color::Blue,
            Color::Yellow,
            Color::Green,
            Color::Cyan,
        ];
        let colors = series_colors(
            [Color::Reset, Color::Magenta].into_iter().chain(expected),
            Color::White,
        );
        assert_eq!(colors, expected);
        let lines = chart.lines(124, &crate::themes::light_theme());
        for series in 0..chart.series.len() {
            let header = 1 + series * (1 + usize::from(PLOT_HEIGHT));
            let label_color = lines[header].spans[1].style.fg;
            assert_eq!(lines[header + 1].spans[1].style.fg, label_color);
        }
        let mut monochrome = crate::themes::dark_theme();
        for color in [
            &mut monochrome.colors.accent,
            &mut monochrome.colors.text,
            &mut monochrome.colors.syntax_function,
            &mut monochrome.colors.syntax_number,
            &mut monochrome.colors.syntax_string,
            &mut monochrome.colors.syntax_type,
            &mut monochrome.colors.syntax_variable,
            &mut monochrome.colors.syntax_keyword,
        ] {
            color.clear();
        }
        let lines = chart.lines(44, &monochrome);
        assert!(lines[1].to_string().contains("one  Last 2"));
        assert_eq!(lines[1].spans[1].style.fg, Some(Color::Reset));
        assert!(
            lines[2..4]
                .iter()
                .flat_map(|line| line.spans[1].content.chars())
                .any(|c| ('\u{2801}'..='\u{28ff}').contains(&c))
        );
    }

    #[test]
    fn auto_chart_preserves_close_values_and_never_asserts_lossy_integer_changes() {
        let theme = crate::themes::dark_theme();
        let table = "day|near_one|tiny|large\n\
                     2026-11-11|1.000011|0.000149|9007199254740992\n\
                     2026-11-18|1.000012|0.000151|9007199254740993";
        let chart = AutoChart::from_result(table, None).unwrap();
        let lines = chart.lines(124, &theme);
        assert!(
            lines[1]
                .to_string()
                .contains("Last 1.000012  Range 1.000011–1.000012")
        );
        assert!(lines[4].to_string().contains("Range 1.49e-4–1.51e-4"));
        let large = lines[7].to_string();
        assert!(large.contains("Last ≈"));
        assert!(!large.contains("Range"));
        assert!(!large.contains("Change"));
    }

    #[test]
    fn auto_chart_preserves_both_precise_utc_endpoints_at_minimum_width() {
        let table = "at|count\n\
                     2026-11-11T00:00:00.123456789Z|1\n\
                     2026-11-11T00:00:00.123456790Z|2";
        let chart = AutoChart::from_result(table, None).unwrap();
        let lines = chart.lines(AutoChart::MIN_WIDTH, &crate::themes::dark_theme());
        let axis = lines.last().unwrap();
        assert!(axis.width() <= usize::from(AutoChart::MIN_WIDTH));
        assert!(axis.to_string().contains("00:00:00.123456789"));
        assert!(axis.to_string().ends_with("00:00:00.123456790 UTC"));
    }

    #[test]
    fn auto_chart_accepts_complete_pipe_tables_and_rejects_previews() {
        let table = "day|turns|p95_s\n2026-11-08|6|50\n2026-11-11|60|(null)\n2026-11-13|462|56";
        let wrapped = json!({"content": table}).to_string();
        for output in [table, &wrapped] {
            let chart = AutoChart::from_result(output, None).unwrap();
            assert_eq!(chart.series[0].values, [Some(6.0), Some(60.0), Some(462.0)]);
            assert_eq!(chart.series[1].values, [Some(50.0), None, Some(56.0)]);
        }
        let incomplete = format!("{table}\n[SQL result preview: Showing 3 of 4 returned rows.]");
        assert!(AutoChart::from_result(&incomplete, None).is_none());
    }

    #[test]
    fn auto_chart_omits_ambiguous_and_non_tabular_results() {
        for output in [
            "Completed 5 work items.",
            r#"[{"day":"2026-11-18","n":3}]"#,
            r#"[{"day":"2026-11-18","n":3},{"day":"2026-11-18","n":4}]"#,
            r#"[{"day":"2026-11-18","start":"2026-11-15","n":3},{"day":"2026-11-19","start":"2026-11-16","n":4}]"#,
            r#"[{"day":"2026-11-18","n":3},{"day":"2026-11-19","n":"4"}]"#,
            r#"{"columns":["day","n"],"results":[["2026-11-18",3],["2026-11-19",4,5]]}"#,
        ] {
            assert!(AutoChart::from_result(output, None).is_none(), "{output}");
        }
    }
}
