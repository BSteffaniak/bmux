//! End-to-end cluster pane replication against a real server process.
//!
//! One node is ingress, voter and worker. A logical workspace's pane runs on it, and the
//! in-process `bmux.cluster` attach provider follows the pane through the same services the CLI
//! uses: the layout from the ingress, observe leases from `routes`, frames the worker pushes to
//! the observing connection, input and resizes through the ingress, and control revisions the
//! ingress pushes. Nothing here polls the provider: every change arrives from `next_event`.
use crate::support::{ServerEnv, assert_success, combined_output};
use bmux_client::{
    AttachEndpointConnectFuture, AttachEndpointConnector, AttachInputPayload,
    AttachProviderBackend, AttachProviderChange, AttachProviderEvent, AttachProviderInput,
    AttachProviderOpenContext, AttachProviderViewport, AttachSession, AttachStreamCursor,
    AttachTarget, BmuxClient,
};
use bmux_cluster_plugin_api::cluster_types::{
    CommandId, ControlCommand, ControlCommandRequest, ControlCommandResult, ControlStateView,
    ExecutionAssignment, ExecutionId, LogicalPaneId, LogicalPaneRecord, LogicalTabId,
    LogicalTabRecord, PaneAvailability, PaneRestartPolicy, PlacementIntent, WorkerLaunchSpec,
    WorkspaceId,
};
use bmux_frontend_runtime::AttachState;
use serial_test::serial;
use std::path::PathBuf;
use std::time::Duration;

/// The worker's advertised member endpoint: a configured alias for the server's local socket.
const WORKER_ALIAS: &str = "frames-worker";
const WORKSPACE_NAME: &str = "frames";
const PRINCIPAL: &str = "principal:frame-replication-test";
/// How long a pushed change may take to reach the rendered state.
const PUSH_DEADLINE: Duration = Duration::from_secs(10);
/// How long the leader's reconciler may take to launch an execution.
const LAUNCH_DEADLINE: Duration = Duration::from_secs(30);

fn server_config() -> String {
    format!(
        "[general]\nserver_timeout = 30000\n\n[connections.targets.{WORKER_ALIAS}]\ntransport = \"local\"\n"
    )
}

/// Dials endpoint targets as the CLI's attach connector does for local-transport targets:
/// `local` and configured aliases whose transport is local reach this server's socket; anything
/// else is not an explicit connection target. The configuration is read on each connect.
#[derive(Debug)]
struct ServerConfigConnector {
    socket: PathBuf,
    config: PathBuf,
}

impl ServerConfigConnector {
    fn new(server: &ServerEnv) -> Self {
        Self {
            socket: server.runtime_dir.join("server.sock"),
            config: server.config_dir.join("bmux.toml"),
        }
    }

    fn resolves_locally(&self, target: &str) -> Result<(), String> {
        if target == "local" {
            return Ok(());
        }
        let text = std::fs::read_to_string(&self.config).map_err(|error| error.to_string())?;
        let config: bmux_config::BmuxConfig =
            toml::from_str(&text).map_err(|error| error.to_string())?;
        match config.connections.targets.get(target) {
            Some(named) if named.transport == bmux_config::ConnectionTransport::Local => Ok(()),
            Some(_) => Err(format!(
                "attach endpoint target '{target}' is not a local-transport target"
            )),
            None => Err(format!(
                "attach endpoint target '{target}' is not a configured alias or direct endpoint"
            )),
        }
    }
}

impl AttachEndpointConnector for ServerConfigConnector {
    fn connect<'a>(
        &'a self,
        target: &'a str,
        client_name: &'static str,
    ) -> AttachEndpointConnectFuture<'a> {
        Box::pin(async move {
            self.resolves_locally(target)?;
            BmuxClient::connect(
                &bmux_ipc::IpcEndpoint::UnixSocket(self.socket.clone()),
                Duration::from_secs(30),
                client_name,
            )
            .await
            .map_err(|error| error.to_string())
        })
    }
}

async fn local_connection(server: &ServerEnv) -> BmuxClient {
    BmuxClient::connect(
        &bmux_ipc::IpcEndpoint::UnixSocket(server.runtime_dir.join("server.sock")),
        Duration::from_secs(30),
        "cluster-frame-replication-integration",
    )
    .await
    .expect("connect to isolated local owner endpoint")
}

fn command(request: ControlCommandRequest) -> ControlCommand {
    ControlCommand {
        schema_version: 1,
        principal_id: PRINCIPAL.to_string(),
        command_id: CommandId {
            value: uuid::Uuid::new_v4(),
        },
        issued_at_unix_ms: u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_millis(),
        )
        .expect("timestamp fits u64"),
        request,
    }
}

