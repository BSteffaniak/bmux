use super::*;
use bmux_cluster_plugin_api::cluster_control_command::ClusterControlCommandService;
use bmux_cluster_plugin_api::cluster_control_state::ClusterControlStateService;
use bmux_cluster_plugin_api::cluster_types::{
    CommandId, ControlCommand, ControlCommandRequest, ControlReadConsistency, WorkspaceId,
};
use bmux_connections_plugin_api::{connection_types::ConnectionError, connections_commands};
use bmux_plugin_sdk::{PluginError, Result as PluginResult, ServiceKind};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

#[derive(Default)]
struct ForwardingCaller {
    leader: Mutex<Option<Arc<ControlServiceHandle<Self>>>>,
    forwarded: Mutex<Vec<String>>,
    drop_next_mutation_response: std::sync::atomic::AtomicBool,
}

impl ForwardingCaller {
    fn set_leader(&self, leader: Arc<ControlServiceHandle<Self>>) {
        *self.leader.lock().unwrap() = Some(leader);
    }
}

impl ServiceCaller for ForwardingCaller {
    fn call_service_raw(
        &self,
        _capability: &str,
        _kind: ServiceKind,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> PluginResult<Vec<u8>> {
        if interface_id != connections_commands::INTERFACE_ID.as_str()
            || operation != connections_commands::OP_INVOKE_SERVICE.as_str()
        {
            return Err(PluginError::UnsupportedHostOperation {
                operation: "unexpected forwarded service",
            });
        }
        let request: connections_commands::client::InvokeServiceRequest =
            bmux_plugin_sdk::decode_service_message(&payload)?;
        let invocation = request.invocation;
        self.forwarded.lock().unwrap().push(format!(
            "{}/{}",
            invocation.interface_id, invocation.operation
        ));
        let leader =
            self.leader
                .lock()
                .unwrap()
                .clone()
                .ok_or(PluginError::UnsupportedHostOperation {
                    operation: "missing leader",
                })?;
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| {
            PluginError::UnsupportedHostOperation {
                operation: "missing runtime",
            }
        })?;
        let response_payload = match (
            invocation.interface_id.as_str(),
            invocation.operation.as_str(),
        ) {
            ("cluster-control-command/v1", "mutate") => {
                let request: bmux_cluster_plugin_api::cluster_control_command::client::MutateRequest =
                    bmux_plugin_sdk::decode_service_message(&invocation.payload)?;
                let response = tokio::task::block_in_place(|| {
                    runtime.block_on(leader.mutate(request.request))
                });
                if self
                    .drop_next_mutation_response
                    .swap(false, std::sync::atomic::Ordering::SeqCst)
                {
                    return Err(PluginError::UnsupportedHostOperation {
                        operation: "injected response loss",
                    });
                }
                bmux_plugin_sdk::encode_service_message(&response)?
            }
            ("cluster-control-state/v1", "read_linearizable") => {
                let response =
                    tokio::task::block_in_place(|| runtime.block_on(leader.read_linearizable()));
                bmux_plugin_sdk::encode_service_message(&response)?
            }
            _ => {
                return Err(PluginError::UnsupportedHostOperation {
                    operation: "unexpected forwarded invocation",
                });
            }
        };
        bmux_plugin_sdk::encode_service_message(&Result::<Vec<u8>, ConnectionError>::Ok(
            response_payload,
        ))
    }

    fn execute_kernel_request(
        &self,
        _request: bmux_ipc::Request,
    ) -> PluginResult<bmux_ipc::ResponsePayload> {
        Err(PluginError::UnsupportedHostOperation {
            operation: "execute_kernel_request",
        })
    }
}

fn command(id: u128, workspace: u128) -> ControlCommand {
    ControlCommand {
        schema_version: 1,
        principal_id: "principal:forward".to_string(),
        command_id: CommandId {
            value: uuid::Uuid::from_u128(id),
        },
        issued_at_unix_ms: u64::try_from(id).unwrap(),
        request: ControlCommandRequest::CreateWorkspace {
            workspace_id: WorkspaceId {
                value: uuid::Uuid::from_u128(workspace),
            },
            name: Some("forwarded".to_string()),
        },
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn follower_forwards_mutations_and_linearizable_reads_once() {
    let network = crate::consensus_runtime::tests::InMemoryNetworkFactory::default();
    let leader_root = TempDir::new().unwrap();
    let follower_root = TempDir::new().unwrap();
    let leader_id = NodeId::from(51);
    let follower_id = NodeId::from(52);
    let leader = ConsensusNode::start(
        leader_root.path(),
        "cluster-forward-test",
        leader_id,
        network.clone(),
    )
    .await
    .unwrap();
    let follower = ConsensusNode::start(
        follower_root.path(),
        "cluster-forward-test",
        follower_id,
        network.clone(),
    )
    .await
    .unwrap();
    network.register(
        leader_id,
        BasicNode::new("memory://leader"),
        leader.raft().clone(),
    );
    network.register(
        follower_id,
        BasicNode::new("memory://follower"),
        follower.raft().clone(),
    );
    leader
        .initialize_single(leader_id, "memory://leader")
        .await
        .unwrap();
    crate::consensus_runtime::tests::wait_for_leader(&[&leader]).await;
    leader
        .raft()
        .add_learner(follower_id, BasicNode::new("memory://follower"), true)
        .await
        .unwrap();
    leader
        .raft()
        .change_membership(
            std::collections::BTreeSet::from([leader_id, follower_id]),
            false,
        )
        .await
        .unwrap();

    let registry = ConsensusNodeRegistry::default();
    registry.insert(leader_id, leader.clone()).unwrap();
    registry.insert(follower_id, follower.clone()).unwrap();
    let caller = Arc::new(ForwardingCaller::default());
    let leader_service = Arc::new(ControlServiceHandle::new(
        caller.clone(),
        leader_id,
        registry.clone(),
    ));
    caller.set_leader(leader_service);
    let follower_service = ControlServiceHandle::new(caller.clone(), follower_id, registry);

    follower_service.mutate(command(1, 100)).await.unwrap();
    caller
        .drop_next_mutation_response
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let lost = follower_service.mutate(command(2, 200)).await.unwrap_err();
    assert!(matches!(
        lost,
        bmux_cluster_plugin_api::cluster_types::ControlServiceError::RuntimeUnavailable {
            reason
        } if reason.contains("retry the same CommandId")
    ));
    let replay = follower_service.mutate(command(2, 200)).await.unwrap();
    assert_eq!(replay.command_id.value, uuid::Uuid::from_u128(2));
    let view = follower_service.read_linearizable().await.unwrap();
    assert_eq!(view.consistency, ControlReadConsistency::Linearizable);
    assert!(
        view.workspaces
            .iter()
            .any(|workspace| workspace.workspace_id.value == uuid::Uuid::from_u128(100))
    );
    assert!(
        view.workspaces
            .iter()
            .any(|workspace| workspace.workspace_id.value == uuid::Uuid::from_u128(200))
    );
    assert_eq!(
        *caller.forwarded.lock().unwrap(),
        vec![
            "cluster-control-command/v1/mutate".to_string(),
            "cluster-control-command/v1/mutate".to_string(),
            "cluster-control-command/v1/mutate".to_string(),
            "cluster-control-state/v1/read_linearizable".to_string(),
        ]
    );

    leader.shutdown().await.unwrap();
    follower.shutdown().await.unwrap();
}
