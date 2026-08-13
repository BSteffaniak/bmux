//! Typed handlers for the `attach-runtime-state` interface.
//!
//! State queries dispatch through the registered
//! `SessionRuntimeManagerHandle`. JSON-encoded payloads (layout,
//! snapshot, pane-images) serialize the relevant IPC structs so
//! consumers can decode them without the plugin having to invent a
//! parallel BPDL representation of every field.

use bmux_attach_layout_protocol::{
    AttachPaneChunk, AttachPaneInputMode, AttachPaneMouseProtocol, AttachScene, PaneLayoutNode,
    PaneSummary,
};
use bmux_pane_runtime_plugin_api::attach_runtime_state::{
    AttachLayout as AttachLayoutRecord, AttachPaneGridDelta, AttachPaneGridSnapshot,
    AttachPaneGridWindow, AttachPaneImages, AttachPaneOutputBatch,
    AttachPaneSnapshot as AttachPaneSnapshotRecord, AttachSnapshot as AttachSnapshotRecord,
    AttachStateError, PaneChunk, PaneGridDelta, PaneGridSnapshot, PaneGridWindow,
    PaneGridWindowRequest, PaneInputMode, PaneMouseProtocol, PaneOutputCursorRead,
    PaneScrollbackPin, PaneScrollbackUnpinAck,
};
use bmux_plugin_sdk::NativeServiceContext;
use bmux_session_models::{ClientId, SessionId};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachLayoutArgs {
    pub session_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachSnapshotArgs {
    pub session_id: Uuid,
    pub max_bytes_per_pane: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachPaneSnapshotArgs {
    pub session_id: Uuid,
    pub pane_ids: Vec<Uuid>,
    pub max_bytes_per_pane: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachPaneOutputBatchArgs {
    pub session_id: Uuid,
    pub pane_ids: Vec<Uuid>,
    pub max_bytes: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneOutputCursorArgs {
    pub session_id: Uuid,
    pub pane_id: Uuid,
    pub cursor: u64,
    pub max_bytes: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneGridSnapshotArgs {
    pub session_id: Uuid,
    pub pane_id: Uuid,
    pub max_rows: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachPaneGridSnapshotArgs {
    pub session_id: Uuid,
    pub pane_ids: Vec<Uuid>,
    pub max_rows_per_pane: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachPaneGridWindowArgs {
    pub session_id: Uuid,
    pub windows: Vec<PaneGridWindowRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachPaneScrollbackPinArgs {
    pub session_id: Uuid,
    pub pane_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(
    clippy::struct_field_names,
    reason = "fields mirror the generated BPDL request contract"
)]
pub struct AttachPaneScrollbackUnpinArgs {
    pub session_id: Uuid,
    pub pane_id: Uuid,
    pub pin_id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachPaneGridDeltaArgs {
    pub session_id: Uuid,
    pub pane_ids: Vec<Uuid>,
    pub base_revisions: Vec<u64>,
    pub max_batches_per_pane: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachPaneImagesArgs {
    pub session_id: Uuid,
    pub pane_ids: Vec<Uuid>,
    pub since_sequences: Vec<u64>,
}

fn failed(reason: impl Into<String>) -> AttachStateError {
    AttachStateError::Failed {
        reason: reason.into(),
    }
}

fn caller_client_id(ctx: &NativeServiceContext) -> ClientId {
    ctx.caller_client_id
        .map_or_else(|| ClientId(Uuid::nil()), ClientId)
}

#[derive(Serialize)]
struct LayoutPayload {
    panes: Vec<PaneSummary>,
    layout_root: PaneLayoutNode,
    scene: AttachScene,
    zoomed: bool,
}

pub fn attach_layout_state(
    req: &AttachLayoutArgs,
    ctx: &NativeServiceContext,
) -> Result<AttachLayoutRecord, AttachStateError> {
    let handle = super::session_runtime_handle()
        .ok_or_else(|| failed("pane-runtime manager handle not registered"))?;
    let state = handle
        .0
        .attach_layout_state(SessionId(req.session_id), caller_client_id(ctx))
        .map_err(|_| AttachStateError::NotAttached)?;
    let payload = LayoutPayload {
        panes: state.panes,
        layout_root: state.layout_root,
        scene: state.scene,
        zoomed: state.zoomed,
    };
    let encoded =
        serde_json::to_vec(&payload).map_err(|e| failed(format!("encode layout payload: {e}")))?;
    Ok(AttachLayoutRecord {
        session_id: req.session_id,
        context_id: None,
        focused_pane_id: state.focused_pane_id,
        encoded,
    })
}

#[derive(Serialize)]
struct SnapshotLayoutPayload {
    panes: Vec<PaneSummary>,
    layout_root: PaneLayoutNode,
    scene: AttachScene,
}

pub fn attach_snapshot_state(
    req: &AttachSnapshotArgs,
    ctx: &NativeServiceContext,
) -> Result<AttachSnapshotRecord, AttachStateError> {
    let handle = super::session_runtime_handle()
        .ok_or_else(|| failed("pane-runtime manager handle not registered"))?;
    let state = handle
        .0
        .attach_snapshot_state(
            SessionId(req.session_id),
            caller_client_id(ctx),
            req.max_bytes_per_pane as usize,
        )
        .map_err(|_| AttachStateError::NotAttached)?;
    let layout_payload = SnapshotLayoutPayload {
        panes: state.panes,
        layout_root: state.layout_root,
        scene: state.scene,
    };
    let layout_encoded = serde_json::to_vec(&layout_payload)
        .map_err(|e| failed(format!("encode layout payload: {e}")))?;
    Ok(AttachSnapshotRecord {
        session_id: req.session_id,
        context_id: None,
        focused_pane_id: state.focused_pane_id,
        zoomed: state.zoomed,
        layout_encoded,
        chunks: state.chunks.into_iter().map(chunk_to_record).collect(),
        pane_mouse_protocols: state
            .pane_mouse_protocols
            .into_iter()
            .map(mouse_to_record)
            .collect(),
        pane_input_modes: state
            .pane_input_modes
            .into_iter()
            .map(input_mode_to_record)
            .collect(),
    })
}

pub fn attach_pane_snapshot_state(
    req: &AttachPaneSnapshotArgs,
    ctx: &NativeServiceContext,
) -> Result<AttachPaneSnapshotRecord, AttachStateError> {
    let handle = super::session_runtime_handle()
        .ok_or_else(|| failed("pane-runtime manager handle not registered"))?;
    let state = handle
        .0
        .attach_pane_snapshot_state(
            SessionId(req.session_id),
            caller_client_id(ctx),
            &req.pane_ids,
            req.max_bytes_per_pane as usize,
        )
        .map_err(|_| AttachStateError::NotAttached)?;
    Ok(AttachPaneSnapshotRecord {
        chunks: state.chunks.into_iter().map(chunk_to_record).collect(),
        pane_mouse_protocols: state
            .pane_mouse_protocols
            .into_iter()
            .map(mouse_to_record)
            .collect(),
        pane_input_modes: state
            .pane_input_modes
            .into_iter()
            .map(input_mode_to_record)
            .collect(),
    })
}

pub fn attach_pane_output_batch(
    req: &AttachPaneOutputBatchArgs,
    ctx: &NativeServiceContext,
) -> Result<AttachPaneOutputBatch, AttachStateError> {
    let handle = super::session_runtime_handle()
        .ok_or_else(|| failed("pane-runtime manager handle not registered"))?;
    let (chunks, output_still_pending) = handle.0.attach_pane_output_batch_with_dirty_check(
        SessionId(req.session_id),
        caller_client_id(ctx),
        &req.pane_ids,
        req.max_bytes as usize,
    );
    let chunks = chunks.map_err(|_| AttachStateError::NotAttached)?;
    Ok(AttachPaneOutputBatch {
        chunks: chunks.into_iter().map(chunk_to_record).collect(),
        output_still_pending,
    })
}

pub fn pane_output_cursor_state(
    req: &PaneOutputCursorArgs,
) -> Result<PaneOutputCursorRead, AttachStateError> {
    let handle = super::session_runtime_handle()
        .ok_or_else(|| failed("pane-runtime manager handle not registered"))?;
    let read = handle
        .0
        .read_pane_output_at(
            SessionId(req.session_id),
            req.pane_id,
            req.cursor,
            usize::try_from(req.max_bytes).unwrap_or(usize::MAX),
        )
        .map_err(|error| failed(error.to_string()))?;
    Ok(PaneOutputCursorRead {
        data: read.bytes,
        retained_start: read.retained_start,
        stream_start: read.stream_start,
        stream_end: read.stream_end,
        source_end: read.source_end,
        stream_gap: read.stream_gap,
    })
}

pub fn pane_grid_snapshot_state(
    req: &PaneGridSnapshotArgs,
) -> Result<PaneGridSnapshot, AttachStateError> {
    let handle = super::session_runtime_handle()
        .ok_or_else(|| failed("pane-runtime manager handle not registered"))?;
    let state = handle
        .0
        .attach_grid_snapshot_state(
            SessionId(req.session_id),
            ClientId(Uuid::nil()),
            &[req.pane_id],
            usize::try_from(req.max_rows).unwrap_or(usize::MAX),
        )
        .map_err(|error| failed(error.to_string()))?;
    state
        .snapshots
        .into_iter()
        .find(|snapshot| snapshot.pane_id == req.pane_id)
        .map(|snapshot| PaneGridSnapshot {
            pane_id: snapshot.pane_id,
            stream_end: snapshot.stream_end,
            encoded: snapshot.encoded,
        })
        .ok_or_else(|| failed("pane snapshot is missing"))
}

pub fn attach_pane_grid_snapshot_state(
    req: &AttachPaneGridSnapshotArgs,
    ctx: &NativeServiceContext,
) -> Result<AttachPaneGridSnapshot, AttachStateError> {
    let handle = super::session_runtime_handle()
        .ok_or_else(|| failed("pane-runtime manager handle not registered"))?;
    let state = handle
        .0
        .attach_grid_snapshot_state(
            SessionId(req.session_id),
            caller_client_id(ctx),
            &req.pane_ids,
            req.max_rows_per_pane as usize,
        )
        .map_err(|_| AttachStateError::NotAttached)?;
    Ok(AttachPaneGridSnapshot {
        snapshots: state
            .snapshots
            .into_iter()
            .map(|snapshot| PaneGridSnapshot {
                pane_id: snapshot.pane_id,
                stream_end: snapshot.stream_end,
                encoded: snapshot.encoded,
            })
            .collect(),
    })
}

pub fn attach_pane_grid_window_state(
    req: &AttachPaneGridWindowArgs,
    ctx: &NativeServiceContext,
) -> Result<AttachPaneGridWindow, AttachStateError> {
    let handle = super::session_runtime_handle()
        .ok_or_else(|| failed("pane-runtime manager handle not registered"))?;
    let windows = req
        .windows
        .iter()
        .map(
            |window| bmux_pane_runtime_state::AttachPaneGridWindowRequest {
                pane_id: window.pane_id,
                scrollback_offset: window.scrollback_offset as usize,
                rows: window.rows as usize,
                anchor_total_scrolled_rows: window.anchor_total_scrolled_rows,
                pin_id: window.pin_id,
            },
        )
        .collect::<Vec<_>>();
    let state = handle
        .0
        .attach_grid_window_state(SessionId(req.session_id), caller_client_id(ctx), &windows)
        .map_err(|_| AttachStateError::NotAttached)?;
    Ok(AttachPaneGridWindow {
        windows: state
            .windows
            .into_iter()
            .map(|window| PaneGridWindow {
                pane_id: window.pane_id,
                scrollback_offset: u32::try_from(window.scrollback_offset).unwrap_or(u32::MAX),
                max_scrollback_offset: u32::try_from(window.max_scrollback_offset)
                    .unwrap_or(u32::MAX),
                total_scrolled_rows: window.total_scrolled_rows,
                anchor_delta_rows: u32::try_from(window.anchor_delta_rows).unwrap_or(u32::MAX),
                anchor_clamped: window.anchor_clamped,
                stream_end: window.stream_end,
                encoded: window.encoded,
            })
            .collect(),
    })
}

pub fn attach_pane_scrollback_pin(
    req: &AttachPaneScrollbackPinArgs,
    ctx: &NativeServiceContext,
) -> Result<PaneScrollbackPin, AttachStateError> {
    let handle = super::session_runtime_handle()
        .ok_or_else(|| failed("pane-runtime manager handle not registered"))?;
    let pin = handle
        .0
        .attach_scrollback_pin(
            SessionId(req.session_id),
            caller_client_id(ctx),
            req.pane_id,
        )
        .map_err(|_| AttachStateError::NotAttached)?;
    Ok(PaneScrollbackPin {
        pane_id: pin.pane_id,
        pin_id: pin.pin_id,
        total_scrolled_rows: pin.total_scrolled_rows,
        max_scrollback_offset: u32::try_from(pin.max_scrollback_offset).unwrap_or(u32::MAX),
        stream_end: pin.stream_end,
    })
}

pub fn attach_pane_scrollback_unpin(
    req: &AttachPaneScrollbackUnpinArgs,
    ctx: &NativeServiceContext,
) -> Result<PaneScrollbackUnpinAck, AttachStateError> {
    let handle = super::session_runtime_handle()
        .ok_or_else(|| failed("pane-runtime manager handle not registered"))?;
    let ack = handle
        .0
        .attach_scrollback_unpin(
            SessionId(req.session_id),
            caller_client_id(ctx),
            req.pane_id,
            req.pin_id,
        )
        .map_err(|_| AttachStateError::NotAttached)?;
    Ok(PaneScrollbackUnpinAck {
        pane_id: ack.pane_id,
        pin_id: ack.pin_id,
        released: ack.released,
    })
}

/// `attach-pane-grid-delta-state` delivers revisioned structured grid
/// deltas serialized as JSON (`Vec<GridDeltaBatch>`) per pane.
pub fn attach_pane_grid_delta_state(
    req: &AttachPaneGridDeltaArgs,
    ctx: &NativeServiceContext,
) -> Result<AttachPaneGridDelta, AttachStateError> {
    let handle = super::session_runtime_handle()
        .ok_or_else(|| failed("pane-runtime manager handle not registered"))?;
    let state = handle
        .0
        .attach_grid_delta_state(
            SessionId(req.session_id),
            caller_client_id(ctx),
            &req.pane_ids,
            &req.base_revisions,
            req.max_batches_per_pane as usize,
        )
        .map_err(|_| AttachStateError::NotAttached)?;
    Ok(AttachPaneGridDelta {
        deltas: state
            .deltas
            .into_iter()
            .map(|delta| PaneGridDelta {
                pane_id: delta.pane_id,
                base_revision: delta.base_revision,
                revision: delta.revision,
                desynced: delta.desynced,
                encoded: delta.encoded,
            })
            .collect(),
    })
}

pub fn attach_pane_images(
    req: &AttachPaneImagesArgs,
    _ctx: &NativeServiceContext,
) -> Result<AttachPaneImages, AttachStateError> {
    let handle = super::session_runtime_handle()
        .ok_or_else(|| failed("pane-runtime manager handle not registered"))?;
    let deltas = handle.0.attach_pane_image_deltas(
        SessionId(req.session_id),
        &req.pane_ids,
        &req.since_sequences,
        None,
    );
    let encoded = serde_json::to_vec(&deltas)
        .map_err(|e| failed(format!("encode pane-images deltas: {e}")))?;
    Ok(AttachPaneImages { encoded })
}

fn chunk_to_record(chunk: AttachPaneChunk) -> PaneChunk {
    PaneChunk {
        pane_id: chunk.pane_id,
        data: chunk.data,
        stream_start: chunk.stream_start,
        stream_end: chunk.stream_end,
        stream_gap: chunk.stream_gap,
        sync_update_active: chunk.sync_update_active,
    }
}

fn mouse_to_record(mouse: AttachPaneMouseProtocol) -> PaneMouseProtocol {
    let encoded = serde_json::to_vec(&mouse.protocol).unwrap_or_default();
    PaneMouseProtocol {
        pane_id: mouse.pane_id,
        encoded,
    }
}

fn input_mode_to_record(mode: AttachPaneInputMode) -> PaneInputMode {
    let encoded = serde_json::to_vec(&mode.mode).unwrap_or_default();
    PaneInputMode {
        pane_id: mode.pane_id,
        encoded,
    }
}
