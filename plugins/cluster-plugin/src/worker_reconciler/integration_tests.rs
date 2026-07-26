use super::*;
use crate::consensus_runtime::tests::{
    InMemoryNetworkFactory, wait_for_leader, wait_for_leader_excluding,
};
use crate::membership::{ClusterId, NodeId};
use crate::worker_runtime::{WorkerLeaseVerifier, WorkerPaneRuntime, WorkerServiceHandle};
use bmux_cluster_plugin_api::cluster_types::{
    ClusterConsensusRole, ClusterNodeCapabilities, ControlWorkflowStatus, LogicalPaneRecord,
    LogicalWindowId, LogicalWindowRecord, PaneRestartPolicy, PlacementIntent, WorkerLaunchSpec,
};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

#[derive(Clone, Default)]
struct RecordingRuntime {
    launches: Arc<Mutex<u32>>,
}

impl WorkerPaneRuntime for RecordingRuntime {
    fn launch(
        &self,
        authority: &WorkerAuthority,
        _spec: &WorkerLaunchSpec,
    ) -> Result<(uuid::Uuid, uuid::Uuid), WorkerServiceError> {
        *self.launches.lock().unwrap() += 1;
        Ok((authority.execution_id.value, authority.pane_id.value))
    }

    fn adopt(
        &self,
        _authority: &WorkerAuthority,
        _spec: &bmux_cluster_plugin_api::cluster_types::WorkerAdoptionSpec,
    ) -> Result<(uuid::Uuid, uuid::Uuid), WorkerServiceError> {
        unreachable!()
    }

    fn input(&self, _: uuid::Uuid, _: uuid::Uuid, _: &[u8]) -> Result<(), WorkerServiceError> {
        Ok(())
    }

    fn output(
        &self,
        _: uuid::Uuid,
        _: uuid::Uuid,
        cursor: u64,
        _: u32,
    ) -> Result<bmux_pane_runtime_state::OutputRead, WorkerServiceError> {
        Ok(bmux_pane_runtime_state::OutputRead {
            bytes: Vec::new(),
            retained_start: cursor,
            stream_start: cursor,
            stream_end: cursor,
            source_end: cursor,
            stream_gap: false,
        })
    }

    fn resize(
        &self,
        _: uuid::Uuid,
        _: uuid::Uuid,
        _: u16,
        _: u16,
    ) -> Result<(), WorkerServiceError> {
        Ok(())
    }
    fn signal(
        &self,
        _: uuid::Uuid,
        _: uuid::Uuid,
        _: bmux_cluster_plugin_api::cluster_types::WorkerSignal,
    ) -> Result<(), WorkerServiceError> {
        Ok(())
    }
    fn close(&self, _: uuid::Uuid, _: uuid::Uuid) -> Result<(), WorkerServiceError> {
        Ok(())
    }
    fn restart(
        &self,
        _: uuid::Uuid,
        _: uuid::Uuid,
        _: &WorkerLaunchSpec,
    ) -> Result<(uuid::Uuid, uuid::Uuid), WorkerServiceError> {
        unreachable!()
    }
    fn snapshot(&self, _: uuid::Uuid, _: uuid::Uuid) -> Result<(u64, Vec<u8>), WorkerServiceError> {
        Ok((0, Vec::new()))
    }
    fn contains(&self, _: uuid::Uuid, _: uuid::Uuid) -> bool {
        true
    }
}

struct IdentityVerifier(BTreeMap<String, NodeIdentity>);

impl WorkerLeaseVerifier for IdentityVerifier {
    fn verify(&self, authority: &WorkerAuthority, payload: &[u8]) -> Result<(), String> {
        self.0
            .get(&authority.issuer_node_id)
            .ok_or_else(|| "unknown test lease issuer".to_string())?
            .verify(payload, &authority.lease_signature)
    }
}

