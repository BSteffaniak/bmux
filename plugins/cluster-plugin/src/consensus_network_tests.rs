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

struct ForwardingCaller {
    // Retain the selected durable scope for the whole forwarding fixture.
    _root: TempDir,
    connection: bmux_plugin_sdk::HostConnectionInfo,
    storage: Mutex<std::collections::BTreeMap<String, Vec<u8>>>,
    leader: Mutex<Option<Arc<ControlServiceHandle<Self>>>>,
    forwarded: Mutex<Vec<String>>,
    drop_next_mutation_response: std::sync::atomic::AtomicBool,
    incompatible_protocol: std::sync::atomic::AtomicBool,
}

impl Default for ForwardingCaller {
    fn default() -> Self {
        let root = tempfile::tempdir().expect("owned forwarding storage");
        let data_dir = root.path().to_string_lossy().into_owned();
        Self {
            _root: root,
            connection: bmux_plugin_sdk::HostConnectionInfo {
                config_dir: data_dir.clone(),
                config_dir_candidates: Vec::new(),
                runtime_dir: data_dir.clone(),
                state_dir: data_dir.clone(),
                data_dir,
            },
            storage: Mutex::default(),
            leader: Mutex::default(),
            forwarded: Mutex::default(),
            drop_next_mutation_response: std::sync::atomic::AtomicBool::new(false),
            incompatible_protocol: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl ForwardingCaller {
    fn bounded_forwarding_response(
        &self,
        leader: &ControlServiceHandle<Self>,
        runtime: &tokio::runtime::Handle,
        operation: &str,
        payload: &[u8],
    ) -> Result<Vec<u8>, Box<PluginError>> {
        use bmux_cluster_plugin_api::cluster_bounded_control_command::{
            ClusterBoundedControlCommandService, client::MutateLocalRequest,
        };
        if operation == "forwarding-protocol" {
            let protocol = if self
                .incompatible_protocol
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                0u16
            } else {
                tokio::task::block_in_place(|| {
                    runtime.block_on(ClusterBoundedControlCommandService::forwarding_protocol(
                        leader,
                    ))
                })
            };
            return Ok(encode_service_message(&protocol)?);
        }
        let request: MutateLocalRequest = decode_service_message(payload)?;
        let response = tokio::task::block_in_place(|| {
            runtime.block_on(ClusterBoundedControlCommandService::mutate_local(
                leader,
                request.request,
            ))
        });
        if self
            .drop_next_mutation_response
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(Box::new(PluginError::UnsupportedHostOperation {
                operation: "injected bounded response loss",
            }));
        }
        Ok(encode_service_message(&response)?)
    }
    fn storage_get_for_test(&self, payload: &[u8]) -> Result<Vec<u8>, Box<PluginError>> {
        let request: bmux_plugin_sdk::StorageGetRequest = decode_service_message(payload)?;
        Ok(encode_service_message(
            &bmux_plugin_sdk::StorageGetResponse {
                value: self
                    .storage
                    .lock()
                    .unwrap()
                    .get(request.key.as_str())
                    .cloned(),
            },
        )?)
    }

    fn storage_set_for_test(&self, payload: &[u8]) -> Result<Vec<u8>, Box<PluginError>> {
        let request: bmux_plugin_sdk::StorageSetRequest = decode_service_message(payload)?;
        self.storage
            .lock()
            .unwrap()
            .insert(request.key.as_str().into(), request.value);
        Ok(encode_service_message(&())?)
    }

    fn storage_call(
        &self,
        interface: &str,
        operation: &str,
        payload: &[u8],
    ) -> Option<PluginResult<Vec<u8>>> {
        match (interface, operation) {
            ("storage-query/v1", "get") => {
                Some(self.storage_get_for_test(payload).map_err(|error| *error))
            }
            ("storage-command/v1", "set") => {
                Some(self.storage_set_for_test(payload).map_err(|error| *error))
            }
            _ => None,
        }
    }

    fn set_leader(&self, leader: Arc<ControlServiceHandle<Self>>) {
        *self.leader.lock().unwrap() = Some(leader);
    }
}

fn forwarding_runtime() -> Result<tokio::runtime::Handle, Box<PluginError>> {
    tokio::runtime::Handle::try_current().map_err(|_| {
        Box::new(PluginError::UnsupportedHostOperation {
            operation: "missing runtime",
        })
    })
}

impl ServiceCaller for ForwardingCaller {
    fn host_connection(&self) -> Option<&bmux_plugin_sdk::HostConnectionInfo> {
        Some(&self.connection)
    }

