//! Typed public API of the bmux performance plugin.
//!
//! Hand-written (no BPDL). Mirrors the pattern used by
//! `bmux_recording_plugin_api`: hosts the domain types the server's
//! hot path needs (so server imports types, not the plugin impl crate)
//! plus typed request/response/event wire enums for the plugin's
//! `performance-commands::dispatch` service surface.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

pub mod typed_client;

use bmux_ipc::PerformanceRuntimeSettings;
use bmux_performance_state::PerformanceCaptureSettings;
use bmux_plugin_sdk::{CapabilityId, InterfaceId, PluginEventKind};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// Capability gating read access to the performance plugin's query
/// surface.
pub const PERFORMANCE_READ: CapabilityId = CapabilityId::from_static("bmux.performance.read");

/// Capability gating write access to the performance plugin's command
/// surface.
pub const PERFORMANCE_WRITE: CapabilityId = CapabilityId::from_static("bmux.performance.write");

/// Interface id for the performance plugin's typed command surface.
pub const PERFORMANCE_COMMANDS_INTERFACE: InterfaceId =
    InterfaceId::from_static("performance-commands");

/// Interface id for the performance plugin's typed event surface.
pub const PERFORMANCE_EVENTS_INTERFACE: InterfaceId =
    InterfaceId::from_static("performance-events");

/// Event-bus channel kind for the performance plugin's typed event
/// stream.
pub const EVENT_KIND: PluginEventKind =
    PluginEventKind::from_static("bmux.performance/performance-events");

/// State-channel kind carrying the latest system + pane metrics snapshot.
pub const METRICS_STATE_KIND: PluginEventKind =
    PluginEventKind::from_static("bmux.performance/metrics-state");

/// Broadcast event-channel kind for noteworthy metric changes.
pub const METRIC_EVENT_KIND: PluginEventKind =
    PluginEventKind::from_static("bmux.performance/metric-events");

/// Default watch id used by the shipped performance sampler.
pub const DEFAULT_METRICS_WATCH_ID: &str = "default";

/// Minimum supported sampler interval. Lower values are clamped by the
/// performance plugin to avoid turning decoration scripts into a CPU
/// load source.
pub const MIN_METRICS_INTERVAL_MS: u64 = 500;

// ── Rate limiter ─────────────────────────────────────────────────────

/// Sliding-window rate limiter for performance-recording events.
///
/// Owned by server's event-push pump (one instance per client). Reads
/// `PerformanceCaptureSettings` on each call; mutates its own
/// window/counters. Lives here so server can construct it without
/// depending on the plugin impl crate.
#[derive(Debug)]
pub struct PerformanceEventRateLimiter {
    settings: PerformanceCaptureSettings,
    rate_window_started_at: Instant,
    emitted_events_in_window: u32,
    emitted_payload_bytes_in_window: usize,
    dropped_events_since_emit: u64,
    dropped_payload_bytes_since_emit: u64,
}

impl PerformanceEventRateLimiter {
    #[must_use]
    pub fn new(settings: PerformanceCaptureSettings) -> Self {
        Self {
            settings,
            rate_window_started_at: Instant::now(),
            emitted_events_in_window: 0,
            emitted_payload_bytes_in_window: 0,
            dropped_events_since_emit: 0,
            dropped_payload_bytes_since_emit: 0,
        }
    }

    fn reset_rate_window_if_needed(&mut self) {
        if self.rate_window_started_at.elapsed() >= Duration::from_secs(1) {
            self.rate_window_started_at = Instant::now();
            self.emitted_events_in_window = 0;
            self.emitted_payload_bytes_in_window = 0;
        }
    }

