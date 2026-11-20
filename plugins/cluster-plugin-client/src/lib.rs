//! Plugin-owned client adapter for native federated attach.
//!
//! A `cluster://` attach shows a workspace's logical layout, read from the ingress node, with the
//! screens of its panes' executions, which run on worker nodes. The ingress names each
//! execution's worker and signs a lease to observe it (`cluster-attach-state` `routes`); the
//! session holds one connection per worker and has the worker push frames of the executions it
//! runs there (`cluster-worker-stream` `observe`). Each stream's screen is a replica grid kept
//! only by those frames, and frames written from it bring the renderer's replica level with it:
//! frames relay through the session, which neither polls workers nor parses terminal output.
//! Replicas follow their source's size: a viewport change resizes the workers' panes, whose
//! frames then carry the new size.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

use bmux_client::{
    AttachDeltaSequence, AttachDetachOutcome, AttachEndpointConnector, AttachProvider,
    AttachProviderAck, AttachProviderAction, AttachProviderBackend, AttachProviderError,
    AttachProviderFuture, AttachProviderInput, AttachProviderSession, AttachProviderSnapshot,
    AttachProviderViewport, AttachResumeState, AttachSession, AttachSessionError,
    AttachSessionFuture, AttachStreamCursor, AttachStreamId, AttachStreamSnapshot, AttachTarget,
    AttachViewRevision, BmuxClient, ResolvedAttachTarget,
};
use bmux_cluster_plugin_api::cluster_types::{
    ExecutionRef, ObservedExecution, WorkerAuthority, WorkerRoute, WorkerServiceError,
};
use bmux_cluster_plugin_api::{
    cluster_attach_command, cluster_attach_state, cluster_control_state, cluster_worker_stream,
};
use bmux_pane_runtime_plugin_api::attach_runtime_state::{AttachPaneFrames, PaneFrame};
use std::any::Any;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

const CLUSTER_PROVIDER_ID: &str = "bmux.cluster";

/// How long a new stream's worker has to push its first frame (it pushes one at once).
const HYDRATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// How often the session asks the ingress whether the layout changed (see
/// [`ClusterAttachSession::changed_layout`]).
const LAYOUT_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// How long after a worker could not be observed (unreachable, or not yet running the execution)
/// the session tries again, unless the layout changes first.
const OBSERVE_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

#[derive(Debug)]
pub struct ClusterAttachProvider;

#[derive(Debug)]
struct ClusterAttachTarget {
    cluster: String,
    workspace: String,
}

impl ResolvedAttachTarget for ClusterAttachTarget {
    fn provider_id(&self) -> &str {
        CLUSTER_PROVIDER_ID
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl AttachProvider for ClusterAttachProvider {
    fn id(&self) -> &str {
        CLUSTER_PROVIDER_ID
    }

    fn priority(&self) -> i32 {
        100
    }

    fn supports(&self, target: &AttachTarget) -> bool {
        target.scheme() == Some("cluster")
    }

    fn requires_fallback_client(&self) -> bool {
        false
    }

    fn resolve(
        &self,
        target: &AttachTarget,
    ) -> Result<Arc<dyn ResolvedAttachTarget>, AttachProviderError> {
        let (cluster, workspace) = target.reference().split_once('/').ok_or_else(|| {
            AttachProviderError::InvalidTarget {
                provider_id: CLUSTER_PROVIDER_ID.to_string(),
                target: target.raw().to_string(),
                reason: "expected cluster://<cluster>/<workspace>".to_string(),
            }
        })?;
        if cluster.trim().is_empty() || workspace.trim().is_empty() || workspace.contains('/') {
            return Err(AttachProviderError::InvalidTarget {
                provider_id: CLUSTER_PROVIDER_ID.to_string(),
                target: target.raw().to_string(),
                reason: "cluster and workspace must be non-empty single path segments".to_string(),
            });
        }
        Ok(Arc::new(ClusterAttachTarget {
            cluster: cluster.to_string(),
            workspace: workspace.to_string(),
        }))
    }

    fn open(
        &self,
        resolved: Arc<dyn ResolvedAttachTarget>,
        resume: Option<AttachResumeState>,
        context: bmux_client::AttachProviderOpenContext,
    ) -> AttachProviderFuture<'_, AttachProviderSession> {
        Box::pin(async move {
            let target = resolved
                .as_any()
                .downcast_ref::<ClusterAttachTarget>()
                .ok_or_else(|| AttachProviderError::InvalidTarget {
                    provider_id: CLUSTER_PROVIDER_ID.to_string(),
                    target: String::new(),
                    reason: format!(
                        "resolved plan belongs to provider '{}'",
                        resolved.provider_id()
                    ),
                })?;
            // Workers are dialed directly, so the connector is needed even with a client in hand.
            let connector =
                context
                    .endpoint_connector
                    .ok_or_else(|| AttachProviderError::OpenFailed {
                        provider_id: CLUSTER_PROVIDER_ID.to_string(),
                        reason: "cluster provider requires the generic endpoint connector"
                            .to_string(),
                    })?;
            let client = if let Some(client) = context.fallback_client {
                client
            } else {
                connector
                    .connect(&target.cluster, "bmux-cli-cluster-attach")
                    .await
                    .map_err(|reason| AttachProviderError::OpenFailed {
                        provider_id: CLUSTER_PROVIDER_ID.to_string(),
                        reason: format!(
                            "failed connecting cluster bootstrap target '{}': {reason}",
                            target.cluster
                        ),
                    })?
            };
            let principal_id = format!("principal:{}", client.principal_id());
            let session = ClusterAttachSession::open(
                client,
                connector,
                target.cluster.clone(),
                target.workspace.clone(),
                principal_id,
                resume,
            )
            .await
            .map_err(|error| AttachProviderError::OpenFailed {
                provider_id: CLUSTER_PROVIDER_ID.to_string(),
                reason: error.to_string(),
            })?;
            Ok(AttachProviderSession {
                backend: AttachProviderBackend::Session(Box::new(session)),
                target: Some(target.workspace.clone()),
            })
        })
    }
}

/// One stream's screen as the session holds it: a grid kept only by its worker's frames, and
/// where the worker's and the renderer's replicas of it stand.
struct StreamReplica {
    generation: u64,
    grid: bmux_terminal_grid::TerminalGrid,
    /// The last worker frame applied; `None` until the first.
    upstream: Option<bmux_terminal_grid::ReplicaCursor>,
    /// Where the renderer's replica stands: frames for it are written since. Its epoch is the
    /// stream cursor's offset.
    downstream: bmux_terminal_grid::ReplicaCursor,
    /// Where the renderer's replica will stand once it takes the snapshot last built, which may
    /// never reach it (the view that built it was abandoned).
    snapshotted: Option<bmux_terminal_grid::ReplicaCursor>,
    sync_update_active: bool,
}

impl StreamReplica {
    fn new(generation: u64) -> Result<Self, AttachSessionError> {
        Ok(Self {
            generation,
            grid: bmux_terminal_grid::TerminalGrid::new(
                1,
                1,
                bmux_terminal_grid::GridLimits::default(),
            )
            .map_err(|error| provider_reason(format!("stream grid failed: {error}")))?,
            upstream: None,
            downstream: bmux_terminal_grid::ReplicaCursor::default(),
            snapshotted: None,
            sync_update_active: false,
        })
    }

    const fn hydrated(&self) -> bool {
        self.upstream.is_some()
    }

    /// Apply a frame the worker pushed.
    ///
    /// # Errors
    /// The frame was not written for this replica: the worker must send a reset frame.
    fn apply_upstream(
        &mut self,
        frame: &[u8],
        sync_update_active: bool,
    ) -> Result<(), bmux_terminal_grid::FrameApplyError> {
        let mut cursor = self.upstream.unwrap_or_default();
        self.grid.apply_frame(frame, &mut cursor)?;
        self.upstream = Some(cursor);
        self.sync_update_active = sync_update_active;
        Ok(())
    }

    /// The whole screen for a renderer's new replica; the renderer's replica stands where it
    /// leaves it once the snapshot is adopted ([`adopt_snapshot_replicas`]).
    fn snapshot(
        &mut self,
        stream_id: AttachStreamId,
        surface_id: uuid::Uuid,
    ) -> AttachStreamSnapshot {
        let mut frame = Vec::new();
        let next = self
            .grid
            .write_frame_since(bmux_terminal_grid::ReplicaCursor::default(), 0, &mut frame)
            .next;
        self.snapshotted = Some(next);
        AttachStreamSnapshot {
            cursor: AttachStreamCursor {
                stream_id,
                surface_id,
                generation: self.generation,
                offset: next.epoch,
            },
            sync_update_active: self.sync_update_active,
            frame,
        }
    }

    /// The frame bringing the renderer's replica at `cursor` level with this one; `cursor`
    /// advances past it.
    fn relay(&mut self, cursor: &mut AttachStreamCursor) -> bmux_client::AttachStreamFrame {
        let mut frame = Vec::new();
        let summary = self.grid.write_frame_since(self.downstream, 0, &mut frame);
        let base = cursor.clone();
        self.downstream = summary.next;
        cursor.offset = summary.next.epoch;
        bmux_client::AttachStreamFrame {
            cursor: base,
            end_offset: summary.next.epoch,
            reset: summary.reset,
            sync_update_active: self.sync_update_active,
            frame,
        }
    }
}

/// The renderer takes `snapshot` (as a snapshot or in a reconciliation delta): its replicas of
/// the snapshot's streams stand where the snapshot leaves them.
fn adopt_snapshot_replicas(
    replicas: &mut BTreeMap<AttachStreamId, StreamReplica>,
    snapshot: &AttachProviderSnapshot,
) {
    for stream in &snapshot.streams {
        if let Some(replica) = replicas.get_mut(&stream.cursor.stream_id)
            && let Some(snapshotted) = replica.snapshotted.take()
        {
            replica.downstream = snapshotted;
        }
    }
}

/// Frames bringing the renderer's replicas of the `changed` streams level with the session's,
/// in stream order; their cursors advance.
fn relay_frames(
    streams: &mut [AttachStreamCursor],
    replicas: &mut BTreeMap<AttachStreamId, StreamReplica>,
    changed: &BTreeSet<AttachStreamId>,
) -> Vec<bmux_client::AttachProviderChange> {
    streams
        .iter_mut()
        .filter(|cursor| changed.contains(&cursor.stream_id))
        .filter_map(|cursor| {
            let replica = replicas
                .get_mut(&cursor.stream_id)
                .filter(|replica| replica.generation == cursor.generation)?;
            Some(bmux_client::AttachProviderChange::StreamFrame(
                replica.relay(cursor),
            ))
        })
        .collect()
}

/// A stream to observe: its execution and the lease the ingress signed for it.
struct ObserveTarget {
    stream_id: AttachStreamId,
    execution: ExecutionRef,
    authority: WorkerAuthority,
}

/// Why streams could not be observed.
enum ObserveError {
    /// The worker refused: the execution moved, ended, or is not running there yet.
    Worker(WorkerServiceError),
    /// The worker could not be reached, or its connection closed.
    Link(String),
    /// Anything else, which the session cannot recover from.
    Failed(AttachSessionError),
}

impl From<AttachSessionError> for ObserveError {
    fn from(error: AttachSessionError) -> Self {
        Self::Failed(error)
    }
}

impl ObserveError {
    fn into_session_error(self) -> AttachSessionError {
        match self {
            Self::Worker(error) => provider_reason(format!("worker observation failed: {error:?}")),
            Self::Link(reason) => provider_reason(format!("worker connection failed: {reason}")),
            Self::Failed(error) => error,
        }
    }
}

enum LinkCommand {
    Observe {
        authorities: Vec<WorkerAuthority>,
        reply: oneshot::Sender<Result<Result<Vec<ObservedExecution>, WorkerServiceError>, String>>,
    },
}

/// What a worker connection reports to the session.
enum LinkEvent {
    /// Frames the worker pushed, as one batch.
    Frames { link: u64, frames: Vec<PaneFrame> },
    /// The connection closed, or can no longer be followed.
    Closed { link: u64 },
}

/// One connection to a worker, which pushes it frames of the executions it observes.
struct WorkerLink {
    id: u64,
    commands: mpsc::UnboundedSender<LinkCommand>,
    /// The worker's panes this connection observes, and the streams they are.
    observed: BTreeMap<uuid::Uuid, AttachStreamId>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for WorkerLink {
    fn drop(&mut self) {
        // Closing the connection ends the worker's observation of it.
        self.task.abort();
    }
}

/// The session's worker connections, by endpoint, and the events they report.
struct WorkerLinks {
    connector: Arc<dyn AttachEndpointConnector>,
    links: BTreeMap<String, WorkerLink>,
    next_id: u64,
    events_tx: mpsc::UnboundedSender<LinkEvent>,
    events: mpsc::UnboundedReceiver<LinkEvent>,
}

impl WorkerLinks {
    fn new(connector: Arc<dyn AttachEndpointConnector>) -> Self {
        let (events_tx, events) = mpsc::unbounded_channel();
        Self {
            connector,
            links: BTreeMap::new(),
            next_id: 0,
            events_tx,
            events,
        }
    }

    /// A link, while it is current.
    fn current(&self, link: u64) -> Option<&WorkerLink> {
        self.links.values().find(|candidate| candidate.id == link)
    }

    /// Close a link; false when it was already replaced or closed.
    fn remove(&mut self, link: u64) -> bool {
        let before = self.links.len();
        self.links.retain(|_, candidate| candidate.id != link);
        self.links.len() != before
    }

    async fn connect(&mut self, endpoint: &str) -> Result<WorkerLink, String> {
        let client = self
            .connector
            .connect(endpoint, "bmux-cli-cluster-observe")
            .await?;
        let mut client =
            bmux_client::StreamingBmuxClient::from_client(client).map_err(|e| e.to_string())?;
        client
            .enable_event_push()
            .await
            .map_err(|error| error.to_string())?;
        self.next_id += 1;
        let id = self.next_id;
        let (commands, receiver) = mpsc::unbounded_channel();
        let task = tokio::spawn(run_worker_link(
            id,
            client,
            receiver,
            self.events_tx.clone(),
        ));
        Ok(WorkerLink {
            id,
            commands,
            observed: BTreeMap::new(),
            task,
        })
    }