    fn call_service_raw(
        &self,
        _capability: &str,
        _kind: ServiceKind,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> PluginResult<Vec<u8>> {
        if let Some(response) = self.storage_call(interface_id, operation, &payload) {
            return response;
        }
        if interface_id != connections_commands::INTERFACE_ID.as_str()
            || operation != connections_commands::OP_INVOKE_SERVICE.as_str()
        {
            return Err(PluginError::UnsupportedHostOperation {
                operation: "unexpected forwarded service",
            });
        }
        let request: connections_commands::client::InvokeServiceRequest =
            bmux_plugin_sdk::decode_service_message(
                bmux_plugin_sdk::service_contract::decode_for(
                    interface_id,
                    connections_commands::SERVICE_CONTRACT,
                    &payload,
                )?,
            )?;
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
        let runtime = forwarding_runtime().map_err(|error| *error)?;
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
            (
                "cluster-bounded-control-command/v1",
                operation @ ("forwarding-protocol" | "mutate-local"),
            ) => self
                .bounded_forwarding_response(
                    leader.as_ref(),
                    &runtime,
                    operation,
                    &invocation.payload,
                )
                .map_err(|error| *error)?,
            ("cluster-peer-auth/v1", "challenge") => {
                let request: bmux_cluster_plugin_api::cluster_peer_auth::client::ChallengeRequest =
                    decode_service_message(&invocation.payload)?;
                let response =
                    crate::membership::create_peer_auth_challenge(leader.caller.as_ref(), &request)
                        .unwrap();
                encode_service_message(&response)?
            }
            ("cluster-peer-read/v1", "read_linearizable") => {
                let request: bmux_cluster_plugin_api::cluster_peer_read::client::ReadLinearizableRequest = decode_service_message(&invocation.payload)?;
                let (_, query) = authenticate_request(
                    leader.caller.as_ref(),
                    PEER_READ_OPERATION,
                    leader.local_node_id,
                    request.request,
                )
                .unwrap();
                decode_service_message::<()>(&query)?;
                let response = tokio::task::block_in_place(|| {
                    runtime.block_on(leader.active().unwrap().read_linearizable_view())
                });
                encode_service_message(&response)?
            }
            _ => {
                return Err(PluginError::UnsupportedHostOperation {
                    operation: "unexpected forwarded invocation",
                });
            }
        };
        let response = bmux_plugin_sdk::encode_service_message(
            &Result::<Vec<u8>, ConnectionError>::Ok(response_payload),
        )?;
        Ok(bmux_plugin_sdk::service_contract::encode_response(
            &response,
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

fn enrolled_forwarding_callers() -> (
    Arc<ForwardingCaller>,
    Arc<ForwardingCaller>,
    crate::ClusterJoinResult,
) {
    let leader_caller = Arc::new(ForwardingCaller::default());
    let caller = Arc::new(ForwardingCaller::default());
    crate::membership::load_or_create_node_identity(leader_caller.as_ref()).unwrap();
    let follower_identity =
        crate::membership::load_or_create_node_identity(caller.as_ref()).unwrap();
    crate::membership::initialize_cluster(leader_caller.as_ref()).unwrap();
    crate::membership::configure_local_consensus_endpoint(
        leader_caller.as_ref(),
        Some("tls://leader:7443"),
    )
    .unwrap();
    let token = crate::membership::create_enrollment_token(
        leader_caller.as_ref(),
        "forwarding",
        "tls://leader:7443",
        Some(60_000),
        None,
    )
    .unwrap();
    let signed =
        crate::membership::decode_and_verify_enrollment_token(&token.token, crate::now_unix_ms())
            .unwrap();
    let endpoint = Some("tls://follower:7443".to_string());
    let protocol = crate::membership::current_protocol_offer();
    let possession_signature = crate::membership::create_enrollment_possession_proof(
        caller.as_ref(),
        &signed,
        endpoint.clone(),
        &protocol,
    )
    .unwrap();
    let enrollment = crate::membership::redeem_enrollment(
        leader_caller.as_ref(),
        &bmux_cluster_plugin_api::cluster_command::client::RedeemEnrollmentRequest {
            token: token.token.clone(),
            node_id: follower_identity.node_id().to_string(),
            public_key: follower_identity.public_key().to_string(),
            endpoint,
            protocol,
            possession_signature,
        },
    )
    .unwrap();
    crate::membership::adopt_join_result(
        caller.as_ref(),
        &bmux_cluster_plugin_api::cluster_command::client::JoinRequest {
            token: token.token,
            issuer: "tls://leader:7443".into(),
            enrollment_result: enrollment.clone(),
        },
    )
    .unwrap();
    (leader_caller, caller, enrollment)
}

async fn start_forwarding_nodes(
    leader_root: &TempDir,
    follower_root: &TempDir,
    leader_id: NodeId,
    follower_id: NodeId,
    cluster_id: &str,
) -> (ConsensusNode, ConsensusNode) {
    let network = crate::consensus_runtime::tests::InMemoryNetworkFactory::default();
    let leader = ConsensusNode::start(leader_root.path(), cluster_id, leader_id, network.clone())
        .await
        .unwrap();
    let follower = ConsensusNode::start(
        follower_root.path(),
        cluster_id,
        follower_id,
        network.clone(),
    )
    .await
    .unwrap();
    network.register(
        leader_id,
        BasicNode::new("tls://leader:7443"),
        leader.raft().clone(),
    );
    network.register(
        follower_id,
        BasicNode::new("tls://follower:7443"),
        follower.raft().clone(),
    );
    leader
        .initialize_single(leader_id, "tls://leader:7443")
        .await
        .unwrap();
    crate::consensus_runtime::tests::wait_for_leader(&[&leader]).await;
    leader
        .raft()
        .add_learner(follower_id, BasicNode::new("tls://follower:7443"), true)
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
    (leader, follower)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn follower_forwards_mutations_and_linearizable_reads_once() {
    let leader_root = TempDir::new().unwrap();
    let follower_root = TempDir::new().unwrap();
    let (leader_caller, caller, enrollment) = enrolled_forwarding_callers();
    let leader_identity = crate::membership::load_node_identity(leader_caller.as_ref())
        .unwrap()
        .unwrap();
    let follower_identity = crate::membership::load_node_identity(caller.as_ref())
        .unwrap()
        .unwrap();
    let leader_id = *leader_identity.node_id();
    let follower_id = *follower_identity.node_id();
    let (leader, follower) = start_forwarding_nodes(
        &leader_root,
        &follower_root,
        leader_id,
        follower_id,
        &enrollment.member.cluster_id,
    )
    .await;

    let registry = ConsensusNodeRegistry::default();
    registry.insert(leader_id, leader.clone()).unwrap();
    registry.insert(follower_id, follower.clone()).unwrap();
    crate::consensus_membership::publish_members(
        leader.clone(),
        &leader_id.to_string(),
        enrollment.members,
    )
    .await
    .unwrap();
    let leader_service = Arc::new(ControlServiceHandle::new(
        leader_caller,
        leader_id,
        registry.clone(),
    ));
    caller.set_leader(leader_service);
    let follower_service = ControlServiceHandle::new(caller.clone(), follower_id, registry.clone());

    let before = leader.read_linearizable_view().await.unwrap();
    let mut metadata_bypass = command(99, 999);
    metadata_bypass.request = ControlCommandRequest::SetMemberState {
        node_id: before.members[0].node_id.clone(),
        expected_credential_serial: before.members[0].credential_serial.clone(),
        state: bmux_cluster_plugin_api::cluster_types::ClusterMemberState::Left,
    };
    assert!(matches!(
        follower_service.mutate(metadata_bypass.clone()).await,
        Err(bmux_cluster_plugin_api::cluster_types::ControlServiceError::Rejected { .. })
    ));
    metadata_bypass.request = ControlCommandRequest::UpsertMember {
        member: before.members[0].clone(),
    };
    assert!(matches!(
        follower_service.mutate(metadata_bypass).await,
        Err(bmux_cluster_plugin_api::cluster_types::ControlServiceError::Rejected { .. })
    ));
    assert_eq!(
        leader.read_linearizable_view().await.unwrap().revision,
        before.revision
    );
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
            "cluster-peer-auth/v1/challenge".to_string(),
            "cluster-peer-read/v1/read_linearizable".to_string(),
        ]
    );

    assert_bounded_forwarding(&caller, &follower_service, &leader).await;
    activate_bounded_members(&leader).await;
    assert_activated_forwarding(&caller, &follower_service, &leader).await;
    assert_incompatible_forwarding(&caller, &follower_service).await;
    assert_redirect_stops(&caller, &follower_service, follower_id, registry).await;
    leader.shutdown().await.unwrap();
    follower.shutdown().await.unwrap();
}

fn bounded_invocations(count: usize) -> Vec<String> {
    (0..count)
        .flat_map(|_| ["forwarding-protocol", "mutate-local"])
        .map(|operation| format!("cluster-bounded-control-command/v1/{operation}"))
        .collect()
}

async fn assert_incompatible_forwarding(
    caller: &ForwardingCaller,
    service: &ControlServiceHandle<ForwardingCaller>,
) {
    caller.forwarded.lock().unwrap().clear();
    caller
        .incompatible_protocol
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let result = bmux_cluster_plugin_api::cluster_bounded_control_command::ClusterBoundedControlCommandService::mutate(service, command(800, 800)).await;
    assert!(
        matches!(result, Err(ControlServiceError::RuntimeUnavailable { reason }) if reason.contains("unsupported bounded forwarding protocol"))
    );
    assert_eq!(
        *caller.forwarded.lock().unwrap(),
        vec!["cluster-bounded-control-command/v1/forwarding-protocol".to_string()]
    );
    caller
        .incompatible_protocol
        .store(false, std::sync::atomic::Ordering::SeqCst);
}

async fn assert_redirect_stops(
    caller: &Arc<ForwardingCaller>,
    service: &ControlServiceHandle<ForwardingCaller>,
    follower_id: NodeId,
    registry: ConsensusNodeRegistry,
) {
    caller.set_leader(Arc::new(ControlServiceHandle::new(
        caller.clone(),
        follower_id,
        registry,
    )));
    caller.forwarded.lock().unwrap().clear();
    // Route the forwarded hop back to a follower with the same leader hint.
    // It must return guidance instead of recursively issuing another RPC.
    let result = bmux_cluster_plugin_api::cluster_bounded_control_command::ClusterBoundedControlCommandService::mutate(service, command(700, 700)).await;
    assert!(matches!(result, Err(ControlServiceError::NotLeader { .. })));
    assert_eq!(*caller.forwarded.lock().unwrap(), bounded_invocations(1));
}

async fn activate_bounded_members(leader: &ConsensusNode) {
    let issuer = crate::membership::NodeIdentity::new_for_test(51);
    for seed in [51, 52] {
        let identity = crate::membership::NodeIdentity::new_for_test(seed);
        let member = crate::membership::issue_test_member(
            &issuer,
            leader.cluster_id().parse().unwrap(),
            &identity,
            &format!("tls://127.0.0.1:{}", 49000 + seed),
            crate::membership::initializer_capabilities(),
            42,
        );
        let mut publish = command(u128::from(seed), 0);
        publish.request = ControlCommandRequest::UpsertMember { member };
        leader.mutate(publish).await.unwrap();
    }
    leader
        .activate_feature(crate::control_codec::FeatureActivationCommand {
            principal_id: "principal:forward".into(),
            command_id: CommandId {
                value: uuid::Uuid::from_u128(500),
            },
            issued_at_unix_ms: 500,
            expected_control_revision: leader.persisted_control_state().unwrap().revision,
            read_schema_floor: 2,
            write_schema_floor: 2,
            feature: "bounded-control-command-v1".into(),
        })
        .await
        .unwrap();
}

async fn assert_activated_forwarding(
    caller: &ForwardingCaller,
    service: &ControlServiceHandle<ForwardingCaller>,
    leader: &ConsensusNode,
) {
    use bmux_cluster_plugin_api::cluster_bounded_control_command::ClusterBoundedControlCommandService;
    caller.forwarded.lock().unwrap().clear();
    caller
        .drop_next_mutation_response
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(matches!(
        ClusterBoundedControlCommandService::mutate(service, command(600, 600)).await,
        Err(ControlServiceError::RuntimeUnavailable { .. })
    ));
    let revision = leader.persisted_control_state().unwrap().revision;
    let response = ClusterBoundedControlCommandService::mutate(service, command(600, 600))
        .await
        .unwrap();
    assert_eq!(response.control_revision, revision);
    assert_eq!(leader.persisted_control_state().unwrap().revision, revision);
    let mut oversized = command(601, 0);
    oversized.request = ControlCommandRequest::CompleteWorkflow {
        original_command_id: CommandId {
            value: uuid::Uuid::from_u128(600),
        },
        response: vec![0; 1_048_576],
    };
    assert!(matches!(
        ClusterBoundedControlCommandService::mutate(service, oversized).await,
        Err(ControlServiceError::Rejected { .. })
    ));
    assert_eq!(leader.persisted_control_state().unwrap().revision, revision);
    assert_eq!(*caller.forwarded.lock().unwrap(), bounded_invocations(3));
}

async fn assert_bounded_forwarding(
    caller: &ForwardingCaller,
    follower_service: &ControlServiceHandle<ForwardingCaller>,
    leader: &ConsensusNode,
) {
    caller.forwarded.lock().unwrap().clear();
    let bounded = || {
        bmux_cluster_plugin_api::cluster_bounded_control_command::ClusterBoundedControlCommandService::mutate(follower_service, command(3, 300))
    };
    assert!(matches!(
        bounded().await,
        Err(ControlServiceError::Rejected { .. })
    ));
    caller
        .drop_next_mutation_response
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(matches!(
        bounded().await,
        Err(ControlServiceError::RuntimeUnavailable { .. })
    ));
    assert!(matches!(
        bounded().await,
        Err(ControlServiceError::Rejected { .. })
    ));
    assert_eq!(*caller.forwarded.lock().unwrap(), bounded_invocations(3),);
    assert!(
        !leader
            .persisted_control_state()
            .unwrap()
            .workspaces
            .contains_key(&uuid::Uuid::from_u128(300))
    );
}
