//! Snapshot catch-up over the generated transfer and install contracts.
//!
//! Outbound, the leader stages published snapshot bytes on the follower with
//! `begin`, sequential 64 KiB `chunk`s and `finish` (sealed, not installed),
//! then asks it to install the sealed object. Inbound, [`SnapshotStaging`] owns
//! the disposable staged objects; the install provider validates the sealed
//! object, hands its bytes to the driver's storage through the same handle, and
//! signs the engine's reply. Every operation carries a fresh single-use proof.

use super::{
    Prepared, RPC_TIMEOUT, Route, WireService, from_wire, prepare, request_id, sequence, wire,
};
use crate::consensus_driver::SnapshotObject;
use crate::consensus_engine::{Message, NodeId};
use crate::consensus_model::{LogIdentity, Membership, StoredMembership, Term};
use crate::transfer_staging::{TransferKey, TransferStaging};
use bmux_cluster_plugin_api::cluster_consensus_snapshot_types::{
    InstallError, InstallOutcome, InstallRejection, InstallRequest, InstallResponse,
};
use bmux_cluster_plugin_api::cluster_consensus_transfer_types::{
    AbortOutcome, ChunkOutcome, ChunkRequest, FinishOutcome, NodeAddress, SnapshotDescriptor,
    StoredMembership as WireMembership, TransferError, TransferHeader, TransferPurpose,
    TransferRequest, VoterConfiguration,
};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::future::Future;
use std::io::{self, Read, Seek};
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Raw bytes per chunk, the contract's bound.
pub(super) const CHUNK_BYTES: usize = 64 * 1024;
/// Expected-offset resumes tolerated in one upload before giving up.
const MAX_RESUMES: u32 = 8;
/// Successful installs remembered for exact replay.
const MAX_RECEIPTS: usize = 64;

/// The identity of one successful install, for exact replay of a lost reply.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Receipt {
    request_id: uuid::Uuid,
    source: NodeId,
    key: TransferKey,
    binding: [u8; 32],
    boundary: LogIdentity,
    term: u64,
}

/// Validated install bytes by sender and boundary, held only during delivery.
type Accepted = Vec<((NodeId, LogIdentity), Vec<u8>)>;

struct Staged {
    directory: std::fs::File,
    transfers: TransferStaging,
}

/// Disposable inbound snapshot staging shared by the transfer and install
/// providers and the driver's storage.
///
/// Staged objects grant no authority and do not survive a restart. Validated
/// install bytes are handed to storage only while the install provider waits
/// for the engine, keyed by sender and boundary.
pub struct SnapshotStaging {
    staged: Mutex<Staged>,
    validation: tokio::sync::Semaphore,
    installing: tokio::sync::Semaphore,
    closing: AtomicBool,
    accepted: Mutex<Accepted>,
    receipts: Mutex<VecDeque<Receipt>>,
}

impl SnapshotStaging {
    /// Open owner-private staging, discarding transfers left by a previous run.
    /// Quotas bound reserved bytes per peer and in total.
    /// # Errors
    /// Rejects invalid quotas and untrusted, foreign or corrupt directories.
    pub fn open(directory: &Path, per_peer_bytes: u64, total_bytes: u64) -> io::Result<Arc<Self>> {
        let transfers = TransferStaging::new(per_peer_bytes, total_bytes)?;
        let directory = crate::transfer_staging::recover_directory(directory)?;
        Ok(Arc::new(Self {
            staged: Mutex::new(Staged {
                directory,
                transfers,
            }),
            validation: tokio::sync::Semaphore::new(1),
            installing: tokio::sync::Semaphore::new(1),
            closing: AtomicBool::new(false),
            accepted: Mutex::new(Vec::new()),
            receipts: Mutex::new(VecDeque::new()),
        }))
    }

