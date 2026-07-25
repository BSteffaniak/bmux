//! `SessionRuntimeManagerApi` trait + handle newtype.
//!
//! The pane-runtime plugin's `SessionRuntimeManager` implements this
//! trait. Server acquires the handle via the plugin state registry
//! and dispatches through the trait instead of holding a concrete
//! `Mutex<SessionRuntimeManager>` — this is how the "core must not
//! name plugin impl types" rule holds across the attach + pane
//! lifecycle.

#![allow(clippy::too_many_arguments)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::must_use_candidate)]

use crate::{
    AttachViewport, FloatingPaneLayer, FloatingPaneScope, FloatingSurfaceRuntime, LayoutRect,
    PaneLayoutNode, PaneResizeDirection, PaneRuntimeMeta, SessionRuntimeError,
};
use bmux_attach_layout_protocol::{
    AttachPaneChunk, AttachPaneInputMode, AttachPaneMouseProtocol, AttachScene, PaneFocusDirection,
    PaneLaunchCommand, PaneLayoutNode as IpcPaneLayoutNode, PaneSelector, PaneSplitDirection,
    PaneState, PaneSummary,
};
use bmux_session_models::{ClientId, SessionId};
use std::collections::BTreeSet;
use std::sync::Arc;
use uuid::Uuid;

/// Opaque projection of a removed session runtime — carries the info
/// server needs after `remove_runtime` without leaking the concrete
/// pane-runtime types.
#[derive(Debug, Clone, Default)]
pub struct RemovedRuntimeInfo {
    pub session_id: SessionId,
    pub attached_clients: BTreeSet<ClientId>,
    /// Opaque payload the plugin uses to actually shut down the
    /// removed runtime asynchronously. Kept behind an `Any` so server
    /// can round-trip it back through `shutdown_runtime_handle`
    /// without interpreting it.
    pub shutdown_token: Arc<std::sync::Mutex<Option<Box<dyn std::any::Any + Send + 'static>>>>,
}

/// Attach-layout DTO returned by `attach_layout_state`.
#[derive(Debug, Clone)]
pub struct AttachLayoutState {
    pub focused_pane_id: Uuid,
    pub panes: Vec<PaneSummary>,
    pub layout_root: IpcPaneLayoutNode,
    pub scene: AttachScene,
    pub zoomed: bool,
}

/// Attach-snapshot DTO returned by `attach_snapshot_state`.
#[derive(Debug, Clone)]
pub struct AttachSnapshotState {
    pub focused_pane_id: Uuid,
    pub panes: Vec<PaneSummary>,
    pub layout_root: IpcPaneLayoutNode,
    pub scene: AttachScene,
    pub zoomed: bool,
    pub chunks: Vec<AttachPaneChunk>,
    pub pane_mouse_protocols: Vec<AttachPaneMouseProtocol>,
    pub pane_input_modes: Vec<AttachPaneInputMode>,
}

/// Attach-pane-snapshot DTO.
#[derive(Debug, Clone)]
pub struct AttachPaneSnapshotState {
    pub chunks: Vec<AttachPaneChunk>,
    pub pane_mouse_protocols: Vec<AttachPaneMouseProtocol>,
    pub pane_input_modes: Vec<AttachPaneInputMode>,
}

/// Structured terminal-grid snapshot for one pane.
#[derive(Debug, Clone)]
pub struct AttachPaneGridSnapshot {
    pub pane_id: Uuid,
    pub stream_end: u64,
    pub encoded: Vec<u8>,
}

/// Structured terminal-grid snapshot DTO.
#[derive(Debug, Clone)]
pub struct AttachGridSnapshotState {
    pub snapshots: Vec<AttachPaneGridSnapshot>,
}

/// Bounded structured terminal-grid scrollback window request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachPaneGridWindowRequest {
    pub pane_id: Uuid,
    pub scrollback_offset: usize,
    pub rows: usize,
    pub anchor_total_scrolled_rows: Option<u64>,
}

/// Bounded structured terminal-grid scrollback window for one pane.
#[derive(Debug, Clone)]
pub struct AttachPaneGridWindow {
    pub pane_id: Uuid,
    pub scrollback_offset: usize,
    pub max_scrollback_offset: usize,
    pub total_scrolled_rows: u64,
    pub anchor_delta_rows: usize,
    pub anchor_clamped: bool,
    pub stream_end: u64,
    pub encoded: Vec<u8>,
}