async fn mutate(client: &mut BmuxClient, request: ControlCommandRequest) -> u64 {
    let response =
        bmux_cluster_plugin_api::cluster_control_command::client::mutate(client, command(request))
            .await
            .expect("dispatch control mutation")
            .expect("control mutation reaches consensus");
    assert!(
        matches!(response.result, ControlCommandResult::Accepted { .. }),
        "control mutation rejected: {:?}",
        response.result
    );
    response.control_revision
}

async fn control_view(client: &mut BmuxClient) -> ControlStateView {
    bmux_cluster_plugin_api::cluster_control_state::client::read_linearizable(client)
        .await
        .expect("dispatch control read")
        .expect("linearizable control read")
}

/// Runs `sh`, which prints `ready:<word>` (assembled at run time, so the marker never appears in
/// the command line), then answers each input line with `SEEN:<LINE>`.
fn marker_launch(word: &str) -> WorkerLaunchSpec {
    WorkerLaunchSpec {
        program: Some("/bin/sh".to_string()),
        args: vec![
            "-c".to_string(),
            format!(
                "printf 'ready:%s\\n' {word}; while read line; do printf 'seen:%s\\n' \"$line\" | tr a-z A-Z; done"
            ),
        ],
        cwd: None,
        env: Vec::new(),
        cols: 80,
        rows: 24,
    }
}

/// The cluster's logical pane, created and assigned by real control mutations.
struct ClusterPane {
    cluster_id: String,
    node_id: String,
    pane_id: LogicalPaneId,
}

impl ClusterPane {
    /// A workspace with one tab and one pane placed on this node.
    async fn create(client: &mut BmuxClient) -> Self {
        let identity = bmux_cluster_plugin_api::cluster_query::client::identity(client)
            .await
            .expect("read cluster identity");
        let cluster_id = identity
            .cluster_id
            .clone()
            .expect("initialized node has a cluster id");
        let node_id = identity.node_id;
        let workspace_id = WorkspaceId {
            value: uuid::Uuid::new_v4(),
        };
        let tab_id = LogicalTabId {
            value: uuid::Uuid::new_v4(),
        };
        let pane_id = LogicalPaneId {
            value: uuid::Uuid::new_v4(),
        };
        let revision = mutate(
            client,
            ControlCommandRequest::CreateWorkspace {
                workspace_id: workspace_id.clone(),
                name: Some(WORKSPACE_NAME.to_string()),
            },
        )
        .await;
        let tab = LogicalTabRecord {
            tab_id: tab_id.clone(),
            workspace_id: workspace_id.clone(),
            name: Some("main".to_string()),
            layout_schema_version: 1,
            layout: Vec::new(),
            revision: 0,
        };
        let revision = mutate(
            client,
            ControlCommandRequest::PutTab {
                tab,
                expected_workspace_revision: revision,
            },
        )
        .await;
        let pane = LogicalPaneRecord {
            pane_id: pane_id.clone(),
            workspace_id,
            tab_id,
            name: Some("shell".to_string()),
            restart_policy: PaneRestartPolicy::Manual,
            placement: PlacementIntent {
                explicit_node_id: Some(node_id.clone()),
                required_labels: Vec::new(),
                preferred_labels: Vec::new(),
            },
            availability: PaneAvailability::Pending,
            availability_reason: None,
            execution: None,
            revision: 0,
        };
        mutate(
            client,
            ControlCommandRequest::PutPane {
                pane,
                expected_workspace_revision: revision,
            },
        )
        .await;
        Self {
            cluster_id,
            node_id,
            pane_id,
        }
    }

    async fn record(&self, client: &mut BmuxClient) -> LogicalPaneRecord {
        control_view(client)
            .await
            .panes
            .into_iter()
            .find(|pane| pane.pane_id == self.pane_id)
            .expect("logical pane exists")
    }

    /// Assign the pane's next execution generation to this node, running `launch`; the leader's
    /// reconciler launches it on the worker. Returns the new execution's id.
    async fn assign(&self, client: &mut BmuxClient, launch: WorkerLaunchSpec) -> uuid::Uuid {
        let pane = self.record(client).await;
        let generation = pane
            .execution
            .as_ref()
            .map_or(0, |current| current.generation);
        let execution_id = uuid::Uuid::new_v4();
        mutate(
            client,
            ControlCommandRequest::AssignExecution {
                pane_id: self.pane_id.clone(),
                expected_revision: pane.revision,
                expected_generation: generation,
                assignment: ExecutionAssignment {
                    node_id: self.node_id.clone(),
                    generation: generation + 1,
                    execution_id: ExecutionId {
                        value: execution_id,
                    },
                },
                launch_spec: Some(launch),
            },
        )
        .await;
        execution_id
    }