    /// The storage-side reader: the validated bytes of the sealed snapshot the
    /// install provider is delivering from `from` through `boundary`.
    pub fn reader(self: &Arc<Self>) -> crate::owned_vote_store::StagedSnapshot {
        let staging = self.clone();
        Box::new(move |from, boundary| {
            let mut accepted = staging
                .accepted
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let index = accepted
                .iter()
                .position(|(key, _)| *key == (from, boundary))
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "no sealed snapshot for boundary")
                })?;
            Ok(accepted.swap_remove(index).1)
        })
    }

    /// Delete transfers without progress for the contract's idle lifetime.
    /// # Errors
    /// Reports cleanup failure; failed deletions keep their reservation.
    pub fn expire(&self) -> io::Result<()> {
        self.lock().transfers.expire(Instant::now())
    }

    /// Expire idle transfers once a second until the handle is dropped.
    pub fn spawn_expiry(self: &Arc<Self>) {
        let owner = Arc::downgrade(self);
        switchy_async::task::spawn(async move {
            loop {
                switchy_async::time::sleep(Duration::from_secs(1)).await;
                let Some(staging) = owner.upgrade() else {
                    break;
                };
                let expired = switchy_async::task::spawn_blocking(move || staging.expire()).await;
                if let Ok(Err(error)) = expired {
                    tracing::error!(%error, "owned snapshot staging expiry failed");
                }
            }
        });
    }

    /// Stop admitting transfers and delete every staged object.
    /// # Errors
    /// Reports incomplete cleanup.
    pub fn close(&self) -> io::Result<()> {
        self.closing.store(true, Ordering::SeqCst);
        self.lock().transfers.clear()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Staged> {
        self.staged.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Run one staging operation off the async thread, expiring idle transfers first.
    async fn run<T: Send + 'static>(
        self: &Arc<Self>,
        operation: impl FnOnce(&mut Staged, Instant) -> Result<T, TransferError> + Send + 'static,
    ) -> Result<T, TransferError> {
        let staging = self.clone();
        switchy_async::task::spawn_blocking(move || {
            let mut staged = staging.lock();
            let now = Instant::now();
            let result = staged
                .transfers
                .expire(now)
                .map_err(|_| TransferError::CleanupIncomplete)
                .and_then(|()| operation(&mut staged, now));
            drop(staged);
            result
        })
        .await
        .map_err(|_| TransferError::StorageFailure)?
    }

    /// `Some(true)` replays an identical install, `Some(false)` conflicts with one.
    fn replay(&self, candidate: &Receipt) -> Option<bool> {
        self.receipts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|old| {
                (old.request_id, old.source) == (candidate.request_id, candidate.source)
                    || (old.source, old.key) == (candidate.source, candidate.key)
            })
            .map(|old| old == candidate)
    }

    fn record(&self, receipt: Receipt) {
        let mut receipts = self.receipts.lock().unwrap_or_else(PoisonError::into_inner);
        if receipts.len() == MAX_RECEIPTS {
            receipts.pop_front();
        }
        receipts.push_back(receipt);
    }

    fn accept(&self, key: (NodeId, LogIdentity), bytes: Vec<u8>) {
        let mut accepted = self.accepted.lock().unwrap_or_else(PoisonError::into_inner);
        accepted.retain(|(existing, _)| *existing != key);
        accepted.push((key, bytes));
    }

    fn withdraw(&self, key: (NodeId, LogIdentity)) {
        self.accepted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(existing, _)| *existing != key);
    }
}

fn transfer_key(
    source: NodeId,
    header: &TransferHeader,
) -> Result<(TransferKey, [u8; 32]), TransferError> {
    let binding = Sha256::digest(
        bmux_plugin_sdk::encode_service_message(header)
            .map_err(|_| TransferError::InvalidObject)?,
    )
    .into();
    Ok((
        TransferKey {
            peer: *source.as_bytes(),
            incarnation: header.sender_incarnation,
            sequence: header.sequence,
        },
        binding,
    ))
}

fn staging_error(error: &io::Error) -> TransferError {
    match error.kind() {
        io::ErrorKind::WouldBlock => TransferError::CapacityUnavailable,
        io::ErrorKind::NotFound => TransferError::UnknownTransfer,
        io::ErrorKind::InvalidInput => TransferError::ConflictingIdentity,
        _ => TransferError::InvalidObject,
    }
}

fn chunk_form(request: TransferRequest) -> ChunkRequest {
    ChunkRequest {
        header: request.header,
        offset: 0,
        data: Vec::new(),
        proof: request.proof,
    }
}