    /// Have the worker at `endpoint` push frames of `targets` (replacing what it pushed this
    /// session before), connecting first if needed.
    async fn observe(
        &mut self,
        endpoint: &str,
        targets: &[ObserveTarget],
    ) -> Result<(), ObserveError> {
        if !self.links.contains_key(endpoint) {
            let link = self.connect(endpoint).await.map_err(ObserveError::Link)?;
            self.links.insert(endpoint.to_string(), link);
        }
        let (reply, replied) = oneshot::channel();
        let sent = self.links.get(endpoint).is_some_and(|link| {
            link.commands
                .send(LinkCommand::Observe {
                    authorities: targets
                        .iter()
                        .map(|target| target.authority.clone())
                        .collect(),
                    reply,
                })
                .is_ok()
        });
        let replied = if sent {
            replied
                .await
                .unwrap_or_else(|_| Err("worker connection closed".to_string()))
        } else {
            Err("worker connection closed".to_string())
        };
        let observed = match replied {
            Ok(Ok(observed)) => observed,
            Ok(Err(error)) => return Err(ObserveError::Worker(error)),
            Err(reason) => {
                self.links.remove(endpoint);
                return Err(ObserveError::Link(reason));
            }
        };
        let panes = observed_panes(targets, observed)?;
        if let Some(link) = self.links.get_mut(endpoint) {
            link.observed = panes;
        }
        Ok(())
    }
}

/// A worker connection: forwards the frames the worker pushes, and sends the session's requests.
async fn run_worker_link(
    id: u64,
    mut client: bmux_client::StreamingBmuxClient,
    mut commands: mpsc::UnboundedReceiver<LinkCommand>,
    events: mpsc::UnboundedSender<LinkEvent>,
) {
    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(LinkCommand::Observe { authorities, reply }) = command else {
                    return;
                };
                let result = cluster_worker_stream::client::observe(&mut client, authorities, 0)
                    .await
                    .map_err(|error| error.to_string());
                let _ = reply.send(result);
            }
            event = client.event_receiver().recv() => {
                let Some(event) = event else {
                    let _ = events.send(LinkEvent::Closed { link: id });
                    return;
                };
                let bmux_client::ServerEvent::PluginBusEvent { kind, payload } = event else {
                    continue;
                };
                if kind != bmux_pane_runtime_plugin_api::FRAME_PUSH_KIND {
                    continue;
                }
                // Frames that cannot be read leave the replicas behind: the connection is
                // replaced, and the new one is sent reset frames.
                let Ok(batch) = bmux_ipc::decode::<AttachPaneFrames>(&payload) else {
                    let _ = events.send(LinkEvent::Closed { link: id });
                    return;
                };
                if events
                    .send(LinkEvent::Frames { link: id, frames: batch.frames })
                    .is_err()
                {
                    return;
                }
            }
        }
    }
}

/// The ready executions of a layout's panes, as streams.
fn ready_executions(
    layout: &bmux_cluster_plugin_api::cluster_types::AttachLayout,
) -> Result<Vec<(AttachStreamId, ExecutionRef)>, AttachSessionError> {
    layout
        .panes
        .iter()
        .filter(|pane| {
            pane.availability == bmux_cluster_plugin_api::cluster_types::PaneAvailability::Ready
        })
        .filter_map(|pane| pane.execution.as_ref())
        .map(|assignment| {
            Ok((
                AttachStreamId::new(format!("execution:{}", assignment.execution_id.value))?,
                ExecutionRef {
                    execution_id: assignment.execution_id.clone(),
                    generation: assignment.generation,
                },
            ))
        })
        .collect()
}

/// `routes` for `executions` grouped by worker endpoint, each checked to be for the execution
/// generation it was asked for.
fn route_plan(
    executions: &[(AttachStreamId, ExecutionRef)],
    routes: Vec<WorkerRoute>,
) -> Result<BTreeMap<String, Vec<ObserveTarget>>, AttachSessionError> {
    if routes.len() != executions.len() {
        return Err(provider_reason(format!(
            "ingress returned {} worker routes for {} executions",
            routes.len(),
            executions.len()
        )));
    }
    let mut plan: BTreeMap<String, Vec<ObserveTarget>> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for route in routes {
        let (stream_id, execution) = executions
            .iter()
            .find(|(_, execution)| execution.execution_id == route.execution_id)
            .ok_or_else(|| {
                provider_reason(format!(
                    "ingress routed execution {} that was not asked for",
                    route.execution_id.value
                ))
            })?;
        if route.generation != execution.generation
            || route.authority.execution_id != execution.execution_id
            || route.authority.generation != execution.generation
        {
            return Err(provider_reason(format!(
                "worker route for execution {} is not for generation {}",
                execution.execution_id.value, execution.generation
            )));
        }
        if !seen.insert(execution.execution_id.value) {
            return Err(provider_reason(format!(
                "ingress routed execution {} twice",
                execution.execution_id.value
            )));
        }
        plan.entry(route.endpoint).or_default().push(ObserveTarget {
            stream_id: stream_id.clone(),
            execution: execution.clone(),
            authority: route.authority,
        });
    }
    Ok(plan)
}

/// The worker panes whose pushed frames are `targets`' streams, checked to be exactly the
/// executions asked for.
fn observed_panes(
    targets: &[ObserveTarget],
    observed: Vec<ObservedExecution>,
) -> Result<BTreeMap<uuid::Uuid, AttachStreamId>, AttachSessionError> {
    if observed.len() != targets.len() {
        return Err(provider_reason(format!(
            "worker observes {} executions of the {} asked for",
            observed.len(),
            targets.len()
        )));
    }
    let mut panes = BTreeMap::new();
    for execution in observed {
        let target = targets
            .iter()
            .find(|target| {
                target.execution.execution_id == execution.execution_id
                    && target.execution.generation == execution.generation
            })
            .ok_or_else(|| {
                provider_reason(format!(
                    "worker observes execution {} generation {}, which was not asked for",
                    execution.execution_id.value, execution.generation
                ))
            })?;
        // As many executions as asked for, none twice and no pane twice: exactly those asked.
        if panes
            .values()
            .any(|stream_id| *stream_id == target.stream_id)
            || panes
                .insert(execution.pane_id, target.stream_id.clone())
                .is_some()
        {
            return Err(provider_reason(format!(
                "worker observes execution {} or its pane more than once",
                execution.execution_id.value
            )));
        }
    }
    Ok(panes)
}

struct ClusterAttachSession {
    client: BmuxClient,
    links: WorkerLinks,
    cluster: String,
    workspace_id: uuid::Uuid,
    tab_id: bmux_cluster_plugin_api::cluster_types::LogicalTabId,
    principal_id: String,
    snapshot: Option<AttachProviderSnapshot>,
    view_revision: AttachViewRevision,
    control_revision: u64,
    event_sequence: AttachDeltaSequence,
    streams: Vec<AttachStreamCursor>,
    replicas: BTreeMap<AttachStreamId, StreamReplica>,
    scene: bmux_attach_layout_protocol::AttachScene,
    pending_events: std::collections::VecDeque<bmux_client::AttachProviderEvent>,
    unzoomed_scene: Option<bmux_attach_layout_protocol::AttachScene>,
    zoomed_pane: Option<uuid::Uuid>,
    viewport: (u16, u16),
    next_layout_check: tokio::time::Instant,
    /// When the streams must be observed again (a worker connection closed or fell out of step).
    reobserve_at: Option<tokio::time::Instant>,
    detached: bool,
}

impl std::fmt::Debug for ClusterAttachSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClusterAttachSession")
            .field("cluster", &self.cluster)
            .field("workspace_id", &self.workspace_id)
            .field("tab_id", &self.tab_id)
            .field("principal_id", &self.principal_id)
            .field("view_revision", &self.view_revision)
            .field("control_revision", &self.control_revision)
            .field("event_sequence", &self.event_sequence)
            .field("viewport", &self.viewport)
            .field("detached", &self.detached)
            .finish_non_exhaustive()
    }
}

impl ClusterAttachSession {
    async fn open(
        mut client: BmuxClient,
        connector: Arc<dyn AttachEndpointConnector>,
        cluster: String,
        workspace: String,
        principal_id: String,
        resume: Option<AttachResumeState>,
    ) -> Result<Self, AttachSessionError> {
        let view = cluster_control_state::client::read_linearizable(&mut client)
            .await
            .map_err(provider_error)?
            .map_err(|error| provider_reason(format!("control-state read failed: {error:?}")))?;
        if view.cluster_id != cluster {
            return Err(provider_reason(format!(
                "ingress belongs to cluster '{}', not '{cluster}'",
                view.cluster_id
            )));
        }
        let record = view
            .workspaces
            .iter()
            .find(|record| {
                record.workspace_id.value.to_string() == workspace
                    || record.name.as_deref() == Some(workspace.as_str())
            })
            .ok_or_else(|| provider_reason(format!("workspace '{workspace}' was not found")))?;
        let workspace_id = record.workspace_id.value;
        let layout = cluster_attach_state::client::layout(
            &mut client,
            record.workspace_id.clone(),
            None,
            80,
            24,
        )
        .await
        .map_err(provider_error)?
        .map_err(|error| provider_reason(format!("logical layout failed: {error:?}")))?;
        let mut session = Self {
            client,
            links: WorkerLinks::new(connector),
            cluster,
            workspace_id,
            tab_id: layout.tab_id.clone(),
            principal_id,
            snapshot: None,
            view_revision: AttachViewRevision::default(),
            control_revision: 0,
            event_sequence: AttachDeltaSequence::default(),
            streams: Vec::new(),
            replicas: BTreeMap::new(),
            scene: bmux_attach_layout_protocol::AttachScene {
                session_id: workspace_id,
                focus: bmux_attach_layout_protocol::AttachFocusTarget::None,
                surfaces: Vec::new(),
            },
            pending_events: std::collections::VecDeque::new(),
            unzoomed_scene: None,
            zoomed_pane: None,
            viewport: (80, 24),
            next_layout_check: tokio::time::Instant::now(),
            reobserve_at: None,
            detached: false,
        };
        let (layout, built) = session.build_consistent_snapshot(layout).await?;
        let built = initial_snapshot_for_resume(built, layout.control_revision, resume)?;
        session.view_revision = built.view_revision;
        session.event_sequence = built.event_sequence;
        session.adopt(&layout, &built);
        session.snapshot = Some(built);
        Ok(session)
    }

    /// Take `snapshot` of `layout` as the session's view: its streams, scene, and revisions.
    fn adopt(
        &mut self,
        layout: &bmux_cluster_plugin_api::cluster_types::AttachLayout,
        snapshot: &AttachProviderSnapshot,
    ) {
        self.streams = snapshot
            .streams
            .iter()
            .map(|stream| stream.cursor.clone())
            .collect();
        let streams = &self.streams;
        self.replicas
            .retain(|id, _| streams.iter().any(|cursor| &cursor.stream_id == id));
        adopt_snapshot_replicas(&mut self.replicas, snapshot);
        self.scene = snapshot.scene.clone();
        self.control_revision = layout.control_revision;
        self.tab_id = layout.tab_id.clone();
    }

    fn workspace(&self) -> bmux_cluster_plugin_api::cluster_types::WorkspaceId {
        bmux_cluster_plugin_api::cluster_types::WorkspaceId {
            value: self.workspace_id,
        }
    }

    async fn current_layout(
        &mut self,
    ) -> Result<bmux_cluster_plugin_api::cluster_types::AttachLayout, AttachSessionError> {
        let workspace = self.workspace();
        cluster_attach_state::client::layout(
            &mut self.client,
            workspace,
            Some(self.tab_id.clone()),
            self.viewport.0,
            self.viewport.1,
        )
        .await
        .map_err(provider_error)?
        .map_err(|error| provider_reason(format!("logical layout read failed: {error:?}")))
    }

    /// The layout, when its control revision advanced past the session's.
    ///
    /// This is the session's remaining poll: arrangement changes (panes added, moved or closed,
    /// executions restarted as new generations, availability) are committed through cluster
    /// consensus on the ingress, and nothing yet pushes them to a client, so the session asks
    /// every [`LAYOUT_CHECK_INTERVAL`]. Screens are not polled: workers push their frames.
    async fn changed_layout(
        &mut self,
    ) -> Result<Option<bmux_cluster_plugin_api::cluster_types::AttachLayout>, AttachSessionError>
    {
        let layout = self.current_layout().await?;
        if layout.control_revision < self.control_revision {
            return Err(provider_reason(format!(
                "cluster control revision regressed from {} to {}",
                self.control_revision, layout.control_revision
            )));
        }
        Ok((layout.control_revision > self.control_revision).then_some(layout))
    }

    /// Have each stream's worker push its frames to this session: routes and leases from the
    /// ingress, then one connection per worker. Connections no stream needs close. Until this
    /// succeeds, a pending re-observation stays pending (also when the call is cancelled).
    async fn observe(
        &mut self,
        executions: &[(AttachStreamId, ExecutionRef)],
    ) -> Result<(), ObserveError> {
        if executions.is_empty() {
            self.links.links.clear();
            self.reobserve_at = None;
            return Ok(());
        }
        let workspace = self.workspace();
        let routes = cluster_attach_state::client::routes(
            &mut self.client,
            workspace,
            self.principal_id.clone(),
            executions
                .iter()
                .map(|(_, execution)| execution.clone())
                .collect(),
        )
        .await
        .map_err(provider_error)?
        .map_err(ObserveError::Worker)?;
        let plan = route_plan(executions, routes)?;
        self.links
            .links
            .retain(|endpoint, _| plan.contains_key(endpoint));
        for (endpoint, targets) in &plan {
            self.links.observe(endpoint, targets).await?;
        }
        self.reobserve_at = None;
        Ok(())
    }

    /// Observe the session's streams again after a worker connection closed or fell out of step:
    /// a new connection is sent reset frames. When the workers cannot be observed, the layout
    /// says whether the executions moved on (reconciled at once) or not (tried again later).
    async fn reobserve(
        &mut self,
    ) -> Result<Option<bmux_client::AttachProviderEvent>, AttachSessionError> {
        let executions = self
            .streams
            .iter()
            .filter(|cursor| cursor.stream_id.as_str().starts_with("execution:"))
            .map(|cursor| {
                Ok((
                    cursor.stream_id.clone(),
                    ExecutionRef {
                        execution_id: stream_execution_id(&cursor.stream_id)?,
                        generation: cursor.generation,
                    },
                ))
            })
            .collect::<Result<Vec<_>, AttachSessionError>>()?;
        let error = match self.observe(&executions).await {
            Ok(()) => return Ok(None),
            Err(ObserveError::Failed(error)) => return Err(error),
            Err(error) => error,
        };
        if let Some(layout) = self.changed_layout().await? {
            return self.reconcile_layout(layout).await.map(Some);
        }
        match error {
            ObserveError::Worker(
                WorkerServiceError::NotFound { .. } | WorkerServiceError::StaleGeneration { .. },
            ) => Err(provider_reason(
                "worker stream ended without an authoritative control-state transition".to_string(),
            )),
            ObserveError::Worker(WorkerServiceError::Unavailable { .. })
            | ObserveError::Link(_) => {
                self.reobserve_at = Some(tokio::time::Instant::now() + OBSERVE_RETRY_INTERVAL);
                Ok(None)
            }
            error => Err(error.into_session_error()),
        }
    }

