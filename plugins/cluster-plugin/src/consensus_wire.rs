//! Maps engine messages onto the authenticated generated consensus contracts.
//!
//! Outbound, [`WireTransport`] signs `cluster-consensus-rpc` vote (v1) and append
//! (v2) requests with a fresh single-use peer proof and verifies that each reply is
//! signed by the intended responder and bound to the request. Inbound,
//! [`WireService`] authenticates the sender against current member credentials,
//! hands the decoded message to the driver, and signs the engine's reply. Request
//! identities carry the sender's incarnation and the engine's sequence number.
//! Snapshots travel through staged transfer and a separate install call; see
//! [`snapshot`].

use crate::consensus_driver::{Driver, Reply, SnapshotObject, Transport};
use crate::consensus_engine::{AppendReplyOutcome, Message, NodeId};
use crate::consensus_model::{Entry, LogIdentity, LogIndex, Term};
use bmux_cluster_plugin_api::cluster_consensus_rpc_types::{
    AppendEntry, AppendOutcome, AppendRequest, RpcError,
};
use bmux_cluster_plugin_api::cluster_consensus_rpc_v2_types::{
    AppendOutcome as AppendOutcomeV2, AppendResponse as AppendResponseV2,
};
use bmux_cluster_plugin_api::cluster_consensus_transfer_types::{
    LogIdentity as WireIdentity, TransferProof,
};
use bmux_cluster_plugin_api::cluster_consensus_vote_types::{VoteRequest, VoteResponse};
use bmux_cluster_plugin_api::cluster_types::ClusterMember;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

mod snapshot;

pub use snapshot::SnapshotStaging;

const RPC_TIMEOUT: Duration = Duration::from_secs(5);

/// Current cluster identity and member credentials, refreshed from applied state.
#[derive(Debug, Clone, Default)]
pub struct MemberView(Arc<RwLock<(String, Vec<ClusterMember>)>>);

impl MemberView {
    pub fn update(&self, cluster: String, members: Vec<ClusterMember>) {
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = (cluster, members);
    }

