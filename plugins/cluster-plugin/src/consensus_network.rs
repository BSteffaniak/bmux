//! Authenticated typed peer transport for `OpenRaft`.

use crate::consensus_runtime::ConsensusNode;
use crate::consensus_storage::ControlRaftConfig;
use crate::endpoint::{EndpointDispatchClient, PeerAuthenticationFailure};
use crate::membership::{NodeId, NodeIdentity, authenticate_peer_proof, verify_node_signature};
use bmux_cluster_plugin_api::cluster_raft_rpc::ClusterRaftRpcService;
use bmux_cluster_plugin_api::cluster_types::{
    ControlResponse, ControlServiceError, ControlStateView, RaftRpcRequest, RaftRpcResponse,
};
use bmux_plugin::ServiceCaller;
use bmux_plugin_sdk::{decode_service_message, encode_service_message};
use openraft::error::{
    Fatal, NetworkError, RPCError, RaftError, RemoteError, ReplicationClosed, StreamingError,
    Unreachable,
};
use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    SnapshotResponse, VoteRequest, VoteResponse,
};
use openraft::{BasicNode, Snapshot, Vote};
use std::future::Future;
use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

const RAFT_RPC_DOMAIN: &[u8] = b"bmux.cluster.raft-rpc.v1\0";

fn signing_payload(
    operation: &str,
    target_node_id: &str,
    proof: &bmux_cluster_plugin_api::cluster_types::PeerAuthProof,
    payload: &[u8],
) -> Result<Vec<u8>, String> {
    let mut signed = RAFT_RPC_DOMAIN.to_vec();
    for field in [
        operation.as_bytes(),
        target_node_id.as_bytes(),
        proof.claimant_node_id.as_bytes(),
        proof.challenge.nonce.as_bytes(),
        payload,
    ] {
        let length = u64::try_from(field.len()).map_err(|_| "raft RPC field is too large")?;
        signed.extend_from_slice(&length.to_be_bytes());
        signed.extend_from_slice(field);
    }
    Ok(signed)
}

/// Builds and signs one target-bound Raft RPC request.
///
/// # Errors
///
/// Returns an error when the proof claimant differs from the local identity
/// or a field exceeds canonical bounds.
pub fn authenticated_request(
    operation: &str,
    target_node_id: NodeId,
    identity: &NodeIdentity,
    proof: bmux_cluster_plugin_api::cluster_types::PeerAuthProof,
    payload: Vec<u8>,
) -> Result<RaftRpcRequest, String> {
    if proof.claimant_node_id != identity.node_id().to_string() {
        return Err("raft RPC proof claimant does not match local node identity".to_string());
    }
    let target = target_node_id.to_string();
    let signature = identity.sign(&signing_payload(operation, &target, &proof, &payload)?);
    Ok(RaftRpcRequest {
        target_node_id: target,
        proof,
        payload,
        signature,
    })
}

/// Authenticates, target-checks, and signature-checks an inbound Raft RPC.
///
/// # Errors
///
/// Returns an error for invalid, expired, replayed, revoked, misdirected, or
/// incorrectly signed requests.
pub fn authenticate_request(
    caller: &impl crate::ClusterRuntimeOps,
    operation: &str,
    local_node_id: NodeId,
    request: RaftRpcRequest,
) -> Result<(NodeId, Vec<u8>), String> {
    if request.target_node_id != local_node_id.to_string() {
        return Err("raft RPC targets a different node".to_string());
    }
    let peer = authenticate_peer_proof(caller, request.proof.clone())?;
    let source = peer.node_id.parse::<NodeId>()?;
    let signed = signing_payload(
        operation,
        &request.target_node_id,
        &request.proof,
        &request.payload,
    )?;
    verify_node_signature(&peer.node_id, &signed, &request.signature)?;
    Ok((source, request.payload))
}

#[derive(Clone)]
pub struct EndpointRaftNetworkFactory<C> {
    caller: Arc<C>,
    identity: NodeIdentity,
}

impl<C> EndpointRaftNetworkFactory<C> {
    #[must_use]
    pub const fn new(caller: Arc<C>, identity: NodeIdentity) -> Self {
        Self { caller, identity }
    }
}

pub struct EndpointRaftNetwork<C> {
    caller: Arc<C>,
    identity: NodeIdentity,
    target: NodeId,
    endpoint: String,
}

