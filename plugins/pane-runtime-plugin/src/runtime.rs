//! Concrete pane runtime implementation owned by the pane-runtime plugin.

use anyhow::{Context, Result};
use bmux_attach_layout_protocol::{
    AttachFocusTarget, AttachInputModeState, AttachLayer, AttachMouseProtocolEncoding,
    AttachMouseProtocolMode, AttachMouseProtocolState, AttachPaneChunk, AttachPaneInputMode,
    AttachPaneMouseProtocol, AttachRect, AttachScene, AttachSurface, AttachSurfaceKind,
    PaneFocusDirection, PaneLaunchCommand, PaneLayoutNode as IpcPaneLayoutNode, PaneSelector,
    PaneSplitDirection, PaneState, PaneSummary,
};
use bmux_context_state::ContextStateHandle;
use bmux_ipc::{ErrorCode, Event};
use bmux_pane_runtime_plugin_api::{
    PaneRuntimePluginConfig,
    pane_runtime_events::{self, AttachViewComponent, PaneEvent},
};
use bmux_pane_runtime_state::{
    AttachViewport, FloatingPaneLayer, FloatingPaneRuntimeSummary, FloatingPaneScope,
    FloatingSurfaceRuntime, LayoutRect, PaneCommandSource, PaneLaunchSpec, PaneLayoutNode,
    PaneResizeDirection, PaneResurrectionSnapshot, PaneRuntimeMeta, SessionRuntimeError,
};
use bmux_recording_protocol::{RecordingEventKind, RecordingPayload as ProtocolRecordingPayload};
use bmux_recording_runtime::{RecordMeta, RecordingSinkHandle};
use bmux_session_models::{ClientId, SessionId};
use bmux_session_state::SessionManagerHandle;
use bmux_snapshot_runtime::{SnapshotDirtyFlag, SnapshotDirtyFlagHandle};
use bmux_terminal_grid::{GridDeltaBatch, GridLimits, TerminalGridStream};
use bmux_terminal_protocol::{ProtocolProfile, TerminalProtocolEngine, protocol_profile_for_term};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tracing::{trace, warn};

static PANE_BLOCKING_THREAD_COUNT: AtomicUsize = AtomicUsize::new(0);

struct PaneBlockingThreadGuard {
    pane_id: Uuid,
    role: &'static str,
}

impl PaneBlockingThreadGuard {
    fn new(pane_id: Uuid, role: &'static str) -> Self {
        let active = PANE_BLOCKING_THREAD_COUNT.fetch_add(1, Ordering::SeqCst) + 1;
        trace!(%pane_id, role, active, "pane blocking thread started");
        Self { pane_id, role }
    }
}

impl Drop for PaneBlockingThreadGuard {
    fn drop(&mut self) {
        let active = PANE_BLOCKING_THREAD_COUNT.fetch_sub(1, Ordering::SeqCst) - 1;
        trace!(pane_id = %self.pane_id, role = self.role, active, "pane blocking thread exited");
    }
}
use uuid::Uuid;

type RecordingPayload = ProtocolRecordingPayload<Event, ErrorCode>;

const MAX_WINDOW_OUTPUT_BUFFER_BYTES: usize = 1_048_576;
#[cfg(test)]
const MAX_TERMINAL_GRID_DELTA_BATCHES: usize = 1_024;
#[cfg(test)]
const MAX_TERMINAL_GRID_DELTA_BYTES: usize = 16 * 1024 * 1024;
const RESPONSE_METADATA_HEADROOM: usize = 65_536;
const RESPONSE_OUTPUT_BUDGET: usize =
    bmux_ipc::frame::MAX_FRAME_PAYLOAD_SIZE - RESPONSE_METADATA_HEADROOM;

fn context_handle() -> ContextStateHandle {
    bmux_plugin::global_plugin_state_registry()
        .get::<ContextStateHandle>()
        .and_then(|arc| arc.read().ok().map(|guard| guard.clone()))
        .unwrap_or_else(ContextStateHandle::noop)
}

pub(crate) fn session_handle() -> SessionManagerHandle {
    bmux_plugin::global_plugin_state_registry()
        .get::<SessionManagerHandle>()
        .and_then(|arc| arc.read().ok().map(|guard| guard.clone()))
        .unwrap_or_else(SessionManagerHandle::noop)
}

fn snapshot_dirty_flag() -> Arc<SnapshotDirtyFlag> {
    bmux_plugin::global_plugin_state_registry()
        .get::<SnapshotDirtyFlagHandle>()
        .and_then(|arc| arc.read().ok().map(|guard| Arc::clone(&guard.0)))
        .unwrap_or_else(|| Arc::new(SnapshotDirtyFlag::new()))
}

fn mark_snapshot_dirty_flag() {
    snapshot_dirty_flag().mark_dirty();
}

pub(crate) fn publish_pane_event(event: PaneEvent) {
    if let Err(error) =
        bmux_plugin::global_event_bus().emit(&pane_runtime_events::EVENT_KIND, event)
    {
        warn!(%error, "failed publishing pane-runtime event");
    }
}

fn record_to_all_runtimes(kind: RecordingEventKind, payload: RecordingPayload, meta: RecordMeta) {
    let Some(handle) = bmux_plugin::global_plugin_state_registry()
        .get::<RecordingSinkHandle>()
        .and_then(|arc| arc.read().ok().map(|guard| guard.clone()))
    else {
        return;
    };
    handle.0.record(kind, payload, meta);
}

fn current_context_id_for_session(session_id: SessionId) -> Option<Uuid> {
    context_handle().0.context_for_session(session_id)
}

fn emit_attach_view_changed_for_layout(session_id: SessionId) {
    let revision = session_runtime_handle()
        .0
        .bump_attach_view_revision(session_id);
    publish_pane_event(PaneEvent::AttachViewChanged {
        context_id: current_context_id_for_session(session_id),
        session_id: session_id.0,
        revision: revision.unwrap_or(0),
        components: vec![
            AttachViewComponent::Scene,
            AttachViewComponent::SurfaceContent,
            AttachViewComponent::Layout,
            AttachViewComponent::Status,
        ],
    });
}

pub(crate) fn session_runtime_handle() -> bmux_pane_runtime_state::SessionRuntimeManagerHandle {
    bmux_plugin::global_plugin_state_registry()
        .get::<bmux_pane_runtime_state::SessionRuntimeManagerHandle>()
        .and_then(|arc| arc.read().ok().map(|guard| guard.clone()))
        .unwrap_or_else(bmux_pane_runtime_state::SessionRuntimeManagerHandle::noop)
}

async fn shutdown_runtime_handle(removed: RemovedRuntime) {
    for pane in removed.handle.panes.into_values() {
        shutdown_pane_handle(pane).await;
    }
}

async fn shutdown_pane_handle(mut pane: PaneRuntimeHandle) {
    if let Some(stop_tx) = pane.stop_tx.take() {
        let _ = stop_tx.send(());
    }

    if tokio::time::timeout(Duration::from_millis(250), &mut pane.task)
        .await
        .is_err()
    {
        let active_blocking_threads = PANE_BLOCKING_THREAD_COUNT.load(Ordering::SeqCst);
        warn!(
            pane_id = %pane.meta.id,
            active_blocking_threads,
            "pane runtime shutdown exceeded 250ms; aborting async task and leaving blocking thread cleanup detached",
        );
        // The pane runtime task owns OS-thread handles for PTY reader/waiter
        // teardown. If one of those threads is stuck in a blocking read or
        // wait, awaiting an aborted task here can wedge the server-side
        // service dispatcher. Abort and detach instead; the task/thread
        // resources are process-scoped cleanup best-effort after the pane has
        // already been removed from runtime state.
        pane.task.abort();
    }
}

fn push_pane_runtime_notice(
    output_buffer: &Arc<std::sync::Mutex<OutputFanoutBuffer>>,
    message: impl AsRef<str>,
) {
    if let Ok(mut output) = output_buffer.lock() {
        output.push_chunk(message.as_ref().as_bytes());
    }
}

fn format_pane_exit_reason(status: &portable_pty::ExitStatus) -> String {
    if let Some(signal) = status.signal() {
        return format!("process terminated by signal {signal}");
    }
    format!("process exited with status {}", status.exit_code())
}

struct SessionRuntimeManager {
    runtimes: BTreeMap<SessionId, SessionRuntimeHandle>,
    pane_input_index: Arc<RwLock<BTreeMap<Uuid, PaneInputHandle>>>,
    client_write_permissions: Arc<RwLock<BTreeMap<SessionId, BTreeSet<ClientId>>>>,
    shell: String,
    pane_term: String,
    protocol_profile: ProtocolProfile,
    shell_integration_root: Option<std::path::PathBuf>,
    pane_exit_tx: mpsc::UnboundedSender<PaneExitEvent>,
}

#[derive(Clone)]
struct PaneInputHandle {
    session_id: SessionId,
    input_tx: mpsc::UnboundedSender<PaneRuntimeCommand>,
    exited: Arc<AtomicBool>,
}

struct SessionRuntimeHandle {
    panes: BTreeMap<Uuid, PaneRuntimeHandle>,
    layout_root: Option<PaneLayoutNode>,
    focused_pane_id: Uuid,
    zoomed_pane_id: Option<Uuid>,
    floating_surfaces: Vec<FloatingSurfaceRuntime>,
    attached_clients: BTreeSet<ClientId>,
    attach_viewport: Option<AttachViewport>,
    attach_view_revision: u64,
}

#[derive(Debug, Clone, Default)]
struct PaneResurrectionRuntime {
    active_command: Option<String>,
    active_command_source: Option<PaneCommandSource>,
    last_known_cwd: Option<String>,
}

impl PaneResurrectionRuntime {
    fn from_snapshot(snapshot: &PaneResurrectionSnapshot) -> Self {
        Self {
            active_command: snapshot.active_command.clone(),
            active_command_source: snapshot.active_command_source,
            last_known_cwd: snapshot.last_known_cwd.clone(),
        }
    }

    fn to_snapshot(&self) -> PaneResurrectionSnapshot {
        PaneResurrectionSnapshot {
            active_command: self.active_command.clone(),
            active_command_source: self.active_command_source,
            last_known_cwd: self.last_known_cwd.clone(),
        }
    }

    fn apply_event(&mut self, event: PaneShellMetadataEvent) {
        match event {
            PaneShellMetadataEvent::CommandStart { command, cwd } => {
                self.active_command = Some(command);
                self.active_command_source = Some(PaneCommandSource::Verbatim);
                self.last_known_cwd = Some(cwd);
            }
            PaneShellMetadataEvent::Prompt { cwd } => {
                self.last_known_cwd = Some(cwd);
                self.active_command = None;
                self.active_command_source = None;
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PaneShellMetadataEvent {
    CommandStart { command: String, cwd: String },
    Prompt { cwd: String },
}

fn apply_shell_metadata_events_and_take_prompt_replay(
    state: &mut PaneResurrectionRuntime,
    pending_replay_command: &mut Option<String>,
    events: impl IntoIterator<Item = PaneShellMetadataEvent>,
) -> Option<String> {
    let mut replay_command = None;
    for event in events {
        let is_prompt = matches!(event, PaneShellMetadataEvent::Prompt { .. });
        state.apply_event(event);
        if is_prompt
            && replay_command.is_none()
            && let Some(command) = pending_replay_command.take()
        {
            state.active_command = Some(command.clone());
            state.active_command_source = Some(PaneCommandSource::Verbatim);
            replay_command = Some(command);
        }
    }
    replay_command
}

struct PaneRuntimeHandle {
    meta: PaneRuntimeMeta,
    process_id: Arc<std::sync::Mutex<Option<u32>>>,
    process_group_id: Arc<std::sync::Mutex<Option<i32>>>,
    resurrection_state: Arc<std::sync::Mutex<PaneResurrectionRuntime>>,
    exit_reason: Arc<std::sync::Mutex<Option<String>>>,
    stop_tx: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
    input_tx: mpsc::UnboundedSender<PaneRuntimeCommand>,
    output_buffer: Arc<std::sync::Mutex<OutputFanoutBuffer>>,
    terminal_grid: Arc<std::sync::Mutex<TerminalGridStream>>,
    terminal_grid_deltas: Arc<std::sync::Mutex<TerminalGridDeltaLog>>,
    exited: Arc<AtomicBool>,
    last_requested_size: Arc<std::sync::Mutex<(u16, u16)>>,
    /// Set to `true` by the PTY reader when new output arrives. The broadcast
    /// event is only emitted on the `false→true` transition, coalescing
    /// thousands of per-chunk writes into ~1 event per fetch cycle.
    output_dirty: Arc<AtomicBool>,
    /// True while the inner application is inside a DEC mode 2026
    /// synchronized update (`\x1b[?2026h` seen, `\x1b[?2026l` not yet).
    /// Set by the PTY reader thread via the terminal mode tracker.
    sync_update_in_progress: Arc<AtomicBool>,
    mouse_protocol_state: Arc<std::sync::Mutex<AttachMouseProtocolState>>,
    input_mode_state: Arc<std::sync::Mutex<AttachInputModeState>>,
    #[cfg(feature = "image-registry")]
    image_registry: Arc<std::sync::Mutex<bmux_image::ImageRegistry>>,
    /// Cell pixel dimensions reported by the client (width, height).
    #[cfg(feature = "image-registry")]
    cell_pixel_size: Arc<std::sync::Mutex<(u16, u16)>>,
    /// Set to `true` when the image registry has new content.
    #[cfg(feature = "image-registry")]
    image_dirty: Arc<AtomicBool>,
}

enum PaneRuntimeCommand {
    Input(Vec<u8>),
    Resize { rows: u16, cols: u16 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerminalModeParseState {
    Ground,
    Esc,
    Csi,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug)]
struct PaneTerminalModeTracker {
    parse_state: TerminalModeParseState,
    csi_buffer: Vec<u8>,
    x10_mode: bool,
    press_release_mode: bool,
    button_motion_mode: bool,
    any_motion_mode: bool,
    utf8_encoding: bool,
    sgr_encoding: bool,
    application_cursor: bool,
    application_keypad: bool,
    /// DEC mode 2026: the inner application has begun a synchronized
    /// update (`\x1b[?2026h`) but has not yet ended it (`\x1b[?2026l`).
    sync_update: bool,
}

impl Default for PaneTerminalModeTracker {
    fn default() -> Self {
        Self {
            parse_state: TerminalModeParseState::Ground,
            csi_buffer: Vec::new(),
            x10_mode: false,
            press_release_mode: false,
            button_motion_mode: false,
            any_motion_mode: false,
            utf8_encoding: false,
            sgr_encoding: false,
            application_cursor: false,
            application_keypad: false,
            sync_update: false,
        }
    }
}

impl PaneTerminalModeTracker {
    fn process(&mut self, bytes: &[u8]) {
        for byte in bytes {
            match self.parse_state {
                TerminalModeParseState::Ground => {
                    if *byte == 0x1b {
                        self.parse_state = TerminalModeParseState::Esc;
                    }
                }
                TerminalModeParseState::Esc => {
                    if *byte == b'[' {
                        self.parse_state = TerminalModeParseState::Csi;
                        self.csi_buffer.clear();
                    } else if *byte == b'=' {
                        self.application_keypad = true;
                        self.parse_state = TerminalModeParseState::Ground;
                    } else if *byte == b'>' {
                        self.application_keypad = false;
                        self.parse_state = TerminalModeParseState::Ground;
                    } else if *byte == b'c' {
                        self.reset();
                    } else if *byte == 0x1b {
                        self.parse_state = TerminalModeParseState::Esc;
                    } else {
                        self.parse_state = TerminalModeParseState::Ground;
                    }
                }
                TerminalModeParseState::Csi => {
                    if *byte == 0x1b {
                        self.parse_state = TerminalModeParseState::Esc;
                        self.csi_buffer.clear();
                        continue;
                    }
                    self.csi_buffer.push(*byte);
                    if (0x40..=0x7e).contains(byte) {
                        let sequence = std::mem::take(&mut self.csi_buffer);
                        self.apply_csi_sequence(&sequence);
                        self.parse_state = TerminalModeParseState::Ground;
                    } else if self.csi_buffer.len() > 64 {
                        self.parse_state = TerminalModeParseState::Ground;
                        self.csi_buffer.clear();
                    }
                }
            }
        }
    }

    const fn current_protocol(&self) -> AttachMouseProtocolState {
        let mode = if self.any_motion_mode {
            AttachMouseProtocolMode::AnyMotion
        } else if self.button_motion_mode {
            AttachMouseProtocolMode::ButtonMotion
        } else if self.press_release_mode {
            AttachMouseProtocolMode::PressRelease
        } else if self.x10_mode {
            AttachMouseProtocolMode::Press
        } else {
            AttachMouseProtocolMode::None
        };

        let encoding = if self.sgr_encoding {
            AttachMouseProtocolEncoding::Sgr
        } else if self.utf8_encoding {
            AttachMouseProtocolEncoding::Utf8
        } else {
            AttachMouseProtocolEncoding::Default
        };

        AttachMouseProtocolState { mode, encoding }
    }

    const fn current_input_modes(&self) -> AttachInputModeState {
        AttachInputModeState {
            application_cursor: self.application_cursor,
            application_keypad: self.application_keypad,
        }
    }

    fn reset(&mut self) {
        self.parse_state = TerminalModeParseState::Ground;
        self.csi_buffer.clear();
        self.x10_mode = false;
        self.press_release_mode = false;
        self.button_motion_mode = false;
        self.any_motion_mode = false;
        self.utf8_encoding = false;
        self.sgr_encoding = false;
        self.application_cursor = false;
        self.application_keypad = false;
        self.sync_update = false;
    }

    fn apply_csi_sequence(&mut self, sequence: &[u8]) {
        if sequence == b"!p" {
            self.reset();
            return;
        }

        let Some((&final_byte, params)) = sequence.split_last() else {
            return;
        };

        let enable = match final_byte {
            b'h' => true,
            b'l' => false,
            _ => return,
        };

        let Some(private_modes) = params.strip_prefix(b"?") else {
            return;
        };

        for mode in private_modes
            .split(|byte| *byte == b';')
            .filter_map(parse_private_mode_number)
        {
            self.apply_private_mode(mode, enable);
        }
    }

    const fn apply_private_mode(&mut self, mode: u16, enable: bool) {
        match mode {
            1 => self.application_cursor = enable,
            9 => self.x10_mode = enable,
            1000 => self.press_release_mode = enable,
            1002 => self.button_motion_mode = enable,
            1003 => self.any_motion_mode = enable,
            1005 => self.utf8_encoding = enable,
            1006 => self.sgr_encoding = enable,
            2026 => self.sync_update = enable,
            _ => {}
        }
    }
}

struct PaneCursorTracker {
    terminal_grid: bmux_terminal_grid::TerminalGridStream,
    rows: u16,
    cols: u16,
    cursor_escape_state: CursorEscapeState,
    /// Cumulative number of lines that have scrolled off the top.
    /// Used by the image registry to shift image positions on scroll.
    #[cfg(feature = "image-registry")]
    total_scrollback: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CursorEscapeState {
    Ground,
    Esc,
    EscBracket,
}

impl PaneCursorTracker {
    fn new(rows: u16, cols: u16) -> Self {
        let (rows, cols) = sanitize_pty_size(rows, cols);
        Self {
            terminal_grid: bmux_terminal_grid::TerminalGridStream::new(
                cols,
                rows,
                bmux_terminal_grid::GridLimits::default(),
            )
            .expect("pane cursor tracker grid dimensions are valid"),
            rows,
            cols,
            cursor_escape_state: CursorEscapeState::Ground,
            #[cfg(feature = "image-registry")]
            total_scrollback: 0,
        }
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        let (rows, cols) = sanitize_pty_size(rows, cols);
        if self.rows == rows && self.cols == cols {
            return;
        }
        let _ = self.terminal_grid.resize(cols, rows);
        self.rows = rows;
        self.cols = cols;
    }

    fn process(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }

        let mut normalized = Vec::with_capacity(bytes.len());
        for byte in bytes {
            match self.cursor_escape_state {
                CursorEscapeState::Ground => {
                    if *byte == 0x1b {
                        self.cursor_escape_state = CursorEscapeState::Esc;
                    } else {
                        normalized.push(*byte);
                    }
                }
                CursorEscapeState::Esc => {
                    if *byte == b'[' {
                        self.cursor_escape_state = CursorEscapeState::EscBracket;
                    } else if *byte == 0x1b {
                        normalized.push(0x1b);
                        self.cursor_escape_state = CursorEscapeState::Esc;
                    } else {
                        normalized.extend_from_slice(&[0x1b, *byte]);
                        self.cursor_escape_state = CursorEscapeState::Ground;
                    }
                }
                CursorEscapeState::EscBracket => {
                    match *byte {
                        // Preserve CSI s/u save/restore short forms across split
                        // escape chunks before feeding the structured grid.
                        b's' => normalized.extend_from_slice(b"\x1b7"),
                        b'u' => normalized.extend_from_slice(b"\x1b8"),
                        _ => {
                            normalized.extend_from_slice(b"\x1b[");
                            normalized.push(*byte);
                        }
                    }
                    self.cursor_escape_state = CursorEscapeState::Ground;
                }
            }
        }

        if !normalized.is_empty() {
            self.terminal_grid.process(&normalized);
        }
    }

    fn cursor_position(&self) -> (u16, u16) {
        let cursor = self.terminal_grid.grid().cursor();
        (
            u16::try_from(cursor.row).unwrap_or(u16::MAX),
            u16::try_from(cursor.col).unwrap_or(u16::MAX),
        )
    }

    /// Consume any scrollback that accumulated since the last call.
    /// Returns the number of lines that scrolled since last drain.
    #[cfg(feature = "image-registry")]
    fn drain_scroll_delta(&mut self) -> u16 {
        let total_scrolled = self.terminal_grid.grid().total_scrolled_rows();
        let delta = total_scrolled.saturating_sub(self.total_scrollback);
        self.total_scrollback = total_scrolled;
        u16::try_from(delta).unwrap_or(u16::MAX)
    }
}

fn sanitize_pty_size(rows: u16, cols: u16) -> (u16, u16) {
    (rows.max(1), cols.max(1))
}

/// Convert an image event to a recording payload.
#[cfg(feature = "image-registry")]
fn image_event_to_recording_payload(event: &bmux_image::ImageEvent) -> RecordingPayload {
    match event {
        #[cfg(feature = "image-registry")]
        bmux_image::ImageEvent::SixelImage {
            data,
            position,
            pixel_size,
            ..
        } => RecordingPayload::Image {
            protocol: 0,
            position_row: position.row,
            position_col: position.col,
            cell_rows: 0,
            cell_cols: 0,
            pixel_width: pixel_size.width,
            pixel_height: pixel_size.height,
            data: data.clone(),
        },
        #[cfg(feature = "image-registry")]
        bmux_image::ImageEvent::KittyCommand { command: cmd, .. } => {
            let mut apc_body = Vec::new();
            match cmd {
                bmux_image::KittyCommand::Transmit {
                    image_id,
                    data,
                    width,
                    height,
                    ..
                } => {
                    apc_body = bmux_image::codec::kitty::encode_transmit(
                        *image_id,
                        bmux_image::KittyFormat::Rgba,
                        data,
                        *width,
                        *height,
                    );
                }
                bmux_image::KittyCommand::Place(placement) => {
                    apc_body = bmux_image::codec::kitty::encode_place(
                        placement.image_id,
                        placement.placement_id,
                        placement.position.row,
                        placement.position.col,
                    );
                }
                _ => {}
            }
            RecordingPayload::Image {
                protocol: 1,
                position_row: 0,
                position_col: 0,
                cell_rows: 0,
                cell_cols: 0,
                pixel_width: 0,
                pixel_height: 0,
                data: apc_body,
            }
        }
        #[cfg(feature = "image-registry")]
        bmux_image::ImageEvent::ITerm2Image { data, position, .. } => RecordingPayload::Image {
            protocol: 2,
            position_row: position.row,
            position_col: position.col,
            cell_rows: 0,
            cell_cols: 0,
            pixel_width: 0,
            pixel_height: 0,
            data: data.clone(),
        },
    }
}

/// Check if a chunk contains a screen-clearing CSI sequence.
/// Looks for `\e[2J` (erase display) or `\e[3J` (erase scrollback + display).
#[cfg(feature = "image-registry")]
fn chunk_contains_screen_clear(chunk: &[u8]) -> bool {
    // Fast scan for the byte patterns.
    for window in chunk.windows(4) {
        if window[0] == 0x1b
            && window[1] == b'['
            && window[3] == b'J'
            && (window[2] == b'2' || window[2] == b'3')
        {
            return true;
        }
    }
    false
}

fn protocol_reply_for_chunk(
    protocol_engine: &mut TerminalProtocolEngine,
    cursor_tracker: &mut PaneCursorTracker,
    chunk: &[u8],
) -> Vec<u8> {
    let mut reply = Vec::new();
    for byte in chunk {
        let byte_slice = std::slice::from_ref(byte);
        cursor_tracker.process(byte_slice);
        let byte_reply =
            protocol_engine.process_output(byte_slice, cursor_tracker.cursor_position());
        if let Some((query_kind, reply_row, reply_col)) = parse_cpr_reply(&byte_reply) {
            let (tracked_row, tracked_col) = cursor_tracker.cursor_position();
            trace!(
                query_kind,
                reply_row,
                reply_col,
                tracked_row = tracked_row.saturating_add(1),
                tracked_col = tracked_col.saturating_add(1),
                pane_rows = cursor_tracker.rows,
                pane_cols = cursor_tracker.cols,
                alternate_screen = matches!(
                    cursor_tracker.terminal_grid.grid().mode(),
                    bmux_terminal_grid::GridMode::Alternate
                ),
                "pane protocol reply: cursor position report"
            );
        }
        reply.extend(byte_reply);
    }
    reply
}

fn parse_cpr_reply(reply: &[u8]) -> Option<(&'static str, u16, u16)> {
    if let Some(body) = reply.strip_prefix(b"\x1b[?")
        && let Some((row, col)) = parse_cpr_coords(body, true)
    {
        return Some(("dec_cpr", row, col));
    }
    let body = reply.strip_prefix(b"\x1b[")?;
    parse_cpr_coords(body, false).map(|(row, col)| ("cpr", row, col))
}

fn parse_cpr_coords(body: &[u8], dec: bool) -> Option<(u16, u16)> {
    let body = body.strip_suffix(b"R")?;
    if !dec && body.starts_with(b"?") {
        return None;
    }

    let mut parts = body.splitn(2, |byte| *byte == b';');
    let row = std::str::from_utf8(parts.next()?)
        .ok()?
        .parse::<u16>()
        .ok()?;
    let col = std::str::from_utf8(parts.next()?)
        .ok()?
        .parse::<u16>()
        .ok()?;
    Some((row, col))
}

fn parse_private_mode_number(bytes: &[u8]) -> Option<u16> {
    if bytes.is_empty() {
        return None;
    }
    let mut value: u16 = 0;
    for byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?;
        value = value.checked_add(u16::from(*byte - b'0'))?;
    }
    Some(value)
}

const BMUX_SHELL_METADATA_PREFIX: &str = "633;bmux;";
const MAX_SHELL_METADATA_PAYLOAD_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellKind {
    Bash,
    Zsh,
    Fish,
    Nu,
    Other,
}

#[derive(Debug, Default)]
struct PaneShellMetadataParseOutput {
    filtered: Vec<u8>,
    events: Vec<PaneShellMetadataEvent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneShellMetadataTerminator {
    Bell,
    StringTerminator,
}

#[derive(Debug, Default)]
struct PaneShellMetadataParser {
    state: PaneShellMetadataParserState,
}

#[derive(Debug, Default)]
enum PaneShellMetadataParserState {
    #[default]
    Ground,
    Escape,
    Osc {
        payload: Vec<u8>,
        saw_escape: bool,
    },
}

impl PaneShellMetadataParser {
    fn process_chunk(&mut self, chunk: &[u8]) -> PaneShellMetadataParseOutput {
        let mut out = PaneShellMetadataParseOutput {
            filtered: Vec::with_capacity(chunk.len()),
            events: Vec::new(),
        };

        for byte in chunk {
            match &mut self.state {
                PaneShellMetadataParserState::Ground => {
                    if *byte == 0x1b {
                        self.state = PaneShellMetadataParserState::Escape;
                    } else {
                        out.filtered.push(*byte);
                    }
                }
                PaneShellMetadataParserState::Escape => {
                    if *byte == b']' {
                        self.state = PaneShellMetadataParserState::Osc {
                            payload: Vec::new(),
                            saw_escape: false,
                        };
                    } else {
                        out.filtered.push(0x1b);
                        if *byte == 0x1b {
                            self.state = PaneShellMetadataParserState::Escape;
                        } else {
                            out.filtered.push(*byte);
                            self.state = PaneShellMetadataParserState::Ground;
                        }
                    }
                }
                PaneShellMetadataParserState::Osc {
                    payload,
                    saw_escape,
                } => {
                    if *saw_escape {
                        if *byte == b'\\' {
                            let payload = std::mem::take(payload);
                            if let Some(event) = decode_shell_metadata_payload(&payload) {
                                out.events.push(event);
                            } else {
                                append_raw_osc_sequence(
                                    &mut out.filtered,
                                    &payload,
                                    PaneShellMetadataTerminator::StringTerminator,
                                );
                            }
                            self.state = PaneShellMetadataParserState::Ground;
                            continue;
                        }
                        payload.push(0x1b);
                        *saw_escape = false;
                    }

                    if *byte == 0x1b {
                        *saw_escape = true;
                    } else if *byte == 0x07 {
                        let payload = std::mem::take(payload);
                        if let Some(event) = decode_shell_metadata_payload(&payload) {
                            out.events.push(event);
                        } else {
                            append_raw_osc_sequence(
                                &mut out.filtered,
                                &payload,
                                PaneShellMetadataTerminator::Bell,
                            );
                        }
                        self.state = PaneShellMetadataParserState::Ground;
                    } else {
                        payload.push(*byte);
                        if payload.len() > MAX_SHELL_METADATA_PAYLOAD_BYTES {
                            out.filtered.push(0x1b);
                            out.filtered.push(b']');
                            out.filtered.extend_from_slice(payload);
                            self.state = PaneShellMetadataParserState::Ground;
                        }
                    }
                }
            }
        }

        out
    }
}

fn append_raw_osc_sequence(
    output: &mut Vec<u8>,
    payload: &[u8],
    terminator: PaneShellMetadataTerminator,
) {
    output.push(0x1b);
    output.push(b']');
    output.extend_from_slice(payload);
    match terminator {
        PaneShellMetadataTerminator::Bell => output.push(0x07),
        PaneShellMetadataTerminator::StringTerminator => {
            output.push(0x1b);
            output.push(b'\\');
        }
    }
}

fn decode_shell_metadata_payload(payload: &[u8]) -> Option<PaneShellMetadataEvent> {
    let payload = std::str::from_utf8(payload).ok()?;
    let payload = payload.strip_prefix(BMUX_SHELL_METADATA_PREFIX)?;
    let mut fields = payload.split(';');
    let kind = fields.next()?;
    match kind {
        "start" => {
            let command = decode_shell_metadata_field(fields.next()?)?;
            let cwd = decode_shell_metadata_field(fields.next()?)?;
            if fields.next().is_some() || command.trim().is_empty() || cwd.trim().is_empty() {
                return None;
            }
            Some(PaneShellMetadataEvent::CommandStart { command, cwd })
        }
        "prompt" => {
            let cwd = decode_shell_metadata_field(fields.next()?)?;
            if fields.next().is_some() || cwd.trim().is_empty() {
                return None;
            }
            Some(PaneShellMetadataEvent::Prompt { cwd })
        }
        _ => None,
    }
}

fn decode_shell_metadata_field(value: &str) -> Option<String> {
    let decoded = decode_base64(value)?;
    String::from_utf8(decoded).ok()
}

fn decode_base64(value: &str) -> Option<Vec<u8>> {
    let bytes = value.as_bytes();
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return None;
    }

    let mut out = Vec::with_capacity((bytes.len() / 4) * 3);
    let mut chunk_start = 0_usize;
    while chunk_start < bytes.len() {
        let a = decode_base64_value(bytes[chunk_start])?;
        let b = decode_base64_value(bytes[chunk_start + 1])?;
        let c_raw = bytes[chunk_start + 2];
        let d_raw = bytes[chunk_start + 3];

        let c = if c_raw == b'=' {
            0
        } else {
            decode_base64_value(c_raw)?
        };
        let d = if d_raw == b'=' {
            0
        } else {
            decode_base64_value(d_raw)?
        };

        let padding = usize::from(c_raw == b'=') + usize::from(d_raw == b'=');
        if padding > 0 && chunk_start + 4 != bytes.len() {
            return None;
        }

        out.push((a << 2) | (b >> 4));
        if padding < 2 {
            out.push((b << 4) | (c >> 2));
        }
        if padding == 0 {
            out.push((c << 6) | d);
        }

        chunk_start += 4;
    }

    Some(out)
}

const fn decode_base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

fn shell_kind_for_path(shell: &str) -> ShellKind {
    let basename = std::path::Path::new(shell)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(shell)
        .trim_start_matches('-')
        .to_ascii_lowercase();
    match basename.as_str() {
        "bash" => ShellKind::Bash,
        "zsh" => ShellKind::Zsh,
        "fish" => ShellKind::Fish,
        "nu" => ShellKind::Nu,
        _ => ShellKind::Other,
    }
}

fn configure_device_seal_broker_env(command: &mut CommandBuilder) {
    if std::env::var("BMUX_DEVICE_SEAL_SCOPE").as_deref() == Ok("client") {
        if let Ok(exe) = std::env::current_exe() {
            command.env(
                "SSHENV_DEVICE_SEAL_COMMAND",
                format!("{} device-seal-broker", exe.display()),
            );
        }
        return;
    }

    command.env("SSHENV_DEVICE_SEAL_COMMAND", "");
}

fn configure_shell_integration_command(
    command: &mut CommandBuilder,
    shell: &str,
    integration_root: Option<&std::path::Path>,
) -> Result<()> {
    let Some(integration_root) = integration_root else {
        return Ok(());
    };

    match shell_kind_for_path(shell) {
        ShellKind::Bash => {
            let rcfile_path = integration_root.join("bash").join("bmux.bashrc");
            write_shell_integration_file(&rcfile_path, shell_integration_bash_rcfile())?;
            command.arg("--rcfile");
            command.arg(&rcfile_path);
        }
        ShellKind::Zsh => {
            let zdotdir = integration_root.join("zsh");
            let zshenv_path = zdotdir.join(".zshenv");
            let zshrc_path = zdotdir.join(".zshrc");
            write_shell_integration_file(&zshenv_path, shell_integration_zsh_env())?;
            write_shell_integration_file(&zshrc_path, shell_integration_zsh_rc())?;
            command.env("ZDOTDIR", zdotdir.as_os_str());
            if let Ok(original_zdotdir) = std::env::var("ZDOTDIR")
                && !original_zdotdir.trim().is_empty()
            {
                command.env("BMUX_ORIG_ZDOTDIR", original_zdotdir);
            }
        }
        ShellKind::Fish => {
            let xdg_config_home = integration_root.join("fish-xdg");
            let fish_config_path = xdg_config_home.join("fish").join("config.fish");
            write_shell_integration_file(&fish_config_path, shell_integration_fish_config())?;
            command.env("XDG_CONFIG_HOME", xdg_config_home.as_os_str());
            if let Ok(original_xdg_config_home) = std::env::var("XDG_CONFIG_HOME")
                && !original_xdg_config_home.trim().is_empty()
            {
                command.env("BMUX_ORIG_XDG_CONFIG_HOME", original_xdg_config_home);
            }
        }
        ShellKind::Nu => {
            let config_path = integration_root.join("nu").join("config.nu");
            write_shell_integration_file(&config_path, shell_integration_nu_config())?;
            command.arg("--config");
            command.arg(&config_path);
        }
        ShellKind::Other => {}
    }

    Ok(())
}

fn write_shell_integration_file(path: &std::path::Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("failed creating shell integration dir {}", parent.display())
        })?;
    }
    if let Ok(existing) = std::fs::read_to_string(path)
        && existing == contents
    {
        return Ok(());
    }
    std::fs::write(path, contents)
        .with_context(|| format!("failed writing shell integration file {}", path.display()))
}

#[allow(clippy::literal_string_with_formatting_args)]
const fn shell_integration_bash_rcfile() -> &'static str {
    r#"if [[ -f "${HOME}/.bashrc" ]]; then
  source "${HOME}/.bashrc"
fi

if [[ -z "${__BMUX_RESURRECTION_HOOK_INSTALLED:-}" ]]; then
  __BMUX_RESURRECTION_HOOK_INSTALLED=1

