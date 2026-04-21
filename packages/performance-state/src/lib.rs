//! Neutral primitive crate for the performance-plugin domain.
//!
//! Hosts the `PerformanceCaptureSettings` record, `Reader`/`Writer`
//! traits, a handle newtype for registry lookup, and a `DefaultNoOp`
//! fallback impl.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

use bmux_config::{BmuxConfig, PerformanceRecordingLevel as ConfigPerformanceRecordingLevel};
use bmux_ipc::{PerformanceRecordingLevel, PerformanceRuntimeSettings};
use std::sync::Arc;

/// Normalized performance capture settings.
///
/// Lives here because server's event-push rate limiter reads it on
/// the hot path. The performance plugin's typed handlers read/write
/// it via the `PerformanceSettingsWriter` trait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PerformanceCaptureSettings {
    pub level: PerformanceRecordingLevel,
    pub window_ms: u64,
    pub max_events_per_sec: u32,
    pub max_payload_bytes_per_sec: usize,
}

impl Default for PerformanceCaptureSettings {
    fn default() -> Self {
        Self::from_config(&BmuxConfig::default())
    }
}

impl PerformanceCaptureSettings {
    const fn from_config_level(
        level: ConfigPerformanceRecordingLevel,
    ) -> PerformanceRecordingLevel {
        match level {
            ConfigPerformanceRecordingLevel::Off => PerformanceRecordingLevel::Off,
            ConfigPerformanceRecordingLevel::Basic => PerformanceRecordingLevel::Basic,
            ConfigPerformanceRecordingLevel::Detailed => PerformanceRecordingLevel::Detailed,
            ConfigPerformanceRecordingLevel::Trace => PerformanceRecordingLevel::Trace,
        }
    }

    #[must_use]
    pub fn from_config(config: &BmuxConfig) -> Self {
        let perf = &config.performance;
        Self {
            level: Self::from_config_level(perf.recording_level),
            window_ms: perf.window_ms.max(1),
            max_events_per_sec: perf.max_events_per_sec.max(1),
            max_payload_bytes_per_sec: perf.max_payload_bytes_per_sec.max(1),
        }
    }

    #[must_use]
    pub fn from_runtime_settings(settings: &PerformanceRuntimeSettings) -> Self {
        Self {
            level: settings.recording_level,
            window_ms: settings.window_ms.max(1),
            max_events_per_sec: settings.max_events_per_sec.max(1),
            max_payload_bytes_per_sec: settings.max_payload_bytes_per_sec.max(1),
        }
    }

    #[must_use]
    pub const fn to_runtime_settings(self) -> PerformanceRuntimeSettings {
        PerformanceRuntimeSettings {
            recording_level: self.level,
            window_ms: self.window_ms,
            max_events_per_sec: self.max_events_per_sec,
            max_payload_bytes_per_sec: self.max_payload_bytes_per_sec,
        }
    }

    const fn level_rank(level: PerformanceRecordingLevel) -> u8 {
        match level {
            PerformanceRecordingLevel::Off => 0,
            PerformanceRecordingLevel::Basic => 1,
            PerformanceRecordingLevel::Detailed => 2,
            PerformanceRecordingLevel::Trace => 3,
        }
    }

    #[must_use]
    pub const fn level_at_least(self, level: PerformanceRecordingLevel) -> bool {
        Self::level_rank(self.level) >= Self::level_rank(level)
    }

    #[must_use]
    pub const fn enabled(self) -> bool {
        !matches!(self.level, PerformanceRecordingLevel::Off)
    }

    #[must_use]
    pub const fn level_label(self) -> &'static str {
        match self.level {
            PerformanceRecordingLevel::Off => "off",
            PerformanceRecordingLevel::Basic => "basic",
            PerformanceRecordingLevel::Detailed => "detailed",
            PerformanceRecordingLevel::Trace => "trace",
        }
    }
}

/// Read-only view over performance settings.
pub trait PerformanceSettingsReader: Send + Sync {
    /// Snapshot the current settings.
    fn current(&self) -> PerformanceCaptureSettings;
}

/// Mutation surface over performance settings.
pub trait PerformanceSettingsWriter: PerformanceSettingsReader {
    /// Replace the current settings with `settings`.
    fn set(&self, settings: PerformanceCaptureSettings);
}

/// Registry newtype wrapping an `Arc<dyn PerformanceSettingsWriter>`.
#[derive(Clone)]
pub struct PerformanceSettingsHandle(pub Arc<dyn PerformanceSettingsWriter>);

impl PerformanceSettingsHandle {
    #[must_use]
    pub fn new<W: PerformanceSettingsWriter + 'static>(writer: W) -> Self {
        Self(Arc::new(writer))
    }

    #[must_use]
    pub fn from_arc(writer: Arc<dyn PerformanceSettingsWriter>) -> Self {
        Self(writer)
    }

    #[must_use]
    pub fn noop() -> Self {
        Self::new(NoopPerformanceSettings)
    }
}

/// No-op default impl. Returns default settings for reads; silently
/// drops writes. Registered by server at startup; performance plugin
/// overwrites during `activate`.
#[derive(Debug, Default)]
pub struct NoopPerformanceSettings;

impl PerformanceSettingsReader for NoopPerformanceSettings {
    fn current(&self) -> PerformanceCaptureSettings {
        PerformanceCaptureSettings::default()
    }
}

impl PerformanceSettingsWriter for NoopPerformanceSettings {
    fn set(&self, _settings: PerformanceCaptureSettings) {}
}