pub(super) fn membership_to_wire(membership: &StoredMembership) -> Option<WireMembership> {
    Some(WireMembership {
        introduced_at: wire(membership.introduced_at?),
        configurations: membership
            .membership
            .configurations()
            .iter()
            .map(|voters| VoterConfiguration {
                voters: voters.iter().map(|id| id.as_bytes().to_vec()).collect(),
            })
            .collect(),
        nodes: membership
            .membership
            .nodes()
            .iter()
            .map(|(node, endpoint)| NodeAddress {
                node: node.as_bytes().to_vec(),
                endpoint: endpoint.clone(),
            })
            .collect(),
    })
}

pub(super) fn membership_from_wire(membership: &WireMembership) -> Option<StoredMembership> {
    let node = |bytes: &Vec<u8>| Some(NodeId::from_bytes(bytes.as_slice().try_into().ok()?));
    let configurations = membership
        .configurations
        .iter()
        .map(|configuration| configuration.voters.iter().map(node).collect())
        .collect::<Option<Vec<Vec<_>>>>()?;
    let nodes = membership
        .nodes
        .iter()
        .map(|address| Some((node(&address.node)?, address.endpoint.clone())))
        .collect::<Option<Vec<_>>>()?;
    Some(StoredMembership {
        introduced_at: Some(from_wire(&membership.introduced_at)?),
        membership: Membership::new(configurations, nodes).ok()?,
    })
}

/// Read and check the exact sealed object named by an install reference.
fn sealed_bytes(
    staged: &mut Staged,
    key: TransferKey,
    binding: [u8; 32],
    reference: &TransferHeader,
    closing: &AtomicBool,
) -> Result<Vec<u8>, InstallRejection> {
    let (file, total) =
        staged
            .transfers
            .sealed_reader(key, binding)
            .map_err(|error| match error.kind() {
                io::ErrorKind::NotFound => InstallRejection::UnknownTransfer,
                io::ErrorKind::WouldBlock => InstallRejection::NotSealed,
                _ => InstallRejection::ConflictingReference,
            })?;
    if total != reference.total_bytes {
        return Err(InstallRejection::ConflictingReference);
    }
    crate::consensus_network::validate_staged_object(
        file,
        total,
        reference,
        &AtomicU8::new(0),
        closing,
    )
    .map_err(|_| InstallRejection::InvalidSnapshot)?;
    let mut bytes =
        vec![0; usize::try_from(total).map_err(|_| InstallRejection::CapacityUnavailable)?];
    file.rewind()
        .and_then(|()| file.read_exact(&mut bytes))
        .map_err(|_| InstallRejection::InvalidSnapshot)?;
    if Sha256::digest(&bytes).as_slice() != reference.checksum.as_slice() {
        return Err(InstallRejection::InvalidSnapshot);
    }
    Ok(bytes)
}

// ----- outbound --------------------------------------------------------------

/// The fixed identity of one outbound snapshot transfer.
#[derive(Debug, Clone, Copy)]
pub(super) struct Link {
    pub(super) local: NodeId,
    pub(super) to: NodeId,
    pub(super) incarnation: u64,
    pub(super) transfer: u64,
}

/// The immutable transfer header for `snapshot`, bound to its descriptor.
pub(super) fn snapshot_header(
    cluster: &str,
    link: Link,
    term: Term,
    snapshot: &SnapshotObject,
) -> Option<TransferHeader> {
    let (descriptor, _) = crate::consensus_storage::decode_owned_snapshot(&snapshot.bytes).ok()?;
    if descriptor.last_included != Some(snapshot.last_included)
        || descriptor.membership.as_ref() != Some(&snapshot.membership)
        || descriptor.cluster_id != cluster
    {
        return None;
    }
    Some(TransferHeader {
        cluster_id: cluster.to_owned(),
        sender: link.local.as_bytes().to_vec(),
        recipient: link.to.as_bytes().to_vec(),
        leader_term: term.0,
        sender_incarnation: uuid::Uuid::from_u64_pair(link.incarnation, 0),
        sequence: link.transfer,
        purpose: TransferPurpose::Snapshot {
            value: SnapshotDescriptor {
                format_version: descriptor.format_version,
                cluster_id: descriptor.cluster_id,
                snapshot_id: descriptor.snapshot_id,
                last_included: Some(wire(snapshot.last_included)),
                membership: Some(membership_to_wire(&snapshot.membership)?),
                total_bytes: descriptor.total_bytes,
                checksum: descriptor.checksum.to_vec(),
            },
        },
        total_bytes: u64::try_from(snapshot.bytes.len()).ok()?,
        checksum: Sha256::digest(&snapshot.bytes).to_vec(),
    })
}