    pub fn can_emit_payload(&mut self, payload_len: usize) -> bool {
        if !self.settings.enabled() {
            return false;
        }

        self.reset_rate_window_if_needed();

        let event_limit_hit = self.emitted_events_in_window >= self.settings.max_events_per_sec;
        let payload_limit_hit = self
            .emitted_payload_bytes_in_window
            .saturating_add(payload_len)
            > self.settings.max_payload_bytes_per_sec;
        if event_limit_hit || payload_limit_hit {
            self.dropped_events_since_emit = self.dropped_events_since_emit.saturating_add(1);
            self.dropped_payload_bytes_since_emit = self
                .dropped_payload_bytes_since_emit
                .saturating_add(u64::try_from(payload_len).unwrap_or(u64::MAX));
            return false;
        }

        self.emitted_events_in_window = self.emitted_events_in_window.saturating_add(1);
        self.emitted_payload_bytes_in_window = self
            .emitted_payload_bytes_in_window
            .saturating_add(payload_len);
        true
    }

    pub fn encode_payload(&mut self, payload: serde_json::Value) -> Option<Vec<u8>> {
        if !self.settings.enabled() {
            return None;
        }

        let mut object = match payload {
            serde_json::Value::Object(map) => map,
            other => {
                let mut map = serde_json::Map::new();
                map.insert("value".to_string(), other);
                map
            }
        };

        object.insert(
            "schema_version".to_string(),
            serde_json::Value::from(bmux_ipc::PERF_RECORDING_SCHEMA_VERSION),
        );
        object.insert(
            "level".to_string(),
            serde_json::Value::String(self.settings.level_label().to_string()),
        );
        object.insert(
            "runtime".to_string(),
            serde_json::Value::String("server".to_string()),
        );
        object.insert(
            "ts_epoch_ms".to_string(),
            serde_json::Value::from(epoch_millis_now()),
        );

        if self.dropped_events_since_emit > 0 || self.dropped_payload_bytes_since_emit > 0 {
            object.insert(
                "dropped_events_since_emit".to_string(),
                serde_json::Value::from(self.dropped_events_since_emit),
            );
            object.insert(
                "dropped_payload_bytes_since_emit".to_string(),
                serde_json::Value::from(self.dropped_payload_bytes_since_emit),
            );
            self.dropped_events_since_emit = 0;
            self.dropped_payload_bytes_since_emit = 0;
        }

        let encoded = serde_json::to_vec(&serde_json::Value::Object(object)).ok()?;
        if self.can_emit_payload(encoded.len()) {
            Some(encoded)
        } else {
            None
        }
    }
}

#[allow(clippy::cast_possible_truncation)]
fn epoch_millis_now() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    now.as_millis() as u64
}

// ── Wire enums ───────────────────────────────────────────────────────

/// Typed request variants for the performance plugin's typed service
/// dispatch surface. Replaces the former `Request::Performance*`
/// variants that used to live on `bmux_ipc::Request` before the
/// performance plugin migration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PerformanceRequest {
    /// Return the current normalized settings.
    GetSettings,
    /// Replace the current settings with the given values (normalized
    /// server-side to respect minimum ratios).
    SetSettings {
        settings: PerformanceRuntimeSettings,
    },
    /// Return the currently active metric watches.
    ListWatches,
    /// Start or replace a metric watch.
    StartWatch { watch: MetricWatch },
    /// Stop a metric watch by id.
    StopWatch { watch_id: String },
    /// Return the latest sampled metrics snapshot, if one exists.
    GetSnapshot,
}

/// Typed response variants for the performance plugin's typed service
/// dispatch surface. Both operations return a single variant carrying
/// the current (or newly-updated) settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PerformanceResponse {
    Settings {
        settings: PerformanceRuntimeSettings,
    },
    Watches {
        watches: Vec<MetricWatch>,
    },
    Snapshot {
        snapshot: MetricsSnapshot,
    },
    Ack,
}

/// Metric target selected by a watch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MetricTarget {
    /// Whole-machine CPU/memory metrics.
    System,
    /// A process tree rooted at a specific process id.
    Process { pid: u32 },
    /// A bmux pane's process tree, resolved through pane-runtime state.
    Pane { pane_id: Uuid },
}

/// Metric names a watch may request.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum MetricName {
    CpuPercent,
    MemoryBytes,
    ProcessCount,
}

/// One subscribed metric watch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MetricWatch {
    pub id: String,
    pub target: MetricTarget,
    pub metrics: Vec<MetricName>,
    pub interval_ms: u64,
}

