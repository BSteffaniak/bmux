#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Cross-platform IPC protocol models for bmux.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub mod frame;
pub mod transport;

/// Cross-platform local IPC endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "transport", content = "address", rename_all = "snake_case")]
pub enum IpcEndpoint {
    UnixSocket(PathBuf),
    WindowsNamedPipe(String),
}

impl IpcEndpoint {
    /// Construct a Unix domain socket endpoint.
    #[must_use]
    pub fn unix_socket(path: impl Into<PathBuf>) -> Self {
        Self::UnixSocket(path.into())
    }

    /// Construct a Windows named pipe endpoint.
    #[must_use]
    pub fn windows_named_pipe(name: impl Into<String>) -> Self {
        Self::WindowsNamedPipe(name.into())
    }

    /// Return the Unix socket path when this endpoint uses Unix sockets.
    #[must_use]
    pub fn as_unix_socket(&self) -> Option<&Path> {
        match self {
            Self::UnixSocket(path) => Some(path.as_path()),
            Self::WindowsNamedPipe(_) => None,
        }
    }

    /// Return the Windows named pipe when this endpoint uses named pipes.
    #[must_use]
    pub const fn as_windows_named_pipe(&self) -> Option<&str> {
        match self {
            Self::UnixSocket(_) => None,
            Self::WindowsNamedPipe(name) => Some(name.as_str()),
        }
    }
}

/// Current IPC protocol version.
pub const CURRENT_PROTOCOL_VERSION: u16 = 3;

/// Current wire-compatibility epoch for IPC framing.
pub const CURRENT_WIRE_EPOCH: u16 = CURRENT_PROTOCOL_VERSION;

/// Current negotiated protocol revision.
pub const CURRENT_PROTOCOL_REVISION: u32 = 1;

/// Minimum protocol revision this build can negotiate.
pub const MIN_SUPPORTED_PROTOCOL_REVISION: u32 = 1;

pub const CORE_CAPABILITY_SESSION: &str = "core.session";
pub const CORE_CAPABILITY_ATTACH: &str = "core.attach";
pub const CORE_CAPABILITY_PANE_IO: &str = "core.pane_io";
pub const CORE_CAPABILITY_DETACH: &str = "core.detach";

