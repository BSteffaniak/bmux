//! Concrete pane runtime implementation owned by the pane-runtime plugin.

use anyhow::{Context, Result};
use bmux_context_state::ContextStateHandle;
use bmux_ipc::{
    AttachFocusTarget, AttachInputModeState, AttachLayer, AttachMouseProtocolEncoding,
    AttachMouseProtocolMode, AttachMouseProtocolState, AttachPaneChunk, AttachPaneInputMode,
    AttachPaneMouseProtocol, AttachRect, AttachScene, AttachSurface, AttachSurfaceKind,
    AttachViewComponent, Event, PaneFocusDirection, PaneLaunchCommand,
    PaneLayoutNode as IpcPaneLayoutNode, PaneSelector, PaneSplitDirection, PaneState, PaneSummary,
    RecordingEventKind, RecordingPayload,
};
use bmux_pane_runtime_plugin_api::PaneRuntimePluginConfig;
use bmux_pane_runtime_state::{
    AttachViewport, FloatingSurfaceRuntime, LayoutRect, PaneCommandSource, PaneLaunchSpec,
    PaneLayoutNode, PaneResizeDirection, PaneResurrectionSnapshot, PaneRuntimeMeta,
    SessionRuntimeError,
};
use bmux_plugin_sdk::WireEventSinkHandle;
use bmux_recording_runtime::{RecordMeta, RecordingSinkHandle};
use bmux_session_models::{ClientId, SessionId};
use bmux_session_state::SessionManagerHandle;
use bmux_snapshot_runtime::{SnapshotDirtyFlag, SnapshotDirtyFlagHandle};
use bmux_terminal_protocol::{ProtocolProfile, TerminalProtocolEngine, protocol_profile_for_term};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tracing::{trace, warn};
use uuid::Uuid;

const MAX_WINDOW_OUTPUT_BUFFER_BYTES: usize = 1_048_576;
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

