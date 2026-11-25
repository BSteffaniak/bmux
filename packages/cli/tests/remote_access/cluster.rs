//! Gate 9's cluster fixture: two hosts that paired with each other become one cluster, and a
//! logical pane is placed on the worker through the ingress's control service.
use std::time::Duration;

use bmux_client::BmuxClient;
use bmux_cluster_plugin_api::cluster_types::{
    CommandId, ControlCommand, ControlCommandRequest, ControlCommandResult, ExecutionAssignment,
    ExecutionId, LogicalPaneId, LogicalPaneRecord, LogicalTabId, LogicalTabRecord,
    PaneAvailability, PaneRestartPolicy, PlacementIntent, WorkerLaunchSpec, WorkspaceId,
};
use bmux_cluster_plugin_api::{cluster_control_command, cluster_control_state, cluster_query};

use crate::harness::Host;

/// How long the leader's reconciler may take to launch an execution on a worker.
const LAUNCH_DEADLINE: Duration = Duration::from_secs(60);

/// The host's own server, as its owner reaches it.
pub async fn owner(host: &Host) -> BmuxClient {
    BmuxClient::connect(
        &bmux_ipc::IpcEndpoint::UnixSocket(host.env.runtime_dir.join("server.sock")),
        Duration::from_secs(30),
        "remote-access-cluster",
    )
    .await
    .expect("connect to the host's own server")
}

/// This node's id and, once initialized or joined, its cluster's id.
pub async fn identity(client: &mut BmuxClient) -> (String, Option<String>) {
    let identity = cluster_query::client::identity(client)
        .await
        .expect("read cluster identity");
    (identity.node_id, identity.cluster_id)
}

fn command(request: ControlCommandRequest) -> ControlCommand {
    ControlCommand {
        schema_version: 1,
        principal_id: "principal:remote-access-gate9".to_string(),
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

/// The cluster's nodes, as their owners reach them. Control requests go to whichever node
/// leads: one that does not answers `NotLeader`, and the request is retried on the next
/// node with the same command id, so it applies once.
pub struct Nodes(pub Vec<BmuxClient>);

async fn mutate(nodes: &mut Nodes, request: ControlCommandRequest) -> u64 {
    let command = command(request);
    let deadline = tokio::time::Instant::now() + LAUNCH_DEADLINE;
    loop {
        for client in &mut nodes.0 {
            match cluster_control_command::client::mutate(client, command.clone())
                .await
                .expect("dispatch control mutation")
            {
                Ok(response) => {
                    assert!(
                        matches!(response.result, ControlCommandResult::Accepted { .. }),
                        "control mutation rejected: {:?}",
                        response.result
                    );
                    return response.control_revision;
                }
                Err(error) => assert!(
                    tokio::time::Instant::now() < deadline,
                    "control mutation never reached a leader: {error:?}"
                ),
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn pane(nodes: &mut Nodes, pane_id: &LogicalPaneId) -> LogicalPaneRecord {
    let deadline = tokio::time::Instant::now() + LAUNCH_DEADLINE;
    loop {
        for client in &mut nodes.0 {
            if let Ok(view) = cluster_control_state::client::read_linearizable(client)
                .await
                .expect("dispatch control read")
            {
                return view
                    .panes
                    .into_iter()
                    .find(|pane| &pane.pane_id == pane_id)
                    .expect("logical pane exists");
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no node answered a linearizable read"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Creates workspace `name` with one pane placed on `worker_node`, running a shell that
/// prints `ready:<word>` and answers each input line with `SEEN:<LINE>`, and waits until
/// the worker runs it.
pub async fn place_pane(ingress: &mut Nodes, name: &str, worker_node: &str, word: &str) {
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
        ingress,
        ControlCommandRequest::CreateWorkspace {
            workspace_id: workspace_id.clone(),
            name: Some(name.to_string()),
        },
    )
    .await;
    let revision = mutate(
        ingress,
        ControlCommandRequest::PutTab {
            tab: LogicalTabRecord {
                tab_id: tab_id.clone(),
                workspace_id: workspace_id.clone(),
                name: Some("main".to_string()),
                layout_schema_version: 1,
                layout: Vec::new(),
                revision: 0,
            },
            expected_workspace_revision: revision,
        },
    )
    .await;
    mutate(
        ingress,
        ControlCommandRequest::PutPane {
            pane: LogicalPaneRecord {
                pane_id: pane_id.clone(),
                workspace_id,
                tab_id,
                name: Some("worker".to_string()),
                restart_policy: PaneRestartPolicy::Manual,
                placement: PlacementIntent {
                    explicit_node_id: Some(worker_node.to_string()),
                    required_labels: Vec::new(),
                    preferred_labels: Vec::new(),
                },
                availability: PaneAvailability::Pending,
                availability_reason: None,
                execution: None,
                revision: 0,
            },
            expected_workspace_revision: revision,
        },
    )
    .await;
    let record = pane(ingress, &pane_id).await;
    let execution_id = uuid::Uuid::new_v4();
    mutate(
        ingress,
        ControlCommandRequest::AssignExecution {
            pane_id: pane_id.clone(),
            expected_revision: record.revision,
            expected_generation: 0,
            assignment: ExecutionAssignment {
                node_id: worker_node.to_string(),
                generation: 1,
                execution_id: ExecutionId {
                    value: execution_id,
                },
            },
            launch_spec: Some(WorkerLaunchSpec {
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
            }),
        },
    )
    .await;
    let deadline = tokio::time::Instant::now() + LAUNCH_DEADLINE;
    loop {
        let record = pane(ingress, &pane_id).await;
        let current = record
            .execution
            .as_ref()
            .is_some_and(|assignment| assignment.execution_id.value == execution_id);
        if current && record.availability == PaneAvailability::Ready {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the worker did not run the pane in time: {:?} ({:?})",
            record.availability,
            record.availability_reason
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