/// Structured terminal-grid scrollback window DTO.
#[derive(Debug, Clone)]
pub struct AttachGridWindowState {
    pub windows: Vec<AttachPaneGridWindow>,
}

/// Structured terminal-grid delta batches for one pane.
#[derive(Debug, Clone)]
pub struct AttachPaneGridDelta {
    pub pane_id: Uuid,
    pub base_revision: u64,
    pub revision: u64,
    pub desynced: bool,
    pub encoded: Vec<u8>,
}

/// Structured terminal-grid delta DTO.
#[derive(Debug, Clone)]
pub struct AttachGridDeltaState {
    pub deltas: Vec<AttachPaneGridDelta>,
}

/// Process identity for a running pane runtime. `pid` is the spawned
/// shell/process id; `process_group_id` is the platform process-group
/// root when available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneProcessIdentity {
    pub session_id: SessionId,
    pub pane_id: Uuid,
    pub pid: Option<u32>,
    pub process_group_id: Option<i32>,
}

/// Public projection of a floating pane runtime.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FloatingPaneRuntimeSummary {
    pub id: Uuid,
    pub pane_id: Uuid,
    pub anchor_pane_id: Option<Uuid>,
    pub context_id: Option<Uuid>,
    pub client_id: Option<ClientId>,
    pub rect: LayoutRect,
    pub scope: FloatingPaneScope,
    pub layer: FloatingPaneLayer,
    pub z: i32,
    pub visible: bool,
    pub opaque: bool,
    pub accepts_input: bool,
    pub cursor_owner: bool,
}

/// Per-session pane-runtime projection used when building a
/// persistence snapshot.
#[derive(Debug, Clone)]
pub struct SessionRuntimeSnapshot {
    pub session_id: SessionId,
    pub panes: Vec<PaneRuntimeMeta>,
    pub focused_pane_id: Uuid,
    pub layout_root: Option<PaneLayoutNode>,
    pub floating_surfaces: Vec<FloatingSurfaceRuntime>,
    pub attached_clients: BTreeSet<ClientId>,
    pub attach_viewport: Option<AttachViewport>,
}

/// Trait implemented by the pane-runtime plugin's
/// `SessionRuntimeManager`. Server + other plugins consume pane
/// runtime exclusively through this trait object.
pub trait SessionRuntimeManagerApi: Send + Sync {
    // ── Session lifecycle ──────────────────────────────────────────

    fn start_runtime(&self, session_id: SessionId) -> anyhow::Result<()>;

    fn restore_runtime(
        &self,
        session_id: SessionId,
        panes: &[PaneRuntimeMeta],
        layout_root: Option<PaneLayoutNode>,
        focused_pane_id: Uuid,
        floating_surfaces: Vec<FloatingSurfaceRuntime>,
        attach_viewport: Option<AttachViewport>,
    ) -> anyhow::Result<()>;

    fn remove_runtime(&self, session_id: SessionId) -> Option<RemovedRuntimeInfo>;
    fn remove_all_runtimes(&self) -> Vec<RemovedRuntimeInfo>;
    fn session_exists(&self, session_id: SessionId) -> bool;
    fn active_session_ids(&self) -> Vec<SessionId>;

    // ── Pane lifecycle ─────────────────────────────────────────────

    fn split_pane(
        &self,
        session_id: SessionId,
        target: Option<PaneSelector>,
        direction: PaneSplitDirection,
    ) -> anyhow::Result<Uuid>;

    fn launch_pane(
        &self,
        session_id: SessionId,
        target: Option<PaneSelector>,
        direction: PaneSplitDirection,
        name: Option<String>,
        command: PaneLaunchCommand,
    ) -> anyhow::Result<Uuid>;

    fn focus_pane(
        &self,
        session_id: SessionId,
        direction: PaneFocusDirection,
    ) -> anyhow::Result<Uuid>;

    fn focus_pane_target(
        &self,
        session_id: SessionId,
        target: &PaneSelector,
    ) -> anyhow::Result<Uuid>;

