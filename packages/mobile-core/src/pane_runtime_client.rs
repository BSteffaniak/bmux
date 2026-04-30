use bmux_attach_layout_protocol::{
    AttachPaneChunk, AttachPaneInputMode, AttachPaneMouseProtocol, AttachScene, PaneLayoutNode,
    PaneSummary,
};
use bmux_client::{
    AttachLayoutState, AttachOpenInfo, AttachPaneSnapshotState, AttachSnapshotState, ClientError,
    PaneOutputBatchResult,
};
use bmux_ipc::{AttachGrant, ErrorCode};
use bmux_pane_runtime_plugin_api::{
    attach_runtime_commands as attach_commands, attach_runtime_state as attach_state,
};
use bmux_session_models::SessionSelector;
use std::future::Future;
use uuid::Uuid;

type ClientResult<T> = bmux_client::Result<T>;

#[derive(serde::Deserialize)]
struct LayoutPayload {
    panes: Vec<PaneSummary>,
    layout_root: PaneLayoutNode,
    scene: AttachScene,
    zoomed: bool,
}

#[derive(serde::Deserialize)]
struct SnapshotLayoutPayload {
    panes: Vec<PaneSummary>,
    layout_root: PaneLayoutNode,
    scene: AttachScene,
}

pub trait PaneRuntimeClientExt {
    fn attach_grant(
        &mut self,
        selector: SessionSelector,
    ) -> impl Future<Output = ClientResult<AttachGrant>> + Send;

    fn open_attach_stream_info(
        &mut self,
        grant: &AttachGrant,
    ) -> impl Future<Output = ClientResult<AttachOpenInfo>> + Send;

    fn attach_set_viewport(
        &mut self,
        session_id: Uuid,
        cols: u16,
        rows: u16,
    ) -> impl Future<Output = ClientResult<(u16, u16)>> + Send;

    fn attach_set_viewport_with_insets(
        &mut self,
        session_id: Uuid,
        cols: u16,
        rows: u16,
        status_top_inset: u16,
        status_bottom_inset: u16,
    ) -> impl Future<Output = ClientResult<(u16, u16)>> + Send;

    fn attach_input(
        &mut self,
        session_id: Uuid,
        data: Vec<u8>,
    ) -> impl Future<Output = ClientResult<usize>> + Send;

    fn attach_layout(
        &mut self,
        session_id: Uuid,
    ) -> impl Future<Output = ClientResult<AttachLayoutState>> + Send;

    fn attach_pane_output_batch(
        &mut self,
        session_id: Uuid,
        pane_ids: Vec<Uuid>,
        max_bytes: usize,
    ) -> impl Future<Output = ClientResult<PaneOutputBatchResult>> + Send;

    fn attach_snapshot(
        &mut self,
        session_id: Uuid,
        max_bytes_per_pane: usize,
    ) -> impl Future<Output = ClientResult<AttachSnapshotState>> + Send;

    fn attach_pane_snapshot(
        &mut self,
        session_id: Uuid,
        pane_ids: Vec<Uuid>,
        max_bytes_per_pane: usize,
    ) -> impl Future<Output = ClientResult<AttachPaneSnapshotState>> + Send;

    fn detach(&mut self) -> impl Future<Output = ClientResult<()>> + Send;
}

impl PaneRuntimeClientExt for bmux_client::BmuxClient {
    async fn attach_grant(&mut self, selector: SessionSelector) -> ClientResult<AttachGrant> {
        attach_grant(self, selector).await
    }

    async fn open_attach_stream_info(
        &mut self,
        grant: &AttachGrant,
    ) -> ClientResult<AttachOpenInfo> {
        open_attach_stream_info(self, grant).await
    }

    async fn attach_set_viewport(
        &mut self,
        session_id: Uuid,
        cols: u16,
        rows: u16,
    ) -> ClientResult<(u16, u16)> {
        attach_set_viewport_with_insets(self, session_id, cols, rows, 0, 0).await
    }

    async fn attach_set_viewport_with_insets(
        &mut self,
        session_id: Uuid,
        cols: u16,
        rows: u16,
        status_top_inset: u16,
        status_bottom_inset: u16,
    ) -> ClientResult<(u16, u16)> {
        attach_set_viewport_with_insets(
            self,
            session_id,
            cols,
            rows,
            status_top_inset,
            status_bottom_inset,
        )
        .await
    }

    async fn attach_input(&mut self, session_id: Uuid, data: Vec<u8>) -> ClientResult<usize> {
        attach_input(self, session_id, data).await
    }