/// Stage, seal and install `snapshot` on `link.to`, returning the engine reply.
pub(super) async fn send<C>(
    caller: &C,
    link: Link,
    route: Route,
    term: Term,
    seq: u64,
    snapshot: SnapshotObject,
) -> Option<Message>
where
    C: crate::ClusterRuntimeOps + bmux_plugin::ServiceCaller + Sync,
{
    let header = snapshot_header(&route.cluster, link, term, &snapshot)?;
    let reply = match stage(caller, link, &route, &header, &snapshot.bytes).await {
        Some(()) => install(caller, link, &route, header.clone(), term, seq).await,
        None => None,
    };
    let installed = matches!(
        reply,
        Some(Message::SnapshotReply {
            installed: Some(_),
            ..
        })
    );
    if !installed {
        // Free the peer's reservation now rather than after idle expiry.
        let _ = abort(caller, link, &route, &header).await;
    }
    reply
}

/// Prepare a fresh proof and sign `operation` over the complete request.
pub(super) async fn signed<C>(
    caller: &C,
    link: Link,
    route: &Route,
    operation: &str,
    header: &TransferHeader,
    offset: u64,
    data: Vec<u8>,
) -> Option<(String, ChunkRequest)>
where
    C: crate::ClusterRuntimeOps + bmux_plugin::ServiceCaller + Sync,
{
    let Prepared {
        identity,
        endpoint,
        proof,
        ..
    } = prepare(caller, link.local, link.to, route.clone()).await?;
    let mut request = ChunkRequest {
        header: header.clone(),
        offset,
        data,
        proof,
    };
    request.proof.signature = identity
        .sign(&crate::consensus_network::transfer_signing_payload(operation, &request).ok()?);
    Some((endpoint, request))
}

pub(super) fn whole(request: ChunkRequest) -> TransferRequest {
    TransferRequest {
        header: request.header,
        proof: request.proof,
    }
}

async fn stage<C>(
    caller: &C,
    link: Link,
    route: &Route,
    header: &TransferHeader,
    bytes: &[u8],
) -> Option<()>
where
    C: crate::ClusterRuntimeOps + bmux_plugin::ServiceCaller + Sync,
{
    use bmux_cluster_plugin_api::cluster_consensus_transfer::client;
    let (endpoint, request) = signed(caller, link, route, "begin", header, 0, Vec::new()).await?;
    let mut remote = crate::endpoint::EndpointDispatchClient::new(caller, endpoint);
    // A matching earlier transfer continues from its staged offset.
    let offset =
        switchy_async::time::timeout(RPC_TIMEOUT, client::begin(&mut remote, whole(request)))
            .await
            .ok()?
            .ok()?
            .ok()?;
    upload(bytes, offset, |offset, data| async move {
        let (endpoint, request) =
            signed(caller, link, route, "chunk", header, offset, data).await?;
        let mut remote = crate::endpoint::EndpointDispatchClient::new(caller, endpoint);
        switchy_async::time::timeout(RPC_TIMEOUT, client::chunk(&mut remote, request))
            .await
            .ok()?
            .ok()?
            .ok()
    })
    .await?;
    let (endpoint, request) = signed(caller, link, route, "finish", header, 0, Vec::new()).await?;
    let mut remote = crate::endpoint::EndpointDispatchClient::new(caller, endpoint);
    let FinishOutcome::Sealed =
        switchy_async::time::timeout(RPC_TIMEOUT, client::finish(&mut remote, whole(request)))
            .await
            .ok()?
            .ok()?
            .ok()?;
    Some(())
}

async fn abort<C>(caller: &C, link: Link, route: &Route, header: &TransferHeader) -> Option<()>
where
    C: crate::ClusterRuntimeOps + bmux_plugin::ServiceCaller + Sync,
{
    let (endpoint, request) = signed(caller, link, route, "abort", header, 0, Vec::new()).await?;
    let mut remote = crate::endpoint::EndpointDispatchClient::new(caller, endpoint);
    switchy_async::time::timeout(
        RPC_TIMEOUT,
        bmux_cluster_plugin_api::cluster_consensus_transfer::client::abort(
            &mut remote,
            whole(request),
        ),
    )
    .await
    .ok()?
    .ok()?
    .ok()
    .map(|_| ())
}

