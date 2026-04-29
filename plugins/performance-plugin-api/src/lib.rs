//! Typed public API of the bmux performance plugin.
//!
//! The [`performance_types`], [`performance_state`],
//! [`performance_commands`], [`performance_events`], [`metric_events`],
//! and [`metrics_state`] modules are generated from
//! `bpdl/performance-plugin.bpdl` at compile time via the
//! [`bmux_plugin_schema_macros::schema!`] macro. Hand-written code in
//! this crate is limited to shared runtime helpers and compatibility
//! conversions for existing server-side settings types.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

use bmux_ipc::PerformanceRuntimeSettings;
use bmux_performance_state::PerformanceCaptureSettings;
use bmux_plugin_sdk::{PluginEventKind, PromptRequest};
use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

bmux_plugin_schema_macros::schema! {
    source: "bpdl/performance-plugin.bpdl",
}

pub use capabilities::{PERFORMANCE_READ, PERFORMANCE_WRITE};

impl From<performance_types::PerformanceRecordingLevel> for bmux_ipc::PerformanceRecordingLevel {
    fn from(value: performance_types::PerformanceRecordingLevel) -> Self {
        match value {
            performance_types::PerformanceRecordingLevel::Off => Self::Off,
            performance_types::PerformanceRecordingLevel::Basic => Self::Basic,
            performance_types::PerformanceRecordingLevel::Detailed => Self::Detailed,
            performance_types::PerformanceRecordingLevel::Trace => Self::Trace,
        }
    }
}

impl From<bmux_ipc::PerformanceRecordingLevel> for performance_types::PerformanceRecordingLevel {
    fn from(value: bmux_ipc::PerformanceRecordingLevel) -> Self {
        match value {
            bmux_ipc::PerformanceRecordingLevel::Off => Self::Off,
            bmux_ipc::PerformanceRecordingLevel::Basic => Self::Basic,
            bmux_ipc::PerformanceRecordingLevel::Detailed => Self::Detailed,
            bmux_ipc::PerformanceRecordingLevel::Trace => Self::Trace,
        }
    }
}

impl From<performance_types::PerformanceRuntimeSettings> for bmux_ipc::PerformanceRuntimeSettings {
    fn from(value: performance_types::PerformanceRuntimeSettings) -> Self {
        Self {
            recording_level: value.recording_level.into(),
            window_ms: value.window_ms,
            max_events_per_sec: value.max_events_per_sec,
            max_payload_bytes_per_sec: usize::try_from(value.max_payload_bytes_per_sec)
                .unwrap_or(usize::MAX),
        }
    }
}

impl From<bmux_ipc::PerformanceRuntimeSettings> for performance_types::PerformanceRuntimeSettings {
    fn from(value: bmux_ipc::PerformanceRuntimeSettings) -> Self {
        Self {
            recording_level: value.recording_level.into(),
            window_ms: value.window_ms,
            max_events_per_sec: value.max_events_per_sec,
            max_payload_bytes_per_sec: u64::try_from(value.max_payload_bytes_per_sec)
                .unwrap_or(u64::MAX),
        }
    }
}

impl From<performance_types::MetricTarget> for MetricTarget {
    fn from(value: performance_types::MetricTarget) -> Self {
        match value {
            performance_types::MetricTarget::System => Self::System,
            performance_types::MetricTarget::Process { pid } => Self::Process { pid },
            performance_types::MetricTarget::Pane { pane_id } => Self::Pane { pane_id },
        }
    }
}

impl From<MetricTarget> for performance_types::MetricTarget {
    fn from(value: MetricTarget) -> Self {
        match value {
            MetricTarget::System => Self::System,
            MetricTarget::Process { pid } => Self::Process { pid },
            MetricTarget::Pane { pane_id } => Self::Pane { pane_id },
        }
    }
}

impl From<performance_types::MetricName> for MetricName {
    fn from(value: performance_types::MetricName) -> Self {
        match value {
            performance_types::MetricName::CpuPercent => Self::CpuPercent,
            performance_types::MetricName::MemoryBytes => Self::MemoryBytes,
            performance_types::MetricName::ProcessCount => Self::ProcessCount,
            performance_types::MetricName::DiskReadBytesPerSec => Self::DiskReadBytesPerSec,
            performance_types::MetricName::DiskWriteBytesPerSec => Self::DiskWriteBytesPerSec,
            performance_types::MetricName::NetworkRxBytesPerSec => Self::NetworkRxBytesPerSec,
            performance_types::MetricName::NetworkTxBytesPerSec => Self::NetworkTxBytesPerSec,
        }
    }
}