    /// Apply what a worker connection reported; streams whose screens changed join `changed`.
    fn apply_link_event(&mut self, event: LinkEvent, changed: &mut BTreeSet<AttachStreamId>) {
        match event {
            LinkEvent::Frames { link, frames } => {
                // A closed or replaced connection's frames are dropped, as are frames of panes
                // it no longer observes.
                let Some(current) = self.links.current(link) else {
                    return;
                };
                let frames: Vec<(AttachStreamId, PaneFrame)> = frames
                    .into_iter()
                    .filter_map(|frame| {
                        let stream_id = current.observed.get(&frame.pane_id)?.clone();
                        Some((stream_id, frame))
                    })
                    .collect();
                for (stream_id, frame) in frames {
                    let Some(replica) = self.replicas.get_mut(&stream_id) else {
                        continue;
                    };
                    if replica
                        .apply_upstream(&frame.encoded, frame.sync_update_active)
                        .is_err()
                    {
                        // The connection's frames no longer follow this replica; a new
                        // connection is sent reset frames.
                        self.links.remove(link);
                        self.reobserve_at = Some(tokio::time::Instant::now());
                        return;
                    }
                    changed.insert(stream_id);
                }
            }
            LinkEvent::Closed { link } => {
                if self.links.remove(link) {
                    self.reobserve_at = Some(tokio::time::Instant::now());
                }
            }
        }
    }

    /// Wait until every stream of `executions` has its first frame (a worker pushes one as soon
    /// as it observes a pane), observing again if a connection fails meanwhile.
    async fn await_first_frames(
        &mut self,
        executions: &[(AttachStreamId, ExecutionRef)],
    ) -> Result<(), AttachSessionError> {
        let deadline = tokio::time::Instant::now() + HYDRATION_TIMEOUT;
        loop {
            if self.reobserve_at.is_some() {
                self.observe(executions)
                    .await
                    .map_err(ObserveError::into_session_error)?;
            }
            if executions.iter().all(|(stream_id, _)| {
                self.replicas
                    .get(stream_id)
                    .is_some_and(StreamReplica::hydrated)
            }) {
                return Ok(());
            }
            let event = tokio::time::timeout_at(deadline, self.links.events.recv())
                .await
                .map_err(|_| {
                    provider_reason("workers did not send their panes' screens in time".to_string())
                })?
                .ok_or_else(|| provider_reason("worker connections ended".to_string()))?;
            // The snapshot being built carries these screens whole.
            self.apply_link_event(event, &mut BTreeSet::new());
        }
    }

    /// Observe `layout`'s streams and build its snapshot from their screens.
    async fn build_snapshot(
        &mut self,
        layout: &bmux_cluster_plugin_api::cluster_types::AttachLayout,
    ) -> Result<AttachProviderSnapshot, AttachSessionError> {
        if layout.panes.is_empty() {
            return Err(provider_reason(
                "workspace has no logical panes".to_string(),
            ));
        }
        validate_unique_snapshot_streams(layout)?;
        let executions = ready_executions(layout)?;
        for (stream_id, execution) in &executions {
            if self
                .replicas
                .get(stream_id)
                .is_none_or(|replica| replica.generation != execution.generation)
            {
                self.replicas
                    .insert(stream_id.clone(), StreamReplica::new(execution.generation)?);
            }
        }
        self.observe(&executions)
            .await
            .map_err(ObserveError::into_session_error)?;
        self.await_first_frames(&executions).await?;
        assemble_cluster_snapshot(layout, &mut self.replicas)
    }

    /// A snapshot of `layout`, or of the layout it became if the control state changed while the
    /// snapshot was built.
    async fn build_consistent_snapshot(
        &mut self,
        mut layout: bmux_cluster_plugin_api::cluster_types::AttachLayout,
    ) -> Result<
        (
            bmux_cluster_plugin_api::cluster_types::AttachLayout,
            AttachProviderSnapshot,
        ),
        AttachSessionError,
    > {
        const MAX_ATTEMPTS: usize = 4;
        for _ in 0..MAX_ATTEMPTS {
            let built = self.build_snapshot(&layout).await;
            let confirmed = cluster_attach_state::client::layout(
                &mut self.client,
                layout.workspace_id.clone(),
                Some(layout.tab_id.clone()),
                self.viewport.0,
                self.viewport.1,
            )
            .await
            .map_err(provider_error)?
            .map_err(|error| {
                provider_reason(format!(
                    "logical layout confirmation failed during snapshot assembly: {error:?}"
                ))
            })?;
            if confirmed.control_revision < layout.control_revision {
                return Err(provider_reason(format!(
                    "cluster control revision regressed from {} to {} during snapshot assembly",
                    layout.control_revision, confirmed.control_revision
                )));
            }
            if confirmed.control_revision == layout.control_revision {
                return Ok((layout, built?));
            }
            layout = confirmed;
        }
        Err(provider_reason(
            "cluster control state changed continuously during attach snapshot assembly"
                .to_string(),
        ))
    }

    async fn reconcile_layout(
        &mut self,
        layout: bmux_cluster_plugin_api::cluster_types::AttachLayout,
    ) -> Result<bmux_client::AttachProviderEvent, AttachSessionError> {
        if layout.control_revision <= self.control_revision {
            return Err(provider_reason(format!(
                "cluster control revision did not advance during reconciliation: current {}, received {}",
                self.control_revision, layout.control_revision
            )));
        }
        let focused = match self.scene.focus {
            bmux_attach_layout_protocol::AttachFocusTarget::Pane { pane_id }
            | bmux_attach_layout_protocol::AttachFocusTarget::Surface {
                surface_id: pane_id,
            } => Some(pane_id),
            bmux_attach_layout_protocol::AttachFocusTarget::None => None,
        };
        let (layout, mut built) = self.build_consistent_snapshot(layout).await?;
        let base_scene = cluster_scene_from_layout(&layout, focused)?;
        let (scene, next_unzoomed_scene, next_zoomed_pane) =
            if let Some(zoomed_pane) = self.zoomed_pane {
                match zoom_cluster_scene(&base_scene, zoomed_pane, self.viewport) {
                    Ok(scene) => (scene, Some(base_scene), Some(zoomed_pane)),
                    Err(_) => (base_scene, None, None),
                }
            } else {
                (base_scene, None, None)
            };
        let reconciliation_id = format!("reconcile:{}", layout.control_revision);
        // The workers' panes take the scene's sizes; their frames then carry them.
        resize_cluster_scene_workers(
            &mut self.client,
            self.workspace_id,
            &built
                .streams
                .iter()
                .map(|stream| stream.cursor.clone())
                .collect::<Vec<_>>(),
            &scene,
            &self.principal_id,
            &reconciliation_id,
        )
        .await?;
        built.scene = scene;

        let base_view_revision = self.view_revision;
        self.view_revision = AttachViewRevision(self.view_revision.0.saturating_add(1));
        self.event_sequence = AttachDeltaSequence(self.event_sequence.0.saturating_add(1));
        let delta = cluster_reconciliation_delta(
            base_view_revision,
            self.view_revision,
            self.event_sequence,
            layout.control_revision,
            &self.streams,
            &built,
        );
        self.adopt(&layout, &built);
        self.unzoomed_scene = next_unzoomed_scene;
        self.zoomed_pane = next_zoomed_pane;
        Ok(bmux_client::AttachProviderEvent::Delta(delta))
    }

    async fn replace_attached_tab(
        &mut self,
        tab_id: bmux_cluster_plugin_api::cluster_types::LogicalTabId,
        action_command_id: &str,
    ) -> Result<(), AttachSessionError> {
        if tab_id == self.tab_id {
            return Ok(());
        }
        let workspace = self.workspace();
        let layout = cluster_attach_state::client::layout(
            &mut self.client,
            workspace,
            Some(tab_id),
            self.viewport.0,
            self.viewport.1,
        )
        .await
        .map_err(provider_error)?
        .map_err(|error| provider_reason(format!("logical tab selection failed: {error:?}")))?;
        if layout.control_revision < self.control_revision {
            return Err(provider_reason(format!(
                "cluster control revision regressed from {} to {} during tab selection",
                self.control_revision, layout.control_revision
            )));
        }
        let (layout, built) = self.build_consistent_snapshot(layout).await?;
        resize_cluster_layout_workers(
            &mut self.client,
            &layout,
            &self.principal_id,
            action_command_id,
        )
        .await?;
        let base_view_revision = self.view_revision;
        self.view_revision = AttachViewRevision(self.view_revision.0.saturating_add(1));
        self.event_sequence = AttachDeltaSequence(self.event_sequence.0.saturating_add(1));
        let delta = cluster_reconciliation_delta(
            base_view_revision,
            self.view_revision,
            self.event_sequence,
            layout.control_revision,
            &self.streams,
            &built,
        );
        self.adopt(&layout, &built);
        self.pending_events
            .push_back(bmux_client::AttachProviderEvent::Delta(delta));
        self.unzoomed_scene = None;
        self.zoomed_pane = None;
        Ok(())
    }

    async fn select_tab(
        &mut self,
        action: &str,
        arguments: &[String],
        action_command_id: &str,
    ) -> Result<bool, AttachSessionError> {
        let view = cluster_control_state::client::read_linearizable(&mut self.client)
            .await
            .map_err(provider_error)?
            .map_err(|error| provider_reason(format!("control-state read failed: {error:?}")))?;
        if view.revision < self.control_revision {
            return Err(provider_reason(format!(
                "cluster control revision regressed from {} to {} during tab navigation",
                self.control_revision, view.revision
            )));
        }
        let mut tabs = view
            .tabs
            .iter()
            .filter(|tab| tab.workspace_id.value == self.workspace_id)
            .collect::<Vec<_>>();
        tabs.sort_by_key(|tab| tab.tab_id.value.as_u128());
        let selected = select_cluster_tab(&tabs, &self.tab_id, action, arguments)?;
        if selected.tab_id == self.tab_id {
            return Ok(false);
        }
        self.replace_attached_tab(selected.tab_id.clone(), action_command_id)
            .await?;
        Ok(true)
    }

    async fn toggle_zoom(
        &mut self,
        pane_id: uuid::Uuid,
        action_command_id: &str,
    ) -> Result<(), AttachSessionError> {
        let was_zoomed = self.zoomed_pane;
        let previous_unzoomed = self.unzoomed_scene.clone();
        let scene = if was_zoomed == Some(pane_id) {
            previous_unzoomed
                .clone()
                .ok_or_else(|| provider_reason("zoom restore scene is unavailable".to_string()))?
        } else {
            let base = previous_unzoomed
                .clone()
                .unwrap_or_else(|| self.scene.clone());
            zoom_cluster_scene(&base, pane_id, self.viewport)?
        };
        // The workers' panes take the zoomed sizes; their frames then carry them.
        resize_cluster_scene_workers(
            &mut self.client,
            self.workspace_id,
            &self.streams,
            &scene,
            &self.principal_id,
            action_command_id,
        )
        .await?;
        if was_zoomed == Some(pane_id) {
            self.zoomed_pane = None;
            self.unzoomed_scene = None;
        } else {
            self.unzoomed_scene = Some(previous_unzoomed.unwrap_or_else(|| self.scene.clone()));
            self.zoomed_pane = Some(pane_id);
        }
        self.enqueue_scene(scene);
        Ok(())
    }

    fn resume_state(&self) -> AttachResumeState {
        AttachResumeState {
            view_revision: self.view_revision,
            event_sequence: self.event_sequence,
            streams: self.streams.clone(),
            provider_token: self.control_revision.to_be_bytes().to_vec(),
        }
    }

    fn enqueue_scene(&mut self, scene: bmux_attach_layout_protocol::AttachScene) {
        let base_view_revision = self.view_revision;
        self.view_revision = AttachViewRevision(self.view_revision.0.saturating_add(1));
        self.event_sequence = AttachDeltaSequence(self.event_sequence.0.saturating_add(1));
        self.scene = scene.clone();
        self.pending_events
            .push_back(bmux_client::AttachProviderEvent::Delta(
                bmux_client::AttachProviderDelta {
                    sequence: self.event_sequence,
                    base_view_revision,
                    view_revision: self.view_revision,
                    changes: vec![bmux_client::AttachProviderChange::Scene(scene)],
                    resume: self.resume_state(),
                },
            ));
    }

    /// One delta of frames for the streams whose screens changed, if any did.
    fn frames_delta(
        &mut self,
        changed: &BTreeSet<AttachStreamId>,
    ) -> Option<bmux_client::AttachProviderEvent> {
        let frames = relay_frames(&mut self.streams, &mut self.replicas, changed);
        if frames.is_empty() {
            return None;
        }
        self.event_sequence = AttachDeltaSequence(self.event_sequence.0.saturating_add(1));
        Some(bmux_client::AttachProviderEvent::Delta(
            bmux_client::AttachProviderDelta {
                sequence: self.event_sequence,
                base_view_revision: self.view_revision,
                view_revision: self.view_revision,
                changes: frames,
                resume: self.resume_state(),
            },
        ))
    }

    /// Wait for the next frames or the next scheduled check, then return the delta it produced.
    async fn next_change(
        &mut self,
    ) -> Result<Option<bmux_client::AttachProviderEvent>, AttachSessionError> {
        let now = tokio::time::Instant::now();
        if now >= self.next_layout_check {
            self.next_layout_check = now + LAYOUT_CHECK_INTERVAL;
            if let Some(layout) = self.changed_layout().await? {
                return self.reconcile_layout(layout).await.map(Some);
            }
        }
        if self.reobserve_at.is_some_and(|at| at <= now) {
            return self.reobserve().await;
        }
        let wake_at = self
            .reobserve_at
            .map_or(self.next_layout_check, |at| at.min(self.next_layout_check));
        tokio::select! {
            event = self.links.events.recv() => {
                let event = event
                    .ok_or_else(|| provider_reason("worker connections ended".to_string()))?;
                // Everything already pushed goes out as one delta.
                let mut changed = BTreeSet::new();
                self.apply_link_event(event, &mut changed);
                while let Ok(event) = self.links.events.try_recv() {
                    self.apply_link_event(event, &mut changed);
                }
                Ok(self.frames_delta(&changed))
            }
            () = tokio::time::sleep_until(wake_at) => Ok(None),
        }
    }
}