    fn resize_pane(
        &self,
        session_id: SessionId,
        target: Option<PaneSelector>,
        direction: PaneResizeDirection,
        cells: u16,
    ) -> anyhow::Result<()>;

    /// Sets the exact PTY dimensions for one pane without mutating the logical
    /// split layout. Worker runtimes use this for generation-fenced remote
    /// resize requests.
    fn set_pane_pty_size(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
        rows: u16,
        cols: u16,
    ) -> anyhow::Result<()>;

    fn close_pane(
        &self,
        session_id: SessionId,
        target: Option<PaneSelector>,
    ) -> anyhow::Result<(Uuid, Option<RemovedRuntimeInfo>)>;

    fn restart_pane(
        &self,
        session_id: SessionId,
        target: Option<PaneSelector>,
    ) -> anyhow::Result<Uuid>;

    fn toggle_zoom(&self, session_id: SessionId) -> anyhow::Result<(Uuid, bool)>;

    // ── Floating pane lifecycle ────────────────────────────────────

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
    ) -> anyhow::Result<FloatingPaneRuntimeSummary>;

    fn list_floating_panes(
        &self,
        session_id: SessionId,
    ) -> anyhow::Result<Vec<FloatingPaneRuntimeSummary>>;

    fn move_floating_pane(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
        x: u16,
        y: u16,
    ) -> anyhow::Result<FloatingPaneRuntimeSummary>;

    fn resize_floating_pane(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
        w: u16,
        h: u16,
    ) -> anyhow::Result<FloatingPaneRuntimeSummary>;

    fn focus_floating_pane(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
    ) -> anyhow::Result<FloatingPaneRuntimeSummary>;

    fn raise_floating_pane(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
    ) -> anyhow::Result<FloatingPaneRuntimeSummary>;

    fn lower_floating_pane(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
    ) -> anyhow::Result<FloatingPaneRuntimeSummary>;

    fn set_floating_pane_z(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
        z: i32,
    ) -> anyhow::Result<FloatingPaneRuntimeSummary>;

    fn set_floating_pane_layer(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
        layer: FloatingPaneLayer,
    ) -> anyhow::Result<FloatingPaneRuntimeSummary>;

    fn close_floating_pane(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
    ) -> anyhow::Result<(Uuid, Option<RemovedRuntimeInfo>)>;

    // ── Pane I/O ───────────────────────────────────────────────────

    fn list_panes(&self, session_id: SessionId) -> anyhow::Result<Vec<PaneSummary>>;

    fn list_pane_processes(&self) -> Vec<PaneProcessIdentity>;
    fn pane_process_identity(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
    ) -> Option<PaneProcessIdentity>;

    fn write_input(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        data: Vec<u8>,
    ) -> Result<(usize, Uuid), SessionRuntimeError>;

    fn write_input_to_pane(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        pane_id: Uuid,
        data: Vec<u8>,
    ) -> Result<usize, SessionRuntimeError>;

    /// Reads an absolute output cursor range for a pane without registering a
    /// connection-scoped client cursor. Returns the retained range and whether
    /// the requested cursor was repaired to the retention boundary.
    fn read_pane_output_at(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
        cursor: u64,
        max_bytes: usize,
    ) -> Result<crate::OutputRead, SessionRuntimeError>;

    fn set_client_write_permission(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        allowed: bool,
    );

    fn client_can_write(&self, session_id: SessionId, client_id: ClientId) -> bool;

    fn read_output(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        max_bytes: usize,
    ) -> Result<Vec<u8>, SessionRuntimeError>;

    fn read_pane_output_batch(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        pane_ids: &[Uuid],
        max_bytes: usize,
    ) -> Result<Vec<AttachPaneChunk>, SessionRuntimeError>;

    /// Composite: clear the `output_dirty` flag on each requested pane,
    /// drain their per-client output, then re-check `output_dirty` to
    /// see if the PTY reader pushed additional data in between. Used by
    /// the `AttachPaneOutputBatch` IPC path so the client can know
    /// whether to keep draining before rendering.
    ///
    /// Returns `(chunks, output_still_pending)`.
    fn attach_pane_output_batch_with_dirty_check(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        pane_ids: &[Uuid],
        max_bytes: usize,
    ) -> (Result<Vec<AttachPaneChunk>, SessionRuntimeError>, bool);

    fn attach_grid_snapshot_state(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        pane_ids: &[Uuid],
        max_rows_per_pane: usize,
    ) -> Result<AttachGridSnapshotState, SessionRuntimeError>;

    fn attach_grid_window_state(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        windows: &[AttachPaneGridWindowRequest],
    ) -> Result<AttachGridWindowState, SessionRuntimeError>;

    fn attach_grid_delta_state(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        pane_ids: &[Uuid],
        base_revisions: &[u64],
        max_batches_per_pane: usize,
    ) -> Result<AttachGridDeltaState, SessionRuntimeError>;

    /// Composite: clear `image_dirty` on each pane, then compute image
    /// registry deltas per pane since the provided sequence numbers.
    /// Returns one `AttachPaneImageDelta` per pane in `pane_ids` order.
    /// When `session_id` is unknown, returns an empty vector.
    fn attach_pane_image_deltas(
        &self,
        session_id: SessionId,
        pane_ids: &[Uuid],
        since_sequences: &[u64],
        payload_codec: Option<&dyn bmux_ipc::compression::CompressionCodec>,
    ) -> Vec<bmux_attach_image_protocol::AttachPaneImageDelta>;

    // ── Attach lifecycle ───────────────────────────────────────────

    fn begin_attach(
        &self,
        session_id: SessionId,
        client_id: ClientId,
    ) -> Result<(), SessionRuntimeError>;

    fn end_attach(&self, session_id: SessionId, client_id: ClientId);

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
    ) -> Result<(u16, u16, u16, u16), SessionRuntimeError>;

    #[allow(
        clippy::too_many_arguments,
        reason = "retargeting atomically combines attach-open and viewport dimensions; grouping would churn the public trait more than it clarifies"
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
        if let Some(previous_session_id) = previous_session_id
            && previous_session_id != next_session_id
        {
            self.end_attach(previous_session_id, client_id);
        }
        self.begin_attach(next_session_id, client_id)?;
        self.set_attach_viewport(
            next_session_id,
            client_id,
            cols,
            rows,
            status_top_inset,
            status_bottom_inset,
            cell_pixel_width,
            cell_pixel_height,
        )
    }

    fn apply_stored_attach_viewport(&self, session_id: SessionId);

    fn attach_layout_state(
        &self,
        session_id: SessionId,
        client_id: ClientId,
    ) -> Result<AttachLayoutState, SessionRuntimeError>;

    fn attach_snapshot_state(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        max_bytes_per_pane: usize,
    ) -> Result<AttachSnapshotState, SessionRuntimeError>;

    fn attach_pane_snapshot_state(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        pane_ids: &[Uuid],
        max_bytes_per_pane: usize,
    ) -> Result<AttachPaneSnapshotState, SessionRuntimeError>;

    // ── Misc ───────────────────────────────────────────────────────

    fn pane_state(&self, session_id: SessionId, pane_id: Uuid) -> Option<PaneState>;
    fn pane_state_reason(&self, session_id: SessionId, pane_id: Uuid) -> Option<String>;

    /// Clear per-pane `output_dirty` atomic.
    fn clear_output_dirty(&self, session_id: SessionId, pane_id: Uuid);

    /// Clear per-pane `image_dirty` atomic.
    fn clear_image_dirty(&self, session_id: SessionId, pane_id: Uuid);

    fn client_is_attached(&self, session_id: SessionId, client_id: ClientId) -> bool;

    fn pane_output_has_pending(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
        client_id: ClientId,
    ) -> bool;

    fn session_has_stored_viewport(&self, session_id: SessionId) -> bool;

    fn snapshot_session_runtime(&self, session_id: SessionId) -> Option<SessionRuntimeSnapshot>;

    /// Snapshot a session runtime for persistence. Implementations may
    /// refresh runtime-derived fields (for example, inspected active
    /// commands / working directories) before returning the projection.
    /// The default keeps simple implementors equivalent to the normal
    /// in-memory snapshot path.
    fn snapshot_session_runtime_for_persistence(
        &self,
        session_id: SessionId,
    ) -> anyhow::Result<Option<SessionRuntimeSnapshot>> {
        Ok(self.snapshot_session_runtime(session_id))
    }

    fn list_session_ids(&self) -> Vec<SessionId>;

    /// Drive the async shutdown for a `RemovedRuntimeInfo` produced
    /// by `remove_runtime` / `remove_all_runtimes` / `close_pane`.
    fn shutdown_removed_runtime(&self, info: RemovedRuntimeInfo);

    /// Composite operation for the per-connection push loop: for the
    /// given `(session, pane, client)` triple, atomically:
    ///   1. Confirm the client is attached to the session,
    ///   2. Clear the pane's `output_dirty` flag,
    ///   3. Read up to `budget` bytes from the pane's output buffer
    ///      for the client's cursor,
    ///   4. Observe whether the pane is currently inside a DEC mode
    ///      2026 synchronized update (the reader-thread flag).
    ///
    /// Returns `(OutputRead, sync_update_active)` when the pane was
    /// found and the client is attached; `None` otherwise (caller
    /// should `continue` its loop).
    fn read_pane_output_for_push(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
        client_id: ClientId,
        budget: usize,
    ) -> Option<(crate::OutputRead, bool)>;

    /// Lag-recovery: for each session the given `client_id` is
    /// attached to, bump its `attach_view_revision` by 1 and return
    /// the list of `(session_id, new_revision)` pairs.
    fn lag_recovery_bump_attach_view_for_client(
        &self,
        client_id: ClientId,
    ) -> Vec<(SessionId, u64)>;

    /// Bump the `attach_view_revision` for a single session. Returns
    /// the new revision, or `None` if the session is not present.
    fn bump_attach_view_revision(&self, session_id: SessionId) -> Option<u64>;

    /// Shell-integration root directory, if configured. Exposed for
    /// tests that verify the server's shell-integration wiring.
    fn shell_integration_root(&self) -> Option<std::path::PathBuf>;

    /// Test-only helper: force a pane into the "exited" state with the
    /// given reason string. Returns `true` when the pane was found and
    /// updated. Used by server tests that simulate process exit without
    /// spawning real PTYs.
    fn test_mark_pane_exited(&self, session_id: SessionId, pane_id: Uuid, reason: String) -> bool;
}

