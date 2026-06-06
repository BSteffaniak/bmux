use super::{BmuxConfig, Context, Instant, Result, Uuid, active_runtime_name};
use bmux_performance_state::{
    PERF_RECORDING_SCHEMA_VERSION, PERF_RECORDING_SOURCE,
    PerformanceRecordingLevel as RuntimePerformanceRecordingLevel,
    PerformanceRuntimeSettings as RuntimePerformanceRuntimeSettings,
};
use bmux_recording_plugin_api::{recording_commands, recording_types};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum PerfCaptureLevel {
    Off,
    Basic,
    Detailed,
    Trace,
}

impl PerfCaptureLevel {
    #[must_use]
    pub(super) const fn from_config(level: bmux_config::PerformanceRecordingLevel) -> Self {
        match level {
            bmux_config::PerformanceRecordingLevel::Off => Self::Off,
            bmux_config::PerformanceRecordingLevel::Basic => Self::Basic,
            bmux_config::PerformanceRecordingLevel::Detailed => Self::Detailed,
            bmux_config::PerformanceRecordingLevel::Trace => Self::Trace,
        }
    }

    #[must_use]
    pub(super) const fn from_runtime(level: RuntimePerformanceRecordingLevel) -> Self {
        match level {
            RuntimePerformanceRecordingLevel::Off => Self::Off,
            RuntimePerformanceRecordingLevel::Basic => Self::Basic,
            RuntimePerformanceRecordingLevel::Detailed => Self::Detailed,
            RuntimePerformanceRecordingLevel::Trace => Self::Trace,
        }
    }

    #[must_use]
    pub(super) const fn from_plugin(
        level: bmux_performance_plugin_api::performance_types::PerformanceRecordingLevel,
    ) -> Self {
        match level {
            bmux_performance_plugin_api::performance_types::PerformanceRecordingLevel::Off => {
                Self::Off
            }
            bmux_performance_plugin_api::performance_types::PerformanceRecordingLevel::Basic => {
                Self::Basic
            }
            bmux_performance_plugin_api::performance_types::PerformanceRecordingLevel::Detailed => {
                Self::Detailed
            }
            bmux_performance_plugin_api::performance_types::PerformanceRecordingLevel::Trace => {
                Self::Trace
            }
        }
    }

    #[must_use]
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Basic => "basic",
            Self::Detailed => "detailed",
            Self::Trace => "trace",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct PerfCaptureSettings {
    level: PerfCaptureLevel,
    window_ms: u64,
    max_events_per_sec: u32,
    max_payload_bytes_per_sec: usize,
}

impl PerfCaptureSettings {
    #[must_use]
    pub(super) fn from_config(config: &BmuxConfig) -> Self {
        let perf = &config.performance;
        Self {
            level: PerfCaptureLevel::from_config(perf.recording_level),
            window_ms: perf.window_ms.max(1),
            max_events_per_sec: perf.max_events_per_sec.max(1),
            max_payload_bytes_per_sec: perf.max_payload_bytes_per_sec.max(1),
        }
    }

    #[must_use]
    pub(super) fn from_runtime_settings(settings: &RuntimePerformanceRuntimeSettings) -> Self {
        Self {
            level: PerfCaptureLevel::from_runtime(settings.recording_level),
            window_ms: settings.window_ms.max(1),
            max_events_per_sec: settings.max_events_per_sec.max(1),
            max_payload_bytes_per_sec: settings.max_payload_bytes_per_sec.max(1),
        }
    }

    #[must_use]
    pub(super) fn from_plugin_settings(
        settings: &bmux_performance_plugin_api::performance_types::PerformanceRuntimeSettings,
    ) -> Self {
        Self {
            level: PerfCaptureLevel::from_plugin(settings.recording_level),
            window_ms: settings.window_ms.max(1),
            max_events_per_sec: settings.max_events_per_sec.max(1),
            max_payload_bytes_per_sec: usize::try_from(settings.max_payload_bytes_per_sec)
                .unwrap_or(usize::MAX)
                .max(1),
        }
    }
}

#[derive(Debug)]
pub(super) struct PerfEventEmitter {
    settings: PerfCaptureSettings,
    rate_window_started_at: Instant,
    emitted_events_in_window: u32,
    emitted_payload_bytes_in_window: usize,
    dropped_events_since_emit: u64,
    dropped_payload_bytes_since_emit: u64,
}

impl PerfEventEmitter {
    #[must_use]
    pub(super) fn new(settings: PerfCaptureSettings) -> Self {
        Self {
            settings,
            rate_window_started_at: Instant::now(),
            emitted_events_in_window: 0,
            emitted_payload_bytes_in_window: 0,
            dropped_events_since_emit: 0,
            dropped_payload_bytes_since_emit: 0,
        }
    }

    pub(super) fn update_settings(&mut self, settings: PerfCaptureSettings) {
        self.settings = settings;
        self.rate_window_started_at = Instant::now();
        self.emitted_events_in_window = 0;
        self.emitted_payload_bytes_in_window = 0;
        self.dropped_events_since_emit = 0;
        self.dropped_payload_bytes_since_emit = 0;
    }

