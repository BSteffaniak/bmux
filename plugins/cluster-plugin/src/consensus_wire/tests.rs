use super::snapshot::{
    CHUNK_BYTES, Link, membership_from_wire, membership_to_wire, signed, snapshot_header,
    snapshot_reply, upload, whole,
};
use super::*;
use crate::consensus_driver::{DriverConfig, Jitter};
use crate::consensus_engine::{Config, Engine};
use crate::consensus_model::{Entry, EntryPayload, Membership, StoredMembership};
use crate::control_state::ControlState;
use crate::owned_vote_store::{OwnedStorage, OwnedVoteStore};
use bmux_cluster_plugin_api::cluster_consensus_snapshot_types::{
    InstallOutcome, InstallRejection, InstallRequest, InstallResponse,
};
use bmux_cluster_plugin_api::cluster_consensus_transfer_types::{
    AbortOutcome, ChunkOutcome, FinishOutcome, TransferError,
};
use bmux_cluster_plugin_api::cluster_types::{
    ClusterConsensusRole, ClusterNodeCapabilities, CommandId, ControlCommand,
    ControlCommandRequest, PeerAuthChallenge, PeerAuthProof, WorkspaceId,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Mutex;

// ----- outbound chunking -----------------------------------------------------

/// A receiver holding `staged` bytes that answers like the transfer provider.
struct Receiver {
    staged: u64,
    total: u64,
    sends: Vec<(u64, usize)>,
}

impl Receiver {
    fn receive(&mut self, offset: u64, data: &[u8]) -> ChunkOutcome {
        self.sends.push((offset, data.len()));
        if offset != self.staged {
            return ChunkOutcome::ExpectedOffset {
                offset: self.staged,
            };
        }
        self.staged += data.len() as u64;
        assert!(self.staged <= self.total);
        ChunkOutcome::Accepted {
            offset: self.staged,
        }
    }
}

#[switchy_async::test]
async fn upload_sends_bounded_sequential_chunks() {
    let bytes = vec![7; CHUNK_BYTES * 2 + 10];
    let receiver = Mutex::new(Receiver {
        staged: 0,
        total: bytes.len() as u64,
        sends: Vec::new(),
    });
    let result = upload(&bytes, 0, |offset, data| {
        let outcome = receiver.lock().unwrap().receive(offset, &data);
        async move { Some(outcome) }
    })
    .await;
    assert_eq!(result, Some(()));
    assert_eq!(
        receiver.into_inner().unwrap().sends,
        vec![
            (0, CHUNK_BYTES),
            (CHUNK_BYTES as u64, CHUNK_BYTES),
            (2 * CHUNK_BYTES as u64, 10)
        ]
    );
}

#[switchy_async::test]
async fn upload_follows_expected_offset_and_resumes_from_begin() {
    let bytes = vec![1; CHUNK_BYTES * 3];
    let total = bytes.len() as u64;
    // The receiver already staged one chunk the sender does not know about.
    let receiver = Mutex::new(Receiver {
        staged: CHUNK_BYTES as u64,
        total,
        sends: Vec::new(),
    });
    let result = upload(&bytes, 0, |offset, data| {
        let outcome = receiver.lock().unwrap().receive(offset, &data);
        async move { Some(outcome) }
    })
    .await;
    assert_eq!(result, Some(()));
    assert_eq!(
        receiver.into_inner().unwrap().sends,
        vec![
            (0, CHUNK_BYTES),
            (CHUNK_BYTES as u64, CHUNK_BYTES),
            (2 * CHUNK_BYTES as u64, CHUNK_BYTES)
        ]
    );
    // A begin that reports a continuation offset resumes without resending.
    let receiver = Mutex::new(Receiver {
        staged: 2 * CHUNK_BYTES as u64,
        total,
        sends: Vec::new(),
    });
    let result = upload(&bytes, 2 * CHUNK_BYTES as u64, |offset, data| {
        let outcome = receiver.lock().unwrap().receive(offset, &data);
        async move { Some(outcome) }
    })
    .await;
    assert_eq!(result, Some(()));
    assert_eq!(
        receiver.into_inner().unwrap().sends,
        vec![(2 * CHUNK_BYTES as u64, CHUNK_BYTES)]
    );
    // Everything already staged: nothing to send.
    assert_eq!(
        upload(&bytes, total, |_, _| async { panic!("no chunk expected") }).await,
        Some(())
    );
}

#[switchy_async::test]
async fn upload_fails_without_progress_or_on_out_of_range_offsets() {
    let bytes = vec![2; CHUNK_BYTES + 1];
    let total = bytes.len() as u64;
    let outcomes = [
        ChunkOutcome::Accepted { offset: 0 },
        ChunkOutcome::Accepted { offset: total + 1 },
        ChunkOutcome::ExpectedOffset { offset: total + 1 },
    ];
    for outcome in outcomes {
        let outcome = outcome.clone();
        assert_eq!(
            upload(&bytes, 0, |_, _| {
                let outcome = outcome.clone();
                async move { Some(outcome) }
            })
            .await,
            None,
            "{outcome:?}"
        );
    }
    // A receiver that keeps redirecting is abandoned after bounded resumes.
    let mut sends = 0;
    assert_eq!(
        upload(&bytes, 0, |offset, _| {
            sends += 1;
            let offset = if offset == 0 { CHUNK_BYTES as u64 } else { 0 };
            async move { Some(ChunkOutcome::ExpectedOffset { offset }) }
        })
        .await,
        None
    );
    assert!(sends <= 10);
    // An unanswered chunk fails the upload.
    assert_eq!(upload(&bytes, 0, |_, _| async { None }).await, None);
    assert_eq!(upload(&bytes, total + 1, |_, _| async { None }).await, None);
}

// ----- signed install responses ----------------------------------------------

fn test_snapshot(
    cluster: &str,
    leader: NodeId,
    voters: &[NodeId],
    control: &[u8],
) -> SnapshotObject {
    let introduced = LogIdentity {
        term: Term(0),
        leader,
        index: LogIndex(0),
    };
    let last_included = LogIdentity {
        term: Term(1),
        leader,
        index: LogIndex(3),
    };
    let membership = StoredMembership {
        introduced_at: Some(introduced),
        membership: Membership::new(
            vec![voters.to_vec()],
            voters
                .iter()
                .enumerate()
                .map(|(index, id)| (*id, format!("node-{index}")))
                .collect(),
        )
        .unwrap(),
    };
    let bytes = crate::consensus_storage::encode_owned_snapshot(
        cluster,
        "snapshot",
        last_included,
        &membership,
        control,
    )
    .unwrap();
    SnapshotObject {
        bytes,
        last_included,
        membership,
    }
}

/// Canonical control state encoding at least `min_len` bytes.
fn control_bytes(cluster: &str, min_len: usize) -> Vec<u8> {
    let mut state = ControlState::new(cluster);
    let mut id = 0;
    loop {
        let bytes = state.encode_snapshot().unwrap();
        if bytes.len() >= min_len {
            return bytes;
        }
        for _ in 0..64 {
            id += 1;
            state.apply(&command(id));
        }
    }
}

fn dummy_proof() -> TransferProof {
    TransferProof {
        credential_serial: "serial".into(),
        signature: Vec::new(),
        peer: PeerAuthProof {
            challenge: PeerAuthChallenge {
                protocol_version: 1,
                cluster_id: "cluster".into(),
                verifier_node_id: "verifier".into(),
                verifier_credential_serial: "verifier-serial".into(),
                audience_node_id: "claimant".into(),
                nonce: "nonce".into(),
                issued_at_unix_ms: 1,
                expires_at_unix_ms: 2,
                signature: String::new(),
            },
            claimant_node_id: "claimant".into(),
            claimant_credential_serial: "serial".into(),
            claimant_signature: String::new(),
        },
    }
}

#[test]
fn snapshot_membership_round_trips_through_the_wire() {
    let ids = [1, 2, 3].map(NodeId::from);
    let snapshot = test_snapshot("cluster", ids[0], &ids, &control_bytes("cluster", 0));
    let wire = membership_to_wire(&snapshot.membership).unwrap();
    assert_eq!(membership_from_wire(&wire), Some(snapshot.membership));
}

#[test]
fn install_reply_requires_a_bound_response_signed_by_the_recipient() {
    use crate::membership::NodeIdentity;
    let source = NodeIdentity::new_for_test(1);
    let target = NodeIdentity::new_for_test(2);
    let (local, to) = (*source.node_id(), *target.node_id());
    let snapshot = test_snapshot("cluster", local, &[local, to], &control_bytes("cluster", 0));
    let link = Link {
        local,
        to,
        incarnation: 5,
        transfer: 1,
    };
    let term = Term(4);
    let header = snapshot_header("cluster", link, term, &snapshot).unwrap();
    assert!(snapshot_header("other", link, term, &snapshot).is_none());
    let mut request = InstallRequest {
        request_id: request_id(5, 9),
        reference: header,
        proof: dummy_proof(),
    };
    request.proof.signature = source
        .sign(&crate::consensus_network::owned_snapshot_install_signing_payload(&request).unwrap());
    let sign = |signer: &NodeIdentity, mut response: InstallResponse| {
        response.signature = signer.sign(
            &crate::consensus_network::owned_snapshot_install_response_signing_payload(
                &request, &response,
            )
            .unwrap(),
        );
        response
    };
    let installed = sign(
        &target,
        InstallResponse {
            request_id: request.request_id,
            source: to.as_bytes().to_vec(),
            term: term.0,
            outcome: InstallOutcome::Installed {
                boundary: wire(snapshot.last_included),
            },
            signature: Vec::new(),
        },
    );
    assert_eq!(
        snapshot_reply(&request, &installed, to, term, 9),
        Some(Message::SnapshotReply {
            term,
            seq: 9,
            installed: Some(snapshot.last_included),
        })
    );
    assert_eq!(snapshot_reply(&request, &installed, local, term, 9), None);
    let mut tampered = installed.clone();
    tampered.signature[0] ^= 1;
    assert_eq!(snapshot_reply(&request, &tampered, to, term, 9), None);
    let forged = sign(&source, installed.clone());
    assert_eq!(snapshot_reply(&request, &forged, to, term, 9), None);
    let mut other = request.clone();
    other.request_id = request_id(5, 10);
    assert_eq!(snapshot_reply(&other, &installed, to, term, 9), None);

    let rejected = |reason, at: u64| {
        sign(
            &target,
            InstallResponse {
                request_id: request.request_id,
                source: to.as_bytes().to_vec(),
                term: at,
                outcome: InstallOutcome::Rejected { reason },
                signature: Vec::new(),
            },
        )
    };
    // A signed higher term re-enters so the engine steps down.
    assert_eq!(
        snapshot_reply(
            &request,
            &rejected(InstallRejection::StaleTerm, 7),
            to,
            term,
            9
        ),
        Some(Message::SnapshotReply {
            term: Term(7),
            seq: 9,
            installed: None,
        })
    );
    for (reason, at) in [
        (InstallRejection::StaleTerm, term.0),
        (InstallRejection::UnknownTransfer, term.0),
        (InstallRejection::InvalidSnapshot, 9),
    ] {
        assert_eq!(
            snapshot_reply(&request, &rejected(reason, at), to, term, 9),
            None
        );
    }
}

// ----- in-memory endpoint network --------------------------------------------

type Service = WireService<TestCaller, ControlState>;

#[derive(Default)]
struct Network {
    callers: Mutex<BTreeMap<String, Arc<TestCaller>>>,
    down: Mutex<BTreeSet<String>>,
    /// `(target, interface, operation)` of every delivered invocation.
    trace: Mutex<Vec<(String, String, String)>>,
}

struct TestCaller {
    endpoint: String,
    network: Arc<Network>,
    storage: Mutex<BTreeMap<String, Vec<u8>>>,
    scope: crate::durable_storage::TestScope,
    members: MemberView,
    service: Mutex<Option<Arc<Service>>>,
}

impl bmux_plugin::ServiceCaller for TestCaller {
    fn host_connection(&self) -> Option<&bmux_plugin_sdk::HostConnectionInfo> {
        Some(&self.scope.connection)
    }

    fn execute_kernel_request(
        &self,
        _: bmux_ipc::Request,
    ) -> bmux_plugin_sdk::Result<bmux_ipc::ResponsePayload> {
        Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
            operation: "wire test has no kernel bridge",
        })
    }

    fn call_service_raw(
        &self,
        _: &str,
        _: bmux_plugin_sdk::ServiceKind,
        interface: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> bmux_plugin_sdk::Result<Vec<u8>> {
        use bmux_connections_plugin_api::{
            connection_types::ConnectionError, connections_commands,
        };
        use bmux_plugin_sdk::{
            PluginError, decode_service_message, encode_service_message, service_contract,
        };
        let unsupported = || PluginError::UnsupportedHostOperation {
            operation: "wire test endpoint dispatch",
        };
        if let Some(response) = self.serve_storage(interface, operation, &payload) {
            return response;
        }
        if interface != connections_commands::INTERFACE_ID.as_str()
            || operation != connections_commands::OP_INVOKE_SERVICE.as_str()
        {
            return Err(unsupported());
        }
        let request: connections_commands::client::InvokeServiceRequest =
            decode_service_message(service_contract::decode_for(
                interface,
                connections_commands::SERVICE_CONTRACT,
                &payload,
            )?)?;
        let invocation = request.invocation;
        {
            let down = self.network.down.lock().unwrap();
            if down.contains(&self.endpoint) || down.contains(&invocation.target) {
                return Err(unsupported());
            }
        }
        let remote = self
            .network
            .callers
            .lock()
            .unwrap()
            .get(&invocation.target)
            .cloned()
            .ok_or_else(unsupported)?;
        self.network.trace.lock().unwrap().push((
            invocation.target.clone(),
            invocation.interface_id.clone(),
            invocation.operation.clone(),
        ));
        let result = deliver(&remote, &invocation)?;
        let response = encode_service_message(&Result::<Vec<u8>, ConnectionError>::Ok(result))?;
        Ok(service_contract::encode_response(&response))
    }
}