impl From<MetricName> for performance_types::MetricName {
    fn from(value: MetricName) -> Self {
        match value {
            MetricName::CpuPercent => Self::CpuPercent,
            MetricName::MemoryBytes => Self::MemoryBytes,
            MetricName::ProcessCount => Self::ProcessCount,
            MetricName::DiskReadBytesPerSec => Self::DiskReadBytesPerSec,
            MetricName::DiskWriteBytesPerSec => Self::DiskWriteBytesPerSec,
            MetricName::NetworkRxBytesPerSec => Self::NetworkRxBytesPerSec,
            MetricName::NetworkTxBytesPerSec => Self::NetworkTxBytesPerSec,
        }
    }
}

impl From<performance_types::ThemeHeaderMetric> for ThemeHeaderMetric {
    fn from(value: performance_types::ThemeHeaderMetric) -> Self {
        match value {
            performance_types::ThemeHeaderMetric::Cpu => Self::Cpu,
            performance_types::ThemeHeaderMetric::Memory => Self::Memory,
            performance_types::ThemeHeaderMetric::ProcessCount => Self::ProcessCount,
            performance_types::ThemeHeaderMetric::DiskRead => Self::DiskRead,
            performance_types::ThemeHeaderMetric::DiskWrite => Self::DiskWrite,
            performance_types::ThemeHeaderMetric::NetworkRx => Self::NetworkRx,
            performance_types::ThemeHeaderMetric::NetworkTx => Self::NetworkTx,
        }
    }
}

impl From<ThemeHeaderMetric> for performance_types::ThemeHeaderMetric {
    fn from(value: ThemeHeaderMetric) -> Self {
        match value {
            ThemeHeaderMetric::Cpu => Self::Cpu,
            ThemeHeaderMetric::Memory => Self::Memory,
            ThemeHeaderMetric::ProcessCount => Self::ProcessCount,
            ThemeHeaderMetric::DiskRead => Self::DiskRead,
            ThemeHeaderMetric::DiskWrite => Self::DiskWrite,
            ThemeHeaderMetric::NetworkRx => Self::NetworkRx,
            ThemeHeaderMetric::NetworkTx => Self::NetworkTx,
        }
    }
}

impl From<performance_types::MetricTargetKind> for MetricTargetKind {
    fn from(value: performance_types::MetricTargetKind) -> Self {
        match value {
            performance_types::MetricTargetKind::System => Self::System,
            performance_types::MetricTargetKind::Process => Self::Process,
            performance_types::MetricTargetKind::Pane => Self::Pane,
        }
    }
}

impl From<MetricTargetKind> for performance_types::MetricTargetKind {
    fn from(value: MetricTargetKind) -> Self {
        match value {
            MetricTargetKind::System => Self::System,
            MetricTargetKind::Process => Self::Process,
            MetricTargetKind::Pane => Self::Pane,
        }
    }
}

impl From<performance_types::MetricAccuracy> for MetricAccuracy {
    fn from(value: performance_types::MetricAccuracy) -> Self {
        match value {
            performance_types::MetricAccuracy::Exact => Self::Exact,
            performance_types::MetricAccuracy::Estimated => Self::Estimated,
        }
    }
}

impl From<MetricAccuracy> for performance_types::MetricAccuracy {
    fn from(value: MetricAccuracy) -> Self {
        match value {
            MetricAccuracy::Exact => Self::Exact,
            MetricAccuracy::Estimated => Self::Estimated,
        }
    }
}

impl From<performance_types::CpuPercentMode> for CpuPercentMode {
    fn from(value: performance_types::CpuPercentMode) -> Self {
        match value {
            performance_types::CpuPercentMode::Normalized => Self::Normalized,
            performance_types::CpuPercentMode::RawCoreSum => Self::RawCoreSum,
        }
    }
}

impl From<CpuPercentMode> for performance_types::CpuPercentMode {
    fn from(value: CpuPercentMode) -> Self {
        match value {
            CpuPercentMode::Normalized => Self::Normalized,
            CpuPercentMode::RawCoreSum => Self::RawCoreSum,
        }
    }
}