/// Registry newtype wrapping an `Arc<dyn SessionRuntimeManagerApi>`.
#[derive(Clone)]
pub struct SessionRuntimeManagerHandle(pub Arc<dyn SessionRuntimeManagerApi>);

impl SessionRuntimeManagerHandle {
    #[must_use]
    pub fn new<M: SessionRuntimeManagerApi + 'static>(manager: M) -> Self {
        Self(Arc::new(manager))
    }

    #[must_use]
    pub fn from_arc(manager: Arc<dyn SessionRuntimeManagerApi>) -> Self {
        Self(manager)
    }

    #[must_use]
    pub fn noop() -> Self {
        Self::new(NoopSessionRuntimeManager)
    }
}

/// Fallback no-op impl returned when the pane-runtime plugin is not
/// loaded. Every fallible method returns an error; every query-style
/// method returns the "empty" value.
#[derive(Debug, Default)]
pub struct NoopSessionRuntimeManager;

#[allow(unused_variables)]
impl SessionRuntimeManagerApi for NoopSessionRuntimeManager {
    fn start_runtime(&self, _session_id: SessionId) -> anyhow::Result<()> {
        anyhow::bail!("pane-runtime plugin not active")
    }
    fn restore_runtime(
        &self,
        _session_id: SessionId,
        _panes: &[PaneRuntimeMeta],
        _layout_root: Option<PaneLayoutNode>,
        _focused_pane_id: Uuid,
        _floating_surfaces: Vec<FloatingSurfaceRuntime>,
        _attach_viewport: Option<AttachViewport>,
    ) -> anyhow::Result<()> {
        anyhow::bail!("pane-runtime plugin not active")
    }
    fn remove_runtime(&self, _session_id: SessionId) -> Option<RemovedRuntimeInfo> {
        None
    }
    fn remove_all_runtimes(&self) -> Vec<RemovedRuntimeInfo> {
        Vec::new()
    }
    fn session_exists(&self, _session_id: SessionId) -> bool {
        false
    }
    fn active_session_ids(&self) -> Vec<SessionId> {
        Vec::new()
    }
    fn split_pane(
        &self,
        _session_id: SessionId,
        _target: Option<PaneSelector>,
        _direction: PaneSplitDirection,
    ) -> anyhow::Result<Uuid> {
        anyhow::bail!("pane-runtime plugin not active")
    }
    fn launch_pane(
        &self,
        _session_id: SessionId,
        _target: Option<PaneSelector>,
        _direction: PaneSplitDirection,
        _name: Option<String>,
        _command: PaneLaunchCommand,
    ) -> anyhow::Result<Uuid> {
        anyhow::bail!("pane-runtime plugin not active")
    }
    fn focus_pane(
        &self,
        _session_id: SessionId,
        _direction: PaneFocusDirection,
    ) -> anyhow::Result<Uuid> {
        anyhow::bail!("pane-runtime plugin not active")
    }
    fn focus_pane_target(
        &self,
        _session_id: SessionId,
        _target: &PaneSelector,
    ) -> anyhow::Result<Uuid> {
        anyhow::bail!("pane-runtime plugin not active")
    }
    fn resize_pane(
        &self,
        _session_id: SessionId,
        _target: Option<PaneSelector>,
        _direction: PaneResizeDirection,
        _cells: u16,
    ) -> anyhow::Result<()> {
        anyhow::bail!("pane-runtime plugin not active")
    }
    fn set_pane_pty_size(
        &self,
        _session_id: SessionId,
        _pane_id: Uuid,
        _rows: u16,
        _cols: u16,
    ) -> anyhow::Result<()> {
        anyhow::bail!("pane-runtime plugin not active")
    }
    fn close_pane(
        &self,
        _session_id: SessionId,
        _target: Option<PaneSelector>,
    ) -> anyhow::Result<(Uuid, Option<RemovedRuntimeInfo>)> {
        anyhow::bail!("pane-runtime plugin not active")
    }
    fn restart_pane(
        &self,
        _session_id: SessionId,
        _target: Option<PaneSelector>,
    ) -> anyhow::Result<Uuid> {
        anyhow::bail!("pane-runtime plugin not active")
    }
    fn toggle_zoom(&self, _session_id: SessionId) -> anyhow::Result<(Uuid, bool)> {
        anyhow::bail!("pane-runtime plugin not active")
    }
    fn create_floating_pane(
        &self,
        _session_id: SessionId,
        _target: Option<PaneSelector>,
        _rect: LayoutRect,
        _scope: FloatingPaneScope,
        _layer: FloatingPaneLayer,
        _z: i32,
        _name: Option<String>,
        _command: Option<PaneLaunchCommand>,
        _anchor_pane_id: Option<Uuid>,
        _context_id: Option<Uuid>,
        _client_id: Option<ClientId>,
    ) -> anyhow::Result<FloatingPaneRuntimeSummary> {
        anyhow::bail!("pane-runtime plugin not active")
    }
    fn list_floating_panes(
        &self,
        _session_id: SessionId,
    ) -> anyhow::Result<Vec<FloatingPaneRuntimeSummary>> {
        Ok(Vec::new())
    }
    fn move_floating_pane(
        &self,
        _session_id: SessionId,
        _pane_id: Uuid,
        _x: u16,
        _y: u16,
    ) -> anyhow::Result<FloatingPaneRuntimeSummary> {
        anyhow::bail!("pane-runtime plugin not active")
    }
    fn resize_floating_pane(
        &self,
        _session_id: SessionId,
        _pane_id: Uuid,
        _w: u16,
        _h: u16,
    ) -> anyhow::Result<FloatingPaneRuntimeSummary> {
        anyhow::bail!("pane-runtime plugin not active")
    }
    fn focus_floating_pane(
        &self,
        _session_id: SessionId,
        _pane_id: Uuid,
    ) -> anyhow::Result<FloatingPaneRuntimeSummary> {
        anyhow::bail!("pane-runtime plugin not active")
    }
    fn raise_floating_pane(
        &self,
        _session_id: SessionId,
        _pane_id: Uuid,
    ) -> anyhow::Result<FloatingPaneRuntimeSummary> {
        anyhow::bail!("pane-runtime plugin not active")
    }
    fn lower_floating_pane(
        &self,
        _session_id: SessionId,
        _pane_id: Uuid,
    ) -> anyhow::Result<FloatingPaneRuntimeSummary> {
        anyhow::bail!("pane-runtime plugin not active")
    }
    fn set_floating_pane_z(
        &self,
        _session_id: SessionId,
        _pane_id: Uuid,
        _z: i32,
    ) -> anyhow::Result<FloatingPaneRuntimeSummary> {
        anyhow::bail!("pane-runtime plugin not active")
    }
    fn set_floating_pane_layer(
        &self,
        _session_id: SessionId,
        _pane_id: Uuid,
        _layer: FloatingPaneLayer,
    ) -> anyhow::Result<FloatingPaneRuntimeSummary> {
        anyhow::bail!("pane-runtime plugin not active")
    }
    fn close_floating_pane(
        &self,
        _session_id: SessionId,
        _pane_id: Uuid,
    ) -> anyhow::Result<(Uuid, Option<RemovedRuntimeInfo>)> {
        anyhow::bail!("pane-runtime plugin not active")
    }
    fn list_panes(&self, _session_id: SessionId) -> anyhow::Result<Vec<PaneSummary>> {
        Ok(Vec::new())
    }
    fn list_pane_processes(&self) -> Vec<PaneProcessIdentity> {
        Vec::new()
    }
    fn pane_process_identity(
        &self,
        _session_id: SessionId,
        _pane_id: Uuid,
    ) -> Option<PaneProcessIdentity> {
        None
    }
    fn write_input(
        &self,
        _session_id: SessionId,
        _client_id: ClientId,
        _data: Vec<u8>,
    ) -> Result<(usize, Uuid), SessionRuntimeError> {
        Err(SessionRuntimeError::NotFound)
    }
    fn write_input_to_pane(
        &self,
        _session_id: SessionId,
        _client_id: ClientId,
        _pane_id: Uuid,
        _data: Vec<u8>,
    ) -> Result<usize, SessionRuntimeError> {
        Err(SessionRuntimeError::NotFound)
    }
    fn read_pane_output_at(
        &self,
        _session_id: SessionId,
        _pane_id: Uuid,
        _cursor: u64,
        _max_bytes: usize,
    ) -> Result<crate::OutputRead, SessionRuntimeError> {
        Err(SessionRuntimeError::NotFound)
    }
    fn set_client_write_permission(
        &self,
        _session_id: SessionId,
        _client_id: ClientId,
        _allowed: bool,
    ) {
    }
    fn client_can_write(&self, _session_id: SessionId, _client_id: ClientId) -> bool {
        true
    }
    fn read_output(
        &self,
        _session_id: SessionId,
        _client_id: ClientId,
        _max_bytes: usize,
    ) -> Result<Vec<u8>, SessionRuntimeError> {
        Ok(Vec::new())
    }
    fn read_pane_output_batch(
        &self,
        _session_id: SessionId,
        _client_id: ClientId,
        _pane_ids: &[Uuid],
        _max_bytes: usize,
    ) -> Result<Vec<AttachPaneChunk>, SessionRuntimeError> {
        Ok(Vec::new())
    }
    fn attach_pane_output_batch_with_dirty_check(
        &self,
        _session_id: SessionId,
        _client_id: ClientId,
        _pane_ids: &[Uuid],
        _max_bytes: usize,
    ) -> (Result<Vec<AttachPaneChunk>, SessionRuntimeError>, bool) {
        (Ok(Vec::new()), false)
    }
    fn attach_grid_snapshot_state(
        &self,
        _session_id: SessionId,
        _client_id: ClientId,
        _pane_ids: &[Uuid],
        _max_rows_per_pane: usize,
    ) -> Result<AttachGridSnapshotState, SessionRuntimeError> {
        Err(SessionRuntimeError::NotFound)
    }
    fn attach_grid_window_state(
        &self,
        _session_id: SessionId,
        _client_id: ClientId,
        _windows: &[AttachPaneGridWindowRequest],
    ) -> Result<AttachGridWindowState, SessionRuntimeError> {
        Err(SessionRuntimeError::NotFound)
    }

