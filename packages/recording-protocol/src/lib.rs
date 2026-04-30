//! Neutral recording protocol DTOs.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

use bmux_attach_image_protocol::AttachPaneImage;
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

/// Recording event payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingPayload<ServerEvent, RequestErrorCode> {
    Bytes {
        #[serde(with = "bmux_codec::serde_bytes_vec")]
        data: Vec<u8>,
    },
    ServerEvent {
        event: ServerEvent,
    },
    RequestStart {
        request_id: u64,
        request_kind: String,
        exclusive: bool,
        /// Full request, binary-encoded.
        #[serde(with = "bmux_codec::serde_bytes_vec")]
        request_data: Vec<u8>,
    },
    RequestDone {
        request_id: u64,
        request_kind: String,
        response_kind: String,
        elapsed_ms: u64,
        /// Full request, binary-encoded.
        #[serde(with = "bmux_codec::serde_bytes_vec")]
        request_data: Vec<u8>,
        /// Full response payload, binary-encoded.
        #[serde(with = "bmux_codec::serde_bytes_vec")]
        response_data: Vec<u8>,
    },
    RequestError {
        request_id: u64,
        request_kind: String,
        error_code: RequestErrorCode,
        message: String,
        elapsed_ms: u64,
    },
    Custom {
        source: String,
        name: String,
        /// Pre-serialized JSON payload bytes.
        #[serde(with = "bmux_codec::serde_bytes_vec")]
        payload: Vec<u8>,
    },
    /// A terminal image extracted from pane output.
    Image {
        /// Protocol identifier: 0=Sixel, 1=KittyGraphics, 2=ITerm2.
        protocol: u8,
        position_row: u16,
        position_col: u16,
        cell_rows: u16,
        cell_cols: u16,
        pixel_width: u32,
        pixel_height: u32,
        /// Raw protocol bytes (sixel body, kitty payload, iTerm2 data).
        #[serde(with = "bmux_codec::serde_bytes_vec")]
        data: Vec<u8>,
    },
}

/// Timeline event envelope persisted in recordings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingEventEnvelope<ServerEvent, RequestErrorCode> {
    pub seq: u64,
    pub mono_ns: u64,
    pub wall_epoch_ms: u64,
    pub session_id: Option<Uuid>,
    pub pane_id: Option<Uuid>,
    pub client_id: Option<Uuid>,
    pub kind: RecordingEventKind,
    pub payload: RecordingPayload<ServerEvent, RequestErrorCode>,
}

/// Display track event shared by recording writers and exporters.
///
/// The `terminal_profile` field stores pre-serialized bytes (codec-encoded
/// terminal profile data) to avoid cross-crate type dependencies. Use `None`
/// when no terminal profile is available, such as headless playbook execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayTrackEvent {
    StreamOpened {
        client_id: Uuid,
        recording_id: Uuid,
        cell_width_px: Option<u16>,
        cell_height_px: Option<u16>,
        window_width_px: Option<u16>,
        window_height_px: Option<u16>,
        /// Pre-serialized terminal profile bytes (binary-encoded), or `None`.
        #[serde(with = "bmux_codec::serde_bytes_vec::option")]
        terminal_profile: Option<Vec<u8>>,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    FrameBytes {
        #[serde(with = "bmux_codec::serde_bytes_vec")]
        data: Vec<u8>,
    },
    CursorSnapshot {
        x: u16,
        y: u16,
        visible: bool,
        shape: DisplayCursorShape,
        blink_enabled: bool,
    },
    Activity {
        kind: DisplayActivityKind,
    },
    StreamClosed,
    /// Snapshot of all visible images for a set of panes at frame time.
    ImageUpdate {
        images: Vec<AttachPaneImage>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayCursorShape {
    Block,
    Bar,
    Underline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayActivityKind {
    Input,
    Output,
    Cursor,
}

/// Display track envelope wraps an event with a monotonic timestamp.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayTrackEnvelope {
    pub mono_ns: u64,
    pub event: DisplayTrackEvent,
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