impl AttachSession for ClusterAttachSession {
    fn initial_snapshot(&mut self) -> AttachSessionFuture<'_, AttachProviderSnapshot> {
        let snapshot = self.snapshot.take();
        Box::pin(async move {
            snapshot.ok_or_else(|| provider_reason("initial snapshot already consumed".to_string()))
        })
    }

    fn next_event(&mut self) -> AttachSessionFuture<'_, bmux_client::AttachProviderEvent> {
        Box::pin(async move {
            loop {
                if self.detached {
                    return Ok(bmux_client::AttachProviderEvent::Detached);
                }
                if let Some(event) = self.pending_events.pop_front() {
                    return Ok(event);
                }
                if let Some(event) = self.next_change().await? {
                    return Ok(event);
                }
            }
        })
    }

    fn send_input(
        &mut self,
        input: AttachProviderInput,
    ) -> AttachSessionFuture<'_, AttachProviderAck> {
        Box::pin(async move {
            let execution_id = stream_execution_id(&input.stream_id)?;
            let view = cluster_control_state::client::read_linearizable(&mut self.client)
                .await
                .map_err(provider_error)?
                .map_err(|error| {
                    provider_reason(format!("control-state read failed: {error:?}"))
                })?;
            let pane = view
                .panes
                .iter()
                .find(|pane| {
                    pane.workspace_id.value == self.workspace_id
                        && pane
                            .execution
                            .as_ref()
                            .is_some_and(|assignment| assignment.execution_id == execution_id)
                })
                .ok_or_else(|| {
                    provider_reason("stream no longer has an authoritative pane".to_string())
                })?;
            let assignment = pane.execution.as_ref().expect("checked assignment");
            if assignment.generation != input.generation {
                return Err(provider_reason(
                    "input targets a stale execution generation".to_string(),
                ));
            }
            // Input is encoded for the modes the program set, as the stream's screen has them.
            let protocol = self
                .replicas
                .get(&input.stream_id)
                .filter(|replica| replica.hydrated())
                .ok_or_else(|| {
                    provider_reason("stream terminal protocol state is unavailable".to_string())
                })?
                .grid
                .protocol_state();
            let data = match input.payload {
                bmux_client::AttachInputPayload::Bytes(data) => data,
                bmux_client::AttachInputPayload::Key { stroke, enhanced } => {
                    encode_cluster_key(&stroke, enhanced, protocol)?
                }
                bmux_client::AttachInputPayload::Paste(data) => {
                    encode_cluster_paste(data, protocol)
                }
                bmux_client::AttachInputPayload::Mouse(mouse) => {
                    encode_cluster_mouse(mouse, protocol)?
                }
            };
            let command_id = command_id(input.command_sequence, &input.stream_id);
            let workspace = self.workspace();
            cluster_attach_command::client::input(
                &mut self.client,
                workspace,
                command_id,
                execution_id,
                input.generation,
                self.principal_id.clone(),
                data,
            )
            .await
            .map_err(provider_error)?
            .map_err(|error| provider_reason(format!("worker input failed: {error:?}")))?;
            Ok(AttachProviderAck {
                command_id: None,
                accepted: true,
                message: None,
            })
        })
    }

    fn update_viewport(
        &mut self,
        viewport: AttachProviderViewport,
    ) -> AttachSessionFuture<'_, AttachProviderAck> {
        Box::pin(async move {
            let next_viewport = (viewport.columns, viewport.rows);
            let workspace = self.workspace();
            let layout = cluster_attach_state::client::layout(
                &mut self.client,
                workspace.clone(),
                Some(self.tab_id.clone()),
                viewport.columns,
                viewport.rows,
            )
            .await
            .map_err(provider_error)?
            .map_err(|error| provider_reason(format!("logical layout resize failed: {error:?}")))?;
            if layout.control_revision != self.control_revision {
                return Err(provider_reason(
                    "logical layout changed during viewport resize; wait for scene reconciliation"
                        .to_string(),
                ));
            }
            let focused = match self.scene.focus {
                bmux_attach_layout_protocol::AttachFocusTarget::Pane { pane_id }
                | bmux_attach_layout_protocol::AttachFocusTarget::Surface {
                    surface_id: pane_id,
                } => Some(pane_id),
                bmux_attach_layout_protocol::AttachFocusTarget::None => None,
            };
            let base_scene = cluster_scene_from_layout(&layout, focused)?;
            let scene = if let Some(zoomed_pane) = self.zoomed_pane {
                zoom_cluster_scene(&base_scene, zoomed_pane, next_viewport)?
            } else {
                base_scene.clone()
            };
            // The workers' panes take the new sizes; their frames then carry them.
            for (cursor, cols, rows) in provider_stream_viewports(&scene, &self.streams)? {
                if !cursor.stream_id.as_str().starts_with("execution:") {
                    continue;
                }
                let execution_id = stream_execution_id(&cursor.stream_id)?;
                let resize_id = command_id(viewport.command_sequence, &cursor.stream_id);
                cluster_attach_command::client::resize(
                    &mut self.client,
                    bmux_cluster_plugin_api::cluster_types::AttachResizeRequest {
                        workspace_id: workspace.clone(),
                        command_id: resize_id,
                        execution_id,
                        generation: cursor.generation,
                        principal_id: self.principal_id.clone(),
                        cols,
                        rows,
                    },
                )
                .await
                .map_err(provider_error)?
                .map_err(|error| provider_reason(format!("worker resize failed: {error:?}")))?;
            }
            self.viewport = next_viewport;
            if self.zoomed_pane.is_some() {
                self.unzoomed_scene = Some(base_scene);
            }
            if scene != self.scene {
                self.enqueue_scene(scene);
            }
            Ok(AttachProviderAck {
                command_id: None,
                accepted: true,
                message: None,
            })
        })
    }

    fn execute_action(
        &mut self,
        action: AttachProviderAction,
    ) -> AttachSessionFuture<'_, AttachProviderAck> {
        if action.action == "focus" {
            let result = apply_local_focus_action(&self.scene, &action.arguments);
            return Box::pin(async move {
                let scene = result?;
                if let Some(scene) = scene {
                    self.enqueue_scene(scene);
                    Ok(AttachProviderAck {
                        command_id: Some(action.command_id),
                        accepted: true,
                        message: None,
                    })
                } else {
                    Ok(AttachProviderAck {
                        command_id: Some(action.command_id),
                        accepted: true,
                        message: Some("logical focus did not change".to_string()),
                    })
                }
            });
        }
        if action.action == "zoom" {
            let pane_id = action
                .arguments
                .first()
                .ok_or_else(|| provider_reason("logical zoom requires a pane ID".to_string()))
                .and_then(|value| {
                    value.parse::<uuid::Uuid>().map_err(|error| {
                        provider_reason(format!("logical zoom pane ID is invalid: {error}"))
                    })
                });
            return Box::pin(async move {
                let pane_id = pane_id?;
                self.toggle_zoom(pane_id, &action.command_id).await?;
                Ok(AttachProviderAck {
                    command_id: Some(action.command_id),
                    accepted: true,
                    message: None,
                })
            });
        }
        if matches!(action.action.as_str(), "tab-next" | "tab-prev" | "tab-goto") {
            return Box::pin(async move {
                let changed = self
                    .select_tab(&action.action, &action.arguments, &action.command_id)
                    .await?;
                Ok(AttachProviderAck {
                    command_id: Some(action.command_id),
                    accepted: true,
                    message: (!changed).then(|| "logical tab did not change".to_string()),
                })
            });
        }
        Box::pin(async move {
            let command_uuid = action
                .command_id
                .parse::<uuid::Uuid>()
                .unwrap_or_else(|_| command_id_from_text(&action.command_id));
            let response = cluster_attach_command::client::action(
                &mut self.client,
                bmux_cluster_plugin_api::cluster_types::AttachActionRequest {
                    workspace_id: bmux_cluster_plugin_api::cluster_types::WorkspaceId {
                        value: self.workspace_id,
                    },
                    command_id: bmux_cluster_plugin_api::cluster_types::CommandId {
                        value: command_uuid,
                    },
                    principal_id: self.principal_id.clone(),
                    action: action.action,
                    arguments: action.arguments,
                },
            )
            .await
            .map_err(provider_error)?
            .map_err(|error| provider_reason(format!("logical action failed: {error:?}")))?;
            Ok(AttachProviderAck {
                command_id: Some(action.command_id),
                accepted: matches!(
                    response.result,
                    bmux_cluster_plugin_api::cluster_types::ControlCommandResult::Accepted { .. }
                ),
                message: None,
            })
        })
    }

    fn detach(&mut self) -> AttachSessionFuture<'_, AttachDetachOutcome> {
        let already = std::mem::replace(&mut self.detached, true);
        // Workers stop pushing once their connections close.
        self.links.links.clear();
        Box::pin(async move {
            Ok(if already {
                AttachDetachOutcome::AlreadyDetached
            } else {
                AttachDetachOutcome::Detached
            })
        })
    }
}

fn select_cluster_tab<'a>(
    tabs: &[&'a bmux_cluster_plugin_api::cluster_types::LogicalTabRecord],
    current: &bmux_cluster_plugin_api::cluster_types::LogicalTabId,
    action: &str,
    arguments: &[String],
) -> Result<&'a bmux_cluster_plugin_api::cluster_types::LogicalTabRecord, AttachSessionError> {
    if tabs.is_empty() {
        return Err(provider_reason("logical workspace has no tabs".to_string()));
    }
    let current_index = tabs
        .iter()
        .position(|tab| tab.tab_id == *current)
        .ok_or_else(|| provider_reason("current logical tab no longer exists".to_string()))?;
    match action {
        "tab-next" => Ok(tabs[(current_index + 1) % tabs.len()]),
        "tab-prev" => Ok(tabs[(current_index + tabs.len() - 1) % tabs.len()]),
        "tab-goto" => {
            let target = arguments
                .iter()
                .find(|argument| !argument.starts_with('-'))
                .ok_or_else(|| provider_reason("tab-goto requires a target".to_string()))?;
            if let Ok(index) = target.parse::<usize>() {
                return index
                    .checked_sub(1)
                    .and_then(|index| tabs.get(index).copied())
                    .ok_or_else(|| {
                        provider_reason(format!(
                            "logical tab index {index} is out of range for {} tabs",
                            tabs.len()
                        ))
                    });
            }
            if let Ok(id) = target.parse::<uuid::Uuid>() {
                return tabs
                    .iter()
                    .copied()
                    .find(|tab| tab.tab_id.value == id)
                    .ok_or_else(|| provider_reason(format!("logical tab {id} was not found")));
            }
            let matches = tabs
                .iter()
                .copied()
                .filter(|tab| tab.name.as_deref() == Some(target.as_str()))
                .collect::<Vec<_>>();
            match matches.as_slice() {
                [tab] => Ok(*tab),
                [] => Err(provider_reason(format!(
                    "logical tab '{target}' was not found"
                ))),
                _ => Err(provider_reason(format!(
                    "logical tab name '{target}' is ambiguous"
                ))),
            }
        }
        _ => Err(provider_reason(format!(
            "unsupported logical tab action '{action}'"
        ))),
    }
}

fn zoom_cluster_scene(
    scene: &bmux_attach_layout_protocol::AttachScene,
    pane_id: uuid::Uuid,
    viewport: (u16, u16),
) -> Result<bmux_attach_layout_protocol::AttachScene, AttachSessionError> {
    let target = scene
        .surfaces
        .iter()
        .find(|surface| surface.id == pane_id && surface.visible && surface.accepts_input)
        .ok_or_else(|| provider_reason("logical zoom target is unavailable".to_string()))?;
    let mut zoomed = scene.clone();
    for surface in &mut zoomed.surfaces {
        let selected = surface.id == target.id;
        surface.visible = selected;
        surface.accepts_input = selected;
        surface.cursor_owner = selected;
        if selected {
            surface.rect = bmux_attach_layout_protocol::AttachRect {
                x: 0,
                y: 0,
                w: viewport.0,
                h: viewport.1,
            };
            surface.content_rect = surface.rect;
        }
    }
    zoomed.focus = bmux_attach_layout_protocol::AttachFocusTarget::Pane { pane_id };
    Ok(zoomed)
}

async fn resize_cluster_scene_workers(
    client: &mut BmuxClient,
    workspace_id: uuid::Uuid,
    streams: &[AttachStreamCursor],
    scene: &bmux_attach_layout_protocol::AttachScene,
    principal_id: &str,
    action_command_id: &str,
) -> Result<(), AttachSessionError> {
    for (cursor, cols, rows) in provider_stream_viewports(scene, streams)? {
        if !cursor.stream_id.as_str().starts_with("execution:") {
            continue;
        }
        cluster_attach_command::client::resize(
            client,
            bmux_cluster_plugin_api::cluster_types::AttachResizeRequest {
                workspace_id: bmux_cluster_plugin_api::cluster_types::WorkspaceId {
                    value: workspace_id,
                },
                command_id: bmux_cluster_plugin_api::cluster_types::CommandId {
                    value: action_stream_command_id(action_command_id, &cursor.stream_id),
                },
                execution_id: stream_execution_id(&cursor.stream_id)?,
                generation: cursor.generation,
                principal_id: principal_id.to_string(),
                cols,
                rows,
            },
        )
        .await
        .map_err(provider_error)?
        .map_err(|error| provider_reason(format!("worker resize failed: {error:?}")))?;
    }
    Ok(())
}

async fn resize_cluster_layout_workers(
    client: &mut BmuxClient,
    layout: &bmux_cluster_plugin_api::cluster_types::AttachLayout,
    principal_id: &str,
    action_command_id: &str,
) -> Result<(), AttachSessionError> {
    for pane in &layout.panes {
        let Some(assignment) = pane.execution.as_ref() else {
            continue;
        };
        if pane.availability != bmux_cluster_plugin_api::cluster_types::PaneAvailability::Ready {
            continue;
        }
        let rect = layout
            .rects
            .iter()
            .find(|rect| rect.pane_id == pane.pane_id)
            .ok_or_else(|| provider_reason("logical layout is missing a pane rect".to_string()))?;
        let stream_id =
            AttachStreamId::new(format!("execution:{}", assignment.execution_id.value))?;
        cluster_attach_command::client::resize(
            client,
            bmux_cluster_plugin_api::cluster_types::AttachResizeRequest {
                workspace_id: layout.workspace_id.clone(),
                command_id: bmux_cluster_plugin_api::cluster_types::CommandId {
                    value: action_stream_command_id(action_command_id, &stream_id),
                },
                execution_id: assignment.execution_id.clone(),
                generation: assignment.generation,
                principal_id: principal_id.to_string(),
                cols: rect.width.max(1),
                rows: rect.height.max(1),
            },
        )
        .await
        .map_err(provider_error)?
        .map_err(|error| provider_reason(format!("worker resize failed: {error:?}")))?;
    }
    Ok(())
}

fn provider_stream_viewports(
    scene: &bmux_attach_layout_protocol::AttachScene,
    streams: &[AttachStreamCursor],
) -> Result<Vec<(AttachStreamCursor, u16, u16)>, AttachSessionError> {
    streams
        .iter()
        .map(|cursor| {
            let surface = scene
                .surfaces
                .iter()
                .find(|surface| surface.id == cursor.surface_id)
                .ok_or_else(|| {
                    provider_reason("resized layout dropped an active stream".to_string())
                })?;
            Ok((
                cursor.clone(),
                surface.content_rect.w.max(1),
                surface.content_rect.h.max(1),
            ))
        })
        .collect()
}