    async fn attach_layout(&mut self, session_id: Uuid) -> ClientResult<AttachLayoutState> {
        attach_layout(self, session_id).await
    }

    async fn attach_pane_output_batch(
        &mut self,
        session_id: Uuid,
        pane_ids: Vec<Uuid>,
        max_bytes: usize,
    ) -> ClientResult<PaneOutputBatchResult> {
        attach_pane_output_batch(self, session_id, pane_ids, max_bytes).await
    }

    async fn attach_snapshot(
        &mut self,
        session_id: Uuid,
        max_bytes_per_pane: usize,
    ) -> ClientResult<AttachSnapshotState> {
        attach_snapshot(self, session_id, max_bytes_per_pane).await
    }

    async fn attach_pane_snapshot(
        &mut self,
        session_id: Uuid,
        pane_ids: Vec<Uuid>,
        max_bytes_per_pane: usize,
    ) -> ClientResult<AttachPaneSnapshotState> {
        attach_pane_snapshot(self, session_id, pane_ids, max_bytes_per_pane).await
    }

    async fn detach(&mut self) -> ClientResult<()> {
        detach(self).await
    }
}

impl PaneRuntimeClientExt for bmux_client::StreamingBmuxClient {
    async fn attach_grant(&mut self, selector: SessionSelector) -> ClientResult<AttachGrant> {
        attach_grant(self, selector).await
    }

    async fn open_attach_stream_info(
        &mut self,
        grant: &AttachGrant,
    ) -> ClientResult<AttachOpenInfo> {
        open_attach_stream_info(self, grant).await
    }

    async fn attach_set_viewport(
        &mut self,
        session_id: Uuid,
        cols: u16,
        rows: u16,
    ) -> ClientResult<(u16, u16)> {
        attach_set_viewport_with_insets(self, session_id, cols, rows, 0, 0).await
    }

    async fn attach_set_viewport_with_insets(
        &mut self,
        session_id: Uuid,
        cols: u16,
        rows: u16,
        status_top_inset: u16,
        status_bottom_inset: u16,
    ) -> ClientResult<(u16, u16)> {
        attach_set_viewport_with_insets(
            self,
            session_id,
            cols,
            rows,
            status_top_inset,
            status_bottom_inset,
        )
        .await
    }

    async fn attach_input(&mut self, session_id: Uuid, data: Vec<u8>) -> ClientResult<usize> {
        attach_input(self, session_id, data).await
    }

    async fn attach_layout(&mut self, session_id: Uuid) -> ClientResult<AttachLayoutState> {
        attach_layout(self, session_id).await
    }

    async fn attach_pane_output_batch(
        &mut self,
        session_id: Uuid,
        pane_ids: Vec<Uuid>,
        max_bytes: usize,
    ) -> ClientResult<PaneOutputBatchResult> {
        attach_pane_output_batch(self, session_id, pane_ids, max_bytes).await
    }

    async fn attach_snapshot(
        &mut self,
        session_id: Uuid,
        max_bytes_per_pane: usize,
    ) -> ClientResult<AttachSnapshotState> {
        attach_snapshot(self, session_id, max_bytes_per_pane).await
    }

    async fn attach_pane_snapshot(
        &mut self,
        session_id: Uuid,
        pane_ids: Vec<Uuid>,
        max_bytes_per_pane: usize,
    ) -> ClientResult<AttachPaneSnapshotState> {
        attach_pane_snapshot(self, session_id, pane_ids, max_bytes_per_pane).await
    }

    async fn detach(&mut self) -> ClientResult<()> {
        detach(self).await
    }
}

async fn attach_grant(
    client: &mut impl bmux_plugin_sdk::TypedDispatchClient,
    selector: SessionSelector,
) -> ClientResult<AttachGrant> {
    match attach_commands::client::attach_session(client, session_selector(selector), true).await {
        Ok(Ok(grant)) => Ok(AttachGrant {
            attach_token: grant.token,
            session_id: grant.session_id,
            context_id: grant.context_id,
            expires_at_epoch_ms: grant.expires_epoch_ms,
        }),
        Ok(Err(err)) => typed_server_error("attach-session", err),
        Err(err) => typed_dispatch_error("attach-session", err),
    }
}

async fn open_attach_stream_info(
    client: &mut impl bmux_plugin_sdk::TypedDispatchClient,
    grant: &AttachGrant,
) -> ClientResult<AttachOpenInfo> {
    match attach_commands::client::attach_open(client, grant.session_id, grant.attach_token).await {
        Ok(Ok(ready)) => Ok(AttachOpenInfo {
            context_id: ready.context_id,
            session_id: ready.session_id,
            can_write: ready.can_write,
        }),
        Ok(Err(err)) => typed_server_error("attach-open", err),
        Err(err) => typed_dispatch_error("attach-open", err),
    }
}