impl TestCaller {
    fn serve_storage(
        &self,
        interface: &str,
        operation: &str,
        payload: &[u8],
    ) -> Option<bmux_plugin_sdk::Result<Vec<u8>>> {
        use bmux_plugin_sdk::{decode_service_message, encode_service_message};
        match (interface, operation) {
            ("storage-query/v1", "get") => {
                let request: bmux_plugin_sdk::StorageGetRequest =
                    match decode_service_message(payload) {
                        Ok(request) => request,
                        Err(error) => return Some(Err(error)),
                    };
                let value = self
                    .storage
                    .lock()
                    .unwrap()
                    .get(request.key.as_str())
                    .cloned();
                Some(encode_service_message(
                    &bmux_plugin_sdk::StorageGetResponse { value },
                ))
            }
            ("storage-command/v1", "set") => {
                let request: bmux_plugin_sdk::StorageSetRequest =
                    match decode_service_message(payload) {
                        Ok(request) => request,
                        Err(error) => return Some(Err(error)),
                    };
                self.storage
                    .lock()
                    .unwrap()
                    .insert(request.key.as_str().into(), request.value);
                Some(encode_service_message(&()))
            }
            _ => None,
        }
    }
}

/// Serve peer challenges from the remote's member view and every other
/// invocation through the remote's generated portable providers.
fn deliver(
    remote: &Arc<TestCaller>,
    invocation: &bmux_connections_plugin_api::connection_types::ServiceInvocation,
) -> bmux_plugin_sdk::Result<Vec<u8>> {
    use bmux_plugin_sdk::{
        PluginError, decode_service_message, encode_service_message, service_contract,
    };
    let unsupported = || PluginError::UnsupportedHostOperation {
        operation: "wire test endpoint dispatch",
    };
    Ok(
        if (
            invocation.interface_id.as_str(),
            invocation.operation.as_str(),
        ) == ("cluster-peer-auth/v1", "challenge")
        {
            let request: bmux_cluster_plugin_api::cluster_peer_auth::client::ChallengeRequest =
                decode_service_message(&invocation.payload)?;
            let (_, members) = remote.members.snapshot();
            let challenge = crate::membership::create_peer_auth_challenge_for_members(
                remote.as_ref(),
                &request,
                &members,
            )
            .map_err(|_| unsupported())?;
            encode_service_message(&challenge)?
        } else {
            let service = remote
                .service
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(unsupported)?;
            let mut registry = bmux_plugin_sdk::TypedServiceRegistry::new();
            service.register(&mut registry).map_err(|_| unsupported())?;
            let provider = registry
                .portable_providers()
                .into_iter()
                .find(|provider| {
                    provider.descriptor().interface_id.as_str() == invocation.interface_id
                })
                .ok_or_else(unsupported)?;
            let descriptor = provider.descriptor();
            let framed = service_contract::encode(
                descriptor.interface_id.as_str(),
                descriptor.contract,
                &invocation.payload,
            )?;
            let runtime = tokio::runtime::Handle::current();
            let response = tokio::task::block_in_place(|| {
                runtime.block_on(provider.dispatch_checked(&invocation.operation, &framed, None))
            })?;
            service_contract::decode_response(&response)?.to_vec()
        },
    )
}

