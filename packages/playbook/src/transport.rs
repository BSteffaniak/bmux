//! Execution-scoped pane transport adapter. General interactive clients remain application-owned.
use bmux_attach_token_state::AttachGrant;
use bmux_client::{
    AttachLayoutState, AttachOpenInfo, AttachSnapshotState, BmuxClient, PaneOutputBatchResult,
};
use bmux_context_state::ContextSelector;
use bmux_ipc::InvokeServiceKind;
use bmux_plugin_sdk::{TypedDispatchClient, TypedDispatchClientResult};
use bmux_session_models::SessionSelector;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use uuid::Uuid;

pub type ClientFuture<'a, T> = Pin<Box<dyn Future<Output = bmux_client::Result<T>> + Send + 'a>>;
#[derive(Debug, Clone)]
pub struct PaneGridSnapshotResult {
    pub pane_id: Uuid,
    pub stream_end: u64,
    pub encoded: Vec<u8>,
}

pub trait PaneTransport: std::fmt::Debug + Send + Sync {
    fn attach_grant<'a>(
        &'a self,
        client: &'a mut BmuxClient,
        selector: SessionSelector,
    ) -> ClientFuture<'a, AttachGrant>;
    fn attach_context_grant<'a>(
        &'a self,
        client: &'a mut BmuxClient,
        selector: ContextSelector,
    ) -> ClientFuture<'a, AttachGrant>;
    fn retarget_attach_context<'a>(
        &'a self,
        client: &'a mut BmuxClient,
        context_id: Uuid,
        cols: u16,
        rows: u16,
    ) -> ClientFuture<'a, AttachOpenInfo>;
    fn open_attach_stream_info<'a>(
        &'a self,
        client: &'a mut BmuxClient,
        grant: &'a AttachGrant,
    ) -> ClientFuture<'a, AttachOpenInfo>;
    fn detach<'a>(&'a self, client: &'a mut BmuxClient) -> ClientFuture<'a, ()>;
    fn attach_input<'a>(
        &'a self,
        client: &'a mut BmuxClient,
        session_id: Uuid,
        data: Vec<u8>,
    ) -> ClientFuture<'a, usize>;
    fn pane_direct_input<'a>(
        &'a self,
        client: &'a mut BmuxClient,
        session_id: Uuid,
        pane_id: Uuid,
        data: Vec<u8>,
    ) -> ClientFuture<'a, usize>;
    fn attach_set_viewport<'a>(
        &'a self,
        client: &'a mut BmuxClient,
        session_id: Uuid,
        cols: u16,
        rows: u16,
    ) -> ClientFuture<'a, (u16, u16)>;
    fn attach_layout<'a>(
        &'a self,
        client: &'a mut BmuxClient,
        session_id: Uuid,
    ) -> ClientFuture<'a, AttachLayoutState>;
    fn attach_pane_output_batch<'a>(
        &'a self,
        client: &'a mut BmuxClient,
        session_id: Uuid,
        pane_ids: Vec<Uuid>,
        max_bytes: usize,
    ) -> ClientFuture<'a, PaneOutputBatchResult>;
    fn attach_snapshot<'a>(
        &'a self,
        client: &'a mut BmuxClient,
        session_id: Uuid,
        max_bytes_per_pane: usize,
    ) -> ClientFuture<'a, AttachSnapshotState>;
    fn grid_snapshot<'a>(
        &'a self,
        client: &'a mut BmuxClient,
        session: Uuid,
        panes: Vec<Uuid>,
        rows: usize,
    ) -> ClientFuture<'a, Vec<PaneGridSnapshotResult>>;
}