fn publish_wire_event(event: Event) {
    if let Some(handle) = bmux_plugin::global_plugin_state_registry()
        .get::<WireEventSinkHandle>()
        .and_then(|arc| arc.read().ok().map(|guard| guard.clone()))
        && let Err(error) = handle.0.publish(event)
    {
        warn!(%error, "failed publishing pane-runtime wire event");
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
    publish_wire_event(Event::AttachViewChanged {
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
        .is_ok()
    {
    } else {
        pane.task.abort();
        let _ = pane.task.await;
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
    shell: String,
    pane_term: String,
    protocol_profile: ProtocolProfile,
    shell_integration_root: Option<std::path::PathBuf>,
    pane_exit_tx: mpsc::UnboundedSender<PaneExitEvent>,
}

struct SessionRuntimeHandle {
    panes: BTreeMap<Uuid, PaneRuntimeHandle>,
    layout_root: PaneLayoutNode,
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
    parser: vt100::Parser,
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
            // Use scrollback of 1 so we can detect scroll events via
            // screen().scrollback() incrementing from 0 to 1.
            parser: vt100::Parser::new(rows, cols, 1),
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
        self.parser.screen_mut().set_size(rows, cols);
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
                        // vt100::Parser reliably restores cursor for ESC 7/8 but
                        // can miss CSI s/u (especially when apps emit save/probe/
                        // restore around alt-screen transitions). Normalize those
                        // short forms to ESC 7/8 before feeding the parser.
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
            self.parser.process(&normalized);
        }
    }

    fn cursor_position(&self) -> (u16, u16) {
        self.parser.screen().cursor_position()
    }

    /// Consume any scrollback that accumulated since the last call.
    /// Returns the number of lines that scrolled since last drain.
    #[cfg(feature = "image-registry")]
    fn drain_scroll_delta(&mut self) -> u16 {
        #[allow(clippy::cast_possible_truncation)]
        let scrollback = self.parser.screen().scrollback() as u16;
        if scrollback > 0 {
            self.total_scrollback += u64::from(scrollback);
            // Reset scrollback to 0 so we can detect the next scroll.
            self.parser.screen_mut().set_scrollback(0);
            scrollback
        } else {
            0
        }
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
                alternate_screen = cursor_tracker.parser.screen().alternate_screen(),
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
    r#"const __bmux_user_config = ($nu.default-config-dir | path join "config.nu")
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

def __bmux_emit_start [command: string, cwd: string] {
  let command_b64 = ($command | encode base64)
  let cwd_b64 = ($cwd | encode base64)
  ^printf '\033]633;bmux;start;%s;%s\a' $command_b64 $cwd_b64
}

def __bmux_emit_prompt [cwd: string] {
  let cwd_b64 = ($cwd | encode base64)
  ^printf '\033]633;bmux;prompt;%s\a' $cwd_b64
}

let __bmux_pre_execution_hooks = (__bmux_hook_list ($env.config | get -o hooks.pre_execution))
let __bmux_pre_prompt_hooks = (__bmux_hook_list ($env.config | get -o hooks.pre_prompt))

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
  | upsert hooks.pre_prompt (
      $__bmux_pre_prompt_hooks
      | append {||
          __bmux_emit_prompt ($env.PWD | into string)
        }
    )
)
"#
}

impl PaneRuntimeHandle {
    fn send_input(&self, data: Vec<u8>) -> std::result::Result<(), SessionRuntimeError> {
        self.input_tx
            .send(PaneRuntimeCommand::Input(data))
            .map_err(|_| SessionRuntimeError::Closed)
    }

    fn resize_pty(&self, rows: u16, cols: u16) {
        if let Ok(mut last) = self.last_requested_size.lock() {
            *last = (rows, cols);
        }
        let _ = self
            .input_tx
            .send(PaneRuntimeCommand::Resize { rows, cols });
    }
}

impl SessionRuntimeManager {
    fn bump_attach_view_revision(&mut self, session_id: SessionId) -> Option<u64> {
        let runtime = self.runtimes.get_mut(&session_id)?;
        runtime.attach_view_revision = runtime.attach_view_revision.saturating_add(1);
        Some(runtime.attach_view_revision)
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
        if self.data.is_empty() {
            return OutputRead {
                bytes: Vec::new(),
                stream_start: end,
                stream_end: end,
                stream_gap: false,
            };
        }
        let to_read = self.data.len().min(max_bytes.max(1));
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

struct AttachLayoutState {
    focused_pane_id: Uuid,
    panes: Vec<PaneSummary>,
    layout_root: IpcPaneLayoutNode,
    scene: AttachScene,
    zoomed: bool,
}

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

fn validate_runtime_layout_matches_panes(
    layout_root: &PaneLayoutNode,
    panes: &BTreeMap<Uuid, PaneRuntimeHandle>,
) -> Result<()> {
    let pane_ids = panes.keys().copied().collect::<BTreeSet<_>>();
    let mut layout_ids = BTreeSet::new();
    collect_runtime_layout_pane_ids(layout_root, &mut layout_ids)?;
    if pane_ids != layout_ids {
        anyhow::bail!(
            "runtime layout panes do not match runtime pane map (layout: {}, panes: {})",
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

// Building the attach scene requires constructing every surface's
// `rect` + `content_rect` + `interactive_regions` literally; splitting
// this further would hurt readability more than it helps.
#[allow(clippy::too_many_lines)]
fn build_attach_scene(
    session_id: SessionId,
    runtime: &SessionRuntimeHandle,
    viewport: Option<AttachViewport>,
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
                .filter(|surface| runtime.panes.contains_key(&surface.pane_id))
                .map(|surface| {
                    let rect = attach_rect_from_layout_rect(surface.rect);
                    AttachSurface {
                        id: surface.id,
                        kind: AttachSurfaceKind::FloatingPane,
                        layer: AttachLayer::FloatingPane,
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
                }),
        );

        return AttachScene {
            session_id: session_id.0,
            focus: AttachFocusTarget::Pane { pane_id: zoomed_id },
            surfaces,
        };
    }
    // Zoomed pane was removed; fall through to normal rendering.

    let mut rects = BTreeMap::new();
    collect_layout_rects(&runtime.layout_root, scene_root, &mut rects);

    let mut pane_ids = Vec::new();
    runtime.layout_root.pane_order(&mut pane_ids);

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
            .filter(|surface| runtime.panes.contains_key(&surface.pane_id))
            .map(|surface| {
                let rect = attach_rect_from_layout_rect(surface.rect);
                AttachSurface {
                    id: surface.id,
                    kind: AttachSurfaceKind::FloatingPane,
                    layer: AttachLayer::FloatingPane,
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
            }),
    );

    AttachScene {
        session_id: session_id.0,
        focus: AttachFocusTarget::Pane {
            pane_id: runtime.focused_pane_id,
        },
        surfaces,
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
    collect_layout_rects(&runtime.layout_root, root, &mut rects);
    for (pane_id, pane) in &runtime.panes {
        if pane.exited.load(Ordering::SeqCst) {
            continue;
        }
        if let Some(rect) = rects.get(pane_id).copied() {
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
    const fn new(
        shell: String,
        pane_term: String,
        protocol_profile: ProtocolProfile,
        shell_integration_root: Option<std::path::PathBuf>,
        pane_exit_tx: mpsc::UnboundedSender<PaneExitEvent>,
    ) -> Self {
        Self {
            runtimes: BTreeMap::new(),
            shell,
            pane_term,
            protocol_profile,
            shell_integration_root,
            pane_exit_tx,
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
        let first_pane = self.spawn_pane_runtime(session_id, pane_meta);
        let mut panes = BTreeMap::new();
        panes.insert(first_pane_id, first_pane);

        self.runtimes.insert(
            session_id,
            SessionRuntimeHandle {
                panes,
                layout_root: PaneLayoutNode::Leaf {
                    pane_id: first_pane_id,
                },
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

        let mut runtime_panes = BTreeMap::new();
        for pane_meta in panes {
            let pane = self.spawn_pane_runtime(session_id, pane_meta.clone());
            runtime_panes.insert(pane_meta.id, pane);
        }

        let runtime_layout_root = layout_root
            .unwrap_or_else(|| layout_from_panes(panes).expect("restored runtime has panes"));
        validate_runtime_layout_matches_panes(&runtime_layout_root, &runtime_panes)?;

        self.runtimes.insert(
            session_id,
            SessionRuntimeHandle {
                panes: runtime_panes,
                layout_root: runtime_layout_root,
                focused_pane_id,
                zoomed_pane_id: None,
                floating_surfaces,
                attached_clients: BTreeSet::new(),
                attach_viewport: None,
                attach_view_revision: 0,
            },
        );

        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn spawn_pane_runtime(
        &self,
        session_id: SessionId,
        pane_meta: PaneRuntimeMeta,
    ) -> PaneRuntimeHandle {
        let (stop_tx, mut stop_rx) = oneshot::channel();
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<PaneRuntimeCommand>();
        let output_buffer = Arc::new(std::sync::Mutex::new(OutputFanoutBuffer::new(
            MAX_WINDOW_OUTPUT_BUFFER_BYTES,
        )));
        let last_requested_size = Arc::new(std::sync::Mutex::new((24_u16, 80_u16)));
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
                rows: 24,
                cols: 80,
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

            let (command, failed_spawn_label) = launch.as_ref().map_or_else(
                || {
                    let mut command = CommandBuilder::new(&shell);
                    command.env("TERM", &pane_term);
                    if let Some(cwd) = initial_cwd.as_deref()
                        && !cwd.is_empty()
                    {
                        command.cwd(cwd);
                    }
                    if let Err(error) = configure_shell_integration_command(
                        &mut command,
                        &shell,
                        shell_integration_root.as_deref(),
                    ) {
                        warn!("failed configuring shell integration for pane {pane_id}: {error:#}");
                    }
                    (command, format!("shell '{shell}'"))
                },
                |launch| {
                    let mut command = CommandBuilder::new(&launch.program);
                    command.env("TERM", &pane_term);
                    for arg in &launch.args {
                        command.arg(arg);
                    }
                    for (key, value) in &launch.env {
                        command.env(key, value);
                    }
                    if let Some(cwd) = launch.cwd.as_deref().or(initial_cwd.as_deref())
                        && !cwd.is_empty()
                    {
                        command.cwd(cwd);
                    }
                    (command, format!("command '{}'", launch.program))
                },
            );
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
                && let Ok(mut writer_guard) = writer.lock()
            {
                let mut replay_bytes = command_text.as_bytes().to_vec();
                replay_bytes.push(b'\n');
                if writer_guard.write_all(&replay_bytes).is_ok() {
                    let _ = writer_guard.flush();
                }
            }

            let (child_exit_tx, mut child_exit_rx) = mpsc::unbounded_channel::<()>();
            let exited_for_waiter = Arc::clone(&exited_for_task);
            let exit_reason_for_waiter = Arc::clone(&exit_reason_for_task);
            let output_buffer_for_waiter = Arc::clone(&output_buffer_for_reader);
            let resurrection_state_for_waiter = Arc::clone(&resurrection_state_for_waiter_seed);
            let child_waiter = std::thread::Builder::new()
                .name(format!("bmux-server-pane-{pane_id}-wait"))
                .spawn(move || {
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
                })
                .ok();

            let reader_output = Arc::clone(&output_buffer_for_reader);
            let writer_for_reader = Arc::clone(&writer);
            let reader_thread = std::thread::Builder::new()
                .name(format!("bmux-server-pane-{pane_id}"))
                .spawn(move || {
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
                                // pushed to the output buffer for vt100 parsing.
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
                                            publish_wire_event(Event::PaneImageAvailable {
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
                                    if let Ok(mut resurrection_state) =
                                        resurrection_state_for_reader.lock()
                                    {
                                        for event in metadata.events {
                                            resurrection_state.apply_event(event);
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
                                        publish_wire_event(Event::PaneImageAvailable {
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
                                    publish_wire_event(Event::PaneOutputAvailable {
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
                                            publish_wire_event(Event::PaneImageAvailable {
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
                })
                .ok();

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

            if let Some(waiter) = child_waiter {
                let _ = waiter.join();
            }
            if let Some(thread) = reader_thread {
                let _ = thread.join();
            }
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
        let handle = self.spawn_pane_runtime(session_id, pane_meta);
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
        let replaced =
            session
                .layout_root
                .replace_leaf_with_split(target_pane_id, direction, 0.5, pane_id);
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
        let mut pane_ids = Vec::new();
        session.layout_root.pane_order(&mut pane_ids);
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
            (pane_id, session.panes.len() == 1)
        };

        if remove_runtime {
            let removed = self.remove_runtime(session_id)?;
            return Ok((pane_id, Some(removed)));
        }

        let session = self
            .runtimes
            .get_mut(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        let pane = session
            .panes
            .remove(&pane_id)
            .ok_or_else(|| anyhow::anyhow!("focused pane not found"))?;
        let _ = session.layout_root.remove_leaf(pane_id);
        let mut remaining = Vec::new();
        session.layout_root.pane_order(&mut remaining);
        if (session.focused_pane_id == pane_id
            || !session.panes.contains_key(&session.focused_pane_id))
            && let Some(next_id) = remaining.first().copied()
        {
            session.focused_pane_id = next_id;
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
        tokio::spawn(async move {
            shutdown_pane_handle(old_pane).await;
        });

        let new_pane = self.spawn_pane_runtime(session_id, pane_meta.clone());
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
        let _ = session
            .layout_root
            .resize_focused(pane_id, direction, root, cells.max(1));
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
            // Only zoom if there are at least 2 panes (zooming a single pane is a no-op).
            let mut pane_ids = Vec::new();
            session.layout_root.pane_order(&mut pane_ids);
            if pane_ids.len() < 2 {
                return Ok((focused, false));
            }
            session.zoomed_pane_id = Some(focused);
            self.apply_stored_attach_viewport(session_id);
            Ok((focused, true))
        }
    }

    #[allow(clippy::cast_possible_truncation)]
    fn list_panes(&self, session_id: SessionId) -> Result<Vec<PaneSummary>> {
        let session = self
            .runtimes
            .get(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        let mut pane_ids = Vec::new();
        session.layout_root.pane_order(&mut pane_ids);
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

    #[allow(clippy::cast_possible_truncation)]
    fn attach_layout_state(
        &self,
        session_id: SessionId,
        client_id: ClientId,
    ) -> Result<AttachLayoutState, SessionRuntimeError> {
        let session = self
            .runtimes
            .get(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?;
        if !session.attached_clients.contains(&client_id) {
            return Err(SessionRuntimeError::NotAttached);
        }
        let scene = build_attach_scene(session_id, session, session.attach_viewport);
        let mut pane_ids = Vec::new();
        session.layout_root.pane_order(&mut pane_ids);
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
        Ok(AttachLayoutState {
            focused_pane_id: session.focused_pane_id,
            panes,
            layout_root: ipc_layout_from_runtime(&session.layout_root),
            scene,
            zoomed: session.zoomed_pane_id.is_some(),
        })
    }

    #[allow(clippy::cast_possible_truncation)]
    fn attach_snapshot_state(
        &mut self,
        session_id: SessionId,
        client_id: ClientId,
        max_bytes_per_pane: usize,
    ) -> Result<AttachSnapshotState, SessionRuntimeError> {
        let session = self
            .runtimes
            .get_mut(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?;
        if !session.attached_clients.contains(&client_id) {
            return Err(SessionRuntimeError::NotAttached);
        }
        let scene = build_attach_scene(session_id, session, session.attach_viewport);
        let mut pane_ids = Vec::new();
        session.layout_root.pane_order(&mut pane_ids);
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

        let mut chunks = Vec::new();
        let mut pane_mouse_protocols = Vec::new();
        let mut pane_input_modes = Vec::new();
        let num_panes = pane_ids.len().max(1);
        let per_pane_budget = (RESPONSE_OUTPUT_BUDGET / num_panes).min(max_bytes_per_pane);
        let mut budget_remaining = RESPONSE_OUTPUT_BUDGET;
        for pane_id in pane_ids {
            let Some(pane) = session.panes.get_mut(&pane_id) else {
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

        Ok(AttachSnapshotState {
            focused_pane_id: session.focused_pane_id,
            panes,
            layout_root: ipc_layout_from_runtime(&session.layout_root),
            scene,
            chunks,
            pane_mouse_protocols,
            pane_input_modes,
            zoomed: session.zoomed_pane_id.is_some(),
        })
    }

    fn read_pane_output_batch(
        &mut self,
        session_id: SessionId,
        client_id: ClientId,
        pane_ids: &[Uuid],
        max_bytes: usize,
    ) -> Result<Vec<AttachPaneChunk>, SessionRuntimeError> {
        let chunks = {
            let session = self
                .runtimes
                .get_mut(&session_id)
                .ok_or(SessionRuntimeError::NotFound)?;
            if !session.attached_clients.contains(&client_id) {
                return Err(SessionRuntimeError::NotAttached);
            }

            let mut chunks = Vec::new();
            let num_panes = pane_ids.len().max(1);
            let per_pane_budget = (RESPONSE_OUTPUT_BUDGET / num_panes).min(max_bytes);
            let mut budget_remaining = RESPONSE_OUTPUT_BUDGET;
            for pane_id in pane_ids {
                let Some(pane) = session.panes.get_mut(pane_id) else {
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

    fn attach_pane_snapshot_state(
        &mut self,
        session_id: SessionId,
        client_id: ClientId,
        pane_ids: &[Uuid],
        max_bytes_per_pane: usize,
    ) -> Result<AttachPaneSnapshotState, SessionRuntimeError> {
        let session = self
            .runtimes
            .get_mut(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?;
        if !session.attached_clients.contains(&client_id) {
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

            let Some(pane) = session.panes.get_mut(pane_id) else {
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

        Ok(RemovedRuntime {
            session_id,
            handle: runtime,
        })
    }

    fn remove_all_runtimes(&mut self) -> Vec<RemovedRuntime> {
        std::mem::take(&mut self.runtimes)
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
        let runtime = self
            .runtimes
            .get_mut(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?;

        if !runtime.attached_clients.contains(&client_id) {
            return Err(SessionRuntimeError::NotAttached);
        }

        let focused_pane_id = runtime.focused_pane_id;
        let pane = runtime
            .panes
            .get_mut(&focused_pane_id)
            .ok_or(SessionRuntimeError::NotFound)?;

        if pane.exited.load(Ordering::SeqCst) {
            return Err(SessionRuntimeError::Closed);
        }

        let bytes = data.len();
        pane.send_input(data)?;
        Ok((bytes, focused_pane_id))
    }

    /// Write input bytes directly to a specific pane by ID, bypassing focus routing.
    fn write_input_to_pane(
        &mut self,
        session_id: SessionId,
        pane_id: Uuid,
        data: Vec<u8>,
    ) -> Result<usize, SessionRuntimeError> {
        let runtime = self
            .runtimes
            .get_mut(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?;

        let pane = runtime
            .panes
            .get_mut(&pane_id)
            .ok_or(SessionRuntimeError::NotFound)?;

        if pane.exited.load(Ordering::SeqCst) {
            return Err(SessionRuntimeError::Closed);
        }

        let bytes = data.len();
        pane.send_input(data)?;
        Ok(bytes)
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
            let mut pane_ids = Vec::new();
            runtime.layout_root.pane_order(&mut pane_ids);
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

struct ServerSessionRuntimeAdapter {
    inner: Arc<Mutex<SessionRuntimeManager>>,
}

impl ServerSessionRuntimeAdapter {
    const fn new(inner: Arc<Mutex<SessionRuntimeManager>>) -> Self {
        Self { inner }
    }

    fn with_lock<R>(&self, f: impl FnOnce(&mut SessionRuntimeManager) -> R) -> Option<R> {
        self.inner.lock().ok().map(|mut g| f(&mut g))
    }

    fn with_lock_read<R>(&self, f: impl FnOnce(&SessionRuntimeManager) -> R) -> Option<R> {
        self.inner.lock().ok().map(|g| f(&g))
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
    ) -> anyhow::Result<()> {
        self.with_lock(|m| {
            m.restore_runtime(
                session_id,
                panes,
                layout_root,
                focused_pane_id,
                floating_surfaces,
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
        self.with_lock(|m| m.write_input_to_pane(session_id, pane_id, data))
            .unwrap_or(Err(SessionRuntimeError::Closed))
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
        self.with_lock(|m| m.read_pane_output_batch(session_id, client_id, pane_ids, max_bytes))
            .unwrap_or(Err(SessionRuntimeError::Closed))
    }

    fn attach_pane_output_batch_with_dirty_check(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        pane_ids: &[Uuid],
        max_bytes: usize,
    ) -> (Result<Vec<AttachPaneChunk>, SessionRuntimeError>, bool) {
        self.with_lock(|m| {
            if let Some(runtime) = m.runtimes.get(&session_id) {
                for pane_id in pane_ids {
                    if let Some(pane) = runtime.panes.get(pane_id) {
                        pane.output_dirty.store(false, Ordering::SeqCst);
                    }
                }
            }
            let chunks = m.read_pane_output_batch(session_id, client_id, pane_ids, max_bytes);
            let still_pending = m.runtimes.get(&session_id).is_some_and(|rt| {
                pane_ids.iter().any(|pane_id| {
                    rt.panes
                        .get(pane_id)
                        .is_some_and(|p| p.output_dirty.load(Ordering::SeqCst))
                })
            });
            (chunks, still_pending)
        })
        .unwrap_or((Err(SessionRuntimeError::Closed), false))
    }

    fn attach_pane_image_deltas(
        &self,
        session_id: SessionId,
        pane_ids: &[Uuid],
        since_sequences: &[u64],
        payload_codec: Option<&dyn bmux_ipc::compression::CompressionCodec>,
    ) -> Vec<bmux_ipc::AttachPaneImageDelta> {
        self.with_lock_read(|m| {
            let mut result = Vec::new();
            let Some(runtime) = m.runtimes.get(&session_id) else {
                return result;
            };
            for pane_id in pane_ids {
                if let Some(pane) = runtime.panes.get(pane_id) {
                    #[cfg(feature = "image-registry")]
                    pane.image_dirty.store(false, Ordering::SeqCst);
                    #[cfg(not(feature = "image-registry"))]
                    let _ = pane;
                }
            }
            for (i, pane_id) in pane_ids.iter().enumerate() {
                let since = since_sequences.get(i).copied().unwrap_or(0);
                if let Some(pane) = runtime.panes.get(pane_id) {
                    #[cfg(feature = "image-registry")]
                    if let Ok(reg) = pane.image_registry.lock() {
                        let delta = reg.delta_since(since);
                        result.push(delta.to_ipc(*pane_id, payload_codec));
                    }
                    #[cfg(not(feature = "image-registry"))]
                    {
                        let _ = (pane, since, payload_codec);
                        result.push(bmux_ipc::AttachPaneImageDelta {
                            pane_id: *pane_id,
                            added: Vec::new(),
                            removed: Vec::new(),
                            sequence: 0,
                        });
                    }
                }
            }
            result
        })
        .unwrap_or_default()
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
        let inner = self
            .with_lock(|m| m.attach_layout_state(session_id, client_id))
            .unwrap_or(Err(SessionRuntimeError::Closed))?;
        Ok(bmux_pane_runtime_state::AttachLayoutState {
            focused_pane_id: inner.focused_pane_id,
            panes: inner.panes,
            layout_root: inner.layout_root,
            scene: inner.scene,
            zoomed: inner.zoomed,
        })
    }

    fn attach_snapshot_state(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        max_bytes_per_pane: usize,
    ) -> Result<bmux_pane_runtime_state::AttachSnapshotState, SessionRuntimeError> {
        let inner = self
            .with_lock(|m| m.attach_snapshot_state(session_id, client_id, max_bytes_per_pane))
            .unwrap_or(Err(SessionRuntimeError::Closed))?;
        Ok(bmux_pane_runtime_state::AttachSnapshotState {
            focused_pane_id: inner.focused_pane_id,
            panes: inner.panes,
            layout_root: inner.layout_root,
            scene: inner.scene,
            zoomed: inner.zoomed,
            chunks: inner.chunks,
            pane_mouse_protocols: inner.pane_mouse_protocols,
            pane_input_modes: inner.pane_input_modes,
        })
    }

    fn attach_pane_snapshot_state(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        pane_ids: &[Uuid],
        max_bytes_per_pane: usize,
    ) -> Result<bmux_pane_runtime_state::AttachPaneSnapshotState, SessionRuntimeError> {
        let inner = self
            .with_lock(|m| {
                m.attach_pane_snapshot_state(session_id, client_id, pane_ids, max_bytes_per_pane)
            })
            .unwrap_or(Err(SessionRuntimeError::Closed))?;
        Ok(bmux_pane_runtime_state::AttachPaneSnapshotState {
            chunks: inner.chunks,
            pane_mouse_protocols: inner.pane_mouse_protocols,
            pane_input_modes: inner.pane_input_modes,
        })
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
        let _ = self.with_lock_read(|m| {
            if let Some(r) = m.runtimes.get(&session_id)
                && let Some(p) = r.panes.get(&pane_id)
            {
                p.output_dirty.store(false, Ordering::SeqCst);
            }
        });
    }

    fn clear_image_dirty(&self, session_id: SessionId, pane_id: Uuid) {
        #[cfg(feature = "image-registry")]
        {
            let _ = self.with_lock_read(|m| {
                if let Some(r) = m.runtimes.get(&session_id)
                    && let Some(p) = r.panes.get(&pane_id)
                {
                    p.image_dirty.store(false, Ordering::SeqCst);
                }
            });
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
        self.with_lock_read(|m| {
            m.runtimes
                .get(&session_id)
                .and_then(|r| r.panes.get(&pane_id))
                .is_some_and(|p| p.output_dirty.load(Ordering::SeqCst))
        })
        .unwrap_or(false)
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

    #[allow(
        clippy::significant_drop_tightening,
        reason = "Snapshot persistence must validate and refresh all panes from one consistent runtime-manager view."
    )]
    fn snapshot_session_runtime_for_persistence(
        &self,
        session_id: SessionId,
    ) -> anyhow::Result<Option<bmux_pane_runtime_state::SessionRuntimeSnapshot>> {
        self.with_lock(|m| {
            let Some(runtime) = m.runtimes.get_mut(&session_id) else {
                return Ok(None);
            };

            validate_runtime_layout_matches_panes(&runtime.layout_root, &runtime.panes)
                .with_context(|| {
                    format!(
                        "cannot snapshot inconsistent layout for session {}",
                        session_id.0
                    )
                })?;

            let mut pane_ids = Vec::new();
            runtime.layout_root.pane_order(&mut pane_ids);
            let mut panes = Vec::with_capacity(pane_ids.len());
            for pane_id in pane_ids {
                let Some(pane) = runtime.panes.get_mut(&pane_id) else {
                    anyhow::bail!(
                        "layout references missing pane {pane_id} in session {}",
                        session_id.0
                    );
                };

                let process_id = pane.process_id.lock().ok().and_then(|v| *v);
                let process_group_id = pane.process_group_id.lock().ok().and_then(|v| *v);
                let mut resurrection_runtime = pane
                    .resurrection_state
                    .lock()
                    .ok()
                    .map(|s| s.clone())
                    .unwrap_or_default();

                if !pane.exited.load(Ordering::SeqCst)
                    && resurrection_runtime.active_command_source
                        != Some(PaneCommandSource::Verbatim)
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

                let mut meta = pane.meta.clone();
                meta.resurrection = resurrection_runtime.to_snapshot();
                panes.push(meta);
            }

            Ok(Some(bmux_pane_runtime_state::SessionRuntimeSnapshot {
                session_id,
                panes,
                focused_pane_id: runtime.focused_pane_id,
                layout_root: runtime.layout_root.clone(),
                floating_surfaces: runtime.floating_surfaces.clone(),
                attached_clients: runtime.attached_clients.clone(),
                attach_viewport: runtime.attach_viewport,
            }))
        })
        .unwrap_or_else(|| Err(anyhow::anyhow!("session runtime manager lock poisoned")))
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

    #[allow(
        clippy::significant_drop_tightening,
        reason = "Guard must live for the duration of the runtime lookup + pane read; tightening would split it into two locks."
    )]
    fn read_pane_output_for_push(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
        client_id: ClientId,
        budget: usize,
    ) -> Option<(bmux_pane_runtime_state::OutputRead, bool)> {
        let (inner_read, sync_update_active) = {
            let guard = self.inner.lock().ok()?;
            let runtime = guard.runtimes.get(&session_id)?;
            if !runtime.attached_clients.contains(&client_id) {
                return None;
            }
            let pane = runtime.panes.get(&pane_id)?;
            pane.output_dirty.store(false, Ordering::SeqCst);
            let mut buf = pane.output_buffer.lock().ok()?;
            let inner_read = buf.read_for_client(client_id, budget);
            let sync_update_active = pane.sync_update_in_progress.load(Ordering::SeqCst);
            drop(buf);
            (inner_read, sync_update_active)
        };
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

fn reap_exited_pane(session_id: SessionId, pane_id: Uuid) {
    let state_reason = session_runtime_handle()
        .0
        .pane_state_reason(session_id, pane_id);
    publish_wire_event(Event::PaneExited {
        session_id: session_id.0,
        pane_id,
        reason: state_reason,
    });
    emit_attach_view_changed_for_layout(session_id);
    mark_snapshot_dirty_flag();
}

async fn process_pane_exit_events(
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
                reap_exited_pane(event.session_id, event.pane_id);
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
    tokio::spawn(async move {
        process_pane_exit_events(pane_exit_rx, shutdown_rx).await;
    });

    crate::snapshot::PaneRuntimeStateful::register();
}

#[cfg(test)]
mod tests {
    use super::*;

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