    /// Wait for `execution_id` to be the pane's ready execution (setup only: the reconciler
    /// launches it in the background, before any attach exists).
    async fn await_ready(&self, client: &mut BmuxClient, execution_id: uuid::Uuid) {
        let deadline = tokio::time::Instant::now() + LAUNCH_DEADLINE;
        loop {
            let pane = self.record(client).await;
            let current = pane
                .execution
                .as_ref()
                .is_some_and(|assignment| assignment.execution_id.value == execution_id);
            if current && pane.availability == PaneAvailability::Ready {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "execution {execution_id} not ready in time: {:?} ({:?})",
                pane.availability,
                pane.availability_reason
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// What every replica in `state` shows: width, height and visible text.
fn rendered(state: &mut AttachState) -> Vec<(usize, usize, String)> {
    state
        .buffers_mut()
        .values()
        .map(|buffer| {
            let grid = buffer.terminal_grid.grid();
            (
                grid.width(),
                grid.height(),
                bmux_terminal_grid::visible_text(grid, 0, grid.height()),
            )
        })
        .collect()
}

fn shows(state: &mut AttachState, text: &str) -> bool {
    rendered(state)
        .iter()
        .any(|(_, _, screen)| screen.contains(text))
}

fn has_stream(state: &AttachState, execution_id: uuid::Uuid) -> bool {
    let stream_id = format!("execution:{execution_id}");
    state
        .resume_state()
        .streams
        .iter()
        .any(|cursor| cursor.stream_id.as_str() == stream_id)
}

fn execution_stream(state: &AttachState, execution_id: uuid::Uuid) -> AttachStreamCursor {
    let stream_id = format!("execution:{execution_id}");
    let streams = state.resume_state().streams;
    streams
        .iter()
        .find(|cursor| cursor.stream_id.as_str() == stream_id)
        .cloned()
        .unwrap_or_else(|| panic!("no stream for execution {execution_id}: {streams:?}"))
}

/// What the provider pushed while waiting for a condition.
#[derive(Debug, Default)]
struct Pushed {
    scenes: usize,
    resets: usize,
    frames: usize,
}

impl Pushed {
    fn count(&mut self, changes: &[AttachProviderChange]) {
        for change in changes {
            match change {
                AttachProviderChange::Scene(_) => self.scenes += 1,
                AttachProviderChange::StreamFrame(frame) => {
                    self.frames += 1;
                    self.resets += usize::from(frame.reset);
                }
                AttachProviderChange::StreamRemoved { .. }
                | AttachProviderChange::Status { .. } => {}
            }
        }
    }
}

/// The provider's session as a renderer holds it: events applied to an `AttachState`.
struct Attached {
    session: Box<dyn AttachSession>,
    state: AttachState,
    command_sequence: u64,
}

impl Attached {
    async fn open(server: &ServerEnv, cluster_id: &str) -> Self {
        let provider = bmux_cluster_plugin_client::attach_provider();
        let target = AttachTarget::parse(format!("cluster://{cluster_id}/{WORKSPACE_NAME}"));
        assert!(provider.supports(&target));
        let resolved = provider.resolve(&target).expect("resolve cluster target");
        // As the CLI opens it: the server connection in hand, and an endpoint connector for
        // the workers and the ingress's control pushes.
        let context = AttachProviderOpenContext {
            fallback_client: Some(local_connection(server).await),
            endpoint_connector: Some(std::sync::Arc::new(ServerConfigConnector::new(server))),
        };
        let opened = tokio::time::timeout(PUSH_DEADLINE, provider.open(resolved, None, context))
            .await
            .expect("cluster attach opens in time")
            .expect("open cluster attach");
        let AttachProviderBackend::Session(mut session) = opened.backend else {
            panic!("cluster provider opens a native session");
        };
        let snapshot = session
            .initial_snapshot()
            .await
            .expect("initial cluster snapshot");
        let state = AttachState::from_snapshot(&snapshot).expect("snapshot applies");
        Self {
            session,
            state,
            command_sequence: 0,
        }
    }

    /// Apply the provider's events until `done` holds, failing after `deadline`.
    async fn apply_until(
        &mut self,
        deadline: Duration,
        what: &str,
        mut done: impl FnMut(&mut AttachState) -> bool,
    ) -> Pushed {
        let mut pushed = Pushed::default();
        let until = tokio::time::Instant::now() + deadline;
        while !done(&mut self.state) {
            let Ok(event) = tokio::time::timeout_at(until, self.session.next_event()).await else {
                panic!(
                    "{what}: not pushed within {deadline:?}; pushed={pushed:?}; rendered={:?}",
                    rendered(&mut self.state)
                );
            };
            let event = event.unwrap_or_else(|error| panic!("{what}: provider failed: {error}"));
            let AttachProviderEvent::Delta(delta) = event else {
                panic!("{what}: provider ended the session: {event:?}");
            };
            pushed.count(&delta.changes);
            self.state
                .apply_delta(&delta)
                .unwrap_or_else(|error| panic!("{what}: delta does not apply: {error:#}"));
        }
        pushed
    }

    async fn type_line(&mut self, execution_id: uuid::Uuid, line: &str) {
        let stream = execution_stream(&self.state, execution_id);
        self.command_sequence += 1;
        self.session
            .send_input(AttachProviderInput {
                command_sequence: self.command_sequence,
                stream_id: stream.stream_id,
                generation: stream.generation,
                payload: AttachInputPayload::Bytes(format!("{line}\r").into_bytes()),
            })
            .await
            .expect("input reaches the worker");
    }

    async fn resize(&mut self, columns: u16, rows: u16) {
        self.command_sequence += 1;
        self.session
            .update_viewport(AttachProviderViewport {
                command_sequence: self.command_sequence,
                columns,
                rows,
                pixel_width: None,
                pixel_height: None,
            })
            .await
            .expect("viewport resize reaches the worker");
    }
}

async fn replicate(server: &ServerEnv) {
    let mut client = local_connection(server).await;
    let pane = ClusterPane::create(&mut client).await;
    let first = pane.assign(&mut client, marker_launch("one")).await;
    pane.await_ready(&mut client, first).await;
    // A ready execution's program may not have printed yet: watch its marker arrive through a
    // first attach, so the next attach's snapshot has a known screen to carry.
    let mut watching = Attached::open(server, &pane.cluster_id).await;
    watching
        .apply_until(PUSH_DEADLINE, "pane output", |state| {
            shows(state, "ready:one")
        })
        .await;
    watching.session.detach().await.expect("detach");
    let mut attached = Attached::open(server, &pane.cluster_id).await;

    // A new attach's initial snapshot carries the pane's current screen.
    assert!(
        shows(&mut attached.state, "ready:one"),
        "initial snapshot shows the pane's output: {:?}",
        rendered(&mut attached.state)
    );
    let stream = execution_stream(&attached.state, first);
    assert_eq!(stream.generation, 1);
    assert_eq!(stream.surface_id, pane.pane_id.value);

    // Input typed through the provider reaches the worker's pane, and the pane's answer comes
    // back as pushed frames.
    attached.type_line(first, "two").await;
    let pushed = attached
        .apply_until(PUSH_DEADLINE, "input answer", |state| {
            shows(state, "SEEN:TWO")
        })
        .await;
    assert!(
        pushed.frames > 0,
        "the answer arrived as frames: {pushed:?}"
    );
    assert!(
        shows(&mut attached.state, "ready:one"),
        "earlier output is kept"
    );

    // A viewport change resizes the worker's pane; its frames carry the new size.
    attached.resize(100, 30).await;
    let pushed = attached
        .apply_until(PUSH_DEADLINE, "resize", |state| {
            rendered(state)
                .iter()
                .any(|(width, height, _)| (*width, *height) == (100, 30))
        })
        .await;
    assert!(pushed.scenes > 0, "the new scene was pushed: {pushed:?}");
    assert!(
        shows(&mut attached.state, "SEEN:TWO"),
        "the resized replica keeps the pane's screen: {:?}",
        rendered(&mut attached.state)
    );

    // A control change (the pane's next execution generation, as a retry makes) is pushed by
    // the ingress, and the provider reconciles to the new execution's stream.
    let second = pane.assign(&mut client, marker_launch("three")).await;
    let pushed = attached
        .apply_until(LAUNCH_DEADLINE, "new generation", |state| {
            has_stream(state, second) && shows(state, "ready:three")
        })
        .await;
    assert!(
        pushed.scenes > 0,
        "the reconciled scene was pushed: {pushed:?}"
    );
    assert_eq!(execution_stream(&attached.state, second).generation, 2);
    assert!(
        !has_stream(&attached.state, first),
        "the replaced generation's stream is gone"
    );

    // The new generation is live: its input round-trips too.
    attached.type_line(second, "four").await;
    attached
        .apply_until(PUSH_DEADLINE, "new generation input", |state| {
            shows(state, "SEEN:FOUR")
        })
        .await;
    attached.session.detach().await.expect("detach");
}

#[test]
#[serial]
fn cluster_attach_replicates_a_worker_pane_through_pushed_frames() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("test runtime");
    let mut server = ServerEnv::new("cluster-frames");
    server.write_config(&server_config());
    server.start();
    // A single-node cluster: this node is its voter, ingress and worker, advertised under an
    // alias of its own local socket.
    let init = server.run(&["cluster", "init", "--endpoint", WORKER_ALIAS]);
    assert_success(&init, "initialize single-node cluster");
    assert!(
        combined_output(&init).contains("cluster initialized"),
        "unexpected init output: {}",
        combined_output(&init)
    );
    runtime.block_on(replicate(&server));
    server.finish();
}