/// A transport handle with caller-supplied execution workflows; not a second kernel client.
pub struct ExecutionClient {
    inner: BmuxClient,
    transport: Arc<dyn PaneTransport>,
    host: Arc<dyn crate::host::HostAdapter>,
}
impl ExecutionClient {
    pub fn new(inner: BmuxClient, host: Arc<dyn crate::host::HostAdapter>) -> Self {
        Self {
            inner,
            transport: host.pane_transport(),
            host,
        }
    }
    pub fn host(&self) -> &Arc<dyn crate::host::HostAdapter> {
        &self.host
    }
    pub fn into_inner(self) -> BmuxClient {
        self.inner
    }
    pub fn inner_mut(&mut self) -> &mut BmuxClient {
        &mut self.inner
    }
    pub async fn attach_grant(
        &mut self,
        selector: SessionSelector,
    ) -> bmux_client::Result<AttachGrant> {
        self.transport.attach_grant(&mut self.inner, selector).await
    }
    pub async fn attach_context_grant(
        &mut self,
        selector: ContextSelector,
    ) -> bmux_client::Result<AttachGrant> {
        self.transport
            .attach_context_grant(&mut self.inner, selector)
            .await
    }
    pub async fn retarget_attach_context(
        &mut self,
        context_id: Uuid,
        cols: u16,
        rows: u16,
    ) -> bmux_client::Result<AttachOpenInfo> {
        self.transport
            .retarget_attach_context(&mut self.inner, context_id, cols, rows)
            .await
    }
    pub async fn open_attach_stream_info(
        &mut self,
        grant: &AttachGrant,
    ) -> bmux_client::Result<AttachOpenInfo> {
        self.transport
            .open_attach_stream_info(&mut self.inner, grant)
            .await
    }
    pub async fn detach(&mut self) -> bmux_client::Result<()> {
        self.transport.detach(&mut self.inner).await
    }
    pub async fn attach_input(
        &mut self,
        session_id: Uuid,
        data: Vec<u8>,
    ) -> bmux_client::Result<usize> {
        self.transport
            .attach_input(&mut self.inner, session_id, data)
            .await
    }
    pub async fn pane_direct_input(
        &mut self,
        session_id: Uuid,
        pane_id: Uuid,
        data: Vec<u8>,
    ) -> bmux_client::Result<usize> {
        self.transport
            .pane_direct_input(&mut self.inner, session_id, pane_id, data)
            .await
    }
    pub async fn attach_set_viewport(
        &mut self,
        session_id: Uuid,
        cols: u16,
        rows: u16,
    ) -> bmux_client::Result<(u16, u16)> {
        self.transport
            .attach_set_viewport(&mut self.inner, session_id, cols, rows)
            .await
    }
    pub async fn attach_layout(
        &mut self,
        session_id: Uuid,
    ) -> bmux_client::Result<AttachLayoutState> {
        self.transport
            .attach_layout(&mut self.inner, session_id)
            .await
    }
    pub async fn attach_pane_output_batch(
        &mut self,
        session_id: Uuid,
        pane_ids: Vec<Uuid>,
        max_bytes: usize,
    ) -> bmux_client::Result<PaneOutputBatchResult> {
        self.transport
            .attach_pane_output_batch(&mut self.inner, session_id, pane_ids, max_bytes)
            .await
    }
    pub async fn attach_snapshot(
        &mut self,
        session_id: Uuid,
        max_bytes_per_pane: usize,
    ) -> bmux_client::Result<AttachSnapshotState> {
        self.transport
            .attach_snapshot(&mut self.inner, session_id, max_bytes_per_pane)
            .await
    }
}
impl std::ops::Deref for ExecutionClient {
    type Target = BmuxClient;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
impl std::ops::DerefMut for ExecutionClient {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}
impl TypedDispatchClient for ExecutionClient {
    fn invoke_service_raw(
        &mut self,
        capability: &str,
        kind: InvokeServiceKind,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> impl Future<Output = TypedDispatchClientResult<Vec<u8>>> + Send {
        // Select the typed implementation explicitly: the inherent method returns
        // ClientError, while this trait preserves typed-dispatch error mapping.
        TypedDispatchClient::invoke_service_raw(
            &mut self.inner,
            capability,
            kind,
            interface_id,
            operation,
            payload,
        )
    }
}
pub async fn attach_pane_grid_snapshot_state(
    client: &mut ExecutionClient,
    session: Uuid,
    panes: Vec<Uuid>,
    rows: usize,
) -> bmux_client::Result<Vec<PaneGridSnapshotResult>> {
    client
        .transport
        .grid_snapshot(&mut client.inner, session, panes, rows)
        .await
}