fn cluster_scene_from_layout(
    layout: &bmux_cluster_plugin_api::cluster_types::AttachLayout,
    preferred_focus: Option<uuid::Uuid>,
) -> Result<bmux_attach_layout_protocol::AttachScene, AttachSessionError> {
    let focus_pane = preferred_focus
        .filter(|focused| {
            layout.panes.iter().any(|pane| {
                pane.pane_id.value == *focused
                    && pane.availability
                        == bmux_cluster_plugin_api::cluster_types::PaneAvailability::Ready
                    && pane.execution.is_some()
            })
        })
        .or_else(|| {
            layout.panes.iter().find_map(|pane| {
                (pane.availability
                    == bmux_cluster_plugin_api::cluster_types::PaneAvailability::Ready
                    && pane.execution.is_some())
                .then_some(pane.pane_id.value)
            })
        });
    let mut surfaces = Vec::with_capacity(layout.panes.len());
    for (index, pane) in layout.panes.iter().enumerate() {
        let rect = layout
            .rects
            .iter()
            .find(|rect| rect.pane_id == pane.pane_id)
            .ok_or_else(|| {
                provider_reason(format!(
                    "logical layout is missing pane {}",
                    pane.pane_id.value
                ))
            })?;
        let accepts_input = pane.execution.is_some()
            && pane.availability == bmux_cluster_plugin_api::cluster_types::PaneAvailability::Ready;
        surfaces.push(bmux_attach_layout_protocol::AttachSurface {
            id: pane.pane_id.value,
            kind: bmux_attach_layout_protocol::AttachSurfaceKind::Pane,
            layer: bmux_attach_layout_protocol::AttachLayer::Pane,
            z: i32::try_from(index).unwrap_or(i32::MAX),
            rect: bmux_attach_layout_protocol::AttachRect {
                x: rect.x,
                y: rect.y,
                w: rect.width,
                h: rect.height,
            },
            content_rect: bmux_attach_layout_protocol::AttachRect {
                x: rect.x,
                y: rect.y,
                w: rect.width,
                h: rect.height,
            },
            interactive_regions: Vec::new(),
            opaque: true,
            visible: true,
            accepts_input,
            cursor_owner: focus_pane == Some(pane.pane_id.value),
            pane_id: Some(pane.pane_id.value),
        });
    }
    Ok(bmux_attach_layout_protocol::AttachScene {
        session_id: layout.workspace_id.value,
        focus: focus_pane.map_or(
            bmux_attach_layout_protocol::AttachFocusTarget::None,
            |pane_id| bmux_attach_layout_protocol::AttachFocusTarget::Pane { pane_id },
        ),
        surfaces,
    })
}

fn validate_unique_snapshot_streams(
    layout: &bmux_cluster_plugin_api::cluster_types::AttachLayout,
) -> Result<(), AttachSessionError> {
    let mut executions = std::collections::BTreeSet::new();
    for pane in &layout.panes {
        if let Some(assignment) = &pane.execution
            && !executions.insert(assignment.execution_id.value)
        {
            return Err(provider_reason(format!(
                "logical snapshot assigns execution {} to multiple panes",
                assignment.execution_id.value
            )));
        }
    }
    Ok(())
}

/// `layout`'s snapshot: each ready pane's screen from its stream's replica (hydrated), each
/// other pane's status as a screen of its own.
fn assemble_cluster_snapshot(
    layout: &bmux_cluster_plugin_api::cluster_types::AttachLayout,
    replicas: &mut BTreeMap<AttachStreamId, StreamReplica>,
) -> Result<AttachProviderSnapshot, AttachSessionError> {
    if layout.panes.is_empty() {
        return Err(provider_reason(
            "workspace has no logical panes".to_string(),
        ));
    }
    validate_unique_snapshot_streams(layout)?;
    let mut streams = Vec::new();
    let mut focus_pane = None;
    for pane in &layout.panes {
        let assignment = pane.execution.as_ref().filter(|_| {
            pane.availability == bmux_cluster_plugin_api::cluster_types::PaneAvailability::Ready
        });
        if let Some(assignment) = assignment {
            let stream_id =
                AttachStreamId::new(format!("execution:{}", assignment.execution_id.value))?;
            let replica = replicas
                .get_mut(&stream_id)
                .filter(|replica| replica.hydrated())
                .ok_or_else(|| {
                    provider_reason(format!(
                        "screen is missing for ready pane {}",
                        pane.pane_id.value
                    ))
                })?;
            if replica.generation != assignment.generation {
                return Err(provider_reason(format!(
                    "screen of pane {} is of generation {}, not {}",
                    pane.pane_id.value, replica.generation, assignment.generation
                )));
            }
            streams.push(replica.snapshot(stream_id, pane.pane_id.value));
            focus_pane.get_or_insert(pane.pane_id.value);
        } else {
            let rect = layout
                .rects
                .iter()
                .find(|rect| rect.pane_id == pane.pane_id)
                .ok_or_else(|| {
                    provider_reason(format!(
                        "logical layout is missing pane {}",
                        pane.pane_id.value
                    ))
                })?;
            streams.push(status_stream(pane, rect)?);
        }
    }
    let scene = cluster_scene_from_layout(layout, focus_pane)?;
    let resume = AttachResumeState {
        view_revision: AttachViewRevision(layout.control_revision),
        event_sequence: AttachDeltaSequence(0),
        streams: streams.iter().map(|stream| stream.cursor.clone()).collect(),
        provider_token: layout.control_revision.to_be_bytes().to_vec(),
    };
    Ok(AttachProviderSnapshot {
        view_revision: resume.view_revision,
        event_sequence: resume.event_sequence,
        scene,
        streams,
        resume,
    })
}

/// A pane without a running execution, shown as a screen of its status at the pane's size.
fn status_stream(
    pane: &bmux_cluster_plugin_api::cluster_types::LogicalPaneRecord,
    rect: &bmux_cluster_plugin_api::cluster_types::AttachLayoutRect,
) -> Result<AttachStreamSnapshot, AttachSessionError> {
    let message = format!(
        "\r\n  pane {} is {:?}{}\r\n",
        pane.name.as_deref().unwrap_or("unnamed"),
        pane.availability,
        pane.availability_reason
            .as_deref()
            .map_or_else(String::new, |reason| format!(": {reason}"))
    );
    let mut grid = bmux_terminal_grid::TerminalGridStream::new(
        rect.width.max(1),
        rect.height.max(1),
        bmux_terminal_grid::GridLimits::default(),
    )
    .map_err(|error| provider_reason(format!("status grid failed: {error}")))?;
    grid.process(message.as_bytes());
    let mut frame = Vec::new();
    let next = grid
        .grid_mut()
        .write_frame_since(bmux_terminal_grid::ReplicaCursor::default(), 0, &mut frame)
        .next;
    Ok(AttachStreamSnapshot {
        cursor: AttachStreamCursor {
            stream_id: AttachStreamId::new(format!("status:{}", pane.pane_id.value))?,
            surface_id: pane.pane_id.value,
            generation: 0,
            offset: next.epoch,
        },
        sync_update_active: false,
        frame,
    })
}

/// A snapshot stream as the reset frame that brings a renderer's replica to it.
fn stream_reset_change(stream: &AttachStreamSnapshot) -> bmux_client::AttachProviderChange {
    bmux_client::AttachProviderChange::StreamFrame(bmux_client::AttachStreamFrame {
        cursor: stream.cursor.clone(),
        end_offset: stream.cursor.offset,
        reset: true,
        sync_update_active: stream.sync_update_active,
        frame: stream.frame.clone(),
    })
}

fn cluster_reconciliation_delta(
    base_view_revision: AttachViewRevision,
    view_revision: AttachViewRevision,
    event_sequence: AttachDeltaSequence,
    control_revision: u64,
    previous_streams: &[AttachStreamCursor],
    snapshot: &AttachProviderSnapshot,
) -> bmux_client::AttachProviderDelta {
    let mut changes = previous_streams
        .iter()
        .map(|cursor| bmux_client::AttachProviderChange::StreamRemoved {
            stream_id: cursor.stream_id.clone(),
        })
        .collect::<Vec<_>>();
    changes.push(bmux_client::AttachProviderChange::Scene(
        snapshot.scene.clone(),
    ));
    changes.extend(snapshot.streams.iter().map(stream_reset_change));
    let streams = snapshot
        .streams
        .iter()
        .map(|stream| stream.cursor.clone())
        .collect::<Vec<_>>();
    bmux_client::AttachProviderDelta {
        sequence: event_sequence,
        base_view_revision,
        view_revision,
        changes,
        resume: AttachResumeState {
            view_revision,
            event_sequence,
            streams,
            provider_token: control_revision.to_be_bytes().to_vec(),
        },
    }
}

fn apply_local_focus_action(
    scene: &bmux_attach_layout_protocol::AttachScene,
    arguments: &[String],
) -> Result<Option<bmux_attach_layout_protocol::AttachScene>, AttachSessionError> {
    let direction = arguments.first().ok_or_else(|| {
        provider_reason("logical focus requires a direction or pane ID".to_string())
    })?;
    if let Ok(target) = direction.parse::<uuid::Uuid>() {
        if !scene
            .surfaces
            .iter()
            .any(|surface| surface.id == target && surface.visible && surface.accepts_input)
        {
            return Err(provider_reason(
                "logical focus target is unavailable".to_string(),
            ));
        }
        let mut replacement = scene.clone();
        set_scene_focus(&mut replacement, target);
        return Ok(Some(replacement));
    }
    let focused = match scene.focus {
        bmux_attach_layout_protocol::AttachFocusTarget::Pane { pane_id } => pane_id,
        bmux_attach_layout_protocol::AttachFocusTarget::Surface { surface_id } => surface_id,
        bmux_attach_layout_protocol::AttachFocusTarget::None => {
            return Ok(scene
                .surfaces
                .iter()
                .find(|surface| surface.accepts_input)
                .map(|surface| {
                    let mut replacement = scene.clone();
                    set_scene_focus(&mut replacement, surface.id);
                    replacement
                }));
        }
    };
    let target = match direction.as_str() {
        "next" | "prev" => sequential_focus_target(scene, focused, direction == "next"),
        "left" | "right" | "up" | "down" => directional_focus_target(scene, focused, direction),
        _ => {
            return Err(provider_reason(format!(
                "unsupported logical focus direction '{direction}'"
            )));
        }
    };
    Ok(target.filter(|target| *target != focused).map(|target| {
        let mut replacement = scene.clone();
        set_scene_focus(&mut replacement, target);
        replacement
    }))
}

fn set_scene_focus(scene: &mut bmux_attach_layout_protocol::AttachScene, pane_id: uuid::Uuid) {
    scene.focus = bmux_attach_layout_protocol::AttachFocusTarget::Pane { pane_id };
    for surface in &mut scene.surfaces {
        surface.cursor_owner = surface.id == pane_id;
    }
}

fn focusable_surfaces(
    scene: &bmux_attach_layout_protocol::AttachScene,
) -> Vec<&bmux_attach_layout_protocol::AttachSurface> {
    let mut surfaces = scene
        .surfaces
        .iter()
        .filter(|surface| surface.visible && surface.accepts_input)
        .collect::<Vec<_>>();
    surfaces.sort_by_key(|surface| (surface.rect.y, surface.rect.x, surface.id));
    surfaces
}

fn sequential_focus_target(
    scene: &bmux_attach_layout_protocol::AttachScene,
    focused: uuid::Uuid,
    next: bool,
) -> Option<uuid::Uuid> {
    let surfaces = focusable_surfaces(scene);
    let index = surfaces.iter().position(|surface| surface.id == focused)?;
    let target = if next {
        index.saturating_add(1) % surfaces.len()
    } else {
        index
            .checked_sub(1)
            .unwrap_or_else(|| surfaces.len().saturating_sub(1))
    };
    Some(surfaces[target].id)
}

fn directional_focus_target(
    scene: &bmux_attach_layout_protocol::AttachScene,
    focused: uuid::Uuid,
    direction: &str,
) -> Option<uuid::Uuid> {
    let surfaces = focusable_surfaces(scene);
    let current = surfaces.iter().find(|surface| surface.id == focused)?.rect;
    surfaces
        .into_iter()
        .filter(|candidate| candidate.id != focused)
        .filter_map(|candidate| {
            directional_distance(current, candidate.rect, direction)
                .map(|distance| (distance, candidate.rect.y, candidate.rect.x, candidate.id))
        })
        .min()
        .map(|(_, _, _, pane_id)| pane_id)
}

fn directional_distance(
    current: bmux_attach_layout_protocol::AttachRect,
    candidate: bmux_attach_layout_protocol::AttachRect,
    direction: &str,
) -> Option<(u32, u32)> {
    let center = |origin: u16, length: u16| {
        u32::from(origin)
            .saturating_mul(2)
            .saturating_add(u32::from(length))
    };
    let current_x = center(current.x, current.w);
    let current_y = center(current.y, current.h);
    let candidate_x = center(candidate.x, candidate.w);
    let candidate_y = center(candidate.y, candidate.h);
    let (primary, secondary) = match direction {
        "left" if candidate_x < current_x => {
            (current_x - candidate_x, current_y.abs_diff(candidate_y))
        }
        "right" if candidate_x > current_x => {
            (candidate_x - current_x, current_y.abs_diff(candidate_y))
        }
        "up" if candidate_y < current_y => {
            (current_y - candidate_y, current_x.abs_diff(candidate_x))
        }
        "down" if candidate_y > current_y => {
            (candidate_y - current_y, current_x.abs_diff(candidate_x))
        }
        _ => return None,
    };
    Some((primary, secondary))
}

/// The snapshot as a resumption of `resume`, which must name the same view: the same control
/// revision and the same streams at the same generations on the same surfaces. Stream offsets
/// are not compared: they are epochs of this session's replicas, and a snapshot carries every
/// stream's screen whole.
fn initial_snapshot_for_resume(
    mut built: AttachProviderSnapshot,
    control_revision: u64,
    resume: Option<AttachResumeState>,
) -> Result<AttachProviderSnapshot, AttachSessionError> {
    let Some(resume) = resume else {
        return Ok(built);
    };
    let same_streams = resume.streams.len() == built.streams.len()
        && resume
            .streams
            .iter()
            .zip(&built.streams)
            .all(|(resumed, stream)| {
                resumed.stream_id == stream.cursor.stream_id
                    && resumed.surface_id == stream.cursor.surface_id
                    && resumed.generation == stream.cursor.generation
            });
    if !same_streams || resume.provider_token != control_revision.to_be_bytes() {
        return Err(provider_reason(
            "resume descriptor does not match current cluster state".to_string(),
        ));
    }
    built.view_revision = resume.view_revision;
    built.event_sequence = resume.event_sequence;
    built.resume = AttachResumeState {
        view_revision: resume.view_revision,
        event_sequence: resume.event_sequence,
        streams: built
            .streams
            .iter()
            .map(|stream| stream.cursor.clone())
            .collect(),
        provider_token: resume.provider_token,
    };
    Ok(built)
}

fn encode_cluster_key(
    stroke: &bmux_keyboard::KeyStroke,
    enhanced: bool,
    protocol: bmux_terminal_grid::ProtocolState,
) -> Result<Vec<u8>, AttachSessionError> {
    bmux_keyboard::encode::encode_key_with_modes(
        stroke,
        enhanced,
        bmux_keyboard::encode::KeyEncodingModes {
            application_cursor: protocol.application_cursor,
            application_keypad: protocol.application_keypad,
        },
    )
    .ok_or_else(|| provider_reason("key cannot be encoded for worker terminal".to_string()))
}

fn encode_cluster_paste(data: Vec<u8>, protocol: bmux_terminal_grid::ProtocolState) -> Vec<u8> {
    if !protocol.bracketed_paste {
        return data;
    }
    let mut encoded = Vec::with_capacity(data.len().saturating_add(12));
    encoded.extend_from_slice(b"\x1b[200~");
    encoded.extend_from_slice(&data);
    encoded.extend_from_slice(b"\x1b[201~");
    encoded
}