  __bmux_b64() {
    printf '%s' "$1" | base64 | tr -d '\r\n'
  }

  __bmux_emit_start() {
    local cmd_b64 cwd_b64
    cmd_b64="$(__bmux_b64 "$1")"
    cwd_b64="$(__bmux_b64 "$2")"
    printf '\033]633;bmux;start;%s;%s\007' "$cmd_b64" "$cwd_b64"
  }

  __bmux_emit_prompt() {
    local cwd_b64
    cwd_b64="$(__bmux_b64 "$1")"
    printf '\033]633;bmux;prompt;%s\007' "$cwd_b64"
  }

  __BMUX_READY_FOR_CMD=1

  __bmux_preexec_trap() {
    [[ -n "${__BMUX_EMIT_GUARD:-}" ]] && return
    [[ "${__BMUX_READY_FOR_CMD:-0}" != "1" ]] && return
    __BMUX_READY_FOR_CMD=0
    __BMUX_EMIT_GUARD=1

    local hist_line cmd
    hist_line="$(HISTTIMEFORMAT= builtin history 1 2>/dev/null)"
    cmd="$hist_line"
    if [[ "$hist_line" =~ ^[[:space:]]*[0-9]+[[:space:]]+(.*)$ ]]; then
      cmd="${BASH_REMATCH[1]}"
    fi
    if [[ -z "$cmd" && -n "${BASH_COMMAND:-}" ]]; then
      cmd="${BASH_COMMAND}"
    fi
    if [[ -n "$cmd" ]]; then
      __bmux_emit_start "$cmd" "$PWD"
    fi

    __BMUX_EMIT_GUARD=
  }

  __bmux_precmd_hook() {
    [[ -n "${__BMUX_EMIT_GUARD:-}" ]] && return
    __BMUX_EMIT_GUARD=1
    __bmux_emit_prompt "$PWD"
    __BMUX_READY_FOR_CMD=1
    __BMUX_EMIT_GUARD=
  }

  trap '__bmux_preexec_trap' DEBUG
  if [[ -n "${PROMPT_COMMAND:-}" ]]; then
    PROMPT_COMMAND="__bmux_precmd_hook;${PROMPT_COMMAND}"
  else
    PROMPT_COMMAND="__bmux_precmd_hook"
  fi
fi
"#
}

#[allow(clippy::literal_string_with_formatting_args)]
const fn shell_integration_zsh_env() -> &'static str {
    r#"if [[ -n "${BMUX_ORIG_ZDOTDIR:-}" ]]; then
  __bmux_orig_zdotdir="${BMUX_ORIG_ZDOTDIR}"
else
  __bmux_orig_zdotdir="${HOME}"
fi

if [[ -f "${__bmux_orig_zdotdir}/.zshenv" ]]; then
  source "${__bmux_orig_zdotdir}/.zshenv"
fi
"#
}

#[allow(clippy::literal_string_with_formatting_args)]
const fn shell_integration_zsh_rc() -> &'static str {
    r#"if [[ -n "${BMUX_ORIG_ZDOTDIR:-}" ]]; then
  __bmux_orig_zdotdir="${BMUX_ORIG_ZDOTDIR}"
else
  __bmux_orig_zdotdir="${HOME}"
fi

if [[ -f "${__bmux_orig_zdotdir}/.zshrc" ]]; then
  source "${__bmux_orig_zdotdir}/.zshrc"
fi

if [[ -z "${__BMUX_RESURRECTION_HOOK_INSTALLED:-}" ]]; then
  __BMUX_RESURRECTION_HOOK_INSTALLED=1

  __bmux_b64() {
    printf '%s' "$1" | base64 | tr -d '\r\n'
  }

  __bmux_emit_start() {
    local cmd_b64 cwd_b64
    cmd_b64="$(__bmux_b64 "$1")"
    cwd_b64="$(__bmux_b64 "$2")"
    printf '\033]633;bmux;start;%s;%s\007' "$cmd_b64" "$cwd_b64"
  }

  __bmux_emit_prompt() {
    local cwd_b64
    cwd_b64="$(__bmux_b64 "$1")"
    printf '\033]633;bmux;prompt;%s\007' "$cwd_b64"
  }

  function __bmux_preexec_hook() {
    [[ -n "${__BMUX_EMIT_GUARD:-}" ]] && return
    __BMUX_EMIT_GUARD=1
    if [[ -n "$1" ]]; then
      __bmux_emit_start "$1" "$PWD"
    fi
    __BMUX_EMIT_GUARD=
  }

  function __bmux_precmd_hook() {
    [[ -n "${__BMUX_EMIT_GUARD:-}" ]] && return
    __BMUX_EMIT_GUARD=1
    __bmux_emit_prompt "$PWD"
    __BMUX_EMIT_GUARD=
  }

  typeset -ga preexec_functions
  typeset -ga precmd_functions
  preexec_functions=(__bmux_preexec_hook ${preexec_functions:#__bmux_preexec_hook})
  precmd_functions=(__bmux_precmd_hook ${precmd_functions:#__bmux_precmd_hook})
fi
"#
}

#[allow(clippy::literal_string_with_formatting_args)]
const fn shell_integration_fish_config() -> &'static str {
    r#"set -l __bmux_orig_xdg "$BMUX_ORIG_XDG_CONFIG_HOME"
if test -z "$__bmux_orig_xdg"
  set __bmux_orig_xdg "$HOME/.config"
end

set -l __bmux_orig_fish_config "$__bmux_orig_xdg/fish/config.fish"
if test -f "$__bmux_orig_fish_config"
  source "$__bmux_orig_fish_config"
end

if not set -q __bmux_resurrection_hook_installed
  set -g __bmux_resurrection_hook_installed 1

  function __bmux_b64 --argument-names value
    printf '%s' "$value" | base64 | string replace -ra '\n|\r' ''
  end

  function __bmux_emit_start --argument-names cmd cwd
    set -l cmd_b64 (__bmux_b64 "$cmd")
    set -l cwd_b64 (__bmux_b64 "$cwd")
    printf '\e]633;bmux;start;%s;%s\a' "$cmd_b64" "$cwd_b64"
  end

  function __bmux_emit_prompt --argument-names cwd
    set -l cwd_b64 (__bmux_b64 "$cwd")
    printf '\e]633;bmux;prompt;%s\a' "$cwd_b64"
  end

  function __bmux_preexec --on-event fish_preexec
    set -l cmd (string join ' ' -- $argv)
    if test -n "$cmd"
      __bmux_emit_start "$cmd" "$PWD"
    end
  end

  function __bmux_prompt --on-event fish_prompt
    __bmux_emit_prompt "$PWD"
  end
end
"#
}

#[allow(clippy::literal_string_with_formatting_args)]
const fn shell_integration_nu_config() -> &'static str {
    r#"const __bmux_user_config_path = ($nu.default-config-dir | path join "config.nu")
const __bmux_user_config = if ($__bmux_user_config_path | path exists) { $__bmux_user_config_path } else { null }
source $__bmux_user_config

def __bmux_hook_list [value] {
  let kind = ($value | describe)
  if ($kind | str starts-with "list<") {
    $value
  } else if $kind == "nothing" {
    []
  } else {
    [$value]
  }
}

def __bmux_marker [kind: string, fields: list<string>] {
  let esc = (char -u "1b")
  let bel = (char bel)
  let encoded = ($fields | each {|field| $field | encode base64 } | str join ";")
  if (($encoded | str length) == 0) {
    $"($esc)]633;bmux;($kind)($bel)"
  } else {
    $"($esc)]633;bmux;($kind);($encoded)($bel)"
  }
}

def __bmux_emit_start [command: string, cwd: string] {
  print --no-newline (__bmux_marker "start" [$command $cwd])
}

def __bmux_prompt_marker [cwd: string] {
  __bmux_marker "prompt" [$cwd]
}

def __bmux_render_prompt_command [value] {
  let kind = ($value | describe)
  if ($kind | str starts-with "closure") {
    do $value
  } else if $kind == "nothing" {
    ""
  } else {
    $value | into string
  }
}

let __bmux_pre_execution_hooks = (__bmux_hook_list ($env.config | get -o hooks.pre_execution))
let __bmux_original_prompt_command = ($env | get -o PROMPT_COMMAND)

$env.PROMPT_COMMAND = {||
  let marker = (__bmux_prompt_marker ($env.PWD | into string))
  let prompt = (__bmux_render_prompt_command $__bmux_original_prompt_command)
  $"($marker)($prompt)"
}

$env.config = (
  $env.config
  | upsert hooks.pre_execution (
      $__bmux_pre_execution_hooks
      | append {||
          let command = (commandline)
          if (($command | str trim | str length) > 0) {
            __bmux_emit_start $command ($env.PWD | into string)
          }
        }
    )
)
"#
}

#[derive(Default)]
struct TerminalGridDeltaLog {
    batches: VecDeque<GridDeltaBatch>,
    #[cfg(test)]
    estimated_bytes: usize,
}

impl TerminalGridDeltaLog {
    #[cfg(test)]
    fn push(&mut self, delta: GridDeltaBatch) {
        let estimated = estimate_terminal_grid_delta_bytes(&delta);
        if estimated > MAX_TERMINAL_GRID_DELTA_BYTES {
            self.batches.clear();
            self.estimated_bytes = 0;
            return;
        }
        self.estimated_bytes = self.estimated_bytes.saturating_add(estimated);
        self.batches.push_back(delta);
        while self.batches.len() > MAX_TERMINAL_GRID_DELTA_BATCHES
            || self.estimated_bytes > MAX_TERMINAL_GRID_DELTA_BYTES
        {
            let Some(removed) = self.batches.pop_front() else {
                self.estimated_bytes = 0;
                break;
            };
            self.estimated_bytes = self
                .estimated_bytes
                .saturating_sub(estimate_terminal_grid_delta_bytes(&removed));
        }
    }

    fn iter(&self) -> impl Iterator<Item = &GridDeltaBatch> {
        self.batches.iter()
    }
}

fn estimate_terminal_grid_delta_bytes(delta: &GridDeltaBatch) -> usize {
    let row_bytes = delta
        .row_updates
        .iter()
        .map(|update| {
            std::mem::size_of_val(update)
                + update
                    .row
                    .runs
                    .iter()
                    .map(|run| std::mem::size_of_val(run) + run.text.len())
                    .sum::<usize>()
        })
        .sum::<usize>();
    std::mem::size_of_val(delta)
        + delta.mode.len()
        + delta.pending_bytes.len()
        + delta.styles.len() * std::mem::size_of::<bmux_terminal_grid::Style>()
        + row_bytes
}

fn select_terminal_grid_deltas(
    log: &TerminalGridDeltaLog,
    start: usize,
    max_batches: usize,
    response_budget: usize,
) -> Vec<GridDeltaBatch> {
    let mut selected = Vec::new();
    let mut estimated_bytes = 0_usize;
    for delta in log.iter().skip(start).take(max_batches.max(1)) {
        let delta_bytes = estimate_terminal_grid_delta_bytes(delta);
        if selected.is_empty() && delta_bytes > response_budget {
            break;
        }
        if !selected.is_empty() && estimated_bytes.saturating_add(delta_bytes) > response_budget {
            break;
        }
        estimated_bytes = estimated_bytes.saturating_add(delta_bytes);
        selected.push(delta.clone());
    }
    selected
}

#[cfg(test)]
fn push_terminal_grid_delta(
    deltas: &Arc<std::sync::Mutex<TerminalGridDeltaLog>>,
    delta: GridDeltaBatch,
) {
    if let Ok(mut deltas) = deltas.lock() {
        deltas.push(delta);
    }
}

impl PaneInputHandle {
    fn send_input(&self, data: Vec<u8>) -> std::result::Result<(), SessionRuntimeError> {
        if self.exited.load(Ordering::SeqCst) {
            return Err(SessionRuntimeError::Closed);
        }
        self.input_tx
            .send(PaneRuntimeCommand::Input(data))
            .map_err(|_| SessionRuntimeError::Closed)
    }
}

impl PaneRuntimeHandle {
    fn input_handle(&self, session_id: SessionId) -> PaneInputHandle {
        PaneInputHandle {
            session_id,
            input_tx: self.input_tx.clone(),
            exited: Arc::clone(&self.exited),
        }
    }

    fn send_input(&self, data: Vec<u8>) -> std::result::Result<(), SessionRuntimeError> {
        self.input_tx
            .send(PaneRuntimeCommand::Input(data))
            .map_err(|_| SessionRuntimeError::Closed)
    }

    fn resize_pty(&self, rows: u16, cols: u16) {
        if let Ok(mut last) = self.last_requested_size.lock() {
            *last = (rows, cols);
        }
        if let Ok(mut grid) = self.terminal_grid.lock() {
            let _ = grid.resize(cols, rows);
        }
        let _ = self
            .input_tx
            .send(PaneRuntimeCommand::Resize { rows, cols });
    }
}

#[derive(Clone)]
struct PaneAttachDataRef {
    pane_id: Uuid,
    name: Option<String>,
    exited: Arc<AtomicBool>,
    exit_reason: Arc<std::sync::Mutex<Option<String>>>,
    output_buffer: Arc<std::sync::Mutex<OutputFanoutBuffer>>,
    terminal_grid: Arc<std::sync::Mutex<TerminalGridStream>>,
    terminal_grid_deltas: Arc<std::sync::Mutex<TerminalGridDeltaLog>>,
    output_dirty: Arc<AtomicBool>,
    sync_update_in_progress: Arc<AtomicBool>,
    mouse_protocol_state: Arc<std::sync::Mutex<AttachMouseProtocolState>>,
    input_mode_state: Arc<std::sync::Mutex<AttachInputModeState>>,
    #[cfg(feature = "image-registry")]
    image_registry: Arc<std::sync::Mutex<bmux_image::ImageRegistry>>,
    #[cfg(feature = "image-registry")]
    image_dirty: Arc<AtomicBool>,
}

struct AttachSceneDataSnapshot {
    focused_pane_id: Uuid,
    panes: Vec<PaneAttachDataRef>,
    layout_root: bmux_attach_layout_protocol::PaneLayoutNode,
    scene: AttachScene,
    zoomed: bool,
}

#[derive(Clone)]
struct PanePersistenceDataRef {
    meta: PaneRuntimeMeta,
    exited: Arc<AtomicBool>,
    process_id: Arc<std::sync::Mutex<Option<u32>>>,
    process_group_id: Arc<std::sync::Mutex<Option<i32>>>,
    resurrection_state: Arc<std::sync::Mutex<PaneResurrectionRuntime>>,
}

struct PersistenceSnapshotParts {
    focused_pane_id: Uuid,
    layout_root: Option<PaneLayoutNode>,
    floating_surfaces: Vec<FloatingSurfaceRuntime>,
    attached_clients: BTreeSet<ClientId>,
    attach_viewport: Option<AttachViewport>,
    panes: Vec<PanePersistenceDataRef>,
}

fn pane_persistence_data_ref(pane: &PaneRuntimeHandle) -> PanePersistenceDataRef {
    PanePersistenceDataRef {
        meta: pane.meta.clone(),
        exited: Arc::clone(&pane.exited),
        process_id: Arc::clone(&pane.process_id),
        process_group_id: Arc::clone(&pane.process_group_id),
        resurrection_state: Arc::clone(&pane.resurrection_state),
    }
}

fn pane_attach_data_ref(
    _owner_session_id: SessionId,
    pane_id: Uuid,
    pane: &PaneRuntimeHandle,
) -> PaneAttachDataRef {
    PaneAttachDataRef {
        pane_id,
        name: pane.meta.name.clone(),
        exited: Arc::clone(&pane.exited),
        exit_reason: Arc::clone(&pane.exit_reason),
        output_buffer: Arc::clone(&pane.output_buffer),
        terminal_grid: Arc::clone(&pane.terminal_grid),
        terminal_grid_deltas: Arc::clone(&pane.terminal_grid_deltas),
        output_dirty: Arc::clone(&pane.output_dirty),
        sync_update_in_progress: Arc::clone(&pane.sync_update_in_progress),
        mouse_protocol_state: Arc::clone(&pane.mouse_protocol_state),
        input_mode_state: Arc::clone(&pane.input_mode_state),
        #[cfg(feature = "image-registry")]
        image_registry: Arc::clone(&pane.image_registry),
        #[cfg(feature = "image-registry")]
        image_dirty: Arc::clone(&pane.image_dirty),
    }
}

fn pane_summary_for_data_ref(
    index: usize,
    focused_pane_id: Uuid,
    pane: &PaneAttachDataRef,
) -> PaneSummary {
    PaneSummary {
        id: pane.pane_id,
        index: u32::try_from(index.saturating_add(1)).unwrap_or(u32::MAX),
        name: pane.name.clone(),
        focused: pane.pane_id == focused_pane_id,
        state: if pane.exited.load(Ordering::SeqCst) {
            PaneState::Exited
        } else {
            PaneState::Running
        },
        state_reason: pane
            .exit_reason
            .lock()
            .ok()
            .and_then(|reason| reason.clone()),
    }
}

fn attach_pane_chunk_for_client(
    pane: &PaneAttachDataRef,
    client_id: ClientId,
    max_bytes: usize,
) -> Result<AttachPaneChunk, SessionRuntimeError> {
    let read = pane
        .output_buffer
        .lock()
        .map_err(|_| SessionRuntimeError::Closed)?
        .read_for_client(client_id, max_bytes);
    Ok(AttachPaneChunk {
        pane_id: pane.pane_id,
        data: read.bytes,
        stream_start: read.stream_start,
        stream_end: read.stream_end,
        stream_gap: read.stream_gap,
        sync_update_active: pane.sync_update_in_progress.load(Ordering::SeqCst),
    })
}

fn attach_pane_recent_chunk(
    pane: &PaneAttachDataRef,
    client_id: ClientId,
    max_bytes: usize,
) -> Result<AttachPaneChunk, SessionRuntimeError> {
    let read = {
        let mut output = pane
            .output_buffer
            .lock()
            .map_err(|_| SessionRuntimeError::Closed)?;
        let read = output.read_recent_with_offsets(max_bytes);
        output.advance_client_to_end(client_id);
        read
    };
    pane.output_dirty.store(false, Ordering::SeqCst);
    Ok(AttachPaneChunk {
        pane_id: pane.pane_id,
        data: read.bytes,
        stream_start: read.stream_start,
        stream_end: read.stream_end,
        stream_gap: read.stream_gap,
        sync_update_active: pane.sync_update_in_progress.load(Ordering::SeqCst),
    })
}

impl SessionRuntimeManager {
    fn bump_attach_view_revision(&mut self, session_id: SessionId) -> Option<u64> {
        let runtime = self.runtimes.get_mut(&session_id)?;
        runtime.attach_view_revision = runtime.attach_view_revision.saturating_add(1);
        Some(runtime.attach_view_revision)
    }

    fn active_session_ids(&self) -> Vec<SessionId> {
        self.runtimes.keys().copied().collect()
    }

    fn reconcile_exited_pane_focus(&mut self, session_id: SessionId, pane_id: Uuid) -> bool {
        let Some(runtime) = self.runtimes.get_mut(&session_id) else {
            return false;
        };

        let exited_surface_scopes = runtime
            .floating_surfaces
            .iter()
            .filter(|surface| surface.pane_id == pane_id)
            .map(|surface| surface.scope)
            .collect::<Vec<_>>();
        let exited_was_floating = !exited_surface_scopes.is_empty();

        let floating_focus_is_exited = runtime
            .floating_surfaces
            .iter()
            .any(|surface| surface.pane_id == pane_id && surface.cursor_owner);

        for surface in &mut runtime.floating_surfaces {
            if surface.pane_id == pane_id {
                surface.cursor_owner = false;
            }
        }

        let focus_is_exited = runtime
            .panes
            .get(&runtime.focused_pane_id)
            .is_none_or(|pane| pane.exited.load(Ordering::SeqCst));
        if (runtime.focused_pane_id == pane_id || focus_is_exited || floating_focus_is_exited)
            && let Some(next_focus) = next_live_focus_after_exit(runtime, pane_id)
        {
            runtime.focused_pane_id = next_focus;
            for surface in &mut runtime.floating_surfaces {
                surface.cursor_owner = surface.pane_id == next_focus;
            }
        }

        exited_was_floating
            && exited_surface_scopes.iter().any(|scope| {
                matches!(
                    scope,
                    FloatingPaneScope::ClientGlobal | FloatingPaneScope::ServerGlobal
                )
            })
    }
}

/// Lightweight ECMA-48 escape sequence phase tracker.
///
/// Classifies each byte of a terminal output stream as either part of normal
/// ground-state text or inside an escape sequence (CSI, OSC, DCS, etc.).
/// Used by [`OutputFanoutBuffer`] to record safe resume boundaries so that
/// [`OutputFanoutBuffer::read_recent`] never returns bytes starting mid-sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscSeqPhase {
    /// Normal text and C0 controls — safe for a fresh parser to start here.
    Ground,
    /// Saw ESC (0x1B); next byte determines the sequence type.
    Escape,
    /// Inside a CSI sequence (ESC `[` …); ends on a final byte 0x40–0x7E.
    Csi,
    /// Inside an OSC string (ESC `]` …); ends on BEL (0x07) or ST (ESC `\`).
    Osc,
    /// Saw ESC inside an OSC body — looking for `\` to complete ST.
    OscEsc,
    /// Inside a DCS passthrough (ESC `P` …); ends on ST (ESC `\`).
    Dcs,
    /// Saw ESC inside a DCS body — looking for `\` to complete ST.
    DcsEsc,
    /// Inside an SOS, PM, or APC string (ESC `X`/`^`/`_` …); ends on ST.
    Sos,
    /// Saw ESC inside an SOS/PM/APC body — looking for `\` to complete ST.
    SosEsc,
}

impl EscSeqPhase {
    /// Advance the state machine by one byte, returning the new phase.
    #[inline]
    const fn advance(self, byte: u8) -> Self {
        // CAN (0x18) and SUB (0x1A) abort any sequence from any state.
        if byte == 0x18 || byte == 0x1A {
            return Self::Ground;
        }
        match self {
            Self::Ground => {
                if byte == 0x1B {
                    Self::Escape
                } else {
                    Self::Ground
                }
            }
            Self::Escape => match byte {
                b'[' => Self::Csi,
                b']' => Self::Osc,
                b'P' => Self::Dcs,
                b'X' | b'^' | b'_' => Self::Sos,
                // Intermediate bytes (0x20–0x2F) stay in Escape (ESC intermediate
                // sequence).  ESC restarts.
                0x1B | 0x20..=0x2F => Self::Escape,
                // Final bytes (0x30–0x7E) complete a two-byte escape.
                // Everything else also returns to Ground.
                _ => Self::Ground,
            },
            Self::Csi => match byte {
                // Final byte completes the CSI sequence.
                0x40..=0x7E => Self::Ground,
                // ESC inside CSI aborts it and starts a new escape.
                0x1B => Self::Escape,
                // Parameter bytes (0x30–0x3F) and intermediate bytes (0x20–0x2F)
                // continue the sequence.  Anything else also stays in CSI (tolerant
                // parsing, matching xterm behavior for invalid bytes).
                _ => Self::Csi,
            },
            Self::Osc => match byte {
                0x07 => Self::Ground, // BEL terminates OSC
                0x1B => Self::OscEsc,
                _ => Self::Osc,
            },
            Self::OscEsc => match byte {
                b'\\' => Self::Ground, // ST (ESC \) terminates OSC
                0x1B => Self::Escape,  // nested ESC aborts OSC
                _ => Self::Osc,        // false alarm, back to body
            },
            Self::Dcs => match byte {
                0x1B => Self::DcsEsc,
                _ => Self::Dcs,
            },
            Self::DcsEsc => match byte {
                b'\\' => Self::Ground,
                0x1B => Self::Escape,
                _ => Self::Dcs,
            },
            Self::Sos => match byte {
                0x1B => Self::SosEsc,
                _ => Self::Sos,
            },
            Self::SosEsc => match byte {
                b'\\' => Self::Ground,
                0x1B => Self::Escape,
                _ => Self::Sos,
            },
        }
    }

    const fn is_ground(self) -> bool {
        matches!(self, Self::Ground)
    }
}

struct OutputFanoutBuffer {
    max_bytes: usize,
    start_offset: u64,
    data: VecDeque<u8>,
    cursors: BTreeMap<ClientId, u64>,
    /// Running escape-sequence phase at the end of the buffer.
    esc_phase: EscSeqPhase,
    /// Escape-sequence spans: `(esc_start, safe_resume)` pairs where
    /// `esc_start` is the offset of the ESC byte that began a sequence and
    /// `safe_resume` is the first offset after the sequence completed
    /// (Ground state).  An open (incomplete) span has `safe_resume == u64::MAX`.
    /// Sorted ascending by `esc_start`.
    esc_spans: VecDeque<(u64, u64)>,
}

struct OutputRead {
    bytes: Vec<u8>,
    stream_start: u64,
    stream_end: u64,
    stream_gap: bool,
}