struct Node {
    driver: Arc<Driver<ControlState>>,
    staging: Arc<SnapshotStaging>,
}

struct Harness {
    network: Arc<Network>,
    callers: Vec<Arc<TestCaller>>,
    paths: Vec<PathBuf>,
    staging: Vec<PathBuf>,
    ids: Vec<NodeId>,
    cluster: String,
    nodes: Vec<Option<Node>>,
    incarnation: u64,
    _root: tempfile::TempDir,
}

fn command(id: u128) -> ControlCommand {
    ControlCommand {
        schema_version: 1,
        principal_id: "principal:test".into(),
        command_id: CommandId {
            value: uuid::Uuid::from_u128(id),
        },
        issued_at_unix_ms: 1,
        request: ControlCommandRequest::CreateWorkspace {
            workspace_id: WorkspaceId {
                value: uuid::Uuid::from_u128(id),
            },
            name: Some(format!("workspace-{id}")),
        },
    }
}

impl Harness {
    /// Three voters with signed credentials, every member record committed in
    /// the shared initial log, and private staging per node.
    #[allow(clippy::too_many_lines)] // One explicit provisioning step.
    fn new(count: usize) -> Self {
        use crate::consensus_model::AppendRequest;
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let network = Arc::new(Network::default());
        let callers = (0..count)
            .map(|index| {
                Arc::new(TestCaller {
                    endpoint: format!("node-{index}"),
                    network: network.clone(),
                    storage: Mutex::new(BTreeMap::new()),
                    scope: crate::durable_storage::TestScope::default(),
                    members: MemberView::default(),
                    service: Mutex::new(None),
                })
            })
            .collect::<Vec<_>>();
        let identities = callers
            .iter()
            .map(|caller| crate::load_or_create_node_identity(caller.as_ref()).unwrap())
            .collect::<Vec<_>>();
        let cluster = crate::membership::ClusterId::generate();
        let protocol = crate::membership::current_protocol_offer();
        let members = identities
            .iter()
            .zip(&callers)
            .map(|(identity, caller)| {
                let mut member = crate::membership::issue_membership_credential(
                    &identities[0],
                    cluster,
                    identity.node_id().to_string(),
                    identity.public_key().to_string(),
                    ClusterNodeCapabilities {
                        ingress: true,
                        worker: true,
                        consensus_role: ClusterConsensusRole::Voter,
                    },
                    bmux_cluster_plugin_api::cluster_types::ClusterNegotiatedProtocol {
                        wire_epoch: protocol.wire_epoch,
                        local_plugin_version: protocol.plugin_version.clone(),
                        remote_plugin_version: protocol.plugin_version.clone(),
                        peer_revision: 1,
                        schema_version: 1,
                        features: [
                            bmux_cluster_plugin_api::cluster_consensus_rpc::INTERFACE_ID,
                            bmux_cluster_plugin_api::cluster_consensus_rpc_v2::INTERFACE_ID,
                            bmux_cluster_plugin_api::cluster_consensus_transfer::INTERFACE_ID,
                            bmux_cluster_plugin_api::cluster_consensus_snapshot::INTERFACE_ID,
                        ]
                        .iter()
                        .map(ToString::to_string)
                        .collect(),
                    },
                    crate::now_unix_ms(),
                )
                .unwrap();
                member.endpoint = Some(caller.endpoint.clone());
                member
            })
            .collect::<Vec<_>>();
        let ids = identities
            .iter()
            .map(|identity| *identity.node_id())
            .collect::<Vec<_>>();
        let membership = Membership::new(
            vec![ids.clone()],
            ids.iter()
                .zip(&callers)
                .map(|(id, caller)| (*id, caller.endpoint.clone()))
                .collect(),
        )
        .unwrap();
        let identity = |index| LogIdentity {
            term: Term(0),
            leader: ids[0],
            index: LogIndex(index),
        };
        let mut entries = vec![Entry {
            identity: identity(0),
            payload: EntryPayload::Membership(membership),
        }];
        for (index, member) in members.iter().enumerate() {
            let mut command = command(u128::try_from(index).unwrap() + 1000);
            command.issued_at_unix_ms = member.credential_issued_at_unix_ms;
            command.request = ControlCommandRequest::UpsertMember {
                member: member.clone(),
            };
            entries.push(Entry {
                identity: identity(u64::try_from(index).unwrap() + 1),
                payload: EntryPayload::Application(crate::control_codec::encode_control_command(
                    &command,
                )),
            });
        }
        let mut paths = Vec::new();
        let mut staging = Vec::new();
        for (index, caller) in callers.iter().enumerate() {
            caller.storage.lock().unwrap().insert(
                crate::membership::CLUSTER_ID_STORAGE_KEY.into(),
                serde_json::to_vec(
                    &serde_json::json!({"version": 1, "cluster_id": cluster.to_string()}),
                )
                .unwrap(),
            );
            let state: crate::membership::MembershipState =
                serde_json::from_value(serde_json::json!({
                    "version": 1, "cluster_id": cluster.to_string(), "issuer_endpoint": null,
                    "members": members
                        .iter()
                        .map(|member| (member.node_id.clone(), member.clone()))
                        .collect::<BTreeMap<_, _>>(),
                    "enrollment_tokens": {}, "accepted_rotations": {}
                }))
                .unwrap();
            crate::membership::store_membership_state(caller.as_ref(), &state).unwrap();
            network
                .callers
                .lock()
                .unwrap()
                .insert(caller.endpoint.clone(), caller.clone());
            let path = root.path().join(format!("node-{index}.redb"));
            let mut store = OwnedVoteStore::create(&path, &cluster.to_string()).unwrap();
            assert!(
                store
                    .append(
                        &AppendRequest {
                            term: Term(0),
                            leader: ids[0],
                            previous: None,
                            leader_commit: Some(identity(u64::try_from(count).unwrap())),
                            entries: entries.clone(),
                        },
                        (64, 1024 * 1024)
                    )
                    .unwrap()
                    .outcome
                    .is_ok()
            );
            store.close().unwrap();
            paths.push(path);
            let directory = root.path().join(format!("staging-{index}"));
            std::fs::create_dir(&directory).unwrap();
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
            staging.push(directory);
        }
        let nodes = (0..count).map(|_| None).collect();
        Self {
            network,
            callers,
            paths,
            staging,
            ids,
            cluster: cluster.to_string(),
            nodes,
            incarnation: 0,
            _root: root,
        }
    }

