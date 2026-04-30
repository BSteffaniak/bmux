//! Typed public API of the bmux performance plugin.
//!
//! The [`performance_types`], [`performance_state`],
//! [`performance_commands`], [`performance_events`], [`metric_events`],
//! and [`metrics_state`] modules are generated from
//! `bpdl/performance-plugin.bpdl` at compile time via the
//! [`bmux_plugin_schema_macros::schema!`] macro. Hand-written code in
//! this crate is limited to compatibility conversions for existing
//! runtime settings types.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

use bmux_plugin_sdk::{PluginEventKind, PromptRequest};

bmux_plugin_schema_macros::schema! {
    source: "bpdl/performance-plugin.bpdl",
}

pub use bmux_performance_state::{
    PerformanceEventRateLimiter, PerformanceRecordingLevel as PrimitivePerformanceRecordingLevel,
    PerformanceRuntimeSettings as PrimitivePerformanceRuntimeSettings,
};
pub use capabilities::{PERFORMANCE_READ, PERFORMANCE_WRITE};
pub use metric_events::MetricEvent;
pub use performance_events::PerformanceEvent;
pub use performance_types::{
    CpuPercentMode, MetricAccuracy, MetricCapability, MetricName, MetricTarget, MetricTargetKind,
    MetricWatch, MetricsSnapshot, PaneMetricsSnapshot, PerformanceRuntimeSettings,
    ProcessMetricsSnapshot, SystemMetricsSnapshot, ThemeHeaderMetric, ThemeHeaderScope,
    ThemeHeaderSettings, ThemeHeaderStyle,
};

impl From<performance_types::PerformanceRecordingLevel> for PrimitivePerformanceRecordingLevel {
    fn from(value: performance_types::PerformanceRecordingLevel) -> Self {
        match value {
            performance_types::PerformanceRecordingLevel::Off => Self::Off,
            performance_types::PerformanceRecordingLevel::Basic => Self::Basic,
            performance_types::PerformanceRecordingLevel::Detailed => Self::Detailed,
            performance_types::PerformanceRecordingLevel::Trace => Self::Trace,
        }
    }
}

impl From<PrimitivePerformanceRecordingLevel> for performance_types::PerformanceRecordingLevel {
    fn from(value: PrimitivePerformanceRecordingLevel) -> Self {
        match value {
            PrimitivePerformanceRecordingLevel::Off => Self::Off,
            PrimitivePerformanceRecordingLevel::Basic => Self::Basic,
            PrimitivePerformanceRecordingLevel::Detailed => Self::Detailed,
            PrimitivePerformanceRecordingLevel::Trace => Self::Trace,
        }
    }
}

impl From<performance_types::PerformanceRuntimeSettings> for PrimitivePerformanceRuntimeSettings {
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

impl From<PrimitivePerformanceRuntimeSettings> for performance_types::PerformanceRuntimeSettings {
    fn from(value: PrimitivePerformanceRuntimeSettings) -> Self {
        Self {
            recording_level: value.recording_level.into(),
            window_ms: value.window_ms,
            max_events_per_sec: value.max_events_per_sec,
            max_payload_bytes_per_sec: u64::try_from(value.max_payload_bytes_per_sec)
                .unwrap_or(u64::MAX),
        }
    }
}

impl Default for performance_types::ThemeHeaderSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            sample_interval_ms: 1_000,
            scope: performance_types::ThemeHeaderScope::Pane,
            style: performance_types::ThemeHeaderStyle::Compact,
            cpu_percent_mode: performance_types::CpuPercentMode::Normalized,
            metrics: vec![
                performance_types::ThemeHeaderMetric::Cpu,
                performance_types::ThemeHeaderMetric::Memory,
                performance_types::ThemeHeaderMetric::ProcessCount,
            ],
        }
    }
}

impl performance_types::MetricWatch {
    #[must_use]
    pub fn normalized(mut self) -> Self {
        self.interval_ms = self.interval_ms.max(MIN_METRICS_INTERVAL_MS);
        if self.metrics.is_empty() {
            self.metrics = vec![
                performance_types::MetricName::CpuPercent,
                performance_types::MetricName::MemoryBytes,
            ];
        }
        self
    }
}

impl Default for performance_types::MetricWatch {
    fn default() -> Self {
        Self {
            id: DEFAULT_METRICS_WATCH_ID.to_string(),
            target: performance_types::MetricTarget::System,
            metrics: vec![
                performance_types::MetricName::CpuPercent,
                performance_types::MetricName::MemoryBytes,
            ],
            interval_ms: 1_000,
            cpu_percent_mode: performance_types::CpuPercentMode::Normalized,
        }
    }
}

// This would be derivable on the generated struct, but the BPDL macro owns
// the item definition; keep the impl here with the other generated-type
// extensions.
#[allow(clippy::derivable_impls)]
impl Default for performance_types::MetricsSnapshot {
    fn default() -> Self {
        Self {
            sampled_at_epoch_ms: 0,
            watches: Vec::new(),
            system: performance_types::SystemMetricsSnapshot::default(),
            processes: std::collections::BTreeMap::new(),
            panes: std::collections::BTreeMap::new(),
        }
    }
}

impl Default for performance_types::SystemMetricsSnapshot {
    fn default() -> Self {
        Self {
            cpu_percent: 0.0,
            cpu_raw_percent: 0.0,
            cpu_normalized_percent: 0.0,
            memory_used_bytes: 0,
            memory_total_bytes: 0,
        }
    }
}

impl Default for performance_types::ProcessMetricsSnapshot {
    fn default() -> Self {
        Self {
            pid: 0,
            cpu_percent: 0.0,
            cpu_raw_percent: 0.0,
            cpu_normalized_percent: 0.0,
            memory_bytes: 0,
            process_count: 0,
        }
    }
}

impl Default for performance_types::PaneMetricsSnapshot {
    fn default() -> Self {
        Self {
            pane_id: uuid::Uuid::nil(),
            session_id: None,
            pid: None,
            process_group_id: None,
            cpu_percent: 0.0,
            cpu_raw_percent: 0.0,
            cpu_normalized_percent: 0.0,
            memory_bytes: 0,
            process_count: 0,
            available: false,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use uuid::Uuid;

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