impl<C> RaftNetworkFactory<ControlRaftConfig> for EndpointRaftNetworkFactory<C>
where
    C: ServiceCaller + Send + Sync + 'static,
{
    type Network = EndpointRaftNetwork<C>;

    async fn new_client(&mut self, target: NodeId, node: &BasicNode) -> Self::Network {
        EndpointRaftNetwork {
            caller: self.caller.clone(),
            identity: self.identity.clone(),
            target,
            endpoint: node.addr.clone(),
        }
    }
}

impl<C> EndpointRaftNetwork<C>
where
    C: ServiceCaller + Send + Sync + 'static,
{
    async fn invoke<Request, Response>(
        &self,
        operation: &str,
        request: &Request,
    ) -> Result<Response, PeerAuthenticationFailure>
    where
        Request: serde::Serialize + Sync,
        Response: serde::de::DeserializeOwned,
    {
        let proof = crate::endpoint::peer_authentication_proof(
            self.caller.as_ref(),
            &self.endpoint,
            &self.identity.node_id().to_string(),
            &self.target.to_string(),
        )
        .await?;
        let payload = encode_service_message(request).map_err(|error| {
            PeerAuthenticationFailure::Local(format!("raft RPC encode failed: {error}"))
        })?;
        let envelope =
            authenticated_request(operation, self.target, &self.identity, proof, payload)
                .map_err(PeerAuthenticationFailure::Local)?;
        let mut remote = EndpointDispatchClient::new(self.caller.as_ref(), &self.endpoint);
        let response = match operation {
            "append_entries" => {
                bmux_cluster_plugin_api::cluster_raft_rpc::client::append_entries(
                    &mut remote,
                    envelope,
                )
                .await
            }
            "install_snapshot" => {
                bmux_cluster_plugin_api::cluster_raft_rpc::client::install_snapshot(
                    &mut remote,
                    envelope,
                )
                .await
            }
            "vote" => {
                bmux_cluster_plugin_api::cluster_raft_rpc::client::vote(&mut remote, envelope).await
            }
            _ => unreachable!("closed raft RPC operation"),
        }
        .map_err(|error| {
            PeerAuthenticationFailure::Unreachable(format!("raft RPC failed: {error}"))
        })?;
        if response.source_node_id != self.target.to_string() {
            return Err(PeerAuthenticationFailure::Untrusted(
                "raft RPC response came from an unexpected node".to_string(),
            ));
        }
        if let Some(error) = response.error {
            return Err(PeerAuthenticationFailure::Untrusted(format!(
                "remote raft RPC rejected request: {error}"
            )));
        }
        decode_service_message(&response.payload).map_err(|error| {
            PeerAuthenticationFailure::Untrusted(format!("raft RPC response is invalid: {error}"))
        })
    }

    fn rpc_error<E: std::error::Error>(
        error: PeerAuthenticationFailure,
    ) -> RPCError<NodeId, BasicNode, E> {
        match error {
            PeerAuthenticationFailure::Unreachable(reason) => Unreachable::new(
                &std::io::Error::new(std::io::ErrorKind::NotConnected, reason),
            )
            .into(),
            PeerAuthenticationFailure::Untrusted(reason)
            | PeerAuthenticationFailure::Local(reason) => NetworkError::new(&std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                reason,
            ))
            .into(),
        }
    }
}