/// Send `bytes` from `offset` one chunk at a time, one chunk outstanding,
/// following the receiver's expected offset. Fails without progress, on an
/// out-of-range offset, or after a bounded number of resumes.
pub(super) async fn upload<F, Fut>(bytes: &[u8], mut offset: u64, mut send: F) -> Option<()>
where
    F: FnMut(u64, Vec<u8>) -> Fut,
    Fut: Future<Output = Option<ChunkOutcome>>,
{
    let total = u64::try_from(bytes.len()).ok()?;
    let mut resumes = 0;
    while offset < total {
        let start = usize::try_from(offset).ok()?;
        let end = bytes.len().min(start.checked_add(CHUNK_BYTES)?);
        match send(offset, bytes[start..end].to_vec()).await? {
            ChunkOutcome::Accepted { offset: next } => {
                if next <= offset || next > total {
                    return None;
                }
                offset = next;
            }
            ChunkOutcome::ExpectedOffset { offset: expected } => {
                resumes += 1;
                if expected > total || resumes > MAX_RESUMES {
                    return None;
                }
                offset = expected;
            }
        }
    }
    (offset == total).then_some(())
}

async fn install<C>(
    caller: &C,
    link: Link,
    route: &Route,
    reference: TransferHeader,
    term: Term,
    seq: u64,
) -> Option<Message>
where
    C: crate::ClusterRuntimeOps + bmux_plugin::ServiceCaller + Sync,
{
    let Prepared {
        identity,
        endpoint,
        proof,
        ..
    } = prepare(caller, link.local, link.to, route.clone()).await?;
    let mut request = InstallRequest {
        request_id: request_id(link.incarnation, seq),
        reference,
        proof,
    };
    request.proof.signature = identity
        .sign(&crate::consensus_network::owned_snapshot_install_signing_payload(&request).ok()?);
    let response = crate::consensus_network::exchange_snapshot_install(caller, &endpoint, &request)
        .await
        .ok()?;
    snapshot_reply(&request, &response, link.to, term, seq)
}

/// Map a verified install response onto the engine's reply. An installed
/// boundary and a signed higher term re-enter; anything else is a failure.
/// Verification is repeated here so unverified evidence can never map.
pub(super) fn snapshot_reply(
    request: &InstallRequest,
    response: &InstallResponse,
    to: NodeId,
    term: Term,
    seq: u64,
) -> Option<Message> {
    if response.source.as_slice() != to.as_bytes() {
        return None;
    }
    crate::consensus_network::verify_owned_snapshot_install_response(request, response).ok()?;
    match &response.outcome {
        InstallOutcome::Installed { boundary } => Some(Message::SnapshotReply {
            term: Term(response.term),
            seq,
            installed: Some(from_wire(boundary)?),
        }),
        InstallOutcome::Rejected {
            reason: InstallRejection::StaleTerm,
        } if response.term > term.0 => Some(Message::SnapshotReply {
            term: Term(response.term),
            seq,
            installed: None,
        }),
        InstallOutcome::Rejected { .. } => None,
    }
}

// ----- inbound ---------------------------------------------------------------

type Rpc<'a, T, E> = Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'a>>;

