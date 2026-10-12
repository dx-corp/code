//! Offline A2A telemetry event-file inspection.
//!
//! Ports `src/platform/a2a-telemetry-inspect.ts`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const A2A_TELEMETRY_INSPECTION_SCHEMA: &str = "evalops.maestro.a2a-telemetry-inspection.v2";

const PEER_SELECTED: &str = "maestro.events.a2a.peer.selected";
const TASK_DISPATCHED: &str = "maestro.events.a2a.task.dispatched";
const TASK_COMPLETED: &str = "maestro.events.a2a.task.completed";
const TASK_FAILED: &str = "maestro.events.a2a.task.failed";
const TASK_CANCELLED: &str = "maestro.events.a2a.task.cancelled";
const PUSH_RECEIVED: &str = "maestro.events.a2a.push.received";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct A2ATelemetryCloudEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<BTreeMap<String, Value>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct A2ATelemetryInspection {
    pub schema: &'static str,
    pub swarm_id: String,
    pub complete: bool,
    pub counts: A2ATelemetryCounts,
    pub lanes: Vec<A2ATelemetryInspectionLane>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct A2ATelemetryCounts {
    pub events: usize,
    pub lanes: usize,
    pub selected_peers: usize,
    pub completed_lanes: usize,
    pub failed_lanes: usize,
    pub missing_telemetry_lanes: usize,
    pub ordering_anomaly_lanes: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct A2ATelemetryInspectionLane {
    pub lane_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub a2a_task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub a2a_message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    pub event_types: Vec<String>,
    pub timing: A2ATelemetryInspectionLaneTiming,
    pub ordering_anomalies: Vec<String>,
    pub missing_event_types: Vec<String>,
    pub missing_signals: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct A2ATelemetryInspectionLaneTiming {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_event_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_selected_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dispatched_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_event_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection_to_dispatch_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_duration_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lifecycle_duration_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reported_dispatch_latency_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reported_duration_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub push_lag_ms: Option<f64>,
}

pub fn load_a2a_telemetry_events(path: impl AsRef<Path>) -> Result<Vec<A2ATelemetryCloudEvent>> {
    let path = path.as_ref();
    let raw = fs::read_to_string(path)
        .with_context(|| format!("read A2A telemetry events {}", path.display()))?;
    let parsed: Value = serde_json::from_str(&raw)
        .with_context(|| format!("parse A2A telemetry events {}", path.display()))?;
    let events = if let Some(array) = parsed.as_array() {
        array.clone()
    } else if let Some(array) = parsed.get("events").and_then(|v| v.as_array()) {
        array.clone()
    } else {
        bail!("A2A telemetry events file must be an array or {{ events }}");
    };
    Ok(events.into_iter().filter_map(parse_event).collect())
}

pub fn inspect_a2a_telemetry(
    swarm_id: &str,
    events: &[A2ATelemetryCloudEvent],
) -> A2ATelemetryInspection {
    let events: Vec<_> = events
        .iter()
        .filter(|event| {
            string_data(event, "swarm_id")
                .or_else(|| string_data(event, "swarmId"))
                .as_deref()
                == Some(swarm_id)
        })
        .cloned()
        .collect();
    let mut lane_events: BTreeMap<String, Vec<A2ATelemetryCloudEvent>> = BTreeMap::new();
    for event in &events {
        if let Some(lane_id) =
            string_data(event, "lane_id").or_else(|| string_data(event, "laneId"))
        {
            lane_events.entry(lane_id).or_default().push(event.clone());
        }
    }
    let lanes: Vec<_> = lane_events
        .into_iter()
        .map(|(lane_id, events)| inspect_lane(lane_id, &events))
        .collect();
    let selected_peers: BTreeSet<_> = lanes
        .iter()
        .filter_map(|lane| lane.peer_agent_id.clone().or_else(|| lane.peer.clone()))
        .collect();
    let missing_telemetry_lanes = lanes
        .iter()
        .filter(|lane| !lane.missing_event_types.is_empty())
        .count();
    let ordering_anomaly_lanes = lanes
        .iter()
        .filter(|lane| !lane.ordering_anomalies.is_empty())
        .count();
    let failed_lanes = lanes.iter().filter(|lane| is_failed_lane(lane)).count();
    let completed_lanes = lanes.iter().filter(|lane| is_completed_lane(lane)).count();
    A2ATelemetryInspection {
        schema: A2A_TELEMETRY_INSPECTION_SCHEMA,
        swarm_id: swarm_id.to_string(),
        complete: !lanes.is_empty()
            && missing_telemetry_lanes == 0
            && ordering_anomaly_lanes == 0
            && lanes.iter().all(|lane| lane.missing_signals.is_empty()),
        counts: A2ATelemetryCounts {
            events: events.len(),
            lanes: lanes.len(),
            selected_peers: selected_peers.len(),
            completed_lanes,
            failed_lanes,
            missing_telemetry_lanes,
            ordering_anomaly_lanes,
        },
        lanes,
    }
}

fn parse_event(value: Value) -> Option<A2ATelemetryCloudEvent> {
    let obj = value.as_object()?;
    let event_type = obj.get("type")?.as_str()?.trim();
    if event_type.is_empty() {
        return None;
    }
    let data = obj.get("data").and_then(|value| {
        value.as_object().map(|map| {
            map.iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<BTreeMap<_, _>>()
        })
    });
    Some(A2ATelemetryCloudEvent {
        event_type: event_type.to_string(),
        time: obj
            .get("time")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        data,
    })
}

fn inspect_lane(lane_id: String, events: &[A2ATelemetryCloudEvent]) -> A2ATelemetryInspectionLane {
    let merged = merge_lane_event_data(events);
    let event_types = unique(
        events
            .iter()
            .map(|event| event.event_type.clone())
            .collect(),
    );
    let timing = lane_timing(events);
    A2ATelemetryInspectionLane {
        lane_id,
        parent_task_id: string_map(&merged, "parent_task_id")
            .or_else(|| string_map(&merged, "parentTaskId")),
        a2a_task_id: string_map(&merged, "a2a_task_id")
            .or_else(|| string_map(&merged, "a2aTaskId")),
        a2a_message_id: string_map(&merged, "a2a_message_id")
            .or_else(|| string_map(&merged, "a2aMessageId")),
        context_id: string_map(&merged, "context_id").or_else(|| string_map(&merged, "contextId")),
        peer: string_map(&merged, "peer_name").or_else(|| string_map(&merged, "peer")),
        peer_agent_id: string_map(&merged, "peer_agent_id")
            .or_else(|| string_map(&merged, "peerAgentId")),
        source: string_map(&merged, "source"),
        status: string_map(&merged, "status"),
        event_types: event_types.clone(),
        timing: timing.clone(),
        ordering_anomalies: ordering_anomalies(events, &timing),
        missing_event_types: missing_event_types(events, &event_types),
        missing_signals: vec![],
    }
}

fn merge_lane_event_data(events: &[A2ATelemetryCloudEvent]) -> BTreeMap<String, Value> {
    let mut merged = BTreeMap::new();
    for event in events {
        if let Some(data) = &event.data {
            for (key, value) in data {
                merged.insert(key.clone(), value.clone());
            }
        }
    }
    merged
}

fn missing_event_types(events: &[A2ATelemetryCloudEvent], event_types: &[String]) -> Vec<String> {
    let mut missing = Vec::new();
    if !event_types.iter().any(|t| t == PEER_SELECTED) {
        missing.push(PEER_SELECTED.into());
    }
    if !event_types.iter().any(|t| t == TASK_DISPATCHED)
        && !is_pre_dispatch_terminal_failure(events, event_types)
    {
        missing.push(TASK_DISPATCHED.into());
    }
    if !event_types.iter().any(|t| is_terminal_event_type(t)) {
        missing.push(TASK_COMPLETED.into());
    }
    missing
}

fn is_pre_dispatch_terminal_failure(
    events: &[A2ATelemetryCloudEvent],
    event_types: &[String],
) -> bool {
    !event_types.iter().any(|t| t == TASK_DISPATCHED)
        && (event_types.iter().any(|t| t == TASK_FAILED)
            || event_types.iter().any(|t| t == TASK_CANCELLED))
        && events.iter().all(|event| {
            string_data(event, "a2a_task_id")
                .or_else(|| string_data(event, "a2aTaskId"))
                .is_none()
        })
}

fn is_terminal_event_type(event_type: &str) -> bool {
    matches!(event_type, TASK_COMPLETED | TASK_FAILED | TASK_CANCELLED)
}

fn lane_timing(events: &[A2ATelemetryCloudEvent]) -> A2ATelemetryInspectionLaneTiming {
    let mut timed: Vec<_> = events
        .iter()
        .filter_map(|event| event_time_ms(event).map(|ms| (event, ms)))
        .collect();
    timed.sort_by_key(|(_, ms)| *ms);
    let first_event_at = timed.first().map(|(_, ms)| *ms);
    let last_event_at = timed.last().map(|(_, ms)| *ms);
    let peer_selected_at = timed
        .iter()
        .find(|(event, _)| event.event_type == PEER_SELECTED)
        .map(|(_, ms)| *ms);
    let dispatched_at = timed
        .iter()
        .find(|(event, _)| event.event_type == TASK_DISPATCHED)
        .map(|(_, ms)| *ms);
    let terminal_at = timed
        .iter()
        .find(|(event, _)| is_terminal_event_type(&event.event_type))
        .map(|(_, ms)| *ms);
    A2ATelemetryInspectionLaneTiming {
        first_event_at: iso_time(first_event_at),
        peer_selected_at: iso_time(peer_selected_at),
        dispatched_at: iso_time(dispatched_at),
        terminal_at: iso_time(terminal_at),
        last_event_at: iso_time(last_event_at),
        selection_to_dispatch_ms: elapsed_ms(peer_selected_at, dispatched_at),
        observed_duration_ms: elapsed_ms(dispatched_at, terminal_at),
        lifecycle_duration_ms: elapsed_ms(first_event_at, last_event_at),
        reported_dispatch_latency_ms: reported_number(
            events,
            |e| e.event_type == TASK_DISPATCHED,
            "latency_ms",
        ),
        reported_duration_ms: reported_number(
            events,
            |e| is_terminal_event_type(&e.event_type),
            "duration_ms",
        ),
        push_lag_ms: reported_number(events, |e| e.event_type == PUSH_RECEIVED, "push_lag_ms"),
    }
}

fn ordering_anomalies(
    events: &[A2ATelemetryCloudEvent],
    timing: &A2ATelemetryInspectionLaneTiming,
) -> Vec<String> {
    let mut anomalies = Vec::new();
    let peer_selected_at = parse_time_ms(timing.peer_selected_at.as_deref());
    let dispatched_at = parse_time_ms(timing.dispatched_at.as_deref());
    let terminal_at = parse_time_ms(timing.terminal_at.as_deref());
    if let (Some(selected), Some(dispatched)) = (peer_selected_at, dispatched_at) {
        if dispatched < selected {
            anomalies.push("dispatch_before_peer_selected".into());
        }
    }
    if let (Some(dispatched), Some(terminal)) = (dispatched_at, terminal_at) {
        if terminal < dispatched {
            anomalies.push("terminal_before_dispatch".into());
        }
    }
    if dispatched_at.is_none() {
        if let (Some(selected), Some(terminal)) = (peer_selected_at, terminal_at) {
            if terminal < selected {
                anomalies.push("terminal_before_peer_selected".into());
            }
        }
    }
    if events
        .iter()
        .filter(|event| is_terminal_event_type(&event.event_type))
        .count()
        > 1
    {
        anomalies.push("duplicate_terminal_event".into());
    }
    anomalies
}

fn is_completed_lane(lane: &A2ATelemetryInspectionLane) -> bool {
    lane.event_types.iter().any(|t| t == TASK_COMPLETED)
        || lane
            .status
            .as_deref()
            .unwrap_or("")
            .to_ascii_uppercase()
            .contains("COMPLETED")
}

fn is_failed_lane(lane: &A2ATelemetryInspectionLane) -> bool {
    let status = lane.status.as_deref().unwrap_or("").to_ascii_uppercase();
    lane.event_types.iter().any(|t| t == TASK_FAILED)
        || lane.event_types.iter().any(|t| t == TASK_CANCELLED)
        || status.contains("FAILED")
        || status.contains("CANCEL")
        || status.contains("REJECTED")
}

fn string_data(event: &A2ATelemetryCloudEvent, key: &str) -> Option<String> {
    event
        .data
        .as_ref()
        .and_then(|data| data.get(key))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn string_map(map: &BTreeMap<String, Value>, key: &str) -> Option<String> {
    map.get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn event_time_ms(event: &A2ATelemetryCloudEvent) -> Option<i64> {
    let time = event.time.as_deref()?;
    chrono::DateTime::parse_from_rfc3339(time)
        .ok()
        .map(|dt| dt.timestamp_millis())
        .or_else(|| {
            // Accept millis-truncated ISO strings chrono may already parse via RFC3339.
            DateTimeLoose::parse(time)
        })
}

struct DateTimeLoose;
impl DateTimeLoose {
    fn parse(value: &str) -> Option<i64> {
        chrono::DateTime::parse_from_rfc3339(value)
            .ok()
            .map(|dt| dt.timestamp_millis())
    }
}

fn reported_number(
    events: &[A2ATelemetryCloudEvent],
    predicate: impl Fn(&A2ATelemetryCloudEvent) -> bool,
    field: &str,
) -> Option<f64> {
    for event in events {
        if !predicate(event) {
            continue;
        }
        if let Some(value) = event
            .data
            .as_ref()
            .and_then(|data| data.get(field))
            .and_then(|value| value.as_f64())
        {
            return Some(value);
        }
    }
    None
}

fn elapsed_ms(start: Option<i64>, end: Option<i64>) -> Option<i64> {
    match (start, end) {
        (Some(start), Some(end)) => Some((end - start).max(0)),
        _ => None,
    }
}

fn iso_time(value: Option<i64>) -> Option<String> {
    value.and_then(|ms| {
        chrono::DateTime::from_timestamp_millis(ms)
            .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
    })
}

fn parse_time_ms(value: Option<&str>) -> Option<i64> {
    value.and_then(|raw| {
        chrono::DateTime::parse_from_rfc3339(raw)
            .ok()
            .map(|dt| dt.timestamp_millis())
    })
}

fn unique(values: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for value in values {
        if seen.insert(value.clone()) {
            out.push(value);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn inspects_complete_lane_lifecycle() {
        let events = vec![
            event(
                PEER_SELECTED,
                "2026-07-21T00:00:00.000Z",
                "lane-1",
                "swarm-1",
                None,
            ),
            event(
                TASK_DISPATCHED,
                "2026-07-21T00:00:01.000Z",
                "lane-1",
                "swarm-1",
                Some(10.0),
            ),
            event(
                TASK_COMPLETED,
                "2026-07-21T00:00:03.000Z",
                "lane-1",
                "swarm-1",
                Some(2000.0),
            ),
        ];
        let inspection = inspect_a2a_telemetry("swarm-1", &events);
        assert!(inspection.complete);
        assert_eq!(inspection.counts.lanes, 1);
        assert_eq!(inspection.counts.completed_lanes, 1);
        assert_eq!(
            inspection.lanes[0].timing.selection_to_dispatch_ms,
            Some(1000)
        );
    }

    #[test]
    fn loads_events_from_file() {
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"[{{"type":"{PEER_SELECTED}","time":"2026-07-21T00:00:00.000Z","data":{{"swarm_id":"s1","lane_id":"l1"}}}}]"#
        )
        .unwrap();
        let events = load_a2a_telemetry_events(file.path()).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, PEER_SELECTED);
    }

    fn event(
        event_type: &str,
        time: &str,
        lane_id: &str,
        swarm_id: &str,
        duration_or_latency: Option<f64>,
    ) -> A2ATelemetryCloudEvent {
        let mut data = BTreeMap::from([
            ("swarm_id".into(), Value::String(swarm_id.into())),
            ("lane_id".into(), Value::String(lane_id.into())),
            ("status".into(), Value::String("COMPLETED".into())),
            ("peer_name".into(), Value::String("peer-a".into())),
        ]);
        if let Some(value) = duration_or_latency {
            if event_type == TASK_DISPATCHED {
                data.insert("latency_ms".into(), json_number(value));
            } else {
                data.insert("duration_ms".into(), json_number(value));
            }
        }
        A2ATelemetryCloudEvent {
            event_type: event_type.into(),
            time: Some(time.into()),
            data: Some(data),
        }
    }

    fn json_number(value: f64) -> Value {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    }
}