struct UnusedRuntimeOps;
impl crate::ClusterRuntimeOps for UnusedRuntimeOps {
    fn core_cli_command_run_path(
        &self,
        _: &bmux_plugin_sdk::CoreCliCommandRequest,
    ) -> Result<bmux_plugin_sdk::CoreCliCommandResponse, String> {
        unreachable!()
    }
    fn session_list(&self) -> Result<crate::SessionListResponse, String> {
        unreachable!()
    }
    fn session_create(
        &self,
        _: &crate::SessionCreateRequest,
    ) -> Result<crate::SessionCreateResponse, String> {
        unreachable!()
    }
    fn session_select(
        &self,
        _: &crate::SessionSelectRequest,
    ) -> Result<crate::SessionSelectResponse, String> {
        unreachable!()
    }
    fn pane_list(&self, _: &crate::PaneListRequest) -> Result<crate::PaneListResponse, String> {
        unreachable!()
    }
    fn pane_launch(
        &self,
        _: &crate::PaneLaunchRequest,
    ) -> Result<crate::PaneLaunchResponse, String> {
        unreachable!()
    }
    fn pane_close(&self, _: &crate::PaneCloseRequest) -> Result<crate::PaneCloseResponse, String> {
        unreachable!()
    }
    fn storage_get(
        &self,
        _: &bmux_plugin_sdk::StorageGetRequest,
    ) -> Result<bmux_plugin_sdk::StorageGetResponse, String> {
        unreachable!()
    }
    fn storage_set(&self, _: &bmux_plugin_sdk::StorageSetRequest) -> Result<(), String> {
        unreachable!()
    }
}

fn capabilities(worker: bool) -> ClusterNodeCapabilities {
    ClusterNodeCapabilities {
        consensus_role: ClusterConsensusRole::Voter,
        worker,
        ingress: true,
    }
}

fn command(id: u128, request: ControlCommandRequest) -> ControlCommand {
    ControlCommand {
        schema_version: 1,
        principal_id: "principal:test".to_string(),
        command_id: CommandId {
            value: uuid::Uuid::from_u128(id),
        },
        issued_at_unix_ms: crate::now_unix_ms(),
        request,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines)]
