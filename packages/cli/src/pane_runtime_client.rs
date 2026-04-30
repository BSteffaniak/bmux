use bmux_attach_image_protocol::AttachPaneImageDelta;
use bmux_attach_layout_protocol::{
    AttachPaneChunk, AttachPaneInputMode, AttachPaneMouseProtocol, AttachScene, PaneLayoutNode,
    PaneSummary,
};
use bmux_attach_token_state::AttachGrant;
use bmux_client::{
    AttachLayoutState, AttachOpenInfo, AttachPaneSnapshotState, AttachSnapshotState, ClientError,
    PaneOutputBatchResult,
};
use bmux_context_state::ContextSelector;
use bmux_ipc::ErrorCode;
use bmux_pane_runtime_plugin_api::{
    attach_runtime_commands as AttachCommands, attach_runtime_state as AttachState,
    pane_runtime_commands as PaneCommands,
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

pub trait BmuxPaneRuntimeClientExt {
    fn attach_grant(
        &mut self,
        selector: SessionSelector,
    ) -> impl Future<Output = ClientResult<AttachGrant>> + Send;

    fn attach_context_grant(
        &mut self,
        selector: ContextSelector,
    ) -> impl Future<Output = ClientResult<AttachGrant>> + Send;

    fn retarget_attach_context(
        &mut self,
        context_id: Uuid,
        cols: u16,
        rows: u16,
    ) -> impl Future<Output = ClientResult<AttachOpenInfo>> + Send;

    fn retarget_attach_context_with_insets(
        &mut self,
        context_id: Uuid,
        cols: u16,
        rows: u16,
        status_top_inset: u16,
        status_bottom_inset: u16,
    ) -> impl Future<Output = ClientResult<AttachOpenInfo>> + Send;

    fn open_attach_stream_info(
        &mut self,
        grant: &AttachGrant,
    ) -> impl Future<Output = ClientResult<AttachOpenInfo>> + Send;

    fn detach(&mut self) -> impl Future<Output = ClientResult<()>> + Send;

    fn set_attach_policy(
        &mut self,
        allow_detach: bool,
    ) -> impl Future<Output = ClientResult<()>> + Send;

    fn attach_input(
        &mut self,
        session_id: Uuid,
        data: Vec<u8>,
    ) -> impl Future<Output = ClientResult<usize>> + Send;

    fn pane_direct_input(
        &mut self,
        session_id: Uuid,
        pane_id: Uuid,
        data: Vec<u8>,
    ) -> impl Future<Output = ClientResult<usize>> + Send;

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

    fn attach_output(
        &mut self,
        session_id: Uuid,
        max_bytes: usize,
    ) -> impl Future<Output = ClientResult<Vec<u8>>> + Send;

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

    // Used by the attach runtime binary path; `cargo check -p bmux_cli` checks the
    // library target separately and reports this trait item as otherwise unused.
    #[allow(dead_code)]
    fn attach_pane_images(
        &mut self,
        session_id: Uuid,
        pane_ids: Vec<Uuid>,
        since_sequences: Vec<u64>,
    ) -> impl Future<Output = ClientResult<Vec<AttachPaneImageDelta>>> + Send;

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
}

impl BmuxPaneRuntimeClientExt for bmux_client::BmuxClient {
    async fn attach_grant(&mut self, selector: SessionSelector) -> ClientResult<AttachGrant> {
        match AttachCommands::client::attach_session(
            self,
            pane_runtime_session_selector(selector),
            true,
        )
        .await
        {
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

    async fn attach_context_grant(
        &mut self,
        selector: ContextSelector,
    ) -> ClientResult<AttachGrant> {
        match AttachCommands::client::attach_context(
            self,
            pane_runtime_context_selector(selector),
            true,
        )
        .await
        {
            Ok(Ok(grant)) => Ok(AttachGrant {
                attach_token: grant.token,
                session_id: grant.session_id,
                context_id: grant.context_id,
                expires_at_epoch_ms: grant.expires_epoch_ms,
            }),
            Ok(Err(err)) => typed_server_error("attach-context", err),
            Err(err) => typed_dispatch_error("attach-context", err),
        }
    }

    async fn retarget_attach_context(
        &mut self,
        context_id: Uuid,
        cols: u16,
        rows: u16,
    ) -> ClientResult<AttachOpenInfo> {
        self.retarget_attach_context_with_insets(context_id, cols, rows, 0, 0)
            .await
    }

    async fn retarget_attach_context_with_insets(
        &mut self,
        context_id: Uuid,
        cols: u16,
        rows: u16,
        status_top_inset: u16,
        status_bottom_inset: u16,
    ) -> ClientResult<AttachOpenInfo> {
        match AttachCommands::client::attach_retarget_context(
            self,
            context_id,
            true,
            cols,
            rows,
            status_top_inset,
            status_bottom_inset,
            cell_pixel_width(),
            cell_pixel_height(),
        )
        .await
        {
            Ok(Ok(ready)) => Ok(AttachOpenInfo {
                context_id: ready.context_id,
                session_id: ready.session_id,
                can_write: ready.can_write,
            }),
            Ok(Err(err)) => typed_server_error("attach-retarget-context", err),
            Err(err) => typed_dispatch_error("attach-retarget-context", err),
        }
    }

    async fn open_attach_stream_info(
        &mut self,
        grant: &AttachGrant,
    ) -> ClientResult<AttachOpenInfo> {
        match AttachCommands::client::attach_open(self, grant.session_id, grant.attach_token).await
        {
            Ok(Ok(ready)) => Ok(AttachOpenInfo {
                context_id: ready.context_id,
                session_id: ready.session_id,
                can_write: ready.can_write,
            }),
            Ok(Err(err)) => typed_server_error("attach-open", err),
            Err(err) => typed_dispatch_error("attach-open", err),
        }
    }

    async fn detach(&mut self) -> ClientResult<()> {
        match AttachCommands::client::detach(self).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(err)) => typed_server_error("detach", err),
            Err(err) => typed_dispatch_error("detach", err),
        }
    }

    async fn set_attach_policy(&mut self, allow_detach: bool) -> ClientResult<()> {
        match AttachCommands::client::set_client_attach_policy(self, allow_detach).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(err)) => typed_server_error("set-client-attach-policy", err),
            Err(err) => typed_dispatch_error("set-client-attach-policy", err),
        }
    }

    async fn attach_input(&mut self, session_id: Uuid, data: Vec<u8>) -> ClientResult<usize> {
        match AttachCommands::client::attach_input(self, session_id, data).await {
            Ok(Ok(accepted)) => Ok(accepted.bytes as usize),
            Ok(Err(err)) => typed_server_error("attach-input", err),
            Err(err) => typed_dispatch_error("attach-input", err),
        }
    }

    async fn pane_direct_input(
        &mut self,
        session_id: Uuid,
        pane_id: Uuid,
        data: Vec<u8>,
    ) -> ClientResult<usize> {
        let bytes_len = data.len();
        match PaneCommands::client::pane_direct_input(self, session_id, pane_id, data).await {
            Ok(Ok(_ack)) => Ok(bytes_len),
            Ok(Err(err)) => typed_server_error("pane-direct-input", err),
            Err(err) => typed_dispatch_error("pane-direct-input", err),
        }
    }

    async fn attach_set_viewport(
        &mut self,
        session_id: Uuid,
        cols: u16,
        rows: u16,
    ) -> ClientResult<(u16, u16)> {
        self.attach_set_viewport_with_insets(session_id, cols, rows, 0, 0)
            .await
    }

    async fn attach_set_viewport_with_insets(
        &mut self,
        session_id: Uuid,
        cols: u16,
        rows: u16,
        status_top_inset: u16,
        status_bottom_inset: u16,
    ) -> ClientResult<(u16, u16)> {
        match AttachCommands::client::attach_set_viewport(
            self,
            session_id,
            cols,
            rows,
            status_top_inset,
            status_bottom_inset,
            cell_pixel_width(),
            cell_pixel_height(),
        )
        .await
        {
            Ok(Ok(set)) => Ok((set.cols, set.rows)),
            Ok(Err(err)) => typed_server_error("attach-set-viewport", err),
            Err(err) => typed_dispatch_error("attach-set-viewport", err),
        }
    }

    async fn attach_output(&mut self, session_id: Uuid, max_bytes: usize) -> ClientResult<Vec<u8>> {
        let max_bytes_u32 = u32::try_from(max_bytes).unwrap_or(u32::MAX);
        match AttachCommands::client::attach_output(self, session_id, max_bytes_u32).await {
            Ok(Ok(out)) => Ok(out.data),
            Ok(Err(err)) => typed_server_error("attach-output", err),
            Err(err) => typed_dispatch_error("attach-output", err),
        }
    }

    async fn attach_layout(&mut self, session_id: Uuid) -> ClientResult<AttachLayoutState> {
        match AttachState::client::attach_layout_state(self, session_id).await {
            Ok(Ok(layout)) => decode_attach_layout(&layout),
            Ok(Err(err)) => typed_server_error("attach-layout-state", err),
            Err(err) => typed_dispatch_error("attach-layout-state", err),
        }
    }

    async fn attach_pane_output_batch(
        &mut self,
        session_id: Uuid,
        pane_ids: Vec<Uuid>,
        max_bytes: usize,
    ) -> ClientResult<PaneOutputBatchResult> {
        let max_bytes_u32 = u32::try_from(max_bytes).unwrap_or(u32::MAX);
        match AttachState::client::attach_pane_output_batch(
            self,
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

    async fn attach_pane_images(
        &mut self,
        session_id: Uuid,
        pane_ids: Vec<Uuid>,
        since_sequences: Vec<u64>,
    ) -> ClientResult<Vec<AttachPaneImageDelta>> {
        match AttachState::client::attach_pane_images(self, session_id, pane_ids, since_sequences)
            .await
        {
            Ok(Ok(images)) => serde_json::from_slice::<Vec<AttachPaneImageDelta>>(&images.encoded)
                .map_err(|e| ClientError::ServerError {
                    code: ErrorCode::Internal,
                    message: format!("decode pane-images deltas: {e}"),
                }),
            Ok(Err(err)) => typed_server_error("attach-pane-images", err),
            Err(err) => typed_dispatch_error("attach-pane-images", err),
        }
    }

    async fn attach_snapshot(
        &mut self,
        session_id: Uuid,
        max_bytes_per_pane: usize,
    ) -> ClientResult<AttachSnapshotState> {
        let max_bytes_u32 = u32::try_from(max_bytes_per_pane).unwrap_or(u32::MAX);
        match AttachState::client::attach_snapshot_state(self, session_id, max_bytes_u32).await {
            Ok(Ok(snap)) => decode_attach_snapshot(snap),
            Ok(Err(err)) => typed_server_error("attach-snapshot-state", err),
            Err(err) => typed_dispatch_error("attach-snapshot-state", err),
        }
    }

    async fn attach_pane_snapshot(
        &mut self,
        session_id: Uuid,
        pane_ids: Vec<Uuid>,
        max_bytes_per_pane: usize,
    ) -> ClientResult<AttachPaneSnapshotState> {
        let max_bytes_u32 = u32::try_from(max_bytes_per_pane).unwrap_or(u32::MAX);
        match AttachState::client::attach_pane_snapshot_state(
            self,
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
}

pub trait StreamingAttachInputExt {
    fn send_one_way_attach_input(
        &mut self,
        session_id: Uuid,
        data: Vec<u8>,
    ) -> impl Future<Output = ClientResult<()>> + Send;
}

impl StreamingAttachInputExt for bmux_client::StreamingBmuxClient {
    async fn send_one_way_attach_input(
        &mut self,
        session_id: Uuid,
        data: Vec<u8>,
    ) -> ClientResult<()> {
        #[derive(serde::Serialize)]
        struct AttachInputArgs {
            session_id: Uuid,
            data: Vec<u8>,
        }

        let typed_payload =
            bmux_ipc::encode(&AttachInputArgs { session_id, data }).map_err(ClientError::from)?;
        self.send_one_way(bmux_ipc::Request::InvokeService {
            capability: bmux_pane_runtime_plugin_api::capabilities::ATTACH_RUNTIME_WRITE
                .as_str()
                .to_string(),
            kind: bmux_ipc::InvokeServiceKind::Command,
            interface_id: AttachCommands::INTERFACE_ID.as_str().to_string(),
            operation: "attach-input".to_string(),
            payload: typed_payload,
        })
        .await
    }
}

impl BmuxPaneRuntimeClientExt for bmux_client::StreamingBmuxClient {
    async fn attach_grant(&mut self, selector: SessionSelector) -> ClientResult<AttachGrant> {
        match AttachCommands::client::attach_session(
            self,
            pane_runtime_session_selector(selector),
            true,
        )
        .await
        {
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

    async fn attach_context_grant(
        &mut self,
        selector: ContextSelector,
    ) -> ClientResult<AttachGrant> {
        match AttachCommands::client::attach_context(
            self,
            pane_runtime_context_selector(selector),
            true,
        )
        .await
        {
            Ok(Ok(grant)) => Ok(AttachGrant {
                attach_token: grant.token,
                session_id: grant.session_id,
                context_id: grant.context_id,
                expires_at_epoch_ms: grant.expires_epoch_ms,
            }),
            Ok(Err(err)) => typed_server_error("attach-context", err),
            Err(err) => typed_dispatch_error("attach-context", err),
        }
    }

    async fn retarget_attach_context(
        &mut self,
        context_id: Uuid,
        cols: u16,
        rows: u16,
    ) -> ClientResult<AttachOpenInfo> {
        self.retarget_attach_context_with_insets(context_id, cols, rows, 0, 0)
            .await
    }

    async fn retarget_attach_context_with_insets(
        &mut self,
        context_id: Uuid,
        cols: u16,
        rows: u16,
        status_top_inset: u16,
        status_bottom_inset: u16,
    ) -> ClientResult<AttachOpenInfo> {
        match AttachCommands::client::attach_retarget_context(
            self,
            context_id,
            true,
            cols,
            rows,
            status_top_inset,
            status_bottom_inset,
            cell_pixel_width(),
            cell_pixel_height(),
        )
        .await
        {
            Ok(Ok(ready)) => Ok(AttachOpenInfo {
                context_id: ready.context_id,
                session_id: ready.session_id,
                can_write: ready.can_write,
            }),
            Ok(Err(err)) => typed_server_error("attach-retarget-context", err),
            Err(err) => typed_dispatch_error("attach-retarget-context", err),
        }
    }

    async fn open_attach_stream_info(
        &mut self,
        grant: &AttachGrant,
    ) -> ClientResult<AttachOpenInfo> {
        match AttachCommands::client::attach_open(self, grant.session_id, grant.attach_token).await
        {
            Ok(Ok(ready)) => Ok(AttachOpenInfo {
                context_id: ready.context_id,
                session_id: ready.session_id,
                can_write: ready.can_write,
            }),
            Ok(Err(err)) => typed_server_error("attach-open", err),
            Err(err) => typed_dispatch_error("attach-open", err),
        }
    }

    async fn detach(&mut self) -> ClientResult<()> {
        match AttachCommands::client::detach(self).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(err)) => typed_server_error("detach", err),
            Err(err) => typed_dispatch_error("detach", err),
        }
    }

    async fn set_attach_policy(&mut self, allow_detach: bool) -> ClientResult<()> {
        match AttachCommands::client::set_client_attach_policy(self, allow_detach).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(err)) => typed_server_error("set-client-attach-policy", err),
            Err(err) => typed_dispatch_error("set-client-attach-policy", err),
        }
    }

    async fn attach_input(&mut self, session_id: Uuid, data: Vec<u8>) -> ClientResult<usize> {
        match AttachCommands::client::attach_input(self, session_id, data).await {
            Ok(Ok(accepted)) => Ok(accepted.bytes as usize),
            Ok(Err(err)) => typed_server_error("attach-input", err),
            Err(err) => typed_dispatch_error("attach-input", err),
        }
    }

    async fn pane_direct_input(
        &mut self,
        session_id: Uuid,
        pane_id: Uuid,
        data: Vec<u8>,
    ) -> ClientResult<usize> {
        let bytes_len = data.len();
        match PaneCommands::client::pane_direct_input(self, session_id, pane_id, data).await {
            Ok(Ok(_ack)) => Ok(bytes_len),
            Ok(Err(err)) => typed_server_error("pane-direct-input", err),
            Err(err) => typed_dispatch_error("pane-direct-input", err),
        }
    }

    async fn attach_set_viewport(
        &mut self,
        session_id: Uuid,
        cols: u16,
        rows: u16,
    ) -> ClientResult<(u16, u16)> {
        self.attach_set_viewport_with_insets(session_id, cols, rows, 0, 0)
            .await
    }

    async fn attach_set_viewport_with_insets(
        &mut self,
        session_id: Uuid,
        cols: u16,
        rows: u16,
        status_top_inset: u16,
        status_bottom_inset: u16,
    ) -> ClientResult<(u16, u16)> {
        match AttachCommands::client::attach_set_viewport(
            self,
            session_id,
            cols,
            rows,
            status_top_inset,
            status_bottom_inset,
            cell_pixel_width(),
            cell_pixel_height(),
        )
        .await
        {
            Ok(Ok(set)) => Ok((set.cols, set.rows)),
            Ok(Err(err)) => typed_server_error("attach-set-viewport", err),
            Err(err) => typed_dispatch_error("attach-set-viewport", err),
        }
    }

    async fn attach_output(&mut self, session_id: Uuid, max_bytes: usize) -> ClientResult<Vec<u8>> {
        let max_bytes_u32 = u32::try_from(max_bytes).unwrap_or(u32::MAX);
        match AttachCommands::client::attach_output(self, session_id, max_bytes_u32).await {
            Ok(Ok(out)) => Ok(out.data),
            Ok(Err(err)) => typed_server_error("attach-output", err),
            Err(err) => typed_dispatch_error("attach-output", err),
        }
    }

    async fn attach_layout(&mut self, session_id: Uuid) -> ClientResult<AttachLayoutState> {
        match AttachState::client::attach_layout_state(self, session_id).await {
            Ok(Ok(layout)) => decode_attach_layout(&layout),
            Ok(Err(err)) => typed_server_error("attach-layout-state", err),
            Err(err) => typed_dispatch_error("attach-layout-state", err),
        }
    }

    async fn attach_pane_output_batch(
        &mut self,
        session_id: Uuid,
        pane_ids: Vec<Uuid>,
        max_bytes: usize,
    ) -> ClientResult<PaneOutputBatchResult> {
        let max_bytes_u32 = u32::try_from(max_bytes).unwrap_or(u32::MAX);
        match AttachState::client::attach_pane_output_batch(
            self,
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

    async fn attach_pane_images(
        &mut self,
        session_id: Uuid,
        pane_ids: Vec<Uuid>,
        since_sequences: Vec<u64>,
    ) -> ClientResult<Vec<AttachPaneImageDelta>> {
        match AttachState::client::attach_pane_images(self, session_id, pane_ids, since_sequences)
            .await
        {
            Ok(Ok(images)) => serde_json::from_slice::<Vec<AttachPaneImageDelta>>(&images.encoded)
                .map_err(|e| ClientError::ServerError {
                    code: ErrorCode::Internal,
                    message: format!("decode pane-images deltas: {e}"),
                }),
            Ok(Err(err)) => typed_server_error("attach-pane-images", err),
            Err(err) => typed_dispatch_error("attach-pane-images", err),
        }
    }

    async fn attach_snapshot(
        &mut self,
        session_id: Uuid,
        max_bytes_per_pane: usize,
    ) -> ClientResult<AttachSnapshotState> {
        let max_bytes_u32 = u32::try_from(max_bytes_per_pane).unwrap_or(u32::MAX);
        match AttachState::client::attach_snapshot_state(self, session_id, max_bytes_u32).await {
            Ok(Ok(snap)) => decode_attach_snapshot(snap),
            Ok(Err(err)) => typed_server_error("attach-snapshot-state", err),
            Err(err) => typed_dispatch_error("attach-snapshot-state", err),
        }
    }

    async fn attach_pane_snapshot(
        &mut self,
        session_id: Uuid,
        pane_ids: Vec<Uuid>,
        max_bytes_per_pane: usize,
    ) -> ClientResult<AttachPaneSnapshotState> {
        let max_bytes_u32 = u32::try_from(max_bytes_per_pane).unwrap_or(u32::MAX);
        match AttachState::client::attach_pane_snapshot_state(
            self,
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
}

fn decode_attach_layout(layout: &AttachState::AttachLayout) -> ClientResult<AttachLayoutState> {
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

fn decode_attach_snapshot(snap: AttachState::AttachSnapshot) -> ClientResult<AttachSnapshotState> {
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

fn pane_chunk_from_record(chunk: AttachState::PaneChunk) -> AttachPaneChunk {
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
    mouse: &AttachState::PaneMouseProtocol,
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
    mode: &AttachState::PaneInputMode,
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

fn pane_runtime_session_selector(selector: SessionSelector) -> AttachCommands::SessionSelector {
    match selector {
        SessionSelector::ById(id) => AttachCommands::SessionSelector {
            id: Some(id),
            name: None,
        },
        SessionSelector::ByName(name) => AttachCommands::SessionSelector {
            id: None,
            name: Some(name),
        },
    }
}

fn pane_runtime_context_selector(selector: ContextSelector) -> AttachCommands::ContextSelector {
    match selector {
        ContextSelector::ById(id) => AttachCommands::ContextSelector {
            id: Some(id),
            name: None,
        },
        ContextSelector::ByName(name) => AttachCommands::ContextSelector {
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

#[cfg(unix)]
fn cell_pixel_width() -> u16 {
    let (w, _) = cell_pixel_size_from_ioctl();
    w
}

#[cfg(unix)]
fn cell_pixel_height() -> u16 {
    let (_, h) = cell_pixel_size_from_ioctl();
    h
}

#[cfg(unix)]
fn cell_pixel_size_from_ioctl() -> (u16, u16) {
    use std::os::unix::io::AsRawFd;

    #[repr(C)]
    #[allow(clippy::struct_field_names)]
    struct Winsize {
        ws_row: u16,
        ws_col: u16,
        ws_xpixel: u16,
        ws_ypixel: u16,
    }

    #[cfg(target_os = "macos")]
    const TIOCGWINSZ: u64 = 0x4008_7468;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const TIOCGWINSZ: u64 = 0x5413;

    let fd = std::io::stdout().as_raw_fd();
    let mut ws = std::mem::MaybeUninit::<Winsize>::uninit();
    let ret = unsafe {
        unsafe extern "C" {
            fn ioctl(fd: i32, request: u64, ...) -> i32;
        }
        ioctl(fd, TIOCGWINSZ, ws.as_mut_ptr())
    };
    if ret != 0 {
        return (0, 0);
    }
    let ws = unsafe { ws.assume_init() };
    if ws.ws_col == 0 || ws.ws_row == 0 || ws.ws_xpixel == 0 || ws.ws_ypixel == 0 {
        return (0, 0);
    }
    (ws.ws_xpixel / ws.ws_col, ws.ws_ypixel / ws.ws_row)
}

#[cfg(windows)]
fn cell_pixel_width() -> u16 {
    let (w, _) = cell_pixel_size_from_console();
    w
}

#[cfg(windows)]
fn cell_pixel_height() -> u16 {
    let (_, h) = cell_pixel_size_from_console();
    h
}

#[cfg(windows)]
fn cell_pixel_size_from_console() -> (u16, u16) {
    #[repr(C)]
    #[allow(non_snake_case)]
    struct Coord {
        X: i16,
        Y: i16,
    }

    #[repr(C)]
    #[allow(non_snake_case)]
    struct ConsoleFontInfoEx {
        cbSize: u32,
        nFont: u32,
        dwFontSize: Coord,
        FontFamily: u32,
        FontWeight: u32,
        FaceName: [u16; 32],
    }

    unsafe {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Console::{
            GetCurrentConsoleFontEx, GetStdHandle, STD_OUTPUT_HANDLE,
        };

        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            return (0, 0);
        }
        let mut info = ConsoleFontInfoEx {
            cbSize: std::mem::size_of::<ConsoleFontInfoEx>() as u32,
            nFont: 0,
            dwFontSize: Coord { X: 0, Y: 0 },
            FontFamily: 0,
            FontWeight: 0,
            FaceName: [0; 32],
        };
        if GetCurrentConsoleFontEx(handle, 0, std::ptr::addr_of_mut!(info).cast()) == 0 {
            return (0, 0);
        }
        (
            u16::try_from(info.dwFontSize.X).unwrap_or(0),
            u16::try_from(info.dwFontSize.Y).unwrap_or(0),
        )
    }
}

#[cfg(not(any(unix, windows)))]
const fn cell_pixel_width() -> u16 {
    0
}

#[cfg(not(any(unix, windows)))]
const fn cell_pixel_height() -> u16 {
    0
}
