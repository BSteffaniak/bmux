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

use bmux_ipc::PerformanceRuntimeSettings;
use bmux_performance_state::PerformanceCaptureSettings;
use bmux_plugin_sdk::{CapabilityId, InterfaceId, PluginEventKind};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
}

/// Typed response variants for the performance plugin's typed service
/// dispatch surface. Both operations return a single variant carrying
/// the current (or newly-updated) settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PerformanceResponse {
    Settings {
        settings: PerformanceRuntimeSettings,
    },
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
