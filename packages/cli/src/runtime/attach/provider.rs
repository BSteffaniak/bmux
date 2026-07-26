use anyhow::Result;
use bmux_client::{
    AttachDeltaSequence, AttachDetachOutcome, AttachProvider, AttachProviderAck,
    AttachProviderAction, AttachProviderBackend, AttachProviderError, AttachProviderFuture,
    AttachProviderInput, AttachProviderSession, AttachProviderSnapshot, AttachProviderViewport,
    AttachResumeState, AttachSession, AttachSessionError, AttachSessionFuture, AttachStreamCursor,
    AttachStreamId, AttachStreamSnapshot, AttachTarget, AttachViewRevision, BmuxClient,
    ResolvedAttachTarget, global_attach_provider_registry,
};
use bmux_cluster_plugin_api::{
    cluster_attach_command, cluster_attach_state, cluster_control_state,
};
use std::any::Any;
use std::sync::{Arc, OnceLock};

const PROVIDER_ID: &str = "bmux.pane-runtime";
const CLUSTER_PROVIDER_ID: &str = "bmux.cluster";

#[derive(Debug)]
struct ClusterAttachProvider;

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
        true
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
        fallback_client: Option<BmuxClient>,
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
            let client = fallback_client.ok_or_else(|| AttachProviderError::OpenFailed {
                provider_id: CLUSTER_PROVIDER_ID.to_string(),
                reason: "cluster provider requires an ingress client".to_string(),
            })?;
            let principal_id = format!("principal:{}", client.principal_id());
            let session = ClusterAttachSession::open(
                client,
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

struct ClusterSnapshotBuild {
    snapshot: AttachProviderSnapshot,
    protocols:
        std::collections::BTreeMap<AttachStreamId, bmux_terminal_grid::TerminalProtocolTracker>,
    grids: std::collections::BTreeMap<AttachStreamId, bmux_terminal_grid::TerminalGridStream>,
}

struct DecodedClusterTerminalSnapshot {
    cursor: u64,
    stream: bmux_terminal_grid::TerminalGridStream,
}

struct ClusterAttachSession {
    client: BmuxClient,
    cluster: String,
    workspace_id: uuid::Uuid,
    window_id: bmux_cluster_plugin_api::cluster_types::LogicalWindowId,
    principal_id: String,
    snapshot: Option<AttachProviderSnapshot>,
    view_revision: AttachViewRevision,
    control_revision: u64,
    event_sequence: AttachDeltaSequence,
    streams: Vec<AttachStreamCursor>,
    protocols:
        std::collections::BTreeMap<AttachStreamId, bmux_terminal_grid::TerminalProtocolTracker>,
    grids: std::collections::BTreeMap<AttachStreamId, bmux_terminal_grid::TerminalGridStream>,
    scene: bmux_attach_layout_protocol::AttachScene,
    pending_events: std::collections::VecDeque<bmux_client::AttachProviderEvent>,
    unzoomed_scene: Option<bmux_attach_layout_protocol::AttachScene>,
    zoomed_pane: Option<uuid::Uuid>,
    viewport: (u16, u16),
    next_reconcile_at: tokio::time::Instant,
    detached: bool,
}

impl std::fmt::Debug for ClusterAttachSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClusterAttachSession")
            .field("cluster", &self.cluster)
            .field("workspace_id", &self.workspace_id)
            .field("window_id", &self.window_id)
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
        let (layout, built) =
            build_consistent_cluster_snapshot(&mut client, layout, (80, 24)).await?;
        let built = initial_snapshot_for_resume(built, layout.control_revision, resume)?;
        let streams = built
            .snapshot
            .streams
            .iter()
            .map(|stream| stream.cursor.clone())
            .collect();
        let view_revision = built.snapshot.view_revision;
        let event_sequence = built.snapshot.event_sequence;
        let scene = built.snapshot.scene.clone();
        Ok(Self {
            client,
            cluster,
            workspace_id,
            window_id: layout.window_id.clone(),
            principal_id,
            snapshot: Some(built.snapshot),
            view_revision,
            control_revision: layout.control_revision,
            event_sequence,
            streams,
            protocols: built.protocols,
            grids: built.grids,
            scene,
            pending_events: std::collections::VecDeque::new(),
            unzoomed_scene: None,
            zoomed_pane: None,
            viewport: (80, 24),
            next_reconcile_at: tokio::time::Instant::now(),
            detached: false,
        })
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
        let (layout, mut built) =
            build_consistent_cluster_snapshot(&mut self.client, layout, self.viewport).await?;
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
        resize_cluster_scene_workers(
            &mut self.client,
            self.workspace_id,
            &built
                .snapshot
                .streams
                .iter()
                .map(|stream| stream.cursor.clone())
                .collect::<Vec<_>>(),
            &scene,
            &self.principal_id,
            &reconciliation_id,
        )
        .await?;
        built.snapshot.scene = scene;
        reflow_cluster_build_to_scene(&mut built)?;

        let base_view_revision = self.view_revision;
        self.view_revision = AttachViewRevision(self.view_revision.0.saturating_add(1));
        self.event_sequence = AttachDeltaSequence(self.event_sequence.0.saturating_add(1));
        self.control_revision = layout.control_revision;
        self.window_id = layout.window_id;
        let delta = cluster_reconciliation_delta(
            base_view_revision,
            self.view_revision,
            self.event_sequence,
            self.control_revision,
            &self.streams,
            &built.snapshot,
        );

        self.streams = built
            .snapshot
            .streams
            .iter()
            .map(|stream| stream.cursor.clone())
            .collect();
        self.protocols = built.protocols;
        self.grids = built.grids;
        self.scene = built.snapshot.scene;
        self.unzoomed_scene = next_unzoomed_scene;
        self.zoomed_pane = next_zoomed_pane;
        Ok(bmux_client::AttachProviderEvent::Delta(delta))
    }
    async fn replace_attached_window(
        &mut self,
        window_id: bmux_cluster_plugin_api::cluster_types::LogicalWindowId,
        action_command_id: &str,
    ) -> Result<(), AttachSessionError> {
        if window_id == self.window_id {
            return Ok(());
        }
        let layout = cluster_attach_state::client::layout(
            &mut self.client,
            bmux_cluster_plugin_api::cluster_types::WorkspaceId {
                value: self.workspace_id,
            },
            Some(window_id),
            self.viewport.0,
            self.viewport.1,
        )
        .await
        .map_err(provider_error)?
        .map_err(|error| provider_reason(format!("logical window selection failed: {error:?}")))?;
        if layout.control_revision < self.control_revision {
            return Err(provider_reason(format!(
                "cluster control revision regressed from {} to {} during window selection",
                self.control_revision, layout.control_revision
            )));
        }
        let (layout, built) =
            build_consistent_cluster_snapshot(&mut self.client, layout, self.viewport).await?;
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
        self.control_revision = layout.control_revision;
        self.window_id = layout.window_id;
        let delta = cluster_reconciliation_delta(
            base_view_revision,
            self.view_revision,
            self.event_sequence,
            self.control_revision,
            &self.streams,
            &built.snapshot,
        );
        self.streams = built
            .snapshot
            .streams
            .iter()
            .map(|stream| stream.cursor.clone())
            .collect();
        self.protocols = built.protocols;
        self.grids = built.grids;
        self.scene = built.snapshot.scene;
        self.pending_events
            .push_back(bmux_client::AttachProviderEvent::Delta(delta));
        self.unzoomed_scene = None;
        self.zoomed_pane = None;
        Ok(())
    }

    async fn select_window(
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
                "cluster control revision regressed from {} to {} during window navigation",
                self.control_revision, view.revision
            )));
        }
        let mut windows = view
            .windows
            .iter()
            .filter(|window| window.workspace_id.value == self.workspace_id)
            .collect::<Vec<_>>();
        windows.sort_by_key(|window| window.window_id.value.as_u128());
        let selected = select_cluster_window(&windows, &self.window_id, action, arguments)?;
        if selected.window_id == self.window_id {
            return Ok(false);
        }
        self.replace_attached_window(selected.window_id.clone(), action_command_id)
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
        resize_cluster_scene_workers(
            &mut self.client,
            self.workspace_id,
            &self.streams,
            &scene,
            &self.principal_id,
            action_command_id,
        )
        .await?;
        for (cursor, cols, rows) in provider_stream_viewports(&scene, &self.streams)? {
            if let Some(grid) = self.grids.get_mut(&cursor.stream_id) {
                grid.resize(cols, rows).map_err(|error| {
                    provider_reason(format!("cluster terminal grid resize failed: {error}"))
                })?;
            }
        }
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
                    resume: AttachResumeState {
                        view_revision: self.view_revision,
                        event_sequence: self.event_sequence,
                        streams: self.streams.clone(),
                        provider_token: self.control_revision.to_be_bytes().to_vec(),
                    },
                },
            ));
    }
}