impl OutputFanoutBuffer {
    fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes: max_bytes.max(1),
            start_offset: 0,
            data: VecDeque::new(),
            cursors: BTreeMap::new(),
            esc_phase: EscSeqPhase::Ground,
            esc_spans: VecDeque::new(),
        }
    }

    fn end_offset(&self) -> u64 {
        self.start_offset + self.data.len() as u64
    }

    fn register_client_at_tail(&mut self, client_id: ClientId) {
        self.cursors.insert(client_id, self.end_offset());
    }

    fn unregister_client(&mut self, client_id: ClientId) {
        self.cursors.remove(&client_id);
    }

    fn set_client_cursor(&mut self, client_id: ClientId, offset: u64) {
        let clamped = offset.clamp(self.start_offset, self.end_offset());
        self.cursors.insert(client_id, clamped);
    }

    fn push_chunk(&mut self, chunk: &[u8]) {
        let base_offset = self.end_offset();
        self.data.extend(chunk.iter().copied());

        // Track escape-sequence phase for every byte.  Record spans
        // so that `first_safe_offset_at_or_after` can determine whether
        // any position is inside an escape sequence.
        //
        // The ESC byte itself is a safe start — a fresh ground-state parser
        // correctly handles it.  The unsafe region begins at the byte AFTER
        // the ESC (e.g. the `[` in CSI, the `]` in OSC) and extends to the
        // byte after the final/terminator byte.
        for (i, &byte) in chunk.iter().enumerate() {
            let prev = self.esc_phase;
            self.esc_phase = prev.advance(byte);

            if prev.is_ground() && !self.esc_phase.is_ground() {
                // Ground → non-Ground: open a new span starting AFTER the ESC byte.
                self.esc_spans
                    .push_back((base_offset + i as u64 + 1, u64::MAX));
            } else if !prev.is_ground() && self.esc_phase.is_ground() {
                // non-Ground → Ground: close the current span.  The safe
                // resume point is the byte after the final/terminator byte.
                if let Some(last) = self.esc_spans.back_mut()
                    && last.1 == u64::MAX
                {
                    last.1 = base_offset + i as u64 + 1;
                }
            }
        }

        while self.data.len() > self.max_bytes {
            let _ = self.data.pop_front();
            self.start_offset = self.start_offset.saturating_add(1);
        }

        // Prune spans that are entirely before start_offset.
        while let Some(&(_, safe_resume)) = self.esc_spans.front() {
            if safe_resume != u64::MAX && safe_resume <= self.start_offset {
                self.esc_spans.pop_front();
            } else {
                break;
            }
        }

        // Do not mutate per-client cursors here.  `read_for_client` performs
        // clamping and reports `stream_gap` so clients can recover parser
        // continuity with explicit metadata.
    }

    fn read_for_client(&mut self, client_id: ClientId, max_bytes: usize) -> OutputRead {
        let limit = max_bytes.max(1);
        let end = self.end_offset();

        // Pre-compute the safe resume position before borrowing cursors
        // mutably, since first_ground_boundary_at_or_after borrows self
        // immutably.
        let safe_resume = self.first_safe_offset_at_or_after(self.start_offset);

        let cursor = self.cursors.entry(client_id).or_insert(end);

        let stream_gap = if *cursor < self.start_offset {
            // Bytes were evicted before the client could read them.  Advance
            // the cursor to the nearest safe position so the client
            // never receives bytes starting mid-escape-sequence.
            *cursor = safe_resume;
            true
        } else {
            false
        };

        let stream_start = *cursor;

        #[allow(clippy::cast_possible_truncation)]
        let available = end.saturating_sub(*cursor) as usize;
        if available == 0 {
            return OutputRead {
                bytes: Vec::new(),
                stream_start,
                stream_end: stream_start,
                stream_gap,
            };
        }

        let to_read = available.min(limit);
        #[allow(clippy::cast_possible_truncation)]
        let start_index = (*cursor - self.start_offset) as usize;
        let bytes = self
            .data
            .iter()
            .skip(start_index)
            .take(to_read)
            .copied()
            .collect::<Vec<_>>();
        *cursor = cursor.saturating_add(bytes.len() as u64);

        OutputRead {
            bytes,
            stream_start,
            stream_end: *cursor,
            stream_gap,
        }
    }

    fn read_recent_with_offsets(&self, max_bytes: usize) -> OutputRead {
        let end = self.end_offset();
        if self.data.is_empty() || max_bytes == 0 {
            return OutputRead {
                bytes: Vec::new(),
                stream_start: end,
                stream_end: end,
                stream_gap: false,
            };
        }
        let to_read = self.data.len().min(max_bytes);
        let intended_start = end - to_read as u64;
        let safe_start = self.first_safe_offset_at_or_after(intended_start);

        if safe_start >= end {
            return OutputRead {
                bytes: Vec::new(),
                stream_start: end,
                stream_end: end,
                stream_gap: false,
            };
        }

        #[allow(clippy::cast_possible_truncation)]
        let start_index = (safe_start - self.start_offset) as usize;
        OutputRead {
            bytes: self.data.iter().skip(start_index).copied().collect(),
            stream_start: safe_start,
            stream_end: end,
            stream_gap: false,
        }
    }

    /// Return the first stream offset >= `target` where a fresh ground-state
    /// parser can safely start consuming bytes.
    ///
    /// Checks the escape-sequence span list to determine whether `target`
    /// falls inside an open span.  If so, advances to the span's
    /// `safe_resume` offset.
    fn first_safe_offset_at_or_after(&self, target: u64) -> u64 {
        // Find the span that could contain `target`.  We need the latest
        // span whose esc_start <= target.
        //
        // Binary search by esc_start (the first element of each tuple).
        let idx = self
            .esc_spans
            .binary_search_by(|&(esc_start, _)| esc_start.cmp(&target))
            .unwrap_or_else(|insert_point| insert_point.saturating_sub(1));

        // Check a small window of spans around the search result.  Due to
        // binary_search edge cases with saturating_sub, check idx and idx+1.
        for check in idx..self.esc_spans.len().min(idx + 2) {
            let (esc_start, safe_resume) = self.esc_spans[check];
            if esc_start <= target && target < safe_resume {
                // `target` falls inside this escape sequence.
                if safe_resume == u64::MAX {
                    // Sequence is still open (not yet terminated).
                    return self.end_offset();
                }
                return safe_resume;
            }
        }

        // `target` is not inside any escape sequence — it's in Ground state.
        target
    }

    /// Advance an existing client's read cursor to the end of the buffer,
    /// so the next `read_for_client` call only returns data written after
    /// this point. Used after snapshot reads to avoid re-delivering bytes
    /// the client already received via `read_recent`.
    fn advance_client_to_end(&mut self, client_id: ClientId) {
        let end = self.end_offset();
        if let Some(cursor) = self.cursors.get_mut(&client_id) {
            *cursor = end;
        }
    }
}

struct RemovedRuntime {
    session_id: SessionId,
    handle: SessionRuntimeHandle,
}

// Retained as manager-local DTOs for legacy helper methods that are still useful
// in tests and during the ongoing runtime-store split.
#[allow(dead_code)]
struct AttachLayoutState {
    focused_pane_id: Uuid,
    panes: Vec<PaneSummary>,
    layout_root: IpcPaneLayoutNode,
    scene: AttachScene,
    zoomed: bool,
}

// Retained as manager-local DTOs for legacy helper methods that are still useful
// in tests and during the ongoing runtime-store split.
#[allow(dead_code)]
struct AttachSnapshotState {
    focused_pane_id: Uuid,
    panes: Vec<PaneSummary>,
    layout_root: IpcPaneLayoutNode,
    scene: AttachScene,
    chunks: Vec<AttachPaneChunk>,
    pane_mouse_protocols: Vec<AttachPaneMouseProtocol>,
    pane_input_modes: Vec<AttachPaneInputMode>,
    zoomed: bool,
}

// Retained as manager-local DTOs for legacy helper methods that are still useful
// in tests and during the ongoing runtime-store split.
#[allow(dead_code)]
struct AttachPaneSnapshotState {
    chunks: Vec<AttachPaneChunk>,
    pane_mouse_protocols: Vec<AttachPaneMouseProtocol>,
    pane_input_modes: Vec<AttachPaneInputMode>,
}

fn ipc_layout_from_runtime(node: &PaneLayoutNode) -> IpcPaneLayoutNode {
    match node {
        PaneLayoutNode::Leaf { pane_id } => IpcPaneLayoutNode::Leaf { pane_id: *pane_id },
        PaneLayoutNode::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            let percent = (ratio * 100.0).round();
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let ratio_percent = percent.clamp(10.0, 90.0) as u8;
            IpcPaneLayoutNode::Split {
                direction: *direction,
                ratio_percent,
                first: Box::new(ipc_layout_from_runtime(first)),
                second: Box::new(ipc_layout_from_runtime(second)),
            }
        }
    }
}

fn collect_runtime_layout_pane_ids(node: &PaneLayoutNode, out: &mut BTreeSet<Uuid>) -> Result<()> {
    match node {
        PaneLayoutNode::Leaf { pane_id } => {
            if !out.insert(*pane_id) {
                anyhow::bail!("duplicate pane id {pane_id} in runtime layout")
            }
        }
        PaneLayoutNode::Split {
            ratio,
            first,
            second,
            ..
        } => {
            if !(0.1..=0.9).contains(ratio) {
                anyhow::bail!("runtime split ratio {ratio} out of range [0.1, 0.9]")
            }
            collect_runtime_layout_pane_ids(first, out)?;
            collect_runtime_layout_pane_ids(second, out)?;
        }
    }
    Ok(())
}

fn floating_pane_ids(runtime: &SessionRuntimeHandle) -> BTreeSet<Uuid> {
    runtime
        .floating_surfaces
        .iter()
        .map(|surface| surface.pane_id)
        .collect()
}

fn tiled_pane_ids(runtime: &SessionRuntimeHandle) -> BTreeSet<Uuid> {
    let floating = floating_pane_ids(runtime);
    runtime
        .panes
        .keys()
        .copied()
        .filter(|pane_id| !floating.contains(pane_id))
        .collect()
}

fn ordered_tiled_pane_ids(runtime: &SessionRuntimeHandle) -> Vec<Uuid> {
    let mut pane_ids = Vec::new();
    if let Some(layout_root) = &runtime.layout_root {
        layout_root.pane_order(&mut pane_ids);
    }
    pane_ids
}

fn ordered_pane_ids(runtime: &SessionRuntimeHandle) -> Vec<Uuid> {
    let mut pane_ids = ordered_tiled_pane_ids(runtime);
    for surface in &runtime.floating_surfaces {
        if runtime.panes.contains_key(&surface.pane_id) && !pane_ids.contains(&surface.pane_id) {
            pane_ids.push(surface.pane_id);
        }
    }
    pane_ids
}

fn fallback_ipc_layout(runtime: &SessionRuntimeHandle) -> IpcPaneLayoutNode {
    runtime.layout_root.as_ref().map_or_else(
        || IpcPaneLayoutNode::Leaf {
            pane_id: runtime.focused_pane_id,
        },
        ipc_layout_from_runtime,
    )
}

fn floating_summary(surface: FloatingSurfaceRuntime) -> FloatingPaneRuntimeSummary {
    FloatingPaneRuntimeSummary {
        id: surface.id,
        pane_id: surface.pane_id,
        anchor_pane_id: surface.anchor_pane_id,
        context_id: surface.context_id,
        client_id: surface.client_id,
        rect: surface.rect,
        scope: surface.scope,
        layer: surface.layer,
        z: surface.z,
        visible: surface.visible,
        opaque: surface.opaque,
        accepts_input: surface.accepts_input,
        cursor_owner: surface.cursor_owner,
    }
}

fn validate_runtime_layout_matches_panes(runtime: &SessionRuntimeHandle) -> Result<()> {
    let pane_ids = tiled_pane_ids(runtime);
    let mut layout_ids = BTreeSet::new();
    if let Some(layout_root) = &runtime.layout_root {
        collect_runtime_layout_pane_ids(layout_root, &mut layout_ids)?;
    }
    if pane_ids != layout_ids {
        anyhow::bail!(
            "runtime layout panes do not match tiled runtime pane map (layout: {}, panes: {})",
            layout_ids.len(),
            pane_ids.len()
        )
    }
    Ok(())
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn split_layout_rect(rect: LayoutRect, ratio: f32, vertical: bool) -> (LayoutRect, LayoutRect) {
    let ratio = ratio.clamp(0.1, 0.9);
    if vertical {
        let split = ((f32::from(rect.w) * ratio).round()) as u16;
        let first_w = split.max(1).min(rect.w.saturating_sub(1).max(1));
        let second_w = rect.w.saturating_sub(first_w).max(1);
        (
            LayoutRect {
                x: rect.x,
                y: rect.y,
                w: first_w,
                h: rect.h,
            },
            LayoutRect {
                x: rect.x.saturating_add(first_w),
                y: rect.y,
                w: second_w,
                h: rect.h,
            },
        )
    } else {
        let split = ((f32::from(rect.h) * ratio).round()) as u16;
        let first_h = split.max(1).min(rect.h.saturating_sub(1).max(1));
        let second_h = rect.h.saturating_sub(first_h).max(1);
        (
            LayoutRect {
                x: rect.x,
                y: rect.y,
                w: rect.w,
                h: first_h,
            },
            LayoutRect {
                x: rect.x,
                y: rect.y.saturating_add(first_h),
                w: rect.w,
                h: second_h,
            },
        )
    }
}

fn collect_layout_rects(
    node: &PaneLayoutNode,
    rect: LayoutRect,
    out: &mut BTreeMap<Uuid, LayoutRect>,
) {
    match node {
        PaneLayoutNode::Leaf { pane_id } => {
            out.insert(*pane_id, rect);
        }
        PaneLayoutNode::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            let vertical = matches!(direction, PaneSplitDirection::Vertical);
            let (first_rect, second_rect) = split_layout_rect(rect, *ratio, vertical);
            collect_layout_rects(first, first_rect, out);
            collect_layout_rects(second, second_rect, out);
        }
    }
}

fn layout_order_fallback_after_close(order: &[Uuid], target: Uuid) -> Option<Uuid> {
    let target_index = order.iter().position(|id| *id == target)?;
    order
        .iter()
        .copied()
        .filter(|id| *id != target)
        .nth(target_index)
        .or_else(|| {
            order
                .iter()
                .copied()
                .take(target_index)
                .rfind(|id| *id != target)
        })
}

fn rect_center(rect: LayoutRect) -> (u32, u32) {
    (
        u32::from(rect.x).saturating_mul(2) + u32::from(rect.w),
        u32::from(rect.y).saturating_mul(2) + u32::from(rect.h),
    )
}

fn rect_center_distance(a: LayoutRect, b: LayoutRect) -> u32 {
    let (ax, ay) = rect_center(a);
    let (bx, by) = rect_center(b);
    ax.abs_diff(bx).saturating_add(ay.abs_diff(by))
}

fn next_focus_after_close(
    layout_root: &PaneLayoutNode,
    focused_pane_id: Uuid,
    target: Uuid,
    viewport: Option<AttachViewport>,
) -> Option<Uuid> {
    let mut order = Vec::new();
    layout_root.pane_order(&mut order);
    if !order.contains(&target) {
        return None;
    }
    if focused_pane_id != target && order.contains(&focused_pane_id) {
        return Some(focused_pane_id);
    }

    let fallback = layout_order_fallback_after_close(&order, target);
    let root = scene_root_from_viewport(viewport);
    let mut rects = BTreeMap::new();
    collect_layout_rects(layout_root, root, &mut rects);
    let target_rect = rects.get(&target).copied()?;
    let target_index = order.iter().position(|id| *id == target).unwrap_or(0);

    rects
        .iter()
        .filter(|(pane_id, _)| **pane_id != target)
        .min_by_key(|(pane_id, rect)| {
            let order_index = order
                .iter()
                .position(|id| id == *pane_id)
                .unwrap_or(usize::MAX);
            (
                rect_center_distance(target_rect, **rect),
                order_index.abs_diff(target_index),
                order_index,
            )
        })
        .map(|(pane_id, _)| *pane_id)
        .or(fallback)
}

const fn attach_rect_from_layout_rect(rect: LayoutRect) -> AttachRect {
    AttachRect {
        x: rect.x,
        y: rect.y,
        w: rect.w,
        h: rect.h,
    }
}

/// Compute the `content_rect` for a pane surface that reserves a
/// 1-cell inset on all sides for decoration chrome (borders, badges).
///
/// Every decoration consumer (the decoration plugin today; future
/// overlay / chrome plugins later) lays out within this content rect.
/// Panes smaller than 2 cells in either dimension have no interior
/// and fall back to the full rect so downstream consumers don't see
/// a zero-sized content rect.
const fn pane_content_rect_for_outer(rect: AttachRect) -> AttachRect {
    if rect.w < 2 || rect.h < 2 {
        return rect;
    }
    AttachRect {
        x: rect.x + 1,
        y: rect.y + 1,
        w: rect.w - 2,
        h: rect.h - 2,
    }
}

fn scene_root_from_viewport(viewport: Option<AttachViewport>) -> LayoutRect {
    let (cols, rows, status_top_inset, status_bottom_inset) =
        viewport.map_or((0, 0, 0, 0), |viewport| {
            (
                viewport.cols,
                viewport.rows,
                viewport.status_top_inset,
                viewport.status_bottom_inset,
            )
        });
    let y = status_top_inset.min(rows.saturating_sub(1));
    let reserved = status_top_inset.saturating_add(status_bottom_inset);
    let h = rows.saturating_sub(reserved).max(1);
    LayoutRect {
        x: 0,
        y,
        w: cols.max(1),
        h,
    }
}

fn floating_surface_visible_for_attach(
    owner_session_id: SessionId,
    attach_session_id: SessionId,
    runtime: &SessionRuntimeHandle,
    surface: &FloatingSurfaceRuntime,
    client_id: ClientId,
    context_id: Option<Uuid>,
) -> bool {
    if !runtime.panes.contains_key(&surface.pane_id) {
        return false;
    }
    match surface.scope {
        FloatingPaneScope::PerPane => {
            owner_session_id == attach_session_id
                && surface
                    .anchor_pane_id
                    .is_some_and(|anchor| runtime.panes.contains_key(&anchor))
        }
        FloatingPaneScope::PerWindow => {
            owner_session_id == attach_session_id
                && (surface.context_id.is_none() || surface.context_id == context_id)
        }
        FloatingPaneScope::PerSession => owner_session_id == attach_session_id,
        FloatingPaneScope::ClientGlobal => surface.client_id.is_none_or(|owner| owner == client_id),
        FloatingPaneScope::ServerGlobal => true,
    }
}

fn attach_surface_from_floating(surface: &FloatingSurfaceRuntime) -> AttachSurface {
    let rect = attach_rect_from_layout_rect(surface.rect);
    AttachSurface {
        id: surface.id,
        kind: AttachSurfaceKind::FloatingPane,
        layer: surface.layer.to_attach_layer(),
        z: surface.z,
        rect,
        content_rect: pane_content_rect_for_outer(rect),
        interactive_regions: Vec::new(),
        opaque: surface.opaque,
        visible: surface.visible,
        accepts_input: surface.accepts_input,
        cursor_owner: surface.cursor_owner,
        pane_id: Some(surface.pane_id),
    }
}

fn focused_pane_for_scene(scene: &AttachScene, fallback: Uuid) -> Uuid {
    let mut best: Option<(AttachLayer, i32, usize, Uuid)> = None;
    for (index, surface) in scene.surfaces.iter().enumerate() {
        let Some(pane_id) = surface.pane_id else {
            continue;
        };
        if !surface.visible || !surface.accepts_input || !surface.cursor_owner {
            continue;
        }
        let candidate = (surface.layer, surface.z, index, pane_id);
        if best.as_ref().is_none_or(|current| {
            (candidate.0, candidate.1, candidate.2) > (current.0, current.1, current.2)
        }) {
            best = Some(candidate);
        }
    }
    best.map_or(fallback, |(_, _, _, pane_id)| pane_id)
}

fn runtime_pane_is_live(runtime: &SessionRuntimeHandle, pane_id: Uuid) -> bool {
    runtime
        .panes
        .get(&pane_id)
        .is_some_and(|pane| !pane.exited.load(Ordering::SeqCst))
}

fn next_live_focus_after_exit(
    runtime: &SessionRuntimeHandle,
    exited_pane_id: Uuid,
) -> Option<Uuid> {
    let floating_focus = runtime
        .floating_surfaces
        .iter()
        .enumerate()
        .filter(|(_, surface)| {
            surface.pane_id != exited_pane_id
                && surface.visible
                && surface.accepts_input
                && runtime_pane_is_live(runtime, surface.pane_id)
        })
        .max_by_key(|(index, surface)| (surface.layer.to_attach_layer(), surface.z, *index))
        .map(|(_, surface)| surface.pane_id);

    floating_focus
        .or_else(|| {
            ordered_tiled_pane_ids(runtime).into_iter().find(|pane_id| {
                *pane_id != exited_pane_id && runtime_pane_is_live(runtime, *pane_id)
            })
        })
        .or_else(|| {
            runtime.panes.keys().copied().find(|pane_id| {
                *pane_id != exited_pane_id && runtime_pane_is_live(runtime, *pane_id)
            })
        })
}

// Building the attach scene requires constructing every surface's
// `rect` + `content_rect` + `interactive_regions` literally; splitting
// this further would hurt readability more than it helps.
#[allow(clippy::too_many_lines)]
fn build_attach_scene(
    session_id: SessionId,
    runtime: &SessionRuntimeHandle,
    viewport: Option<AttachViewport>,
    client_id: ClientId,
    context_id: Option<Uuid>,
) -> AttachScene {
    let scene_root = scene_root_from_viewport(viewport);

    // When a pane is zoomed, produce a single-pane scene that fills the viewport.
    if let Some(zoomed_id) = runtime.zoomed_pane_id
        && runtime.panes.contains_key(&zoomed_id)
    {
        let zoomed_rect = attach_rect_from_layout_rect(scene_root);
        let zoomed_surface = AttachSurface {
            id: zoomed_id,
            kind: AttachSurfaceKind::Pane,
            layer: AttachLayer::Pane,
            z: 0,
            rect: zoomed_rect,
            content_rect: pane_content_rect_for_outer(zoomed_rect),
            interactive_regions: Vec::new(),
            opaque: true,
            visible: true,
            accepts_input: true,
            cursor_owner: true,
            pane_id: Some(zoomed_id),
        };

        let mut surfaces = vec![zoomed_surface];

        // Floating surfaces still render on top of the zoomed pane.
        surfaces.extend(
            runtime
                .floating_surfaces
                .iter()
                .filter(|surface| {
                    floating_surface_visible_for_attach(
                        session_id, session_id, runtime, surface, client_id, context_id,
                    )
                })
                .map(attach_surface_from_floating),
        );

        let scene = AttachScene {
            session_id: session_id.0,
            focus: AttachFocusTarget::Pane { pane_id: zoomed_id },
            surfaces,
        };
        let pane_id = focused_pane_for_scene(&scene, zoomed_id);
        return AttachScene {
            focus: AttachFocusTarget::Pane { pane_id },
            ..scene
        };
    }
    // Zoomed pane was removed; fall through to normal rendering.

    let mut rects = BTreeMap::new();
    if let Some(layout_root) = &runtime.layout_root {
        collect_layout_rects(layout_root, scene_root, &mut rects);
    }

    let pane_ids = ordered_tiled_pane_ids(runtime);

    let mut surfaces = pane_ids
        .into_iter()
        .enumerate()
        .filter_map(|(index, pane_id)| {
            rects.get(&pane_id).copied().map(|rect| {
                let attach_rect = attach_rect_from_layout_rect(rect);
                AttachSurface {
                    id: pane_id,
                    kind: AttachSurfaceKind::Pane,
                    layer: AttachLayer::Pane,
                    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
                    z: index as i32,
                    rect: attach_rect,
                    content_rect: pane_content_rect_for_outer(attach_rect),
                    interactive_regions: Vec::new(),
                    opaque: true,
                    visible: true,
                    accepts_input: true,
                    cursor_owner: pane_id == runtime.focused_pane_id,
                    pane_id: Some(pane_id),
                }
            })
        })
        .collect::<Vec<_>>();

    surfaces.extend(
        runtime
            .floating_surfaces
            .iter()
            .filter(|surface| {
                floating_surface_visible_for_attach(
                    session_id, session_id, runtime, surface, client_id, context_id,
                )
            })
            .map(attach_surface_from_floating),
    );

    let scene = AttachScene {
        session_id: session_id.0,
        focus: AttachFocusTarget::Pane {
            pane_id: runtime.focused_pane_id,
        },
        surfaces,
    };
    let pane_id = focused_pane_for_scene(&scene, runtime.focused_pane_id);
    AttachScene {
        focus: AttachFocusTarget::Pane { pane_id },
        ..scene
    }
}

fn pane_pty_size(layout_rect: LayoutRect) -> (u16, u16) {
    // PTY size must match the surface's `content_rect` so that what the
    // program inside the pane draws aligns with what the renderer/mouse
    // hit-tester sees. We route through the same helper that computes
    // `content_rect` in scene construction to keep them consistent.
    let content = pane_content_rect_for_outer(attach_rect_from_layout_rect(layout_rect));
    let cols = content.w.max(1);
    let rows = content.h.max(1);
    (rows, cols)
}

fn resize_session_ptys(
    runtime: &SessionRuntimeHandle,
    cols: u16,
    rows: u16,
    status_top_inset: u16,
    status_bottom_inset: u16,
) {
    let y = status_top_inset.min(rows.saturating_sub(1));
    let reserved = status_top_inset.saturating_add(status_bottom_inset);
    let root = LayoutRect {
        x: 0,
        y,
        w: cols.max(1),
        h: rows.saturating_sub(reserved).max(1),
    };

    // When zoomed, only resize the zoomed pane to fill the viewport.
    if let Some(zoomed_id) = runtime.zoomed_pane_id {
        if let Some(pane) = runtime.panes.get(&zoomed_id)
            && !pane.exited.load(Ordering::SeqCst)
        {
            let (zoom_rows, zoom_cols) = pane_pty_size(root);
            pane.resize_pty(zoom_rows, zoom_cols);
        }
        return;
    }

    let mut rects = BTreeMap::new();
    if let Some(layout_root) = &runtime.layout_root {
        collect_layout_rects(layout_root, root, &mut rects);
    }
    for (pane_id, pane) in &runtime.panes {
        if pane.exited.load(Ordering::SeqCst) {
            continue;
        }
        let rect = rects.get(pane_id).copied().or_else(|| {
            runtime
                .floating_surfaces
                .iter()
                .find(|surface| surface.pane_id == *pane_id)
                .map(|surface| surface.rect)
        });
        if let Some(rect) = rect {
            let (rows, cols) = pane_pty_size(rect);
            pane.resize_pty(rows, cols);
        }
    }
}

fn layout_from_panes(panes: &[PaneRuntimeMeta]) -> Option<PaneLayoutNode> {
    let mut iter = panes.iter();
    let first = iter.next()?;
    let mut root = PaneLayoutNode::Leaf { pane_id: first.id };
    for pane in iter {
        root = PaneLayoutNode::Split {
            direction: PaneSplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(root),
            second: Box::new(PaneLayoutNode::Leaf { pane_id: pane.id }),
        };
    }
    Some(root)
}

fn pane_launch_spec_from_command(command: PaneLaunchCommand) -> Result<PaneLaunchSpec> {
    if command.program.trim().is_empty() {
        anyhow::bail!("pane launch command program cannot be empty");
    }
    Ok(PaneLaunchSpec {
        program: command.program,
        args: command.args,
        cwd: command.cwd,
        env: command.env,
    })
}

impl SessionRuntimeManager {
    #[allow(clippy::too_many_arguments)]
    fn new(
        shell: String,
        pane_term: String,
        protocol_profile: ProtocolProfile,
        shell_integration_root: Option<std::path::PathBuf>,
        pane_exit_tx: mpsc::UnboundedSender<PaneExitEvent>,
    ) -> Self {
        Self {
            runtimes: BTreeMap::new(),
            pane_input_index: Arc::new(RwLock::new(BTreeMap::new())),
            client_write_permissions: Arc::new(RwLock::new(BTreeMap::new())),
            shell,
            pane_term,
            protocol_profile,
            shell_integration_root,
            pane_exit_tx,
        }
    }

    fn register_pane_input(&self, session_id: SessionId, pane: &PaneRuntimeHandle) {
        if let Ok(mut index) = self.pane_input_index.write() {
            index.insert(pane.meta.id, pane.input_handle(session_id));
        }
    }

    fn unregister_pane_input(&self, pane_id: Uuid) {
        if let Ok(mut index) = self.pane_input_index.write() {
            index.remove(&pane_id);
        }
    }

    fn unregister_session_inputs(
        &self,
        session_id: SessionId,
        pane_ids: impl IntoIterator<Item = Uuid>,
    ) {
        if let Ok(mut index) = self.pane_input_index.write() {
            for pane_id in pane_ids {
                if index
                    .get(&pane_id)
                    .is_some_and(|handle| handle.session_id == session_id)
                {
                    index.remove(&pane_id);
                }
            }
        }
        if let Ok(mut permissions) = self.client_write_permissions.write() {
            permissions.remove(&session_id);
        }
    }

    fn start_runtime(&mut self, session_id: SessionId) -> Result<()> {
        if self.runtimes.contains_key(&session_id) {
            anyhow::bail!("runtime already exists for session {}", session_id.0);
        }

        let first_pane_id = Uuid::new_v4();
        let pane_meta = PaneRuntimeMeta {
            id: first_pane_id,
            name: Some("pane-1".to_string()),
            shell: self.shell.clone(),
            launch: None,
            resurrection: PaneResurrectionSnapshot::default(),
        };
        let first_pane = self.spawn_pane_runtime(session_id, pane_meta, (24, 80));
        self.register_pane_input(session_id, &first_pane);
        let mut panes = BTreeMap::new();
        panes.insert(first_pane_id, first_pane);

        self.runtimes.insert(
            session_id,
            SessionRuntimeHandle {
                panes,
                layout_root: Some(PaneLayoutNode::Leaf {
                    pane_id: first_pane_id,
                }),
                focused_pane_id: first_pane_id,
                zoomed_pane_id: None,
                floating_surfaces: Vec::new(),
                attached_clients: BTreeSet::new(),
                attach_viewport: None,
                attach_view_revision: 0,
            },
        );
        Ok(())
    }