fn encode_cluster_mouse(
    mouse: bmux_client::AttachMouseInput,
    protocol: bmux_terminal_grid::ProtocolState,
) -> Result<Vec<u8>, AttachSessionError> {
    use bmux_attach_pipeline::{
        AttachMouseButton as Button, AttachMouseEvent as Event, AttachMouseEventKind as EventKind,
        AttachMouseModifiers as Modifiers, AttachPaneMouseProtocol as PaneProtocol,
    };
    let button = match mouse.button {
        bmux_client::AttachMouseButton::Left
        | bmux_client::AttachMouseButton::WheelUp
        | bmux_client::AttachMouseButton::WheelDown
        | bmux_client::AttachMouseButton::None => Button::Left,
        bmux_client::AttachMouseButton::Middle => Button::Middle,
        bmux_client::AttachMouseButton::Right => Button::Right,
    };
    let kind = match (mouse.button, mouse.phase) {
        (bmux_client::AttachMouseButton::WheelUp, _) => EventKind::ScrollUp,
        (bmux_client::AttachMouseButton::WheelDown, _) => EventKind::ScrollDown,
        (_, bmux_client::AttachMousePhase::Press) => EventKind::Down(button),
        (_, bmux_client::AttachMousePhase::Release) => EventKind::Up(button),
        (_, bmux_client::AttachMousePhase::Drag) => EventKind::Drag(button),
        (_, bmux_client::AttachMousePhase::Move) => EventKind::Moved,
        (_, bmux_client::AttachMousePhase::Scroll) => {
            return Err(provider_reason(
                "mouse scroll has no wheel direction".to_string(),
            ));
        }
    };
    bmux_attach_pipeline::mouse::encode_for_protocol(
        Event {
            kind,
            column: mouse.x,
            row: mouse.y,
            modifiers: Modifiers {
                shift: mouse.modifiers & 1 != 0,
                control: mouse.modifiers & 2 != 0,
                alt: mouse.modifiers & 4 != 0,
            },
        },
        PaneProtocol {
            mode: bmux_attach_pipeline::mouse_protocol_mode_to_ipc(protocol.mouse_mode()),
            encoding: bmux_attach_pipeline::mouse_protocol_encoding_to_ipc(
                protocol.mouse_encoding(),
            ),
        },
    )
    .ok_or_else(|| provider_reason("mouse event is disabled by terminal protocol mode".to_string()))
}

fn action_stream_command_id(action_command_id: &str, stream_id: &AttachStreamId) -> uuid::Uuid {
    use sha2::Digest;
    let mut digest = sha2::Sha256::new();
    digest.update(b"bmux.cluster.attach-action-stream.v1\0");
    digest.update(action_command_id.as_bytes());
    digest.update(b"\0");
    digest.update(stream_id.as_str().as_bytes());
    let bytes: [u8; 16] = digest.finalize()[..16]
        .try_into()
        .expect("SHA-256 prefix length");
    uuid::Uuid::from_bytes(bytes)
}

fn command_id_from_text(value: &str) -> uuid::Uuid {
    use sha2::Digest;
    let mut digest = sha2::Sha256::new();
    digest.update(b"bmux.cluster.attach-action.v1\0");
    digest.update(value.as_bytes());
    let bytes: [u8; 16] = digest.finalize()[..16]
        .try_into()
        .expect("SHA-256 prefix length");
    uuid::Uuid::from_bytes(bytes)
}

fn command_id(
    sequence: u64,
    stream_id: &AttachStreamId,
) -> bmux_cluster_plugin_api::cluster_types::CommandId {
    use sha2::Digest;
    let mut digest = sha2::Sha256::new();
    digest.update(b"bmux.cluster.attach-command.v1\0");
    digest.update(sequence.to_be_bytes());
    digest.update(stream_id.as_str().as_bytes());
    let bytes: [u8; 16] = digest.finalize()[..16]
        .try_into()
        .expect("SHA-256 prefix length");
    bmux_cluster_plugin_api::cluster_types::CommandId {
        value: uuid::Uuid::from_bytes(bytes),
    }
}

fn stream_execution_id(
    stream_id: &AttachStreamId,
) -> Result<bmux_cluster_plugin_api::cluster_types::ExecutionId, AttachSessionError> {
    let value = stream_id
        .as_str()
        .strip_prefix("execution:")
        .ok_or_else(|| provider_reason("invalid cluster stream identity".to_string()))?
        .parse::<uuid::Uuid>()
        .map_err(|error| provider_reason(format!("invalid execution stream UUID: {error}")))?;
    Ok(bmux_cluster_plugin_api::cluster_types::ExecutionId { value })
}

fn provider_error(error: impl std::fmt::Display) -> AttachSessionError {
    provider_reason(error.to_string())
}
const fn provider_reason(reason: String) -> AttachSessionError {
    AttachSessionError::Provider { reason }
}