async fn attach_set_viewport_with_insets(
    client: &mut impl bmux_plugin_sdk::TypedDispatchClient,
    session_id: Uuid,
    cols: u16,
    rows: u16,
    status_top_inset: u16,
    status_bottom_inset: u16,
) -> ClientResult<(u16, u16)> {
    match attach_commands::client::attach_set_viewport(
        client,
        session_id,
        cols,
        rows,
        status_top_inset,
        status_bottom_inset,
        0,
        0,
    )
    .await
    {
        Ok(Ok(set)) => Ok((set.cols, set.rows)),
        Ok(Err(err)) => typed_server_error("attach-set-viewport", err),
        Err(err) => typed_dispatch_error("attach-set-viewport", err),
    }
}

async fn attach_input(
    client: &mut impl bmux_plugin_sdk::TypedDispatchClient,
    session_id: Uuid,
    data: Vec<u8>,
) -> ClientResult<usize> {
    match attach_commands::client::attach_input(client, session_id, data).await {
        Ok(Ok(accepted)) => Ok(accepted.bytes as usize),
        Ok(Err(err)) => typed_server_error("attach-input", err),
        Err(err) => typed_dispatch_error("attach-input", err),
    }
}

async fn attach_layout(
    client: &mut impl bmux_plugin_sdk::TypedDispatchClient,
    session_id: Uuid,
) -> ClientResult<AttachLayoutState> {
    match attach_state::client::attach_layout_state(client, session_id).await {
        Ok(Ok(layout)) => decode_attach_layout(&layout),
        Ok(Err(err)) => typed_server_error("attach-layout-state", err),
        Err(err) => typed_dispatch_error("attach-layout-state", err),
    }
}

async fn attach_pane_output_batch(
    client: &mut impl bmux_plugin_sdk::TypedDispatchClient,
    session_id: Uuid,
    pane_ids: Vec<Uuid>,
    max_bytes: usize,
) -> ClientResult<PaneOutputBatchResult> {
    let max_bytes_u32 = u32::try_from(max_bytes).unwrap_or(u32::MAX);
    match attach_state::client::attach_pane_output_batch(
        client,
        session_id,
        pane_ids,
        max_bytes_u32,
    )
    .await
    {
        Ok(Ok(batch)) => Ok(PaneOutputBatchResult {
            chunks: batch
                .chunks
                .into_iter()
                .map(pane_chunk_from_record)
                .collect(),
            output_still_pending: batch.output_still_pending,
        }),
        Ok(Err(err)) => typed_server_error("attach-pane-output-batch", err),
        Err(err) => typed_dispatch_error("attach-pane-output-batch", err),
    }
}

async fn attach_snapshot(
    client: &mut impl bmux_plugin_sdk::TypedDispatchClient,
    session_id: Uuid,
    max_bytes_per_pane: usize,
) -> ClientResult<AttachSnapshotState> {
    let max_bytes_u32 = u32::try_from(max_bytes_per_pane).unwrap_or(u32::MAX);
    match attach_state::client::attach_snapshot_state(client, session_id, max_bytes_u32).await {
        Ok(Ok(snap)) => decode_attach_snapshot(snap),
        Ok(Err(err)) => typed_server_error("attach-snapshot-state", err),
        Err(err) => typed_dispatch_error("attach-snapshot-state", err),
    }
}

async fn attach_pane_snapshot(
    client: &mut impl bmux_plugin_sdk::TypedDispatchClient,
    session_id: Uuid,
    pane_ids: Vec<Uuid>,
    max_bytes_per_pane: usize,
) -> ClientResult<AttachPaneSnapshotState> {
    let max_bytes_u32 = u32::try_from(max_bytes_per_pane).unwrap_or(u32::MAX);
    match attach_state::client::attach_pane_snapshot_state(
        client,
        session_id,
        pane_ids,
        max_bytes_u32,
    )
    .await
    {
        Ok(Ok(snap)) => Ok(AttachPaneSnapshotState {
            chunks: snap
                .chunks
                .into_iter()
                .map(pane_chunk_from_record)
                .collect(),
            pane_mouse_protocols: snap
                .pane_mouse_protocols
                .iter()
                .map(pane_mouse_from_record)
                .collect::<std::result::Result<Vec<_>, _>>()?,
            pane_input_modes: snap
                .pane_input_modes
                .iter()
                .map(pane_input_mode_from_record)
                .collect::<std::result::Result<Vec<_>, _>>()?,
        }),
        Ok(Err(err)) => typed_server_error("attach-pane-snapshot-state", err),
        Err(err) => typed_dispatch_error("attach-pane-snapshot-state", err),
    }
}