    fn start(&mut self, index: usize) {
        let local = self.ids[index];
        let caller = self.callers[index].clone();
        let mut store = OwnedVoteStore::open(&self.paths[index], &self.cluster).unwrap();
        let recovered = store.recovered().unwrap();
        let (engine, effects) = Engine::recover(local, Config::default(), recovered).unwrap();
        let staging = SnapshotStaging::open(&self.staging[index], 1 << 24, 1 << 26).unwrap();
        let storage = OwnedStorage::new(store, staging.reader())
            .with_members(caller.members.clone())
            .unwrap();
        self.incarnation += 1;
        let transport = Arc::new(WireTransport::new(
            caller.clone(),
            local,
            caller.members.clone(),
            self.incarnation,
        ));
        let mut state = u64::from_le_bytes(local.as_bytes()[24..].try_into().unwrap());
        let jitter: Jitter = Box::new(move |min, max| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            let span = u64::try_from(max.saturating_sub(min).as_millis().max(1)).unwrap_or(1);
            min + Duration::from_millis((state >> 33) % span)
        });
        let driver = Arc::new(Driver::start(
            engine,
            effects,
            storage,
            transport,
            jitter,
            DriverConfig {
                compaction_interval: 4,
            },
        ));
        *caller.service.lock().unwrap() = Some(Arc::new(WireService::new(
            caller.clone(),
            local,
            caller.members.clone(),
            driver.clone(),
            staging.clone(),
        )));
        self.network.down.lock().unwrap().remove(&caller.endpoint);
        self.nodes[index] = Some(Node { driver, staging });
    }

    async fn stop(&mut self, index: usize) {
        let caller = &self.callers[index];
        self.network
            .down
            .lock()
            .unwrap()
            .insert(caller.endpoint.clone());
        caller.service.lock().unwrap().take();
        let Node {
            mut driver,
            staging,
        } = self.nodes[index].take().unwrap();
        // In-flight provider calls briefly hold the service and its driver.
        for _ in 0..500 {
            match Arc::try_unwrap(driver) {
                Ok(owned) => {
                    owned.shutdown().await;
                    staging.close().unwrap();
                    return;
                }
                Err(shared) => driver = shared,
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("driver {index} is still shared");
    }

    fn driver(&self, index: usize) -> Arc<Driver<ControlState>> {
        self.nodes[index].as_ref().unwrap().driver.clone()
    }

    async fn leader(&self) -> usize {
        for _ in 0..800 {
            for (index, node) in self.nodes.iter().enumerate() {
                let Some(node) = node else { continue };
                let status = node.driver.status();
                if status.leader == Some(self.ids[index])
                    && status
                        .commit
                        .is_some_and(|commit| commit.term == status.term)
                    && status.applied == status.commit
                {
                    return index;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("no leader elected: {:?}", self.trace_tail());
    }

    async fn propose(&self, id: u128) -> LogIdentity {
        for _ in 0..20 {
            let leader = self.leader().await;
            let driver = self.driver(leader);
            if driver
                .propose(crate::control_codec::encode_control_command(&command(id)))
                .await
                .is_ok()
            {
                return driver.status().commit.unwrap();
            }
        }
        panic!("proposal {id} never committed");
    }

    async fn wait_applied(&self, index: usize, through: LogIdentity) {
        for _ in 0..800 {
            if self
                .driver(index)
                .status()
                .applied
                .is_some_and(|applied| applied.index >= through.index)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!(
            "node {index} did not apply through {through:?}: {:?}; {:?}",
            self.driver(index).status(),
            self.trace_tail()
        );
    }

    fn installs_to(&self, index: usize) -> usize {
        let target = &self.callers[index].endpoint;
        self.network
            .trace
            .lock()
            .unwrap()
            .iter()
            .filter(|(to, interface, operation)| {
                to == target
                    && interface
                        == bmux_cluster_plugin_api::cluster_consensus_snapshot::INTERFACE_ID
                            .as_str()
                    && operation == "install"
            })
            .count()
    }

    fn trace_tail(&self) -> Vec<(String, String, String)> {
        let trace = self.network.trace.lock().unwrap();
        trace[trace.len().saturating_sub(16)..].to_vec()
    }

    async fn shutdown(mut self) {
        for index in 0..self.nodes.len() {
            if self.nodes[index].is_some() {
                self.stop(index).await;
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn follower_restarts_and_catches_up_through_a_snapshot_install() {
    let mut harness = Harness::new(3);
    for index in 0..3 {
        harness.start(index);
    }
    let first = harness.propose(1).await;
    let leader = harness.leader().await;
    let follower = (leader + 1) % 3;
    harness.wait_applied(follower, first).await;
    harness.stop(follower).await;
    // Enough entries past the interval that the leader compacts what the
    // stopped follower still needs.
    let mut last = first;
    for id in 2..=12 {
        last = harness.propose(id).await;
    }
    assert_eq!(harness.installs_to(follower), 0);
    harness.start(follower);
    harness.wait_applied(follower, last).await;
    assert!(
        harness.installs_to(follower) >= 1,
        "catch-up did not use a snapshot install: {:?}",
        harness.trace_tail()
    );
    // Normal replication continues after the installed boundary.
    let after = harness.propose(100).await;
    harness.wait_applied(follower, after).await;
    harness.stop(follower).await;
    let mut store = OwnedVoteStore::open(&harness.paths[follower], &harness.cluster).unwrap();
    let recovered = store.recovered().unwrap();
    store.close().unwrap();
    let (boundary, _) = recovered.snapshot.expect("installed snapshot boundary");
    assert!(boundary.index > first.index);
    assert!(
        recovered
            .commit
            .is_some_and(|commit| commit.index >= after.index)
    );
    harness.shutdown().await;
}

// One scripted protocol exchange; each step depends on the staged state the
// previous one left, so it reads best top to bottom.
#[allow(clippy::too_many_lines)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inbound_transfer_reports_expected_offsets_resumes_and_seals() {
    let mut harness = Harness::new(3);
    for index in 0..3 {
        harness.start(index);
    }
    harness.leader().await;
    let (local, to) = (harness.ids[0], harness.ids[1]);
    let caller = harness.callers[0].clone();
    let transport = WireTransport::new(
        caller.clone(),
        local,
        caller.members.clone(),
        harness.incarnation + 100,
    );
    let route = transport
        .route(
            to,
            crate::consensus_network::supports_owned_snapshot_install,
        )
        .unwrap();
    // Larger than two chunks, so resumption crosses chunk boundaries.
    let snapshot = test_snapshot(
        &harness.cluster,
        local,
        &harness.ids,
        &control_bytes(&harness.cluster, 2 * CHUNK_BYTES + 100),
    );
    let link = Link {
        local,
        to,
        incarnation: 77,
        transfer: 1,
    };
    let term = harness.driver(1).status().term;
    let header = snapshot_header(&harness.cluster, link, term, &snapshot).unwrap();
    let bytes = snapshot.bytes.clone();
    let total = bytes.len() as u64;
    tokio::spawn(async move {
        use bmux_cluster_plugin_api::cluster_consensus_transfer::client;
        let caller = caller.as_ref();
        let remote = |endpoint| crate::endpoint::EndpointDispatchClient::new(caller, endpoint);
        let begin = async || {
            let (endpoint, request) = signed(caller, link, &route, "begin", &header, 0, Vec::new())
                .await
                .unwrap();
            client::begin(&mut remote(endpoint), whole(request))
                .await
                .unwrap()
        };
        let chunk = async |offset: u64| {
            let start = usize::try_from(offset).unwrap();
            let data = bytes[start..bytes.len().min(start + CHUNK_BYTES)].to_vec();
            let (endpoint, request) = signed(caller, link, &route, "chunk", &header, offset, data)
                .await
                .unwrap();
            client::chunk(&mut remote(endpoint), request).await.unwrap()
        };
        let chunk_size = CHUNK_BYTES as u64;
        assert_eq!(begin().await, Ok(0));
        assert_eq!(
            chunk(chunk_size).await,
            Ok(ChunkOutcome::ExpectedOffset { offset: 0 })
        );
        assert_eq!(
            chunk(0).await,
            Ok(ChunkOutcome::Accepted { offset: chunk_size })
        );
        // An exact replay of the last chunk is acknowledged without rewriting.
        assert_eq!(
            chunk(0).await,
            Ok(ChunkOutcome::Accepted { offset: chunk_size })
        );
        // A repeated begin reports where the transfer continues.
        assert_eq!(begin().await, Ok(chunk_size));
        assert_eq!(
            chunk(2 * chunk_size).await,
            Ok(ChunkOutcome::ExpectedOffset { offset: chunk_size })
        );
        assert_eq!(
            chunk(chunk_size).await,
            Ok(ChunkOutcome::Accepted {
                offset: 2 * chunk_size
            })
        );
        // Unsealed objects cannot be installed or finished early.
        let (endpoint, request) = signed(caller, link, &route, "finish", &header, 0, Vec::new())
            .await
            .unwrap();
        assert_eq!(
            client::finish(&mut remote(endpoint), whole(request))
                .await
                .unwrap(),
            Err(TransferError::InvalidObject)
        );
        assert_eq!(
            chunk(2 * chunk_size).await,
            Ok(ChunkOutcome::Accepted { offset: total })
        );
        let (endpoint, request) = signed(caller, link, &route, "finish", &header, 0, Vec::new())
            .await
            .unwrap();
        assert_eq!(
            client::finish(&mut remote(endpoint), whole(request.clone()))
                .await
                .unwrap(),
            Ok(FinishOutcome::Sealed)
        );
        // A consumed proof is never reusable.
        assert_eq!(
            client::finish(&mut remote(route.endpoint.clone()), whole(request))
                .await
                .unwrap(),
            Err(TransferError::Unauthorized)
        );
        // A different header cannot retarget the staged identity.
        let mut conflicting = header.clone();
        conflicting.checksum[0] ^= 1;
        let (endpoint, request) =
            signed(caller, link, &route, "begin", &conflicting, 0, Vec::new())
                .await
                .unwrap();
        assert!(
            client::begin(&mut remote(endpoint), whole(request))
                .await
                .unwrap()
                .is_err()
        );
        let (endpoint, request) = signed(caller, link, &route, "abort", &header, 0, Vec::new())
            .await
            .unwrap();
        assert_eq!(
            client::abort(&mut remote(endpoint), whole(request))
                .await
                .unwrap(),
            Ok(AbortOutcome::NoStagingRemains)
        );
        assert_eq!(chunk(0).await, Err(TransferError::UnknownTransfer));
    })
    .await
    .unwrap();
    harness.shutdown().await;
}
