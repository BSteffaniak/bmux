use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Map, Value};

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
    if channel.enabled() {
        eprintln!("{}{payload}", channel.marker());
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
}