async fn leader_failover_recovers_committed_launch_without_duplicate_execution() {
    let network = InMemoryNetworkFactory::default();
    let roots = [
        TempDir::new().unwrap(),
        TempDir::new().unwrap(),
        TempDir::new().unwrap(),
    ];
    let ids = [NodeId::from(71), NodeId::from(72), NodeId::from(73)];
    let endpoints = ["memory://one", "memory://two", "memory://three"];
    let mut nodes = Vec::new();
    for index in 0..3 {
        let node = crate::consensus_runtime::ConsensusNode::start(
            roots[index].path(),
            "cluster:00000000-0000-0000-0000-000000000001",
            ids[index],
            network.clone(),
        )
        .await
        .unwrap();
        network.register(
            ids[index],
            openraft::BasicNode::new(endpoints[index]),
            node.raft().clone(),
        );
        nodes.push(node);
    }
    nodes[0]
        .initialize_single(ids[0], endpoints[0])
        .await
        .unwrap();
    nodes[0]
        .change_voters(BTreeMap::from([
            (ids[0], openraft::BasicNode::new(endpoints[0])),
            (ids[1], openraft::BasicNode::new(endpoints[1])),
            (ids[2], openraft::BasicNode::new(endpoints[2])),
        ]))
        .await
        .unwrap();
    let leader_id = wait_for_leader(&nodes.iter().collect::<Vec<_>>()).await;
    let leader_index = ids.iter().position(|id| *id == leader_id).unwrap();
    let identities = [
        NodeIdentity::new_for_test(71),
        NodeIdentity::new_for_test(72),
        NodeIdentity::new_for_test(73),
    ];
    let issuer = identities[leader_index].clone();
    let worker_identity = NodeIdentity::new_for_test(99);
    let cluster_id: ClusterId = "cluster:00000000-0000-0000-0000-000000000001"
        .parse()
        .unwrap();
    let now = crate::now_unix_ms();
    let mut issuer_member = crate::membership::issue_test_member(
        &issuer,
        cluster_id,
        &issuer,
        endpoints[leader_index],
        capabilities(false),
        now,
    );
    issuer_member.node_id = leader_id.to_string();
    let worker_member = crate::membership::issue_test_member(
        &issuer,
        cluster_id,
        &worker_identity,
        "memory://worker",
        capabilities(true),
        now,
    );
    let workspace_id = WorkspaceId {
        value: uuid::Uuid::from_u128(10),
    };
    let window_id = LogicalWindowId {
        value: uuid::Uuid::from_u128(20),
    };
    let pane_id = LogicalPaneId {
        value: uuid::Uuid::from_u128(30),
    };
    for request in [
        ControlCommandRequest::UpsertMember {
            member: worker_member.clone(),
        },
        ControlCommandRequest::CreateWorkspace {
            workspace_id: workspace_id.clone(),
            name: None,
        },
        ControlCommandRequest::PutWindow {
            window: LogicalWindowRecord {
                window_id: window_id.clone(),
                workspace_id: workspace_id.clone(),
                name: None,
                layout_schema_version: 1,
                layout: Vec::new(),
                revision: 0,
            },
            expected_workspace_revision: 2,
        },
        ControlCommandRequest::PutPane {
            pane: LogicalPaneRecord {
                pane_id: pane_id.clone(),
                workspace_id: workspace_id.clone(),
                window_id: window_id.clone(),
                name: None,
                restart_policy: PaneRestartPolicy::Manual,
                placement: PlacementIntent {
                    explicit_node_id: Some(worker_member.node_id.clone()),
                    required_labels: Vec::new(),
                    preferred_labels: Vec::new(),
                },
                availability: PaneAvailability::Pending,
                availability_reason: None,
                execution: None,
                revision: 0,
            },
            expected_workspace_revision: 3,
        },
    ] {
        nodes[leader_index]
            .mutate(command(uuid::Uuid::new_v4().as_u128(), request))
            .await
            .unwrap();
    }
    let assignment = ExecutionAssignment {
        node_id: worker_member.node_id.clone(),
        generation: 1,
        execution_id: bmux_cluster_plugin_api::cluster_types::ExecutionId {
            value: uuid::Uuid::from_u128(40),
        },
    };
    let original = command(
        50,
        ControlCommandRequest::AssignExecution {
            pane_id: pane_id.clone(),
            expected_revision: 4,
            expected_generation: 0,
            assignment: assignment.clone(),
            launch_spec: Some(WorkerLaunchSpec {
                program: Some("sh".to_string()),
                args: Vec::new(),
                cwd: None,
                env: Vec::new(),
                cols: 80,
                rows: 24,
            }),
        },
    );
    assert_eq!(
        nodes[leader_index]
            .mutate(original)
            .await
            .unwrap()
            .workflow_status,
        ControlWorkflowStatus::Pending
    );
    let runtime = RecordingRuntime::default();
    let launches = runtime.launches.clone();
    let launch_observed = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let worker: TestWorkerClient<_, _, UnusedRuntimeOps> = TestWorkerClient::new(
        "memory://worker",
        WorkerServiceHandle::new(crate::worker_runtime::WorkerRegistry::new(
            worker_member.node_id.clone(),
            runtime,
            IdentityVerifier(
                identities
                    .iter()
                    .map(|identity| (identity.node_id().to_string(), identity.clone()))
                    .collect(),
            ),
        )),
    )
    .with_launch_gate(launch_observed.clone(), release.clone());
    let lost_response = tokio::spawn({
        let worker = worker.clone();
        let issuer = issuer.clone();
        let node = nodes[leader_index].clone();
        async move {
            reconcile_pending(
                &worker,
                &issuer,
                &node,
                node.read_linearizable_view().await.unwrap(),
            )
            .await
        }
    });
    launch_observed.notified().await;
    lost_response.abort();
    let _ = lost_response.await;
    release.notify_waiters();
    assert_eq!(*launches.lock().unwrap(), 1);
    assert_eq!(
        nodes[leader_index]
            .read_linearizable_view()
            .await
            .unwrap()
            .pending_workflows
            .len(),
        1
    );

    let old_leader = nodes.remove(leader_index);
    network.unregister(leader_id);
    old_leader.shutdown().await.unwrap();
    let replacement_id =
        wait_for_leader_excluding(&nodes.iter().collect::<Vec<_>>(), leader_id).await;
    let replacement_index = ids.iter().position(|id| *id == replacement_id).unwrap();
    let replacement_node = nodes
        .iter()
        .find(|node| node.node_id() == replacement_id)
        .unwrap();
    reconcile_pending(
        &worker,
        &identities[replacement_index],
        replacement_node,
        replacement_node.read_linearizable_view().await.unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(*launches.lock().unwrap(), 1);
    let view = replacement_node.read_linearizable_view().await.unwrap();
    assert!(view.pending_workflows.is_empty());
    assert_eq!(view.panes[0].availability, PaneAvailability::Ready);
    assert_eq!(view.panes[0].execution, Some(assignment));
    for node in nodes {
        node.shutdown().await.unwrap();
    }
}
