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
    AttachViewport, FloatingSurfaceRuntime, PaneLayoutNode, PaneRuntimeMeta, SessionRuntimeError,
};
use bmux_ipc::{
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

/// Per-session pane-runtime projection used when building a
/// persistence snapshot.
#[derive(Debug, Clone)]
pub struct SessionRuntimeSnapshot {
    pub session_id: SessionId,
    pub panes: Vec<PaneRuntimeMeta>,
    pub focused_pane_id: Uuid,
    pub layout_root: PaneLayoutNode,
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
    ) -> anyhow::Result<()>;

    fn remove_runtime(&self, session_id: SessionId) -> Option<RemovedRuntimeInfo>;
    fn remove_all_runtimes(&self) -> Vec<RemovedRuntimeInfo>;
    fn session_exists(&self, session_id: SessionId) -> bool;

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
        delta: i16,
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

    // ── Pane I/O ───────────────────────────────────────────────────

    fn list_panes(&self, session_id: SessionId) -> anyhow::Result<Vec<PaneSummary>>;

    fn write_input(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        data: Vec<u8>,
    ) -> Result<(usize, Uuid), SessionRuntimeError>;

    fn write_input_to_pane(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
        data: Vec<u8>,
    ) -> Result<usize, SessionRuntimeError>;

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
    ) -> Vec<bmux_ipc::AttachPaneImageDelta>;

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
        _delta: i16,
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
    fn list_panes(&self, _session_id: SessionId) -> anyhow::Result<Vec<PaneSummary>> {
        Ok(Vec::new())
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
        _pane_id: Uuid,
        _data: Vec<u8>,
    ) -> Result<usize, SessionRuntimeError> {
        Err(SessionRuntimeError::NotFound)
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
    fn attach_pane_image_deltas(
        &self,
        _session_id: SessionId,
        _pane_ids: &[Uuid],
        _since_sequences: &[u64],
        _payload_codec: Option<&dyn bmux_ipc::compression::CompressionCodec>,
    ) -> Vec<bmux_ipc::AttachPaneImageDelta> {
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