impl From<performance_types::ThemeHeaderScope> for ThemeHeaderScope {
    fn from(value: performance_types::ThemeHeaderScope) -> Self {
        match value {
            performance_types::ThemeHeaderScope::Pane => Self::Pane,
            performance_types::ThemeHeaderScope::System => Self::System,
            performance_types::ThemeHeaderScope::Both => Self::Both,
        }
    }
}

impl From<ThemeHeaderScope> for performance_types::ThemeHeaderScope {
    fn from(value: ThemeHeaderScope) -> Self {
        match value {
            ThemeHeaderScope::Pane => Self::Pane,
            ThemeHeaderScope::System => Self::System,
            ThemeHeaderScope::Both => Self::Both,
        }
    }
}

impl From<performance_types::ThemeHeaderStyle> for ThemeHeaderStyle {
    fn from(value: performance_types::ThemeHeaderStyle) -> Self {
        match value {
            performance_types::ThemeHeaderStyle::Compact => Self::Compact,
            performance_types::ThemeHeaderStyle::Detailed => Self::Detailed,
            performance_types::ThemeHeaderStyle::HeatOnly => Self::HeatOnly,
        }
    }
}

impl From<ThemeHeaderStyle> for performance_types::ThemeHeaderStyle {
    fn from(value: ThemeHeaderStyle) -> Self {
        match value {
            ThemeHeaderStyle::Compact => Self::Compact,
            ThemeHeaderStyle::Detailed => Self::Detailed,
            ThemeHeaderStyle::HeatOnly => Self::HeatOnly,
        }
    }
}

impl From<performance_types::MetricCapability> for MetricCapability {
    fn from(value: performance_types::MetricCapability) -> Self {
        Self {
            metric: value.metric.into(),
            target: value.target.into(),
            supported: value.supported,
            disabled_reason: value.disabled_reason,
            accuracy: value.accuracy.map(Into::into),
        }
    }
}

impl From<MetricCapability> for performance_types::MetricCapability {
    fn from(value: MetricCapability) -> Self {
        Self {
            metric: value.metric.into(),
            target: value.target.into(),
            supported: value.supported,
            disabled_reason: value.disabled_reason,
            accuracy: value.accuracy.map(Into::into),
        }
    }
}