impl<C> RaftNetwork<ControlRaftConfig> for EndpointRaftNetwork<C>
where
    C: ServiceCaller + Send + Sync + 'static,
{
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<ControlRaftConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<NodeId>, RPCError<NodeId, BasicNode, RaftError<NodeId>>> {
        self.invoke("append_entries", &rpc)
            .await
            .map_err(Self::rpc_error)
    }

    async fn install_snapshot(
        &mut self,
        rpc: InstallSnapshotRequest<ControlRaftConfig>,
        _option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<NodeId>,
        RPCError<NodeId, BasicNode, RaftError<NodeId, openraft::error::InstallSnapshotError>>,
    > {
        self.invoke("install_snapshot", &rpc)
            .await
            .map_err(Self::rpc_error)
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<NodeId>,
        _option: RPCOption,
    ) -> Result<VoteResponse<NodeId>, RPCError<NodeId, BasicNode, RaftError<NodeId>>> {
        self.invoke("vote", &rpc).await.map_err(Self::rpc_error)
    }

    async fn full_snapshot(
        &mut self,
        vote: Vote<NodeId>,
        mut snapshot: Snapshot<ControlRaftConfig>,
        cancel: impl Future<Output = ReplicationClosed> + Send + 'static,
        _option: RPCOption,
    ) -> Result<SnapshotResponse<NodeId>, StreamingError<ControlRaftConfig, Fatal<NodeId>>> {
        tokio::pin!(cancel);
        if std::future::poll_fn(|context| match cancel.as_mut().poll(context) {
            std::task::Poll::Ready(closed) => std::task::Poll::Ready(Some(closed)),
            std::task::Poll::Pending => std::task::Poll::Ready(None),
        })
        .await
        .is_some()
        {
            return Err(ReplicationClosed::new("snapshot transfer cancelled").into());
        }
        snapshot
            .snapshot
            .seek(SeekFrom::Start(0))
            .map_err(|error| StreamingError::Network(NetworkError::new(&error)))?;
        let mut data = Vec::new();
        snapshot
            .snapshot
            .read_to_end(&mut data)
            .map_err(|error| StreamingError::Network(NetworkError::new(&error)))?;
        let request = InstallSnapshotRequest {
            vote,
            meta: snapshot.meta,
            offset: 0,
            data,
            done: true,
        };
        self.install_snapshot(request, RPCOption::new(std::time::Duration::from_secs(30)))
            .await
            .map(|response| SnapshotResponse::new(response.vote))
            .map_err(|error| match error {
                RPCError::Timeout(error) => error.into(),
                RPCError::Unreachable(error) => error.into(),
                RPCError::Network(error) => error.into(),
                RPCError::RemoteError(error) => {
                    StreamingError::RemoteError(RemoteError::new_with_node(
                        error.target,
                        error
                            .target_node
                            .unwrap_or_else(|| BasicNode::new(&self.endpoint)),
                        error.source.into_fatal().unwrap_or(Fatal::Stopped),
                    ))
                }
                RPCError::PayloadTooLarge(error) => {
                    StreamingError::Network(NetworkError::new(&error))
                }
            })
    }
}

static CONSENSUS_NODES: std::sync::OnceLock<ConsensusNodeRegistry> = std::sync::OnceLock::new();

#[must_use]
pub fn global_consensus_nodes() -> ConsensusNodeRegistry {
    CONSENSUS_NODES.get_or_init(Default::default).clone()
}

#[derive(Clone, Default)]
pub struct ConsensusNodeRegistry {
    nodes: Arc<std::sync::RwLock<std::collections::BTreeMap<NodeId, ConsensusNode>>>,
}

impl ConsensusNodeRegistry {
    /// Registers one active local consensus node.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry lock is poisoned.
    pub fn insert(&self, node_id: NodeId, node: ConsensusNode) -> Result<(), String> {
        self.nodes
            .write()
            .map_err(|_| "consensus node registry lock is poisoned".to_string())?
            .insert(node_id, node);
        Ok(())
    }

    /// Removes one active local consensus node.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry lock is poisoned.
    pub fn remove(&self, node_id: NodeId) -> Result<Option<ConsensusNode>, String> {
        self.nodes
            .write()
            .map_err(|_| "consensus node registry lock is poisoned".to_string())
            .map(|mut nodes| nodes.remove(&node_id))
    }

    /// Reports whether the node is currently registered.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry lock is poisoned.
    pub fn contains(&self, node_id: NodeId) -> Result<bool, String> {
        self.nodes
            .read()
            .map_err(|_| "consensus node registry lock is poisoned".to_string())
            .map(|nodes| nodes.contains_key(&node_id))
    }

    /// Resolves the active node or maps absence to the generated service error.
    ///
    /// # Errors
    ///
    /// Returns `runtime-unavailable` when the node is absent or registry access fails.
    pub fn active(&self, node_id: NodeId) -> Result<ConsensusNode, ControlServiceError> {
        self.get(node_id)
            .map_err(|reason| ControlServiceError::RuntimeUnavailable { reason })
    }

    /// Resolves one active local consensus node.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry lock is poisoned or the node is absent.
    pub fn get(&self, node_id: NodeId) -> Result<ConsensusNode, String> {
        self.nodes
            .read()
            .map_err(|_| "consensus node registry lock is poisoned".to_string())?
            .get(&node_id)
            .cloned()
            .ok_or_else(|| format!("consensus node {node_id} is not active"))
    }

    /// Waits for one local consensus node to become active.
    ///
    /// # Errors
    ///
    /// Returns an error if startup does not register the node before the
    /// deadline or registry access fails.
    pub async fn wait_for(
        &self,
        node_id: NodeId,
        timeout: std::time::Duration,
    ) -> Result<ConsensusNode, String> {
        tokio::time::timeout(timeout, async {
            loop {
                match self.get(node_id) {
                    Ok(node) => return Ok(node),
                    Err(error) if error.contains("lock is poisoned") => return Err(error),
                    Err(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
                }
            }
        })
        .await
        .map_err(|_| format!("consensus node {node_id} did not start before the deadline"))?
    }
}

pub struct ControlServiceHandle<C> {
    caller: Arc<C>,
    local_node_id: NodeId,
    nodes: ConsensusNodeRegistry,
}

impl<C> ControlServiceHandle<C> {
    #[must_use]
    pub const fn new(caller: Arc<C>, local_node_id: NodeId, nodes: ConsensusNodeRegistry) -> Self {
        Self {
            caller,
            local_node_id,
            nodes,
        }
    }

    pub(crate) fn active(&self) -> Result<ConsensusNode, ControlServiceError> {
        self.nodes.active(self.local_node_id)
    }
}

impl<C> ControlServiceHandle<C>
where
    C: ServiceCaller + Send + Sync + 'static,
{
    async fn forward_mutation(
        &self,
        endpoint: &str,
        request: bmux_cluster_plugin_api::cluster_types::ControlCommand,
    ) -> Result<ControlResponse, ControlServiceError> {
        let mut remote = EndpointDispatchClient::new(self.caller.as_ref(), endpoint);
        bmux_cluster_plugin_api::cluster_control_command::client::mutate(&mut remote, request)
            .await
            .map_err(|error| ControlServiceError::RuntimeUnavailable {
                reason: format!("leader forwarding failed before a response was received; retry the same CommandId: {error}"),
            })?
    }

    async fn mutate_or_forward(
        &self,
        request: bmux_cluster_plugin_api::cluster_types::ControlCommand,
    ) -> Result<ControlResponse, ControlServiceError> {
        match self.active()?.mutate(request.clone()).await {
            Err(ControlServiceError::NotLeader {
                leader_endpoint: Some(endpoint),
                ..
            }) => self.forward_mutation(&endpoint, request).await,
            result => result,
        }
    }

    async fn forward_linearizable_read(
        &self,
        endpoint: &str,
    ) -> Result<ControlStateView, ControlServiceError> {
        let mut remote = EndpointDispatchClient::new(self.caller.as_ref(), endpoint);
        bmux_cluster_plugin_api::cluster_control_state::client::read_linearizable(&mut remote)
            .await
            .map_err(|error| ControlServiceError::RuntimeUnavailable {
                reason: format!("leader read forwarding failed: {error}"),
            })?
    }

    async fn read_linearizable_or_forward(&self) -> Result<ControlStateView, ControlServiceError> {
        match self.active()?.read_linearizable_view().await {
            Err(ControlServiceError::NotLeader {
                leader_endpoint: Some(endpoint),
                ..
            }) => self.forward_linearizable_read(&endpoint).await,
            result => result,
        }
    }
}

impl<C> bmux_cluster_plugin_api::cluster_control_command::ClusterControlCommandService
    for ControlServiceHandle<C>
where
    C: ServiceCaller + Send + Sync + 'static,
{
    fn mutate<'a>(
        &'a self,
        request: bmux_cluster_plugin_api::cluster_types::ControlCommand,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<ControlResponse, ControlServiceError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move { self.mutate_or_forward(request).await })
    }
}

