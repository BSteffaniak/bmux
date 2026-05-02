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
    AttachLayout as AttachLayoutRecord, AttachPaneGridSnapshot, AttachPaneImages,
    AttachPaneOutputBatch, AttachPaneSnapshot as AttachPaneSnapshotRecord,
    AttachSnapshot as AttachSnapshotRecord, AttachStateError, PaneChunk, PaneGridSnapshot,
    PaneInputMode, PaneMouseProtocol,
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
pub struct AttachPaneGridSnapshotArgs {
    pub session_id: Uuid,
    pub pane_ids: Vec<Uuid>,
    pub max_rows_per_pane: u32,
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

/// `attach-pane-images` delivers image-registry deltas serialized as
/// JSON (`Vec<AttachPaneImageDelta>`).
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