impl From<performance_types::ThemeHeaderSettings> for ThemeHeaderSettings {
    fn from(value: performance_types::ThemeHeaderSettings) -> Self {
        Self {
            enabled: value.enabled,
            sample_interval_ms: value.sample_interval_ms,
            scope: value.scope.into(),
            style: value.style.into(),
            cpu_percent_mode: value.cpu_percent_mode.into(),
            metrics: value.metrics.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<ThemeHeaderSettings> for performance_types::ThemeHeaderSettings {
    fn from(value: ThemeHeaderSettings) -> Self {
        Self {
            enabled: value.enabled,
            sample_interval_ms: value.sample_interval_ms,
            scope: value.scope.into(),
            style: value.style.into(),
            cpu_percent_mode: value.cpu_percent_mode.into(),
            metrics: value.metrics.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<performance_types::MetricWatch> for MetricWatch {
    fn from(value: performance_types::MetricWatch) -> Self {
        Self {
            id: value.id,
            target: value.target.into(),
            metrics: value.metrics.into_iter().map(Into::into).collect(),
            interval_ms: value.interval_ms,
            cpu_percent_mode: value.cpu_percent_mode.into(),
        }
    }
}

impl From<MetricWatch> for performance_types::MetricWatch {
    fn from(value: MetricWatch) -> Self {
        Self {
            id: value.id,
            target: value.target.into(),
            metrics: value.metrics.into_iter().map(Into::into).collect(),
            interval_ms: value.interval_ms,
            cpu_percent_mode: value.cpu_percent_mode.into(),
        }
    }
}

fn f32_to_wire(value: f32) -> String {
    value.to_string()
}

fn f32_from_wire(value: &str) -> f32 {
    value.parse().unwrap_or_default()
}

impl From<SystemMetricsSnapshot> for performance_types::SystemMetricsSnapshot {
    fn from(value: SystemMetricsSnapshot) -> Self {
        Self {
            cpu_percent: f32_to_wire(value.cpu_percent),
            cpu_raw_percent: f32_to_wire(value.cpu_raw_percent),
            cpu_normalized_percent: f32_to_wire(value.cpu_normalized_percent),
            memory_used_bytes: value.memory_used_bytes,
            memory_total_bytes: value.memory_total_bytes,
        }
    }
}

impl From<performance_types::SystemMetricsSnapshot> for SystemMetricsSnapshot {
    fn from(value: performance_types::SystemMetricsSnapshot) -> Self {
        Self {
            cpu_percent: f32_from_wire(&value.cpu_percent),
            cpu_raw_percent: f32_from_wire(&value.cpu_raw_percent),
            cpu_normalized_percent: f32_from_wire(&value.cpu_normalized_percent),
            memory_used_bytes: value.memory_used_bytes,
            memory_total_bytes: value.memory_total_bytes,
        }
    }
}

impl From<ProcessMetricsSnapshot> for performance_types::ProcessMetricsSnapshot {
    fn from(value: ProcessMetricsSnapshot) -> Self {
        Self {
            pid: value.pid,
            cpu_percent: f32_to_wire(value.cpu_percent),
            cpu_raw_percent: f32_to_wire(value.cpu_raw_percent),
            cpu_normalized_percent: f32_to_wire(value.cpu_normalized_percent),
            memory_bytes: value.memory_bytes,
            process_count: value.process_count,
        }
    }
}

impl From<performance_types::ProcessMetricsSnapshot> for ProcessMetricsSnapshot {
    fn from(value: performance_types::ProcessMetricsSnapshot) -> Self {
        Self {
            pid: value.pid,
            cpu_percent: f32_from_wire(&value.cpu_percent),
            cpu_raw_percent: f32_from_wire(&value.cpu_raw_percent),
            cpu_normalized_percent: f32_from_wire(&value.cpu_normalized_percent),
            memory_bytes: value.memory_bytes,
            process_count: value.process_count,
        }
    }
}

impl From<PaneMetricsSnapshot> for performance_types::PaneMetricsSnapshot {
    fn from(value: PaneMetricsSnapshot) -> Self {
        Self {
            pane_id: value.pane_id,
            session_id: value.session_id,
            pid: value.pid,
            process_group_id: value.process_group_id,
            cpu_percent: f32_to_wire(value.cpu_percent),
            cpu_raw_percent: f32_to_wire(value.cpu_raw_percent),
            cpu_normalized_percent: f32_to_wire(value.cpu_normalized_percent),
            memory_bytes: value.memory_bytes,
            process_count: value.process_count,
            available: value.available,
        }
    }
}

impl From<performance_types::PaneMetricsSnapshot> for PaneMetricsSnapshot {
    fn from(value: performance_types::PaneMetricsSnapshot) -> Self {
        Self {
            pane_id: value.pane_id,
            session_id: value.session_id,
            pid: value.pid,
            process_group_id: value.process_group_id,
            cpu_percent: f32_from_wire(&value.cpu_percent),
            cpu_raw_percent: f32_from_wire(&value.cpu_raw_percent),
            cpu_normalized_percent: f32_from_wire(&value.cpu_normalized_percent),
            memory_bytes: value.memory_bytes,
            process_count: value.process_count,
            available: value.available,
        }
    }
}

impl From<MetricsSnapshot> for performance_types::MetricsSnapshot {
    fn from(value: MetricsSnapshot) -> Self {
        Self {
            sampled_at_epoch_ms: value.sampled_at_epoch_ms,
            watches: value.watches.into_iter().map(Into::into).collect(),
            system: value.system.into(),
            processes: value
                .processes
                .into_iter()
                .map(|(pid, snapshot)| (pid, snapshot.into()))
                .collect(),
            panes: value
                .panes
                .into_iter()
                .map(|(pane_id, snapshot)| (pane_id, snapshot.into()))
                .collect(),
        }
    }
}

impl From<performance_types::MetricsSnapshot> for MetricsSnapshot {
    fn from(value: performance_types::MetricsSnapshot) -> Self {
        Self {
            sampled_at_epoch_ms: value.sampled_at_epoch_ms,
            watches: value.watches.into_iter().map(Into::into).collect(),
            system: value.system.into(),
            processes: value
                .processes
                .into_iter()
                .map(|(pid, snapshot)| (pid, snapshot.into()))
                .collect(),
            panes: value
                .panes
                .into_iter()
                .map(|(pane_id, snapshot)| (pane_id, snapshot.into()))
                .collect(),
        }
    }
}

impl From<bmux_plugin_sdk::PromptFormValue> for performance_types::PromptFormValue {
    fn from(value: bmux_plugin_sdk::PromptFormValue) -> Self {
        match value {
            bmux_plugin_sdk::PromptFormValue::Bool(value) => Self::Bool { value },
            bmux_plugin_sdk::PromptFormValue::Text(value) => Self::Text { value },
            bmux_plugin_sdk::PromptFormValue::Integer(value) => Self::Integer { value },
            bmux_plugin_sdk::PromptFormValue::Number(value) => Self::Number { value },
            bmux_plugin_sdk::PromptFormValue::Single(value) => Self::Single { value },
            bmux_plugin_sdk::PromptFormValue::Multi(values) => Self::Multi { values },
        }
    }
}

impl From<performance_types::PromptFormValue> for bmux_plugin_sdk::PromptFormValue {
    fn from(value: performance_types::PromptFormValue) -> Self {
        match value {
            performance_types::PromptFormValue::Bool { value } => Self::Bool(value),
            performance_types::PromptFormValue::Text { value } => Self::Text(value),
            performance_types::PromptFormValue::Integer { value } => Self::Integer(value),
            performance_types::PromptFormValue::Number { value } => Self::Number(value),
            performance_types::PromptFormValue::Single { value } => Self::Single(value),
            performance_types::PromptFormValue::Multi { values } => Self::Multi(values),
        }
    }
}

impl From<PromptRequest> for performance_types::PromptForm {
    fn from(value: PromptRequest) -> Self {
        Self {
            encoded: serde_json::to_vec(&value).unwrap_or_default(),
        }
    }
}

impl TryFrom<performance_types::PromptForm> for PromptRequest {
    type Error = serde_json::Error;

    fn try_from(value: performance_types::PromptForm) -> Result<Self, Self::Error> {
        serde_json::from_slice(&value.encoded)
    }
}

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

/// Metric target selected by a watch.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MetricTarget {
    /// Whole-machine CPU/memory metrics.
    System,
    /// A process tree rooted at a specific process id.
    Process { pid: u32 },
    /// A bmux pane's process tree, resolved through pane-runtime state.
    Pane { pane_id: Uuid },
}

/// Metric names a watch may request.
#[derive(
    Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq, PartialOrd, Ord,
)]
#[serde(rename_all = "snake_case")]
pub enum MetricName {
    CpuPercent,
    MemoryBytes,
    ProcessCount,
    DiskReadBytesPerSec,
    DiskWriteBytesPerSec,
    NetworkRxBytesPerSec,
    NetworkTxBytesPerSec,
}

#[derive(
    Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq, PartialOrd, Ord,
)]
#[serde(rename_all = "snake_case")]
pub enum ThemeHeaderMetric {
    Cpu,
    Memory,
    ProcessCount,
    DiskRead,
    DiskWrite,
    NetworkRx,
    NetworkTx,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MetricTargetKind {
    System,
    Process,
    Pane,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MetricAccuracy {
    Exact,
    Estimated,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct MetricCapability {
    pub metric: ThemeHeaderMetric,
    pub target: MetricTargetKind,
    pub supported: bool,
    pub disabled_reason: Option<String>,
    pub accuracy: Option<MetricAccuracy>,
}

/// How `cpu_percent` should be presented in consumer-facing snapshots.
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CpuPercentMode {
    /// Normalize multicore process usage into a 0..100 whole-machine load.
    #[default]
    Normalized,
    /// Preserve process-tree core-sum semantics where one full core is 100%.
    RawCoreSum,
}

#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThemeHeaderScope {
    #[default]
    Pane,
    System,
    Both,
}

#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThemeHeaderStyle {
    #[default]
    Compact,
    Detailed,
    HeatOnly,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ThemeHeaderSettings {
    pub enabled: bool,
    pub sample_interval_ms: u64,
    pub scope: ThemeHeaderScope,
    pub style: ThemeHeaderStyle,
    pub cpu_percent_mode: CpuPercentMode,
    pub metrics: Vec<ThemeHeaderMetric>,
}

impl Default for ThemeHeaderSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            sample_interval_ms: 1_000,
            scope: ThemeHeaderScope::Pane,
            style: ThemeHeaderStyle::Compact,
            cpu_percent_mode: CpuPercentMode::Normalized,
            metrics: vec![
                ThemeHeaderMetric::Cpu,
                ThemeHeaderMetric::Memory,
                ThemeHeaderMetric::ProcessCount,
            ],
        }
    }
}