/// Core protocol capabilities required for baseline bmux operation.
pub const CORE_PROTOCOL_CAPABILITIES: &[&str] = &[
    CORE_CAPABILITY_SESSION,
    CORE_CAPABILITY_ATTACH,
    CORE_CAPABILITY_PANE_IO,
    CORE_CAPABILITY_DETACH,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolRevisionRange {
    pub min: u32,
    pub max: u32,
}

impl ProtocolRevisionRange {
    #[must_use]
    pub const fn new(min: u32, max: u32) -> Self {
        Self { min, max }
    }

    #[must_use]
    pub const fn current() -> Self {
        Self {
            min: MIN_SUPPORTED_PROTOCOL_REVISION,
            max: CURRENT_PROTOCOL_REVISION,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolContract {
    pub wire_epoch: u16,
    pub revisions: ProtocolRevisionRange,
    pub capabilities: Vec<String>,
}

impl ProtocolContract {
    #[must_use]
    pub fn current(capabilities: Vec<String>) -> Self {
        Self {
            wire_epoch: CURRENT_WIRE_EPOCH,
            revisions: ProtocolRevisionRange::current(),
            capabilities,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NegotiatedProtocol {
    pub wire_epoch: u16,
    pub revision: u32,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncompatibilityReason {
    WireEpochMismatch {
        client: u16,
        server: u16,
    },
    NoCommonRevision {
        client_min: u32,
        client_max: u32,
        server_min: u32,
        server_max: u32,
    },
    MissingCoreCapabilities {
        missing: Vec<String>,
    },
}

#[must_use]
pub fn default_supported_capabilities() -> Vec<String> {
    vec![
        CORE_CAPABILITY_SESSION.to_string(),
        CORE_CAPABILITY_ATTACH.to_string(),
        CORE_CAPABILITY_PANE_IO.to_string(),
        CORE_CAPABILITY_DETACH.to_string(),
        "feature.contexts".to_string(),
        "feature.attach_snapshot".to_string(),
        "feature.recording.v4".to_string(),
    ]
}

pub fn negotiate_protocol(
    client: &ProtocolContract,
    server: &ProtocolContract,
    core_required: &[&str],
) -> Result<NegotiatedProtocol, IncompatibilityReason> {
    if client.wire_epoch != server.wire_epoch {
        return Err(IncompatibilityReason::WireEpochMismatch {
            client: client.wire_epoch,
            server: server.wire_epoch,
        });
    }

    let overlap_min = client.revisions.min.max(server.revisions.min);
    let overlap_max = client.revisions.max.min(server.revisions.max);
    if overlap_min > overlap_max {
        return Err(IncompatibilityReason::NoCommonRevision {
            client_min: client.revisions.min,
            client_max: client.revisions.max,
            server_min: server.revisions.min,
            server_max: server.revisions.max,
        });
    }

    let server_caps: BTreeSet<&str> = server.capabilities.iter().map(String::as_str).collect();
    let client_caps: BTreeSet<&str> = client.capabilities.iter().map(String::as_str).collect();

    let negotiated_caps: Vec<String> = server_caps
        .intersection(&client_caps)
        .map(|cap| (*cap).to_string())
        .collect();

    let negotiated_set: BTreeSet<&str> = negotiated_caps.iter().map(String::as_str).collect();
    let missing: Vec<String> = core_required
        .iter()
        .copied()
        .filter(|required| !negotiated_set.contains(required))
        .map(std::string::ToString::to_string)
        .collect();
    if !missing.is_empty() {
        return Err(IncompatibilityReason::MissingCoreCapabilities { missing });
    }

    Ok(NegotiatedProtocol {
        wire_epoch: server.wire_epoch,
        revision: overlap_max,
        capabilities: negotiated_caps,
    })
}

/// Protocol version used in IPC envelopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProtocolVersion(pub u16);

impl ProtocolVersion {
    /// The currently supported protocol version.
    #[must_use]
    pub const fn current() -> Self {
        Self(CURRENT_PROTOCOL_VERSION)
    }
}

impl Default for ProtocolVersion {
    fn default() -> Self {
        Self::current()
    }
}

/// Envelope discriminant for payload interpretation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvelopeKind {
    Request,
    Response,
    Event,
}

/// Versioned IPC envelope with request correlation support.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub version: ProtocolVersion,
    pub request_id: u64,
    pub kind: EnvelopeKind,
    pub payload: Vec<u8>,
}

impl Envelope {
    /// Build a new envelope.
    #[must_use]
    pub const fn new(request_id: u64, kind: EnvelopeKind, payload: Vec<u8>) -> Self {
        Self {
            version: ProtocolVersion::current(),
            request_id,
            kind,
            payload,
        }
    }
}

/// Session selector accepted by commands and protocol requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionSelector {
    ById(Uuid),
    ByName(String),
}

/// Generic context selector accepted by context protocol requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContextSelector {
    ById(Uuid),
    ByName(String),
}

/// Pane selector accepted by commands and protocol requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneSelector {
    ById(Uuid),
    ByIndex(u32),
    Active,
}

/// Generic service invocation kind for plugin-dispatched RPC calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvokeServiceKind {
    Query,
    Command,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneSplitDirection {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneFocusDirection {
    Next,
    Prev,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneLayoutNode {
    Leaf {
        pane_id: Uuid,
    },
    Split {
        direction: PaneSplitDirection,
        ratio_percent: u8,
        first: Box<Self>,
        second: Box<Self>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachViewComponent {
    Scene,
    SurfaceContent,
    Layout,
    Status,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachRect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachLayer {
    Status,
    Pane,
    Overlay,
    FloatingPane,
    Tooltip,
    Cursor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachFocusTarget {
    None,
    Pane { pane_id: Uuid },
    Surface { surface_id: Uuid },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachSurfaceKind {
    Pane,
    FloatingPane,
    Modal,
    Overlay,
    Tooltip,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachSurface {
    pub id: Uuid,
    pub kind: AttachSurfaceKind,
    pub layer: AttachLayer,
    pub z: i32,
    pub rect: AttachRect,
    pub opaque: bool,
    pub visible: bool,
    pub accepts_input: bool,
    pub cursor_owner: bool,
    pub pane_id: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachScene {
    pub session_id: Uuid,
    pub focus: AttachFocusTarget,
    pub surfaces: Vec<AttachSurface>,
}

/// Request payload variants for client/server IPC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Request {
    Hello {
        protocol_version: ProtocolVersion,
        client_name: String,
        principal_id: Uuid,
    },
    Ping,
    WhoAmI,
    WhoAmIPrincipal,
    ServerStatus,
    ServerSave,
    ServerRestoreDryRun,
    ServerRestoreApply,
    ServerStop,
    InvokeService {
        capability: String,
        kind: InvokeServiceKind,
        interface_id: String,
        operation: String,
        payload: Vec<u8>,
    },
    NewSession {
        name: Option<String>,
    },
    ListSessions,
    ListClients,
    CreateContext {
        name: Option<String>,
        #[serde(default)]
        attributes: BTreeMap<String, String>,
    },
    ListContexts,
    SelectContext {
        selector: ContextSelector,
    },
    CloseContext {
        selector: ContextSelector,
        force: bool,
    },
    CurrentContext,
    KillSession {
        selector: SessionSelector,
        force_local: bool,
    },
    SplitPane {
        session: Option<SessionSelector>,
        target: Option<PaneSelector>,
        direction: PaneSplitDirection,
        /// Split ratio as a percentage (0-100). Currently passed through the
        /// protocol but not yet used by the server runtime (layout-level feature).
        #[serde(default)]
        ratio_pct: Option<u32>,
    },
    FocusPane {
        session: Option<SessionSelector>,
        target: Option<PaneSelector>,
        direction: Option<PaneFocusDirection>,
    },
    ResizePane {
        session: Option<SessionSelector>,
        target: Option<PaneSelector>,
        delta: i16,
    },
    ClosePane {
        session: Option<SessionSelector>,
        target: Option<PaneSelector>,
    },
    RestartPane {
        session: Option<SessionSelector>,
        target: Option<PaneSelector>,
    },
    ZoomPane {
        session: Option<SessionSelector>,
    },
    ListPanes {
        session: Option<SessionSelector>,
    },
    FollowClient {
        target_client_id: Uuid,
        global: bool,
    },
    Unfollow,
    Attach {
        selector: SessionSelector,
    },
    AttachContext {
        selector: ContextSelector,
    },
    AttachOpen {
        session_id: Uuid,
        attach_token: Uuid,
    },
    AttachInput {
        session_id: Uuid,
        data: Vec<u8>,
    },
    AttachSetViewport {
        session_id: Uuid,
        cols: u16,
        rows: u16,
        #[serde(default)]
        status_top_inset: u16,
        #[serde(default)]
        status_bottom_inset: u16,
        /// Cell width in pixels (0 = unknown). Used for image placement sizing.
        #[serde(default)]
        cell_pixel_width: u16,
        /// Cell height in pixels (0 = unknown). Used for image placement sizing.
        #[serde(default)]
        cell_pixel_height: u16,
    },
    AttachOutput {
        session_id: Uuid,
        max_bytes: usize,
    },
    AttachLayout {
        session_id: Uuid,
    },
    AttachPaneOutputBatch {
        session_id: Uuid,
        pane_ids: Vec<Uuid>,
        max_bytes: usize,
    },
    AttachPaneImages {
        session_id: Uuid,
        pane_ids: Vec<Uuid>,
        /// Per-pane sequence numbers from the last delta received.
        /// Parallel to `pane_ids`.  Use 0 for a full snapshot.
        since_sequences: Vec<u64>,
    },
    AttachSnapshot {
        session_id: Uuid,
        max_bytes_per_pane: usize,
    },
    SubscribeEvents,
    PollEvents {
        max_events: usize,
    },
    /// Enable server-push event delivery on this connection.
    ///
    /// After the server responds with `EventPushEnabled`, it will write
    /// `EnvelopeKind::Event` frames asynchronously. Only streaming-capable
    /// clients (which split the socket into read/write halves and demux
    /// incoming frames) should send this request.
    EnableEventPush,
    RecordingStart {
        #[serde(default)]
        session_id: Option<Uuid>,
        capture_input: bool,
        #[serde(default)]
        profile: Option<RecordingProfile>,
        #[serde(default)]
        event_kinds: Option<Vec<RecordingEventKind>>,
    },
    RecordingStop {
        #[serde(default)]
        recording_id: Option<Uuid>,
    },
    RecordingStatus,
    RecordingList,
    RecordingDelete {
        recording_id: Uuid,
    },
    RecordingWriteCustomEvent {
        #[serde(default)]
        session_id: Option<Uuid>,
        #[serde(default)]
        pane_id: Option<Uuid>,
        source: String,
        name: String,
        /// Pre-serialized JSON payload bytes.
        payload: Vec<u8>,
    },
    RecordingDeleteAll,
    /// Prune completed recordings older than the specified retention period.
    RecordingPrune {
        /// Override retention period in days. If `None`, uses the server config.
        older_than_days: Option<u64>,
    },
    Detach,
    /// Write input bytes directly to a specific pane by ID, bypassing focus routing.
    PaneDirectInput {
        session_id: Uuid,
        pane_id: Uuid,
        data: Vec<u8>,
    },
    HelloV2 {
        contract: ProtocolContract,
        client_name: String,
        principal_id: Uuid,
    },
}

/// Attach grant returned by attach control-plane request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachGrant {
    #[serde(default)]
    pub context_id: Option<Uuid>,
    pub session_id: Uuid,
    pub attach_token: Uuid,
    pub expires_at_epoch_ms: u64,
}

/// Summary returned when listing sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: Uuid,
    pub name: Option<String>,
    pub client_count: usize,
}

/// Summary returned when listing generic runtime contexts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextSummary {
    pub id: Uuid,
    pub name: Option<String>,
    #[serde(default)]
    pub attributes: BTreeMap<String, String>,
}

/// Summary returned when listing panes in the active session runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSummary {
    pub id: Uuid,
    pub index: u32,
    pub name: Option<String>,
    pub focused: bool,
    #[serde(default)]
    pub state: PaneState,
    #[serde(default)]
    pub state_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PaneState {
    #[default]
    Running,
    Exited,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachPaneChunk {
    pub pane_id: Uuid,
    pub data: Vec<u8>,
    /// True when the inner application is inside a DEC mode 2026
    /// synchronized update — the server's PTY reader has seen
    /// `\x1b[?2026h` but not yet `\x1b[?2026l` for this pane.
    #[serde(default)]
    pub sync_update_active: bool,
}

/// Image protocol identifier for IPC transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachImageProtocol {
    Sixel,
    KittyGraphics,
    ITerm2,
}

/// A single image placed within a pane, for IPC transport.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachPaneImage {
    pub id: u64,
    pub protocol: AttachImageProtocol,
    /// Raw protocol bytes (sixel body, kitty payload, iTerm2 data).
    pub raw_data: Vec<u8>,
    pub position_row: u16,
    pub position_col: u16,
    pub cell_rows: u16,
    pub cell_cols: u16,
    pub pixel_width: u32,
    pub pixel_height: u32,
}

/// Incremental image update for a single pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachPaneImageDelta {
    pub pane_id: Uuid,
    pub added: Vec<AttachPaneImage>,
    pub removed: Vec<u64>,
    pub sequence: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AttachMouseProtocolMode {
    #[default]
    None,
    Press,
    PressRelease,
    ButtonMotion,
    AnyMotion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AttachMouseProtocolEncoding {
    #[default]
    Default,
    Utf8,
    Sgr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AttachMouseProtocolState {
    #[serde(default)]
    pub mode: AttachMouseProtocolMode,
    #[serde(default)]
    pub encoding: AttachMouseProtocolEncoding,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachPaneMouseProtocol {
    pub pane_id: Uuid,
    #[serde(default)]
    pub protocol: AttachMouseProtocolState,
}

/// Summary returned when listing connected clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientSummary {
    pub id: Uuid,
    #[serde(default)]
    pub selected_context_id: Option<Uuid>,
    pub selected_session_id: Option<Uuid>,
    pub following_client_id: Option<Uuid>,
    pub following_global: bool,
}

/// Snapshot persistence status returned by server-status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerSnapshotStatus {
    pub enabled: bool,
    pub path: Option<String>,
    pub snapshot_exists: bool,
    pub last_write_epoch_ms: Option<u64>,
    pub last_restore_epoch_ms: Option<u64>,
    pub last_restore_error: Option<String>,
}

/// Recording summary returned by recording APIs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingSummary {
    pub id: Uuid,
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
pub const RECORDING_FORMAT_VERSION: u32 = 4;

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

/// Recording profile used to choose event verbosity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingProfile {
    Full,
    Functional,
    Visual,
}

/// Recording runtime status details.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingStatus {
    pub active: Option<RecordingSummary>,
    pub queue_len: usize,
}

/// Event kind emitted into a recording timeline.
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
pub enum RecordingPayload {
    Bytes {
        data: Vec<u8>,
    },
    ServerEvent {
        event: Event,
    },
    RequestStart {
        request_id: u64,
        request_kind: String,
        exclusive: bool,
        /// Full request, binary-encoded.
        request_data: Vec<u8>,
    },
    RequestDone {
        request_id: u64,
        request_kind: String,
        response_kind: String,
        elapsed_ms: u64,
        /// Full request, binary-encoded.
        request_data: Vec<u8>,
        /// Full response payload, binary-encoded.
        response_data: Vec<u8>,
    },
    RequestError {
        request_id: u64,
        request_kind: String,
        error_code: ErrorCode,
        message: String,
        elapsed_ms: u64,
    },
    Custom {
        source: String,
        name: String,
        /// Pre-serialized JSON payload bytes.
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
        data: Vec<u8>,
    },
}

/// Timeline event envelope persisted in recordings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingEventEnvelope {
    pub seq: u64,
    pub mono_ns: u64,
    pub wall_epoch_ms: u64,
    pub session_id: Option<Uuid>,
    pub pane_id: Option<Uuid>,
    pub client_id: Option<Uuid>,
    pub kind: RecordingEventKind,
    pub payload: RecordingPayload,
}

/// Successful response payload variants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponsePayload {
    Pong,
    ClientIdentity {
        id: Uuid,
    },
    PrincipalIdentity {
        principal_id: Uuid,
        server_control_principal_id: Uuid,
        force_local_permitted: bool,
    },
    ServerStatus {
        running: bool,
        snapshot: ServerSnapshotStatus,
        principal_id: Uuid,
        server_control_principal_id: Uuid,
    },
    ServerSnapshotSaved {
        path: Option<String>,
    },
    ServerSnapshotRestoreDryRun {
        ok: bool,
        message: String,
    },
    ServerSnapshotRestored {
        sessions: usize,
        follows: usize,
        selected_sessions: usize,
    },
    SessionCreated {
        id: Uuid,
        name: Option<String>,
    },
    SessionList {
        sessions: Vec<SessionSummary>,
    },
    ClientList {
        clients: Vec<ClientSummary>,
    },
    ContextCreated {
        context: ContextSummary,
    },
    ContextList {
        contexts: Vec<ContextSummary>,
    },
    ContextSelected {
        context: ContextSummary,
    },
    ContextClosed {
        id: Uuid,
    },
    CurrentContext {
        context: Option<ContextSummary>,
    },
    SessionKilled {
        id: Uuid,
    },
    PaneSplit {
        id: Uuid,
        session_id: Uuid,
    },
    PaneFocused {
        id: Uuid,
        session_id: Uuid,
    },
    PaneResized {
        session_id: Uuid,
    },
    PaneClosed {
        id: Uuid,
        session_id: Uuid,
        session_closed: bool,
    },
    PaneRestarted {
        id: Uuid,
        session_id: Uuid,
    },
    PaneZoomed {
        session_id: Uuid,
        pane_id: Uuid,
        zoomed: bool,
    },
    PaneList {
        panes: Vec<PaneSummary>,
    },
    FollowStarted {
        follower_client_id: Uuid,
        leader_client_id: Uuid,
        global: bool,
    },
    FollowStopped {
        follower_client_id: Uuid,
    },
    Attached {
        grant: AttachGrant,
    },
    AttachReady {
        #[serde(default)]
        context_id: Option<Uuid>,
        session_id: Uuid,
        can_write: bool,
    },
    AttachInputAccepted {
        bytes: usize,
    },
    AttachViewportSet {
        #[serde(default)]
        context_id: Option<Uuid>,
        session_id: Uuid,
        cols: u16,
        rows: u16,
        #[serde(default)]
        status_top_inset: u16,
        #[serde(default)]
        status_bottom_inset: u16,
    },
    AttachOutput {
        data: Vec<u8>,
    },
    AttachLayout {
        #[serde(default)]
        context_id: Option<Uuid>,
        session_id: Uuid,
        focused_pane_id: Uuid,
        panes: Vec<PaneSummary>,
        layout_root: PaneLayoutNode,
        scene: AttachScene,
        #[serde(default)]
        zoomed: bool,
    },
    AttachPaneOutputBatch {
        chunks: Vec<AttachPaneChunk>,
        /// True when at least one requested pane's PTY reader has flagged
        /// new output that was not included in this batch.  The client
        /// should continue draining instead of proceeding to render.
        #[serde(default)]
        output_still_pending: bool,
    },
    AttachPaneImages {
        deltas: Vec<AttachPaneImageDelta>,
    },
    AttachSnapshot {
        #[serde(default)]
        context_id: Option<Uuid>,
        session_id: Uuid,
        focused_pane_id: Uuid,
        panes: Vec<PaneSummary>,
        layout_root: PaneLayoutNode,
        scene: AttachScene,
        chunks: Vec<AttachPaneChunk>,
        #[serde(default)]
        pane_mouse_protocols: Vec<AttachPaneMouseProtocol>,
        #[serde(default)]
        zoomed: bool,
    },
    EventsSubscribed,
    EventBatch {
        events: Vec<Event>,
    },
    /// Acknowledgement that server-push event delivery has been enabled.
    EventPushEnabled,
    RecordingStarted {
        recording: RecordingSummary,
    },
    RecordingStopped {
        recording_id: Uuid,
    },
    RecordingStatus {
        status: RecordingStatus,
    },
    RecordingList {
        recordings: Vec<RecordingSummary>,
    },
    RecordingDeleted {
        recording_id: Uuid,
    },
    RecordingCustomEventWritten {
        accepted: bool,
    },
    RecordingDeleteAll {
        deleted_count: usize,
    },
    RecordingPruned {
        deleted_count: usize,
    },
    Detached,
    PaneDirectInputAccepted {
        bytes: usize,
        pane_id: Uuid,
    },
    ServerStopping,
    ServiceInvoked {
        payload: Vec<u8>,
    },
    HelloNegotiated {
        negotiated: NegotiatedProtocol,
    },
    HelloIncompatible {
        reason: IncompatibilityReason,
    },
}

/// Canonical error codes returned over IPC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    NotFound,
    AlreadyExists,
    InvalidRequest,
    VersionMismatch,
    Timeout,
    Internal,
}

/// Error details returned over IPC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub code: ErrorCode,
    pub message: String,
}

/// Top-level response message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Response {
    Ok(ResponsePayload),
    Err(ErrorResponse),
}

/// Event payload variants emitted by the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    ServerStarted,
    ServerStopping,
    SessionCreated {
        id: Uuid,
        name: Option<String>,
    },
    SessionRemoved {
        id: Uuid,
    },
    ClientAttached {
        id: Uuid,
    },
    ClientDetached {
        id: Uuid,
    },
    FollowStarted {
        follower_client_id: Uuid,
        leader_client_id: Uuid,
        global: bool,
    },
    FollowStopped {
        follower_client_id: Uuid,
    },
    FollowTargetGone {
        follower_client_id: Uuid,
        former_leader_client_id: Uuid,
    },
    FollowTargetChanged {
        follower_client_id: Uuid,
        leader_client_id: Uuid,
        #[serde(default)]
        context_id: Option<Uuid>,
        session_id: Uuid,
    },
    AttachViewChanged {
        #[serde(default)]
        context_id: Option<Uuid>,
        session_id: Uuid,
        revision: u64,
        components: Vec<AttachViewComponent>,
    },
    /// Notification that new pane output is available for reading.
    /// Emitted by the server when PTY output arrives; streaming clients use
    /// this to fetch output on demand instead of polling.
    PaneOutputAvailable {
        session_id: Uuid,
        pane_id: Uuid,
    },
    /// Notification that pane image state has changed (new images placed,
    /// images removed, or positions shifted).  Streaming clients use this
    /// to fetch image deltas on demand instead of polling every frame.
    PaneImageAvailable {
        session_id: Uuid,
        pane_id: Uuid,
    },
    PaneExited {
        session_id: Uuid,
        pane_id: Uuid,
        #[serde(default)]
        reason: Option<String>,
    },
    PaneRestarted {
        session_id: Uuid,
        pane_id: Uuid,
    },
    /// A server-side recording has started.  Attached clients use this to
    /// begin writing their own display tracks into the recording directory.
    RecordingStarted {
        recording_id: Uuid,
        path: String,
    },
    /// A server-side recording has stopped.  Attached clients use this to
    /// flush and close any in-progress display track.
    RecordingStopped {
        recording_id: Uuid,
    },
}