    fn attach_grid_delta_state(
        &self,
        _session_id: SessionId,
        _client_id: ClientId,
        _pane_ids: &[Uuid],
        _base_revisions: &[u64],
        _max_batches_per_pane: usize,
    ) -> Result<AttachGridDeltaState, SessionRuntimeError> {
        Err(SessionRuntimeError::NotFound)
    }
    fn attach_pane_image_deltas(
        &self,
        _session_id: SessionId,
        _pane_ids: &[Uuid],
        _since_sequences: &[u64],
        _payload_codec: Option<&dyn bmux_ipc::compression::CompressionCodec>,
    ) -> Vec<bmux_attach_image_protocol::AttachPaneImageDelta> {
        Vec::new()
    }
    fn begin_attach(
        &self,
        _session_id: SessionId,
        _client_id: ClientId,
    ) -> Result<(), SessionRuntimeError> {
        Err(SessionRuntimeError::NotFound)
    }
    fn end_attach(&self, _session_id: SessionId, _client_id: ClientId) {}
    fn set_attach_viewport(
        &self,
        _session_id: SessionId,
        _client_id: ClientId,
        _cols: u16,
        _rows: u16,
        _status_top_inset: u16,
        _status_bottom_inset: u16,
        _cell_pixel_width: u16,
        _cell_pixel_height: u16,
    ) -> Result<(u16, u16, u16, u16), SessionRuntimeError> {
        Err(SessionRuntimeError::NotFound)
    }
    fn apply_stored_attach_viewport(&self, _session_id: SessionId) {}
    fn attach_layout_state(
        &self,
        _session_id: SessionId,
        _client_id: ClientId,
    ) -> Result<AttachLayoutState, SessionRuntimeError> {
        Err(SessionRuntimeError::NotFound)
    }
    fn attach_snapshot_state(
        &self,
        _session_id: SessionId,
        _client_id: ClientId,
        _max_bytes_per_pane: usize,
    ) -> Result<AttachSnapshotState, SessionRuntimeError> {
        Err(SessionRuntimeError::NotFound)
    }
    fn attach_pane_snapshot_state(
        &self,
        _session_id: SessionId,
        _client_id: ClientId,
        _pane_ids: &[Uuid],
        _max_bytes_per_pane: usize,
    ) -> Result<AttachPaneSnapshotState, SessionRuntimeError> {
        Err(SessionRuntimeError::NotFound)
    }
    fn pane_state(&self, _session_id: SessionId, _pane_id: Uuid) -> Option<PaneState> {
        None
    }
    fn pane_state_reason(&self, _session_id: SessionId, _pane_id: Uuid) -> Option<String> {
        None
    }
    fn clear_output_dirty(&self, _session_id: SessionId, _pane_id: Uuid) {}
    fn clear_image_dirty(&self, _session_id: SessionId, _pane_id: Uuid) {}
    fn client_is_attached(&self, _session_id: SessionId, _client_id: ClientId) -> bool {
        false
    }
    fn pane_output_has_pending(
        &self,
        _session_id: SessionId,
        _pane_id: Uuid,
        _client_id: ClientId,
    ) -> bool {
        false
    }
    fn session_has_stored_viewport(&self, _session_id: SessionId) -> bool {
        false
    }
    fn snapshot_session_runtime(&self, _session_id: SessionId) -> Option<SessionRuntimeSnapshot> {
        None
    }
    fn list_session_ids(&self) -> Vec<SessionId> {
        Vec::new()
    }
    fn shutdown_removed_runtime(&self, _info: RemovedRuntimeInfo) {}
    fn read_pane_output_for_push(
        &self,
        _session_id: SessionId,
        _pane_id: Uuid,
        _client_id: ClientId,
        _budget: usize,
    ) -> Option<(crate::OutputRead, bool)> {
        None
    }
    fn lag_recovery_bump_attach_view_for_client(
        &self,
        _client_id: ClientId,
    ) -> Vec<(SessionId, u64)> {
        Vec::new()
    }
    fn bump_attach_view_revision(&self, _session_id: SessionId) -> Option<u64> {
        None
    }
    fn shell_integration_root(&self) -> Option<std::path::PathBuf> {
        None
    }
    fn test_mark_pane_exited(
        &self,
        _session_id: SessionId,
        _pane_id: Uuid,
        _reason: String,
    ) -> bool {
        false
    }
}