async fn detach(client: &mut impl bmux_plugin_sdk::TypedDispatchClient) -> ClientResult<()> {
    match attach_commands::client::detach(client).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(err)) => typed_server_error("detach", err),
        Err(err) => typed_dispatch_error("detach", err),
    }
}

fn decode_attach_layout(layout: &attach_state::AttachLayout) -> ClientResult<AttachLayoutState> {
    let payload: LayoutPayload =
        serde_json::from_slice(&layout.encoded).map_err(|e| ClientError::ServerError {
            code: ErrorCode::Internal,
            message: format!("decode attach-layout payload: {e}"),
        })?;
    Ok(AttachLayoutState {
        context_id: layout.context_id,
        session_id: layout.session_id,
        focused_pane_id: layout.focused_pane_id,
        panes: payload.panes,
        layout_root: payload.layout_root,
        scene: payload.scene,
        zoomed: payload.zoomed,
    })
}

fn decode_attach_snapshot(snap: attach_state::AttachSnapshot) -> ClientResult<AttachSnapshotState> {
    let layout: SnapshotLayoutPayload =
        serde_json::from_slice(&snap.layout_encoded).map_err(|e| ClientError::ServerError {
            code: ErrorCode::Internal,
            message: format!("decode attach-snapshot layout payload: {e}"),
        })?;
    Ok(AttachSnapshotState {
        context_id: snap.context_id,
        session_id: snap.session_id,
        focused_pane_id: snap.focused_pane_id,
        panes: layout.panes,
        layout_root: layout.layout_root,
        scene: layout.scene,
        chunks: snap
            .chunks
            .into_iter()
            .map(pane_chunk_from_record)
            .collect(),
        pane_mouse_protocols: snap
            .pane_mouse_protocols
            .iter()
            .map(pane_mouse_from_record)
            .collect::<std::result::Result<Vec<_>, _>>()?,
        pane_input_modes: snap
            .pane_input_modes
            .iter()
            .map(pane_input_mode_from_record)
            .collect::<std::result::Result<Vec<_>, _>>()?,
        zoomed: snap.zoomed,
    })
}

fn pane_chunk_from_record(chunk: attach_state::PaneChunk) -> AttachPaneChunk {
    AttachPaneChunk {
        pane_id: chunk.pane_id,
        data: chunk.data,
        stream_start: chunk.stream_start,
        stream_end: chunk.stream_end,
        stream_gap: chunk.stream_gap,
        sync_update_active: chunk.sync_update_active,
    }
}

fn pane_mouse_from_record(
    mouse: &attach_state::PaneMouseProtocol,
) -> ClientResult<AttachPaneMouseProtocol> {
    let protocol =
        serde_json::from_slice(&mouse.encoded).map_err(|e| ClientError::ServerError {
            code: ErrorCode::Internal,
            message: format!("decode pane mouse-protocol record: {e}"),
        })?;
    Ok(AttachPaneMouseProtocol {
        pane_id: mouse.pane_id,
        protocol,
    })
}

fn pane_input_mode_from_record(
    mode: &attach_state::PaneInputMode,
) -> ClientResult<AttachPaneInputMode> {
    let decoded = serde_json::from_slice(&mode.encoded).map_err(|e| ClientError::ServerError {
        code: ErrorCode::Internal,
        message: format!("decode pane input-mode record: {e}"),
    })?;
    Ok(AttachPaneInputMode {
        pane_id: mode.pane_id,
        mode: decoded,
    })
}

fn session_selector(selector: SessionSelector) -> attach_commands::SessionSelector {
    match selector {
        SessionSelector::ById(id) => attach_commands::SessionSelector {
            id: Some(id),
            name: None,
        },
        SessionSelector::ByName(name) => attach_commands::SessionSelector {
            id: None,
            name: Some(name),
        },
    }
}

fn typed_server_error<T>(operation: &str, err: impl std::fmt::Debug) -> ClientResult<T> {
    Err(ClientError::ServerError {
        code: ErrorCode::Internal,
        message: format!("{operation} failed: {err:?}"),
    })
}

fn typed_dispatch_error<T>(operation: &str, err: impl std::fmt::Display) -> ClientResult<T> {
    Err(ClientError::ServerError {
        code: ErrorCode::Internal,
        message: format!("{operation} typed dispatch failed: {err}"),
    })
}