impl<C, V> WireService<C, V>
where
    C: crate::ClusterRuntimeOps + Send + Sync + 'static,
    V: Send + Unpin + 'static,
{
    /// Fence and authenticate one transfer operation, returning its sender.
    ///
    /// Staging grants no authority, so a higher leader term is admitted (the
    /// install later carries it to the engine); a lower one is a deposed
    /// leader's. `consume` spends the single-use proof; the sealing half of
    /// `finish` revalidates the already consumed proof instead.
    fn admit(
        &self,
        operation: &str,
        request: &ChunkRequest,
        consume: bool,
    ) -> Result<NodeId, TransferError> {
        crate::consensus_network::transfer_signing_payload(operation, request)
            .map_err(|_| TransferError::InvalidObject)?;
        if !matches!(request.header.purpose, TransferPurpose::Snapshot { .. }) {
            // Staged entries are not consumed by the driver's append path.
            return Err(TransferError::Incompatible);
        }
        if self.staging.closing.load(Ordering::SeqCst) {
            return Err(TransferError::CapacityUnavailable);
        }
        let (cluster, members) = self.members.snapshot();
        if request.header.cluster_id != cluster {
            return Err(TransferError::Unauthorized);
        }
        if request.header.leader_term < self.driver.status().term.0 {
            return Err(TransferError::StaleTerm);
        }
        let source = if consume {
            crate::consensus_network::authenticate_transfer_request(
                self.caller.as_ref(),
                operation,
                self.local,
                &members,
                request,
            )
            .map_err(|_| TransferError::Unauthorized)?
        } else {
            crate::membership::revalidate_peer_for_members(
                self.caller.as_ref(),
                &crate::ClusterPeerAuthenticateRequest {
                    proof: request.proof.peer.clone(),
                },
                &members,
            )
            .ok()
            .and_then(|peer| peer.node_id.parse::<NodeId>().ok())
            .filter(|source| source.as_bytes().as_slice() == request.header.sender.as_slice())
            .ok_or(TransferError::Unauthorized)?
        };
        if !members.iter().any(|member| {
            member.node_id == source.to_string()
                && crate::consensus_network::supports_owned_consensus(member)
        }) {
            return Err(TransferError::Unauthorized);
        }
        Ok(source)
    }

    async fn transfer_begin(&self, request: TransferRequest) -> Result<u64, TransferError> {
        let request = chunk_form(request);
        let source = self.admit("begin", &request, true)?;
        let (key, binding) = transfer_key(source, &request.header)?;
        let total = request.header.total_bytes;
        let digest: [u8; 32] = request
            .header
            .checksum
            .as_slice()
            .try_into()
            .map_err(|_| TransferError::InvalidObject)?;
        self.staging
            .run(move |staged, now| {
                staged
                    .transfers
                    .begin(&staged.directory, key, binding, total, digest, now)
                    .map_err(|error| match error.kind() {
                        io::ErrorKind::WouldBlock => TransferError::CapacityUnavailable,
                        _ => TransferError::ConflictingIdentity,
                    })
            })
            .await
    }

    async fn transfer_chunk(&self, request: ChunkRequest) -> Result<ChunkOutcome, TransferError> {
        let source = self.admit("chunk", &request, true)?;
        let (key, binding) = transfer_key(source, &request.header)?;
        let total = request.header.total_bytes;
        let digest: [u8; 32] = request
            .header
            .checksum
            .as_slice()
            .try_into()
            .map_err(|_| TransferError::InvalidObject)?;
        let ChunkRequest { offset, data, .. } = request;
        self.staging
            .run(move |staged, now| {
                staged
                    .transfers
                    .validate_binding(key, binding)
                    .map_err(|error| staging_error(&error))?;
                match staged.transfers.chunk(key, offset, &data, now) {
                    Ok(offset) => Ok(ChunkOutcome::Accepted { offset }),
                    Err(error) => {
                        // A matching begin replay reports the continuation
                        // offset without modifying or refreshing staged bytes.
                        let expected = staged
                            .transfers
                            .begin(&staged.directory, key, binding, total, digest, now)
                            .map_err(|_| staging_error(&error))?;
                        if expected == offset {
                            Err(staging_error(&error))
                        } else {
                            Ok(ChunkOutcome::ExpectedOffset { offset: expected })
                        }
                    }
                }
            })
            .await
    }

    /// Validate canonical bytes outside the staging lock, then revalidate
    /// authority before sealing. Cancellation leaves the transfer failed until
    /// abort or expiry; it is never sealed unvalidated.
    async fn transfer_finish(
        &self,
        request: TransferRequest,
    ) -> Result<FinishOutcome, TransferError> {
        let request = chunk_form(request);
        let source = self.admit("finish", &request, true)?;
        let permit = self
            .staging
            .validation
            .try_acquire()
            .map_err(|_| TransferError::CapacityUnavailable)?;
        let (key, binding) = transfer_key(source, &request.header)?;
        let ticket = self
            .staging
            .run(move |staged, _| {
                staged
                    .transfers
                    .validate_binding(key, binding)
                    .map_err(|error| staging_error(&error))?;
                staged
                    .transfers
                    .prepare_validation(key)
                    .map_err(|_| TransferError::InvalidObject)
            })
            .await?;
        let header = request.header.clone();
        let staging = self.staging.clone();
        let validated = switchy_async::task::spawn_blocking(move || {
            let status = AtomicU8::new(0);
            ticket.validate(|file, total| {
                crate::consensus_network::validate_staged_object(
                    file,
                    total,
                    &header,
                    &status,
                    &staging.closing,
                )
            })
        })
        .await
        .map_err(|_| TransferError::StorageFailure)?
        .map_err(|_| TransferError::InvalidObject)?;
        self.admit("finish", &request, false)?;
        self.staging
            .run(move |staged, _| {
                staged
                    .transfers
                    .validate_binding(key, binding)
                    .map_err(|error| staging_error(&error))?;
                staged
                    .transfers
                    .complete_validation(key, validated)
                    .map_err(|_| TransferError::InvalidObject)
            })
            .await?;
        drop(permit);
        Ok(FinishOutcome::Sealed)
    }

    async fn transfer_abort(
        &self,
        request: TransferRequest,
    ) -> Result<AbortOutcome, TransferError> {
        let request = chunk_form(request);
        let source = self.admit("abort", &request, true)?;
        let (key, binding) = transfer_key(source, &request.header)?;
        self.staging
            .run(move |staged, _| {
                match staged.transfers.validate_binding(key, binding) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        return Ok(AbortOutcome::NoStagingRemains);
                    }
                    Err(error) => return Err(staging_error(&error)),
                    Ok(()) => {}
                }
                staged
                    .transfers
                    .abort(key)
                    .map(|()| AbortOutcome::NoStagingRemains)
                    .map_err(|error| match error.kind() {
                        io::ErrorKind::WouldBlock => TransferError::CapacityUnavailable,
                        _ => TransferError::CleanupIncomplete,
                    })
            })
            .await
    }

    /// Authenticate, resolve the sealed snapshot, deliver it to the engine and
    /// sign its reply. Exact retries of the last 64 installs replay their result.
    async fn snapshot_install(
        &self,
        request: InstallRequest,
    ) -> Result<InstallResponse, InstallError> {
        crate::consensus_network::owned_snapshot_install_signing_payload(&request)
            .map_err(|_| InstallError::InvalidRequest)?;
        let identity = crate::load_or_create_node_identity(self.caller.as_ref())
            .map_err(|_| InstallError::Unauthorized)?;
        if *identity.node_id() != self.local {
            return Err(InstallError::Unauthorized);
        }
        let (cluster, members) = self.members.snapshot();
        if request.reference.cluster_id != cluster {
            return Err(InstallError::Unauthorized);
        }
        let source = crate::consensus_network::authenticate_owned_snapshot_install(
            self.caller.as_ref(),
            self.local,
            &members,
            &request,
        )
        .map_err(|_| InstallError::Unauthorized)?;
        if !members.iter().any(|member| {
            member.node_id == self.local.to_string()
                && member.state
                    == bmux_cluster_plugin_api::cluster_types::ClusterMemberState::Active
                && crate::consensus_network::supports_owned_snapshot_install(member)
        }) {
            return Err(InstallError::Incompatible);
        }
        let TransferPurpose::Snapshot { value } = &request.reference.purpose else {
            return Err(InstallError::InvalidRequest);
        };
        let wire_boundary = value
            .last_included
            .clone()
            .ok_or(InstallError::InvalidRequest)?;
        let boundary = from_wire(&wire_boundary).ok_or(InstallError::InvalidRequest)?;
        let membership = value
            .membership
            .as_ref()
            .and_then(membership_from_wire)
            .ok_or(InstallError::InvalidRequest)?;
        let (key, binding) =
            transfer_key(source, &request.reference).map_err(|_| InstallError::InvalidRequest)?;
        let receipt = Receipt {
            request_id: request.request_id,
            source,
            key,
            binding,
            boundary,
            term: request.reference.leader_term,
        };
        let (outcome, term) = match self.staging.replay(&receipt) {
            Some(true) => (
                InstallOutcome::Installed {
                    boundary: wire_boundary,
                },
                receipt.term,
            ),
            Some(false) => (
                InstallOutcome::Rejected {
                    reason: InstallRejection::ConflictingReference,
                },
                self.driver.status().term.0,
            ),
            None => {
                self.install_sealed(&request, receipt, wire_boundary, membership)
                    .await?
            }
        };
        let mut response = InstallResponse {
            request_id: request.request_id,
            source: self.local.as_bytes().to_vec(),
            term,
            outcome,
            signature: Vec::new(),
        };
        response.signature = identity.sign(
            &crate::consensus_network::owned_snapshot_install_response_signing_payload(
                &request, &response,
            )
            .map_err(|_| InstallError::ExecutionFailed)?,
        );
        Ok(response)
    }

    async fn install_sealed(
        &self,
        request: &InstallRequest,
        receipt: Receipt,
        wire_boundary: bmux_cluster_plugin_api::cluster_consensus_transfer_types::LogIdentity,
        membership: StoredMembership,
    ) -> Result<(InstallOutcome, u64), InstallError> {
        let current = self.driver.status().term.0;
        let leader_term = request.reference.leader_term;
        let rejected = |reason, term| Ok((InstallOutcome::Rejected { reason }, term));
        if leader_term < current {
            return rejected(InstallRejection::StaleTerm, current);
        }
        let _installing = self
            .staging
            .installing
            .try_acquire()
            .map_err(|_| InstallError::CapacityUnavailable)?;
        let bytes = match self.read_sealed(request, &receipt).await? {
            Ok(bytes) => bytes,
            Err(reason) => return rejected(reason, current),
        };
        let accepted = (receipt.source, receipt.boundary);
        self.staging.accept(accepted, bytes);
        let reply = self
            .driver
            .request(
                receipt.source,
                Message::Snapshot {
                    term: Term(leader_term),
                    seq: sequence(request.request_id),
                    last_included: receipt.boundary,
                    membership,
                },
            )
            .await;
        self.staging.withdraw(accepted);
        let Message::SnapshotReply {
            term, installed, ..
        } = reply.ok_or(InstallError::Stopped)?
        else {
            return Err(InstallError::ExecutionFailed);
        };
        match installed {
            Some(installed) if installed == receipt.boundary && term.0 == leader_term => {
                // Cleanup cannot downgrade a durable install; failed deletion
                // keeps the reservation until expiry retries it.
                let key = receipt.key;
                let _ = self
                    .staging
                    .run(move |staged, _| Ok(staged.transfers.abort(key)))
                    .await;
                self.staging.record(receipt);
                Ok((
                    InstallOutcome::Installed {
                        boundary: wire_boundary,
                    },
                    leader_term,
                ))
            }
            Some(_) => Err(InstallError::ExecutionFailed),
            None => rejected(InstallRejection::StaleTerm, term.0),
        }
    }

    /// Read and check the exact sealed object named by the install reference.
    async fn read_sealed(
        &self,
        request: &InstallRequest,
        receipt: &Receipt,
    ) -> Result<Result<Vec<u8>, InstallRejection>, InstallError> {
        let reference = request.reference.clone();
        let (key, binding) = (receipt.key, receipt.binding);
        let staging = self.staging.clone();
        switchy_async::task::spawn_blocking(move || {
            let mut staged = staging.lock();
            let result = sealed_bytes(&mut staged, key, binding, &reference, &staging.closing);
            drop(staged);
            result
        })
        .await
        .map_err(|_| InstallError::StorageFailure)
    }
}