    #[must_use]
    pub(super) const fn window_ms(&self) -> u64 {
        self.settings.window_ms
    }

    #[must_use]
    pub(super) fn level_at_least(&self, level: PerfCaptureLevel) -> bool {
        self.settings.level >= level
    }

    #[must_use]
    pub(super) fn enabled(&self) -> bool {
        self.settings.level != PerfCaptureLevel::Off
    }

    fn reset_rate_window_if_needed(&mut self) {
        if self.rate_window_started_at.elapsed() >= std::time::Duration::from_secs(1) {
            self.rate_window_started_at = Instant::now();
            self.emitted_events_in_window = 0;
            self.emitted_payload_bytes_in_window = 0;
        }
    }

    fn can_emit_payload(&mut self, payload_len: usize) -> bool {
        if !self.enabled() {
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

    fn normalized_payload(&mut self, payload: serde_json::Value) -> serde_json::Value {
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
            serde_json::Value::from(PERF_RECORDING_SCHEMA_VERSION),
        );
        object.insert(
            "level".to_string(),
            serde_json::Value::String(self.settings.level.as_str().to_string()),
        );
        object.insert(
            "runtime".to_string(),
            serde_json::Value::String(active_runtime_name()),
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

        serde_json::Value::Object(object)
    }

    pub(super) async fn emit_with_client(
        &mut self,
        client: &mut bmux_client::BmuxClient,
        session_id: Option<Uuid>,
        pane_id: Option<Uuid>,
        event_name: &str,
        payload: serde_json::Value,
    ) -> Result<()> {
        if !self.enabled() {
            return Ok(());
        }

        let payload = self.normalized_payload(payload);
        let encoded = serde_json::to_vec(&payload).context("failed encoding perf payload")?;
        if !self.can_emit_payload(encoded.len()) {
            return Ok(());
        }

        recording_commands::client::write_custom_event(
            client,
            session_id,
            pane_id,
            PERF_RECORDING_SOURCE.to_string(),
            event_name.to_string(),
            encoded,
        )
        .await?
        .map_err(recording_plugin_error)
    }

    pub(super) async fn emit_with_streaming_client(
        &mut self,
        client: &mut bmux_client::StreamingBmuxClient,
        session_id: Option<Uuid>,
        pane_id: Option<Uuid>,
        event_name: &str,
        payload: serde_json::Value,
    ) -> Result<()> {
        if !self.enabled() {
            return Ok(());
        }

        let payload = self.normalized_payload(payload);
        let encoded = serde_json::to_vec(&payload).context("failed encoding perf payload")?;
        if !self.can_emit_payload(encoded.len()) {
            return Ok(());
        }

        recording_commands::client::write_custom_event(
            client,
            session_id,
            pane_id,
            PERF_RECORDING_SOURCE.to_string(),
            event_name.to_string(),
            encoded,
        )
        .await?
        .map_err(recording_plugin_error)
    }
}

fn recording_plugin_error(error: recording_types::RecordingError) -> anyhow::Error {
    match error {
        recording_types::RecordingError::NoActive => anyhow::anyhow!("no active recording"),
        recording_types::RecordingError::Unavailable => {
            anyhow::anyhow!("recording runtime unavailable")
        }
        recording_types::RecordingError::Failed { reason } => anyhow::anyhow!(reason),
    }
}

#[allow(clippy::cast_possible_truncation)]
fn epoch_millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perf_event_emitter_surfaces_drop_counters_on_next_payload() {
        let settings = PerfCaptureSettings {
            level: PerfCaptureLevel::Basic,
            window_ms: 1_000,
            max_events_per_sec: 1,
            max_payload_bytes_per_sec: 4_096,
        };
        let mut emitter = PerfEventEmitter::new(settings);

        let payload_one = emitter.normalized_payload(serde_json::json!({"sample": 1}));
        let encoded_one = serde_json::to_vec(&payload_one).expect("payload should encode");
        assert!(emitter.can_emit_payload(encoded_one.len()));

        let payload_two = emitter.normalized_payload(serde_json::json!({"sample": 2}));
        let encoded_two = serde_json::to_vec(&payload_two).expect("payload should encode");
        assert!(!emitter.can_emit_payload(encoded_two.len()));

        let payload_three = emitter.normalized_payload(serde_json::json!({"sample": 3}));
        let object = payload_three
            .as_object()
            .expect("normalized payload should be object");
        assert_eq!(
            object
                .get("dropped_events_since_emit")
                .and_then(serde_json::Value::as_u64),
            Some(1)
        );
        assert!(
            object
                .get("dropped_payload_bytes_since_emit")
                .and_then(serde_json::Value::as_u64)
                .is_some_and(|bytes| bytes > 0),
            "drop payload bytes should be included after a rate-limited emit"
        );
    }
}