impl<C> bmux_cluster_plugin_api::cluster_control_state::ClusterControlStateService
    for ControlServiceHandle<C>
where
    C: ServiceCaller + Send + Sync + 'static,
{
    fn read_linearizable<'a>(
        &'a self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<ControlStateView, ControlServiceError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move { self.read_linearizable_or_forward().await })
    }

    fn read_stale<'a>(
        &'a self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<ControlStateView, ControlServiceError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move { self.active()?.read_stale_view() })
    }
}

pub struct RaftRpcServiceHandle<C> {
    caller: Arc<C>,
    local_node_id: NodeId,
    nodes: ConsensusNodeRegistry,
}

impl<C> RaftRpcServiceHandle<C> {
    #[must_use]
    pub const fn new(caller: Arc<C>, local_node_id: NodeId, nodes: ConsensusNodeRegistry) -> Self {
        Self {
            caller,
            local_node_id,
            nodes,
        }
    }
}

impl<C> RaftRpcServiceHandle<C>
where
    C: crate::ClusterRuntimeOps + Send + Sync + 'static,
{
    async fn dispatch<Request, Response>(
        &self,
        operation: &'static str,
        request: RaftRpcRequest,
        invoke: impl FnOnce(
            openraft::Raft<ControlRaftConfig>,
            Request,
        )
            -> std::pin::Pin<Box<dyn Future<Output = Result<Response, String>> + Send>>,
    ) -> RaftRpcResponse
    where
        Request: serde::de::DeserializeOwned,
        Response: serde::Serialize,
    {
        let result = async {
            let (_, payload) =
                authenticate_request(self.caller.as_ref(), operation, self.local_node_id, request)?;
            let request = decode_service_message(&payload)
                .map_err(|error| format!("invalid {operation} payload: {error}"))?;
            let raft = self.nodes.get(self.local_node_id)?.raft().clone();
            let response = invoke(raft, request).await?;
            encode_service_message(&response)
                .map_err(|error| format!("failed encoding {operation} response: {error}"))
        }
        .await;
        match result {
            Ok(payload) => response(self.local_node_id, payload),
            Err(error) => RaftRpcResponse {
                source_node_id: self.local_node_id.to_string(),
                payload: Vec::new(),
                error: Some(error),
            },
        }
    }
}