impl MetricWatch {
    #[must_use]
    pub fn normalized(mut self) -> Self {
        self.interval_ms = self.interval_ms.max(MIN_METRICS_INTERVAL_MS);
        if self.metrics.is_empty() {
            self.metrics = vec![MetricName::CpuPercent, MetricName::MemoryBytes];
        }
        self
    }
}

impl Default for MetricWatch {
    fn default() -> Self {
        Self {
            id: DEFAULT_METRICS_WATCH_ID.to_string(),
            target: MetricTarget::System,
            metrics: vec![MetricName::CpuPercent, MetricName::MemoryBytes],
            interval_ms: 1_000,
        }
    }
}

/// Current metrics sampled for the entire machine.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SystemMetricsSnapshot {
    pub cpu_percent: f32,
    pub memory_used_bytes: u64,
    pub memory_total_bytes: u64,
}

/// Current metrics sampled for one process tree.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProcessMetricsSnapshot {
    pub pid: u32,
    pub cpu_percent: f32,
    pub memory_bytes: u64,
    pub process_count: u32,
}

/// Current metrics sampled for one pane's process tree.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PaneMetricsSnapshot {
    pub pane_id: Uuid,
    pub session_id: Option<Uuid>,
    pub pid: Option<u32>,
    pub process_group_id: Option<i32>,
    pub cpu_percent: f32,
    pub memory_bytes: u64,
    pub process_count: u32,
    pub available: bool,
}

/// Latest metrics state published by `bmux.performance`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MetricsSnapshot {
    pub sampled_at_epoch_ms: u64,
    pub watches: Vec<MetricWatch>,
    pub system: SystemMetricsSnapshot,
    pub processes: BTreeMap<u32, ProcessMetricsSnapshot>,
    pub panes: BTreeMap<Uuid, PaneMetricsSnapshot>,
}

/// Broadcast event for threshold/crossing-style consumers. The first
/// implementation emits `SnapshotUpdated` after each sample; consumers
/// that only need latest values should prefer `METRICS_STATE_KIND`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MetricEvent {
    SnapshotUpdated { sampled_at_epoch_ms: u64 },
}

/// Typed event emitted on the plugin event bus when performance
/// settings change. Server's `spawn_performance_events_bridge` maps
/// this to the legacy wire `Event::PerformanceSettingsUpdated` for
/// cross-process subscribers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PerformanceEvent {
    SettingsUpdated {
        settings: PerformanceRuntimeSettings,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_watch_normalizes_interval_and_metrics() {
        let watch = MetricWatch {
            id: "hot".to_string(),
            target: MetricTarget::System,
            metrics: Vec::new(),
            interval_ms: 1,
        }
        .normalized();

        assert_eq!(watch.interval_ms, MIN_METRICS_INTERVAL_MS);
        assert_eq!(
            watch.metrics,
            vec![MetricName::CpuPercent, MetricName::MemoryBytes]
        );
    }

    #[test]
    fn metrics_snapshot_round_trips_json() {
        let pane_id = Uuid::nil();
        let snapshot = MetricsSnapshot {
            sampled_at_epoch_ms: 42,
            watches: vec![MetricWatch::default()],
            system: SystemMetricsSnapshot {
                cpu_percent: 12.5,
                memory_used_bytes: 100,
                memory_total_bytes: 200,
            },
            processes: BTreeMap::from([(
                7,
                ProcessMetricsSnapshot {
                    pid: 7,
                    cpu_percent: 33.0,
                    memory_bytes: 44,
                    process_count: 2,
                },
            )]),
            panes: BTreeMap::from([(
                pane_id,
                PaneMetricsSnapshot {
                    pane_id,
                    session_id: Some(Uuid::nil()),
                    pid: Some(7),
                    process_group_id: Some(7),
                    cpu_percent: 33.0,
                    memory_bytes: 44,
                    process_count: 2,
                    available: true,
                },
            )]),
        };

        let encoded = serde_json::to_string(&snapshot).expect("encode snapshot");
        let decoded: MetricsSnapshot = serde_json::from_str(&encoded).expect("decode snapshot");
        assert_eq!(decoded, snapshot);
    }
}