impl<C, V> bmux_cluster_plugin_api::cluster_consensus_transfer::ClusterConsensusTransferService
    for WireService<C, V>
where
    C: crate::ClusterRuntimeOps + Send + Sync + 'static,
    V: Send + Unpin + 'static,
{
    fn begin(&self, request: TransferRequest) -> Rpc<'_, u64, TransferError> {
        Box::pin(self.transfer_begin(request))
    }

    fn chunk(&self, request: ChunkRequest) -> Rpc<'_, ChunkOutcome, TransferError> {
        Box::pin(self.transfer_chunk(request))
    }

    fn finish(&self, request: TransferRequest) -> Rpc<'_, FinishOutcome, TransferError> {
        Box::pin(self.transfer_finish(request))
    }

    fn abort(&self, request: TransferRequest) -> Rpc<'_, AbortOutcome, TransferError> {
        Box::pin(self.transfer_abort(request))
    }
}

impl<C, V> bmux_cluster_plugin_api::cluster_consensus_snapshot::ClusterConsensusSnapshotService
    for WireService<C, V>
where
    C: crate::ClusterRuntimeOps + Send + Sync + 'static,
    V: Send + Unpin + 'static,
{
    fn install(&self, request: InstallRequest) -> Rpc<'_, InstallResponse, InstallError> {
        Box::pin(self.snapshot_install(request))
    }
}