impl<C> ClusterRaftRpcService for RaftRpcServiceHandle<C>
where
    C: crate::ClusterRuntimeOps + Send + Sync + 'static,
{
    fn append_entries<'a>(
        &'a self,
        request: RaftRpcRequest,
    ) -> std::pin::Pin<Box<dyn Future<Output = RaftRpcResponse> + Send + 'a>> {
        Box::pin(self.dispatch("append_entries", request, |raft, request| {
            Box::pin(async move {
                raft.append_entries(request)
                    .await
                    .map_err(|error| error.to_string())
            })
        }))
    }

    fn install_snapshot<'a>(
        &'a self,
        request: RaftRpcRequest,
    ) -> std::pin::Pin<Box<dyn Future<Output = RaftRpcResponse> + Send + 'a>> {
        Box::pin(self.dispatch("install_snapshot", request, |raft, request| {
            Box::pin(async move {
                raft.install_snapshot(request)
                    .await
                    .map_err(|error| error.to_string())
            })
        }))
    }

    fn vote<'a>(
        &'a self,
        request: RaftRpcRequest,
    ) -> std::pin::Pin<Box<dyn Future<Output = RaftRpcResponse> + Send + 'a>> {
        Box::pin(self.dispatch("vote", request, |raft, request| {
            Box::pin(async move { raft.vote(request).await.map_err(|error| error.to_string()) })
        }))
    }
}

#[must_use]
pub fn response(source_node_id: NodeId, payload: Vec<u8>) -> RaftRpcResponse {
    RaftRpcResponse {
        source_node_id: source_node_id.to_string(),
        payload,
        error: None,
    }
}

#[cfg(test)]
#[path = "consensus_network_tests.rs"]
mod forwarding_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_cluster_plugin_api::cluster_types::{PeerAuthChallenge, PeerAuthProof};

    fn proof(source: NodeId, target: NodeId) -> PeerAuthProof {
        PeerAuthProof {
            challenge: PeerAuthChallenge {
                protocol_version: 1,
                cluster_id: "cluster:test".to_string(),
                verifier_node_id: target.to_string(),
                verifier_credential_serial: "credential:target".to_string(),
                audience_node_id: source.to_string(),
                nonce: "nonce".to_string(),
                issued_at_unix_ms: 1,
                expires_at_unix_ms: 2,
                signature: "signature".to_string(),
            },
            claimant_node_id: source.to_string(),
            claimant_credential_serial: "credential:source".to_string(),
            claimant_signature: "signature".to_string(),
        }
    }

    #[test]
    fn raft_rpc_signature_binds_operation_target_proof_and_payload() {
        let identity = NodeIdentity::new_for_test(7);
        let target = NodeId::from(8);
        let request = authenticated_request(
            "vote",
            target,
            &identity,
            proof(*identity.node_id(), target),
            vec![1, 2, 3],
        )
        .unwrap();
        let signed = signing_payload(
            "vote",
            &request.target_node_id,
            &request.proof,
            &request.payload,
        )
        .unwrap();
        identity.verify(&signed, &request.signature).unwrap();
        assert!(
            identity
                .verify(
                    &signing_payload(
                        "append_entries",
                        &request.target_node_id,
                        &request.proof,
                        &request.payload,
                    )
                    .unwrap(),
                    &request.signature,
                )
                .is_err()
        );
    }
}