    fn snapshot(&self) -> (String, Vec<ClusterMember>) {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

fn wire(identity: LogIdentity) -> WireIdentity {
    WireIdentity {
        term: identity.term.0,
        leader: identity.leader.as_bytes().to_vec(),
        index: identity.index.0,
    }
}

fn from_wire(identity: &WireIdentity) -> Option<LogIdentity> {
    Some(LogIdentity {
        term: Term(identity.term),
        leader: NodeId::from_bytes(identity.leader.as_slice().try_into().ok()?),
        index: LogIndex(identity.index),
    })
}

const fn request_id(incarnation: u64, seq: u64) -> uuid::Uuid {
    uuid::Uuid::from_u64_pair(incarnation, seq)
}

const fn sequence(id: uuid::Uuid) -> u64 {
    id.as_u64_pair().1
}

/// Signs and sends engine requests to peers over the generated contracts.
pub struct WireTransport<C> {
    caller: Arc<C>,
    local: NodeId,
    members: MemberView,
    incarnation: u64,
    /// The last snapshot transfer sequence used by this incarnation.
    transfers: AtomicU64,
}

impl<C> WireTransport<C> {
    #[must_use]
    pub const fn new(caller: Arc<C>, local: NodeId, members: MemberView, incarnation: u64) -> Self {
        Self {
            caller,
            local,
            members,
            incarnation,
            transfers: AtomicU64::new(0),
        }
    }
}

#[derive(Clone)]
struct Route {
    cluster: String,
    endpoint: String,
    credential_serial: String,
}

impl<C> WireTransport<C> {
    /// Route to a peer that negotiated the contract `supports` checks.
    fn route(&self, to: NodeId, supports: fn(&ClusterMember) -> bool) -> Option<Route> {
        let (cluster, members) = self.members.snapshot();
        let peer = members
            .iter()
            .find(|member| member.node_id == to.to_string())?;
        if !supports(peer) {
            return None;
        }
        let local = members
            .iter()
            .find(|member| member.node_id == self.local.to_string())?;
        Some(Route {
            cluster,
            endpoint: peer
                .endpoint
                .clone()
                .filter(|endpoint| !endpoint.is_empty())?,
            credential_serial: local.credential_serial.clone(),
        })
    }
}

impl<C> Transport for WireTransport<C>
where
    C: crate::ClusterRuntimeOps + bmux_plugin::ServiceCaller + Send + Sync + 'static,
{
    fn call(&self, to: NodeId, message: Message) -> Reply {
        let caller = self.caller.clone();
        let local = self.local;
        let incarnation = self.incarnation;
        let route = self.route(to, crate::consensus_network::supports_owned_append_v2);
        Box::pin(async move {
            let prepared = prepare(caller.as_ref(), local, to, route?).await?;
            match message {
                Message::Vote { term, last_log } => {
                    vote_call(
                        caller.as_ref(),
                        prepared,
                        local,
                        to,
                        incarnation,
                        term,
                        last_log,
                    )
                    .await
                }
                Message::Append {
                    term,
                    seq,
                    previous,
                    entries,
                    leader_commit,
                } => {
                    let request = AppendRequest {
                        cluster_id: prepared.cluster.clone(),
                        sender: local.as_bytes().to_vec(),
                        recipient: to.as_bytes().to_vec(),
                        request_id: request_id(incarnation, seq),
                        term: term.0,
                        previous: previous.map(wire),
                        entries: entries
                            .iter()
                            .map(|entry| {
                                crate::consensus_storage::encode_owned_entry(entry)
                                    .map(|canonical| AppendEntry::Inline { canonical })
                            })
                            .collect::<Result<Vec<_>, _>>()
                            .ok()?,
                        leader_commit: leader_commit.map(wire),
                        proof: prepared.proof.clone(),
                    };
                    append_call(caller.as_ref(), prepared, to, seq, request).await
                }
                // Replies are returned to callers, never sent as requests.
                _ => None,
            }
        })
    }

    fn send_snapshot(&self, to: NodeId, term: Term, seq: u64, snapshot: SnapshotObject) -> Reply {
        let caller = self.caller.clone();
        let link = snapshot::Link {
            local: self.local,
            to,
            incarnation: self.incarnation,
            transfer: self.transfers.fetch_add(1, Ordering::SeqCst) + 1,
        };
        let route = self.route(
            to,
            crate::consensus_network::supports_owned_snapshot_install,
        );
        Box::pin(
            async move { snapshot::send(caller.as_ref(), link, route?, term, seq, snapshot).await },
        )
    }
}

/// A signing identity, fresh peer proof and route for one outbound request.
struct Prepared {
    identity: crate::membership::NodeIdentity,
    cluster: String,
    endpoint: String,
    proof: TransferProof,
}

async fn prepare<C>(caller: &C, local: NodeId, to: NodeId, route: Route) -> Option<Prepared>
where
    C: crate::ClusterRuntimeOps + bmux_plugin::ServiceCaller + Sync,
{
    let identity = crate::load_or_create_node_identity(caller).ok()?;
    let peer = switchy_async::time::timeout(
        RPC_TIMEOUT,
        crate::endpoint::peer_authentication_proof(
            caller,
            &route.endpoint,
            &local.to_string(),
            &to.to_string(),
        ),
    )
    .await
    .ok()?
    .ok()?;
    Some(Prepared {
        identity,
        cluster: route.cluster,
        endpoint: route.endpoint,
        proof: TransferProof {
            peer,
            credential_serial: route.credential_serial,
            signature: Vec::new(),
        },
    })
}

async fn vote_call<C>(
    caller: &C,
    prepared: Prepared,
    local: NodeId,
    to: NodeId,
    incarnation: u64,
    term: Term,
    last_log: Option<LogIdentity>,
) -> Option<Message>
where
    C: bmux_plugin::ServiceCaller + Sync,
{
    let mut request = VoteRequest {
        cluster_id: prepared.cluster,
        sender: local.as_bytes().to_vec(),
        recipient: to.as_bytes().to_vec(),
        // One vote request per term, so the term is its sequence.
        request_id: request_id(incarnation, term.0),
        term: term.0,
        last_log: last_log.map(wire),
        proof: prepared.proof,
    };
    request.proof.signature = prepared
        .identity
        .sign(&crate::consensus_network::owned_vote_signing_payload(&request).ok()?);
    let mut remote = crate::endpoint::EndpointDispatchClient::new(caller, prepared.endpoint);
    let response = switchy_async::time::timeout(
        RPC_TIMEOUT,
        bmux_cluster_plugin_api::cluster_consensus_rpc::client::vote(&mut remote, request.clone()),
    )
    .await
    .ok()?
    .ok()?
    .ok()?;
    verify_vote_response(&request, &response, to)?;
    Some(Message::VoteReply {
        term: Term(response.term),
        granted: response.granted,
    })
}

async fn append_call<C>(
    caller: &C,
    prepared: Prepared,
    to: NodeId,
    seq: u64,
    mut request: AppendRequest,
) -> Option<Message>
where
    C: bmux_plugin::ServiceCaller + Sync,
{
    request.proof.signature = prepared
        .identity
        .sign(&crate::consensus_network::owned_append_v2_signing_payload(&request).ok()?);
    let mut remote = crate::endpoint::EndpointDispatchClient::new(caller, prepared.endpoint);
    let response = switchy_async::time::timeout(
        RPC_TIMEOUT,
        bmux_cluster_plugin_api::cluster_consensus_rpc_v2::client::append(
            &mut remote,
            request.clone(),
        ),
    )
    .await
    .ok()?
    .ok()?
    .ok()?;
    crate::consensus_network::verify_owned_append_v2_response(&request, &response).ok()?;
    if response.source.as_slice() != to.as_bytes() {
        return None;
    }
    let outcome = match response.outcome {
        // The signed reply carries the higher term the engine steps down to.
        AppendOutcomeV2::StaleTerm => AppendReplyOutcome::Conflict { next: LogIndex(0) },
        AppendOutcomeV2::Accepted { value } => match value {
            AppendOutcome::Durable { matched } => AppendReplyOutcome::Matched(match matched {
                Some(identity) => Some(from_wire(&identity)?),
                None => None,
            }),
            AppendOutcome::Conflict { next_index } => AppendReplyOutcome::Conflict {
                next: LogIndex(next_index),
            },
            AppendOutcome::MissingTransfer { .. } => return None,
        },
    };
    Some(Message::AppendReply {
        term: Term(response.term),
        seq,
        outcome,
    })
}

fn verify_vote_response(
    request: &VoteRequest,
    response: &VoteResponse,
    peer: NodeId,
) -> Option<()> {
    if response.request_id != request.request_id || response.source.as_slice() != peer.as_bytes() {
        return None;
    }
    let payload =
        crate::consensus_network::owned_vote_response_signing_payload(request, response).ok()?;
    crate::membership::verify_node_signature(&peer.to_string(), &payload, &response.signature).ok()
}

/// Authenticated inbound consensus RPCs served by a running driver.
pub struct WireService<C, V> {
    caller: Arc<C>,
    local: NodeId,
    members: MemberView,
    driver: Arc<Driver<V>>,
    staging: Arc<SnapshotStaging>,
}

impl<C, V> WireService<C, V>
where
    C: crate::ClusterRuntimeOps + Send + Sync + 'static,
    V: Send + Unpin + 'static,
{
    /// `staging` must be the handle whose [`SnapshotStaging::reader`] backs the
    /// driver's storage, so installs consume what this service sealed.
    #[must_use]
    pub const fn new(
        caller: Arc<C>,
        local: NodeId,
        members: MemberView,
        driver: Arc<Driver<V>>,
        staging: Arc<SnapshotStaging>,
    ) -> Self {
        Self {
            caller,
            local,
            members,
            driver,
            staging,
        }
    }

    /// Install every consensus provider: vote (v1), append (v2), transfer and
    /// snapshot install. The registrations end with the registry.
    /// # Errors
    /// Reports registry rejection.
    pub fn register(
        self: &Arc<Self>,
        registry: &mut bmux_plugin_sdk::TypedServiceRegistry,
    ) -> Result<(), Box<bmux_plugin_sdk::PluginError>> {
        bmux_cluster_plugin_api::cluster_consensus_rpc::register_provider(registry, self.clone())
            .map_err(Box::new)?;
        bmux_cluster_plugin_api::cluster_consensus_rpc_v2::register_provider(
            registry,
            self.clone(),
        )
        .map_err(Box::new)?;
        bmux_cluster_plugin_api::cluster_consensus_transfer::register_provider(
            registry,
            self.clone(),
        )
        .map_err(Box::new)?;
        bmux_cluster_plugin_api::cluster_consensus_snapshot::register_provider(
            registry,
            self.clone(),
        )
        .map_err(Box::new)
    }

    async fn vote(&self, request: VoteRequest) -> Result<VoteResponse, RpcError> {
        let identity = crate::load_or_create_node_identity(self.caller.as_ref())
            .map_err(|_| RpcError::Unauthorized)?;
        let (_, members) = self.members.snapshot();
        let from = crate::consensus_network::authenticate_owned_vote(
            self.caller.as_ref(),
            self.local,
            &members,
            &request,
        )
        .map_err(|_| RpcError::Unauthorized)?;
        let last_log = match &request.last_log {
            Some(identity) => Some(from_wire(identity).ok_or(RpcError::InvalidRequest)?),
            None => None,
        };
        let reply = self
            .driver
            .request(
                from,
                Message::Vote {
                    term: Term(request.term),
                    last_log,
                },
            )
            .await
            .ok_or(RpcError::Stopped)?;
        let Message::VoteReply { term, granted } = reply else {
            return Err(RpcError::ExecutionFailed);
        };
        let mut response = VoteResponse {
            request_id: request.request_id,
            source: self.local.as_bytes().to_vec(),
            term: term.0,
            granted,
            signature: Vec::new(),
        };
        response.signature = identity.sign(
            &crate::consensus_network::owned_vote_response_signing_payload(&request, &response)
                .map_err(|_| RpcError::InvalidRequest)?,
        );
        Ok(response)
    }

    async fn append(&self, request: AppendRequest) -> Result<AppendResponseV2, RpcError> {
        let identity = crate::load_or_create_node_identity(self.caller.as_ref())
            .map_err(|_| RpcError::Unauthorized)?;
        let (_, members) = self.members.snapshot();
        let from = crate::consensus_network::authenticate_owned_append_v2(
            self.caller.as_ref(),
            self.local,
            &members,
            &request,
        )
        .map_err(|_| RpcError::Unauthorized)?;
        let decode = |identity: &Option<WireIdentity>| {
            identity.as_ref().map_or(Ok(None), |identity| {
                from_wire(identity)
                    .map(Some)
                    .ok_or(RpcError::InvalidRequest)
            })
        };
        let entries = request
            .entries
            .iter()
            .map(|entry| match entry {
                AppendEntry::Inline { canonical } => {
                    crate::consensus_storage::decode_owned_entry(canonical)
                        .map_err(|_| RpcError::InvalidRequest)
                }
                // Staged large entries are resolved by the transfer path.
                AppendEntry::Staged { .. } => Err(RpcError::Incompatible),
            })
            .collect::<Result<Vec<Entry>, _>>()?;
        let seq = sequence(request.request_id);
        let reply = self
            .driver
            .request(
                from,
                Message::Append {
                    term: Term(request.term),
                    seq,
                    previous: decode(&request.previous)?,
                    entries,
                    leader_commit: decode(&request.leader_commit)?,
                },
            )
            .await
            .ok_or(RpcError::Stopped)?;
        let Message::AppendReply { term, outcome, .. } = reply else {
            return Err(RpcError::ExecutionFailed);
        };
        let outcome = if term.0 > request.term {
            AppendOutcomeV2::StaleTerm
        } else {
            AppendOutcomeV2::Accepted {
                value: match outcome {
                    AppendReplyOutcome::Matched(matched) => AppendOutcome::Durable {
                        matched: matched.map(wire),
                    },
                    AppendReplyOutcome::Conflict { next } => {
                        AppendOutcome::Conflict { next_index: next.0 }
                    }
                },
            }
        };
        let mut response = AppendResponseV2 {
            request_id: request.request_id,
            source: self.local.as_bytes().to_vec(),
            term: term.0,
            outcome,
            signature: Vec::new(),
        };
        response.signature = identity.sign(
            &crate::consensus_network::owned_append_v2_response_signing_payload(
                &request, &response,
            )
            .map_err(|_| RpcError::InvalidRequest)?,
        );
        Ok(response)
    }
}

type Rpc<'a, T> = Pin<Box<dyn Future<Output = Result<T, RpcError>> + Send + 'a>>;

impl<C, V> bmux_cluster_plugin_api::cluster_consensus_rpc::ClusterConsensusRpcService
    for WireService<C, V>
where
    C: crate::ClusterRuntimeOps + Send + Sync + 'static,
    V: Send + Unpin + 'static,
{
    fn vote(&self, request: VoteRequest) -> Rpc<'_, VoteResponse> {
        Box::pin(self.vote(request))
    }

    fn append(
        &self,
        _: AppendRequest,
    ) -> Rpc<'_, bmux_cluster_plugin_api::cluster_consensus_rpc_types::AppendResponse> {
        // Owned-engine appends require the negotiated V2 contract; no V1 fallback.
        Box::pin(async { Err(RpcError::Incompatible) })
    }
}

impl<C, V> bmux_cluster_plugin_api::cluster_consensus_rpc_v2::ClusterConsensusRpcService
    for WireService<C, V>
where
    C: crate::ClusterRuntimeOps + Send + Sync + 'static,
    V: Send + Unpin + 'static,
{
    fn append(&self, request: AppendRequest) -> Rpc<'_, AppendResponseV2> {
        Box::pin(self.append(request))
    }
}
