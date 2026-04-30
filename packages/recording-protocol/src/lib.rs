//! Neutral recording protocol DTOs.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingProfile {
    Full,
    Functional,
    Visual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingEventKind {
    PaneInputRaw,
    PaneOutputRaw,
    ProtocolReplyRaw,
    PaneImage,
    ServerEvent,
    RequestStart,
    RequestDone,
    RequestError,
    Custom,
}

/// Recording summary returned by recording APIs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingSummary {
    pub id: Uuid,
    #[serde(default)]
    pub name: Option<String>,
    /// Recording format version. Absent in recordings created before versioning was added.
    #[serde(default = "recording_format_version_default")]
    pub format_version: u32,
    #[serde(default)]
    pub session_id: Option<Uuid>,
    pub capture_input: bool,
    #[serde(default = "recording_profile_default")]
    pub profile: RecordingProfile,
    #[serde(default = "recording_event_kinds_default")]
    pub event_kinds: Vec<RecordingEventKind>,
    pub started_epoch_ms: u64,
    #[serde(default)]
    pub ended_epoch_ms: Option<u64>,
    pub event_count: u64,
    pub payload_bytes: u64,
    pub path: String,
    /// Ordered list of segment file names within the recording directory.
    #[serde(default = "default_segments")]
    pub segments: Vec<String>,
    /// Total bytes written across all segment files (approximate).
    #[serde(default)]
    pub total_segment_bytes: u64,
}

/// Current recording format version.
pub const RECORDING_FORMAT_VERSION: u32 = 6;

const fn recording_format_version_default() -> u32 {
    1 // pre-versioning recordings are treated as version 1
}

fn default_segments() -> Vec<String> {
    vec!["events_0.bin".to_string()]
}

const fn recording_profile_default() -> RecordingProfile {
    RecordingProfile::Full
}

fn recording_event_kinds_default() -> Vec<RecordingEventKind> {
    vec![
        RecordingEventKind::PaneInputRaw,
        RecordingEventKind::PaneOutputRaw,
        RecordingEventKind::ProtocolReplyRaw,
        RecordingEventKind::PaneImage,
        RecordingEventKind::ServerEvent,
        RecordingEventKind::RequestStart,
        RecordingEventKind::RequestDone,
        RecordingEventKind::RequestError,
    ]
}

/// Recording runtime status details.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingStatus {
    pub active: Option<RecordingSummary>,
    pub queue_len: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingCaptureTarget {
    pub recording_id: Uuid,
    pub path: String,
    #[serde(default)]
    pub rolling_window_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RecordingRollingStartOptions {
    #[serde(default)]
    pub window_secs: Option<u64>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub event_kinds: Option<Vec<RecordingEventKind>>,
    #[serde(default)]
    pub capture_input: Option<bool>,
    #[serde(default)]
    pub capture_output: Option<bool>,
    #[serde(default)]
    pub capture_events: Option<bool>,
    #[serde(default)]
    pub capture_protocol_replies: Option<bool>,
    #[serde(default)]
    pub capture_images: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RecordingRollingUsage {
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub files: u64,
    #[serde(default)]
    pub directories: u64,
    #[serde(default)]
    pub recording_dirs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingRollingStatus {
    pub root_path: String,
    #[serde(default)]
    pub auto_start: bool,
    #[serde(default)]
    pub available: bool,
    #[serde(default)]
    pub active: Option<RecordingSummary>,
    #[serde(default)]
    pub rolling_window_secs: Option<u64>,
    #[serde(default)]
    pub event_kinds: Vec<RecordingEventKind>,
    #[serde(default)]
    pub usage: RecordingRollingUsage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingRollingClearReport {
    pub root_path: String,
    #[serde(default)]
    pub was_active: bool,
    #[serde(default)]
    pub restarted: bool,
    #[serde(default)]
    pub stopped_recording_id: Option<Uuid>,
    #[serde(default)]
    pub restarted_recording: Option<RecordingSummary>,
    #[serde(default)]
    pub usage_before: RecordingRollingUsage,
    #[serde(default)]
    pub usage_after: RecordingRollingUsage,
}