impl AttachSession for ClusterAttachSession {
    fn initial_snapshot(&mut self) -> AttachSessionFuture<'_, AttachProviderSnapshot> {
        let snapshot = self.snapshot.take();
        Box::pin(async move {
            snapshot.ok_or_else(|| provider_reason("initial snapshot already consumed".to_string()))
        })
    }

    #[allow(clippy::too_many_lines)] // Ordered stream polling and repair must mutate one session state atomically.
    fn next_event(&mut self) -> AttachSessionFuture<'_, bmux_client::AttachProviderEvent> {
        Box::pin(async move {
            if self.detached {
                return Ok(bmux_client::AttachProviderEvent::Detached);
            }
            if let Some(event) = self.pending_events.pop_front() {
                return Ok(event);
            }
            loop {
                if tokio::time::Instant::now() >= self.next_reconcile_at {
                    self.next_reconcile_at =
                        tokio::time::Instant::now() + std::time::Duration::from_millis(100);
                    let layout = cluster_attach_state::client::layout(
                        &mut self.client,
                        bmux_cluster_plugin_api::cluster_types::WorkspaceId {
                            value: self.workspace_id,
                        },
                        Some(self.window_id.clone()),
                        self.viewport.0,
                        self.viewport.1,
                    )
                    .await
                    .map_err(provider_error)?
                    .map_err(|error| {
                        provider_reason(format!("logical layout reconciliation failed: {error:?}"))
                    })?;
                    if layout.control_revision < self.control_revision {
                        return Err(provider_reason(format!(
                            "cluster control revision regressed from {} to {}",
                            self.control_revision, layout.control_revision
                        )));
                    }
                    if layout.control_revision > self.control_revision {
                        return self.reconcile_layout(layout).await;
                    }
                }
                for index in 0..self.streams.len() {
                    let cursor = self.streams[index].clone();
                    if !cursor.stream_id.as_str().starts_with("execution:") {
                        continue;
                    }
                    let execution_id = stream_execution_id(&cursor.stream_id)?;
                    let output = cluster_attach_state::client::output(
                        &mut self.client,
                        bmux_cluster_plugin_api::cluster_types::WorkspaceId {
                            value: self.workspace_id,
                        },
                        execution_id.clone(),
                        cursor.generation,
                        cursor.offset,
                        64 * 1024,
                    )
                    .await
                    .map_err(provider_error)?;
                    let output = match output {
                        Ok(output) => output,
                        Err(
                            bmux_cluster_plugin_api::cluster_types::WorkerServiceError::NotFound {
                                ..
                            }
                            | bmux_cluster_plugin_api::cluster_types::WorkerServiceError::StaleGeneration {
                                ..
                            }
                            | bmux_cluster_plugin_api::cluster_types::WorkerServiceError::Unavailable {
                                ..
                            },
                        ) => {
                            let layout = cluster_attach_state::client::layout(
                                &mut self.client,
                                bmux_cluster_plugin_api::cluster_types::WorkspaceId {
                                    value: self.workspace_id,
                                },
                                Some(self.window_id.clone()),
                                self.viewport.0,
                                self.viewport.1,
                            )
                            .await
                            .map_err(provider_error)?
                            .map_err(|error| {
                                provider_reason(format!(
                                    "logical layout repair after worker transition failed: {error:?}"
                                ))
                            })?;
                            if layout.control_revision > self.control_revision {
                                return self.reconcile_layout(layout).await;
                            }
                            return Err(provider_reason(
                                "worker stream became unavailable without an authoritative control-state transition"
                                    .to_string(),
                            ));
                        }
                        Err(error) => {
                            return Err(provider_reason(format!(
                                "worker output failed: {error:?}"
                            )));
                        }
                    };
                    validate_worker_output(&output, &execution_id, &cursor)?;
                    let change = if output.gap {
                        let terminal = cluster_attach_state::client::snapshot(
                            &mut self.client,
                            bmux_cluster_plugin_api::cluster_types::WorkspaceId {
                                value: self.workspace_id,
                            },
                            execution_id,
                            cursor.generation,
                        )
                        .await
                        .map_err(provider_error)?
                        .map_err(|error| {
                            provider_reason(format!("worker snapshot repair failed: {error:?}"))
                        })?;
                        let decoded = decode_cluster_terminal_snapshot(&terminal)?;
                        let repaired = AttachStreamCursor {
                            generation: terminal.generation,
                            offset: decoded.cursor,
                            ..cursor
                        };
                        self.streams[index] = repaired.clone();
                        let repaint = cluster_terminal_repaint(&decoded.stream);
                        self.protocols.insert(
                            repaired.stream_id.clone(),
                            protocol_tracker_from_snapshot(&decoded),
                        );
                        self.grids
                            .insert(repaired.stream_id.clone(), decoded.stream);
                        bmux_client::AttachProviderChange::StreamRepair(AttachStreamSnapshot {
                            cursor: repaired,
                            snapshot: repaint,
                        })
                    } else if !output.data.is_empty() {
                        let end_offset = output.next_cursor;
                        let appended = AttachStreamCursor {
                            offset: end_offset,
                            ..cursor.clone()
                        };
                        self.streams[index] = appended;
                        let grid = self.grids.get_mut(&cursor.stream_id).ok_or_else(|| {
                            provider_reason(
                                "cluster terminal grid is unavailable for output".to_string(),
                            )
                        })?;
                        grid.process(&output.data);
                        self.protocols
                            .insert(cursor.stream_id.clone(), protocol_tracker_from_grid(grid));
                        bmux_client::AttachProviderChange::StreamAppend {
                            cursor,
                            end_offset,
                            bytes: output.data,
                        }
                    } else {
                        continue;
                    };
                    self.event_sequence =
                        AttachDeltaSequence(self.event_sequence.0.saturating_add(1));
                    let resume = AttachResumeState {
                        view_revision: self.view_revision,
                        event_sequence: self.event_sequence,
                        streams: self.streams.clone(),
                        provider_token: self.control_revision.to_be_bytes().to_vec(),
                    };
                    return Ok(bmux_client::AttachProviderEvent::Delta(
                        bmux_client::AttachProviderDelta {
                            sequence: self.event_sequence,
                            base_view_revision: self.view_revision,
                            view_revision: self.view_revision,
                            changes: vec![change],
                            resume,
                        },
                    ));
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
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
            let data = match input.payload {
                bmux_client::AttachInputPayload::Bytes(data) => data,
                bmux_client::AttachInputPayload::Key { stroke, enhanced } => encode_cluster_key(
                    &stroke,
                    enhanced,
                    self.protocols
                        .get(&input.stream_id)
                        .ok_or_else(|| {
                            provider_reason("key protocol state is unavailable".to_string())
                        })?
                        .protocol_state(),
                )?,
                bmux_client::AttachInputPayload::Paste(data) => encode_cluster_paste(
                    data,
                    self.protocols
                        .get(&input.stream_id)
                        .ok_or_else(|| {
                            provider_reason("paste protocol state is unavailable".to_string())
                        })?
                        .protocol_state(),
                ),
                bmux_client::AttachInputPayload::Mouse(mouse) => encode_cluster_mouse(
                    mouse,
                    self.protocols
                        .get(&input.stream_id)
                        .ok_or_else(|| {
                            provider_reason("mouse protocol state is unavailable".to_string())
                        })?
                        .protocol_state(),
                )?,
            };
            let command_id = command_id(input.command_sequence, &input.stream_id);
            cluster_attach_command::client::input(
                &mut self.client,
                bmux_cluster_plugin_api::cluster_types::WorkspaceId {
                    value: self.workspace_id,
                },
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
            let layout = cluster_attach_state::client::layout(
                &mut self.client,
                bmux_cluster_plugin_api::cluster_types::WorkspaceId {
                    value: self.workspace_id,
                },
                Some(self.window_id.clone()),
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
            for (cursor, cols, rows) in provider_stream_viewports(&scene, &self.streams)? {
                if !cursor.stream_id.as_str().starts_with("execution:") {
                    continue;
                }
                let execution_id = stream_execution_id(&cursor.stream_id)?;
                let resize_id = command_id(viewport.command_sequence, &cursor.stream_id);
                cluster_attach_command::client::resize(
                    &mut self.client,
                    bmux_cluster_plugin_api::cluster_types::AttachResizeRequest {
                        workspace_id: bmux_cluster_plugin_api::cluster_types::WorkspaceId {
                            value: self.workspace_id,
                        },
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
            for (cursor, cols, rows) in provider_stream_viewports(&scene, &self.streams)? {
                if let Some(grid) = self.grids.get_mut(&cursor.stream_id) {
                    grid.resize(cols, rows).map_err(|error| {
                        provider_reason(format!("cluster terminal grid resize failed: {error}"))
                    })?;
                }
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
        if matches!(
            action.action.as_str(),
            "window-next" | "window-prev" | "window-goto"
        ) {
            return Box::pin(async move {
                let changed = self
                    .select_window(&action.action, &action.arguments, &action.command_id)
                    .await?;
                Ok(AttachProviderAck {
                    command_id: Some(action.command_id),
                    accepted: true,
                    message: (!changed).then(|| "logical window did not change".to_string()),
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
        Box::pin(async move {
            Ok(if already {
                AttachDetachOutcome::AlreadyDetached
            } else {
                AttachDetachOutcome::Detached
            })
        })
    }
}

fn reflow_cluster_build_to_scene(
    built: &mut ClusterSnapshotBuild,
) -> Result<(), AttachSessionError> {
    for stream in &mut built.snapshot.streams {
        let Some(grid) = built.grids.get_mut(&stream.cursor.stream_id) else {
            continue;
        };
        let surface = built
            .snapshot
            .scene
            .surfaces
            .iter()
            .find(|surface| surface.id == stream.cursor.surface_id)
            .ok_or_else(|| {
                provider_reason("cluster scene dropped an assembled stream".to_string())
            })?;
        grid.resize(surface.content_rect.w.max(1), surface.content_rect.h.max(1))
            .map_err(|error| provider_reason(format!("cluster grid reflow failed: {error}")))?;
        stream.snapshot = cluster_terminal_repaint(grid);
        built.protocols.insert(
            stream.cursor.stream_id.clone(),
            protocol_tracker_from_grid(grid),
        );
    }
    Ok(())
}

fn select_cluster_window<'a>(
    windows: &[&'a bmux_cluster_plugin_api::cluster_types::LogicalWindowRecord],
    current: &bmux_cluster_plugin_api::cluster_types::LogicalWindowId,
    action: &str,
    arguments: &[String],
) -> Result<&'a bmux_cluster_plugin_api::cluster_types::LogicalWindowRecord, AttachSessionError> {
    if windows.is_empty() {
        return Err(provider_reason(
            "logical workspace has no windows".to_string(),
        ));
    }
    let current_index = windows
        .iter()
        .position(|window| window.window_id == *current)
        .ok_or_else(|| provider_reason("current logical window no longer exists".to_string()))?;
    match action {
        "window-next" => Ok(windows[(current_index + 1) % windows.len()]),
        "window-prev" => Ok(windows[(current_index + windows.len() - 1) % windows.len()]),
        "window-goto" => {
            let target = arguments
                .iter()
                .find(|argument| !argument.starts_with('-'))
                .ok_or_else(|| provider_reason("window-goto requires a target".to_string()))?;
            if let Ok(index) = target.parse::<usize>() {
                return index
                    .checked_sub(1)
                    .and_then(|index| windows.get(index).copied())
                    .ok_or_else(|| {
                        provider_reason(format!(
                            "logical window index {index} is out of range for {} windows",
                            windows.len()
                        ))
                    });
            }
            if let Ok(id) = target.parse::<uuid::Uuid>() {
                return windows
                    .iter()
                    .copied()
                    .find(|window| window.window_id.value == id)
                    .ok_or_else(|| provider_reason(format!("logical window {id} was not found")));
            }
            let matches = windows
                .iter()
                .copied()
                .filter(|window| window.name.as_deref() == Some(target.as_str()))
                .collect::<Vec<_>>();
            match matches.as_slice() {
                [window] => Ok(*window),
                [] => Err(provider_reason(format!(
                    "logical window '{target}' was not found"
                ))),
                _ => Err(provider_reason(format!(
                    "logical window name '{target}' is ambiguous"
                ))),
            }
        }
        _ => Err(provider_reason(format!(
            "unsupported logical window action '{action}'"
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

async fn build_consistent_cluster_snapshot(
    client: &mut BmuxClient,
    mut layout: bmux_cluster_plugin_api::cluster_types::AttachLayout,
    viewport: (u16, u16),
) -> Result<
    (
        bmux_cluster_plugin_api::cluster_types::AttachLayout,
        ClusterSnapshotBuild,
    ),
    AttachSessionError,
> {
    const MAX_ATTEMPTS: usize = 4;
    for _ in 0..MAX_ATTEMPTS {
        let built = build_cluster_snapshot(client, &layout, None).await;
        let confirmed = cluster_attach_state::client::layout(
            client,
            layout.workspace_id.clone(),
            Some(layout.window_id.clone()),
            viewport.0,
            viewport.1,
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
        "cluster control state changed continuously during attach snapshot assembly".to_string(),
    ))
}

#[allow(clippy::too_many_lines)]
async fn build_cluster_snapshot(
    client: &mut BmuxClient,
    layout: &bmux_cluster_plugin_api::cluster_types::AttachLayout,
    resume: Option<AttachResumeState>,
) -> Result<ClusterSnapshotBuild, AttachSessionError> {
    let mut worker_snapshots = std::collections::BTreeMap::new();
    for pane in &layout.panes {
        let Some(assignment) = pane.execution.as_ref() else {
            continue;
        };
        if pane.availability != bmux_cluster_plugin_api::cluster_types::PaneAvailability::Ready {
            continue;
        }
        let terminal = cluster_attach_state::client::snapshot(
            client,
            layout.workspace_id.clone(),
            assignment.execution_id.clone(),
            assignment.generation,
        )
        .await
        .map_err(provider_error)?
        .map_err(|error| provider_reason(format!("worker snapshot failed: {error:?}")))?;
        worker_snapshots.insert(pane.pane_id.value, terminal);
    }
    assemble_cluster_snapshot(layout, worker_snapshots, resume)
}

#[allow(clippy::too_many_lines)]
fn assemble_cluster_snapshot(
    layout: &bmux_cluster_plugin_api::cluster_types::AttachLayout,
    mut worker_snapshots: std::collections::BTreeMap<
        uuid::Uuid,
        bmux_cluster_plugin_api::cluster_types::WorkerTerminalSnapshot,
    >,
    resume: Option<AttachResumeState>,
) -> Result<ClusterSnapshotBuild, AttachSessionError> {
    if layout.panes.is_empty() {
        return Err(provider_reason(
            "workspace has no logical panes".to_string(),
        ));
    }
    validate_unique_snapshot_streams(layout)?;
    let mut streams = Vec::new();
    let mut protocols = std::collections::BTreeMap::new();
    let mut grids = std::collections::BTreeMap::new();
    let mut focus_pane = None;
    for pane in &layout.panes {
        let assignment = pane.execution.as_ref();
        if let Some(assignment) = assignment
            && pane.availability == bmux_cluster_plugin_api::cluster_types::PaneAvailability::Ready
        {
            let terminal = worker_snapshots
                .remove(&pane.pane_id.value)
                .ok_or_else(|| {
                    provider_reason(format!(
                        "worker snapshot is missing for ready pane {}",
                        pane.pane_id.value
                    ))
                })?;
            validate_worker_snapshot_assignment(&terminal, assignment)?;
            let mut decoded = decode_cluster_terminal_snapshot(&terminal)?;
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
            decoded
                .stream
                .resize(rect.width.max(1), rect.height.max(1))
                .map_err(|error| {
                    provider_reason(format!("worker snapshot reflow failed: {error}"))
                })?;
            let stream_id =
                AttachStreamId::new(format!("execution:{}", assignment.execution_id.value))?;
            let cursor = AttachStreamCursor {
                stream_id: stream_id.clone(),
                surface_id: pane.pane_id.value,
                generation: assignment.generation,
                offset: decoded.cursor,
            };
            let repaint = cluster_terminal_repaint(&decoded.stream);
            protocols.insert(stream_id.clone(), protocol_tracker_from_snapshot(&decoded));
            grids.insert(stream_id, decoded.stream);
            streams.push(AttachStreamSnapshot {
                cursor,
                snapshot: repaint,
            });
            focus_pane.get_or_insert(pane.pane_id.value);
        } else {
            let stream_id = AttachStreamId::new(format!("status:{}", pane.pane_id.value))?;
            let message = format!(
                "\r\n  pane {} is {:?}{}\r\n",
                pane.name.as_deref().unwrap_or("unnamed"),
                pane.availability,
                pane.availability_reason
                    .as_deref()
                    .map_or_else(String::new, |reason| format!(": {reason}"))
            );
            streams.push(AttachStreamSnapshot {
                cursor: AttachStreamCursor {
                    stream_id,
                    surface_id: pane.pane_id.value,
                    generation: 0,
                    offset: u64::try_from(message.len()).unwrap_or(u64::MAX),
                },
                snapshot: message.into_bytes(),
            });
        }
    }
    if !worker_snapshots.is_empty() {
        return Err(provider_reason(
            "worker snapshots include panes outside the selected logical view".to_string(),
        ));
    }
    let scene = cluster_scene_from_layout(layout, focus_pane)?;
    let resume_state = AttachResumeState {
        view_revision: AttachViewRevision(layout.control_revision),
        event_sequence: AttachDeltaSequence(0),
        streams: streams.iter().map(|stream| stream.cursor.clone()).collect(),
        provider_token: layout.control_revision.to_be_bytes().to_vec(),
    };
    if let Some(resume) = resume
        && resume != resume_state
    {
        return Err(provider_reason(
            "resume descriptor does not match current cluster state".to_string(),
        ));
    }
    Ok(ClusterSnapshotBuild {
        snapshot: AttachProviderSnapshot {
            view_revision: resume_state.view_revision,
            event_sequence: resume_state.event_sequence,
            scene,
            streams,
            resume: resume_state,
        },
        protocols,
        grids,
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
    changes.extend(
        snapshot
            .streams
            .iter()
            .cloned()
            .map(bmux_client::AttachProviderChange::StreamRepair),
    );
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

fn validate_worker_snapshot_assignment(
    terminal: &bmux_cluster_plugin_api::cluster_types::WorkerTerminalSnapshot,
    assignment: &bmux_cluster_plugin_api::cluster_types::ExecutionAssignment,
) -> Result<(), AttachSessionError> {
    if terminal.execution_id != assignment.execution_id {
        return Err(provider_reason(format!(
            "worker snapshot execution mismatch: expected {}, received {}",
            assignment.execution_id.value, terminal.execution_id.value
        )));
    }
    if terminal.generation != assignment.generation {
        return Err(provider_reason(format!(
            "worker snapshot generation mismatch: expected {}, received {}",
            assignment.generation, terminal.generation
        )));
    }
    Ok(())
}

fn initial_snapshot_for_resume(
    mut built: ClusterSnapshotBuild,
    control_revision: u64,
    resume: Option<AttachResumeState>,
) -> Result<ClusterSnapshotBuild, AttachSessionError> {
    if let Some(resume) = resume {
        built.snapshot.view_revision = resume.view_revision;
        built.snapshot.event_sequence = resume.event_sequence;
        built.snapshot.resume = AttachResumeState {
            view_revision: resume.view_revision,
            event_sequence: resume.event_sequence,
            streams: built
                .snapshot
                .streams
                .iter()
                .map(|stream| stream.cursor.clone())
                .collect(),
            provider_token: control_revision.to_be_bytes().to_vec(),
        };
        if built.snapshot.resume != resume {
            return Err(provider_reason(
                "resume descriptor does not match current cluster state".to_string(),
            ));
        }
    }
    Ok(built)
}

fn decode_cluster_terminal_snapshot(
    terminal: &bmux_cluster_plugin_api::cluster_types::WorkerTerminalSnapshot,
) -> Result<DecodedClusterTerminalSnapshot, AttachSessionError> {
    let snapshot = serde_json::from_slice::<bmux_terminal_grid::GridSnapshot>(&terminal.encoded)
        .map_err(|error| {
            provider_reason(format!(
                "worker terminal snapshot is not a valid structured grid: {error}"
            ))
        })?;
    let stream = bmux_terminal_grid::TerminalGridStream::from_snapshot(
        &snapshot,
        bmux_terminal_grid::GridLimits::default(),
    )
    .map_err(|error| {
        provider_reason(format!(
            "worker terminal snapshot could not hydrate a terminal grid: {error}"
        ))
    })?;
    Ok(DecodedClusterTerminalSnapshot {
        cursor: terminal.cursor,
        stream,
    })
}

fn cluster_terminal_repaint(stream: &bmux_terminal_grid::TerminalGridStream) -> Vec<u8> {
    bmux_terminal_grid::full_screen_repaint_bytes(stream.grid())
}

fn protocol_tracker_from_snapshot(
    decoded: &DecodedClusterTerminalSnapshot,
) -> bmux_terminal_grid::TerminalProtocolTracker {
    protocol_tracker_from_grid(&decoded.stream)
}

fn protocol_tracker_from_grid(
    grid: &bmux_terminal_grid::TerminalGridStream,
) -> bmux_terminal_grid::TerminalProtocolTracker {
    let mut tracker = bmux_terminal_grid::TerminalProtocolTracker::new();
    tracker.set_protocol_state(grid.grid().protocol_state());
    tracker.set_alternate_screen(grid.grid().mode() == bmux_terminal_grid::GridMode::Alternate);
    tracker
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

fn validate_worker_output(
    output: &bmux_cluster_plugin_api::cluster_types::WorkerOutput,
    execution_id: &bmux_cluster_plugin_api::cluster_types::ExecutionId,
    cursor: &AttachStreamCursor,
) -> Result<(), AttachSessionError> {
    if output.execution_id != *execution_id {
        return Err(provider_reason(
            "worker output execution identity does not match requested stream".to_string(),
        ));
    }
    if output.generation != cursor.generation {
        return Err(provider_reason(format!(
            "worker output generation does not match requested generation {}: received {}",
            cursor.generation, output.generation
        )));
    }
    if output.requested_cursor != cursor.offset {
        return Err(provider_reason(format!(
            "worker output echoed cursor {} for requested cursor {}",
            output.requested_cursor, cursor.offset
        )));
    }
    if output.next_cursor < cursor.offset {
        return Err(provider_reason(format!(
            "worker output returned regressing cursor range {}..{}",
            cursor.offset, output.next_cursor
        )));
    }
    if output.gap {
        if !output.data.is_empty() {
            return Err(provider_reason(
                "worker output retention gap unexpectedly included bytes".to_string(),
            ));
        }
        return Ok(());
    }
    let expected_end = cursor
        .offset
        .checked_add(u64::try_from(output.data.len()).unwrap_or(u64::MAX));
    if expected_end != Some(output.next_cursor) {
        return Err(provider_reason(format!(
            "worker output returned invalid cursor range {}..{} for {} bytes",
            cursor.offset,
            output.next_cursor,
            output.data.len()
        )));
    }
    Ok(())
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

#[derive(Debug)]
struct PaneRuntimeAttachProvider;

#[derive(Debug)]
struct PaneRuntimeAttachTarget {
    target: Option<String>,
}

impl ResolvedAttachTarget for PaneRuntimeAttachTarget {
    fn provider_id(&self) -> &str {
        PROVIDER_ID
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl AttachProvider for PaneRuntimeAttachProvider {
    fn id(&self) -> &str {
        PROVIDER_ID
    }

    fn supports(&self, target: &AttachTarget) -> bool {
        target.scheme().is_none() || target.scheme() == Some("local")
    }

    fn requires_fallback_client(&self) -> bool {
        true
    }

    fn resolve(
        &self,
        target: &AttachTarget,
    ) -> Result<Arc<dyn ResolvedAttachTarget>, AttachProviderError> {
        let resolved = match target.scheme() {
            Some("local") => target.reference(),
            None => target.raw(),
            Some(_) => {
                return Err(AttachProviderError::InvalidTarget {
                    provider_id: PROVIDER_ID.to_string(),
                    target: target.raw().to_string(),
                    reason: "unsupported target scheme".to_string(),
                });
            }
        };
        Ok(Arc::new(PaneRuntimeAttachTarget {
            target: (!resolved.is_empty()).then(|| resolved.to_string()),
        }))
    }

    fn open(
        &self,
        resolved: Arc<dyn ResolvedAttachTarget>,
        _resume: Option<bmux_client::AttachResumeState>,
        fallback_client: Option<BmuxClient>,
    ) -> AttachProviderFuture<'_, AttachProviderSession> {
        Box::pin(async move {
            let target = resolved
                .as_any()
                .downcast_ref::<PaneRuntimeAttachTarget>()
                .ok_or_else(|| AttachProviderError::InvalidTarget {
                    provider_id: PROVIDER_ID.to_string(),
                    target: String::new(),
                    reason: format!(
                        "resolved plan belongs to provider '{}'",
                        resolved.provider_id()
                    ),
                })?;
            let client = fallback_client.ok_or_else(|| AttachProviderError::OpenFailed {
                provider_id: PROVIDER_ID.to_string(),
                reason: "pane-runtime provider requires the fallback client".to_string(),
            })?;
            Ok(AttachProviderSession {
                backend: AttachProviderBackend::Legacy(client),
                target: target.target.clone(),
            })
        })
    }
}

pub struct ResolvedProviderAttach {
    provider: Arc<dyn AttachProvider>,
    resolved: Arc<dyn ResolvedAttachTarget>,
}

impl ResolvedProviderAttach {
    #[must_use]
    pub fn requires_fallback_client(&self) -> bool {
        self.provider.requires_fallback_client()
    }

    pub async fn open(
        self,
        resume: Option<bmux_client::AttachResumeState>,
        fallback_client: Option<BmuxClient>,
    ) -> Result<AttachProviderSession> {
        self.provider
            .open(self.resolved, resume, fallback_client)
            .await
            .map_err(anyhow::Error::from)
    }
}

pub fn resolve(target: Option<&str>) -> Result<ResolvedProviderAttach> {
    install();
    let target = AttachTarget::parse(target.unwrap_or_default());
    let provider = global_attach_provider_registry()
        .resolve(&target)
        .map_err(anyhow::Error::from)?;
    let resolved = provider.resolve(&target).map_err(anyhow::Error::from)?;
    Ok(ResolvedProviderAttach { provider, resolved })
}

/// Install the existing pane-runtime attach path as the local/bare fallback.
pub fn install() {
    static CLUSTER_REGISTRATION: OnceLock<bmux_client::AttachProviderRegistration> =
        OnceLock::new();
    static REGISTRATION: OnceLock<bmux_client::AttachProviderRegistration> = OnceLock::new();
    CLUSTER_REGISTRATION.get_or_init(|| {
        global_attach_provider_registry()
            .register(Arc::new(ClusterAttachProvider))
            .expect("cluster attach provider ID must be unique")
    });
    REGISTRATION.get_or_init(|| {
        global_attach_provider_registry()
            .register(Arc::new(PaneRuntimeAttachProvider))
            .expect("pane-runtime attach provider ID must be unique")
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_attach_layout_protocol::{
        AttachFocusTarget, AttachLayer, AttachRect, AttachScene, AttachSurface, AttachSurfaceKind,
    };
    use bmux_client::{
        AttachDeltaSequence, AttachDetachOutcome, AttachInputPayload, AttachProviderAck,
        AttachProviderBackend, AttachProviderChange, AttachProviderDelta, AttachProviderEvent,
        AttachProviderInput, AttachProviderSnapshot, AttachProviderViewport, AttachResumeState,
        AttachSession, AttachSessionFuture, AttachStreamCursor, AttachStreamId,
        AttachStreamSnapshot, AttachViewRevision,
    };
    use std::sync::Mutex;
    use uuid::Uuid;

    #[derive(Debug)]
    struct SyntheticResolvedTarget;

    impl ResolvedAttachTarget for SyntheticResolvedTarget {
        fn provider_id(&self) -> &'static str {
            "test.synthetic"
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[derive(Debug, Default)]
    struct SyntheticState {
        viewports: Vec<AttachProviderViewport>,
        inputs: Vec<AttachProviderInput>,
        actions: Vec<bmux_client::AttachProviderAction>,
        detached: usize,
    }

    #[derive(Debug)]
    struct SyntheticSession {
        state: Arc<Mutex<SyntheticState>>,
        event_index: usize,
    }

    fn synthetic_scene() -> AttachScene {
        AttachScene {
            session_id: Uuid::nil(),
            focus: AttachFocusTarget::Surface {
                surface_id: Uuid::nil(),
            },
            surfaces: vec![AttachSurface {
                id: Uuid::nil(),
                kind: AttachSurfaceKind::Pane,
                layer: AttachLayer::Pane,
                z: 0,
                rect: AttachRect {
                    x: 0,
                    y: 0,
                    w: 80,
                    h: 24,
                },
                content_rect: AttachRect {
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
                pane_id: Some(Uuid::nil()),
            }],
        }
    }

    fn synthetic_cursor(offset: u64) -> AttachStreamCursor {
        AttachStreamCursor {
            stream_id: AttachStreamId::new("synthetic-stream").unwrap(),
            surface_id: Uuid::nil(),
            generation: 1,
            offset,
        }
    }

    fn synthetic_snapshot() -> AttachProviderSnapshot {
        AttachProviderSnapshot {
            view_revision: AttachViewRevision(1),
            event_sequence: AttachDeltaSequence(1),
            scene: synthetic_scene(),
            streams: vec![{
                let snapshot = b"\x1b[?1h\x1b[?2004hhello".to_vec();
                AttachStreamSnapshot {
                    cursor: AttachStreamCursor {
                        offset: u64::try_from(snapshot.len()).unwrap(),
                        ..synthetic_cursor(5)
                    },
                    snapshot,
                }
            }],
            resume: AttachResumeState {
                view_revision: AttachViewRevision(1),
                event_sequence: AttachDeltaSequence(1),
                streams: vec![AttachStreamCursor {
                    offset: 18,
                    ..synthetic_cursor(5)
                }],
                provider_token: b"resume".to_vec(),
            },
        }
    }

    impl AttachSession for SyntheticSession {
        fn initial_snapshot(&mut self) -> AttachSessionFuture<'_, AttachProviderSnapshot> {
            Box::pin(async { Ok(synthetic_snapshot()) })
        }

        fn next_event(&mut self) -> AttachSessionFuture<'_, AttachProviderEvent> {
            Box::pin(async move {
                let event_index = self.event_index;
                self.event_index += 1;
                if event_index == 0 {
                    Ok(AttachProviderEvent::Delta(AttachProviderDelta {
                        sequence: AttachDeltaSequence(2),
                        base_view_revision: AttachViewRevision(1),
                        view_revision: AttachViewRevision(1),
                        changes: vec![AttachProviderChange::StreamAppend {
                            cursor: synthetic_cursor(18),
                            end_offset: 19,
                            bytes: b"!".to_vec(),
                        }],
                        resume: AttachResumeState {
                            view_revision: AttachViewRevision(1),
                            event_sequence: AttachDeltaSequence(2),
                            streams: vec![synthetic_cursor(19)],
                            provider_token: b"resume-2".to_vec(),
                        },
                    }))
                } else {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    Ok(AttachProviderEvent::Detached)
                }
            })
        }

        fn send_input(
            &mut self,
            input: AttachProviderInput,
        ) -> AttachSessionFuture<'_, AttachProviderAck> {
            self.state.lock().unwrap().inputs.push(input);
            Box::pin(async {
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
            self.state.lock().unwrap().viewports.push(viewport);
            Box::pin(async {
                Ok(AttachProviderAck {
                    command_id: None,
                    accepted: true,
                    message: None,
                })
            })
        }

        fn execute_action(
            &mut self,
            action: bmux_client::AttachProviderAction,
        ) -> AttachSessionFuture<'_, AttachProviderAck> {
            self.state.lock().unwrap().actions.push(action.clone());
            Box::pin(async move {
                Ok(AttachProviderAck {
                    command_id: Some(action.command_id),
                    accepted: true,
                    message: None,
                })
            })
        }

        fn detach(&mut self) -> AttachSessionFuture<'_, AttachDetachOutcome> {
            self.state.lock().unwrap().detached += 1;
            Box::pin(async { Ok(AttachDetachOutcome::Detached) })
        }
    }

    #[derive(Debug)]
    struct DisconnectSession {
        mismatch: bool,
        detached: Arc<Mutex<usize>>,
    }

    impl AttachSession for DisconnectSession {
        fn initial_snapshot(&mut self) -> AttachSessionFuture<'_, AttachProviderSnapshot> {
            Box::pin(async { Ok(synthetic_snapshot()) })
        }

        fn next_event(&mut self) -> AttachSessionFuture<'_, AttachProviderEvent> {
            let mismatch = self.mismatch;
            Box::pin(async move {
                let mut resume = AttachResumeState {
                    view_revision: AttachViewRevision(1),
                    event_sequence: AttachDeltaSequence(1),
                    streams: vec![synthetic_cursor(18)],
                    provider_token: b"resume".to_vec(),
                };
                if mismatch {
                    resume.event_sequence = AttachDeltaSequence(99);
                }
                Ok(AttachProviderEvent::Disconnected(
                    bmux_client::AttachProviderDisconnect {
                        recoverable: true,
                        reason: "synthetic disconnect".to_string(),
                        resume: Some(resume),
                        retry_after_ms: Some(10),
                    },
                ))
            })
        }

        fn send_input(
            &mut self,
            _input: AttachProviderInput,
        ) -> AttachSessionFuture<'_, AttachProviderAck> {
            unreachable!("disconnect session receives no input")
        }

        fn update_viewport(
            &mut self,
            _viewport: AttachProviderViewport,
        ) -> AttachSessionFuture<'_, AttachProviderAck> {
            Box::pin(async {
                Ok(AttachProviderAck {
                    command_id: None,
                    accepted: true,
                    message: None,
                })
            })
        }

        fn execute_action(
            &mut self,
            _action: bmux_client::AttachProviderAction,
        ) -> AttachSessionFuture<'_, AttachProviderAck> {
            unreachable!("disconnect session receives no actions")
        }

        fn detach(&mut self) -> AttachSessionFuture<'_, AttachDetachOutcome> {
            *self.detached.lock().unwrap() += 1;
            Box::pin(async { Ok(AttachDetachOutcome::Detached) })
        }
    }

    #[tokio::test]
    async fn native_runner_preserves_validated_resume_on_recoverable_disconnect() {
        let detached = Arc::new(Mutex::new(0));
        let (mut terminal, _handle) = super::super::runtime::HeadlessAttachTerminal::new(80, 24);
        let outcome = super::super::runtime::run_native_attach_session_with_terminal(
            Box::new(DisconnectSession {
                mismatch: false,
                detached: Arc::clone(&detached),
            }),
            &mut terminal,
        )
        .await
        .expect("recoverable disconnect");
        assert_eq!(
            outcome.exit_reason,
            super::super::state::AttachExitReason::StreamClosed
        );
        assert_eq!(outcome.status_code, 0);
        let resume = outcome.resume.expect("resume state");
        assert_eq!(resume.provider_token, b"resume");
        assert_eq!(resume.streams, vec![synthetic_cursor(18)]);
        assert_eq!(*detached.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn native_runner_rejects_mismatched_disconnect_resume() {
        let (mut terminal, _handle) = super::super::runtime::HeadlessAttachTerminal::new(80, 24);
        let error = super::super::runtime::run_native_attach_session_with_terminal(
            Box::new(DisconnectSession {
                mismatch: true,
                detached: Arc::new(Mutex::new(0)),
            }),
            &mut terminal,
        )
        .await
        .expect_err("mismatched resume must fail");
        assert!(error.to_string().contains("resume state did not match"));
    }

    #[derive(Debug)]
    struct SyntheticProvider {
        state: Arc<Mutex<SyntheticState>>,
    }

    impl AttachProvider for SyntheticProvider {
        fn id(&self) -> &'static str {
            "test.synthetic"
        }

        fn supports(&self, target: &AttachTarget) -> bool {
            target.scheme() == Some("synthetic")
        }

        fn resolve(
            &self,
            _target: &AttachTarget,
        ) -> Result<Arc<dyn ResolvedAttachTarget>, AttachProviderError> {
            Ok(Arc::new(SyntheticResolvedTarget))
        }

        fn open(
            &self,
            _resolved: Arc<dyn ResolvedAttachTarget>,
            _resume: Option<AttachResumeState>,
            fallback_client: Option<BmuxClient>,
        ) -> AttachProviderFuture<'_, AttachProviderSession> {
            assert!(fallback_client.is_none());
            let state = Arc::clone(&self.state);
            Box::pin(async move {
                Ok(AttachProviderSession {
                    backend: AttachProviderBackend::Session(Box::new(SyntheticSession {
                        state,
                        event_index: 0,
                    })),
                    target: None,
                })
            })
        }
    }

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
    fn logical_window_navigation_is_deterministic_and_rejects_ambiguity() {
        let workspace_id = bmux_cluster_plugin_api::cluster_types::WorkspaceId {
            value: uuid::Uuid::from_u128(1),
        };
        let window =
            |id: u128, name: &str| bmux_cluster_plugin_api::cluster_types::LogicalWindowRecord {
                window_id: bmux_cluster_plugin_api::cluster_types::LogicalWindowId {
                    value: uuid::Uuid::from_u128(id),
                },
                workspace_id: workspace_id.clone(),
                name: Some(name.to_string()),
                layout_schema_version: 1,
                layout: Vec::new(),
                revision: 1,
            };
        let first = window(1, "one");
        let second = window(2, "duplicate");
        let third = window(3, "duplicate");
        let windows = vec![&first, &second, &third];
        assert_eq!(
            select_cluster_window(&windows, &first.window_id, "window-next", &[])
                .unwrap()
                .window_id,
            second.window_id
        );
        assert_eq!(
            select_cluster_window(&windows, &first.window_id, "window-prev", &[])
                .unwrap()
                .window_id,
            third.window_id
        );
        assert_eq!(
            select_cluster_window(
                &windows,
                &first.window_id,
                "window-goto",
                &["2".to_string()]
            )
            .unwrap()
            .window_id,
            second.window_id
        );
        assert!(
            select_cluster_window(
                &windows,
                &first.window_id,
                "window-goto",
                &["duplicate".to_string()]
            )
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

    #[test]
    #[allow(clippy::too_many_lines)] // Full snapshot contract fixture intentionally covers every composed field.
    fn assembles_selected_logical_layout_with_ready_and_unavailable_panes() {
        let workspace_id = bmux_cluster_plugin_api::cluster_types::WorkspaceId {
            value: uuid::Uuid::from_u128(1),
        };
        let window_id = bmux_cluster_plugin_api::cluster_types::LogicalWindowId {
            value: uuid::Uuid::from_u128(2),
        };
        let ready_id = uuid::Uuid::from_u128(3);
        let unavailable_id = uuid::Uuid::from_u128(4);
        let assignment = bmux_cluster_plugin_api::cluster_types::ExecutionAssignment {
            node_id: "node-a".to_string(),
            generation: 7,
            execution_id: bmux_cluster_plugin_api::cluster_types::ExecutionId {
                value: uuid::Uuid::from_u128(5),
            },
        };
        let pane = |id, availability, execution, reason| {
            bmux_cluster_plugin_api::cluster_types::LogicalPaneRecord {
                pane_id: bmux_cluster_plugin_api::cluster_types::LogicalPaneId { value: id },
                workspace_id: workspace_id.clone(),
                window_id: window_id.clone(),
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
            }
        };
        let layout = bmux_cluster_plugin_api::cluster_types::AttachLayout {
            workspace_id: workspace_id.clone(),
            window_id: window_id.clone(),
            control_revision: 11,
            panes: vec![
                pane(
                    ready_id,
                    bmux_cluster_plugin_api::cluster_types::PaneAvailability::Ready,
                    Some(assignment.clone()),
                    None,
                ),
                pane(
                    unavailable_id,
                    bmux_cluster_plugin_api::cluster_types::PaneAvailability::Unavailable,
                    None,
                    Some("worker offline".to_string()),
                ),
            ],
            rects: vec![
                bmux_cluster_plugin_api::cluster_types::AttachLayoutRect {
                    pane_id: bmux_cluster_plugin_api::cluster_types::LogicalPaneId {
                        value: ready_id,
                    },
                    x: 0,
                    y: 0,
                    width: 30,
                    height: 20,
                },
                bmux_cluster_plugin_api::cluster_types::AttachLayoutRect {
                    pane_id: bmux_cluster_plugin_api::cluster_types::LogicalPaneId {
                        value: unavailable_id,
                    },
                    x: 30,
                    y: 0,
                    width: 50,
                    height: 20,
                },
            ],
        };
        let mut grid = bmux_terminal_grid::TerminalGridStream::new(
            80,
            24,
            bmux_terminal_grid::GridLimits::default(),
        )
        .unwrap();
        grid.process(b"ready output");
        let terminal = bmux_cluster_plugin_api::cluster_types::WorkerTerminalSnapshot {
            execution_id: assignment.execution_id.clone(),
            generation: assignment.generation,
            cursor: 12,
            encoded: serde_json::to_vec(&grid.snapshot(0, 24)).unwrap(),
        };
        let built = assemble_cluster_snapshot(
            &layout,
            std::collections::BTreeMap::from([(ready_id, terminal)]),
            None,
        )
        .unwrap();

        assert_eq!(built.snapshot.view_revision, AttachViewRevision(11));
        assert_eq!(built.snapshot.scene.surfaces.len(), 2);
        assert_eq!(
            built.snapshot.scene.focus,
            bmux_attach_layout_protocol::AttachFocusTarget::Pane { pane_id: ready_id }
        );
        assert_eq!(built.snapshot.streams.len(), 2);
        assert_eq!(built.snapshot.streams[0].cursor.generation, 7);
        assert_eq!(built.snapshot.streams[0].cursor.offset, 12);
        assert_eq!(
            built.snapshot.streams[1].cursor.stream_id.as_str(),
            format!("status:{unavailable_id}")
        );
        assert!(
            String::from_utf8_lossy(&built.snapshot.streams[1].snapshot).contains("worker offline")
        );
        assert_eq!(built.snapshot.resume.streams.len(), 2);
        let ready_grid = built
            .grids
            .get(&built.snapshot.streams[0].cursor.stream_id)
            .unwrap();
        assert_eq!(
            (ready_grid.grid().width(), ready_grid.grid().height()),
            (30, 20)
        );
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
            snapshot: b"state".to_vec(),
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
        let built = ClusterSnapshotBuild {
            snapshot: AttachProviderSnapshot {
                view_revision: AttachViewRevision(90),
                event_sequence: AttachDeltaSequence(0),
                scene,
                streams: vec![stream.clone()],
                resume: AttachResumeState::default(),
            },
            protocols: std::collections::BTreeMap::new(),
            grids: std::collections::BTreeMap::new(),
        };
        let resume = AttachResumeState {
            view_revision: AttachViewRevision(7),
            event_sequence: AttachDeltaSequence(12),
            streams: vec![stream.cursor],
            provider_token: 90_u64.to_be_bytes().to_vec(),
        };
        let resumed = initial_snapshot_for_resume(built, 90, Some(resume.clone())).unwrap();
        assert_eq!(resumed.snapshot.view_revision, AttachViewRevision(7));
        assert_eq!(resumed.snapshot.event_sequence, AttachDeltaSequence(12));
        assert_eq!(resumed.snapshot.resume, resume);
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
            snapshot: b"new".to_vec(),
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
                bmux_client::AttachProviderChange::StreamRepair(new_stream.clone()),
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
                snapshot: Vec::new(),
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
            window_id: bmux_cluster_plugin_api::cluster_types::LogicalWindowId {
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
            window_id: bmux_cluster_plugin_api::cluster_types::LogicalWindowId {
                value: uuid::Uuid::from_u128(2),
            },
            control_revision: 1,
            panes: vec![pane(3), pane(4)],
            rects: Vec::new(),
        };
        assert!(validate_unique_snapshot_streams(&layout).is_err());
    }

    #[test]
    fn worker_snapshot_must_match_replicated_assignment() {
        let expected = bmux_cluster_plugin_api::cluster_types::ExecutionAssignment {
            node_id: "node-a".to_string(),
            generation: 4,
            execution_id: bmux_cluster_plugin_api::cluster_types::ExecutionId {
                value: uuid::Uuid::from_u128(10),
            },
        };
        let matching = bmux_cluster_plugin_api::cluster_types::WorkerTerminalSnapshot {
            execution_id: expected.execution_id.clone(),
            generation: 4,
            cursor: 0,
            encoded: Vec::new(),
        };
        assert!(validate_worker_snapshot_assignment(&matching, &expected).is_ok());
        assert!(
            validate_worker_snapshot_assignment(
                &bmux_cluster_plugin_api::cluster_types::WorkerTerminalSnapshot {
                    execution_id: bmux_cluster_plugin_api::cluster_types::ExecutionId {
                        value: uuid::Uuid::from_u128(11),
                    },
                    ..matching.clone()
                },
                &expected,
            )
            .is_err()
        );
        assert!(
            validate_worker_snapshot_assignment(
                &bmux_cluster_plugin_api::cluster_types::WorkerTerminalSnapshot {
                    generation: 5,
                    ..matching
                },
                &expected,
            )
            .is_err()
        );
    }

    #[test]
    fn worker_output_is_fenced_to_requested_execution_generation_and_cursor() {
        let execution_id = bmux_cluster_plugin_api::cluster_types::ExecutionId {
            value: uuid::Uuid::from_u128(10),
        };
        let cursor = AttachStreamCursor {
            stream_id: AttachStreamId::new("execution:test").unwrap(),
            surface_id: uuid::Uuid::from_u128(1),
            generation: 4,
            offset: 20,
        };
        let output = bmux_cluster_plugin_api::cluster_types::WorkerOutput {
            execution_id: execution_id.clone(),
            generation: 4,
            requested_cursor: 20,
            retained_start: 0,
            next_cursor: 23,
            gap: false,
            output_still_pending: false,
            data: b"abc".to_vec(),
        };
        assert!(validate_worker_output(&output, &execution_id, &cursor).is_ok());
        assert!(
            validate_worker_output(
                &bmux_cluster_plugin_api::cluster_types::WorkerOutput {
                    generation: 5,
                    ..output.clone()
                },
                &execution_id,
                &cursor,
            )
            .is_err()
        );
        assert!(
            validate_worker_output(
                &bmux_cluster_plugin_api::cluster_types::WorkerOutput {
                    requested_cursor: 19,
                    ..output.clone()
                },
                &execution_id,
                &cursor,
            )
            .is_err()
        );
        assert!(
            validate_worker_output(
                &bmux_cluster_plugin_api::cluster_types::WorkerOutput {
                    next_cursor: 20,
                    ..output.clone()
                },
                &execution_id,
                &cursor,
            )
            .is_err()
        );
        assert!(
            validate_worker_output(
                &bmux_cluster_plugin_api::cluster_types::WorkerOutput {
                    next_cursor: 24,
                    ..output.clone()
                },
                &execution_id,
                &cursor,
            )
            .is_err()
        );
        assert!(
            validate_worker_output(
                &bmux_cluster_plugin_api::cluster_types::WorkerOutput {
                    gap: true,
                    next_cursor: 30,
                    ..output.clone()
                },
                &execution_id,
                &cursor,
            )
            .is_err()
        );
        assert!(
            validate_worker_output(
                &bmux_cluster_plugin_api::cluster_types::WorkerOutput {
                    execution_id: bmux_cluster_plugin_api::cluster_types::ExecutionId {
                        value: uuid::Uuid::from_u128(11),
                    },
                    ..output
                },
                &execution_id,
                &cursor,
            )
            .is_err()
        );
    }

    #[test]
    fn cluster_terminal_snapshot_repairs_grid_protocol_and_delta_continuity() {
        let mut source = bmux_terminal_grid::TerminalGridStream::new(
            12,
            3,
            bmux_terminal_grid::GridLimits::default(),
        )
        .unwrap();
        source.process(b"\x1b[?1000h\x1b[?1006hhello\x1b[2;4H\x1b[?25l\x1b[");
        let terminal = bmux_cluster_plugin_api::cluster_types::WorkerTerminalSnapshot {
            execution_id: bmux_cluster_plugin_api::cluster_types::ExecutionId {
                value: uuid::Uuid::from_u128(7),
            },
            generation: 3,
            cursor: 41,
            encoded: serde_json::to_vec(&source.snapshot(0, 3)).unwrap(),
        };

        let decoded = decode_cluster_terminal_snapshot(&terminal).unwrap();
        assert_eq!(decoded.cursor, 41);
        assert_eq!(
            decoded.stream.grid().protocol_state(),
            source.grid().protocol_state()
        );
        assert_ne!(
            decoded.stream.grid().mode(),
            bmux_terminal_grid::GridMode::Alternate
        );

        let tracker = protocol_tracker_from_snapshot(&decoded);
        assert_eq!(tracker.protocol_state(), source.grid().protocol_state());
        assert!(!tracker.alternate_screen());

        let next = b"31m!";
        source.process(next);
        let mut replay = decoded.stream;
        replay.process(next);
        assert_eq!(
            bmux_terminal_grid::visible_text(replay.grid(), 0, 3),
            bmux_terminal_grid::visible_text(source.grid(), 0, 3)
        );
        assert_eq!(replay.grid().cursor(), source.grid().cursor());
        let repaired_style = replay.grid().viewport_rows()[1].cells()[3].style();
        assert_eq!(
            replay.grid().palette().get(repaired_style).fg,
            Some(bmux_terminal_grid::Color::Indexed(1))
        );
    }

    #[test]
    fn cluster_terminal_snapshot_rejects_malformed_structured_state() {
        let terminal = bmux_cluster_plugin_api::cluster_types::WorkerTerminalSnapshot {
            execution_id: bmux_cluster_plugin_api::cluster_types::ExecutionId {
                value: uuid::Uuid::from_u128(7),
            },
            generation: 3,
            cursor: 41,
            encoded: b"not a structured terminal snapshot".to_vec(),
        };

        let error = decode_cluster_terminal_snapshot(&terminal)
            .err()
            .expect("malformed snapshot must fail");
        assert!(error.to_string().contains("valid structured grid"));
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
    fn local_and_cluster_providers_coexist_in_global_registry() {
        install();
        let local = global_attach_provider_registry()
            .resolve(&AttachTarget::parse("local://workspace"))
            .unwrap();
        let cluster = global_attach_provider_registry()
            .resolve(&AttachTarget::parse("cluster://prod/workspace"))
            .unwrap();
        assert_eq!(local.id(), PROVIDER_ID);
        assert_eq!(cluster.id(), CLUSTER_PROVIDER_ID);
        assert!(local.requires_fallback_client());
        assert!(cluster.requires_fallback_client());
    }

    #[test]
    fn cluster_provider_claims_and_validates_cluster_uris() {
        let provider = ClusterAttachProvider;
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

    #[tokio::test]
    async fn synthetic_provider_runs_through_domain_neutral_cli_path() {
        install();
        let state = Arc::new(Mutex::new(SyntheticState::default()));
        let registration = global_attach_provider_registry()
            .register(Arc::new(SyntheticProvider {
                state: Arc::clone(&state),
            }))
            .expect("register synthetic provider");
        let provider = resolve(Some("synthetic://workspace")).expect("resolve synthetic provider");
        assert!(!provider.requires_fallback_client());
        let opened = provider
            .open(None, None)
            .await
            .expect("open synthetic provider");
        let mut native_session = match opened.backend {
            AttachProviderBackend::Session(session) => session,
            AttachProviderBackend::Legacy(_) => panic!("expected native provider session"),
        };
        let mut action_controls = bmux_client::AttachControlValidator::default();
        let action = bmux_client::AttachProviderAction {
            command_id: "action-1".to_string(),
            action: "focus-next".to_string(),
            arguments: Vec::new(),
        };
        super::super::runtime::execute_native_provider_action(
            native_session.as_mut(),
            &mut action_controls,
            action.clone(),
        )
        .await
        .expect("execute generic action");
        let duplicate = super::super::runtime::execute_native_provider_action(
            native_session.as_mut(),
            &mut action_controls,
            action,
        )
        .await
        .expect_err("duplicate action must be rejected");
        assert!(duplicate.to_string().contains("duplicate attach action"));
        let (mut terminal, handle) = super::super::runtime::HeadlessAttachTerminal::new(80, 24);
        handle
            .send_event(crossterm::event::Event::Key(
                crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Char('a'),
                    crossterm::event::KeyModifiers::CONTROL,
                ),
            ))
            .unwrap();
        handle
            .send_event(crossterm::event::Event::Key(
                crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Char('o'),
                    crossterm::event::KeyModifiers::NONE,
                ),
            ))
            .unwrap();
        handle
            .send_event(crossterm::event::Event::Paste("input".to_string()))
            .unwrap();
        let outcome = super::super::runtime::run_native_attach_session_with_terminal(
            native_session,
            &mut terminal,
        )
        .await
        .expect("run synthetic provider");
        assert_eq!(
            outcome.exit_reason,
            super::super::state::AttachExitReason::Detached
        );
        let output = handle.output_bytes();
        let mut rendered = bmux_terminal_grid::TerminalGridStream::new(
            80,
            24,
            bmux_terminal_grid::GridLimits::default(),
        )
        .unwrap();
        rendered.process(&output);
        assert!(bmux_terminal_grid::visible_text(rendered.grid(), 0, 24).starts_with("hello!"));
        let state = state.lock().unwrap();
        assert_eq!(state.viewports.len(), 1);
        assert_eq!(state.viewports[0].columns, 80);
        assert_eq!(state.inputs.len(), 1);
        assert_eq!(state.actions.len(), 2);
        assert_eq!(state.actions[1].action, "focus");
        assert_eq!(state.actions[1].arguments, ["next"]);
        assert_eq!(state.inputs[0].generation, 1);
        assert_eq!(
            state.inputs[0].payload,
            AttachInputPayload::Paste(b"input".to_vec())
        );
        assert_eq!(state.detached, 1);
        drop(state);
        drop(registration);
    }

    #[test]
    fn supports_bare_and_local_targets_only() {
        let provider = PaneRuntimeAttachProvider;
        assert!(provider.supports(&AttachTarget::parse("main")));
        assert!(provider.supports(&AttachTarget::parse("local://main")));
        assert!(!provider.supports(&AttachTarget::parse("synthetic://main")));
    }

    #[test]
    fn resolution_strips_local_scheme_preserves_bare_and_supports_follow() {
        let provider = PaneRuntimeAttachProvider;
        for (raw, expected) in [
            ("main", Some("main")),
            ("local://main", Some("main")),
            ("", None),
        ] {
            let resolved = provider.resolve(&AttachTarget::parse(raw)).unwrap();
            let resolved = resolved
                .as_any()
                .downcast_ref::<PaneRuntimeAttachTarget>()
                .unwrap();
            assert_eq!(resolved.target.as_deref(), expected);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn open_preserves_existing_connected_client_and_target() {
        let root = tempfile::tempdir().expect("tempdir");
        let paths = bmux_config::ConfigPaths::new(
            root.path().join("config"),
            root.path().join("runtime"),
            root.path().join("data"),
            root.path().join("state"),
        );
        let server = Arc::new(bmux_server::BmuxServer::from_config_paths(&paths));
        let running = Arc::clone(&server);
        let task = tokio::spawn(async move { running.run().await });
        for _ in 0..100 {
            if paths.server_socket().exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let client = BmuxClient::connect_with_paths(&paths, "attach-provider-test")
            .await
            .expect("connect client");
        let principal_id = client.principal_id();
        let provider = PaneRuntimeAttachProvider;
        let resolved = provider.resolve(&AttachTarget::parse("main")).unwrap();
        let session = provider
            .open(resolved, None, Some(client))
            .await
            .expect("open default provider");
        assert_eq!(session.target.as_deref(), Some("main"));
        let AttachProviderBackend::Legacy(client) = session.backend else {
            panic!("pane-runtime provider must preserve the legacy backend");
        };
        assert_eq!(client.principal_id(), principal_id);

        server.request_shutdown();
        task.await.expect("server join").expect("server run");
    }
}
