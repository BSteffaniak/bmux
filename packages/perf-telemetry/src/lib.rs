use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Map, Value};

const TIMING_MODE_ENV: &str = "BMUX_PHASE_TIMING_MODE";
const BUFFERED_ENV: &str = "BMUX_PHASE_TIMING_BUFFERED";
const FILTER_ENV: &str = "BMUX_PHASE_TIMING_FILTER";
const BUFFER_LIMIT_ENV: &str = "BMUX_PHASE_TIMING_BUFFER_LIMIT";
const DEFAULT_BUFFER_LIMIT: usize = 16_384;

#[derive(Debug, Clone)]
struct BufferedPhaseEvent {
    channel: PhaseChannel,
    payload: Value,
}

static PHASE_BUFFER: OnceLock<Mutex<Vec<BufferedPhaseEvent>>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseChannel {
    Plugin,
    Attach,
    Service,
    Ipc,
    Storage,
}

pub const ALL_PHASE_CHANNELS: [PhaseChannel; 5] = [
    PhaseChannel::Plugin,
    PhaseChannel::Attach,
    PhaseChannel::Service,
    PhaseChannel::Ipc,
    PhaseChannel::Storage,
];

impl PhaseChannel {
    #[must_use]
    pub const fn marker(self) -> &'static str {
        match self {
            Self::Plugin => "[bmux-plugin-phase-json]",
            Self::Attach => "[bmux-attach-phase-json]",
            Self::Service => "[bmux-service-phase-json]",
            Self::Ipc => "[bmux-ipc-phase-json]",
            Self::Storage => "[bmux-storage-phase-json]",
        }
    }

    #[must_use]
    pub const fn env_var(self) -> &'static str {
        match self {
            Self::Plugin => "BMUX_PLUGIN_PHASE_TIMING",
            Self::Attach => "BMUX_ATTACH_PHASE_TIMING",
            Self::Service => "BMUX_SERVICE_PHASE_TIMING",
            Self::Ipc => "BMUX_IPC_PHASE_TIMING",
            Self::Storage => "BMUX_PLUGIN_STORAGE_PHASE_TIMING",
        }
    }

    #[must_use]
    pub fn enabled(self) -> bool {
        std::env::var_os(self.env_var()).is_some()
    }
}

#[must_use]
pub fn phase_marker_payload(line: &str) -> Option<&str> {
    ALL_PHASE_CHANNELS.iter().find_map(|channel| {
        line.split_once(channel.marker())
            .map(|(_, payload)| payload.trim())
    })
}

pub fn emit(channel: PhaseChannel, payload: &Value) {
    if !channel.enabled() || !phase_filter_allows(payload) {
        return;
    }
    if buffered_mode_enabled() {
        buffer_event(channel, payload.clone());
    } else {
        write_phase_event(channel, payload);
    }
}

pub fn flush() {
    let Some(buffer) = PHASE_BUFFER.get() else {
        return;
    };
    let events = match buffer.lock() {
        Ok(mut guard) if !guard.is_empty() => guard.drain(..).collect::<Vec<_>>(),
        Ok(_) | Err(_) => return,
    };
    for event in events {
        write_phase_event(event.channel, &event.payload);
    }
}

fn buffered_mode_enabled() -> bool {
    matches!(
        std::env::var(TIMING_MODE_ENV).as_deref(),
        Ok("buffered" | "buffer" | "batch" | "batched")
    ) || std::env::var_os(BUFFERED_ENV).is_some()
}

fn buffer_event(channel: PhaseChannel, payload: Value) {
    let buffer = PHASE_BUFFER.get_or_init(|| Mutex::new(Vec::new()));
    let should_flush = {
        let Ok(mut guard) = buffer.lock() else {
            write_phase_event(channel, &payload);
            return;
        };
        guard.push(BufferedPhaseEvent { channel, payload });
        guard.len() >= buffer_limit()
    };
    if should_flush {
        flush();
    }
}

fn buffer_limit() -> usize {
    std::env::var(BUFFER_LIMIT_ENV)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_BUFFER_LIMIT)
}

fn write_phase_event(channel: PhaseChannel, payload: &Value) {
    eprintln!("{}{payload}", channel.marker());
}

fn phase_filter_allows(payload: &Value) -> bool {
    let Some(filter) = std::env::var_os(FILTER_ENV) else {
        return true;
    };
    let filter = filter.to_string_lossy();
    let Some(phase) = payload.get("phase").and_then(Value::as_str) else {
        return true;
    };
    filter
        .split(',')
        .map(str::trim)
        .filter(|pattern| !pattern.is_empty())
        .any(|pattern| phase_matches_filter(phase, pattern))
}

fn phase_matches_filter(phase: &str, pattern: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('*') {
        phase.starts_with(prefix)
    } else {
        phase == pattern
    }
}

#[derive(Debug, Clone)]
pub struct PhasePayload {
    fields: Map<String, Value>,
}

impl PhasePayload {
    #[must_use]
    pub fn new(phase: impl Into<String>) -> Self {
        let mut fields = Map::new();
        fields.insert("phase".to_string(), Value::String(phase.into()));
        Self { fields }
    }

    #[must_use]
    pub fn field(mut self, key: impl Into<String>, value: impl Serialize) -> Self {
        self.fields.insert(key.into(), to_value(value));
        self
    }

    #[must_use]
    pub fn service_fields(
        self,
        capability: impl Serialize,
        kind: impl Serialize,
        interface_id: impl Serialize,
        operation: impl Serialize,
    ) -> Self {
        self.field("capability", capability)
            .field("kind", kind)
            .field("interface_id", interface_id)
            .field("operation", operation)
    }

    #[must_use]
    pub fn extend(mut self, fields: Map<String, Value>) -> Self {
        self.fields.extend(fields);
        self
    }

    #[must_use]
    pub fn finish(self) -> Value {
        Value::Object(self.fields)
    }

    #[must_use]
    pub fn into_fields(self) -> Map<String, Value> {
        self.fields
    }
}

fn to_value(value: impl Serialize) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

#[derive(Debug, Clone)]
pub struct PhaseTimer {
    started_at: Instant,
}

impl PhaseTimer {
    #[must_use]
    pub fn start() -> Self {
        Self {
            started_at: Instant::now(),
        }
    }

    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    #[must_use]
    pub fn elapsed_us(&self) -> u128 {
        self.elapsed().as_micros()
    }

    #[must_use]
    pub fn elapsed_ms(&self) -> u128 {
        self.elapsed().as_millis()
    }
}

#[must_use]
pub fn elapsed_us_since(started_at: Instant) -> u128 {
    started_at.elapsed().as_micros()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_marker_payload_extracts_known_channel_payload() {
        assert_eq!(
            phase_marker_payload("INFO [bmux-plugin-phase-json]{\"phase\":\"plugin.command\"}"),
            Some("{\"phase\":\"plugin.command\"}")
        );
    }

    #[test]
    fn phase_marker_payload_ignores_unmarked_lines() {
        assert_eq!(phase_marker_payload("plain stderr"), None);
    }

    #[test]
    fn phase_filter_supports_exact_and_prefix_patterns() {
        assert!(phase_matches_filter(
            "service_pipeline.step",
            "service_pipeline.*"
        ));
        assert!(phase_matches_filter(
            "service.server_invoke",
            "service.server_invoke"
        ));
        assert!(!phase_matches_filter("plugin.command", "service.*"));
    }
}