/// One subscribed metric watch.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct MetricWatch {
    pub id: String,
    pub target: MetricTarget,
    pub metrics: Vec<MetricName>,
    pub interval_ms: u64,
    #[serde(default)]
    pub cpu_percent_mode: CpuPercentMode,
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
            cpu_percent_mode: CpuPercentMode::Normalized,
        }
    }
}

/// Current metrics sampled for the entire machine.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct SystemMetricsSnapshot {
    pub cpu_percent: f32,
    pub cpu_raw_percent: f32,
    pub cpu_normalized_percent: f32,
    pub memory_used_bytes: u64,
    pub memory_total_bytes: u64,
}

/// Current metrics sampled for one process tree.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ProcessMetricsSnapshot {
    pub pid: u32,
    /// Consumer-facing CPU percentage, shaped by `MetricWatch.cpu_percent_mode`.
    pub cpu_percent: f32,
    /// Raw process-tree CPU where one saturated core is 100%.
    pub cpu_raw_percent: f32,
    /// Whole-machine-normalized CPU percentage, clamped to 0..100.
    pub cpu_normalized_percent: f32,
    pub memory_bytes: u64,
    pub process_count: u32,
}

/// Current metrics sampled for one pane's process tree.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct PaneMetricsSnapshot {
    pub pane_id: Uuid,
    pub session_id: Option<Uuid>,
    pub pid: Option<u32>,
    pub process_group_id: Option<i32>,
    /// Consumer-facing CPU percentage, shaped by `MetricWatch.cpu_percent_mode`.
    pub cpu_percent: f32,
    /// Raw process-tree CPU where one saturated core is 100%.
    pub cpu_raw_percent: f32,
    /// Whole-machine-normalized CPU percentage, clamped to 0..100.
    pub cpu_normalized_percent: f32,
    pub memory_bytes: u64,
    pub process_count: u32,
    pub available: bool,
}