/// The plugin-owned cluster attach adapter. The host registers it into its
/// own attach-provider registry only when the cluster plugin is enabled.
#[must_use]
pub fn attach_provider() -> Arc<dyn bmux_client::AttachProvider> {
    Arc::new(ClusterAttachProvider)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn action_command_ids_are_stable_and_separated() {
        assert_eq!(
            command_id_from_text("action-1"),
            command_id_from_text("action-1")
        );
        assert_ne!(
            command_id_from_text("action-1"),
            command_id_from_text("action-2")
        );
    }

    #[test]
    fn logical_resize_uses_each_surface_content_geometry() {
        use bmux_attach_layout_protocol::{
            AttachFocusTarget, AttachLayer, AttachRect, AttachScene, AttachSurface,
            AttachSurfaceKind,
        };
        let surface = |id: u128, x: u16, width: u16| AttachSurface {
            id: uuid::Uuid::from_u128(id),
            kind: AttachSurfaceKind::Pane,
            layer: AttachLayer::Pane,
            z: 0,
            rect: AttachRect {
                x,
                y: 0,
                w: width,
                h: 20,
            },
            content_rect: AttachRect {
                x,
                y: 1,
                w: width,
                h: 18,
            },
            interactive_regions: Vec::new(),
            opaque: true,
            visible: true,
            accepts_input: true,
            cursor_owner: id == 1,
            pane_id: Some(uuid::Uuid::from_u128(id)),
        };
        let scene = AttachScene {
            session_id: uuid::Uuid::nil(),
            focus: AttachFocusTarget::Pane {
                pane_id: uuid::Uuid::from_u128(1),
            },
            surfaces: vec![surface(1, 0, 30), surface(2, 30, 50)],
        };
        let streams = [
            AttachStreamCursor {
                stream_id: AttachStreamId::new("one").unwrap(),
                surface_id: uuid::Uuid::from_u128(1),
                generation: 1,
                offset: 0,
            },
            AttachStreamCursor {
                stream_id: AttachStreamId::new("two").unwrap(),
                surface_id: uuid::Uuid::from_u128(2),
                generation: 1,
                offset: 0,
            },
        ];
        let viewports = provider_stream_viewports(&scene, &streams).unwrap();
        assert_eq!((viewports[0].1, viewports[0].2), (30, 18));
        assert_eq!((viewports[1].1, viewports[1].2), (50, 18));
    }

    #[test]
    fn logical_focus_navigation_is_deterministic_and_preserves_scene_geometry() {
        use bmux_attach_layout_protocol::{
            AttachFocusTarget, AttachLayer, AttachRect, AttachScene, AttachSurface,
            AttachSurfaceKind,
        };
        let pane = |id: u128, x: u16, y: u16| AttachSurface {
            id: uuid::Uuid::from_u128(id),
            kind: AttachSurfaceKind::Pane,
            layer: AttachLayer::Pane,
            z: 0,
            rect: AttachRect { x, y, w: 10, h: 5 },
            content_rect: AttachRect { x, y, w: 10, h: 5 },
            interactive_regions: Vec::new(),
            opaque: true,
            visible: true,
            accepts_input: true,
            cursor_owner: id == 1,
            pane_id: Some(uuid::Uuid::from_u128(id)),
        };
        let scene = AttachScene {
            session_id: uuid::Uuid::nil(),
            focus: AttachFocusTarget::Pane {
                pane_id: uuid::Uuid::from_u128(1),
            },
            surfaces: vec![pane(1, 0, 0), pane(2, 10, 0), pane(3, 0, 5)],
        };

        let right = apply_local_focus_action(&scene, &["right".to_string()])
            .unwrap()
            .unwrap();
        assert_eq!(
            right.focus,
            AttachFocusTarget::Pane {
                pane_id: uuid::Uuid::from_u128(2)
            }
        );
        assert_eq!(right.surfaces[0].rect, scene.surfaces[0].rect);
        assert!(right.surfaces[1].cursor_owner);
        assert!(!right.surfaces[0].cursor_owner);

        let previous = apply_local_focus_action(&scene, &["prev".to_string()])
            .unwrap()
            .unwrap();
        assert_eq!(
            previous.focus,
            AttachFocusTarget::Pane {
                pane_id: uuid::Uuid::from_u128(3)
            }
        );

        let direct = apply_local_focus_action(&scene, &[uuid::Uuid::from_u128(3).to_string()])
            .unwrap()
            .unwrap();
        assert_eq!(direct.focus, previous.focus);
        assert!(
            apply_local_focus_action(&scene, &["up".to_string()])
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn logical_tab_navigation_is_deterministic_and_rejects_ambiguity() {
        let workspace_id = bmux_cluster_plugin_api::cluster_types::WorkspaceId {
            value: uuid::Uuid::from_u128(1),
        };
        let tab = |id: u128, name: &str| bmux_cluster_plugin_api::cluster_types::LogicalTabRecord {
            tab_id: bmux_cluster_plugin_api::cluster_types::LogicalTabId {
                value: uuid::Uuid::from_u128(id),
            },
            workspace_id: workspace_id.clone(),
            name: Some(name.to_string()),
            layout_schema_version: 1,
            layout: Vec::new(),
            revision: 1,
        };
        let first = tab(1, "one");
        let second = tab(2, "duplicate");
        let third = tab(3, "duplicate");
        let tabs = vec![&first, &second, &third];
        assert_eq!(
            select_cluster_tab(&tabs, &first.tab_id, "tab-next", &[])
                .unwrap()
                .tab_id,
            second.tab_id
        );
        assert_eq!(
            select_cluster_tab(&tabs, &first.tab_id, "tab-prev", &[])
                .unwrap()
                .tab_id,
            third.tab_id
        );
        assert_eq!(
            select_cluster_tab(&tabs, &first.tab_id, "tab-goto", &["2".to_string()])
                .unwrap()
                .tab_id,
            second.tab_id
        );
        assert!(
            select_cluster_tab(&tabs, &first.tab_id, "tab-goto", &["duplicate".to_string()])
                .is_err()
        );
    }

    #[test]
    fn logical_zoom_projects_one_pane_without_mutating_base_scene() {
        use bmux_attach_layout_protocol::{
            AttachFocusTarget, AttachLayer, AttachRect, AttachScene, AttachSurface,
            AttachSurfaceKind,
        };
        let pane = |id: u128, x: u16| AttachSurface {
            id: uuid::Uuid::from_u128(id),
            kind: AttachSurfaceKind::Pane,
            layer: AttachLayer::Pane,
            z: 0,
            rect: AttachRect {
                x,
                y: 0,
                w: 40,
                h: 24,
            },
            content_rect: AttachRect {
                x,
                y: 0,
                w: 40,
                h: 24,
            },
            interactive_regions: Vec::new(),
            opaque: true,
            visible: true,
            accepts_input: true,
            cursor_owner: id == 1,
            pane_id: Some(uuid::Uuid::from_u128(id)),
        };
        let scene = AttachScene {
            session_id: uuid::Uuid::nil(),
            focus: AttachFocusTarget::Pane {
                pane_id: uuid::Uuid::from_u128(1),
            },
            surfaces: vec![pane(1, 0), pane(2, 40)],
        };
        let zoomed = zoom_cluster_scene(&scene, uuid::Uuid::from_u128(2), (80, 24)).unwrap();
        assert!(scene.surfaces.iter().all(|surface| surface.visible));
        assert!(!zoomed.surfaces[0].visible);
        assert!(zoomed.surfaces[1].visible);
        assert_eq!(
            zoomed.surfaces[1].rect,
            AttachRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24
            }
        );
        assert_eq!(
            zoomed.focus,
            AttachFocusTarget::Pane {
                pane_id: uuid::Uuid::from_u128(2)
            }
        );
    }

    fn layout_with(
        panes: Vec<(
            uuid::Uuid,
            bmux_cluster_plugin_api::cluster_types::PaneAvailability,
            Option<bmux_cluster_plugin_api::cluster_types::ExecutionAssignment>,
            Option<String>,
        )>,
        control_revision: u64,
    ) -> bmux_cluster_plugin_api::cluster_types::AttachLayout {
        let workspace_id = bmux_cluster_plugin_api::cluster_types::WorkspaceId {
            value: uuid::Uuid::from_u128(1),
        };
        let tab_id = bmux_cluster_plugin_api::cluster_types::LogicalTabId {
            value: uuid::Uuid::from_u128(2),
        };
        let mut x = 0;
        let mut rects = Vec::new();
        let mut records = Vec::new();
        for (id, availability, execution, reason) in panes {
            rects.push(bmux_cluster_plugin_api::cluster_types::AttachLayoutRect {
                pane_id: bmux_cluster_plugin_api::cluster_types::LogicalPaneId { value: id },
                x,
                y: 0,
                width: 30,
                height: 20,
            });
            x += 30;
            records.push(bmux_cluster_plugin_api::cluster_types::LogicalPaneRecord {
                pane_id: bmux_cluster_plugin_api::cluster_types::LogicalPaneId { value: id },
                workspace_id: workspace_id.clone(),
                tab_id: tab_id.clone(),
                name: Some(format!("pane-{id}")),
                restart_policy: bmux_cluster_plugin_api::cluster_types::PaneRestartPolicy::Manual,
                placement: bmux_cluster_plugin_api::cluster_types::PlacementIntent {
                    explicit_node_id: None,
                    required_labels: Vec::new(),
                    preferred_labels: Vec::new(),
                },
                availability,
                availability_reason: reason,
                execution,
                revision: 9,
            });
        }
        bmux_cluster_plugin_api::cluster_types::AttachLayout {
            workspace_id,
            tab_id,
            control_revision,
            panes: records,
            rects,
        }
    }

    fn assignment(
        execution: u128,
        generation: u64,
    ) -> bmux_cluster_plugin_api::cluster_types::ExecutionAssignment {
        bmux_cluster_plugin_api::cluster_types::ExecutionAssignment {
            node_id: "node-a".to_string(),
            generation,
            execution_id: bmux_cluster_plugin_api::cluster_types::ExecutionId {
                value: uuid::Uuid::from_u128(execution),
            },
        }
    }

    fn execution_stream(execution: u128) -> AttachStreamId {
        AttachStreamId::new(format!("execution:{}", uuid::Uuid::from_u128(execution))).unwrap()
    }

    /// A worker's pane: its grid, and where the session's replica of it stands as the worker
    /// sees it.
    struct WorkerPane {
        grid: bmux_terminal_grid::TerminalGridStream,
        observer: bmux_terminal_grid::ReplicaCursor,
    }

    impl WorkerPane {
        fn new(cols: u16, rows: u16) -> Self {
            Self {
                grid: bmux_terminal_grid::TerminalGridStream::new(
                    cols,
                    rows,
                    bmux_terminal_grid::GridLimits::default(),
                )
                .unwrap(),
                observer: bmux_terminal_grid::ReplicaCursor::default(),
            }
        }

        /// The frame the worker pushes now, as the pane runtime builds it.
        fn push(&mut self) -> Vec<u8> {
            let mut frame = Vec::new();
            self.observer = self
                .grid
                .grid_mut()
                .write_frame_since(self.observer, 0, &mut frame)
                .next;
            frame
        }
    }

    /// A grid's screen: its whole snapshot but history, which the session does not relay (it
    /// observes with no history lines), and the parser's unfinished input (replicas do not parse).
    fn screen(grid: &bmux_terminal_grid::TerminalGrid) -> bmux_terminal_grid::GridSnapshot {
        let mut snapshot = grid.snapshot(0, grid.height());
        snapshot.pending_bytes.clear();
        snapshot.scrollback_rows = 0;
        snapshot.history_truncated = None;
        snapshot
    }

    fn rendered(
        state: &mut bmux_frontend_runtime::AttachState,
        surface: uuid::Uuid,
    ) -> bmux_terminal_grid::GridSnapshot {
        screen(state.buffers_mut()[&surface].terminal_grid.grid())
    }

    #[test]
    fn assembles_selected_logical_layout_with_ready_and_unavailable_panes() {
        let ready_id = uuid::Uuid::from_u128(3);
        let unavailable_id = uuid::Uuid::from_u128(4);
        let layout = layout_with(
            vec![
                (
                    ready_id,
                    bmux_cluster_plugin_api::cluster_types::PaneAvailability::Ready,
                    Some(assignment(5, 7)),
                    None,
                ),
                (
                    unavailable_id,
                    bmux_cluster_plugin_api::cluster_types::PaneAvailability::Unavailable,
                    None,
                    Some("worker offline".to_string()),
                ),
            ],
            11,
        );
        let mut worker = WorkerPane::new(30, 20);
        worker.grid.process(b"ready output");
        let mut replicas = BTreeMap::from([(execution_stream(5), StreamReplica::new(7).unwrap())]);
        assert!(
            assemble_cluster_snapshot(&layout, &mut replicas).is_err(),
            "a ready pane needs its screen"
        );
        replicas
            .get_mut(&execution_stream(5))
            .unwrap()
            .apply_upstream(&worker.push(), false)
            .unwrap();
        let built = assemble_cluster_snapshot(&layout, &mut replicas).unwrap();

        assert_eq!(built.view_revision, AttachViewRevision(11));
        assert_eq!(built.scene.surfaces.len(), 2);
        assert_eq!(
            built.scene.focus,
            bmux_attach_layout_protocol::AttachFocusTarget::Pane { pane_id: ready_id }
        );
        assert_eq!(built.streams.len(), 2);
        assert_eq!(built.streams[0].cursor.stream_id, execution_stream(5));
        assert_eq!(built.streams[0].cursor.generation, 7);
        assert_eq!(
            built.streams[1].cursor.stream_id.as_str(),
            format!("status:{unavailable_id}")
        );
        assert_eq!(built.resume.streams.len(), 2);
        let mut state = bmux_frontend_runtime::AttachState::from_snapshot(&built).unwrap();
        assert_eq!(rendered(&mut state, ready_id), screen(worker.grid.grid()));
        let status = rendered(&mut state, unavailable_id);
        assert_eq!((status.width, status.height), (30, 20));
        assert!(
            bmux_terminal_grid::visible_text(
                state.buffers_mut()[&unavailable_id].terminal_grid.grid(),
                0,
                20
            )
            .contains("worker offline")
        );

        // A replica of another generation is not this pane's screen.
        let mut replicas = BTreeMap::from([(execution_stream(5), StreamReplica::new(6).unwrap())]);
        replicas
            .get_mut(&execution_stream(5))
            .unwrap()
            .apply_upstream(&WorkerPane::new(30, 20).push(), false)
            .unwrap();
        assert!(assemble_cluster_snapshot(&layout, &mut replicas).is_err());
    }

    /// One ready pane's stream as a session relays it to a renderer.
    struct Relayed {
        pane: uuid::Uuid,
        stream: AttachStreamId,
        worker: WorkerPane,
        replicas: BTreeMap<AttachStreamId, StreamReplica>,
        streams: Vec<AttachStreamCursor>,
        state: bmux_frontend_runtime::AttachState,
        delta: bmux_client::AttachProviderDelta,
    }

    impl Relayed {
        /// A session hydrated from `worker`'s first frame, its snapshot rendered.
        fn hydrate(worker: WorkerPane, generation: u64) -> Self {
            let pane = uuid::Uuid::from_u128(3);
            let stream = execution_stream(5);
            let mut relayed = Self {
                pane,
                stream: stream.clone(),
                worker,
                replicas: BTreeMap::from([(stream, StreamReplica::new(generation).unwrap())]),
                streams: Vec::new(),
                state: bmux_frontend_runtime::AttachState::from_snapshot(&AttachProviderSnapshot {
                    view_revision: AttachViewRevision(1),
                    event_sequence: AttachDeltaSequence(0),
                    scene: bmux_attach_layout_protocol::AttachScene {
                        session_id: uuid::Uuid::nil(),
                        focus: bmux_attach_layout_protocol::AttachFocusTarget::None,
                        surfaces: Vec::new(),
                    },
                    streams: Vec::new(),
                    resume: AttachResumeState {
                        view_revision: AttachViewRevision(1),
                        ..AttachResumeState::default()
                    },
                })
                .unwrap(),
                delta: bmux_client::AttachProviderDelta {
                    sequence: AttachDeltaSequence(0),
                    base_view_revision: AttachViewRevision(1),
                    view_revision: AttachViewRevision(1),
                    changes: Vec::new(),
                    resume: AttachResumeState::default(),
                },
            };
            relayed.push(false);
            let snapshot = relayed.snapshot(generation, 11);
            adopt_snapshot_replicas(&mut relayed.replicas, &snapshot);
            relayed.state = bmux_frontend_runtime::AttachState::from_snapshot(&snapshot).unwrap();
            relayed.streams = snapshot.resume.streams.clone();
            relayed.delta.view_revision = snapshot.view_revision;
            relayed.delta.resume = snapshot.resume;
            relayed
        }

        /// A snapshot of the pane's layout at `generation` and control `revision`.
        fn snapshot(&mut self, generation: u64, revision: u64) -> AttachProviderSnapshot {
            assemble_cluster_snapshot(
                &layout_with(
                    vec![(
                        self.pane,
                        bmux_cluster_plugin_api::cluster_types::PaneAvailability::Ready,
                        Some(assignment(5, generation)),
                        None,
                    )],
                    revision,
                ),
                &mut self.replicas,
            )
            .unwrap()
        }

        /// The worker pushes a frame; the session applies it.
        fn push(&mut self, sync_update_active: bool) {
            let frame = self.worker.push();
            self.replicas
                .get_mut(&self.stream)
                .unwrap()
                .apply_upstream(&frame, sync_update_active)
                .unwrap();
        }

        /// The session relays its changed stream as one delta; the renderer applies it.
        fn deliver(&mut self) -> bmux_client::AttachStreamFrame {
            let changes = relay_frames(
                &mut self.streams,
                &mut self.replicas,
                &BTreeSet::from([self.stream.clone()]),
            );
            let sequence = AttachDeltaSequence(self.delta.sequence.0 + 1);
            self.delta = bmux_client::AttachProviderDelta {
                sequence,
                base_view_revision: self.delta.view_revision,
                view_revision: self.delta.view_revision,
                changes,
                resume: AttachResumeState {
                    event_sequence: sequence,
                    streams: self.streams.clone(),
                    ..self.delta.resume.clone()
                },
            };
            self.state.apply_delta(&self.delta).unwrap();
            let [bmux_client::AttachProviderChange::StreamFrame(frame)] =
                self.delta.changes.as_slice()
            else {
                panic!("expected one stream frame: {:?}", self.delta.changes);
            };
            frame.clone()
        }

        fn assert_level(&mut self) {
            assert_eq!(
                rendered(&mut self.state, self.pane),
                screen(self.worker.grid.grid())
            );
        }
    }

    /// Worker frames, applied to the session's replica and relayed, keep the renderer's screen
    /// equal to the worker's across output and a resize (reset frames).
    #[test]
    fn worker_frames_relay_through_the_session_to_the_renderer() {
        let mut worker = WorkerPane::new(30, 20);
        worker
            .grid
            .process(b"\x1b[?1049h\x1b[?2004hhello\r\nworld\x1b[?1049l\r\nmain");
        let mut relayed = Relayed::hydrate(worker, 7);
        relayed.assert_level();
        assert!(
            relayed.replicas[&relayed.stream]
                .grid
                .protocol_state()
                .bracketed_paste
        );

        // Output: several pushes go out as one incremental frame.
        for line in 0..30 {
            relayed
                .worker
                .grid
                .process(format!("line {line}\r\n").as_bytes());
            if line % 7 == 0 {
                relayed.push(false);
            }
        }
        relayed.push(true);
        let frame = relayed.deliver();
        assert!(!frame.reset);
        assert!(frame.sync_update_active);
        relayed.assert_level();

        // A resize: the worker's reset frame reaches the renderer as one.
        relayed.worker.grid.resize(50, 12).unwrap();
        relayed.worker.grid.process(b"resized");
        relayed.push(false);
        assert!(relayed.deliver().reset);
        let shown = rendered(&mut relayed.state, relayed.pane);
        assert_eq!((shown.width, shown.height), (50, 12));
        relayed.assert_level();

        // A snapshot built for a view the renderer never takes leaves its replica where it was.
        relayed.snapshot(7, 12);
        relayed
            .worker
            .grid
            .process(b"\r\nafter an abandoned snapshot");
        relayed.push(false);
        assert!(!relayed.deliver().reset);
        relayed.assert_level();

        // A frame the replica cannot follow is refused and changes nothing.
        let mut elsewhere = WorkerPane::new(50, 12);
        elsewhere.push();
        elsewhere.grid.process(b"other");
        let replica = relayed.replicas.get_mut(&relayed.stream).unwrap();
        assert!(replica.apply_upstream(&elsewhere.push(), false).is_err());
        assert_eq!(screen(&replica.grid), screen(relayed.worker.grid.grid()));
    }

    /// A new generation replaces the stream with the new execution's screen, which then relays.
    #[test]
    fn a_new_generation_replaces_the_relayed_stream() {
        let mut relayed = Relayed::hydrate(WorkerPane::new(30, 20), 7);
        relayed.worker.grid.process(b"old");
        relayed.push(false);
        relayed.deliver();

        let mut restarted = WorkerPane::new(30, 20);
        restarted.grid.process(b"restarted");
        let mut next = Relayed::hydrate(restarted, 8);
        let rebuilt = next.snapshot(8, 12);
        let delta = cluster_reconciliation_delta(
            relayed.delta.view_revision,
            AttachViewRevision(relayed.delta.view_revision.0 + 1),
            AttachDeltaSequence(relayed.delta.sequence.0 + 1),
            12,
            &relayed.streams,
            &rebuilt,
        );
        relayed.state.apply_delta(&delta).unwrap();
        adopt_snapshot_replicas(&mut next.replicas, &rebuilt);
        // The renderer now follows the new generation's replica.
        next.state = relayed.state;
        next.streams.clone_from(&delta.resume.streams);
        next.delta = delta;
        assert_eq!(next.state.resume_state().streams[0].generation, 8);
        next.assert_level();
        next.worker.grid.process(b"\r\nmore");
        next.push(false);
        assert!(!next.deliver().reset);
        next.assert_level();
    }

    #[test]
    fn a_new_replica_takes_only_a_reset_frame() {
        let mut worker = WorkerPane::new(20, 4);
        worker.push();
        worker.grid.process(b"later");
        let mut replica = StreamReplica::new(1).unwrap();
        assert!(!replica.hydrated());
        assert!(replica.apply_upstream(&worker.push(), false).is_err());
        assert!(!replica.hydrated());
        worker.observer = bmux_terminal_grid::ReplicaCursor::default();
        replica.apply_upstream(&worker.push(), false).unwrap();
        assert!(replica.hydrated());
    }

    fn authority(execution: u128, generation: u64) -> WorkerAuthority {
        WorkerAuthority {
            cluster_id: "cluster".to_string(),
            workspace_id: bmux_cluster_plugin_api::cluster_types::WorkspaceId {
                value: uuid::Uuid::from_u128(1),
            },
            pane_id: bmux_cluster_plugin_api::cluster_types::LogicalPaneId {
                value: uuid::Uuid::from_u128(execution + 100),
            },
            execution_id: bmux_cluster_plugin_api::cluster_types::ExecutionId {
                value: uuid::Uuid::from_u128(execution),
            },
            generation,
            control_term: 1,
            lease_sequence: 1,
            operation_class: bmux_cluster_plugin_api::cluster_types::WorkerOperationClass::Observe,
            principal_id: "principal:test".to_string(),
            issuer_node_id: "ingress".to_string(),
            audience_node_id: "worker".to_string(),
            lease_id: uuid::Uuid::from_u128(execution + 200),
            lease_issued_at_unix_ms: 0,
            lease_duration_ms: 5_000,
            lease_signature: Vec::new(),
        }
    }

    fn route(execution: u128, generation: u64, endpoint: &str) -> WorkerRoute {
        WorkerRoute {
            execution_id: bmux_cluster_plugin_api::cluster_types::ExecutionId {
                value: uuid::Uuid::from_u128(execution),
            },
            generation,
            node_id: "worker".to_string(),
            endpoint: endpoint.to_string(),
            authority: authority(execution, generation),
        }
    }

    fn execution_ref(execution: u128, generation: u64) -> (AttachStreamId, ExecutionRef) {
        (
            execution_stream(execution),
            ExecutionRef {
                execution_id: bmux_cluster_plugin_api::cluster_types::ExecutionId {
                    value: uuid::Uuid::from_u128(execution),
                },
                generation,
            },
        )
    }

    #[test]
    fn worker_routes_are_grouped_by_endpoint_and_fenced_to_requested_generations() {
        let executions = [
            execution_ref(1, 4),
            execution_ref(2, 9),
            execution_ref(3, 1),
        ];
        let plan = route_plan(
            &executions,
            vec![route(1, 4, "a"), route(2, 9, "b"), route(3, 1, "a")],
        )
        .unwrap();
        assert_eq!(plan.len(), 2);
        assert_eq!(
            plan["a"]
                .iter()
                .map(|target| target.stream_id.clone())
                .collect::<Vec<_>>(),
            vec![execution_stream(1), execution_stream(3)]
        );
        assert_eq!(plan["b"][0].authority, authority(2, 9));

        let fenced = |routes: Vec<WorkerRoute>| route_plan(&executions, routes).is_err();
        assert!(fenced(vec![route(1, 4, "a"), route(2, 9, "b")]), "missing");
        assert!(
            fenced(vec![route(1, 4, "a"), route(2, 8, "b"), route(3, 1, "a")]),
            "stale generation"
        );
        let mut mismatched = route(2, 9, "b");
        mismatched.authority = authority(2, 8);
        assert!(
            fenced(vec![route(1, 4, "a"), mismatched, route(3, 1, "a")]),
            "lease for another generation"
        );
        assert!(
            fenced(vec![route(1, 4, "a"), route(1, 4, "b"), route(3, 1, "a")]),
            "duplicate"
        );
        assert!(
            fenced(vec![route(1, 4, "a"), route(7, 9, "b"), route(3, 1, "a")]),
            "unrequested"
        );
    }

    #[test]
    fn observed_panes_must_be_exactly_the_requested_executions() {
        let executions = [execution_ref(1, 4), execution_ref(2, 9)];
        let plan = route_plan(&executions, vec![route(1, 4, "a"), route(2, 9, "a")]).unwrap();
        let targets = &plan["a"];
        let observed = |execution: u128, generation: u64, pane: u128| ObservedExecution {
            execution_id: bmux_cluster_plugin_api::cluster_types::ExecutionId {
                value: uuid::Uuid::from_u128(execution),
            },
            generation,
            session_id: uuid::Uuid::from_u128(50),
            pane_id: uuid::Uuid::from_u128(pane),
        };
        let panes = observed_panes(targets, vec![observed(2, 9, 20), observed(1, 4, 10)]).unwrap();
        assert_eq!(
            panes,
            BTreeMap::from([
                (uuid::Uuid::from_u128(10), execution_stream(1)),
                (uuid::Uuid::from_u128(20), execution_stream(2)),
            ])
        );
        assert!(observed_panes(targets, vec![observed(1, 4, 10)]).is_err());
        assert!(observed_panes(targets, vec![observed(1, 4, 10), observed(2, 8, 20)]).is_err());
        assert!(observed_panes(targets, vec![observed(1, 4, 10), observed(1, 4, 20)]).is_err());
        assert!(observed_panes(targets, vec![observed(1, 4, 10), observed(2, 9, 10)]).is_err());
    }

    #[test]
    fn cluster_resume_uses_attach_revision_while_fencing_control_revision() {
        let stream = AttachStreamSnapshot {
            cursor: AttachStreamCursor {
                stream_id: AttachStreamId::new("execution:test").unwrap(),
                surface_id: uuid::Uuid::from_u128(1),
                generation: 2,
                offset: 8,
            },
            sync_update_active: false,
            frame: b"state".to_vec(),
        };
        let scene = bmux_attach_layout_protocol::AttachScene {
            session_id: uuid::Uuid::from_u128(9),
            focus: bmux_attach_layout_protocol::AttachFocusTarget::None,
            surfaces: vec![bmux_attach_layout_protocol::AttachSurface {
                id: uuid::Uuid::from_u128(1),
                kind: bmux_attach_layout_protocol::AttachSurfaceKind::Pane,
                layer: bmux_attach_layout_protocol::AttachLayer::Pane,
                z: 0,
                rect: bmux_attach_layout_protocol::AttachRect {
                    x: 0,
                    y: 0,
                    w: 1,
                    h: 1,
                },
                content_rect: bmux_attach_layout_protocol::AttachRect {
                    x: 0,
                    y: 0,
                    w: 1,
                    h: 1,
                },
                interactive_regions: Vec::new(),
                opaque: true,
                visible: true,
                accepts_input: false,
                cursor_owner: false,
                pane_id: Some(uuid::Uuid::from_u128(1)),
            }],
        };
        let built = AttachProviderSnapshot {
            view_revision: AttachViewRevision(90),
            event_sequence: AttachDeltaSequence(0),
            scene,
            streams: vec![stream.clone()],
            resume: AttachResumeState::default(),
        };
        let resume = AttachResumeState {
            view_revision: AttachViewRevision(7),
            event_sequence: AttachDeltaSequence(12),
            streams: vec![stream.cursor],
            provider_token: 90_u64.to_be_bytes().to_vec(),
        };
        assert!(
            initial_snapshot_for_resume(built.clone(), 91, Some(resume.clone())).is_err(),
            "the control revision moved on"
        );
        let mut other_generation = resume.clone();
        other_generation.streams[0].generation = 3;
        assert!(initial_snapshot_for_resume(built.clone(), 90, Some(other_generation)).is_err());
        // Offsets are this session's replica epochs: a resumed view takes the snapshot's.
        let mut earlier_offset = resume.clone();
        earlier_offset.streams[0].offset = 2;
        let resumed = initial_snapshot_for_resume(built, 90, Some(earlier_offset)).unwrap();
        assert_eq!(resumed.view_revision, AttachViewRevision(7));
        assert_eq!(resumed.event_sequence, AttachDeltaSequence(12));
        assert_eq!(resumed.resume, resume);
    }

    #[test]
    fn reconciliation_delta_atomically_replaces_scene_and_stream_generations() {
        let old_stream = AttachStreamCursor {
            stream_id: AttachStreamId::new("execution:old").unwrap(),
            surface_id: uuid::Uuid::from_u128(1),
            generation: 4,
            offset: 20,
        };
        let new_stream = AttachStreamSnapshot {
            cursor: AttachStreamCursor {
                stream_id: AttachStreamId::new("execution:new").unwrap(),
                surface_id: uuid::Uuid::from_u128(1),
                generation: 5,
                offset: 3,
            },
            sync_update_active: false,
            frame: b"new".to_vec(),
        };
        let scene = bmux_attach_layout_protocol::AttachScene {
            session_id: uuid::Uuid::from_u128(2),
            focus: bmux_attach_layout_protocol::AttachFocusTarget::Pane {
                pane_id: uuid::Uuid::from_u128(1),
            },
            surfaces: vec![bmux_attach_layout_protocol::AttachSurface {
                id: uuid::Uuid::from_u128(1),
                kind: bmux_attach_layout_protocol::AttachSurfaceKind::Pane,
                layer: bmux_attach_layout_protocol::AttachLayer::Pane,
                z: 0,
                rect: bmux_attach_layout_protocol::AttachRect {
                    x: 0,
                    y: 0,
                    w: 80,
                    h: 24,
                },
                content_rect: bmux_attach_layout_protocol::AttachRect {
                    x: 0,
                    y: 0,
                    w: 80,
                    h: 24,
                },
                interactive_regions: Vec::new(),
                opaque: true,
                visible: true,
                accepts_input: true,
                cursor_owner: true,
                pane_id: Some(uuid::Uuid::from_u128(1)),
            }],
        };
        let snapshot = AttachProviderSnapshot {
            view_revision: AttachViewRevision(90),
            event_sequence: AttachDeltaSequence(0),
            scene: scene.clone(),
            streams: vec![new_stream.clone()],
            resume: AttachResumeState::default(),
        };
        let delta = cluster_reconciliation_delta(
            AttachViewRevision(7),
            AttachViewRevision(8),
            AttachDeltaSequence(12),
            90,
            std::slice::from_ref(&old_stream),
            &snapshot,
        );

        assert_eq!(delta.base_view_revision, AttachViewRevision(7));
        assert_eq!(delta.view_revision, AttachViewRevision(8));
        assert_eq!(delta.sequence, AttachDeltaSequence(12));
        assert_eq!(
            delta.changes,
            vec![
                bmux_client::AttachProviderChange::StreamRemoved {
                    stream_id: old_stream.stream_id.clone(),
                },
                bmux_client::AttachProviderChange::Scene(scene.clone()),
                super::stream_reset_change(&new_stream),
            ]
        );
        assert_eq!(delta.resume.streams, vec![new_stream.cursor]);
        assert_eq!(delta.resume.provider_token, 90_u64.to_be_bytes());

        let initial = AttachProviderSnapshot {
            view_revision: AttachViewRevision(7),
            event_sequence: AttachDeltaSequence(11),
            scene,
            streams: vec![AttachStreamSnapshot {
                cursor: old_stream.clone(),
                sync_update_active: false,
                frame: Vec::new(),
            }],
            resume: AttachResumeState {
                view_revision: AttachViewRevision(7),
                event_sequence: AttachDeltaSequence(11),
                streams: vec![old_stream],
                provider_token: 80_u64.to_be_bytes().to_vec(),
            },
        };
        let mut validator = bmux_client::AttachContinuityValidator::default();
        validator.apply_snapshot(&initial).unwrap();
        validator.apply_delta(&delta).unwrap();
        assert_eq!(validator.resume_state(), delta.resume);
    }

    #[test]
    fn logical_snapshot_rejects_duplicate_execution_streams() {
        let assignment = bmux_cluster_plugin_api::cluster_types::ExecutionAssignment {
            node_id: "node-a".to_string(),
            generation: 1,
            execution_id: bmux_cluster_plugin_api::cluster_types::ExecutionId {
                value: uuid::Uuid::from_u128(20),
            },
        };
        let pane = |id: u128| bmux_cluster_plugin_api::cluster_types::LogicalPaneRecord {
            pane_id: bmux_cluster_plugin_api::cluster_types::LogicalPaneId {
                value: uuid::Uuid::from_u128(id),
            },
            workspace_id: bmux_cluster_plugin_api::cluster_types::WorkspaceId {
                value: uuid::Uuid::from_u128(1),
            },
            tab_id: bmux_cluster_plugin_api::cluster_types::LogicalTabId {
                value: uuid::Uuid::from_u128(2),
            },
            name: None,
            restart_policy: bmux_cluster_plugin_api::cluster_types::PaneRestartPolicy::Manual,
            placement: bmux_cluster_plugin_api::cluster_types::PlacementIntent {
                explicit_node_id: None,
                required_labels: Vec::new(),
                preferred_labels: Vec::new(),
            },
            availability: bmux_cluster_plugin_api::cluster_types::PaneAvailability::Ready,
            availability_reason: None,
            execution: Some(assignment.clone()),
            revision: 1,
        };
        let layout = bmux_cluster_plugin_api::cluster_types::AttachLayout {
            workspace_id: bmux_cluster_plugin_api::cluster_types::WorkspaceId {
                value: uuid::Uuid::from_u128(1),
            },
            tab_id: bmux_cluster_plugin_api::cluster_types::LogicalTabId {
                value: uuid::Uuid::from_u128(2),
            },
            control_revision: 1,
            panes: vec![pane(3), pane(4)],
            rects: Vec::new(),
        };
        assert!(validate_unique_snapshot_streams(&layout).is_err());
    }

    #[test]
    fn cluster_key_encoding_tracks_application_cursor_mode() {
        let stroke = bmux_keyboard::KeyStroke {
            key: bmux_keyboard::KeyCode::Up,
            modifiers: bmux_keyboard::Modifiers::NONE,
        };
        assert_eq!(
            encode_cluster_key(&stroke, false, bmux_terminal_grid::ProtocolState::default())
                .unwrap(),
            b"\x1b[A"
        );
        assert_eq!(
            encode_cluster_key(
                &stroke,
                false,
                bmux_terminal_grid::ProtocolState {
                    application_cursor: true,
                    ..bmux_terminal_grid::ProtocolState::default()
                }
            )
            .unwrap(),
            b"\x1bOA"
        );
    }

    #[test]
    fn cluster_paste_encoding_tracks_bracketed_paste_mode() {
        let plain = encode_cluster_paste(
            b"input".to_vec(),
            bmux_terminal_grid::ProtocolState::default(),
        );
        assert_eq!(plain, b"input");
        let bracketed = encode_cluster_paste(
            b"input".to_vec(),
            bmux_terminal_grid::ProtocolState {
                bracketed_paste: true,
                ..bmux_terminal_grid::ProtocolState::default()
            },
        );
        assert_eq!(bracketed, b"\x1b[200~input\x1b[201~");
    }

    #[test]
    fn cluster_mouse_encoding_tracks_terminal_protocol_modes() {
        let mouse = bmux_client::AttachMouseInput {
            x: 4,
            y: 2,
            button: bmux_client::AttachMouseButton::Left,
            phase: bmux_client::AttachMousePhase::Press,
            modifiers: 0,
        };
        assert!(encode_cluster_mouse(mouse, bmux_terminal_grid::ProtocolState::default()).is_err());
        let encoded = encode_cluster_mouse(
            mouse,
            bmux_terminal_grid::ProtocolState {
                mouse_x10: true,
                mouse_sgr: true,
                ..bmux_terminal_grid::ProtocolState::default()
            },
        )
        .unwrap();
        assert_eq!(encoded, b"\x1b[<0;5;3M");
    }

    #[test]
    fn cluster_provider_registers_with_generic_registry() {
        let registry = bmux_client::AttachProviderRegistry::new();
        let provider = std::sync::Arc::new(ClusterAttachProvider);
        assert_eq!(provider.id(), CLUSTER_PROVIDER_ID);
        assert!(provider.supports(&AttachTarget::parse("cluster://prod/main")));
        assert!(!provider.supports(&AttachTarget::parse("local://main")));
        let registry = std::sync::Arc::new(registry);
        let registration = registry.register(provider).unwrap();
        assert_eq!(
            registry
                .resolve(&AttachTarget::parse("cluster://prod/main"))
                .unwrap()
                .id(),
            CLUSTER_PROVIDER_ID
        );
        drop(registration);
        assert!(matches!(
            registry.resolve(&AttachTarget::parse("cluster://prod/main")),
            Err(bmux_client::AttachProviderResolutionError::NoProvider { .. })
        ));
    }

    #[test]
    fn cluster_provider_claims_and_validates_cluster_uris() {
        let provider = ClusterAttachProvider;
        assert!(!provider.requires_fallback_client());
        assert!(provider.supports(&AttachTarget::parse("cluster://prod/workspace")));
        assert!(!provider.supports(&AttachTarget::parse("local://workspace")));
        let resolved = provider
            .resolve(&AttachTarget::parse("cluster://prod/workspace"))
            .unwrap();
        let target = resolved
            .as_any()
            .downcast_ref::<ClusterAttachTarget>()
            .unwrap();
        assert_eq!(target.cluster, "prod");
        assert_eq!(target.workspace, "workspace");
        assert!(
            provider
                .resolve(&AttachTarget::parse("cluster://prod"))
                .is_err()
        );
        assert!(
            provider
                .resolve(&AttachTarget::parse("cluster:///workspace"))
                .is_err()
        );
    }

    #[test]
    fn cluster_stream_identity_round_trips_execution_uuid() {
        let id = uuid::Uuid::from_u128(42);
        let stream = AttachStreamId::new(format!("execution:{id}")).unwrap();
        assert_eq!(stream_execution_id(&stream).unwrap().value, id);
        assert!(stream_execution_id(&AttachStreamId::new("other:42").unwrap()).is_err());
    }
}