/// Serialize any protocol message using the bmux binary codec.
///
/// # Errors
///
/// Returns an error when serialization fails.
pub fn encode<T>(message: &T) -> Result<Vec<u8>, bmux_codec::Error>
where
    T: Serialize,
{
    bmux_codec::to_vec(message)
}

/// Deserialize any protocol message using the bmux binary codec.
///
/// # Errors
///
/// Returns an error when deserialization fails.
pub fn decode<T>(bytes: &[u8]) -> Result<T, bmux_codec::Error>
where
    T: DeserializeOwned,
{
    bmux_codec::from_bytes(bytes)
}

// ── Shared display track types for recording files ───────────────────────────

/// Display track event — shared type used by both the attach runtime's
/// `DisplayCaptureWriter` and the playbook engine's `PlaybookDisplayTrackWriter`.
///
/// The `terminal_profile` field stores pre-serialized bytes (codec-encoded
/// `DetectedTerminalProfile`) to avoid cross-crate type dependencies. Use `None`
/// when no terminal profile is available (e.g., headless playbook execution).
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
        terminal_profile: Option<Vec<u8>>,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    FrameBytes {
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

/// Display track envelope — wraps an event with a monotonic timestamp.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayTrackEnvelope {
    pub mono_ns: u64,
    pub event: DisplayTrackEvent,
}