/// Latest metrics state published by `bmux.performance`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
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
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum MetricEvent {
    SnapshotUpdated { sampled_at_epoch_ms: u64 },
}

/// Typed event emitted on the plugin event bus when performance
/// settings change. Server's `spawn_performance_events_bridge` maps
/// this to the legacy wire `Event::PerformanceSettingsUpdated` for
/// cross-process subscribers.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
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
            cpu_percent_mode: CpuPercentMode::Normalized,
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
                cpu_raw_percent: 12.5,
                cpu_normalized_percent: 12.5,
                memory_used_bytes: 100,
                memory_total_bytes: 200,
            },
            processes: BTreeMap::from([(
                7,
                ProcessMetricsSnapshot {
                    pid: 7,
                    cpu_percent: 33.0,
                    cpu_raw_percent: 66.0,
                    cpu_normalized_percent: 33.0,
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
                    cpu_raw_percent: 66.0,
                    cpu_normalized_percent: 33.0,
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

    #[test]
    fn generated_start_watch_request_round_trips_through_service_codec() {
        let request = performance_commands::client::StartWatchRequest {
            watch: performance_types::MetricWatch {
                id: "hot".to_string(),
                target: performance_types::MetricTarget::Pane {
                    pane_id: Uuid::nil(),
                },
                metrics: vec![
                    performance_types::MetricName::CpuPercent,
                    performance_types::MetricName::MemoryBytes,
                ],
                interval_ms: 1_000,
                cpu_percent_mode: performance_types::CpuPercentMode::Normalized,
            },
        };

        let payload =
            bmux_plugin_sdk::encode_service_message(&request).expect("encode performance request");
        assert!(!payload.is_empty());
    }

    #[test]
    fn prompt_form_round_trips_through_service_codec() {
        let request = PromptRequest::form(
            "Performance settings",
            vec![bmux_plugin_sdk::PromptFormSection::new(
                "general",
                "General",
                vec![bmux_plugin_sdk::PromptFormField::new(
                    "enabled",
                    "Enabled",
                    bmux_plugin_sdk::PromptFormFieldKind::Bool { default: true },
                )],
            )],
        );
        let response = performance_types::PromptForm::from(request);

        let payload = bmux_plugin_sdk::encode_service_message(&response)
            .expect("encode prompt form response");
        let decoded: performance_types::PromptForm =
            bmux_plugin_sdk::decode_service_message(&payload).expect("decode prompt form response");

        assert_eq!(decoded, response);
    }
}