    fn restore_runtime(
        &mut self,
        session_id: SessionId,
        panes: &[PaneRuntimeMeta],
        layout_root: Option<PaneLayoutNode>,
        focused_pane_id: Uuid,
        floating_surfaces: Vec<FloatingSurfaceRuntime>,
        attach_viewport: Option<AttachViewport>,
    ) -> Result<()> {
        if self.runtimes.contains_key(&session_id) {
            anyhow::bail!("runtime already exists for session {}", session_id.0);
        }

        if panes.is_empty() {
            anyhow::bail!("restored runtime must include panes");
        }
        if !panes.iter().any(|pane| pane.id == focused_pane_id) {
            anyhow::bail!("focused pane missing from restored runtime");
        }

        let runtime_layout_root = layout_root.or_else(|| layout_from_panes(panes));
        let mut initial_pane_sizes = BTreeMap::new();
        if let (Some(layout_root), Some(viewport)) = (&runtime_layout_root, attach_viewport) {
            let mut rects = BTreeMap::new();
            collect_layout_rects(
                layout_root,
                scene_root_from_viewport(Some(viewport)),
                &mut rects,
            );
            for (pane_id, rect) in rects {
                initial_pane_sizes.insert(pane_id, pane_pty_size(rect));
            }
        }

        let mut runtime_panes = BTreeMap::new();
        for pane_meta in panes {
            let initial_size = initial_pane_sizes
                .get(&pane_meta.id)
                .copied()
                .unwrap_or((24, 80));
            let pane = self.spawn_pane_runtime(session_id, pane_meta.clone(), initial_size);
            self.register_pane_input(session_id, &pane);
            runtime_panes.insert(pane_meta.id, pane);
        }

        self.runtimes.insert(
            session_id,
            SessionRuntimeHandle {
                panes: runtime_panes,
                layout_root: runtime_layout_root,
                focused_pane_id,
                zoomed_pane_id: None,
                floating_surfaces,
                attached_clients: BTreeSet::new(),
                attach_viewport,
                attach_view_revision: 0,
            },
        );
        let runtime = self
            .runtimes
            .get(&session_id)
            .expect("runtime inserted before validation");
        validate_runtime_layout_matches_panes(runtime)?;

        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn spawn_pane_runtime(
        &self,
        session_id: SessionId,
        pane_meta: PaneRuntimeMeta,
        initial_size: (u16, u16),
    ) -> PaneRuntimeHandle {
        let (stop_tx, mut stop_rx) = oneshot::channel();
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<PaneRuntimeCommand>();
        let output_buffer = Arc::new(std::sync::Mutex::new(OutputFanoutBuffer::new(
            MAX_WINDOW_OUTPUT_BUFFER_BYTES,
        )));
        let (initial_rows, initial_cols) = sanitize_pty_size(initial_size.0, initial_size.1);
        let terminal_grid = Arc::new(std::sync::Mutex::new(
            TerminalGridStream::new(initial_cols, initial_rows, GridLimits::default())
                .expect("initial pane terminal grid dimensions are valid"),
        ));
        let terminal_grid_for_reader = Arc::clone(&terminal_grid);
        let terminal_grid_deltas = Arc::new(std::sync::Mutex::new(TerminalGridDeltaLog::default()));
        let last_requested_size = Arc::new(std::sync::Mutex::new((initial_rows, initial_cols)));
        let shell = pane_meta.shell.clone();
        let launch = pane_meta.launch.clone();
        let pane_term = self.pane_term.clone();
        let protocol_profile = self.protocol_profile;
        let pane_id = pane_meta.id;
        let replay_command = pane_meta.resurrection.active_command.clone();
        let initial_cwd = pane_meta.resurrection.last_known_cwd.clone();
        let pane_exit_tx = self.pane_exit_tx.clone();
        let shell_integration_root = self.shell_integration_root.clone();
        let output_buffer_for_reader = Arc::clone(&output_buffer);
        let process_id = Arc::new(std::sync::Mutex::new(None));
        let process_id_for_task = Arc::clone(&process_id);
        let process_group_id = Arc::new(std::sync::Mutex::new(None));
        let process_group_id_for_task = Arc::clone(&process_group_id);
        let resurrection_state = Arc::new(std::sync::Mutex::new(
            PaneResurrectionRuntime::from_snapshot(&pane_meta.resurrection),
        ));
        let resurrection_state_for_reader = Arc::clone(&resurrection_state);
        let resurrection_state_for_waiter_seed = Arc::clone(&resurrection_state);
        let pending_prompt_replay = Arc::new(std::sync::Mutex::new(None::<String>));
        let pending_prompt_replay_for_reader = Arc::clone(&pending_prompt_replay);
        let exit_reason = Arc::new(std::sync::Mutex::new(None::<String>));
        let exit_reason_for_task = Arc::clone(&exit_reason);
        let exited = Arc::new(AtomicBool::new(false));
        let exited_for_task = Arc::clone(&exited);

        let output_dirty = Arc::new(AtomicBool::new(false));
        let output_dirty_for_reader = Arc::clone(&output_dirty);
        let last_requested_size_for_reader = Arc::clone(&last_requested_size);
        let sync_update_in_progress = Arc::new(AtomicBool::new(false));
        let sync_update_for_reader = Arc::clone(&sync_update_in_progress);
        let mouse_protocol_state =
            Arc::new(std::sync::Mutex::new(AttachMouseProtocolState::default()));
        let mouse_protocol_state_for_reader = Arc::clone(&mouse_protocol_state);
        let input_mode_state = Arc::new(std::sync::Mutex::new(AttachInputModeState::default()));
        let input_mode_state_for_reader = Arc::clone(&input_mode_state);

        #[cfg(feature = "image-registry")]
        let image_registry = {
            let img_config = bmux_config::BmuxConfig::load()
                .unwrap_or_default()
                .behavior
                .images;
            Arc::new(std::sync::Mutex::new(if img_config.enabled {
                #[allow(clippy::cast_possible_truncation)]
                bmux_image::ImageRegistry::new(
                    img_config.max_images_per_pane as usize,
                    img_config.max_image_bytes as usize,
                )
            } else {
                // Disabled: zero-capacity registry that drops everything.
                bmux_image::ImageRegistry::new(0, 0)
            }))
        };
        #[cfg(feature = "image-registry")]
        let image_registry_for_reader = Arc::clone(&image_registry);
        #[cfg(feature = "image-registry")]
        let cell_pixel_size = Arc::new(std::sync::Mutex::new((0u16, 0u16)));
        #[cfg(feature = "image-registry")]
        let cell_pixel_size_for_reader = Arc::clone(&cell_pixel_size);
        #[cfg(feature = "image-registry")]
        let image_dirty = Arc::new(AtomicBool::new(false));
        #[cfg(feature = "image-registry")]
        let image_dirty_for_reader = Arc::clone(&image_dirty);

        let task = tokio::spawn(async move {
            let pty_system = native_pty_system();
            let Ok(pty_pair) = pty_system.openpty(PtySize {
                rows: initial_rows,
                cols: initial_cols,
                pixel_width: 0,
                pixel_height: 0,
            }) else {
                if let Ok(mut reason) = exit_reason_for_task.lock() {
                    *reason = Some("failed to allocate PTY".to_string());
                }
                push_pane_runtime_notice(
                    &output_buffer_for_reader,
                    "\r\n[bmux] pane failed to start: failed to allocate PTY\r\n",
                );
                exited_for_task.store(true, Ordering::SeqCst);
                return;
            };

            let mut replay_on_prompt = false;
            let (command, failed_spawn_label) = if let Some(launch) = launch.as_ref() {
                let mut command = CommandBuilder::new(&launch.program);
                command.env("TERM", &pane_term);
                for arg in &launch.args {
                    command.arg(arg);
                }
                for (key, value) in &launch.env {
                    command.env(key, value);
                }
                configure_device_seal_broker_env(&mut command);
                if let Some(cwd) = launch.cwd.as_deref().or(initial_cwd.as_deref())
                    && !cwd.is_empty()
                {
                    command.cwd(cwd);
                }
                (command, format!("command '{}'", launch.program))
            } else {
                let mut command = CommandBuilder::new(&shell);
                command.env("TERM", &pane_term);
                configure_device_seal_broker_env(&mut command);
                if let Some(cwd) = initial_cwd.as_deref()
                    && !cwd.is_empty()
                {
                    command.cwd(cwd);
                }
                let integration_configured = configure_shell_integration_command(
                    &mut command,
                    &shell,
                    shell_integration_root.as_deref(),
                )
                .map_or_else(
                    |error| {
                        warn!("failed configuring shell integration for pane {pane_id}: {error:#}");
                        false
                    },
                    |()| shell_integration_root.is_some(),
                );
                replay_on_prompt = integration_configured
                    && matches!(
                        shell_kind_for_path(&shell),
                        ShellKind::Bash | ShellKind::Zsh | ShellKind::Fish | ShellKind::Nu
                    );
                (command, format!("shell '{shell}'"))
            };
            let Ok(mut child) = pty_pair.slave.spawn_command(command) else {
                if let Ok(mut reason) = exit_reason_for_task.lock() {
                    *reason = Some(format!("failed to spawn {failed_spawn_label}"));
                }
                push_pane_runtime_notice(
                    &output_buffer_for_reader,
                    format!(
                        "\r\n[bmux] pane failed to start: failed to spawn {failed_spawn_label}\r\n"
                    ),
                );
                exited_for_task.store(true, Ordering::SeqCst);
                return;
            };
            if let Ok(mut pid) = process_id_for_task.lock() {
                *pid = child.process_id();
            }
            if let Ok(mut pgid) = process_group_id_for_task.lock() {
                *pgid = child
                    .process_id()
                    .and_then(resolve_process_group_id_for_pid);
            }
            let mut child_killer = child.clone_killer();
            drop(pty_pair.slave);

            let master = pty_pair.master;

            let Ok(mut reader) = master.try_clone_reader() else {
                if let Ok(mut reason) = exit_reason_for_task.lock() {
                    *reason = Some("failed to open PTY reader".to_string());
                }
                push_pane_runtime_notice(
                    &output_buffer_for_reader,
                    "\r\n[bmux] pane failed to start: failed to open PTY reader\r\n",
                );
                let _ = child.kill();
                exited_for_task.store(true, Ordering::SeqCst);
                return;
            };
            let Ok(writer) = master.take_writer() else {
                if let Ok(mut reason) = exit_reason_for_task.lock() {
                    *reason = Some("failed to open PTY writer".to_string());
                }
                push_pane_runtime_notice(
                    &output_buffer_for_reader,
                    "\r\n[bmux] pane failed to start: failed to open PTY writer\r\n",
                );
                let _ = child.kill();
                exited_for_task.store(true, Ordering::SeqCst);
                return;
            };
            let writer = Arc::new(std::sync::Mutex::new(writer));

            if launch.is_none()
                && let Some(command_text) = replay_command.as_deref()
            {
                if replay_on_prompt {
                    if let Ok(mut pending) = pending_prompt_replay.lock() {
                        *pending = Some(command_text.to_string());
                    }
                } else if let Ok(mut writer_guard) = writer.lock() {
                    let mut replay_bytes = command_text.as_bytes().to_vec();
                    replay_bytes.push(b'\n');
                    if writer_guard.write_all(&replay_bytes).is_ok() {
                        let _ = writer_guard.flush();
                    }
                }
            }

            let (child_exit_tx, mut child_exit_rx) = mpsc::unbounded_channel::<()>();
            let exited_for_waiter = Arc::clone(&exited_for_task);
            let exit_reason_for_waiter = Arc::clone(&exit_reason_for_task);
            let output_buffer_for_waiter = Arc::clone(&output_buffer_for_reader);
            let resurrection_state_for_waiter = Arc::clone(&resurrection_state_for_waiter_seed);
            let child_waiter_done = Arc::new(AtomicBool::new(false));
            let child_waiter_done_for_thread = Arc::clone(&child_waiter_done);
            let child_waiter = std::thread::Builder::new()
                .name(format!("bmux-server-pane-{pane_id}-wait"))
                .spawn(move || {
                    let _thread_guard = PaneBlockingThreadGuard::new(pane_id, "waiter");
                    let wait_result = child.wait();
                    exited_for_waiter.store(true, Ordering::SeqCst);
                    if let Ok(mut reason) = exit_reason_for_waiter.lock()
                        && reason.is_none()
                    {
                        *reason = Some(match wait_result {
                            Ok(status) => format_pane_exit_reason(&status),
                            Err(error) => format!("process wait failed: {error}"),
                        });
                    }
                    if let Ok(mut resurrection) = resurrection_state_for_waiter.lock() {
                        resurrection.active_command = None;
                        resurrection.active_command_source = None;
                    }
                    push_pane_runtime_notice(
                        &output_buffer_for_waiter,
                        "\r\n[bmux] pane process exited; layout preserved. Use restart pane or close pane.\r\n",
                    );
                    let _ = pane_exit_tx.send(PaneExitEvent {
                        session_id,
                        pane_id,
                    });
                    let _ = child_exit_tx.send(());
                    child_waiter_done_for_thread.store(true, Ordering::SeqCst);
                })
                .ok();
            if child_waiter.is_none() {
                child_waiter_done.store(true, Ordering::SeqCst);
                warn!(%pane_id, "failed spawning pane process waiter thread");
            }

            let reader_output = Arc::clone(&output_buffer_for_reader);
            let writer_for_reader = Arc::clone(&writer);
            let reader_thread_done = Arc::new(AtomicBool::new(false));
            let reader_thread_done_for_thread = Arc::clone(&reader_thread_done);
            let reader_thread = std::thread::Builder::new()
                .name(format!("bmux-server-pane-{pane_id}"))
                .spawn(move || {
                    let _thread_guard = PaneBlockingThreadGuard::new(pane_id, "reader");
                    let mut buffer = [0_u8; 8192];
                    let mut protocol_engine = TerminalProtocolEngine::new(protocol_profile);
                    let (initial_rows, initial_cols) = last_requested_size_for_reader
                        .lock()
                        .map_or((24, 80), |size| *size);
                    let mut cursor_tracker = PaneCursorTracker::new(initial_rows, initial_cols);
                    let mut terminal_mode_tracker = PaneTerminalModeTracker::default();
                    let mut shell_metadata_parser = PaneShellMetadataParser::default();

                    // Image interceptor: detects and extracts image escape sequences
                    // (Sixel, Kitty, iTerm2) from PTY output before they reach the
                    // output buffer.  Feature-gated to compile away when no image
                    // protocols are enabled.
                    #[cfg(feature = "image-registry")]
                    let mut image_interceptor = bmux_image::ImageInterceptor::new();

                    loop {
                        match reader.read(&mut buffer) {
                            Ok(0) | Err(_) => break,
                            Ok(bytes_read) => {
                                let chunk = &buffer[..bytes_read];
                                if let Ok((rows, cols)) =
                                    last_requested_size_for_reader.lock().map(|size| *size)
                                {
                                    cursor_tracker.resize(rows, cols);
                                }

                                // When image support is enabled, run the interceptor
                                // to extract image sequences from the byte stream.
                                // The filtered bytes (images stripped) are what gets
                                // pushed to the structured grid cursor tracker.
                                #[cfg(feature = "image-registry")]
                                let chunk = {
                                    let mut result = image_interceptor.process(chunk);

                                    if !result.events.is_empty() {
                                        // Resolve cursor positions for each image event.
                                        // Feed filtered bytes up to each event's offset
                                        // to the cursor tracker, then capture position.
                                        let mut cursor_fed_to = 0usize;
                                        for event in &mut result.events {
                                            let offset = event.filtered_byte_offset();
                                            if offset > cursor_fed_to {
                                                cursor_tracker.process(
                                                    &result.filtered[cursor_fed_to..offset],
                                                );
                                                cursor_fed_to = offset;
                                            }
                                            let (row, col) = cursor_tracker.cursor_position();
                                            event.set_position(bmux_image::ImagePosition {
                                                row,
                                                col,
                                            });
                                        }

                                        let (cpw, cph) = cell_pixel_size_for_reader
                                            .lock()
                                            .map_or((8, 16), |s| *s);
                                        let cpw = if cpw == 0 { 8 } else { cpw };
                                        let cph = if cph == 0 { 16 } else { cph };
                                        if let Ok(mut reg) = image_registry_for_reader.lock() {
                                            for event in &result.events {
                                                reg.handle_event(event.clone(), cpw, cph);
                                            }
                                        }
                                        // Notify streaming clients that image state changed.
                                        // Only emit on false→true transition to coalesce.
                                        if image_dirty_for_reader
                                            .compare_exchange(
                                                false,
                                                true,
                                                Ordering::SeqCst,
                                                Ordering::SeqCst,
                                            )
                                            .is_ok()
                                        {
                                            publish_pane_event(PaneEvent::ImageAvailable {
                                                session_id: session_id.0,
                                                pane_id,
                                            });
                                        }
                                        for event in &result.events {
                                            let payload = image_event_to_recording_payload(event);
                                            record_to_all_runtimes(
                                                RecordingEventKind::PaneImage,
                                                payload,
                                                RecordMeta {
                                                    session_id: Some(session_id.0),
                                                    pane_id: Some(pane_id),
                                                    client_id: None,
                                                },
                                            );
                                        }
                                    }
                                    result.filtered
                                };
                                #[cfg(feature = "image-registry")]
                                let chunk = chunk.as_slice();

                                let metadata = shell_metadata_parser.process_chunk(chunk);
                                if !metadata.events.is_empty() {
                                    let mut replay_command = None;
                                    if let Ok(mut resurrection_state) =
                                        resurrection_state_for_reader.lock()
                                    {
                                        if let Ok(mut pending_replay) =
                                            pending_prompt_replay_for_reader.lock()
                                        {
                                            replay_command =
                                                apply_shell_metadata_events_and_take_prompt_replay(
                                                    &mut resurrection_state,
                                                    &mut pending_replay,
                                                    metadata.events,
                                                );
                                        } else {
                                            for event in metadata.events {
                                                resurrection_state.apply_event(event);
                                            }
                                        }
                                    }
                                    if let Some(command_text) = replay_command
                                        && let Ok(mut writer) = writer_for_reader.lock()
                                    {
                                        let mut replay_bytes = command_text.as_bytes().to_vec();
                                        replay_bytes.push(b'\n');
                                        if writer.write_all(&replay_bytes).is_ok() {
                                            let _ = writer.flush();
                                        }
                                    }
                                    mark_snapshot_dirty_flag();
                                }
                                let chunk = metadata.filtered;
                                let chunk = chunk.as_slice();

                                // Detect screen-clearing CSI sequences (\e[2J, \e[3J)
                                // and clear the image registry when they occur.
                                #[cfg(feature = "image-registry")]
                                if chunk_contains_screen_clear(chunk) {
                                    if let Ok(mut reg) = image_registry_for_reader.lock() {
                                        reg.clear();
                                    }
                                    if image_dirty_for_reader
                                        .compare_exchange(
                                            false,
                                            true,
                                            Ordering::SeqCst,
                                            Ordering::SeqCst,
                                        )
                                        .is_ok()
                                    {
                                        publish_pane_event(PaneEvent::ImageAvailable {
                                            session_id: session_id.0,
                                            pane_id,
                                        });
                                    }
                                }

                                // Update terminal mode tracking (mouse protocol,
                                // cursor/keypad modes, synchronized update) BEFORE
                                // making the chunk visible in the output buffer.
                                // This ensures per-pane mode snapshots are always
                                // consistent with or ahead of the buffered data.
                                terminal_mode_tracker.process(chunk);
                                if let Ok(mut protocol) = mouse_protocol_state_for_reader.lock() {
                                    *protocol = terminal_mode_tracker.current_protocol();
                                }
                                if let Ok(mut mode_state) = input_mode_state_for_reader.lock() {
                                    *mode_state = terminal_mode_tracker.current_input_modes();
                                }
                                sync_update_for_reader
                                    .store(terminal_mode_tracker.sync_update, Ordering::SeqCst);

                                if let Ok(mut grid) = terminal_grid_for_reader.lock() {
                                    grid.process(chunk);
                                } else {
                                    break;
                                }

                                if let Ok(mut output) = reader_output.lock() {
                                    output.push_chunk(chunk);
                                } else {
                                    break;
                                }
                                // Notify streaming clients that new output is available.
                                // Only emit when transitioning false→true to coalesce
                                // thousands of per-chunk writes into ~1 event per fetch cycle.
                                if output_dirty_for_reader
                                    .compare_exchange(
                                        false,
                                        true,
                                        Ordering::SeqCst,
                                        Ordering::SeqCst,
                                    )
                                    .is_ok()
                                {
                                    publish_pane_event(PaneEvent::OutputAvailable {
                                        session_id: session_id.0,
                                        pane_id,
                                    });
                                }
                                record_to_all_runtimes(
                                    RecordingEventKind::PaneOutputRaw,
                                    RecordingPayload::Bytes {
                                        data: chunk.to_vec(),
                                    },
                                    RecordMeta {
                                        session_id: Some(session_id.0),
                                        pane_id: Some(pane_id),
                                        client_id: None,
                                    },
                                );
                                let reply = protocol_reply_for_chunk(
                                    &mut protocol_engine,
                                    &mut cursor_tracker,
                                    chunk,
                                );
                                // Detect scroll events and shift image positions.
                                #[cfg(feature = "image-registry")]
                                {
                                    let scroll_delta = cursor_tracker.drain_scroll_delta();
                                    if scroll_delta > 0 {
                                        if let Ok(mut reg) = image_registry_for_reader.lock() {
                                            reg.scroll_up(scroll_delta);
                                        }
                                        // Notify streaming clients that image positions shifted.
                                        if image_dirty_for_reader
                                            .compare_exchange(
                                                false,
                                                true,
                                                Ordering::SeqCst,
                                                Ordering::SeqCst,
                                            )
                                            .is_ok()
                                        {
                                            publish_pane_event(PaneEvent::ImageAvailable {
                                                session_id: session_id.0,
                                                pane_id,
                                            });
                                        }
                                    }
                                }
                                if !reply.is_empty() {
                                    record_to_all_runtimes(
                                        RecordingEventKind::ProtocolReplyRaw,
                                        RecordingPayload::Bytes {
                                            data: reply.clone(),
                                        },
                                        RecordMeta {
                                            session_id: Some(session_id.0),
                                            pane_id: Some(pane_id),
                                            client_id: None,
                                        },
                                    );
                                    if let Ok(mut writer) = writer_for_reader.lock() {
                                        if writer.write_all(&reply).is_err() {
                                            break;
                                        }
                                        let _ = writer.flush();
                                    } else {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    reader_thread_done_for_thread.store(true, Ordering::SeqCst);
                })
                .ok();
            if reader_thread.is_none() {
                reader_thread_done.store(true, Ordering::SeqCst);
                warn!(%pane_id, "failed spawning pane PTY reader thread");
            }

            loop {
                tokio::select! {
                    _ = &mut stop_rx => {
                        let _ = child_killer.kill();
                        break;
                    }
                    child_exit = child_exit_rx.recv() => {
                        if child_exit.is_some() {
                            break;
                        }
                    }
                    input = input_rx.recv() => {
                        match input {
                            Some(PaneRuntimeCommand::Input(bytes)) => {
                                if let Ok(mut writer) = writer.lock()
                                    && writer.write_all(&bytes).is_ok()
                                {
                                    let _ = writer.flush();
                                } else {
                                    break;
                                }
                            }
                            Some(PaneRuntimeCommand::Resize { rows, cols }) => {
                                let _ = master.resize(PtySize {
                                    rows,
                                    cols,
                                    pixel_width: 0,
                                    pixel_height: 0,
                                });
                            }
                            None => break,
                        }
                    }
                }
            }

            // Do not synchronously join the OS reader/waiter threads from the
            // async runtime task. On shutdown the reader can remain blocked in
            // a PTY read until all handles are dropped; joining it here before
            // the task returns can deadlock pane/session teardown and starve
            // unrelated attach service requests. Dropping the handles detaches
            // the threads; they exit once the PTY/process closes.
            drop(writer);
            drop(master);
            drop(child_waiter);
            drop(reader_thread);
            let waiter_done_for_diagnostic = Arc::clone(&child_waiter_done);
            let reader_done_for_diagnostic = Arc::clone(&reader_thread_done);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let waiter_done = waiter_done_for_diagnostic.load(Ordering::SeqCst);
                let reader_done = reader_done_for_diagnostic.load(Ordering::SeqCst);
                if !waiter_done || !reader_done {
                    warn!(
                        %pane_id,
                        waiter_done,
                        reader_done,
                        active_blocking_threads = PANE_BLOCKING_THREAD_COUNT.load(Ordering::SeqCst),
                        "pane blocking thread cleanup still pending 5s after runtime task shutdown",
                    );
                }
            });
            exited_for_task.store(true, Ordering::SeqCst);
        });

        PaneRuntimeHandle {
            meta: pane_meta,
            process_id,
            process_group_id,
            resurrection_state,
            exit_reason,
            stop_tx: Some(stop_tx),
            task,
            input_tx,
            output_buffer,
            terminal_grid,
            terminal_grid_deltas,
            exited,
            last_requested_size,
            output_dirty,
            sync_update_in_progress,
            mouse_protocol_state,
            input_mode_state,
            #[cfg(feature = "image-registry")]
            image_registry,
            #[cfg(feature = "image-registry")]
            cell_pixel_size,
            #[cfg(feature = "image-registry")]
            image_dirty,
        }
    }

    fn split_pane(
        &mut self,
        session_id: SessionId,
        target: Option<PaneSelector>,
        direction: PaneSplitDirection,
    ) -> Result<Uuid> {
        self.create_split_pane(session_id, target, direction, None, None)
    }

    fn launch_pane(
        &mut self,
        session_id: SessionId,
        target: Option<PaneSelector>,
        direction: PaneSplitDirection,
        name: Option<String>,
        command: PaneLaunchCommand,
    ) -> Result<Uuid> {
        let launch = pane_launch_spec_from_command(command)?;
        self.create_split_pane(session_id, target, direction, name, Some(launch))
    }

    fn create_split_pane(
        &mut self,
        session_id: SessionId,
        target: Option<PaneSelector>,
        direction: PaneSplitDirection,
        name: Option<String>,
        launch: Option<PaneLaunchSpec>,
    ) -> Result<Uuid> {
        // Auto-unzoom on layout mutation.
        if let Some(session) = self.runtimes.get_mut(&session_id) {
            session.zoomed_pane_id = None;
        }
        let (target_pane_id, next_pane_name, shell, client_ids) = {
            let session = self
                .runtimes
                .get_mut(&session_id)
                .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
            let target_pane_id =
                resolve_pane_id_from_selector(session, &target.unwrap_or(PaneSelector::Active))
                    .ok_or_else(|| anyhow::anyhow!("target pane not found"))?;
            let focused = session
                .panes
                .get(&target_pane_id)
                .ok_or_else(|| anyhow::anyhow!("target pane not found"))?;
            let name_prefix = match direction {
                PaneSplitDirection::Vertical => "v",
                PaneSplitDirection::Horizontal => "h",
            };
            (
                target_pane_id,
                name.or_else(|| Some(format!("{name_prefix}-pane-{}", session.panes.len() + 1))),
                focused.meta.shell.clone(),
                session.attached_clients.iter().copied().collect::<Vec<_>>(),
            )
        };

        let pane_id = Uuid::new_v4();
        let pane_meta = PaneRuntimeMeta {
            id: pane_id,
            name: next_pane_name,
            shell,
            launch,
            resurrection: PaneResurrectionSnapshot::default(),
        };
        let handle = self.spawn_pane_runtime(session_id, pane_meta, (24, 80));
        self.register_pane_input(session_id, &handle);
        for client_id in client_ids {
            if let Ok(mut output) = handle.output_buffer.lock() {
                output.register_client_at_tail(client_id);
            }
        }

        let session = self
            .runtimes
            .get_mut(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        session.panes.insert(pane_id, handle);
        let replaced = if let Some(layout_root) = &mut session.layout_root {
            layout_root.replace_leaf_with_split(target_pane_id, direction, 0.5, pane_id)
        } else {
            session.layout_root = Some(PaneLayoutNode::Leaf { pane_id });
            true
        };
        if !replaced {
            anyhow::bail!("failed to apply split to layout tree")
        }
        session.focused_pane_id = pane_id;
        self.apply_stored_attach_viewport(session_id);
        Ok(pane_id)
    }

    fn focus_pane(&mut self, session_id: SessionId, direction: PaneFocusDirection) -> Result<Uuid> {
        let session = self
            .runtimes
            .get_mut(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        // If zoomed, stay zoomed but update to the new focused pane.
        let was_zoomed = session.zoomed_pane_id.is_some();
        let pane_ids = ordered_pane_ids(session);
        if pane_ids.is_empty() {
            anyhow::bail!("no panes in session runtime")
        }
        let current_index = pane_ids
            .iter()
            .position(|id| *id == session.focused_pane_id)
            .unwrap_or(0);
        let len = pane_ids.len();
        let next_index = match direction {
            PaneFocusDirection::Next => (current_index + 1) % len,
            PaneFocusDirection::Prev => {
                if current_index == 0 {
                    len - 1
                } else {
                    current_index - 1
                }
            }
        };
        session.focused_pane_id = pane_ids[next_index];
        if was_zoomed {
            session.zoomed_pane_id = Some(pane_ids[next_index]);
            self.apply_stored_attach_viewport(session_id);
        }
        Ok(self.runtimes[&session_id].focused_pane_id)
    }

    fn focus_pane_target(&mut self, session_id: SessionId, target: &PaneSelector) -> Result<Uuid> {
        let session = self
            .runtimes
            .get_mut(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        // If zoomed, stay zoomed but update to the new focused pane.
        let was_zoomed = session.zoomed_pane_id.is_some();
        let pane_id = resolve_pane_id_from_selector(session, target)
            .ok_or_else(|| anyhow::anyhow!("target pane not found"))?;
        session.focused_pane_id = pane_id;
        if was_zoomed {
            session.zoomed_pane_id = Some(pane_id);
            self.apply_stored_attach_viewport(session_id);
        }
        Ok(pane_id)
    }

    fn close_pane(
        &mut self,
        session_id: SessionId,
        target: Option<PaneSelector>,
    ) -> Result<(Uuid, Option<RemovedRuntime>)> {
        // Auto-unzoom on layout mutation.
        if let Some(session) = self.runtimes.get_mut(&session_id) {
            session.zoomed_pane_id = None;
        }
        let (pane_id, remove_runtime) = {
            let session = self
                .runtimes
                .get_mut(&session_id)
                .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
            let pane_id =
                resolve_pane_id_from_selector(session, &target.unwrap_or(PaneSelector::Active))
                    .ok_or_else(|| anyhow::anyhow!("target pane not found"))?;
            let remove_count = 1 + session
                .floating_surfaces
                .iter()
                .filter(|surface| {
                    surface.anchor_pane_id == Some(pane_id) && surface.pane_id != pane_id
                })
                .count();
            (pane_id, session.panes.len() <= remove_count)
        };

        if remove_runtime {
            anyhow::bail!("cannot close the final pane without an explicit session action");
        }

        let pane_input_index = Arc::clone(&self.pane_input_index);
        let session = self
            .runtimes
            .get_mut(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        let next_focus = session.layout_root.as_ref().and_then(|layout_root| {
            next_focus_after_close(
                layout_root,
                session.focused_pane_id,
                pane_id,
                session.attach_viewport,
            )
        });
        let pane = session
            .panes
            .remove(&pane_id)
            .ok_or_else(|| anyhow::anyhow!("focused pane not found"))?;
        if let Ok(mut index) = pane_input_index.write() {
            index.remove(&pane_id);
        }
        let anchored_floating_panes = session
            .floating_surfaces
            .iter()
            .filter(|surface| surface.anchor_pane_id == Some(pane_id))
            .map(|surface| surface.pane_id)
            .filter(|floating_pane_id| *floating_pane_id != pane_id)
            .collect::<Vec<_>>();
        session.floating_surfaces.retain(|surface| {
            surface.pane_id != pane_id && surface.anchor_pane_id != Some(pane_id)
        });
        for floating_pane_id in anchored_floating_panes {
            if let Some(floating_pane) = session.panes.remove(&floating_pane_id) {
                if let Ok(mut index) = pane_input_index.write() {
                    index.remove(&floating_pane_id);
                }
                tokio::spawn(async move {
                    shutdown_pane_handle(floating_pane).await;
                });
            }
        }
        if let Some(layout_root) = &mut session.layout_root {
            let _ = layout_root.remove_leaf(pane_id);
        }
        let layout_ids = session
            .layout_root
            .as_ref()
            .map(|layout| {
                let mut ids = Vec::new();
                layout.pane_order(&mut ids);
                ids
            })
            .unwrap_or_default();
        if layout_ids.iter().any(|id| !session.panes.contains_key(id)) {
            session.layout_root = None;
        }
        if session.focused_pane_id == pane_id
            || !session.panes.contains_key(&session.focused_pane_id)
        {
            let next_focus = next_focus.or_else(|| ordered_pane_ids(session).first().copied());
            if let Some(next_id) = next_focus {
                session.focused_pane_id = next_id;
            }
        }

        tokio::spawn(async move {
            shutdown_pane_handle(pane).await;
        });
        self.apply_stored_attach_viewport(session_id);
        Ok((pane_id, None))
    }

    fn restart_pane(
        &mut self,
        session_id: SessionId,
        target: Option<PaneSelector>,
    ) -> Result<Uuid> {
        let pane_meta = {
            let session = self
                .runtimes
                .get(&session_id)
                .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
            let pane_id =
                resolve_pane_id_from_selector(session, &target.unwrap_or(PaneSelector::Active))
                    .ok_or_else(|| anyhow::anyhow!("target pane not found"))?;
            let pane = session
                .panes
                .get(&pane_id)
                .ok_or_else(|| anyhow::anyhow!("target pane not found"))?;
            let preserved_cwd = pane
                .resurrection_state
                .lock()
                .ok()
                .and_then(|state| state.last_known_cwd.clone());
            PaneRuntimeMeta {
                id: pane_id,
                name: pane.meta.name.clone(),
                shell: pane.meta.shell.clone(),
                launch: pane.meta.launch.clone(),
                resurrection: PaneResurrectionSnapshot {
                    active_command: None,
                    active_command_source: None,
                    last_known_cwd: preserved_cwd,
                },
            }
        };

        let old_pane = {
            let session = self
                .runtimes
                .get_mut(&session_id)
                .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
            session
                .panes
                .remove(&pane_meta.id)
                .ok_or_else(|| anyhow::anyhow!("target pane not found"))?
        };
        self.unregister_pane_input(pane_meta.id);
        tokio::spawn(async move {
            shutdown_pane_handle(old_pane).await;
        });

        let new_pane = self.spawn_pane_runtime(session_id, pane_meta.clone(), (24, 80));
        self.register_pane_input(session_id, &new_pane);
        let client_ids = {
            let session = self
                .runtimes
                .get(&session_id)
                .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
            session.attached_clients.iter().copied().collect::<Vec<_>>()
        };
        for client_id in client_ids {
            if let Ok(mut output) = new_pane.output_buffer.lock() {
                output.register_client_at_tail(client_id);
            }
        }
        if let Ok(mut reason) = new_pane.exit_reason.lock() {
            *reason = None;
        }

        let session = self
            .runtimes
            .get_mut(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        session.panes.insert(pane_meta.id, new_pane);
        session.focused_pane_id = pane_meta.id;
        self.apply_stored_attach_viewport(session_id);
        Ok(pane_meta.id)
    }

    fn resize_pane(
        &mut self,
        session_id: SessionId,
        target: Option<PaneSelector>,
        direction: PaneResizeDirection,
        cells: u16,
    ) -> Result<()> {
        let session = self
            .runtimes
            .get_mut(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        // Auto-unzoom on layout mutation.
        session.zoomed_pane_id = None;
        let pane_id =
            resolve_pane_id_from_selector(session, &target.unwrap_or(PaneSelector::Active))
                .ok_or_else(|| anyhow::anyhow!("target pane not found"))?;
        let root = scene_root_from_viewport(session.attach_viewport);
        if let Some(layout_root) = &mut session.layout_root {
            let _ = layout_root.resize_focused(pane_id, direction, root, cells.max(1));
        }
        self.apply_stored_attach_viewport(session_id);
        Ok(())
    }

    fn toggle_zoom(&mut self, session_id: SessionId) -> Result<(Uuid, bool)> {
        let session = self
            .runtimes
            .get_mut(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        let focused = session.focused_pane_id;
        if session.zoomed_pane_id.is_some() {
            session.zoomed_pane_id = None;
            self.apply_stored_attach_viewport(session_id);
            Ok((focused, false))
        } else {
            // Only zoom if there are at least 2 tiled panes (zooming a single pane is a no-op).
            let pane_ids = ordered_tiled_pane_ids(session);
            if pane_ids.len() < 2 {
                return Ok((focused, false));
            }
            session.zoomed_pane_id = Some(focused);
            self.apply_stored_attach_viewport(session_id);
            Ok((focused, true))
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "Floating pane creation mirrors the BPDL command fields and avoids a plugin-private options type at this boundary."
    )]
    fn create_floating_pane(
        &mut self,
        session_id: SessionId,
        target: Option<PaneSelector>,
        rect: LayoutRect,
        scope: FloatingPaneScope,
        layer: FloatingPaneLayer,
        z: i32,
        name: Option<String>,
        command: Option<PaneLaunchCommand>,
        anchor_pane_id: Option<Uuid>,
        context_id: Option<Uuid>,
        client_id: Option<ClientId>,
    ) -> Result<FloatingPaneRuntimeSummary> {
        let (target_pane_id, next_pane_name, shell, client_ids) = {
            let session = self
                .runtimes
                .get_mut(&session_id)
                .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
            let target_pane_id =
                resolve_pane_id_from_selector(session, &target.unwrap_or(PaneSelector::Active))
                    .unwrap_or(session.focused_pane_id);
            let shell = session
                .panes
                .get(&target_pane_id)
                .map_or_else(|| self.shell.clone(), |pane| pane.meta.shell.clone());
            (
                target_pane_id,
                name.or_else(|| Some(format!("floating-pane-{}", session.panes.len() + 1))),
                shell,
                session.attached_clients.iter().copied().collect::<Vec<_>>(),
            )
        };

        let pane_id = Uuid::new_v4();
        let pane_meta = PaneRuntimeMeta {
            id: pane_id,
            name: next_pane_name,
            shell,
            launch: command.map(pane_launch_spec_from_command).transpose()?,
            resurrection: PaneResurrectionSnapshot::default(),
        };
        let handle = self.spawn_pane_runtime(session_id, pane_meta, (24, 80));
        self.register_pane_input(session_id, &handle);
        for client_id in client_ids {
            if let Ok(mut output) = handle.output_buffer.lock() {
                output.register_client_at_tail(client_id);
            }
        }

        let surface = FloatingSurfaceRuntime {
            id: Uuid::new_v4(),
            pane_id,
            anchor_pane_id: matches!(scope, FloatingPaneScope::PerPane)
                .then_some(anchor_pane_id.unwrap_or(target_pane_id)),
            context_id: matches!(scope, FloatingPaneScope::PerWindow)
                .then(|| context_id.or_else(|| current_context_id_for_session(session_id)))
                .flatten(),
            client_id: matches!(scope, FloatingPaneScope::ClientGlobal)
                .then_some(client_id)
                .flatten(),
            rect,
            scope,
            layer,
            z,
            visible: true,
            opaque: true,
            accepts_input: true,
            cursor_owner: true,
        };
        let session = self
            .runtimes
            .get_mut(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        let _ = target_pane_id;
        session.panes.insert(pane_id, handle);
        session
            .floating_surfaces
            .iter_mut()
            .for_each(|surface| surface.cursor_owner = false);
        session.floating_surfaces.push(surface);
        session.focused_pane_id = pane_id;
        self.apply_stored_attach_viewport(session_id);
        Ok(floating_summary(surface))
    }

    fn list_floating_panes(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<FloatingPaneRuntimeSummary>> {
        let session = self
            .runtimes
            .get(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        Ok(session
            .floating_surfaces
            .iter()
            .copied()
            .filter(|surface| session.panes.contains_key(&surface.pane_id))
            .map(floating_summary)
            .collect())
    }

    fn update_floating_pane(
        &mut self,
        _session_id: SessionId,
        pane_id: Uuid,
        update: impl FnOnce(&mut FloatingSurfaceRuntime, i32, i32),
    ) -> Result<FloatingPaneRuntimeSummary> {
        let owner_session_id = self
            .runtimes
            .iter()
            .find_map(|(owner_session_id, runtime)| {
                runtime
                    .floating_surfaces
                    .iter()
                    .any(|surface| surface.pane_id == pane_id)
                    .then_some(*owner_session_id)
            })
            .ok_or_else(|| anyhow::anyhow!("floating pane not found"))?;
        let session = self.runtimes.get_mut(&owner_session_id).ok_or_else(|| {
            anyhow::anyhow!("runtime not found for session {}", owner_session_id.0)
        })?;
        let max_z = session
            .floating_surfaces
            .iter()
            .map(|surface| surface.z)
            .max()
            .unwrap_or(0);
        let min_z = session
            .floating_surfaces
            .iter()
            .map(|surface| surface.z)
            .min()
            .unwrap_or(0);
        let surface = session
            .floating_surfaces
            .iter_mut()
            .find(|surface| surface.pane_id == pane_id)
            .ok_or_else(|| anyhow::anyhow!("floating pane not found"))?;
        update(surface, min_z, max_z);
        let summary = floating_summary(*surface);
        self.apply_stored_attach_viewport(owner_session_id);
        Ok(summary)
    }

    fn move_floating_pane(
        &mut self,
        session_id: SessionId,
        pane_id: Uuid,
        x: u16,
        y: u16,
    ) -> Result<FloatingPaneRuntimeSummary> {
        self.update_floating_pane(session_id, pane_id, |surface, _, _| {
            surface.rect.x = x;
            surface.rect.y = y;
        })
    }

    fn resize_floating_pane(
        &mut self,
        session_id: SessionId,
        pane_id: Uuid,
        w: u16,
        h: u16,
    ) -> Result<FloatingPaneRuntimeSummary> {
        self.update_floating_pane(session_id, pane_id, |surface, _, _| {
            surface.rect.w = w.max(1);
            surface.rect.h = h.max(1);
        })
    }

    fn focus_floating_pane(
        &mut self,
        _session_id: SessionId,
        pane_id: Uuid,
    ) -> Result<FloatingPaneRuntimeSummary> {
        let mut found = None;
        let mut owner_session_id = None;
        for (candidate_session_id, session) in &mut self.runtimes {
            for surface in &mut session.floating_surfaces {
                surface.cursor_owner = surface.pane_id == pane_id;
                if surface.cursor_owner {
                    found = Some(*surface);
                    owner_session_id = Some(*candidate_session_id);
                }
            }
            if session.panes.contains_key(&pane_id) {
                session.focused_pane_id = pane_id;
            }
        }
        let surface = found.ok_or_else(|| anyhow::anyhow!("floating pane not found"))?;
        if let Some(owner_session_id) = owner_session_id {
            self.apply_stored_attach_viewport(owner_session_id);
        }
        Ok(floating_summary(surface))
    }

    fn raise_floating_pane(
        &mut self,
        session_id: SessionId,
        pane_id: Uuid,
    ) -> Result<FloatingPaneRuntimeSummary> {
        self.update_floating_pane(session_id, pane_id, |surface, _, max_z| {
            surface.z = max_z.saturating_add(1);
        })
    }

    fn lower_floating_pane(
        &mut self,
        session_id: SessionId,
        pane_id: Uuid,
    ) -> Result<FloatingPaneRuntimeSummary> {
        self.update_floating_pane(session_id, pane_id, |surface, min_z, _| {
            surface.z = min_z.saturating_sub(1);
        })
    }

    fn set_floating_pane_z(
        &mut self,
        session_id: SessionId,
        pane_id: Uuid,
        z: i32,
    ) -> Result<FloatingPaneRuntimeSummary> {
        self.update_floating_pane(session_id, pane_id, |surface, _, _| {
            surface.z = z;
        })
    }

    fn set_floating_pane_layer(
        &mut self,
        session_id: SessionId,
        pane_id: Uuid,
        layer: FloatingPaneLayer,
    ) -> Result<FloatingPaneRuntimeSummary> {
        self.update_floating_pane(session_id, pane_id, |surface, _, _| {
            surface.layer = layer;
        })
    }

    fn close_floating_pane(
        &mut self,
        session_id: SessionId,
        pane_id: Uuid,
    ) -> Result<(Uuid, Option<RemovedRuntime>)> {
        let owner_session_id = self
            .pane_session_for_attach(session_id, pane_id)
            .unwrap_or(session_id);
        let remove_runtime = {
            let session = self.runtimes.get(&owner_session_id).ok_or_else(|| {
                anyhow::anyhow!("runtime not found for session {}", owner_session_id.0)
            })?;
            if !session
                .floating_surfaces
                .iter()
                .any(|surface| surface.pane_id == pane_id)
            {
                anyhow::bail!("floating pane not found");
            }
            let remove_count = 1 + session
                .floating_surfaces
                .iter()
                .filter(|surface| {
                    surface.anchor_pane_id == Some(pane_id) && surface.pane_id != pane_id
                })
                .count();
            session.panes.len() <= remove_count
        };
        if remove_runtime {
            anyhow::bail!("cannot close the final pane without an explicit session action");
        }
        self.close_pane(owner_session_id, Some(PaneSelector::ById(pane_id)))
    }

    #[allow(clippy::cast_possible_truncation)]
    fn list_panes(&self, session_id: SessionId) -> Result<Vec<PaneSummary>> {
        let session = self
            .runtimes
            .get(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        let pane_ids = ordered_pane_ids(session);
        let panes = pane_ids
            .iter()
            .enumerate()
            .filter_map(|(index, pane_id)| {
                session.panes.get(pane_id).map(|pane| PaneSummary {
                    id: *pane_id,
                    index: (index + 1) as u32,
                    name: pane.meta.name.clone(),
                    focused: *pane_id == session.focused_pane_id,
                    state: pane_state_for_handle(pane),
                    state_reason: pane_state_reason_for_handle(pane),
                })
            })
            .collect::<Vec<_>>();
        Ok(panes)
    }

    fn build_attach_scene_for_client(
        &self,
        session_id: SessionId,
        client_id: ClientId,
    ) -> Result<AttachScene, SessionRuntimeError> {
        let session = self
            .runtimes
            .get(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?;
        let context_id = current_context_id_for_session(session_id);
        let mut scene = build_attach_scene(
            session_id,
            session,
            session.attach_viewport,
            client_id,
            context_id,
        );
        for (owner_session_id, owner_runtime) in &self.runtimes {
            if *owner_session_id == session_id {
                continue;
            }
            scene.surfaces.extend(
                owner_runtime
                    .floating_surfaces
                    .iter()
                    .filter(|surface| {
                        floating_surface_visible_for_attach(
                            *owner_session_id,
                            session_id,
                            owner_runtime,
                            surface,
                            client_id,
                            context_id,
                        )
                    })
                    .map(attach_surface_from_floating),
            );
        }
        let pane_id = focused_pane_for_scene(&scene, session.focused_pane_id);
        Ok(AttachScene {
            focus: AttachFocusTarget::Pane { pane_id },
            ..scene
        })
    }

    fn pane_session_for_attach(
        &self,
        attach_session_id: SessionId,
        pane_id: Uuid,
    ) -> Option<SessionId> {
        if self
            .runtimes
            .get(&attach_session_id)
            .is_some_and(|runtime| runtime.panes.contains_key(&pane_id))
        {
            return Some(attach_session_id);
        }
        self.runtimes.iter().find_map(|(session_id, runtime)| {
            runtime.panes.contains_key(&pane_id).then_some(*session_id)
        })
    }

    fn attach_scene_pane_refs(
        &self,
        attach_session_id: SessionId,
        scene: &AttachScene,
    ) -> Vec<(SessionId, Uuid)> {
        let mut seen = BTreeSet::new();
        scene
            .surfaces
            .iter()
            .filter(|surface| surface.visible && surface.pane_id.is_some())
            .filter_map(|surface| surface.pane_id)
            .filter_map(|pane_id| {
                let owner_session_id = self.pane_session_for_attach(attach_session_id, pane_id)?;
                seen.insert(pane_id).then_some((owner_session_id, pane_id))
            })
            .collect()
    }

    fn attach_scene_data_snapshot(
        &self,
        session_id: SessionId,
        client_id: ClientId,
    ) -> Result<AttachSceneDataSnapshot, SessionRuntimeError> {
        let session = self
            .runtimes
            .get(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?;
        if !session.attached_clients.contains(&client_id) {
            return Err(SessionRuntimeError::NotAttached);
        }
        let scene = self.build_attach_scene_for_client(session_id, client_id)?;
        let pane_refs = self.attach_scene_pane_refs(session_id, &scene);
        let focused_pane_id = focused_pane_for_scene(&scene, session.focused_pane_id);
        let panes = pane_refs
            .into_iter()
            .filter_map(|(owner_session_id, pane_id)| {
                self.runtimes
                    .get(&owner_session_id)?
                    .panes
                    .get(&pane_id)
                    .map(|pane| pane_attach_data_ref(owner_session_id, pane_id, pane))
            })
            .collect::<Vec<_>>();
        Ok(AttachSceneDataSnapshot {
            focused_pane_id,
            panes,
            layout_root: fallback_ipc_layout(session),
            scene,
            zoomed: session.zoomed_pane_id.is_some(),
        })
    }

    fn attach_pane_data_refs_for_scene_panes(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        pane_ids: &[Uuid],
    ) -> Result<Vec<PaneAttachDataRef>, SessionRuntimeError> {
        let session = self
            .runtimes
            .get(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?;
        if !session.attached_clients.contains(&client_id) {
            return Err(SessionRuntimeError::NotAttached);
        }
        Ok(pane_ids
            .iter()
            .filter_map(|pane_id| {
                let owner_session_id = self.pane_session_for_attach(session_id, *pane_id)?;
                self.runtimes
                    .get(&owner_session_id)?
                    .panes
                    .get(pane_id)
                    .map(|pane| pane_attach_data_ref(owner_session_id, *pane_id, pane))
            })
            .collect())
    }

    fn attach_pane_data_refs_for_session_panes(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        pane_ids: &[Uuid],
    ) -> Result<Vec<PaneAttachDataRef>, SessionRuntimeError> {
        let session = self
            .runtimes
            .get(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?;
        if !session.attached_clients.contains(&client_id) {
            return Err(SessionRuntimeError::NotAttached);
        }
        Ok(pane_ids
            .iter()
            .filter_map(|pane_id| {
                session
                    .panes
                    .get(pane_id)
                    .map(|pane| pane_attach_data_ref(session_id, *pane_id, pane))
            })
            .collect())
    }

    fn attach_pane_data_ref_for_push(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
        client_id: ClientId,
    ) -> Option<PaneAttachDataRef> {
        let session = self.runtimes.get(&session_id)?;
        if !session.attached_clients.contains(&client_id) {
            return None;
        }
        let pane = session.panes.get(&pane_id)?;
        Some(pane_attach_data_ref(session_id, pane_id, pane))
    }

    fn pane_data_refs_for_images(
        &self,
        session_id: SessionId,
        pane_ids: &[Uuid],
    ) -> Vec<PaneAttachDataRef> {
        let Some(session) = self.runtimes.get(&session_id) else {
            return Vec::new();
        };
        pane_ids
            .iter()
            .filter_map(|pane_id| {
                session
                    .panes
                    .get(pane_id)
                    .map(|pane| pane_attach_data_ref(session_id, *pane_id, pane))
            })
            .collect()
    }

    fn persistence_snapshot_parts(
        &self,
        session_id: SessionId,
    ) -> anyhow::Result<Option<PersistenceSnapshotParts>> {
        let Some(runtime) = self.runtimes.get(&session_id) else {
            return Ok(None);
        };
        validate_runtime_layout_matches_panes(runtime).with_context(|| {
            format!(
                "cannot snapshot inconsistent layout for session {}",
                session_id.0
            )
        })?;
        let pane_ids = ordered_pane_ids(runtime);
        let mut panes = Vec::with_capacity(pane_ids.len());
        for pane_id in pane_ids {
            let Some(pane) = runtime.panes.get(&pane_id) else {
                anyhow::bail!(
                    "layout references missing pane {pane_id} in session {}",
                    session_id.0
                );
            };
            panes.push(pane_persistence_data_ref(pane));
        }
        Ok(Some(PersistenceSnapshotParts {
            focused_pane_id: runtime.focused_pane_id,
            layout_root: runtime.layout_root.clone(),
            floating_surfaces: runtime.floating_surfaces.clone(),
            attached_clients: runtime.attached_clients.clone(),
            attach_viewport: runtime.attach_viewport,
            panes,
        }))
    }

    #[allow(dead_code, clippy::cast_possible_truncation)]
    fn attach_snapshot_state(
        &mut self,
        session_id: SessionId,
        client_id: ClientId,
        max_bytes_per_pane: usize,
    ) -> Result<AttachSnapshotState, SessionRuntimeError> {
        if !self
            .runtimes
            .get(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?
            .attached_clients
            .contains(&client_id)
        {
            return Err(SessionRuntimeError::NotAttached);
        }
        let scene = self.build_attach_scene_for_client(session_id, client_id)?;
        let pane_refs = self.attach_scene_pane_refs(session_id, &scene);
        let focused_pane_id = focused_pane_for_scene(
            &scene,
            self.runtimes
                .get(&session_id)
                .map_or(Uuid::nil(), |runtime| runtime.focused_pane_id),
        );
        let panes = pane_refs
            .iter()
            .enumerate()
            .filter_map(|(index, (owner_session_id, pane_id))| {
                self.runtimes
                    .get(owner_session_id)?
                    .panes
                    .get(pane_id)
                    .map(|pane| PaneSummary {
                        id: *pane_id,
                        index: (index + 1) as u32,
                        name: pane.meta.name.clone(),
                        focused: *pane_id == focused_pane_id,
                        state: pane_state_for_handle(pane),
                        state_reason: pane_state_reason_for_handle(pane),
                    })
            })
            .collect::<Vec<_>>();

        let mut chunks = Vec::new();
        let mut pane_mouse_protocols = Vec::new();
        let mut pane_input_modes = Vec::new();
        let num_panes = pane_refs.len().max(1);
        let per_pane_budget = (RESPONSE_OUTPUT_BUDGET / num_panes).min(max_bytes_per_pane);
        let mut budget_remaining = RESPONSE_OUTPUT_BUDGET;
        for (owner_session_id, pane_id) in pane_refs {
            let Some(pane) = self
                .runtimes
                .get_mut(&owner_session_id)
                .and_then(|runtime| runtime.panes.get_mut(&pane_id))
            else {
                continue;
            };
            let protocol = pane
                .mouse_protocol_state
                .lock()
                .map(|state| *state)
                .unwrap_or_default();
            pane_mouse_protocols.push(AttachPaneMouseProtocol { pane_id, protocol });
            let mode = pane
                .input_mode_state
                .lock()
                .map(|state| *state)
                .unwrap_or_default();
            pane_input_modes.push(AttachPaneInputMode { pane_id, mode });
            let allowed = per_pane_budget.min(budget_remaining);
            let mut output = pane
                .output_buffer
                .lock()
                .map_err(|_| SessionRuntimeError::Closed)?;
            let read = output.read_recent_with_offsets(allowed);
            output.advance_client_to_end(client_id);
            drop(output);

            budget_remaining = budget_remaining.saturating_sub(read.bytes.len());
            pane.output_dirty.store(false, Ordering::SeqCst);
            let sync_update_active = pane.sync_update_in_progress.load(Ordering::SeqCst);
            chunks.push(AttachPaneChunk {
                pane_id,
                data: read.bytes,
                stream_start: read.stream_start,
                stream_end: read.stream_end,
                stream_gap: read.stream_gap,
                sync_update_active,
            });
        }

        let session = self
            .runtimes
            .get(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?;
        Ok(AttachSnapshotState {
            focused_pane_id,
            panes,
            layout_root: fallback_ipc_layout(session),
            scene,
            chunks,
            pane_mouse_protocols,
            pane_input_modes,
            zoomed: session.zoomed_pane_id.is_some(),
        })
    }

    #[allow(dead_code)]
    fn read_pane_output_batch(
        &mut self,
        session_id: SessionId,
        client_id: ClientId,
        pane_ids: &[Uuid],
        max_bytes: usize,
    ) -> Result<Vec<AttachPaneChunk>, SessionRuntimeError> {
        let chunks = {
            if !self
                .runtimes
                .get(&session_id)
                .ok_or(SessionRuntimeError::NotFound)?
                .attached_clients
                .contains(&client_id)
            {
                return Err(SessionRuntimeError::NotAttached);
            }

            let mut chunks = Vec::new();
            let num_panes = pane_ids.len().max(1);
            let per_pane_budget = (RESPONSE_OUTPUT_BUDGET / num_panes).min(max_bytes);
            let mut budget_remaining = RESPONSE_OUTPUT_BUDGET;
            for pane_id in pane_ids {
                let Some(owner_session_id) = self.pane_session_for_attach(session_id, *pane_id)
                else {
                    continue;
                };
                let Some(pane) = self
                    .runtimes
                    .get_mut(&owner_session_id)
                    .and_then(|runtime| runtime.panes.get_mut(pane_id))
                else {
                    continue;
                };
                let allowed = per_pane_budget.min(budget_remaining);
                let mut output = pane
                    .output_buffer
                    .lock()
                    .map_err(|_| SessionRuntimeError::Closed)?;
                let read = output.read_for_client(client_id, allowed);
                drop(output);
                budget_remaining = budget_remaining.saturating_sub(read.bytes.len());
                let sync_update_active = pane.sync_update_in_progress.load(Ordering::SeqCst);
                chunks.push(AttachPaneChunk {
                    pane_id: *pane_id,
                    data: read.bytes,
                    stream_start: read.stream_start,
                    stream_end: read.stream_end,
                    stream_gap: read.stream_gap,
                    sync_update_active,
                });
            }
            chunks
        };

        Ok(chunks)
    }

    #[allow(dead_code)]
    fn attach_pane_snapshot_state(
        &mut self,
        session_id: SessionId,
        client_id: ClientId,
        pane_ids: &[Uuid],
        max_bytes_per_pane: usize,
    ) -> Result<AttachPaneSnapshotState, SessionRuntimeError> {
        if !self
            .runtimes
            .get(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?
            .attached_clients
            .contains(&client_id)
        {
            return Err(SessionRuntimeError::NotAttached);
        }

        let num_panes = pane_ids.len().max(1);
        let per_pane_budget = (RESPONSE_OUTPUT_BUDGET / num_panes).min(max_bytes_per_pane);
        let mut budget_remaining = RESPONSE_OUTPUT_BUDGET;
        let mut chunks = Vec::new();
        let mut pane_mouse_protocols = Vec::new();
        let mut pane_input_modes = Vec::new();
        let mut seen = BTreeSet::new();

        for pane_id in pane_ids {
            if !seen.insert(*pane_id) {
                continue;
            }

            let Some(owner_session_id) = self.pane_session_for_attach(session_id, *pane_id) else {
                continue;
            };
            let Some(pane) = self
                .runtimes
                .get_mut(&owner_session_id)
                .and_then(|runtime| runtime.panes.get_mut(pane_id))
            else {
                continue;
            };

            let protocol = pane
                .mouse_protocol_state
                .lock()
                .map(|state| *state)
                .unwrap_or_default();
            pane_mouse_protocols.push(AttachPaneMouseProtocol {
                pane_id: *pane_id,
                protocol,
            });
            let mode = pane
                .input_mode_state
                .lock()
                .map(|state| *state)
                .unwrap_or_default();
            pane_input_modes.push(AttachPaneInputMode {
                pane_id: *pane_id,
                mode,
            });

            let allowed = per_pane_budget.min(budget_remaining);
            let mut output = pane
                .output_buffer
                .lock()
                .map_err(|_| SessionRuntimeError::Closed)?;
            let read = output.read_recent_with_offsets(allowed);
            output.advance_client_to_end(client_id);
            drop(output);

            budget_remaining = budget_remaining.saturating_sub(read.bytes.len());
            pane.output_dirty.store(false, Ordering::SeqCst);
            let sync_update_active = pane.sync_update_in_progress.load(Ordering::SeqCst);
            chunks.push(AttachPaneChunk {
                pane_id: *pane_id,
                data: read.bytes,
                stream_start: read.stream_start,
                stream_end: read.stream_end,
                stream_gap: read.stream_gap,
                sync_update_active,
            });
        }

        Ok(AttachPaneSnapshotState {
            chunks,
            pane_mouse_protocols,
            pane_input_modes,
        })
    }

    fn remove_runtime(&mut self, session_id: SessionId) -> Result<RemovedRuntime> {
        let runtime = self
            .runtimes
            .remove(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        self.unregister_session_inputs(session_id, runtime.panes.keys().copied());

        Ok(RemovedRuntime {
            session_id,
            handle: runtime,
        })
    }

    fn remove_all_runtimes(&mut self) -> Vec<RemovedRuntime> {
        let runtimes = std::mem::take(&mut self.runtimes);
        if let Ok(mut index) = self.pane_input_index.write() {
            index.clear();
        }
        if let Ok(mut permissions) = self.client_write_permissions.write() {
            permissions.clear();
        }
        runtimes
            .into_iter()
            .map(|(session_id, runtime)| RemovedRuntime {
                session_id,
                handle: runtime,
            })
            .collect()
    }

    fn begin_attach(
        &mut self,
        session_id: SessionId,
        client_id: ClientId,
    ) -> Result<(), SessionRuntimeError> {
        let runtime = self
            .runtimes
            .get_mut(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?;

        let _pane = runtime
            .panes
            .get(&runtime.focused_pane_id)
            .ok_or(SessionRuntimeError::NotFound)?;

        runtime.attached_clients.insert(client_id);
        for pane in runtime.panes.values_mut() {
            let mut output = pane
                .output_buffer
                .lock()
                .map_err(|_| SessionRuntimeError::Closed)?;
            output.register_client_at_tail(client_id);
        }
        if let Some(viewport) = runtime.attach_viewport {
            resize_session_ptys(
                runtime,
                viewport.cols,
                viewport.rows,
                viewport.status_top_inset,
                viewport.status_bottom_inset,
            );
        }
        Ok(())
    }

    fn end_attach(&mut self, session_id: SessionId, client_id: ClientId) {
        if let Some(runtime) = self.runtimes.get_mut(&session_id) {
            let removed = runtime.attached_clients.remove(&client_id);
            if removed {
                for pane in runtime.panes.values_mut() {
                    if let Ok(mut output) = pane.output_buffer.lock() {
                        output.unregister_client(client_id);
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn set_attach_viewport(
        &mut self,
        session_id: SessionId,
        client_id: ClientId,
        cols: u16,
        rows: u16,
        status_top_inset: u16,
        status_bottom_inset: u16,
        cell_pixel_width: u16,
        cell_pixel_height: u16,
    ) -> Result<(u16, u16, u16, u16), SessionRuntimeError> {
        let runtime = self
            .runtimes
            .get_mut(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?;

        if !runtime.attached_clients.contains(&client_id) {
            return Err(SessionRuntimeError::NotAttached);
        }

        let cols = cols.max(1);
        let rows = rows.max(2);
        let mut status_top_inset = status_top_inset.min(1);
        let mut status_bottom_inset = status_bottom_inset.min(1);
        while status_top_inset.saturating_add(status_bottom_inset) >= rows {
            if status_bottom_inset > 0 {
                status_bottom_inset -= 1;
            } else if status_top_inset > 0 {
                status_top_inset -= 1;
            } else {
                break;
            }
        }
        runtime.attach_viewport = Some(AttachViewport {
            cols,
            rows,
            status_top_inset,
            status_bottom_inset,
        });
        resize_session_ptys(runtime, cols, rows, status_top_inset, status_bottom_inset);

        // Update cell pixel dimensions for image placement sizing.
        #[cfg(feature = "image-registry")]
        if cell_pixel_width > 0 && cell_pixel_height > 0 {
            for pane in runtime.panes.values() {
                if let Ok(mut size) = pane.cell_pixel_size.lock() {
                    *size = (cell_pixel_width, cell_pixel_height);
                }
            }
        }
        #[cfg(not(feature = "image-registry"))]
        let _ = (cell_pixel_width, cell_pixel_height);

        Ok((cols, rows, status_top_inset, status_bottom_inset))
    }

    fn apply_stored_attach_viewport(&mut self, session_id: SessionId) {
        let Some(runtime) = self.runtimes.get_mut(&session_id) else {
            return;
        };
        let Some(viewport) = runtime.attach_viewport else {
            return;
        };
        resize_session_ptys(
            runtime,
            viewport.cols,
            viewport.rows,
            viewport.status_top_inset,
            viewport.status_bottom_inset,
        );
    }

    fn write_input(
        &mut self,
        session_id: SessionId,
        client_id: ClientId,
        data: Vec<u8>,
    ) -> Result<(usize, Uuid), SessionRuntimeError> {
        let fallback_focused_pane_id = {
            let runtime = self
                .runtimes
                .get(&session_id)
                .ok_or(SessionRuntimeError::NotFound)?;
            if !runtime.attached_clients.contains(&client_id) {
                return Err(SessionRuntimeError::NotAttached);
            }
            runtime.focused_pane_id
        };

        let scene = self.build_attach_scene_for_client(session_id, client_id)?;
        let focused_pane_id = match scene.focus {
            AttachFocusTarget::Pane { pane_id } => pane_id,
            AttachFocusTarget::None | AttachFocusTarget::Surface { .. } => fallback_focused_pane_id,
        };
        let owner_session_id = self
            .pane_session_for_attach(session_id, focused_pane_id)
            .ok_or(SessionRuntimeError::NotFound)?;
        let pane = self
            .runtimes
            .get_mut(&owner_session_id)
            .and_then(|owner_runtime| owner_runtime.panes.get_mut(&focused_pane_id))
            .ok_or(SessionRuntimeError::NotFound)?;

        if pane.exited.load(Ordering::SeqCst) {
            return Err(SessionRuntimeError::Closed);
        }

        let bytes = data.len();
        pane.send_input(data)?;
        Ok((bytes, focused_pane_id))
    }

    fn read_output(
        &mut self,
        session_id: SessionId,
        client_id: ClientId,
        max_bytes: usize,
    ) -> Result<Vec<u8>, SessionRuntimeError> {
        let runtime = self
            .runtimes
            .get_mut(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?;

        if !runtime.attached_clients.contains(&client_id) {
            return Err(SessionRuntimeError::NotAttached);
        }

        let pane = runtime
            .panes
            .get_mut(&runtime.focused_pane_id)
            .ok_or(SessionRuntimeError::NotFound)?;

        if max_bytes == 0 {
            return Ok(Vec::new());
        }

        let mut output = pane
            .output_buffer
            .lock()
            .map_err(|_| SessionRuntimeError::Closed)?;
        let read = output.read_for_client(client_id, max_bytes);
        drop(output);

        Ok(read.bytes)
    }
}

fn resolve_pane_id_from_selector(
    runtime: &SessionRuntimeHandle,
    selector: &PaneSelector,
) -> Option<Uuid> {
    match selector {
        PaneSelector::Active => runtime
            .panes
            .contains_key(&runtime.focused_pane_id)
            .then_some(runtime.focused_pane_id),
        PaneSelector::ById(id) => runtime.panes.contains_key(id).then_some(*id),
        PaneSelector::ByIndex(index) => {
            if *index == 0 {
                return None;
            }
            let pane_ids = ordered_pane_ids(runtime);
            let position = usize::try_from(index.saturating_sub(1)).ok()?;
            let pane_id = pane_ids.get(position).copied()?;
            runtime.panes.contains_key(&pane_id).then_some(pane_id)
        }
    }
}

fn pane_state_for_handle(pane: &PaneRuntimeHandle) -> PaneState {
    if pane.exited.load(Ordering::SeqCst) {
        PaneState::Exited
    } else {
        PaneState::Running
    }
}

fn pane_state_reason_for_handle(pane: &PaneRuntimeHandle) -> Option<String> {
    pane.exit_reason
        .lock()
        .ok()
        .and_then(|reason| reason.clone())
}

//
// Inline adapter implementing `SessionRuntimeManagerApi` over the
// server's own `Arc<Mutex<SessionRuntimeManager>>`. Registered into
// the plugin state registry during `BmuxServer::new` so
// `session_runtime_handle()` lookups reach this single manager
// instance.
//
// Kept in `lib.rs` to access the many private fields/methods of
// `SessionRuntimeManager`, `SessionRuntimeHandle`, and `PaneRuntimeHandle`
// without widening their visibility to `pub(crate)`. When the
// `SessionRuntimeManager` implementation finally relocates into the
// pane-runtime plugin crate, this adapter and its registration move
// with it.

struct ManagerLockHolder {
    operation: &'static str,
    started: std::time::Instant,
}

struct ServerSessionRuntimeAdapter {
    inner: Arc<Mutex<SessionRuntimeManager>>,
    pane_input_index: Arc<RwLock<BTreeMap<Uuid, PaneInputHandle>>>,
    client_write_permissions: Arc<RwLock<BTreeMap<SessionId, BTreeSet<ClientId>>>>,
    current_holder: Arc<Mutex<Option<ManagerLockHolder>>>,
}

impl ServerSessionRuntimeAdapter {
    fn new(inner: Arc<Mutex<SessionRuntimeManager>>) -> Self {
        let (pane_input_index, client_write_permissions) = inner.lock().map_or_else(
            |_| {
                (
                    Arc::new(RwLock::new(BTreeMap::new())),
                    Arc::new(RwLock::new(BTreeMap::new())),
                )
            },
            |manager| {
                (
                    Arc::clone(&manager.pane_input_index),
                    Arc::clone(&manager.client_write_permissions),
                )
            },
        );
        Self {
            inner,
            pane_input_index,
            client_write_permissions,
            current_holder: Arc::new(Mutex::new(None)),
        }
    }

    fn current_holder_snapshot(&self) -> Option<(&'static str, u128)> {
        self.current_holder.lock().ok().and_then(|holder| {
            holder
                .as_ref()
                .map(|h| (h.operation, h.started.elapsed().as_millis()))
        })
    }

    fn mark_holder_start(&self, operation: &'static str) {
        if let Ok(mut holder) = self.current_holder.lock() {
            *holder = Some(ManagerLockHolder {
                operation,
                started: std::time::Instant::now(),
            });
        }
    }

    fn mark_holder_end(&self) {
        if let Ok(mut holder) = self.current_holder.lock() {
            *holder = None;
        }
    }

    fn with_lock<R>(&self, f: impl FnOnce(&mut SessionRuntimeManager) -> R) -> Option<R> {
        self.with_lock_named("unknown", f)
    }

    fn with_lock_named<R>(
        &self,
        operation: &'static str,
        f: impl FnOnce(&mut SessionRuntimeManager) -> R,
    ) -> Option<R> {
        let wait_started = std::time::Instant::now();
        let mut guard = self.inner.lock().ok()?;
        let wait = wait_started.elapsed();
        if wait > Duration::from_millis(10) {
            let holder = self.current_holder_snapshot();
            warn!(
                operation,
                wait_ms = wait.as_millis(),
                holder_operation = holder.map(|(op, _)| op),
                holder_ms = holder.map(|(_, ms)| ms),
                "pane runtime manager lock wait exceeded hot-path budget"
            );
        }
        self.mark_holder_start(operation);
        let hold_started = std::time::Instant::now();
        let result = f(&mut guard);
        let hold = hold_started.elapsed();
        self.mark_holder_end();
        if hold > Duration::from_millis(10) {
            warn!(
                operation,
                hold_ms = hold.as_millis(),
                "pane runtime manager lock hold exceeded hot-path budget"
            );
        }
        Some(result)
    }

    fn with_lock_read<R>(&self, f: impl FnOnce(&SessionRuntimeManager) -> R) -> Option<R> {
        self.with_lock_read_named("unknown", f)
    }

    fn with_lock_read_named<R>(
        &self,
        operation: &'static str,
        f: impl FnOnce(&SessionRuntimeManager) -> R,
    ) -> Option<R> {
        let wait_started = std::time::Instant::now();
        let guard = self.inner.lock().ok()?;
        let wait = wait_started.elapsed();
        if wait > Duration::from_millis(10) {
            let holder = self.current_holder_snapshot();
            warn!(
                operation,
                wait_ms = wait.as_millis(),
                holder_operation = holder.map(|(op, _)| op),
                holder_ms = holder.map(|(_, ms)| ms),
                "pane runtime manager read lock wait exceeded hot-path budget"
            );
        }
        self.mark_holder_start(operation);
        let hold_started = std::time::Instant::now();
        let result = f(&guard);
        let hold = hold_started.elapsed();
        self.mark_holder_end();
        if hold > Duration::from_millis(10) {
            warn!(
                operation,
                hold_ms = hold.as_millis(),
                "pane runtime manager read lock hold exceeded hot-path budget"
            );
        }
        Some(result)
    }

    fn remove_to_info(
        session_id: SessionId,
        handle: SessionRuntimeHandle,
    ) -> bmux_pane_runtime_state::RemovedRuntimeInfo {
        let attached = handle.attached_clients.clone();
        let boxed: Box<dyn std::any::Any + Send + 'static> = Box::new(handle);
        bmux_pane_runtime_state::RemovedRuntimeInfo {
            session_id,
            attached_clients: attached,
            shutdown_token: Arc::new(Mutex::new(Some(boxed))),
        }
    }

    fn lock_poisoned_anyhow() -> anyhow::Error {
        anyhow::anyhow!("session runtime manager lock poisoned")
    }

    fn take_shutdown_handle(
        info: &bmux_pane_runtime_state::RemovedRuntimeInfo,
    ) -> Option<SessionRuntimeHandle> {
        let boxed = {
            let mut guard = info.shutdown_token.lock().ok()?;
            guard.take()?
        };
        boxed.downcast::<SessionRuntimeHandle>().ok().map(|b| *b)
    }
}

impl bmux_pane_runtime_state::SessionRuntimeManagerApi for ServerSessionRuntimeAdapter {
    fn start_runtime(&self, session_id: SessionId) -> anyhow::Result<()> {
        self.with_lock(|m| m.start_runtime(session_id))
            .ok_or_else(Self::lock_poisoned_anyhow)?
    }

    fn restore_runtime(
        &self,
        session_id: SessionId,
        panes: &[bmux_pane_runtime_state::PaneRuntimeMeta],
        layout_root: Option<bmux_pane_runtime_state::PaneLayoutNode>,
        focused_pane_id: Uuid,
        floating_surfaces: Vec<bmux_pane_runtime_state::FloatingSurfaceRuntime>,
        attach_viewport: Option<bmux_pane_runtime_state::AttachViewport>,
    ) -> anyhow::Result<()> {
        self.with_lock(|m| {
            m.restore_runtime(
                session_id,
                panes,
                layout_root,
                focused_pane_id,
                floating_surfaces,
                attach_viewport,
            )
        })
        .ok_or_else(Self::lock_poisoned_anyhow)?
    }

    fn remove_runtime(
        &self,
        session_id: SessionId,
    ) -> Option<bmux_pane_runtime_state::RemovedRuntimeInfo> {
        let removed = self
            .with_lock(|m| m.remove_runtime(session_id).ok())
            .flatten()?;
        Some(Self::remove_to_info(removed.session_id, removed.handle))
    }

    fn remove_all_runtimes(&self) -> Vec<bmux_pane_runtime_state::RemovedRuntimeInfo> {
        let Some(removed) = self.with_lock(SessionRuntimeManager::remove_all_runtimes) else {
            return Vec::new();
        };
        removed
            .into_iter()
            .map(|r| Self::remove_to_info(r.session_id, r.handle))
            .collect()
    }

    fn session_exists(&self, session_id: SessionId) -> bool {
        self.with_lock_read(|m| m.runtimes.contains_key(&session_id))
            .unwrap_or(false)
    }

    fn active_session_ids(&self) -> Vec<SessionId> {
        self.with_lock_read(SessionRuntimeManager::active_session_ids)
            .unwrap_or_default()
    }

    fn split_pane(
        &self,
        session_id: SessionId,
        target: Option<PaneSelector>,
        direction: PaneSplitDirection,
    ) -> anyhow::Result<Uuid> {
        self.with_lock(|m| m.split_pane(session_id, target, direction))
            .ok_or_else(Self::lock_poisoned_anyhow)?
    }

    fn launch_pane(
        &self,
        session_id: SessionId,
        target: Option<PaneSelector>,
        direction: PaneSplitDirection,
        name: Option<String>,
        command: PaneLaunchCommand,
    ) -> anyhow::Result<Uuid> {
        self.with_lock(|m| m.launch_pane(session_id, target, direction, name, command))
            .ok_or_else(Self::lock_poisoned_anyhow)?
    }

    fn focus_pane(
        &self,
        session_id: SessionId,
        direction: PaneFocusDirection,
    ) -> anyhow::Result<Uuid> {
        self.with_lock(|m| m.focus_pane(session_id, direction))
            .ok_or_else(Self::lock_poisoned_anyhow)?
    }

    fn focus_pane_target(
        &self,
        session_id: SessionId,
        target: &PaneSelector,
    ) -> anyhow::Result<Uuid> {
        self.with_lock(|m| m.focus_pane_target(session_id, target))
            .ok_or_else(Self::lock_poisoned_anyhow)?
    }

    fn resize_pane(
        &self,
        session_id: SessionId,
        target: Option<PaneSelector>,
        direction: PaneResizeDirection,
        cells: u16,
    ) -> anyhow::Result<()> {
        self.with_lock(|m| m.resize_pane(session_id, target, direction, cells))
            .ok_or_else(Self::lock_poisoned_anyhow)?
    }

    fn close_pane(
        &self,
        session_id: SessionId,
        target: Option<PaneSelector>,
    ) -> anyhow::Result<(Uuid, Option<bmux_pane_runtime_state::RemovedRuntimeInfo>)> {
        let (pane_id, removed) = self
            .with_lock(|m| m.close_pane(session_id, target))
            .ok_or_else(Self::lock_poisoned_anyhow)??;
        let info = removed.map(|r| Self::remove_to_info(r.session_id, r.handle));
        Ok((pane_id, info))
    }

    fn restart_pane(
        &self,
        session_id: SessionId,
        target: Option<PaneSelector>,
    ) -> anyhow::Result<Uuid> {
        self.with_lock(|m| m.restart_pane(session_id, target))
            .ok_or_else(Self::lock_poisoned_anyhow)?
    }

    fn toggle_zoom(&self, session_id: SessionId) -> anyhow::Result<(Uuid, bool)> {
        self.with_lock(|m| m.toggle_zoom(session_id))
            .ok_or_else(Self::lock_poisoned_anyhow)?
    }

    fn create_floating_pane(
        &self,
        session_id: SessionId,
        target: Option<PaneSelector>,
        rect: LayoutRect,
        scope: FloatingPaneScope,
        layer: FloatingPaneLayer,
        z: i32,
        name: Option<String>,
        command: Option<PaneLaunchCommand>,
        anchor_pane_id: Option<Uuid>,
        context_id: Option<Uuid>,
        client_id: Option<ClientId>,
    ) -> anyhow::Result<FloatingPaneRuntimeSummary> {
        self.with_lock(|m| {
            m.create_floating_pane(
                session_id,
                target,
                rect,
                scope,
                layer,
                z,
                name,
                command,
                anchor_pane_id,
                context_id,
                client_id,
            )
        })
        .ok_or_else(Self::lock_poisoned_anyhow)?
    }

    fn list_floating_panes(
        &self,
        session_id: SessionId,
    ) -> anyhow::Result<Vec<FloatingPaneRuntimeSummary>> {
        self.with_lock_read(|m| m.list_floating_panes(session_id))
            .ok_or_else(Self::lock_poisoned_anyhow)?
    }

    fn move_floating_pane(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
        x: u16,
        y: u16,
    ) -> anyhow::Result<FloatingPaneRuntimeSummary> {
        self.with_lock(|m| m.move_floating_pane(session_id, pane_id, x, y))
            .ok_or_else(Self::lock_poisoned_anyhow)?
    }

    fn resize_floating_pane(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
        w: u16,
        h: u16,
    ) -> anyhow::Result<FloatingPaneRuntimeSummary> {
        self.with_lock(|m| m.resize_floating_pane(session_id, pane_id, w, h))
            .ok_or_else(Self::lock_poisoned_anyhow)?
    }

    fn focus_floating_pane(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
    ) -> anyhow::Result<FloatingPaneRuntimeSummary> {
        self.with_lock(|m| m.focus_floating_pane(session_id, pane_id))
            .ok_or_else(Self::lock_poisoned_anyhow)?
    }

    fn raise_floating_pane(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
    ) -> anyhow::Result<FloatingPaneRuntimeSummary> {
        self.with_lock(|m| m.raise_floating_pane(session_id, pane_id))
            .ok_or_else(Self::lock_poisoned_anyhow)?
    }

    fn lower_floating_pane(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
    ) -> anyhow::Result<FloatingPaneRuntimeSummary> {
        self.with_lock(|m| m.lower_floating_pane(session_id, pane_id))
            .ok_or_else(Self::lock_poisoned_anyhow)?
    }

    fn set_floating_pane_z(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
        z: i32,
    ) -> anyhow::Result<FloatingPaneRuntimeSummary> {
        self.with_lock(|m| m.set_floating_pane_z(session_id, pane_id, z))
            .ok_or_else(Self::lock_poisoned_anyhow)?
    }

    fn set_floating_pane_layer(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
        layer: FloatingPaneLayer,
    ) -> anyhow::Result<FloatingPaneRuntimeSummary> {
        self.with_lock(|m| m.set_floating_pane_layer(session_id, pane_id, layer))
            .ok_or_else(Self::lock_poisoned_anyhow)?
    }

    fn close_floating_pane(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
    ) -> anyhow::Result<(Uuid, Option<bmux_pane_runtime_state::RemovedRuntimeInfo>)> {
        let (pane_id, removed) = self
            .with_lock(|m| m.close_floating_pane(session_id, pane_id))
            .ok_or_else(Self::lock_poisoned_anyhow)??;
        let info = removed.map(|r| Self::remove_to_info(r.session_id, r.handle));
        Ok((pane_id, info))
    }

    fn list_panes(&self, session_id: SessionId) -> anyhow::Result<Vec<PaneSummary>> {
        self.with_lock_read(|m| m.list_panes(session_id))
            .ok_or_else(Self::lock_poisoned_anyhow)?
    }

    fn list_pane_processes(&self) -> Vec<bmux_pane_runtime_state::PaneProcessIdentity> {
        self.with_lock_read(|m| {
            let mut identities = Vec::new();
            for (session_id, runtime) in &m.runtimes {
                for (pane_id, pane) in &runtime.panes {
                    identities.push(bmux_pane_runtime_state::PaneProcessIdentity {
                        session_id: *session_id,
                        pane_id: *pane_id,
                        pid: pane.process_id.lock().ok().and_then(|pid| *pid),
                        process_group_id: pane.process_group_id.lock().ok().and_then(|pgid| *pgid),
                    });
                }
            }
            identities
        })
        .unwrap_or_default()
    }

    fn pane_process_identity(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
    ) -> Option<bmux_pane_runtime_state::PaneProcessIdentity> {
        self.with_lock_read(|m| {
            let runtime = m.runtimes.get(&session_id)?;
            let pane = runtime.panes.get(&pane_id)?;
            Some(bmux_pane_runtime_state::PaneProcessIdentity {
                session_id,
                pane_id,
                pid: pane.process_id.lock().ok().and_then(|pid| *pid),
                process_group_id: pane.process_group_id.lock().ok().and_then(|pgid| *pgid),
            })
        })
        .flatten()
    }

    fn write_input(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        data: Vec<u8>,
    ) -> Result<(usize, Uuid), SessionRuntimeError> {
        self.with_lock(|m| m.write_input(session_id, client_id, data))
            .unwrap_or(Err(SessionRuntimeError::Closed))
    }

    fn write_input_to_pane(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
        data: Vec<u8>,
    ) -> Result<usize, SessionRuntimeError> {
        let lookup_started = std::time::Instant::now();
        let pane = self
            .pane_input_index
            .read()
            .ok()
            .and_then(|index| index.get(&pane_id).cloned())
            .filter(|handle| handle.session_id == session_id)
            .ok_or(SessionRuntimeError::NotFound)?;
        let lookup = lookup_started.elapsed();
        if lookup > Duration::from_millis(2) {
            warn!(session_id = %session_id.0, %pane_id, lookup_us = lookup.as_micros(), "pane direct input lookup exceeded hot-path budget");
        }

        let bytes = data.len();
        let send_started = std::time::Instant::now();
        pane.send_input(data)?;
        let send = send_started.elapsed();
        if send > Duration::from_millis(2) {
            warn!(session_id = %session_id.0, %pane_id, send_us = send.as_micros(), "pane direct input send exceeded hot-path budget");
        }
        Ok(bytes)
    }

    fn set_client_write_permission(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        allowed: bool,
    ) {
        if let Ok(mut permissions) = self.client_write_permissions.write() {
            let writers = permissions.entry(session_id).or_default();
            if allowed {
                writers.insert(client_id);
            } else {
                writers.remove(&client_id);
            }
        }
    }

    fn client_can_write(&self, session_id: SessionId, client_id: ClientId) -> bool {
        self.client_write_permissions
            .read()
            .ok()
            .and_then(|permissions| permissions.get(&session_id).cloned())
            .is_some_and(|writers| writers.contains(&client_id))
    }

    fn read_output(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        max_bytes: usize,
    ) -> Result<Vec<u8>, SessionRuntimeError> {
        self.with_lock(|m| m.read_output(session_id, client_id, max_bytes))
            .unwrap_or(Err(SessionRuntimeError::Closed))
    }

    fn read_pane_output_batch(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        pane_ids: &[Uuid],
        max_bytes: usize,
    ) -> Result<Vec<AttachPaneChunk>, SessionRuntimeError> {
        let panes = self
            .with_lock_read_named("read_pane_output_batch.resolve", |m| {
                m.attach_pane_data_refs_for_scene_panes(session_id, client_id, pane_ids)
            })
            .unwrap_or(Err(SessionRuntimeError::Closed))?;
        let num_panes = pane_ids.len().max(1);
        let per_pane_budget = (RESPONSE_OUTPUT_BUDGET / num_panes).min(max_bytes);
        let mut budget_remaining = RESPONSE_OUTPUT_BUDGET;
        let mut chunks = Vec::new();
        for pane in panes {
            let allowed = per_pane_budget.min(budget_remaining);
            let chunk = attach_pane_chunk_for_client(&pane, client_id, allowed)?;
            budget_remaining = budget_remaining.saturating_sub(chunk.data.len());
            chunks.push(chunk);
        }
        Ok(chunks)
    }

    fn attach_pane_output_batch_with_dirty_check(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        pane_ids: &[Uuid],
        max_bytes: usize,
    ) -> (Result<Vec<AttachPaneChunk>, SessionRuntimeError>, bool) {
        let panes = match self
            .with_lock_read_named("attach_pane_output_batch.resolve", |m| {
                m.attach_pane_data_refs_for_scene_panes(session_id, client_id, pane_ids)
            })
            .unwrap_or(Err(SessionRuntimeError::Closed))
        {
            Ok(panes) => panes,
            Err(err) => return (Err(err), false),
        };
        for pane in &panes {
            pane.output_dirty.store(false, Ordering::SeqCst);
        }
        let num_panes = pane_ids.len().max(1);
        let per_pane_budget = (RESPONSE_OUTPUT_BUDGET / num_panes).min(max_bytes);
        let mut budget_remaining = RESPONSE_OUTPUT_BUDGET;
        let mut chunks = Vec::new();
        for pane in &panes {
            let allowed = per_pane_budget.min(budget_remaining);
            match attach_pane_chunk_for_client(pane, client_id, allowed) {
                Ok(chunk) => {
                    budget_remaining = budget_remaining.saturating_sub(chunk.data.len());
                    chunks.push(chunk);
                }
                Err(err) => return (Err(err), false),
            }
        }
        let still_pending = panes
            .iter()
            .any(|pane| pane.output_dirty.load(Ordering::SeqCst));
        (Ok(chunks), still_pending)
    }

    fn attach_pane_image_deltas(
        &self,
        session_id: SessionId,
        pane_ids: &[Uuid],
        since_sequences: &[u64],
        payload_codec: Option<&dyn bmux_ipc::compression::CompressionCodec>,
    ) -> Vec<bmux_attach_image_protocol::AttachPaneImageDelta> {
        let panes = self
            .with_lock_read_named("attach_pane_image_deltas.resolve", |m| {
                m.pane_data_refs_for_images(session_id, pane_ids)
            })
            .unwrap_or_default();
        let pane_by_id = panes
            .into_iter()
            .map(|pane| (pane.pane_id, pane))
            .collect::<BTreeMap<_, _>>();
        let mut result = Vec::new();
        for (i, pane_id) in pane_ids.iter().enumerate() {
            let Some(pane) = pane_by_id.get(pane_id) else {
                continue;
            };
            #[cfg(feature = "image-registry")]
            pane.image_dirty.store(false, Ordering::SeqCst);
            let since = since_sequences.get(i).copied().unwrap_or(0);
            #[cfg(feature = "image-registry")]
            if let Ok(reg) = pane.image_registry.lock() {
                let delta = reg.delta_since(since);
                result.push(delta.to_ipc(*pane_id, payload_codec));
            }
            #[cfg(not(feature = "image-registry"))]
            {
                let _ = (pane, since, payload_codec);
                result.push(bmux_attach_image_protocol::AttachPaneImageDelta {
                    pane_id: *pane_id,
                    added: Vec::new(),
                    removed: Vec::new(),
                    sequence: 0,
                });
            }
        }
        result
    }

    fn begin_attach(
        &self,
        session_id: SessionId,
        client_id: ClientId,
    ) -> Result<(), SessionRuntimeError> {
        self.with_lock(|m| m.begin_attach(session_id, client_id))
            .unwrap_or(Err(SessionRuntimeError::Closed))
    }

    fn end_attach(&self, session_id: SessionId, client_id: ClientId) {
        let _ = self.with_lock(|m| m.end_attach(session_id, client_id));
    }

    fn set_attach_viewport(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        cols: u16,
        rows: u16,
        status_top_inset: u16,
        status_bottom_inset: u16,
        cell_pixel_width: u16,
        cell_pixel_height: u16,
    ) -> Result<(u16, u16, u16, u16), SessionRuntimeError> {
        self.with_lock(|m| {
            m.set_attach_viewport(
                session_id,
                client_id,
                cols,
                rows,
                status_top_inset,
                status_bottom_inset,
                cell_pixel_width,
                cell_pixel_height,
            )
        })
        .unwrap_or(Err(SessionRuntimeError::Closed))
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "implements the trait's atomic attach-retarget operation with viewport dimensions"
    )]
    fn retarget_attach_stream(
        &self,
        previous_session_id: Option<SessionId>,
        next_session_id: SessionId,
        client_id: ClientId,
        cols: u16,
        rows: u16,
        status_top_inset: u16,
        status_bottom_inset: u16,
        cell_pixel_width: u16,
        cell_pixel_height: u16,
    ) -> Result<(u16, u16, u16, u16), SessionRuntimeError> {
        self.with_lock(|m| {
            if let Some(previous_session_id) = previous_session_id
                && previous_session_id != next_session_id
            {
                m.end_attach(previous_session_id, client_id);
            }
            m.begin_attach(next_session_id, client_id)?;
            m.set_attach_viewport(
                next_session_id,
                client_id,
                cols,
                rows,
                status_top_inset,
                status_bottom_inset,
                cell_pixel_width,
                cell_pixel_height,
            )
        })
        .unwrap_or(Err(SessionRuntimeError::Closed))
    }

    fn apply_stored_attach_viewport(&self, session_id: SessionId) {
        let _ = self.with_lock(|m| m.apply_stored_attach_viewport(session_id));
    }

    fn attach_layout_state(
        &self,
        session_id: SessionId,
        client_id: ClientId,
    ) -> Result<bmux_pane_runtime_state::AttachLayoutState, SessionRuntimeError> {
        let snapshot = self
            .with_lock_read_named("attach_layout_state.snapshot", |m| {
                m.attach_scene_data_snapshot(session_id, client_id)
            })
            .unwrap_or(Err(SessionRuntimeError::Closed))?;
        let panes = snapshot
            .panes
            .iter()
            .enumerate()
            .map(|(index, pane)| pane_summary_for_data_ref(index, snapshot.focused_pane_id, pane))
            .collect();
        Ok(bmux_pane_runtime_state::AttachLayoutState {
            focused_pane_id: snapshot.focused_pane_id,
            panes,
            layout_root: snapshot.layout_root,
            scene: snapshot.scene,
            zoomed: snapshot.zoomed,
        })
    }

    fn attach_snapshot_state(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        max_bytes_per_pane: usize,
    ) -> Result<bmux_pane_runtime_state::AttachSnapshotState, SessionRuntimeError> {
        let snapshot = self
            .with_lock_read_named("attach_snapshot_state.snapshot", |m| {
                m.attach_scene_data_snapshot(session_id, client_id)
            })
            .unwrap_or(Err(SessionRuntimeError::Closed))?;
        let panes = snapshot
            .panes
            .iter()
            .enumerate()
            .map(|(index, pane)| pane_summary_for_data_ref(index, snapshot.focused_pane_id, pane))
            .collect::<Vec<_>>();
        let mut chunks = Vec::new();
        let mut pane_mouse_protocols = Vec::new();
        let mut pane_input_modes = Vec::new();
        let num_panes = snapshot.panes.len().max(1);
        let per_pane_budget = (RESPONSE_OUTPUT_BUDGET / num_panes).min(max_bytes_per_pane);
        let mut budget_remaining = RESPONSE_OUTPUT_BUDGET;
        for pane in &snapshot.panes {
            let protocol = pane
                .mouse_protocol_state
                .lock()
                .map(|state| *state)
                .unwrap_or_default();
            pane_mouse_protocols.push(AttachPaneMouseProtocol {
                pane_id: pane.pane_id,
                protocol,
            });
            let mode = pane
                .input_mode_state
                .lock()
                .map(|state| *state)
                .unwrap_or_default();
            pane_input_modes.push(AttachPaneInputMode {
                pane_id: pane.pane_id,
                mode,
            });
            let allowed = per_pane_budget.min(budget_remaining);
            let chunk = attach_pane_recent_chunk(pane, client_id, allowed)?;
            budget_remaining = budget_remaining.saturating_sub(chunk.data.len());
            chunks.push(chunk);
        }
        Ok(bmux_pane_runtime_state::AttachSnapshotState {
            focused_pane_id: snapshot.focused_pane_id,
            panes,
            layout_root: snapshot.layout_root,
            scene: snapshot.scene,
            zoomed: snapshot.zoomed,
            chunks,
            pane_mouse_protocols,
            pane_input_modes,
        })
    }

    fn attach_pane_snapshot_state(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        pane_ids: &[Uuid],
        max_bytes_per_pane: usize,
    ) -> Result<bmux_pane_runtime_state::AttachPaneSnapshotState, SessionRuntimeError> {
        let panes = self
            .with_lock_read_named("attach_pane_snapshot_state.resolve", |m| {
                m.attach_pane_data_refs_for_scene_panes(session_id, client_id, pane_ids)
            })
            .unwrap_or(Err(SessionRuntimeError::Closed))?;
        let num_panes = pane_ids.len().max(1);
        let per_pane_budget = (RESPONSE_OUTPUT_BUDGET / num_panes).min(max_bytes_per_pane);
        let mut budget_remaining = RESPONSE_OUTPUT_BUDGET;
        let mut chunks = Vec::new();
        let mut pane_mouse_protocols = Vec::new();
        let mut pane_input_modes = Vec::new();
        let mut seen = BTreeSet::new();
        for pane in panes {
            if !seen.insert(pane.pane_id) {
                continue;
            }
            let protocol = pane
                .mouse_protocol_state
                .lock()
                .map(|state| *state)
                .unwrap_or_default();
            pane_mouse_protocols.push(AttachPaneMouseProtocol {
                pane_id: pane.pane_id,
                protocol,
            });
            let mode = pane
                .input_mode_state
                .lock()
                .map(|state| *state)
                .unwrap_or_default();
            pane_input_modes.push(AttachPaneInputMode {
                pane_id: pane.pane_id,
                mode,
            });
            let allowed = per_pane_budget.min(budget_remaining);
            let chunk = attach_pane_recent_chunk(&pane, client_id, allowed)?;
            budget_remaining = budget_remaining.saturating_sub(chunk.data.len());
            chunks.push(chunk);
        }
        Ok(bmux_pane_runtime_state::AttachPaneSnapshotState {
            chunks,
            pane_mouse_protocols,
            pane_input_modes,
        })
    }

    fn attach_grid_snapshot_state(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        pane_ids: &[Uuid],
        max_rows_per_pane: usize,
    ) -> Result<bmux_pane_runtime_state::AttachGridSnapshotState, SessionRuntimeError> {
        let panes = self
            .with_lock_read_named("attach_grid_snapshot_state.resolve", |m| {
                m.attach_pane_data_refs_for_session_panes(session_id, client_id, pane_ids)
            })
            .unwrap_or(Err(SessionRuntimeError::Closed))?;
        let mut snapshots = Vec::new();
        let mut seen = BTreeSet::new();
        for pane in panes {
            if !seen.insert(pane.pane_id) {
                continue;
            }
            let snapshot = pane
                .terminal_grid
                .lock()
                .map_err(|_| SessionRuntimeError::Closed)?
                .snapshot(0, max_rows_per_pane);
            let stream_end = {
                let mut output = pane
                    .output_buffer
                    .lock()
                    .map_err(|_| SessionRuntimeError::Closed)?;
                let stream_end = output.end_offset();
                output.set_client_cursor(client_id, stream_end);
                stream_end
            };
            let encoded = serde_json::to_vec(&snapshot).map_err(|_| SessionRuntimeError::Closed)?;
            snapshots.push(bmux_pane_runtime_state::AttachPaneGridSnapshot {
                pane_id: pane.pane_id,
                stream_end,
                encoded,
            });
        }
        Ok(bmux_pane_runtime_state::AttachGridSnapshotState { snapshots })
    }

    fn attach_grid_window_state(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        windows: &[bmux_pane_runtime_state::AttachPaneGridWindowRequest],
    ) -> Result<bmux_pane_runtime_state::AttachGridWindowState, SessionRuntimeError> {
        let pane_ids = windows
            .iter()
            .map(|window| window.pane_id)
            .collect::<Vec<_>>();
        let panes = self
            .with_lock_read_named("attach_grid_window_state.resolve", |m| {
                m.attach_pane_data_refs_for_scene_panes(session_id, client_id, &pane_ids)
            })
            .unwrap_or(Err(SessionRuntimeError::Closed))?;
        let pane_by_id = panes
            .into_iter()
            .map(|pane| (pane.pane_id, pane))
            .collect::<BTreeMap<_, _>>();
        let mut snapshots = Vec::new();
        for window in windows {
            let Some(pane) = pane_by_id.get(&window.pane_id) else {
                continue;
            };
            let (
                snapshot,
                max_scrollback_offset,
                total_scrolled_rows,
                adjusted_offset,
                desired_offset,
            ) = {
                let grid = pane
                    .terminal_grid
                    .lock()
                    .map_err(|_| SessionRuntimeError::Closed)?;
                let total_scrolled_rows = grid.grid().total_scrolled_rows();
                let max_scrollback_offset = grid.grid().max_scrollback_offset();
                let anchor_growth = window
                    .anchor_total_scrolled_rows
                    .map_or(0, |anchor| total_scrolled_rows.saturating_sub(anchor));
                let desired_offset = window
                    .scrollback_offset
                    .saturating_add(usize::try_from(anchor_growth).unwrap_or(usize::MAX));
                let adjusted_offset = desired_offset.min(max_scrollback_offset);
                (
                    grid.snapshot(adjusted_offset, window.rows),
                    max_scrollback_offset,
                    total_scrolled_rows,
                    adjusted_offset,
                    desired_offset,
                )
            };
            let stream_end = pane
                .output_buffer
                .lock()
                .map_err(|_| SessionRuntimeError::Closed)?
                .end_offset();
            let encoded = serde_json::to_vec(&snapshot).map_err(|_| SessionRuntimeError::Closed)?;
            snapshots.push(bmux_pane_runtime_state::AttachPaneGridWindow {
                pane_id: window.pane_id,
                scrollback_offset: adjusted_offset,
                max_scrollback_offset,
                total_scrolled_rows,
                anchor_delta_rows: adjusted_offset.saturating_sub(window.scrollback_offset),
                anchor_clamped: adjusted_offset < desired_offset,
                stream_end,
                encoded,
            });
        }
        Ok(bmux_pane_runtime_state::AttachGridWindowState { windows: snapshots })
    }

    fn attach_grid_delta_state(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        pane_ids: &[Uuid],
        base_revisions: &[u64],
        max_batches_per_pane: usize,
    ) -> Result<bmux_pane_runtime_state::AttachGridDeltaState, SessionRuntimeError> {
        let panes = self
            .with_lock_read_named("attach_grid_delta_state.resolve", |m| {
                m.attach_pane_data_refs_for_session_panes(session_id, client_id, pane_ids)
            })
            .unwrap_or(Err(SessionRuntimeError::Closed))?;
        let pane_by_id = panes
            .into_iter()
            .map(|pane| (pane.pane_id, pane))
            .collect::<BTreeMap<_, _>>();
        let mut deltas = Vec::new();
        let mut seen = BTreeSet::new();
        for (index, pane_id) in pane_ids.iter().enumerate() {
            if !seen.insert(*pane_id) {
                continue;
            }
            let Some(pane) = pane_by_id.get(pane_id) else {
                continue;
            };
            let base_revision = base_revisions.get(index).copied().unwrap_or_default();
            let current_revision = pane
                .terminal_grid
                .lock()
                .map_err(|_| SessionRuntimeError::Closed)?
                .grid()
                .revision();
            let (encoded, revision, desynced) = if base_revision == current_revision {
                (Vec::new(), current_revision, false)
            } else {
                let log = pane
                    .terminal_grid_deltas
                    .lock()
                    .map_err(|_| SessionRuntimeError::Closed)?;
                let Some(start) = log
                    .iter()
                    .position(|delta| delta.base_revision == base_revision)
                else {
                    deltas.push(bmux_pane_runtime_state::AttachPaneGridDelta {
                        pane_id: *pane_id,
                        base_revision,
                        revision: current_revision,
                        desynced: true,
                        encoded: Vec::new(),
                    });
                    continue;
                };
                let selected = select_terminal_grid_deltas(
                    &log,
                    start,
                    max_batches_per_pane,
                    RESPONSE_OUTPUT_BUDGET,
                );
                if selected.is_empty() {
                    deltas.push(bmux_pane_runtime_state::AttachPaneGridDelta {
                        pane_id: *pane_id,
                        base_revision,
                        revision: current_revision,
                        desynced: true,
                        encoded: Vec::new(),
                    });
                    continue;
                }
                let revision = selected
                    .last()
                    .map_or(base_revision, |delta| delta.revision);
                let desynced = revision != current_revision;
                let encoded =
                    serde_json::to_vec(&selected).map_err(|_| SessionRuntimeError::Closed)?;
                (encoded, revision, desynced)
            };
            deltas.push(bmux_pane_runtime_state::AttachPaneGridDelta {
                pane_id: *pane_id,
                base_revision,
                revision,
                desynced,
                encoded,
            });
        }
        Ok(bmux_pane_runtime_state::AttachGridDeltaState { deltas })
    }

    fn pane_state(&self, session_id: SessionId, pane_id: Uuid) -> Option<PaneState> {
        self.with_lock_read(|m| {
            m.runtimes
                .get(&session_id)
                .and_then(|r| r.panes.get(&pane_id))
                .map(pane_state_for_handle)
        })
        .flatten()
    }

    fn pane_state_reason(&self, session_id: SessionId, pane_id: Uuid) -> Option<String> {
        self.with_lock_read(|m| {
            m.runtimes
                .get(&session_id)
                .and_then(|r| r.panes.get(&pane_id))
                .and_then(pane_state_reason_for_handle)
        })
        .flatten()
    }

    fn clear_output_dirty(&self, session_id: SessionId, pane_id: Uuid) {
        if let Some(pane) = self
            .with_lock_read_named("clear_output_dirty.resolve", |m| {
                m.runtimes
                    .get(&session_id)
                    .and_then(|r| r.panes.get(&pane_id))
                    .map(|pane| pane_attach_data_ref(session_id, pane_id, pane))
            })
            .flatten()
        {
            pane.output_dirty.store(false, Ordering::SeqCst);
        }
    }

    fn clear_image_dirty(&self, session_id: SessionId, pane_id: Uuid) {
        #[cfg(feature = "image-registry")]
        if let Some(pane) = self
            .with_lock_read_named("clear_image_dirty.resolve", |m| {
                m.runtimes
                    .get(&session_id)
                    .and_then(|r| r.panes.get(&pane_id))
                    .map(|pane| pane_attach_data_ref(session_id, pane_id, pane))
            })
            .flatten()
        {
            pane.image_dirty.store(false, Ordering::SeqCst);
        }
        #[cfg(not(feature = "image-registry"))]
        {
            let _ = (session_id, pane_id);
        }
    }

    fn client_is_attached(&self, session_id: SessionId, client_id: ClientId) -> bool {
        self.with_lock_read(|m| {
            m.runtimes
                .get(&session_id)
                .is_some_and(|r| r.attached_clients.contains(&client_id))
        })
        .unwrap_or(false)
    }

    fn pane_output_has_pending(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
        _client_id: ClientId,
    ) -> bool {
        self.with_lock_read_named("pane_output_has_pending.resolve", |m| {
            m.runtimes
                .get(&session_id)
                .and_then(|r| r.panes.get(&pane_id))
                .map(|pane| Arc::clone(&pane.output_dirty))
        })
        .flatten()
        .is_some_and(|dirty| dirty.load(Ordering::SeqCst))
    }

    fn session_has_stored_viewport(&self, session_id: SessionId) -> bool {
        self.with_lock_read(|m| {
            m.runtimes
                .get(&session_id)
                .is_some_and(|r| r.attach_viewport.is_some())
        })
        .unwrap_or(false)
    }

    fn snapshot_session_runtime(
        &self,
        session_id: SessionId,
    ) -> Option<bmux_pane_runtime_state::SessionRuntimeSnapshot> {
        self.with_lock_read(|m| {
            let runtime = m.runtimes.get(&session_id)?;
            let panes = runtime
                .panes
                .values()
                .map(|p| p.meta.clone())
                .collect::<Vec<_>>();
            Some(bmux_pane_runtime_state::SessionRuntimeSnapshot {
                session_id,
                panes,
                focused_pane_id: runtime.focused_pane_id,
                layout_root: runtime.layout_root.clone(),
                floating_surfaces: runtime.floating_surfaces.clone(),
                attached_clients: runtime.attached_clients.clone(),
                attach_viewport: runtime.attach_viewport,
            })
        })
        .flatten()
    }

    fn snapshot_session_runtime_for_persistence(
        &self,
        session_id: SessionId,
    ) -> anyhow::Result<Option<bmux_pane_runtime_state::SessionRuntimeSnapshot>> {
        let Some(parts) = self
            .with_lock_read_named("snapshot_session_runtime_for_persistence.resolve", |m| {
                m.persistence_snapshot_parts(session_id)
            })
            .unwrap_or_else(|| Err(anyhow::anyhow!("session runtime manager lock poisoned")))?
        else {
            return Ok(None);
        };

        let mut panes = Vec::with_capacity(parts.panes.len());
        for pane in parts.panes {
            let process_id = pane.process_id.lock().ok().and_then(|v| *v);
            let process_group_id = pane.process_group_id.lock().ok().and_then(|v| *v);
            let mut resurrection_runtime = pane
                .resurrection_state
                .lock()
                .ok()
                .map(|s| s.clone())
                .unwrap_or_default();

            if !pane.exited.load(Ordering::SeqCst)
                && resurrection_runtime.active_command_source != Some(PaneCommandSource::Verbatim)
            {
                match inspect_process_group_command_and_cwd(
                    process_group_id,
                    process_id,
                    &pane.meta.shell,
                ) {
                    Some(inspection) => {
                        if let Some(command) = inspection.command {
                            resurrection_runtime.active_command = Some(command);
                            resurrection_runtime.active_command_source =
                                Some(PaneCommandSource::Inspection);
                        } else if resurrection_runtime.active_command_source
                            == Some(PaneCommandSource::Inspection)
                        {
                            resurrection_runtime.active_command = None;
                            resurrection_runtime.active_command_source = None;
                        }
                        if let Some(cwd) = inspection.cwd {
                            resurrection_runtime.last_known_cwd = Some(cwd);
                        }
                    }
                    None if resurrection_runtime.active_command_source
                        == Some(PaneCommandSource::Inspection) =>
                    {
                        resurrection_runtime.active_command = None;
                        resurrection_runtime.active_command_source = None;
                    }
                    None => {}
                }
            }

            if let Ok(mut state_guard) = pane.resurrection_state.lock() {
                *state_guard = resurrection_runtime.clone();
            }

            let mut meta = pane.meta;
            meta.resurrection = resurrection_runtime.to_snapshot();
            panes.push(meta);
        }

        Ok(Some(bmux_pane_runtime_state::SessionRuntimeSnapshot {
            session_id,
            panes,
            focused_pane_id: parts.focused_pane_id,
            layout_root: parts.layout_root,
            floating_surfaces: parts.floating_surfaces,
            attached_clients: parts.attached_clients,
            attach_viewport: parts.attach_viewport,
        }))
    }

    fn list_session_ids(&self) -> Vec<SessionId> {
        self.with_lock_read(|m| m.runtimes.keys().copied().collect())
            .unwrap_or_default()
    }

    fn shutdown_removed_runtime(&self, info: bmux_pane_runtime_state::RemovedRuntimeInfo) {
        let Some(handle) = Self::take_shutdown_handle(&info) else {
            return;
        };
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            tokio::task::block_in_place(|| {
                rt.block_on(shutdown_runtime_handle(RemovedRuntime {
                    session_id: info.session_id,
                    handle,
                }));
            });
        } else {
            drop(handle);
        }
    }

    fn read_pane_output_for_push(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
        client_id: ClientId,
        budget: usize,
    ) -> Option<(bmux_pane_runtime_state::OutputRead, bool)> {
        let pane = self
            .with_lock_read_named("read_pane_output_for_push.resolve", |m| {
                m.attach_pane_data_ref_for_push(session_id, pane_id, client_id)
            })
            .flatten()?;
        pane.output_dirty.store(false, Ordering::SeqCst);
        let inner_read = pane
            .output_buffer
            .lock()
            .ok()?
            .read_for_client(client_id, budget);
        let sync_update_active = pane.sync_update_in_progress.load(Ordering::SeqCst);
        Some((
            bmux_pane_runtime_state::OutputRead {
                bytes: inner_read.bytes,
                stream_start: inner_read.stream_start,
                stream_end: inner_read.stream_end,
                stream_gap: inner_read.stream_gap,
            },
            sync_update_active,
        ))
    }

    fn lag_recovery_bump_attach_view_for_client(
        &self,
        client_id: ClientId,
    ) -> Vec<(SessionId, u64)> {
        self.with_lock(|m| {
            m.runtimes
                .iter_mut()
                .filter_map(|(session_id, runtime)| {
                    if !runtime.attached_clients.contains(&client_id) {
                        return None;
                    }
                    runtime.attach_view_revision = runtime.attach_view_revision.saturating_add(1);
                    Some((*session_id, runtime.attach_view_revision))
                })
                .collect()
        })
        .unwrap_or_default()
    }

    fn bump_attach_view_revision(&self, session_id: SessionId) -> Option<u64> {
        self.with_lock(|m| m.bump_attach_view_revision(session_id))
            .flatten()
    }

    fn shell_integration_root(&self) -> Option<std::path::PathBuf> {
        self.with_lock_read(|m| m.shell_integration_root.clone())
            .flatten()
    }

    fn test_mark_pane_exited(&self, session_id: SessionId, pane_id: Uuid, reason: String) -> bool {
        self.with_lock(|m| {
            let Some(runtime) = m.runtimes.get_mut(&session_id) else {
                return false;
            };
            let Some(pane) = runtime.panes.get_mut(&pane_id) else {
                return false;
            };
            pane.exited.store(true, Ordering::SeqCst);
            if let Ok(mut slot) = pane.exit_reason.lock() {
                *slot = Some(reason);
            }
            true
        })
        .unwrap_or(false)
    }
}

#[derive(Debug, Clone, Copy)]
struct PaneExitEvent {
    session_id: SessionId,
    pane_id: Uuid,
}

fn reap_exited_pane(
    manager: &Arc<Mutex<SessionRuntimeManager>>,
    session_id: SessionId,
    pane_id: Uuid,
) {
    let (state_reason, update_sessions) = manager.lock().map_or_else(
        |_| (None, vec![session_id]),
        |mut manager| {
            let state_reason = manager
                .runtimes
                .get(&session_id)
                .and_then(|runtime| runtime.panes.get(&pane_id))
                .and_then(pane_state_reason_for_handle);
            let publish_all = manager.reconcile_exited_pane_focus(session_id, pane_id);
            let update_sessions = if publish_all {
                manager.active_session_ids()
            } else {
                vec![session_id]
            };
            (state_reason, update_sessions)
        },
    );
    publish_pane_event(PaneEvent::Exited {
        session_id: session_id.0,
        pane_id,
        reason: state_reason,
    });
    for session_id in update_sessions {
        emit_attach_view_changed_for_layout(session_id);
    }
    crate::handlers::publish_focus_state_snapshot();
    mark_snapshot_dirty_flag();
}

async fn process_pane_exit_events(
    manager: Arc<Mutex<SessionRuntimeManager>>,
    mut pane_exit_rx: mpsc::UnboundedReceiver<PaneExitEvent>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            changed = shutdown_rx.changed() => {
                if changed.is_ok() && *shutdown_rx.borrow() {
                    break;
                }
                if changed.is_err() {
                    break;
                }
            }
            maybe_event = pane_exit_rx.recv() => {
                let Some(event) = maybe_event else {
                    break;
                };
                reap_exited_pane(&manager, event.session_id, event.pane_id);
            }
        }
    }
}

#[derive(Debug, Clone)]
struct ProcessInspectionResult {
    command: Option<String>,
    cwd: Option<String>,
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct PsProcessEntry {
    pid: u32,
    pgid: i32,
    state: String,
    command: String,
}

#[cfg(unix)]
fn parse_ps_process_entry(line: &str) -> Option<PsProcessEntry> {
    let mut parts = line.split_whitespace();
    let pid = parts.next()?.parse::<u32>().ok()?;
    let process_group = parts.next()?.parse::<i32>().ok()?;
    let state = parts.next()?.to_string();
    let command = parts.collect::<Vec<_>>().join(" ");

    Some(PsProcessEntry {
        pid,
        pgid: process_group,
        state,
        command,
    })
}

#[cfg(unix)]
fn inspect_process_group_entries_with_resolver<F>(
    entries: &[PsProcessEntry],
    process_group_id: i32,
    shell_pid: Option<u32>,
    shell_path: &str,
    mut resolve_cwd: F,
) -> Option<ProcessInspectionResult>
where
    F: FnMut(u32) -> Option<String>,
{
    if process_group_id <= 0 {
        return None;
    }

    let mut selected_command: Option<(u32, String)> = None;
    let mut shell_candidate_pid: Option<u32> = None;

    for entry in entries {
        if entry.pgid != process_group_id || entry.command.is_empty() || entry.state.contains('Z') {
            continue;
        }

        if shell_pid == Some(entry.pid)
            || process_command_looks_like_shell(&entry.command, shell_path)
        {
            if shell_candidate_pid.is_none_or(|best_pid| entry.pid > best_pid) {
                shell_candidate_pid = Some(entry.pid);
            }
            continue;
        }

        if selected_command
            .as_ref()
            .is_none_or(|(best_pid, _)| entry.pid > *best_pid)
        {
            selected_command = Some((entry.pid, entry.command.clone()));
        }
    }

    let cwd = selected_command
        .as_ref()
        .and_then(|(pid, _)| resolve_cwd(*pid))
        .or_else(|| shell_pid.and_then(&mut resolve_cwd))
        .or_else(|| shell_candidate_pid.and_then(&mut resolve_cwd));
    let command = selected_command.map(|(_, command)| command);

    if command.is_none() && cwd.is_none() {
        return None;
    }

    Some(ProcessInspectionResult { command, cwd })
}

#[cfg(unix)]
fn inspect_process_group_command_and_cwd(
    process_group_id: Option<i32>,
    shell_pid: Option<u32>,
    shell_path: &str,
) -> Option<ProcessInspectionResult> {
    let process_group_id = process_group_id?;
    let output = std::process::Command::new("ps")
        .arg("-A")
        .arg("-o")
        .arg("pid=,pgid=,state=,command=")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let entries = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(parse_ps_process_entry)
        .collect::<Vec<_>>();

    inspect_process_group_entries_with_resolver(
        &entries,
        process_group_id,
        shell_pid,
        shell_path,
        resolve_process_working_directory,
    )
}

#[cfg(not(unix))]
fn inspect_process_group_command_and_cwd(
    _process_group_id: Option<i32>,
    _shell_pid: Option<u32>,
    _shell_path: &str,
) -> Option<ProcessInspectionResult> {
    None
}

fn process_command_looks_like_shell(command: &str, shell_path: &str) -> bool {
    let Some(first) = command.split_whitespace().next() else {
        return false;
    };
    let name = std::path::Path::new(first)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(first)
        .trim_start_matches('-')
        .to_ascii_lowercase();
    let shell_name = std::path::Path::new(shell_path)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(shell_path)
        .trim_start_matches('-')
        .to_ascii_lowercase();
    name == shell_name
        || matches!(
            name.as_str(),
            "sh" | "bash"
                | "zsh"
                | "fish"
                | "nu"
                | "nushell"
                | "dash"
                | "ksh"
                | "mksh"
                | "csh"
                | "tcsh"
        )
}

#[cfg(unix)]
fn resolve_process_working_directory(pid: u32) -> Option<String> {
    let proc_cwd = std::path::PathBuf::from(format!("/proc/{pid}/cwd"));
    if let Ok(path) = std::fs::read_link(&proc_cwd)
        && !path.as_os_str().is_empty()
    {
        return Some(path.to_string_lossy().to_string());
    }

    let lsof_output = std::process::Command::new("lsof")
        .arg("-a")
        .arg("-p")
        .arg(pid.to_string())
        .arg("-d")
        .arg("cwd")
        .arg("-Fn")
        .output();
    if let Ok(output) = lsof_output
        && output.status.success()
    {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if let Some(path) = line.strip_prefix('n')
                && !path.trim().is_empty()
            {
                return Some(path.to_string());
            }
        }
    }

    let ps_output = std::process::Command::new("ps")
        .arg("-o")
        .arg("cwd=")
        .arg("-p")
        .arg(pid.to_string())
        .output()
        .ok()?;
    if !ps_output.status.success() {
        return None;
    }
    let cwd = String::from_utf8_lossy(&ps_output.stdout)
        .trim()
        .to_string();
    (!cwd.is_empty()).then_some(cwd)
}

#[cfg(not(unix))]
fn resolve_process_working_directory(_pid: u32) -> Option<String> {
    None
}

#[cfg(unix)]
fn resolve_process_group_id_for_pid(pid: u32) -> Option<i32> {
    let output = std::process::Command::new("ps")
        .arg("-o")
        .arg("pgid=")
        .arg("-p")
        .arg(pid.to_string())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let parsed = value.parse::<i32>().ok()?;
    (parsed > 0).then_some(parsed)
}

/// On Windows there are no POSIX process groups. Return the PID itself so
/// that `terminate_process_group` can use it as the `taskkill /T` target for
/// process-tree termination.
#[cfg(windows)]
fn resolve_process_group_id_for_pid(pid: u32) -> Option<i32> {
    i32::try_from(pid).ok().filter(|&id| id > 0)
}

#[cfg(not(any(unix, windows)))]
fn resolve_process_group_id_for_pid(_pid: u32) -> Option<i32> {
    None
}

pub fn activate_pane_runtime(config: PaneRuntimePluginConfig) {
    let (pane_exit_tx, pane_exit_rx) = mpsc::unbounded_channel();
    let manager = Arc::new(Mutex::new(SessionRuntimeManager::new(
        config.shell,
        config.pane_term.clone(),
        protocol_profile_for_term(&config.pane_term),
        config.shell_integration_root,
        pane_exit_tx,
    )));
    let runtime_handle = bmux_pane_runtime_state::SessionRuntimeManagerHandle::new(
        ServerSessionRuntimeAdapter::new(Arc::clone(&manager)),
    );
    bmux_plugin::global_plugin_state_registry()
        .register::<bmux_pane_runtime_state::SessionRuntimeManagerHandle>(&Arc::new(
            std::sync::RwLock::new(runtime_handle),
        ));

    let shutdown_rx = watch::channel(false).1;
    let exit_manager = Arc::clone(&manager);
    tokio::spawn(async move {
        process_pane_exit_events(exit_manager, pane_exit_rx, shutdown_rx).await;
    });

    crate::snapshot::PaneRuntimeStateful::register();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(id: Uuid) -> PaneLayoutNode {
        PaneLayoutNode::Leaf { pane_id: id }
    }

    #[test]
    fn close_focus_fallback_uses_nearest_layout_neighbor() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        let root = PaneLayoutNode::Split {
            direction: PaneSplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(leaf(a)),
            second: Box::new(PaneLayoutNode::Split {
                direction: PaneSplitDirection::Horizontal,
                ratio: 0.5,
                first: Box::new(leaf(b)),
                second: Box::new(leaf(c)),
            }),
        };
        let viewport = Some(AttachViewport {
            cols: 100,
            rows: 40,
            status_top_inset: 0,
            status_bottom_inset: 0,
        });

        assert_eq!(next_focus_after_close(&root, b, b, viewport), Some(c));
        assert_eq!(next_focus_after_close(&root, c, c, viewport), Some(b));
    }

    #[test]
    fn close_focus_keeps_existing_focus_when_closing_unfocused_pane() {
        let focused = Uuid::new_v4();
        let closed = Uuid::new_v4();
        let root = PaneLayoutNode::Split {
            direction: PaneSplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(leaf(focused)),
            second: Box::new(leaf(closed)),
        };

        assert_eq!(
            next_focus_after_close(&root, focused, closed, None),
            Some(focused)
        );
    }

    fn dummy_pane(id: Uuid) -> PaneRuntimeHandle {
        let (input_tx, _input_rx) = mpsc::unbounded_channel();
        let (stop_tx, stop_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = stop_rx.await;
        });
        PaneRuntimeHandle {
            meta: PaneRuntimeMeta {
                id,
                name: None,
                shell: "sh".to_string(),
                launch: None,
                resurrection: PaneResurrectionSnapshot::default(),
            },
            process_id: Arc::new(std::sync::Mutex::new(None)),
            process_group_id: Arc::new(std::sync::Mutex::new(None)),
            resurrection_state: Arc::new(std::sync::Mutex::new(PaneResurrectionRuntime::default())),
            exit_reason: Arc::new(std::sync::Mutex::new(None)),
            stop_tx: Some(stop_tx),
            task,
            input_tx,
            output_buffer: Arc::new(std::sync::Mutex::new(OutputFanoutBuffer::new(
                MAX_WINDOW_OUTPUT_BUFFER_BYTES,
            ))),
            terminal_grid: Arc::new(std::sync::Mutex::new(
                TerminalGridStream::new(1, 1, GridLimits::default())
                    .expect("dummy pane terminal grid dimensions are valid"),
            )),
            terminal_grid_deltas: Arc::new(std::sync::Mutex::new(TerminalGridDeltaLog::default())),
            exited: Arc::new(AtomicBool::new(false)),
            last_requested_size: Arc::new(std::sync::Mutex::new((1, 1))),
            output_dirty: Arc::new(AtomicBool::new(false)),
            sync_update_in_progress: Arc::new(AtomicBool::new(false)),
            mouse_protocol_state: Arc::new(std::sync::Mutex::new(
                AttachMouseProtocolState::default(),
            )),
            input_mode_state: Arc::new(std::sync::Mutex::new(AttachInputModeState::default())),
            #[cfg(feature = "image-registry")]
            image_registry: Arc::new(std::sync::Mutex::new(bmux_image::ImageRegistry::default())),
            #[cfg(feature = "image-registry")]
            cell_pixel_size: Arc::new(std::sync::Mutex::new((1, 1))),
            #[cfg(feature = "image-registry")]
            image_dirty: Arc::new(AtomicBool::new(false)),
        }
    }

    fn runtime_with_panes(pane_ids: &[Uuid]) -> SessionRuntimeHandle {
        let panes = pane_ids
            .iter()
            .copied()
            .map(|pane_id| (pane_id, dummy_pane(pane_id)))
            .collect::<BTreeMap<_, _>>();
        SessionRuntimeHandle {
            panes,
            layout_root: pane_ids
                .first()
                .copied()
                .map(|pane_id| PaneLayoutNode::Leaf { pane_id }),
            focused_pane_id: pane_ids[0],
            zoomed_pane_id: None,
            floating_surfaces: Vec::new(),
            attached_clients: BTreeSet::new(),
            attach_viewport: Some(AttachViewport {
                cols: 120,
                rows: 40,
                status_top_inset: 0,
                status_bottom_inset: 0,
            }),
            attach_view_revision: 0,
        }
    }

    fn manager_with_runtime(
        session_id: SessionId,
        runtime: SessionRuntimeHandle,
    ) -> SessionRuntimeManager {
        let (pane_exit_tx, _pane_exit_rx) = mpsc::unbounded_channel();
        SessionRuntimeManager {
            runtimes: BTreeMap::from([(session_id, runtime)]),
            pane_input_index: Arc::new(RwLock::new(BTreeMap::new())),
            client_write_permissions: Arc::new(RwLock::new(BTreeMap::new())),
            shell: "sh".to_string(),
            pane_term: "xterm-256color".to_string(),
            protocol_profile: ProtocolProfile::Bmux,
            shell_integration_root: None,
            pane_exit_tx,
        }
    }

    fn set_pane_grid(pane: &PaneRuntimeHandle, width: u16, height: u16) {
        *pane
            .terminal_grid
            .lock()
            .expect("grid lock should be available") =
            TerminalGridStream::new(width, height, GridLimits::default())
                .expect("test grid dimensions are valid");
    }

    fn adapter_for_manager(manager: SessionRuntimeManager) -> ServerSessionRuntimeAdapter {
        ServerSessionRuntimeAdapter::new(Arc::new(Mutex::new(manager)))
    }

    fn row_text(row: &bmux_terminal_grid::PhysicalRow) -> String {
        row.cells()
            .iter()
            .filter(|cell| !cell.is_wide_continuation())
            .map(bmux_terminal_grid::Cell::text)
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    fn floating_surface(pane_id: Uuid, scope: FloatingPaneScope) -> FloatingSurfaceRuntime {
        FloatingSurfaceRuntime {
            id: Uuid::new_v4(),
            pane_id,
            anchor_pane_id: None,
            context_id: None,
            client_id: None,
            rect: LayoutRect {
                x: 1,
                y: 1,
                w: 10,
                h: 5,
            },
            scope,
            layer: FloatingPaneLayer::FloatingPane,
            z: 0,
            visible: true,
            opaque: true,
            accepts_input: true,
            cursor_owner: false,
        }
    }

    fn test_delta(base_revision: u64, revision: u64, text_len: usize) -> GridDeltaBatch {
        GridDeltaBatch {
            base_revision,
            revision,
            content_revision: revision,
            width: 10,
            height: 2,
            mode: "main".to_string(),
            scrollback_rows: 0,
            cursor: bmux_terminal_grid::CursorSnapshot::default(),
            saved_cursor: bmux_terminal_grid::CursorSnapshot::default(),
            saved_pending_wrap: false,
            current_style: bmux_terminal_grid::Style::default(),
            autowrap: true,
            pending_wrap: false,
            scroll_region: None,
            protocol: bmux_terminal_grid::ProtocolState::default(),
            pending_bytes: Vec::new(),
            styles: Vec::new(),
            reset_rows: false,
            row_updates: vec![bmux_terminal_grid::RowUpdateSnapshot {
                row_index: 0,
                row: bmux_terminal_grid::RowSnapshot {
                    wrapped: false,
                    runs: vec![bmux_terminal_grid::CellRunSnapshot {
                        start_col: 0,
                        text: "x".repeat(text_len),
                        style: bmux_terminal_grid::StyleId::DEFAULT,
                    }],
                },
            }],
        }
    }

    #[test]
    fn terminal_grid_delta_selection_respects_response_budget() {
        let mut log = TerminalGridDeltaLog::default();
        log.push(test_delta(0, 1, 8));
        log.push(test_delta(1, 2, 64));
        log.push(test_delta(2, 3, 8));
        let first_delta_size = estimate_terminal_grid_delta_bytes(
            log.batches.front().expect("log should contain first delta"),
        );

        let selected = select_terminal_grid_deltas(&log, 0, 3, first_delta_size + 1);

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].revision, 1);
    }

    #[tokio::test]
    async fn attach_grid_snapshot_state_reports_retained_scrollback_without_encoding_it() {
        let session_id = SessionId(Uuid::new_v4());
        let client_id = ClientId(Uuid::new_v4());
        let pane_id = Uuid::new_v4();
        let mut runtime = runtime_with_panes(&[pane_id]);
        runtime.attached_clients.insert(client_id);
        let pane = runtime.panes.get(&pane_id).expect("pane should exist");
        set_pane_grid(pane, 5, 2);
        pane.terminal_grid
            .lock()
            .expect("grid lock should be available")
            .process(b"line1\r\nline2\r\nline3");
        let adapter = adapter_for_manager(manager_with_runtime(session_id, runtime));

        let state = bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_grid_snapshot_state(
            &adapter,
            session_id,
            client_id,
            &[pane_id],
            usize::MAX,
        )
        .expect("attached client should receive grid snapshot");

        assert_eq!(state.snapshots.len(), 1);
        let snapshot =
            serde_json::from_slice::<bmux_terminal_grid::GridSnapshot>(&state.snapshots[0].encoded)
                .expect("snapshot payload should decode");
        let retained = snapshot
            .rows
            .iter()
            .map(|row| {
                row.runs
                    .iter()
                    .map(|run| run.text.as_str())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>();

        assert!(!retained.iter().any(|row| row == "line1"));
        assert!(retained.iter().any(|row| row == "line3"));
        assert_eq!(retained.len(), usize::from(snapshot.height));
        assert!(snapshot.scrollback_rows > 0);
    }

    #[tokio::test]
    async fn attach_grid_window_state_returns_bounded_scrollback_window() {
        let session_id = SessionId(Uuid::new_v4());
        let client_id = ClientId(Uuid::new_v4());
        let pane_id = Uuid::new_v4();
        let mut runtime = runtime_with_panes(&[pane_id]);
        runtime.attached_clients.insert(client_id);
        let pane = runtime.panes.get(&pane_id).expect("pane should exist");
        set_pane_grid(pane, 5, 2);
        pane.terminal_grid
            .lock()
            .expect("grid lock should be available")
            .process(b"line1\r\nline2\r\nline3");
        let adapter = adapter_for_manager(manager_with_runtime(session_id, runtime));

        let state = bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_grid_window_state(
            &adapter,
            session_id,
            client_id,
            &[bmux_pane_runtime_state::AttachPaneGridWindowRequest {
                pane_id,
                scrollback_offset: 1,
                rows: 2,
                anchor_total_scrolled_rows: None,
            }],
        )
        .expect("attached client should receive grid window");

        assert_eq!(state.windows.len(), 1);
        assert_eq!(state.windows[0].scrollback_offset, 1);
        assert!(state.windows[0].max_scrollback_offset >= 1);
        let snapshot =
            serde_json::from_slice::<bmux_terminal_grid::GridSnapshot>(&state.windows[0].encoded)
                .expect("window payload should decode");
        assert_eq!(snapshot.rows.len(), 2);
        let retained = snapshot
            .rows
            .iter()
            .map(|row| {
                row.runs
                    .iter()
                    .map(|run| run.text.as_str())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>();
        assert!(retained.iter().any(|row| row == "line1"));
        assert!(retained.iter().any(|row| row == "line2"));
        assert!(!retained.iter().any(|row| row == "line3"));
    }

    #[tokio::test]
    async fn attach_grid_window_state_anchors_offset_when_output_appends() {
        let session_id = SessionId(Uuid::new_v4());
        let client_id = ClientId(Uuid::new_v4());
        let pane_id = Uuid::new_v4();
        let mut runtime = runtime_with_panes(&[pane_id]);
        runtime.attached_clients.insert(client_id);
        let pane = runtime.panes.get(&pane_id).expect("pane should exist");
        set_pane_grid(pane, 20, 2);
        pane.terminal_grid
            .lock()
            .expect("grid lock should be available")
            .process(b"line1\r\nline2\r\nline3");
        let adapter = adapter_for_manager(manager_with_runtime(session_id, runtime));

        let first = bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_grid_window_state(
            &adapter,
            session_id,
            client_id,
            &[bmux_pane_runtime_state::AttachPaneGridWindowRequest {
                pane_id,
                scrollback_offset: 1,
                rows: 2,
                anchor_total_scrolled_rows: None,
            }],
        )
        .expect("attached client should receive first window")
        .windows
        .into_iter()
        .next()
        .expect("first window should exist");

        {
            let manager = adapter
                .inner
                .lock()
                .expect("manager lock should be available");
            let pane = manager
                .runtimes
                .get(&session_id)
                .and_then(|runtime| runtime.panes.get(&pane_id))
                .expect("pane should still exist");
            pane.terminal_grid
                .lock()
                .expect("grid lock should be available")
                .process(b"\r\nline4");
        }

        let second = bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_grid_window_state(
            &adapter,
            session_id,
            client_id,
            &[bmux_pane_runtime_state::AttachPaneGridWindowRequest {
                pane_id,
                scrollback_offset: first.scrollback_offset,
                rows: 2,
                anchor_total_scrolled_rows: Some(first.total_scrolled_rows),
            }],
        )
        .expect("attached client should receive anchored window")
        .windows
        .into_iter()
        .next()
        .expect("anchored window should exist");

        assert!(second.total_scrolled_rows > first.total_scrolled_rows);
        assert_eq!(
            second.scrollback_offset,
            first.scrollback_offset
                + usize::try_from(second.total_scrolled_rows - first.total_scrolled_rows)
                    .expect("test delta fits usize")
        );
        assert_eq!(
            second.anchor_delta_rows,
            second.scrollback_offset - first.scrollback_offset
        );
        assert!(!second.anchor_clamped);
    }

    #[tokio::test]
    async fn attach_grid_delta_state_returns_updates_for_attached_client() {
        let session_id = SessionId(Uuid::new_v4());
        let client_id = ClientId(Uuid::new_v4());
        let pane_id = Uuid::new_v4();
        let mut runtime = runtime_with_panes(&[pane_id]);
        runtime.attached_clients.insert(client_id);
        let pane = runtime.panes.get(&pane_id).expect("pane should exist");
        set_pane_grid(pane, 10, 2);
        let delta = pane
            .terminal_grid
            .lock()
            .expect("grid lock should be available")
            .process_delta(b"hello")
            .expect("output should change grid state");
        push_terminal_grid_delta(&pane.terminal_grid_deltas, delta);
        let adapter = adapter_for_manager(manager_with_runtime(session_id, runtime));

        let state = bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_grid_delta_state(
            &adapter,
            session_id,
            client_id,
            &[pane_id],
            &[0],
            16,
        )
        .expect("attached client should receive grid deltas");

        assert_eq!(state.deltas.len(), 1);
        let pane_delta = &state.deltas[0];
        assert_eq!(pane_delta.pane_id, pane_id);
        assert_eq!(pane_delta.base_revision, 0);
        assert!(!pane_delta.desynced);
        let batches = serde_json::from_slice::<Vec<GridDeltaBatch>>(&pane_delta.encoded)
            .expect("delta payload should decode");
        assert_eq!(batches.len(), 1);
        let mut consumer = TerminalGridStream::new(10, 2, GridLimits::default())
            .expect("consumer grid dimensions are valid");
        consumer
            .apply_delta(&batches[0], GridLimits::default())
            .expect("delta should apply to matching revision");
        assert_eq!(row_text(&consumer.grid().viewport_rows()[0]), "hello");
    }

    #[tokio::test]
    async fn resize_pty_reports_delta_desync_without_reflow_payload() {
        let session_id = SessionId(Uuid::new_v4());
        let client_id = ClientId(Uuid::new_v4());
        let pane_id = Uuid::new_v4();
        let mut runtime = runtime_with_panes(&[pane_id]);
        runtime.attached_clients.insert(client_id);
        let pane = runtime.panes.get(&pane_id).expect("pane should exist");
        set_pane_grid(pane, 10, 2);
        let base_revision = {
            let mut grid = pane
                .terminal_grid
                .lock()
                .expect("grid lock should be available");
            grid.process(b"abcdefghij");
            grid.grid().revision()
        };

        pane.resize_pty(2, 5);
        let adapter = adapter_for_manager(manager_with_runtime(session_id, runtime));

        let state = bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_grid_delta_state(
            &adapter,
            session_id,
            client_id,
            &[pane_id],
            &[base_revision],
            16,
        )
        .expect("delta state should be available");

        assert!(state.deltas[0].desynced);
        assert!(state.deltas[0].encoded.is_empty());
    }

    #[tokio::test]
    async fn attach_grid_delta_state_reports_auth_and_lookup_failures() {
        let session_id = SessionId(Uuid::new_v4());
        let client_id = ClientId(Uuid::new_v4());
        let pane_id = Uuid::new_v4();
        let runtime = runtime_with_panes(&[pane_id]);
        let adapter = adapter_for_manager(manager_with_runtime(session_id, runtime));

        let not_attached =
            bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_grid_delta_state(
                &adapter,
                session_id,
                client_id,
                &[pane_id],
                &[0],
                16,
            )
            .expect_err("unattached client should be rejected");
        assert!(matches!(not_attached, SessionRuntimeError::NotAttached));

        let mut runtime = runtime_with_panes(&[pane_id]);
        runtime.attached_clients.insert(client_id);
        let adapter = adapter_for_manager(manager_with_runtime(session_id, runtime));
        let missing_pane = Uuid::new_v4();
        let state = bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_grid_delta_state(
            &adapter,
            session_id,
            client_id,
            &[missing_pane],
            &[0],
            16,
        )
        .expect("missing pane is omitted from delta results");
        assert!(state.deltas.is_empty());

        let (pane_exit_tx, _pane_exit_rx) = mpsc::unbounded_channel();
        let empty_manager = SessionRuntimeManager {
            runtimes: BTreeMap::new(),
            pane_input_index: Arc::new(RwLock::new(BTreeMap::new())),
            client_write_permissions: Arc::new(RwLock::new(BTreeMap::new())),
            shell: "sh".to_string(),
            pane_term: "xterm-256color".to_string(),
            protocol_profile: ProtocolProfile::Bmux,
            shell_integration_root: None,
            pane_exit_tx,
        };
        let adapter = adapter_for_manager(empty_manager);
        let missing_session =
            bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_grid_delta_state(
                &adapter,
                session_id,
                client_id,
                &[pane_id],
                &[0],
                16,
            )
            .expect_err("missing session should be reported");
        assert!(matches!(missing_session, SessionRuntimeError::NotFound));
    }

    #[tokio::test]
    async fn attach_scene_projects_client_and_server_global_floating_panes() {
        let attach_session_id = SessionId(Uuid::new_v4());
        let other_session_id = SessionId(Uuid::new_v4());
        let client_id = ClientId(Uuid::new_v4());
        let attach_pane_id = Uuid::new_v4();
        let client_global_pane_id = Uuid::new_v4();
        let server_global_pane_id = Uuid::new_v4();
        let other_client_pane_id = Uuid::new_v4();

        let mut attach_runtime = runtime_with_panes(&[attach_pane_id]);
        attach_runtime.attached_clients.insert(client_id);
        let mut other_runtime = runtime_with_panes(&[
            client_global_pane_id,
            server_global_pane_id,
            other_client_pane_id,
        ]);
        let mut client_global =
            floating_surface(client_global_pane_id, FloatingPaneScope::ClientGlobal);
        client_global.client_id = Some(client_id);
        let server_global =
            floating_surface(server_global_pane_id, FloatingPaneScope::ServerGlobal);
        let mut other_client =
            floating_surface(other_client_pane_id, FloatingPaneScope::ClientGlobal);
        other_client.client_id = Some(ClientId(Uuid::new_v4()));
        other_runtime.floating_surfaces = vec![client_global, server_global, other_client];

        let (pane_exit_tx, _pane_exit_rx) = mpsc::unbounded_channel();
        let manager = SessionRuntimeManager {
            runtimes: BTreeMap::from([
                (attach_session_id, attach_runtime),
                (other_session_id, other_runtime),
            ]),
            pane_input_index: Arc::new(RwLock::new(BTreeMap::new())),
            client_write_permissions: Arc::new(RwLock::new(BTreeMap::new())),
            shell: "sh".to_string(),
            pane_term: "xterm-256color".to_string(),
            protocol_profile: ProtocolProfile::Bmux,
            shell_integration_root: None,
            pane_exit_tx,
        };

        let scene = manager
            .build_attach_scene_for_client(attach_session_id, client_id)
            .expect("scene should build");
        let visible_panes = scene
            .surfaces
            .iter()
            .filter(|surface| surface.visible)
            .filter_map(|surface| surface.pane_id)
            .collect::<BTreeSet<_>>();
        assert!(visible_panes.contains(&attach_pane_id));
        assert!(visible_panes.contains(&client_global_pane_id));
        assert!(visible_panes.contains(&server_global_pane_id));
        assert!(!visible_panes.contains(&other_client_pane_id));
    }

    #[tokio::test]
    async fn per_window_and_per_pane_scopes_stay_in_owner_session() {
        let attach_session_id = SessionId(Uuid::new_v4());
        let other_session_id = SessionId(Uuid::new_v4());
        let client_id = ClientId(Uuid::new_v4());
        let attach_pane_id = Uuid::new_v4();
        let other_pane_id = Uuid::new_v4();

        let mut attach_runtime = runtime_with_panes(&[attach_pane_id]);
        attach_runtime.attached_clients.insert(client_id);
        let mut other_runtime = runtime_with_panes(&[other_pane_id]);
        let mut per_window = floating_surface(other_pane_id, FloatingPaneScope::PerWindow);
        per_window.context_id = Some(Uuid::new_v4());
        let mut per_pane = floating_surface(other_pane_id, FloatingPaneScope::PerPane);
        per_pane.anchor_pane_id = Some(other_pane_id);
        other_runtime.floating_surfaces = vec![per_window, per_pane];

        let (pane_exit_tx, _pane_exit_rx) = mpsc::unbounded_channel();
        let manager = SessionRuntimeManager {
            runtimes: BTreeMap::from([
                (attach_session_id, attach_runtime),
                (other_session_id, other_runtime),
            ]),
            pane_input_index: Arc::new(RwLock::new(BTreeMap::new())),
            client_write_permissions: Arc::new(RwLock::new(BTreeMap::new())),
            shell: "sh".to_string(),
            pane_term: "xterm-256color".to_string(),
            protocol_profile: ProtocolProfile::Bmux,
            shell_integration_root: None,
            pane_exit_tx,
        };

        let scene = manager
            .build_attach_scene_for_client(attach_session_id, client_id)
            .expect("scene should build");
        assert!(
            scene
                .surfaces
                .iter()
                .all(|surface| surface.pane_id != Some(other_pane_id))
        );
    }

    #[tokio::test]
    async fn closing_last_tiled_pane_leaves_unanchored_floating_pane_visible() {
        let session_id = SessionId(Uuid::new_v4());
        let tiled_pane_id = Uuid::new_v4();
        let floating_pane_id = Uuid::new_v4();
        let mut runtime = runtime_with_panes(&[tiled_pane_id, floating_pane_id]);
        runtime.layout_root = Some(PaneLayoutNode::Leaf {
            pane_id: tiled_pane_id,
        });
        runtime.floating_surfaces = vec![floating_surface(
            floating_pane_id,
            FloatingPaneScope::PerSession,
        )];
        let (pane_exit_tx, _pane_exit_rx) = mpsc::unbounded_channel();
        let mut manager = SessionRuntimeManager {
            runtimes: BTreeMap::from([(session_id, runtime)]),
            pane_input_index: Arc::new(RwLock::new(BTreeMap::new())),
            client_write_permissions: Arc::new(RwLock::new(BTreeMap::new())),
            shell: "sh".to_string(),
            pane_term: "xterm-256color".to_string(),
            protocol_profile: ProtocolProfile::Bmux,
            shell_integration_root: None,
            pane_exit_tx,
        };

        let (_closed_pane_id, removed) = manager
            .close_pane(session_id, Some(PaneSelector::ById(tiled_pane_id)))
            .expect("last tiled pane should close");

        assert!(removed.is_none());
        let runtime = manager.runtimes.get(&session_id).expect("runtime remains");
        assert!(runtime.layout_root.is_none());
        assert!(runtime.panes.contains_key(&floating_pane_id));
        assert_eq!(runtime.focused_pane_id, floating_pane_id);
        assert_eq!(runtime.floating_surfaces.len(), 1);
    }

    #[tokio::test]
    async fn exiting_focused_floating_pane_restores_live_focus() {
        let session_id = SessionId(Uuid::new_v4());
        let tiled_pane_id = Uuid::new_v4();
        let floating_pane_id = Uuid::new_v4();
        let mut runtime = runtime_with_panes(&[tiled_pane_id, floating_pane_id]);
        runtime.layout_root = Some(PaneLayoutNode::Leaf {
            pane_id: tiled_pane_id,
        });
        runtime.focused_pane_id = floating_pane_id;
        let mut surface = floating_surface(floating_pane_id, FloatingPaneScope::PerSession);
        surface.cursor_owner = true;
        runtime.floating_surfaces = vec![surface];
        runtime
            .panes
            .get(&floating_pane_id)
            .expect("floating pane exists")
            .exited
            .store(true, Ordering::SeqCst);
        let (pane_exit_tx, _pane_exit_rx) = mpsc::unbounded_channel();
        let mut manager = SessionRuntimeManager {
            runtimes: BTreeMap::from([(session_id, runtime)]),
            pane_input_index: Arc::new(RwLock::new(BTreeMap::new())),
            client_write_permissions: Arc::new(RwLock::new(BTreeMap::new())),
            shell: "sh".to_string(),
            pane_term: "xterm-256color".to_string(),
            protocol_profile: ProtocolProfile::Bmux,
            shell_integration_root: None,
            pane_exit_tx,
        };

        let publish_all = manager.reconcile_exited_pane_focus(session_id, floating_pane_id);

        assert!(!publish_all);
        let runtime = manager.runtimes.get(&session_id).expect("runtime remains");
        assert_eq!(runtime.focused_pane_id, tiled_pane_id);
        assert!(
            runtime
                .floating_surfaces
                .iter()
                .all(|surface| !surface.cursor_owner)
        );
    }

    #[tokio::test]
    async fn closing_final_floating_pane_requires_explicit_session_action() {
        let session_id = SessionId(Uuid::new_v4());
        let floating_pane_id = Uuid::new_v4();
        let mut runtime = runtime_with_panes(&[floating_pane_id]);
        runtime.layout_root = None;
        runtime.floating_surfaces = vec![floating_surface(
            floating_pane_id,
            FloatingPaneScope::PerSession,
        )];
        let (pane_exit_tx, _pane_exit_rx) = mpsc::unbounded_channel();
        let mut manager = SessionRuntimeManager {
            runtimes: BTreeMap::from([(session_id, runtime)]),
            pane_input_index: Arc::new(RwLock::new(BTreeMap::new())),
            client_write_permissions: Arc::new(RwLock::new(BTreeMap::new())),
            shell: "sh".to_string(),
            pane_term: "xterm-256color".to_string(),
            protocol_profile: ProtocolProfile::Bmux,
            shell_integration_root: None,
            pane_exit_tx,
        };

        let result = manager.close_floating_pane(session_id, floating_pane_id);

        assert!(result.is_err());
        assert!(manager.runtimes.contains_key(&session_id));
    }

    #[test]
    fn pane_mouse_protocol_tracker_tracks_dec_private_modes() {
        let mut tracker = PaneTerminalModeTracker::default();

        assert_eq!(
            tracker.current_protocol().mode,
            AttachMouseProtocolMode::None
        );
        assert_eq!(
            tracker.current_protocol().encoding,
            AttachMouseProtocolEncoding::Default
        );

        tracker.process(b"\x1b[?1000h\x1b[?1006h");
        assert_eq!(
            tracker.current_protocol(),
            AttachMouseProtocolState {
                mode: AttachMouseProtocolMode::PressRelease,
                encoding: AttachMouseProtocolEncoding::Sgr,
            }
        );

        tracker.process(b"\x1b[?1003h");
        assert_eq!(
            tracker.current_protocol().mode,
            AttachMouseProtocolMode::AnyMotion
        );

        tracker.process(b"\x1b[?1003l");
        assert_eq!(
            tracker.current_protocol().mode,
            AttachMouseProtocolMode::PressRelease
        );

        tracker.process(b"\x1b[?1000l\x1b[?1006l");
        assert_eq!(
            tracker.current_protocol(),
            AttachMouseProtocolState {
                mode: AttachMouseProtocolMode::None,
                encoding: AttachMouseProtocolEncoding::Default,
            }
        );
    }

    #[test]
    fn pane_terminal_mode_tracker_tracks_input_modes() {
        let mut tracker = PaneTerminalModeTracker::default();

        assert_eq!(
            tracker.current_input_modes(),
            AttachInputModeState::default()
        );

        tracker.process(b"\x1b[?1h\x1b=");
        assert_eq!(
            tracker.current_input_modes(),
            AttachInputModeState {
                application_cursor: true,
                application_keypad: true,
            }
        );

        tracker.process(b"\x1b[?1l\x1b>");
        assert_eq!(
            tracker.current_input_modes(),
            AttachInputModeState::default()
        );

        tracker.process(b"\x1b[?1h\x1b=");
        tracker.process(b"\x1bc");
        assert_eq!(
            tracker.current_input_modes(),
            AttachInputModeState::default()
        );
    }

    #[test]
    fn protocol_reply_tracks_cursor_position_for_cpr_queries() {
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Xterm);
        let mut cursor_tracker = PaneCursorTracker::new(24, 80);

        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[12;34H");

        let cpr_reply = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[6n");
        assert_eq!(cpr_reply, b"\x1b[12;34R");

        let dec_cpr_reply = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[?6n");
        assert_eq!(dec_cpr_reply, b"\x1b[?12;34R");
    }

    #[test]
    fn protocol_reply_reports_saved_cursor_after_alt_screen_exit() {
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Xterm);
        let mut cursor_tracker = PaneCursorTracker::new(24, 80);

        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[12;34H");
        assert_eq!(
            protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[6n"),
            b"\x1b[12;34R"
        );

        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[?1049h");
        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[4;7H");
        assert_eq!(
            protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[6n"),
            b"\x1b[4;7R"
        );

        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[?1049l");
        assert_eq!(
            protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[6n"),
            b"\x1b[12;34R"
        );
    }

    #[test]
    fn protocol_reply_does_not_confuse_kitty_query_u_with_restore_cursor() {
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Xterm);
        let mut cursor_tracker = PaneCursorTracker::new(30, 120);

        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[9;17H");
        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[s");
        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[?u");
        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[H");
        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[u");

        assert_eq!(
            protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[6n"),
            b"\x1b[9;17R"
        );
    }

    #[test]
    fn cursor_tracker_resize_updates_cursor_bounds() {
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Xterm);
        let mut cursor_tracker = PaneCursorTracker::new(5, 5);

        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[24;80H");
        let clamped_reply = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[6n");
        assert_eq!(clamped_reply, b"\x1b[5;5R");

        cursor_tracker.resize(40, 120);
        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[24;80H");
        let resized_reply = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[6n");
        assert_eq!(resized_reply, b"\x1b[24;80R");
    }

    #[test]
    fn esc_seq_phase_csi_sgr_round_trip() {
        let seq = b"\x1b[38;2;10;10;10m";
        let mut phase = EscSeqPhase::Ground;

        phase = phase.advance(seq[0]);
        assert_eq!(phase, EscSeqPhase::Escape);

        phase = phase.advance(seq[1]);
        assert_eq!(phase, EscSeqPhase::Csi);

        for &byte in &seq[2..seq.len() - 1] {
            phase = phase.advance(byte);
            assert_eq!(phase, EscSeqPhase::Csi);
        }

        phase = phase.advance(seq[seq.len() - 1]);
        assert_eq!(phase, EscSeqPhase::Ground);
    }

    #[test]
    fn esc_seq_phase_can_aborts_from_any_state() {
        for initial in [
            EscSeqPhase::Escape,
            EscSeqPhase::Csi,
            EscSeqPhase::Osc,
            EscSeqPhase::Dcs,
            EscSeqPhase::Sos,
        ] {
            assert_eq!(initial.advance(0x18), EscSeqPhase::Ground);
            assert_eq!(initial.advance(0x1A), EscSeqPhase::Ground);
        }
    }

    #[cfg(unix)]
    #[test]
    fn parse_ps_process_entry_handles_command_with_spaces() {
        let entry = parse_ps_process_entry(" 101  99 Ss opencode --model gpt-5")
            .expect("ps line should parse");
        assert_eq!(entry.pid, 101);
        assert_eq!(entry.pgid, 99);
        assert_eq!(entry.state, "Ss");
        assert_eq!(entry.command, "opencode --model gpt-5");
    }

    #[cfg(unix)]
    #[test]
    fn inspection_fallback_prefers_non_shell_process_for_command() {
        let entries = vec![
            PsProcessEntry {
                pid: 120,
                pgid: 88,
                state: "Ss".to_string(),
                command: "nu".to_string(),
            },
            PsProcessEntry {
                pid: 121,
                pgid: 88,
                state: "R+".to_string(),
                command: "opencode run".to_string(),
            },
        ];

        let inspection =
            inspect_process_group_entries_with_resolver(&entries, 88, Some(120), "nu", |pid| {
                match pid {
                    120 => Some("/home/user".to_string()),
                    121 => Some("/work/project".to_string()),
                    _ => None,
                }
            })
            .expect("inspection should include active command");

        assert_eq!(inspection.command.as_deref(), Some("opencode run"));
        assert_eq!(inspection.cwd.as_deref(), Some("/work/project"));
    }

    #[test]
    fn shell_metadata_parser_strips_bell_marker() {
        let mut parser = PaneShellMetadataParser::default();
        let output =
            parser.process_chunk(b"\x1b]633;bmux;start;ZWNobyBoaQ==;L3RtcA==\x07plain-output");

        assert_eq!(output.filtered, b"plain-output");
        assert_eq!(
            output.events,
            vec![PaneShellMetadataEvent::CommandStart {
                command: "echo hi".to_string(),
                cwd: "/tmp".to_string(),
            }]
        );
    }

    #[test]
    fn resurrection_runtime_prompt_clears_command_and_preserves_cwd() {
        let mut state = PaneResurrectionRuntime::default();
        state.apply_event(PaneShellMetadataEvent::CommandStart {
            command: "sleep 30".to_string(),
            cwd: "/work".to_string(),
        });
        state.apply_event(PaneShellMetadataEvent::Prompt {
            cwd: "/work".to_string(),
        });

        assert_eq!(state.active_command, None);
        assert_eq!(state.active_command_source, None);
        assert_eq!(state.last_known_cwd.as_deref(), Some("/work"));
    }

    #[test]
    fn prompt_replay_waits_for_shell_prompt_and_marks_command_active() {
        let mut state = PaneResurrectionRuntime {
            active_command: Some("lazygit".to_string()),
            active_command_source: Some(PaneCommandSource::Verbatim),
            last_known_cwd: Some("/work".to_string()),
        };
        let mut pending_replay = Some("lazygit".to_string());

        let replay = apply_shell_metadata_events_and_take_prompt_replay(
            &mut state,
            &mut pending_replay,
            [PaneShellMetadataEvent::Prompt {
                cwd: "/work".to_string(),
            }],
        );

        assert_eq!(replay.as_deref(), Some("lazygit"));
        assert_eq!(pending_replay, None);
        assert_eq!(state.active_command.as_deref(), Some("lazygit"));
        assert_eq!(
            state.active_command_source,
            Some(PaneCommandSource::Verbatim)
        );
        assert_eq!(state.last_known_cwd.as_deref(), Some("/work"));
    }

    #[test]
    fn prompt_replay_is_one_shot_then_later_prompt_clears_command() {
        let mut state = PaneResurrectionRuntime::default();
        let mut pending_replay = Some("lazygit".to_string());

        let replay = apply_shell_metadata_events_and_take_prompt_replay(
            &mut state,
            &mut pending_replay,
            [PaneShellMetadataEvent::Prompt {
                cwd: "/work".to_string(),
            }],
        );
        assert_eq!(replay.as_deref(), Some("lazygit"));

        let replay = apply_shell_metadata_events_and_take_prompt_replay(
            &mut state,
            &mut pending_replay,
            [PaneShellMetadataEvent::Prompt {
                cwd: "/work".to_string(),
            }],
        );

        assert_eq!(replay, None);
        assert_eq!(state.active_command, None);
        assert_eq!(state.active_command_source, None);
    }

    #[test]
    fn nu_shell_integration_wraps_prompt_command_for_prompt_marker() {
        let config = shell_integration_nu_config();

        assert!(config.contains("$env.PROMPT_COMMAND = {||"));
        assert!(config.contains("__bmux_original_prompt_command"));
        assert!(config.contains("__bmux_prompt_marker"));
        assert!(!config.contains("hooks.pre_prompt"));
    }

    #[test]
    fn read_recent_never_starts_mid_csi() {
        let mut buf = OutputFanoutBuffer::new(65_536);

        let mut chunk1 = vec![b' '; 1020];
        chunk1.extend_from_slice(b"\x1b[48;");
        assert_eq!(chunk1.len(), 1025);

        let mut chunk2 = Vec::new();
        chunk2.extend_from_slice(b"2;10;10;10m");
        chunk2.extend_from_slice(&vec![b' '; 1013]);
        assert_eq!(chunk2.len(), 1024);

        buf.push_chunk(&chunk1);
        buf.push_chunk(&chunk2);

        let result = buf.read_recent_with_offsets(chunk2.len()).bytes;

        assert!(
            !result.starts_with(b"2;10;10;10m"),
            "read_recent returned data starting mid-escape-sequence: {:?}",
            String::from_utf8_lossy(&result[..20.min(result.len())])
        );
        if !result.is_empty() {
            assert_eq!(result[0], b' ');
        }
    }

    #[test]
    fn read_recent_budget_smaller_than_buffer_skips_mid_seq_start() {
        let mut buf = OutputFanoutBuffer::new(4096);
        let payload = b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAaaaa\x1b[38;2;128;128;128mBBBBBBBBBBBBBBBBBB";
        buf.push_chunk(payload);

        let result = buf.read_recent_with_offsets(30).bytes;
        assert_eq!(result, b"BBBBBBBBBBBBBBBBBB");
    }

    #[test]
    fn read_for_client_gap_advances_to_ground_boundary() {
        let mut buf = OutputFanoutBuffer::new(64);
        let client = ClientId(Uuid::new_v4());
        buf.register_client_at_tail(client);

        buf.push_chunk(b"\x1b[31mhello");

        let mut big_chunk = Vec::new();
        big_chunk.extend_from_slice(b"\x1b[48;");
        big_chunk.extend_from_slice(b"2;10;10;10m");
        big_chunk.extend_from_slice(b"safe text here.");
        big_chunk.resize(80, b' ');
        buf.push_chunk(&big_chunk);

        let read = buf.read_for_client(client, 1024);
        assert!(
            !read.bytes.starts_with(b"2;10;10;10m"),
            "gap read returned mid-sequence data: {:?}",
            String::from_utf8_lossy(&read.bytes[..20.min(read.bytes.len())])
        );
        if !read.bytes.is_empty() {
            assert!(read.bytes[0] == b's' || read.bytes[0] == b' ');
        }
    }

    #[test]
    fn esc_spans_pruned_on_eviction() {
        let mut buf = OutputFanoutBuffer::new(32);

        buf.push_chunk(b"\x1b[mA\x1b[mB\x1b[mC");
        let pre_evict_count = buf.esc_spans.len();
        assert!(pre_evict_count >= 3);

        buf.push_chunk(&[b'X'; 40]);

        for &(_esc_start, safe_resume) in &buf.esc_spans {
            assert!(safe_resume == u64::MAX || safe_resume > buf.start_offset);
        }
    }
}