// ── Binary frame utilities for recording files ───────────────────────────────

/// Write a length-prefixed binary frame to a writer.
///
/// Format: `[u32 little-endian length][codec bytes]`
///
/// Returns an error if the serialized payload exceeds `u32::MAX` bytes.
pub fn write_frame<W: std::io::Write, T: Serialize>(
    writer: &mut W,
    value: &T,
) -> Result<(), Box<dyn std::error::Error>> {
    let bytes = bmux_codec::to_vec(value).map_err(|e| format!("codec serialize failed: {e}"))?;
    let len = u32::try_from(bytes.len())
        .map_err(|_| format!("frame too large: {} bytes exceeds u32::MAX", bytes.len()))?;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(&bytes)?;
    Ok(())
}

/// Result of reading binary frames from a buffer.
pub struct ReadFramesResult<T> {
    /// Successfully deserialized frames.
    pub frames: Vec<T>,
    /// Number of trailing bytes that could not be parsed as a complete frame.
    /// Zero means a clean EOF with no leftover data.
    pub bytes_remaining: usize,
}

/// Read all length-prefixed binary frames from a byte buffer.
///
/// Returns all successfully-parsed frames plus the count of any trailing bytes
/// that could not form a complete frame (indicating a truncated recording).
pub fn read_frames<T: DeserializeOwned>(
    data: &[u8],
) -> Result<ReadFramesResult<T>, Box<dyn std::error::Error>> {
    let mut results = Vec::new();
    let mut offset = 0;
    while offset + 4 <= data.len() {
        let len = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;
        if offset + len > data.len() {
            // Truncated frame — rewind to include the length prefix in remaining bytes.
            offset -= 4;
            break;
        }
        let value: T = bmux_codec::from_bytes(&data[offset..offset + len])
            .map_err(|e| format!("codec deserialize failed at offset {}: {e}", offset))?;
        results.push(value);
        offset += len;
    }
    Ok(ReadFramesResult {
        frames: results,
        bytes_remaining: data.len() - offset,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn serializes_request_roundtrip() {
        let request = Request::KillSession {
            selector: SessionSelector::ByName("dev-shell".to_string()),
            force_local: false,
        };
        let bytes = encode(&request).expect("request should encode");
        let decoded: Request = decode(&bytes).expect("request should decode");
        assert_eq!(decoded, request);
    }

    #[test]
    fn serializes_response_roundtrip() {
        let response = Response::Ok(ResponsePayload::SessionList {
            sessions: vec![SessionSummary {
                id: Uuid::new_v4(),
                name: Some("work".to_string()),
                client_count: 1,
            }],
        });
        let bytes = encode(&response).expect("response should encode");
        let decoded: Response = decode(&bytes).expect("response should decode");
        assert_eq!(decoded, response);
    }

    #[test]
    fn serializes_attach_scene_response_roundtrip() {
        let pane_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let response = Response::Ok(ResponsePayload::AttachLayout {
            context_id: None,
            session_id,
            focused_pane_id: pane_id,
            panes: vec![],
            layout_root: super::PaneLayoutNode::Leaf { pane_id },
            scene: AttachScene {
                session_id,
                focus: AttachFocusTarget::Pane { pane_id },
                surfaces: vec![AttachSurface {
                    id: pane_id,
                    kind: AttachSurfaceKind::Pane,
                    layer: AttachLayer::Pane,
                    z: 0,
                    rect: AttachRect {
                        x: 0,
                        y: 1,
                        w: 80,
                        h: 24,
                    },
                    opaque: true,
                    visible: true,
                    accepts_input: true,
                    cursor_owner: true,
                    pane_id: Some(pane_id),
                }],
            },
            zoomed: false,
        });
        let bytes = encode(&response).expect("response should encode");
        let decoded: Response = decode(&bytes).expect("response should decode");
        assert_eq!(decoded, response);
    }

    #[test]
    fn serializes_event_roundtrip() {
        let event = Event::SessionCreated {
            id: Uuid::new_v4(),
            name: Some("ops".to_string()),
        };
        let bytes = encode(&event).expect("event should encode");
        let decoded: Event = decode(&bytes).expect("event should decode");
        assert_eq!(decoded, event);
    }

    #[test]
    fn serializes_attach_view_changed_roundtrip() {
        let event = Event::AttachViewChanged {
            context_id: None,
            session_id: Uuid::new_v4(),
            revision: 7,
            components: vec![AttachViewComponent::Layout, AttachViewComponent::Status],
        };
        let bytes = encode(&event).expect("event should encode");
        let decoded: Event = decode(&bytes).expect("event should decode");
        assert_eq!(decoded, event);
    }

    #[test]
    fn serializes_envelope_roundtrip() {
        let payload = encode(&Request::Ping).expect("payload should encode");
        let envelope = Envelope {
            version: ProtocolVersion::current(),
            request_id: 7,
            kind: EnvelopeKind::Request,
            payload,
        };
        let bytes = encode(&envelope).expect("envelope should encode");
        let decoded: Envelope = decode(&bytes).expect("envelope should decode");
        assert_eq!(decoded, envelope);
    }

    #[test]
    fn serializes_session_selector_by_id_roundtrip() {
        let selector = SessionSelector::ById(Uuid::new_v4());
        let bytes = encode(&selector).expect("selector should encode");
        let decoded: SessionSelector = decode(&bytes).expect("selector should decode");
        assert_eq!(decoded, selector);
    }

    #[test]
    fn protocol_version_defaults_to_current() {
        assert_eq!(ProtocolVersion::default(), ProtocolVersion::current());
    }

    #[test]
    fn error_code_serializes_roundtrip() {
        let code = ErrorCode::VersionMismatch;
        let bytes = encode(&code).expect("error code should encode");
        let decoded: ErrorCode = decode(&bytes).expect("error code should decode");
        assert_eq!(decoded, code);
    }

    #[test]
    fn negotiate_protocol_selects_highest_common_revision() {
        let client = ProtocolContract {
            wire_epoch: CURRENT_WIRE_EPOCH,
            revisions: ProtocolRevisionRange::new(1, 4),
            capabilities: vec![
                CORE_CAPABILITY_SESSION.to_string(),
                CORE_CAPABILITY_ATTACH.to_string(),
                CORE_CAPABILITY_PANE_IO.to_string(),
                CORE_CAPABILITY_DETACH.to_string(),
                "feature.recording.v4".to_string(),
            ],
        };
        let server = ProtocolContract {
            wire_epoch: CURRENT_WIRE_EPOCH,
            revisions: ProtocolRevisionRange::new(2, 3),
            capabilities: default_supported_capabilities(),
        };

        let negotiated = negotiate_protocol(&client, &server, CORE_PROTOCOL_CAPABILITIES)
            .expect("negotiation should succeed");
        assert_eq!(negotiated.revision, 3);
        assert!(
            negotiated
                .capabilities
                .contains(&"feature.recording.v4".to_string())
        );
    }

    #[test]
    fn negotiate_protocol_rejects_wire_epoch_mismatch() {
        let client = ProtocolContract {
            wire_epoch: 10,
            revisions: ProtocolRevisionRange::new(1, 1),
            capabilities: default_supported_capabilities(),
        };
        let server = ProtocolContract {
            wire_epoch: 11,
            revisions: ProtocolRevisionRange::new(1, 1),
            capabilities: default_supported_capabilities(),
        };

        let error = negotiate_protocol(&client, &server, CORE_PROTOCOL_CAPABILITIES)
            .expect_err("wire mismatch should fail");
        assert!(matches!(
            error,
            IncompatibilityReason::WireEpochMismatch {
                client: 10,
                server: 11,
            }
        ));
    }

    #[test]
    fn negotiate_protocol_rejects_missing_core_capability() {
        let client = ProtocolContract {
            wire_epoch: CURRENT_WIRE_EPOCH,
            revisions: ProtocolRevisionRange::new(1, 1),
            capabilities: vec![CORE_CAPABILITY_SESSION.to_string()],
        };
        let server = ProtocolContract {
            wire_epoch: CURRENT_WIRE_EPOCH,
            revisions: ProtocolRevisionRange::new(1, 1),
            capabilities: vec![CORE_CAPABILITY_SESSION.to_string()],
        };

        let error = negotiate_protocol(&client, &server, CORE_PROTOCOL_CAPABILITIES)
            .expect_err("missing core capabilities should fail");
        assert!(matches!(
            error,
            IncompatibilityReason::MissingCoreCapabilities { missing }
                if missing.contains(&CORE_CAPABILITY_ATTACH.to_string())
        ));
    }

    #[test]
    fn endpoint_helpers_report_correct_transport() {
        let unix_endpoint = IpcEndpoint::unix_socket("/tmp/bmux.sock");
        assert_eq!(
            unix_endpoint.as_unix_socket(),
            Some(Path::new("/tmp/bmux.sock"))
        );
        assert_eq!(unix_endpoint.as_windows_named_pipe(), None);

        let pipe_endpoint = IpcEndpoint::windows_named_pipe(r"\\.\pipe\bmux-test");
        assert_eq!(pipe_endpoint.as_unix_socket(), None);
        assert_eq!(
            pipe_endpoint.as_windows_named_pipe(),
            Some(r"\\.\pipe\bmux-test")
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_endpoint_exposes_socket_path() {
        let endpoint = IpcEndpoint::unix_socket("/tmp/bmux.sock");
        assert_eq!(endpoint.as_unix_socket(), Some(Path::new("/tmp/bmux.sock")));
    }

    #[cfg(windows)]
    #[test]
    fn windows_endpoint_exposes_pipe_name() {
        let endpoint = IpcEndpoint::windows_named_pipe(r"\\.\pipe\bmux-test");
        assert_eq!(
            endpoint.as_windows_named_pipe(),
            Some(r"\\.\pipe\bmux-test")
        );
    }

    // ── Helper: assert encode/decode roundtrip ───────────────────────────────

    fn assert_roundtrip<T>(value: &T)
    where
        T: std::fmt::Debug + PartialEq + serde::Serialize + serde::de::DeserializeOwned,
    {
        let bytes = encode(value).unwrap_or_else(|e| panic!("encode failed: {e}"));
        let decoded: T = decode(&bytes).unwrap_or_else(|e| panic!("decode failed: {e}"));
        assert_eq!(&decoded, value);
    }

    // ── Level 1A: Exhaustive Request variant round-trips ─────────────────────

    #[test]
    fn request_all_variants_roundtrip() {
        let id = Uuid::from_u128(1);
        let id2 = Uuid::from_u128(2);

        let variants: Vec<Request> = vec![
            Request::Hello {
                protocol_version: ProtocolVersion::current(),
                client_name: "test-client".into(),
                principal_id: id,
            },
            Request::HelloV2 {
                contract: ProtocolContract::current(default_supported_capabilities()),
                client_name: "test-client-v2".into(),
                principal_id: id,
            },
            Request::Ping,
            Request::WhoAmI,
            Request::WhoAmIPrincipal,
            Request::ServerStatus,
            Request::ServerSave,
            Request::ServerRestoreDryRun,
            Request::ServerRestoreApply,
            Request::ServerStop,
            Request::InvokeService {
                capability: "bmux.sessions.read".into(),
                kind: InvokeServiceKind::Query,
                interface_id: "session-query/v1".into(),
                operation: "list".into(),
                payload: vec![1, 2, 3],
            },
            Request::NewSession {
                name: Some("dev".into()),
            },
            Request::NewSession { name: None },
            Request::ListSessions,
            Request::ListClients,
            Request::CreateContext {
                name: Some("ctx".into()),
                attributes: {
                    let mut m = BTreeMap::new();
                    m.insert("key".into(), "val".into());
                    m
                },
            },
            Request::CreateContext {
                name: None,
                attributes: BTreeMap::new(),
            },
            Request::ListContexts,
            Request::SelectContext {
                selector: ContextSelector::ById(id),
            },
            Request::SelectContext {
                selector: ContextSelector::ByName("ctx-name".into()),
            },
            Request::CloseContext {
                selector: ContextSelector::ById(id),
                force: true,
            },
            Request::CurrentContext,
            Request::KillSession {
                selector: SessionSelector::ById(id),
                force_local: true,
            },
            Request::KillSession {
                selector: SessionSelector::ByName("session".into()),
                force_local: false,
            },
            Request::SplitPane {
                session: Some(SessionSelector::ById(id)),
                target: Some(PaneSelector::ById(id2)),
                direction: PaneSplitDirection::Vertical,
                ratio_pct: None,
            },
            Request::SplitPane {
                session: None,
                target: Some(PaneSelector::ByIndex(0)),
                direction: PaneSplitDirection::Horizontal,
                ratio_pct: None,
            },
            Request::SplitPane {
                session: None,
                target: Some(PaneSelector::Active),
                direction: PaneSplitDirection::Vertical,
                ratio_pct: None,
            },
            Request::FocusPane {
                session: None,
                target: None,
                direction: Some(PaneFocusDirection::Next),
            },
            Request::FocusPane {
                session: None,
                target: None,
                direction: Some(PaneFocusDirection::Prev),
            },
            Request::FocusPane {
                session: None,
                target: None,
                direction: None,
            },
            Request::ResizePane {
                session: None,
                target: None,
                delta: -5,
            },
            Request::ResizePane {
                session: Some(SessionSelector::ByName("s".into())),
                target: Some(PaneSelector::ByIndex(3)),
                delta: 10,
            },
            Request::ClosePane {
                session: None,
                target: None,
            },
            Request::RestartPane {
                session: None,
                target: Some(PaneSelector::Active),
            },
            Request::ZoomPane { session: None },
            Request::ZoomPane {
                session: Some(SessionSelector::ByName("s".into())),
            },
            Request::ListPanes {
                session: Some(SessionSelector::ById(id)),
            },
            Request::ListPanes { session: None },
            Request::FollowClient {
                target_client_id: id,
                global: true,
            },
            Request::Unfollow,
            Request::Attach {
                selector: SessionSelector::ByName("main".into()),
            },
            Request::AttachContext {
                selector: ContextSelector::ByName("default".into()),
            },
            Request::AttachOpen {
                session_id: id,
                attach_token: id2,
            },
            Request::AttachInput {
                session_id: id,
                data: vec![27, 91, 65], // ESC [ A
            },
            Request::AttachSetViewport {
                session_id: id,
                cols: 120,
                rows: 40,
                status_top_inset: 1,
                status_bottom_inset: 0,
                cell_pixel_width: 8,
                cell_pixel_height: 16,
            },
            Request::AttachOutput {
                session_id: id,
                max_bytes: 65536,
            },
            Request::AttachLayout { session_id: id },
            Request::AttachPaneOutputBatch {
                session_id: id,
                pane_ids: vec![id, id2],
                max_bytes: 4096,
            },
            Request::AttachSnapshot {
                session_id: id,
                max_bytes_per_pane: 8192,
            },
            Request::SubscribeEvents,
            Request::PollEvents { max_events: 100 },
            Request::RecordingStart {
                session_id: Some(id),
                capture_input: true,
                profile: Some(RecordingProfile::Visual),
                event_kinds: Some(vec![
                    RecordingEventKind::PaneOutputRaw,
                    RecordingEventKind::Custom,
                ]),
            },
            Request::RecordingStart {
                session_id: None,
                capture_input: false,
                profile: None,
                event_kinds: None,
            },
            Request::RecordingStop {
                recording_id: Some(id),
            },
            Request::RecordingStop { recording_id: None },
            Request::RecordingStatus,
            Request::RecordingList,
            Request::RecordingDelete { recording_id: id },
            Request::RecordingWriteCustomEvent {
                session_id: Some(id),
                pane_id: Some(id2),
                source: "test-plugin".into(),
                name: "my-event".into(),
                payload: b"{\"key\":\"value\"}".to_vec(),
            },
            Request::RecordingWriteCustomEvent {
                session_id: None,
                pane_id: None,
                source: "s".into(),
                name: "n".into(),
                payload: vec![],
            },
            Request::RecordingDeleteAll,
            Request::Detach,
            Request::PaneDirectInput {
                session_id: id,
                pane_id: id2,
                data: vec![104, 101, 108, 108, 111],
            },
        ];

        for (i, variant) in variants.iter().enumerate() {
            let bytes = encode(variant)
                .unwrap_or_else(|e| panic!("Request variant {i} encode failed: {e}"));
            let decoded: Request =
                decode(&bytes).unwrap_or_else(|e| panic!("Request variant {i} decode failed: {e}"));
            assert_eq!(&decoded, variant, "Request variant {i} roundtrip mismatch");
        }
    }

    // ── Level 1B: Exhaustive ResponsePayload variant round-trips ─────────────

    fn sample_recording_summary() -> RecordingSummary {
        RecordingSummary {
            id: Uuid::from_u128(100),
            format_version: RECORDING_FORMAT_VERSION,
            session_id: Some(Uuid::from_u128(1)),
            capture_input: true,
            profile: RecordingProfile::Full,
            event_kinds: vec![
                RecordingEventKind::PaneInputRaw,
                RecordingEventKind::PaneOutputRaw,
                RecordingEventKind::ServerEvent,
            ],
            started_epoch_ms: 1_700_000_000_000,
            ended_epoch_ms: Some(1_700_000_060_000),
            event_count: 42,
            payload_bytes: 123_456,
            path: "/tmp/recordings/test.bmux".into(),
            segments: vec!["events_0.bin".to_string()],
            total_segment_bytes: 123_456,
        }
    }

    fn sample_attach_scene() -> (AttachScene, Uuid) {
        let pane_id = Uuid::from_u128(10);
        let session_id = Uuid::from_u128(1);
        let scene = AttachScene {
            session_id,
            focus: AttachFocusTarget::Pane { pane_id },
            surfaces: vec![
                AttachSurface {
                    id: pane_id,
                    kind: AttachSurfaceKind::Pane,
                    layer: AttachLayer::Pane,
                    z: 0,
                    rect: AttachRect {
                        x: 0,
                        y: 1,
                        w: 80,
                        h: 24,
                    },
                    opaque: true,
                    visible: true,
                    accepts_input: true,
                    cursor_owner: true,
                    pane_id: Some(pane_id),
                },
                AttachSurface {
                    id: Uuid::from_u128(20),
                    kind: AttachSurfaceKind::FloatingPane,
                    layer: AttachLayer::FloatingPane,
                    z: 10,
                    rect: AttachRect {
                        x: 5,
                        y: 5,
                        w: 40,
                        h: 10,
                    },
                    opaque: false,
                    visible: true,
                    accepts_input: true,
                    cursor_owner: false,
                    pane_id: None,
                },
            ],
        };
        (scene, pane_id)
    }

    fn sample_layout_tree() -> PaneLayoutNode {
        PaneLayoutNode::Split {
            direction: PaneSplitDirection::Horizontal,
            ratio_percent: 50,
            first: Box::new(PaneLayoutNode::Leaf {
                pane_id: Uuid::from_u128(10),
            }),
            second: Box::new(PaneLayoutNode::Split {
                direction: PaneSplitDirection::Vertical,
                ratio_percent: 60,
                first: Box::new(PaneLayoutNode::Leaf {
                    pane_id: Uuid::from_u128(11),
                }),
                second: Box::new(PaneLayoutNode::Leaf {
                    pane_id: Uuid::from_u128(12),
                }),
            }),
        }
    }

    #[test]
    fn response_payload_all_variants_roundtrip() {
        let id = Uuid::from_u128(1);
        let id2 = Uuid::from_u128(2);
        let (scene, pane_id) = sample_attach_scene();
        let layout = sample_layout_tree();

        let variants: Vec<ResponsePayload> = vec![
            ResponsePayload::Pong,
            ResponsePayload::ClientIdentity { id },
            ResponsePayload::PrincipalIdentity {
                principal_id: id,
                server_control_principal_id: id2,
                force_local_permitted: true,
            },
            ResponsePayload::ServerStatus {
                running: true,
                snapshot: ServerSnapshotStatus {
                    enabled: true,
                    path: Some("/tmp/snap".into()),
                    snapshot_exists: true,
                    last_write_epoch_ms: Some(1_700_000_000_000),
                    last_restore_epoch_ms: None,
                    last_restore_error: None,
                },
                principal_id: id,
                server_control_principal_id: id2,
            },
            ResponsePayload::ServerSnapshotSaved {
                path: Some("/tmp/snap".into()),
            },
            ResponsePayload::ServerSnapshotSaved { path: None },
            ResponsePayload::ServerSnapshotRestoreDryRun {
                ok: true,
                message: "all good".into(),
            },
            ResponsePayload::ServerSnapshotRestored {
                sessions: 3,
                follows: 1,
                selected_sessions: 2,
            },
            ResponsePayload::SessionCreated {
                id,
                name: Some("dev".into()),
            },
            ResponsePayload::SessionList {
                sessions: vec![
                    SessionSummary {
                        id,
                        name: Some("s1".into()),
                        client_count: 2,
                    },
                    SessionSummary {
                        id: id2,
                        name: None,
                        client_count: 0,
                    },
                ],
            },
            ResponsePayload::ClientList {
                clients: vec![ClientSummary {
                    id,
                    selected_context_id: Some(id2),
                    selected_session_id: Some(id),
                    following_client_id: None,
                    following_global: false,
                }],
            },
            ResponsePayload::ContextCreated {
                context: ContextSummary {
                    id,
                    name: Some("default".into()),
                    attributes: {
                        let mut m = BTreeMap::new();
                        m.insert("key".into(), "val".into());
                        m
                    },
                },
            },
            ResponsePayload::ContextList {
                contexts: vec![ContextSummary {
                    id,
                    name: None,
                    attributes: BTreeMap::new(),
                }],
            },
            ResponsePayload::ContextSelected {
                context: ContextSummary {
                    id,
                    name: None,
                    attributes: BTreeMap::new(),
                },
            },
            ResponsePayload::ContextClosed { id },
            ResponsePayload::CurrentContext {
                context: Some(ContextSummary {
                    id,
                    name: None,
                    attributes: BTreeMap::new(),
                }),
            },
            ResponsePayload::CurrentContext { context: None },
            ResponsePayload::SessionKilled { id },
            ResponsePayload::PaneSplit {
                id: pane_id,
                session_id: id,
            },
            ResponsePayload::PaneFocused {
                id: pane_id,
                session_id: id,
            },
            ResponsePayload::PaneResized { session_id: id },
            ResponsePayload::PaneClosed {
                id: pane_id,
                session_id: id,
                session_closed: false,
            },
            ResponsePayload::PaneRestarted {
                id: pane_id,
                session_id: id,
            },
            ResponsePayload::PaneZoomed {
                session_id: id,
                pane_id,
                zoomed: true,
            },
            ResponsePayload::PaneList {
                panes: vec![
                    PaneSummary {
                        id: pane_id,
                        index: 0,
                        name: Some("shell".into()),
                        focused: true,
                        state: PaneState::Running,
                        state_reason: None,
                    },
                    PaneSummary {
                        id: id2,
                        index: 1,
                        name: None,
                        focused: false,
                        state: PaneState::Exited,
                        state_reason: Some("process exited".into()),
                    },
                ],
            },
            ResponsePayload::FollowStarted {
                follower_client_id: id,
                leader_client_id: id2,
                global: true,
            },
            ResponsePayload::FollowStopped {
                follower_client_id: id,
            },
            ResponsePayload::Attached {
                grant: AttachGrant {
                    context_id: Some(id2),
                    session_id: id,
                    attach_token: Uuid::from_u128(99),
                    expires_at_epoch_ms: 1_700_000_060_000,
                },
            },
            ResponsePayload::AttachReady {
                context_id: Some(id2),
                session_id: id,
                can_write: true,
            },
            ResponsePayload::AttachReady {
                context_id: None,
                session_id: id,
                can_write: false,
            },
            ResponsePayload::AttachInputAccepted { bytes: 256 },
            ResponsePayload::AttachViewportSet {
                context_id: None,
                session_id: id,
                cols: 120,
                rows: 40,
                status_top_inset: 1,
                status_bottom_inset: 0,
            },
            ResponsePayload::AttachOutput {
                data: vec![27, 91, 72, 27, 91, 50, 74], // ESC[H ESC[2J
            },
            ResponsePayload::AttachLayout {
                context_id: Some(id2),
                session_id: id,
                focused_pane_id: pane_id,
                panes: vec![PaneSummary {
                    id: pane_id,
                    index: 0,
                    name: None,
                    focused: true,
                    state: PaneState::Running,
                    state_reason: None,
                }],
                layout_root: layout.clone(),
                scene: scene.clone(),
                zoomed: false,
            },
            ResponsePayload::AttachPaneOutputBatch {
                chunks: vec![
                    AttachPaneChunk {
                        pane_id,
                        data: vec![65, 66, 67],
                        sync_update_active: false,
                    },
                    AttachPaneChunk {
                        pane_id: id2,
                        data: vec![],
                        sync_update_active: false,
                    },
                ],
                output_still_pending: false,
            },
            ResponsePayload::AttachSnapshot {
                context_id: None,
                session_id: id,
                focused_pane_id: pane_id,
                panes: vec![PaneSummary {
                    id: pane_id,
                    index: 0,
                    name: None,
                    focused: true,
                    state: PaneState::Running,
                    state_reason: None,
                }],
                layout_root: layout,
                scene,
                chunks: vec![AttachPaneChunk {
                    pane_id,
                    data: vec![0; 100],
                    sync_update_active: false,
                }],
                pane_mouse_protocols: vec![AttachPaneMouseProtocol {
                    pane_id,
                    protocol: AttachMouseProtocolState {
                        mode: AttachMouseProtocolMode::AnyMotion,
                        encoding: AttachMouseProtocolEncoding::Sgr,
                    },
                }],
                zoomed: false,
            },
            ResponsePayload::EventsSubscribed,
            ResponsePayload::EventBatch {
                events: vec![
                    Event::ServerStarted,
                    Event::SessionCreated {
                        id,
                        name: Some("test".into()),
                    },
                ],
            },
            ResponsePayload::RecordingStarted {
                recording: sample_recording_summary(),
            },
            ResponsePayload::RecordingStopped { recording_id: id },
            ResponsePayload::RecordingStatus {
                status: RecordingStatus {
                    active: Some(sample_recording_summary()),
                    queue_len: 5,
                },
            },
            ResponsePayload::RecordingStatus {
                status: RecordingStatus {
                    active: None,
                    queue_len: 0,
                },
            },
            ResponsePayload::RecordingList {
                recordings: vec![sample_recording_summary()],
            },
            ResponsePayload::RecordingDeleted { recording_id: id },
            ResponsePayload::RecordingCustomEventWritten { accepted: true },
            ResponsePayload::RecordingDeleteAll { deleted_count: 7 },
            ResponsePayload::Detached,
            ResponsePayload::PaneDirectInputAccepted { bytes: 5, pane_id },
            ResponsePayload::ServerStopping,
            ResponsePayload::ServiceInvoked {
                payload: vec![9, 8, 7],
            },
        ];

        for (i, variant) in variants.iter().enumerate() {
            let response = Response::Ok(variant.clone());
            let bytes = encode(&response)
                .unwrap_or_else(|e| panic!("ResponsePayload variant {i} encode failed: {e}"));
            let decoded: Response = decode(&bytes)
                .unwrap_or_else(|e| panic!("ResponsePayload variant {i} decode failed: {e}"));
            assert_eq!(
                decoded, response,
                "ResponsePayload variant {i} roundtrip mismatch"
            );
        }
    }

    // ── Level 1C: Response::Err, all Event variants, all ErrorCode variants ──

    #[test]
    fn response_err_roundtrip() {
        let response = Response::Err(ErrorResponse {
            code: ErrorCode::NotFound,
            message: "session not found".into(),
        });
        assert_roundtrip(&response);
    }

    #[test]
    fn error_code_all_variants_roundtrip() {
        let codes = [
            ErrorCode::NotFound,
            ErrorCode::AlreadyExists,
            ErrorCode::InvalidRequest,
            ErrorCode::VersionMismatch,
            ErrorCode::Timeout,
            ErrorCode::Internal,
        ];
        for code in &codes {
            assert_roundtrip(code);
        }
    }

    #[test]
    fn event_all_variants_roundtrip() {
        let id = Uuid::from_u128(1);
        let id2 = Uuid::from_u128(2);

        let variants: Vec<Event> = vec![
            Event::ServerStarted,
            Event::ServerStopping,
            Event::SessionCreated {
                id,
                name: Some("test".into()),
            },
            Event::SessionCreated { id, name: None },
            Event::SessionRemoved { id },
            Event::ClientAttached { id },
            Event::ClientDetached { id },
            Event::FollowStarted {
                follower_client_id: id,
                leader_client_id: id2,
                global: false,
            },
            Event::FollowStopped {
                follower_client_id: id,
            },
            Event::FollowTargetGone {
                follower_client_id: id,
                former_leader_client_id: id2,
            },
            Event::FollowTargetChanged {
                follower_client_id: id,
                leader_client_id: id2,
                context_id: Some(Uuid::from_u128(3)),
                session_id: Uuid::from_u128(4),
            },
            Event::FollowTargetChanged {
                follower_client_id: id,
                leader_client_id: id2,
                context_id: None,
                session_id: Uuid::from_u128(4),
            },
            Event::AttachViewChanged {
                context_id: Some(id),
                session_id: id2,
                revision: 42,
                components: vec![
                    AttachViewComponent::Scene,
                    AttachViewComponent::SurfaceContent,
                    AttachViewComponent::Layout,
                    AttachViewComponent::Status,
                ],
            },
            Event::PaneOutputAvailable {
                session_id: id,
                pane_id: id2,
            },
            Event::PaneExited {
                session_id: id,
                pane_id: id2,
                reason: Some("process exited with status 130".to_string()),
            },
            Event::PaneExited {
                session_id: id,
                pane_id: id2,
                reason: None,
            },
            Event::PaneRestarted {
                session_id: id,
                pane_id: id2,
            },
        ];

        for (i, variant) in variants.iter().enumerate() {
            let bytes =
                encode(variant).unwrap_or_else(|e| panic!("Event variant {i} encode failed: {e}"));
            let decoded: Event =
                decode(&bytes).unwrap_or_else(|e| panic!("Event variant {i} decode failed: {e}"));
            assert_eq!(&decoded, variant, "Event variant {i} roundtrip mismatch");
        }
    }

    // ── Level 1D: Recording types round-trips ────────────────────────────────

    #[test]
    fn recording_profile_all_variants_roundtrip() {
        for profile in &[
            RecordingProfile::Full,
            RecordingProfile::Functional,
            RecordingProfile::Visual,
        ] {
            assert_roundtrip(profile);
        }
    }

    #[test]
    fn recording_event_kind_all_variants_roundtrip() {
        let kinds = [
            RecordingEventKind::PaneInputRaw,
            RecordingEventKind::PaneOutputRaw,
            RecordingEventKind::ProtocolReplyRaw,
            RecordingEventKind::ServerEvent,
            RecordingEventKind::RequestStart,
            RecordingEventKind::RequestDone,
            RecordingEventKind::RequestError,
            RecordingEventKind::Custom,
        ];
        for kind in &kinds {
            assert_roundtrip(kind);
        }
    }

    #[test]
    fn recording_summary_roundtrip() {
        assert_roundtrip(&sample_recording_summary());
    }

    #[test]
    fn recording_payload_all_variants_roundtrip() {
        let id = Uuid::from_u128(1);
        let payloads: Vec<RecordingPayload> = vec![
            RecordingPayload::Bytes {
                data: vec![1, 2, 3, 4, 5],
            },
            RecordingPayload::Bytes { data: vec![] },
            RecordingPayload::ServerEvent {
                event: Event::SessionCreated {
                    id,
                    name: Some("test".into()),
                },
            },
            RecordingPayload::RequestStart {
                request_id: 42,
                request_kind: "ping".into(),
                exclusive: false,
                request_data: vec![0, 1],
            },
            RecordingPayload::RequestDone {
                request_id: 42,
                request_kind: "ping".into(),
                response_kind: "pong".into(),
                elapsed_ms: 5,
                request_data: vec![0, 1],
                response_data: vec![2, 3],
            },
            RecordingPayload::RequestError {
                request_id: 43,
                request_kind: "kill_session".into(),
                error_code: ErrorCode::NotFound,
                message: "session not found".into(),
                elapsed_ms: 2,
            },
            RecordingPayload::Custom {
                source: "test-plugin".into(),
                name: "custom-event".into(),
                payload: b"{\"ok\":true}".to_vec(),
            },
        ];

        for (i, payload) in payloads.iter().enumerate() {
            let bytes = encode(payload)
                .unwrap_or_else(|e| panic!("RecordingPayload variant {i} encode failed: {e}"));
            let decoded: RecordingPayload = decode(&bytes)
                .unwrap_or_else(|e| panic!("RecordingPayload variant {i} decode failed: {e}"));
            assert_eq!(&decoded, payload, "RecordingPayload variant {i} mismatch");
        }
    }

    #[test]
    fn recording_event_envelope_roundtrip() {
        let envelope = RecordingEventEnvelope {
            seq: 1,
            mono_ns: 1_000_000,
            wall_epoch_ms: 1_700_000_000_000,
            session_id: Some(Uuid::from_u128(1)),
            pane_id: Some(Uuid::from_u128(2)),
            client_id: Some(Uuid::from_u128(3)),
            kind: RecordingEventKind::RequestDone,
            payload: RecordingPayload::RequestDone {
                request_id: 7,
                request_kind: "attach".into(),
                response_kind: "attached".into(),
                elapsed_ms: 12,
                request_data: vec![1, 2, 3],
                response_data: vec![4, 5, 6],
            },
        };
        assert_roundtrip(&envelope);
    }

    #[test]
    fn recording_event_envelope_with_none_ids_roundtrip() {
        let envelope = RecordingEventEnvelope {
            seq: 0,
            mono_ns: 0,
            wall_epoch_ms: 0,
            session_id: None,
            pane_id: None,
            client_id: None,
            kind: RecordingEventKind::Custom,
            payload: RecordingPayload::Bytes { data: vec![255] },
        };
        assert_roundtrip(&envelope);
    }

    #[test]
    fn recording_event_envelope_write_frame_read_frames_roundtrip() {
        let envelopes = vec![
            RecordingEventEnvelope {
                seq: 0,
                mono_ns: 1000,
                wall_epoch_ms: 1_700_000_000_000,
                session_id: Some(Uuid::from_u128(1)),
                pane_id: None,
                client_id: None,
                kind: RecordingEventKind::PaneOutputRaw,
                payload: RecordingPayload::Bytes {
                    data: vec![65, 66, 67],
                },
            },
            RecordingEventEnvelope {
                seq: 1,
                mono_ns: 2000,
                wall_epoch_ms: 1_700_000_000_001,
                session_id: Some(Uuid::from_u128(1)),
                pane_id: Some(Uuid::from_u128(2)),
                client_id: None,
                kind: RecordingEventKind::ServerEvent,
                payload: RecordingPayload::ServerEvent {
                    event: Event::ServerStarted,
                },
            },
        ];

        let mut buf = Vec::new();
        for env in &envelopes {
            write_frame(&mut buf, env).expect("write_frame should succeed");
        }

        let result =
            read_frames::<RecordingEventEnvelope>(&buf).expect("read_frames should succeed");
        assert_eq!(result.frames, envelopes);
        assert_eq!(result.bytes_remaining, 0);
    }

    // ── Level 1E: DisplayTrack types round-trips ─────────────────────────────

    #[test]
    fn display_track_event_all_variants_roundtrip() {
        let variants: Vec<DisplayTrackEvent> = vec![
            DisplayTrackEvent::StreamOpened {
                client_id: Uuid::from_u128(1),
                recording_id: Uuid::from_u128(2),
                cell_width_px: Some(8),
                cell_height_px: Some(16),
                window_width_px: Some(1920),
                window_height_px: Some(1080),
                terminal_profile: Some(vec![10, 20, 30]),
            },
            DisplayTrackEvent::StreamOpened {
                client_id: Uuid::from_u128(1),
                recording_id: Uuid::from_u128(2),
                cell_width_px: None,
                cell_height_px: None,
                window_width_px: None,
                window_height_px: None,
                terminal_profile: None,
            },
            DisplayTrackEvent::Resize {
                cols: 120,
                rows: 40,
            },
            DisplayTrackEvent::FrameBytes {
                data: vec![27, 91, 72],
            },
            DisplayTrackEvent::CursorSnapshot {
                x: 5,
                y: 7,
                visible: true,
                shape: DisplayCursorShape::Bar,
                blink_enabled: false,
            },
            DisplayTrackEvent::Activity {
                kind: DisplayActivityKind::Input,
            },
            DisplayTrackEvent::FrameBytes { data: vec![] },
            DisplayTrackEvent::StreamClosed,
        ];

        for (i, variant) in variants.iter().enumerate() {
            let envelope = DisplayTrackEnvelope {
                mono_ns: (i as u64) * 1000,
                event: variant.clone(),
            };
            assert_roundtrip(&envelope);
        }
    }

    #[test]
    fn display_track_write_frame_read_frames_roundtrip() {
        let envelopes = vec![
            DisplayTrackEnvelope {
                mono_ns: 0,
                event: DisplayTrackEvent::StreamOpened {
                    client_id: Uuid::from_u128(1),
                    recording_id: Uuid::from_u128(2),
                    cell_width_px: Some(8),
                    cell_height_px: Some(16),
                    window_width_px: None,
                    window_height_px: None,
                    terminal_profile: None,
                },
            },
            DisplayTrackEnvelope {
                mono_ns: 1000,
                event: DisplayTrackEvent::Resize { cols: 80, rows: 24 },
            },
            DisplayTrackEnvelope {
                mono_ns: 2000,
                event: DisplayTrackEvent::FrameBytes {
                    data: vec![65; 100],
                },
            },
            DisplayTrackEnvelope {
                mono_ns: 3000,
                event: DisplayTrackEvent::CursorSnapshot {
                    x: 10,
                    y: 11,
                    visible: true,
                    shape: DisplayCursorShape::Block,
                    blink_enabled: true,
                },
            },
            DisplayTrackEnvelope {
                mono_ns: 3500,
                event: DisplayTrackEvent::Activity {
                    kind: DisplayActivityKind::Output,
                },
            },
            DisplayTrackEnvelope {
                mono_ns: 3600,
                event: DisplayTrackEvent::StreamClosed,
            },
        ];

        let mut buf = Vec::new();
        for env in &envelopes {
            write_frame(&mut buf, env).expect("write_frame should succeed");
        }

        let result = read_frames::<DisplayTrackEnvelope>(&buf).expect("read_frames should succeed");
        assert_eq!(result.frames, envelopes);
        assert_eq!(result.bytes_remaining, 0);
    }

    // ── Supporting type round-trips ──────────────────────────────────────────

    #[test]
    fn attach_focus_target_all_variants_roundtrip() {
        let targets = [
            AttachFocusTarget::None,
            AttachFocusTarget::Pane {
                pane_id: Uuid::from_u128(1),
            },
            AttachFocusTarget::Surface {
                surface_id: Uuid::from_u128(2),
            },
        ];
        for target in &targets {
            assert_roundtrip(target);
        }
    }

    #[test]
    fn attach_surface_kind_all_variants_roundtrip() {
        let kinds = [
            AttachSurfaceKind::Pane,
            AttachSurfaceKind::FloatingPane,
            AttachSurfaceKind::Modal,
            AttachSurfaceKind::Overlay,
            AttachSurfaceKind::Tooltip,
        ];
        for kind in &kinds {
            assert_roundtrip(kind);
        }
    }

    #[test]
    fn attach_layer_all_variants_roundtrip() {
        let layers = [
            AttachLayer::Status,
            AttachLayer::Pane,
            AttachLayer::Overlay,
            AttachLayer::FloatingPane,
            AttachLayer::Tooltip,
            AttachLayer::Cursor,
        ];
        for layer in &layers {
            assert_roundtrip(layer);
        }
    }

    #[test]
    fn pane_layout_node_split_roundtrip() {
        assert_roundtrip(&sample_layout_tree());
    }

    #[test]
    fn context_selector_all_variants_roundtrip() {
        let selectors = [
            ContextSelector::ById(Uuid::from_u128(1)),
            ContextSelector::ByName("ctx-name".into()),
        ];
        for sel in &selectors {
            assert_roundtrip(sel);
        }
    }

    #[test]
    fn pane_selector_all_variants_roundtrip() {
        let selectors = [
            PaneSelector::ById(Uuid::from_u128(1)),
            PaneSelector::ByIndex(42),
            PaneSelector::Active,
        ];
        for sel in &selectors {
            assert_roundtrip(sel);
        }
    }
}
