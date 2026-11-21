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

pub(crate) const PEER_READ_OPERATION: &str = "cluster-peer-read/v1/read_linearizable";

/// Authenticate an owned transfer before reserving staging resources.
///
/// The caller supplies members read from committed owned control state and must
/// serialize that read, current-term/eligibility validation and staging admission.
/// This function deliberately does not load legacy membership projections and
/// does not authorize snapshot installation.
/// # Errors
/// Rejects malformed, misdirected, oversized or unauthenticated requests.
pub fn authenticate_transfer_request(
    caller: &impl crate::ClusterRuntimeOps,
    operation: &str,
    local: NodeId,
    members: &[bmux_cluster_plugin_api::cluster_types::ClusterMember],
    request: &bmux_cluster_plugin_api::cluster_consensus_transfer_types::ChunkRequest,
) -> Result<NodeId, String> {
    validate_transfer_allocation(request)?;
    let header = &request.header;
    if header.sender.len() != 32
        || header.recipient.as_slice() != local.as_bytes()
        || header.checksum.len() != 32
        || request.proof.signature.len() != 64
        || header.cluster_id != request.proof.peer.challenge.cluster_id
        || request.proof.credential_serial != request.proof.peer.claimant_credential_serial
    {
        return Err("invalid transfer identity or proof binding".into());
    }
    let source = request.proof.peer.claimant_node_id.parse::<NodeId>()?;
    if source.as_bytes() != header.sender.as_slice() {
        return Err("transfer sender differs from authenticated claimant".into());
    }
    let signed = transfer_signing_payload(operation, request)?;
    verify_node_signature(&source.to_string(), &signed, &request.proof.signature)?;
    crate::membership::authenticate_peer_for_members(
        caller,
        &crate::ClusterPeerAuthenticateRequest {
            proof: request.proof.peer.clone(),
        },
        members,
    )?;
    Ok(source)
}

/// Canonical domain-separated signature input for the unpublished transfer v1.
///
/// Non-chunk operations use offset zero and empty data. The signature field is
/// excluded; all other proof/header fields use generated typed-stable encoding.
/// # Errors
/// Rejects unknown operations, invalid chunk ranges and oversized envelopes.
pub fn transfer_signing_payload(
    operation: &str,
    request: &bmux_cluster_plugin_api::cluster_consensus_transfer_types::ChunkRequest,
) -> Result<Vec<u8>, String> {
    if !matches!(operation, "begin" | "chunk" | "finish" | "abort")
        || request.data.len() > 64 * 1024
        || (operation != "chunk" && (request.offset != 0 || !request.data.is_empty()))
        || request
            .offset
            .checked_add(u64::try_from(request.data.len()).map_err(|_| "transfer size overflow")?)
            .is_none_or(|end| end > request.header.total_bytes)
    {
        return Err("invalid transfer operation or range".into());
    }
    validate_transfer_allocation(request)?;
    validate_transfer_descriptor(&request.header)?;
    let mut unsigned = request.clone();
    unsigned.proof.signature.clear();
    let encoded = encode_service_message(&unsigned).map_err(|error| error.to_string())?;
    if encoded.len() > 1024 * 1024 - 1024 {
        return Err("transfer envelope exceeds limit".into());
    }
    let mut signed = b"bmux.cluster.consensus-transfer.v1\0".to_vec();
    signed.extend_from_slice(operation.as_bytes());
    signed.push(0);
    signed.extend(encoded);
    Ok(signed)
}

/// Bounded, domain-separated signing input for owned votes.
/// # Errors
/// Rejects oversized proof fields and malformed resource identities before cloning.
pub fn owned_vote_signing_payload(
    request: &bmux_cluster_plugin_api::cluster_consensus_vote_types::VoteRequest,
) -> Result<Vec<u8>, String> {
    let proof = &request.proof;
    let challenge = &proof.peer.challenge;
    let mut remaining = 16 * 1024usize;
    for field in [
        &request.cluster_id,
        &proof.credential_serial,
        &proof.peer.claimant_node_id,
        &proof.peer.claimant_credential_serial,
        &proof.peer.claimant_signature,
        &challenge.cluster_id,
        &challenge.verifier_node_id,
        &challenge.verifier_credential_serial,
        &challenge.audience_node_id,
        &challenge.nonce,
        &challenge.signature,
    ] {
        remaining = remaining
            .checked_sub(field.len())
            .ok_or("vote proof exceeds budget")?;
    }
    if proof.signature.len() > 64 {
        return Err("invalid vote signature length".into());
    }
    transfer_node(&request.sender)?;
    transfer_node(&request.recipient)?;
    if let Some(last) = &request.last_log {
        transfer_node(&last.leader)?;
        if last.term > request.term {
            return Err("vote log term exceeds candidate term".into());
        }
    }
    let mut unsigned = request.clone();
    unsigned.proof.signature.clear();
    let mut signed = b"bmux.cluster.consensus-vote.v1\0request\0".to_vec();
    signed.extend(encode_service_message(&unsigned).map_err(|error| error.to_string())?);
    Ok(signed)
}

/// Bind a signed vote response to the complete request and intended responder.
/// # Errors
/// Rejects malformed, oversized, or mismatched request/response pairs.
pub fn owned_vote_response_signing_payload(
    request: &bmux_cluster_plugin_api::cluster_consensus_vote_types::VoteRequest,
    response: &bmux_cluster_plugin_api::cluster_consensus_vote_types::VoteResponse,
) -> Result<Vec<u8>, String> {
    let request_bytes = owned_vote_signing_payload(request)?;
    if response.source != request.recipient {
        return Err("vote responder mismatch".into());
    }
    if response.request_id != request.request_id || response.signature.len() > 64 {
        return Err("vote response binding mismatch".into());
    }
    let mut unsigned = response.clone();
    unsigned.signature.clear();
    let mut signed = b"bmux.cluster.consensus-vote.v1\0response\0".to_vec();
    signed.extend(
        encode_service_message(&(request_bytes, &request.proof.signature, unsigned))
            .map_err(|error| error.to_string())?,
    );
    Ok(signed)
}

/// Authenticate a vote against the durable members supplied by its executor.
/// # Errors
/// Rejects invalid signatures, identity bindings, and revoked or replayed proofs.
pub fn authenticate_owned_vote(
    caller: &impl crate::ClusterRuntimeOps,
    local: NodeId,
    members: &[bmux_cluster_plugin_api::cluster_types::ClusterMember],
    request: &bmux_cluster_plugin_api::cluster_consensus_vote_types::VoteRequest,
) -> Result<NodeId, String> {
    let signed = owned_vote_signing_payload(request)?;
    let source = request.proof.peer.claimant_node_id.parse::<NodeId>()?;
    if request.recipient.as_slice() != local.as_bytes()
        || request.sender.as_slice() != source.as_bytes()
        || request.cluster_id != request.proof.peer.challenge.cluster_id
        || request.proof.credential_serial != request.proof.peer.claimant_credential_serial
        || request.proof.signature.len() != 64
    {
        return Err("invalid vote identity or proof binding".into());
    }
    verify_node_signature(&source.to_string(), &signed, &request.proof.signature)?;
    crate::membership::authenticate_peer_for_members(
        caller,
        &crate::ClusterPeerAuthenticateRequest {
            proof: request.proof.peer.clone(),
        },
        members,
    )?;
    Ok(source)
}

/// Authenticate unpublished owned append against membership supplied by its owner.
///
/// The owner must serialize membership/term checks and durable mutation; this
/// helper does not authorize against a cached or legacy membership projection.
/// # Errors
/// Rejects malformed, oversized, misdirected or unauthenticated requests.
pub fn authenticate_owned_append(
    caller: &impl crate::ClusterRuntimeOps,
    local: NodeId,
    members: &[bmux_cluster_plugin_api::cluster_types::ClusterMember],
    request: &bmux_cluster_plugin_api::cluster_consensus_rpc_types::AppendRequest,
) -> Result<NodeId, String> {
    let signed = owned_append_signing_payload(request)?;
    let source = request.proof.peer.claimant_node_id.parse::<NodeId>()?;
    if request.recipient.as_slice() != local.as_bytes()
        || request.sender.as_slice() != source.as_bytes()
        || request.cluster_id != request.proof.peer.challenge.cluster_id
        || request.proof.credential_serial != request.proof.peer.claimant_credential_serial
        || request.proof.signature.len() != 64
    {
        return Err("invalid append identity or proof binding".into());
    }
    verify_node_signature(&source.to_string(), &signed, &request.proof.signature)?;
    crate::membership::authenticate_peer_for_members(
        caller,
        &crate::ClusterPeerAuthenticateRequest {
            proof: request.proof.peer.clone(),
        },
        members,
    )?;
    Ok(source)
}

/// Bounded, domain-separated signing input for owned append, not legacy Raft.
/// # Errors
/// Rejects malformed identities and requests exceeding the unpublished v1 budget.
pub fn owned_append_signing_payload(
    request: &bmux_cluster_plugin_api::cluster_consensus_rpc_types::AppendRequest,
) -> Result<Vec<u8>, String> {
    use bmux_cluster_plugin_api::cluster_consensus_rpc_types::AppendEntry;
    let mut remaining = 512 * 1024usize;
    let mut charge = |len: usize| -> Result<(), String> {
        remaining = remaining
            .checked_sub(len.checked_add(128).ok_or("append size overflow")?)
            .ok_or("append allocation budget exceeded")?;
        Ok(())
    };
    let p = &request.proof;
    let c = &p.peer.challenge;
    for field in [
        &request.cluster_id,
        &p.credential_serial,
        &p.peer.claimant_node_id,
        &p.peer.claimant_credential_serial,
        &p.peer.claimant_signature,
        &c.cluster_id,
        &c.verifier_node_id,
        &c.verifier_credential_serial,
        &c.audience_node_id,
        &c.nonce,
        &c.signature,
    ] {
        charge(field.len())?;
    }
    charge(p.signature.len())?;
    transfer_node(&request.sender)?;
    transfer_node(&request.recipient)?;
    for identity in request.previous.iter().chain(request.leader_commit.iter()) {
        transfer_node(&identity.leader)?;
    }
    if request.entries.len() > 64 {
        return Err("append entry count exceeded".into());
    }
    for entry in &request.entries {
        match entry {
            AppendEntry::Inline { canonical } => charge(canonical.len())?,
            AppendEntry::Staged { reference } => {
                charge(128)?;
                transfer_node(&reference.identity.leader)?;
                if reference.checksum.len() != 32 || reference.total_bytes == 0 {
                    return Err("invalid staged append reference".into());
                }
            }
        }
    }
    let mut unsigned = request.clone();
    unsigned.proof.signature.clear();
    let encoded = encode_service_message(&unsigned).map_err(|error| error.to_string())?;
    if encoded.len() > 1024 * 1024 - 1024 {
        return Err("append envelope exceeds limit".into());
    }
    let mut signed = b"bmux.cluster.consensus-rpc.v1\0append\0".to_vec();
    signed.extend(encoded);
    Ok(signed)
}

fn transfer_node(bytes: &[u8]) -> Result<NodeId, String> {
    let node = NodeId::from_bytes(
        bytes
            .try_into()
            .map_err(|_| "invalid transfer node length")?,
    );
    node.public_key()?;
    Ok(node)
}

pub(crate) fn transfer_position(
    value: &bmux_cluster_plugin_api::cluster_consensus_transfer_types::LogIdentity,
) -> Result<crate::consensus_model::LogIdentity, String> {
    Ok(crate::consensus_model::LogIdentity {
        term: crate::consensus_model::Term(value.term),
        leader: transfer_node(&value.leader)?,
        index: crate::consensus_model::LogIndex(value.index),
    })
}

#[test]
fn staged_reader_stops_after_cancellation() {
    use std::io::{Read, Seek, Write};
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(&[1; 16384]).unwrap();
    file.rewind().unwrap();
    let status = std::sync::atomic::AtomicU8::new(0);
    let closing = std::sync::atomic::AtomicBool::new(false);
    let mut reader = CancellableTransferReader {
        file: &mut file,
        status: &status,
        closing: &closing,
    };
    let mut bytes = [0; 16384];
    assert_eq!(reader.read(&mut bytes).unwrap(), 8192);
    closing.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(reader.read_exact(&mut bytes[..1]).is_err());
    assert!(reader.rewind().is_err());
}

struct CancellableTransferReader<'a> {
    file: &'a mut std::fs::File,
    status: &'a std::sync::atomic::AtomicU8,
    closing: &'a std::sync::atomic::AtomicBool,
}

impl CancellableTransferReader<'_> {
    fn check(&self) -> std::io::Result<()> {
        if self.status.load(std::sync::atomic::Ordering::SeqCst) != 0
            || self.closing.load(std::sync::atomic::Ordering::SeqCst)
        {
            // Interrupted is retried by read_exact; cancellation must terminate it.
            return Err(std::io::Error::other("staged validation cancelled"));
        }
        Ok(())
    }
}

impl std::io::Read for CancellableTransferReader<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        self.check()?;
        let length = bytes.len().min(8192);
        std::io::Read::read(self.file, &mut bytes[..length])
    }
}

impl std::io::Seek for CancellableTransferReader<'_> {
    fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
        self.check()?;
        std::io::Seek::seek(self.file, position)
    }
}

pub(crate) fn validate_staged_object(
    file: &mut std::fs::File,
    total: u64,
    header: &bmux_cluster_plugin_api::cluster_consensus_transfer_types::TransferHeader,
    status: &std::sync::atomic::AtomicU8,
    closing: &std::sync::atomic::AtomicBool,
) -> std::io::Result<()> {
    use bmux_cluster_plugin_api::cluster_consensus_transfer_types::TransferPurpose;
    use std::io::Error;
    let length = usize::try_from(total).map_err(Error::other)?;
    let mut file = CancellableTransferReader {
        file,
        status,
        closing,
    };
    file.check()?;
    match &header.purpose {
        TransferPurpose::Entry { value } => {
            if total > 64 * 1024 * 1024 {
                return Err(Error::other("entry exceeds owned storage bound"));
            }
            let entry = crate::consensus_storage::decode_owned_entry_reader(&mut file, length)
                .map_err(Error::other)?;
            if entry.identity != transfer_position(&value.identity).map_err(Error::other)? {
                return Err(Error::other("staged entry identity mismatch"));
            }
        }
        TransferPurpose::Snapshot { value } => {
            if total > 300 * 1024 * 1024 {
                return Err(Error::other("snapshot exceeds owned storage bound"));
            }
            let (descriptor, _) =
                crate::consensus_storage::decode_owned_snapshot_reader(&mut file, length)
                    .map_err(Error::other)?;
            let membership = value
                .membership
                .as_ref()
                .map(|m| {
                    let configurations = m
                        .configurations
                        .iter()
                        .map(|c| {
                            c.voters
                                .iter()
                                .map(|v| transfer_node(v))
                                .collect::<Result<Vec<_>, _>>()
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    let nodes = m
                        .nodes
                        .iter()
                        .map(|n| Ok((transfer_node(&n.node)?, n.endpoint.clone())))
                        .collect::<Result<Vec<_>, String>>()?;
                    Ok::<_, String>(crate::consensus_model::StoredMembership {
                        introduced_at: Some(transfer_position(&m.introduced_at)?),
                        membership: crate::consensus_model::Membership::new(configurations, nodes)
                            .map_err(|e| format!("invalid membership: {e:?}"))?,
                    })
                })
                .transpose()
                .map_err(Error::other)?;
            if descriptor.format_version != value.format_version
                || descriptor.cluster_id != header.cluster_id
                || descriptor.cluster_id != value.cluster_id
                || descriptor.snapshot_id != value.snapshot_id
                || descriptor.last_included
                    != value
                        .last_included
                        .as_ref()
                        .map(transfer_position)
                        .transpose()
                        .map_err(Error::other)?
                || descriptor.membership != membership
                || descriptor.total_bytes != total
                || descriptor.checksum.as_slice() != value.checksum.as_slice()
            {
                return Err(Error::other("staged snapshot descriptor mismatch"));
            }
        }
    }
    file.check()
}

fn validate_transfer_descriptor(
    header: &bmux_cluster_plugin_api::cluster_consensus_transfer_types::TransferHeader,
) -> Result<(), String> {
    use bmux_cluster_plugin_api::cluster_consensus_transfer_types::TransferPurpose;
    transfer_node(&header.sender)?;
    transfer_node(&header.recipient)?;
    if header.cluster_id.is_empty() || header.total_bytes == 0 || header.checksum.len() != 32 {
        return Err("invalid transfer header".into());
    }
    match &header.purpose {
        TransferPurpose::Entry { value } => {
            let identity = transfer_position(&value.identity)?;
            let previous = value.previous.as_ref().map(transfer_position).transpose()?;
            if identity.term.0 > header.leader_term
                || previous.map_or(Some(0), |p| p.index.0.checked_add(1)) != Some(identity.index.0)
            {
                return Err("invalid transferred entry position".into());
            }
            crate::consensus_model::validate_position_prefix(previous, Some(identity))
                .map_err(|_| "invalid transferred entry prefix")?;
        }
        TransferPurpose::Snapshot { value } => {
            let boundary = value
                .last_included
                .as_ref()
                .map(transfer_position)
                .transpose()?;
            if value.format_version != 1
                || value.cluster_id != header.cluster_id
                || value.total_bytes != header.total_bytes
                || value.checksum.len() != 32
                || value.snapshot_id.is_empty()
                || boundary.is_some_and(|b| b.term.0 > header.leader_term)
            {
                return Err("invalid transferred snapshot descriptor".into());
            }
            if let Some(membership) = &value.membership {
                let introduced = transfer_position(&membership.introduced_at)?;
                crate::consensus_model::validate_position_prefix(Some(introduced), boundary)
                    .map_err(|_| "snapshot membership exceeds boundary")?;
                let configurations = membership
                    .configurations
                    .iter()
                    .map(|c| {
                        c.voters
                            .iter()
                            .map(|v| transfer_node(v))
                            .collect::<Result<Vec<_>, _>>()
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let nodes = membership
                    .nodes
                    .iter()
                    .map(|n| Ok((transfer_node(&n.node)?, n.endpoint.clone())))
                    .collect::<Result<Vec<_>, String>>()?;
                crate::consensus_model::Membership::new(configurations, nodes)
                    .map_err(|_| "invalid transferred membership")?;
            }
        }
    }
    Ok(())
}

fn validate_transfer_allocation(
    request: &bmux_cluster_plugin_api::cluster_consensus_transfer_types::ChunkRequest,
) -> Result<(), String> {
    use bmux_cluster_plugin_api::cluster_consensus_transfer_types::TransferPurpose;
    // Charge per-field overhead as well as bytes, before any clone/encoding.
    // This bounds work for this helper, not allocations in the wire decoder.
    let mut remaining = 512 * 1024usize;
    let mut charge = |bytes: usize| -> Result<(), String> {
        remaining = remaining
            .checked_sub(bytes.checked_add(128).ok_or("transfer size overflow")?)
            .ok_or("transfer metadata exceeds allocation budget")?;
        Ok(())
    };
    let h = &request.header;
    let p = &request.proof;
    let c = &p.peer.challenge;
    for field in [
        &h.cluster_id,
        &p.credential_serial,
        &p.peer.claimant_node_id,
        &p.peer.claimant_credential_serial,
        &p.peer.claimant_signature,
        &c.cluster_id,
        &c.verifier_node_id,
        &c.verifier_credential_serial,
        &c.audience_node_id,
        &c.nonce,
        &c.signature,
    ] {
        charge(field.len())?;
    }
    for bytes in [
        &h.sender,
        &h.recipient,
        &h.checksum,
        &p.signature,
        &request.data,
    ] {
        charge(bytes.len())?;
    }
    match &h.purpose {
        TransferPurpose::Entry { value } => {
            charge(value.identity.leader.len())?;
            if let Some(previous) = &value.previous {
                charge(previous.leader.len())?;
            }
        }
        TransferPurpose::Snapshot { value } => {
            charge(value.cluster_id.len())?;
            charge(value.snapshot_id.len())?;
            charge(value.checksum.len())?;
            if let Some(position) = &value.last_included {
                charge(position.leader.len())?;
            }
            if let Some(membership) = &value.membership {
                charge(membership.introduced_at.leader.len())?;
                for configuration in &membership.configurations {
                    charge(0)?;
                    for voter in &configuration.voters {
                        charge(voter.len())?;
                    }
                }
                for node in &membership.nodes {
                    charge(node.node.len())?;
                    charge(node.endpoint.len())?;
                }
            }
        }
    }
    Ok(())
}

const RAFT_RPC_DOMAIN: &[u8] = b"bmux.cluster.raft-rpc.v1\0";
const DECODER_PROBE_OPERATION: &str = "capability-publication-decoder-v1";

fn decoder_proof_payload(request: &RaftRpcRequest) -> Result<Vec<u8>, String> {
    let mut payload = b"bmux.cluster.raft-decoder-proof.v1\0".to_vec();
    payload.extend(encode_service_message(request).map_err(|error| error.to_string())?);
    Ok(payload)
}

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

pub(super) trait DecoderAdmission: Send + Sync {
    fn verify<'a>(
        &'a self,
        target: NodeId,
        endpoint: &'a str,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
}

impl<C> DecoderAdmission for EndpointRaftNetworkFactory<C>
where
    C: ServiceCaller + Send + Sync + 'static,
{
    fn verify<'a>(
        &'a self,
        target: NodeId,
        endpoint: &'a str,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            EndpointRaftNetwork {
                caller: self.caller.clone(),
                identity: self.identity.clone(),
                target,
                endpoint: endpoint.to_string(),
            }
            .require_publication_decoder()
            .await
            .map_err(|error| format!("{error:?}"))
        })
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

    fn new_client(
        &mut self,
        target: NodeId,
        node: &BasicNode,
    ) -> impl std::future::Future<Output = Self::Network> {
        std::future::ready(EndpointRaftNetwork {
            caller: self.caller.clone(),
            identity: self.identity.clone(),
            target,
            endpoint: node.addr.clone(),
        })
    }
}

impl<C> EndpointRaftNetwork<C>
where
    C: ServiceCaller + Send + Sync + 'static,
{
    async fn require_publication_decoder(&self) -> Result<(), PeerAuthenticationFailure> {
        let proof = crate::endpoint::peer_authentication_proof(
            self.caller.as_ref(),
            &self.endpoint,
            &self.identity.node_id().to_string(),
            &self.target.to_string(),
        )
        .await?;
        let request = authenticated_request(
            DECODER_PROBE_OPERATION,
            self.target,
            &self.identity,
            proof,
            Vec::new(),
        )
        .map_err(PeerAuthenticationFailure::Local)?;
        let signed = decoder_proof_payload(&request).map_err(PeerAuthenticationFailure::Local)?;
        let mut remote = EndpointDispatchClient::new(self.caller.as_ref(), &self.endpoint);
        let proof =
            bmux_cluster_plugin_api::cluster_raft_decoder::client::probe(&mut remote, request)
                .await
                .map_err(|error| {
                    PeerAuthenticationFailure::Unreachable(format!(
                        "decoder negotiation failed: {error}"
                    ))
                })?
                .map_err(PeerAuthenticationFailure::Untrusted)?;
        verify_node_signature(&self.target.to_string(), &signed, &proof.signature)
            .map_err(PeerAuthenticationFailure::Untrusted)
    }

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
            "append_entries_v2" => {
                bmux_cluster_plugin_api::cluster_raft_rpc_v2::client::append_entries(
                    &mut remote,
                    envelope,
                )
                .await
            }
            "install_snapshot_v2" => {
                bmux_cluster_plugin_api::cluster_raft_rpc_v2::client::install_snapshot(
                    &mut remote,
                    envelope,
                )
                .await
            }
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
        if rpc.entries.iter().any(|entry| matches!(
            &entry.payload, openraft::EntryPayload::Normal(request) if request.0.starts_with(b"BMCAP001")
        )) {
            self.require_publication_decoder().await.map_err(Self::rpc_error)?;
            return self.invoke("append_entries_v2", &rpc).await.map_err(Self::rpc_error);
        }
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
        // This transport sends complete envelopes. Do not guess decoder support
        // from a partial chunk or scan arbitrary bytes for a format marker.
        if rpc.offset != 0 || !rpc.done {
            return Err(Self::rpc_error(PeerAuthenticationFailure::Local(
                "partial snapshot negotiation is unsupported".into(),
            )));
        }
        let advanced = crate::consensus_storage::snapshot_requires_publication_decoder(&rpc.data)
            .map_err(|error| {
            Self::rpc_error(PeerAuthenticationFailure::Local(error.to_string()))
        })?;
        if advanced {
            self.require_publication_decoder()
                .await
                .map_err(Self::rpc_error)?;
            return self
                .invoke("install_snapshot_v2", &rpc)
                .await
                .map_err(Self::rpc_error);
        }
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
        let mut nodes = self
            .nodes
            .write()
            .map_err(|_| "consensus node registry lock is poisoned".to_string())?;
        if nodes.contains_key(&node_id) {
            return Err("consensus node already owned in this runtime scope".into());
        }
        nodes.insert(node_id, node);
        drop(nodes);
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

    /// Returns the registered local node ID when exactly one consensus runtime
    /// is active.
    ///
    /// # Errors
    ///
    /// Returns an error for poisoned access or an ambiguous multi-runtime host.
    pub fn sole_node_id(&self) -> Result<Option<NodeId>, String> {
        let nodes = self
            .nodes
            .read()
            .map_err(|_| "consensus node registry lock is poisoned".to_string())?;
        match nodes.len() {
            0 => Ok(None),
            1 => Ok(nodes.keys().next().copied()),
            count => Err(format!(
                "cannot select one local consensus node from {count} active runtimes"
            )),
        }
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

    pub(crate) async fn authoritative_members(
        &self,
    ) -> Result<ControlStateView, ControlServiceError>
    where
        C: ServiceCaller + Send + Sync + 'static,
    {
        // Membership changes can briefly leave the local node without a leader
        // hint. Wait for discovery without weakening the linearizable read or
        // replaying a mutation; other errors remain immediately visible.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match self.read_linearizable_or_forward().await {
                Err(ControlServiceError::NotLeader {
                    leader_node_id: None,
                    leader_endpoint: None,
                }) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                result => return result,
            }
        }
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
        if matches!(
            &request.request,
            bmux_cluster_plugin_api::cluster_types::ControlCommandRequest::UpsertMember { .. }
                | bmux_cluster_plugin_api::cluster_types::ControlCommandRequest::SetMemberState { .. }
        ) {
            return Err(ControlServiceError::Rejected {
                error:
                    bmux_cluster_plugin_api::cluster_types::ControlCommandError::InvalidTransition {
                        reason:
                            "membership metadata requires an authenticated lifecycle transition"
                                .into(),
                    },
            });
        }
        match self.active()?.mutate(request.clone()).await {
            Err(ControlServiceError::NotLeader {
                leader_endpoint: Some(endpoint),
                ..
            }) => self.forward_mutation(&endpoint, request).await,
            result => result,
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn forward_feature_activation(
        &self,
        endpoint: &str,
        command_id: bmux_cluster_plugin_api::cluster_types::CommandId,
        principal_id: String,
        issued_at_unix_ms: u64,
        expected_control_revision: u64,
        read_schema_floor: u16,
        write_schema_floor: u16,
        feature: String,
    ) -> Result<ControlResponse, ControlServiceError> {
        let mut remote = EndpointDispatchClient::new(self.caller.as_ref(), endpoint);
        bmux_cluster_plugin_api::cluster_control_command_v2::client::activate_feature(
            &mut remote,
            command_id,
            principal_id,
            issued_at_unix_ms,
            expected_control_revision,
            read_schema_floor,
            write_schema_floor,
            feature,
        )
        .await
        .map_err(|error| ControlServiceError::RuntimeUnavailable {
            reason: format!(
                "leader feature activation forwarding failed before a response was received; retry the same CommandId: {error}"
            ),
        })?
    }

    #[allow(clippy::too_many_arguments)]
    async fn activate_feature_or_forward(
        &self,
        command_id: bmux_cluster_plugin_api::cluster_types::CommandId,
        principal_id: String,
        issued_at_unix_ms: u64,
        expected_control_revision: u64,
        read_schema_floor: u16,
        write_schema_floor: u16,
        feature: String,
    ) -> Result<ControlResponse, ControlServiceError> {
        let node = self.active()?;
        let state = match node.linearizable_control_state().await {
            Ok(state) => state,
            Err(crate::consensus_runtime::ConsensusReadError::NotLeader(not_leader)) => {
                let endpoint =
                    not_leader
                        .leader_endpoint
                        .ok_or_else(|| ControlServiceError::NotLeader {
                            leader_node_id: not_leader.leader_id.map(|id| id.to_string()),
                            leader_endpoint: None,
                        })?;
                return self
                    .forward_feature_activation(
                        &endpoint,
                        command_id,
                        principal_id,
                        issued_at_unix_ms,
                        expected_control_revision,
                        read_schema_floor,
                        write_schema_floor,
                        feature,
                    )
                    .await;
            }
            Err(error) => return Err(control_read_service_error(error)),
        };
        let command = crate::control_codec::FeatureActivationCommand {
            principal_id,
            command_id,
            issued_at_unix_ms,
            expected_control_revision,
            read_schema_floor,
            write_schema_floor,
            feature,
        };
        // A quorum-confirmed outcome predates current revision and membership.
        // Dispatch it through apply again to preserve byte-exact conflict checks.
        if state.has_feature_activation_outcome(&command) {
            return node.activate_feature(command).await;
        }
        validate_feature_activation_membership(
            &node,
            &state,
            expected_control_revision,
            read_schema_floor,
            write_schema_floor,
            &command.feature,
        )?;
        node.activate_feature(command).await
    }

    async fn forward_linearizable_read(
        &self,
        endpoint: &str,
    ) -> Result<ControlStateView, ControlServiceError> {
        let fail = |reason: String| ControlServiceError::RuntimeUnavailable { reason };
        let identity = crate::membership::load_node_identity(self.caller.as_ref())
            .map_err(fail)?
            .ok_or_else(|| fail("local node identity unavailable".into()))?;
        let local = self.active()?.read_stale_view()?;
        let leader = local
            .members
            .iter()
            .find(|member| {
                member.endpoint.as_deref() == Some(endpoint)
                    && member.state
                        == bmux_cluster_plugin_api::cluster_types::ClusterMemberState::Active
            })
            .ok_or_else(|| fail("read leader endpoint is not a committed active member".into()))?;
        let proof = crate::endpoint::peer_authentication_proof(
            self.caller.as_ref(),
            endpoint,
            &self.local_node_id.to_string(),
            &leader.node_id,
        )
        .await
        .map_err(|error| fail(error.to_string()))?;
        let request = authenticated_request(
            PEER_READ_OPERATION,
            leader.node_id.parse().map_err(fail)?,
            &identity,
            proof,
            encode_service_message(&()).map_err(|error| fail(error.to_string()))?,
        )
        .map_err(fail)?;
        let mut remote = EndpointDispatchClient::new(self.caller.as_ref(), endpoint);
        bmux_cluster_plugin_api::cluster_peer_read::client::read_linearizable(&mut remote, request)
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

impl<C>
    bmux_cluster_plugin_api::cluster_bounded_control_command::ClusterBoundedControlCommandService
    for ControlServiceHandle<C>
where
    C: ServiceCaller + Send + Sync + 'static,
{
    fn mutate<'a>(
        &'a self,
        request: bmux_cluster_plugin_api::cluster_types::ControlCommand,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<ControlResponse, ControlServiceError>> + Send + 'a>,
    > {
        Box::pin(async move {
            match self.active()?.mutate_bounded(request.clone()).await {
                Err(ControlServiceError::NotLeader {
                    leader_endpoint: Some(endpoint),
                    ..
                }) => {
                    let mut remote = EndpointDispatchClient::new(self.caller.as_ref(), &endpoint);
                    let protocol = bmux_cluster_plugin_api::cluster_bounded_control_command::client::forwarding_protocol(&mut remote)
                        .await.map_err(|error| ControlServiceError::RuntimeUnavailable {
                            reason: format!("bounded forwarding negotiation failed before mutation submission: {error}"),
                        })?;
                    if protocol != 1 {
                        return Err(ControlServiceError::RuntimeUnavailable {
                            reason: format!(
                                "unsupported bounded forwarding protocol {protocol}; mutation not submitted"
                            ),
                        });
                    }
                    bmux_cluster_plugin_api::cluster_bounded_control_command::client::mutate_local(
                        &mut remote, request,
                    ).await.map_err(|error| ControlServiceError::RuntimeUnavailable {
                        reason: format!("bounded leader forwarding failed before a response was received; retry the same CommandId: {error}"),
                    })?
                }
                result => result,
            }
        })
    }

    fn forwarding_protocol<'a>(
        &'a self,
    ) -> std::pin::Pin<Box<dyn Future<Output = u16> + Send + 'a>> {
        Box::pin(async { 1 })
    }

    fn mutate_local<'a>(
        &'a self,
        request: bmux_cluster_plugin_api::cluster_types::ControlCommand,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<ControlResponse, ControlServiceError>> + Send + 'a>,
    > {
        // A forwarded receiver never follows another hint, even if leadership
        // changed while the request was in flight.
        Box::pin(async move { self.active()?.mutate_bounded(request).await })
    }
}

fn control_read_service_error(
    error: crate::consensus_runtime::ConsensusReadError,
) -> ControlServiceError {
    match error {
        crate::consensus_runtime::ConsensusReadError::NotLeader(not_leader) => {
            ControlServiceError::NotLeader {
                leader_node_id: not_leader.leader_id.map(|id| id.to_string()),
                leader_endpoint: not_leader.leader_endpoint,
            }
        }
        crate::consensus_runtime::ConsensusReadError::QuorumUnavailable(reason) => {
            ControlServiceError::QuorumUnavailable { reason }
        }
        crate::consensus_runtime::ConsensusReadError::Fatal(error) => {
            ControlServiceError::Internal {
                reason: error.to_string(),
            }
        }
        crate::consensus_runtime::ConsensusReadError::Storage(error) => {
            ControlServiceError::Internal {
                reason: error.to_string(),
            }
        }
    }
}

fn validate_feature_activation_membership(
    node: &crate::consensus_runtime::ConsensusNode,
    state: &crate::control_state::ControlState,
    expected_control_revision: u64,
    read_schema_floor: u16,
    write_schema_floor: u16,
    feature: &str,
) -> Result<(), ControlServiceError> {
    if state.revision != expected_control_revision {
        return Err(ControlServiceError::Rejected {
            error: bmux_cluster_plugin_api::cluster_types::ControlCommandError::RevisionConflict {
                expected: expected_control_revision,
                current: state.revision,
            },
        });
    }
    let committed = node.committed_member_ids();
    let voters = node.committed_voter_ids();
    let active_members = state
        .members
        .values()
        .filter(|member| {
            member.state == bmux_cluster_plugin_api::cluster_types::ClusterMemberState::Active
        })
        .map(|member| member.node_id.parse::<NodeId>())
        .collect::<Result<std::collections::BTreeSet<_>, _>>()
        .map_err(|reason| ControlServiceError::Internal { reason })?;
    validate_feature_activation_member_sets(
        state,
        &committed,
        &voters,
        &active_members,
        expected_control_revision,
        read_schema_floor,
        write_schema_floor,
        feature,
        crate::now_unix_ms(),
    )
}

#[allow(clippy::too_many_arguments)]
fn validate_feature_activation_member_sets(
    state: &crate::control_state::ControlState,
    committed: &std::collections::BTreeSet<NodeId>,
    voters: &std::collections::BTreeSet<NodeId>,
    active_members: &std::collections::BTreeSet<NodeId>,
    expected_control_revision: u64,
    read_schema_floor: u16,
    write_schema_floor: u16,
    feature: &str,
    now_unix_ms: u64,
) -> Result<(), ControlServiceError> {
    if state.revision != expected_control_revision {
        return Err(ControlServiceError::Rejected {
            error: bmux_cluster_plugin_api::cluster_types::ControlCommandError::RevisionConflict {
                expected: expected_control_revision,
                current: state.revision,
            },
        });
    }
    if committed.is_empty()
        || voters.is_empty()
        || !voters.is_subset(committed)
        || !committed.is_subset(active_members)
        || !active_members.is_subset(committed)
    {
        return Err(ControlServiceError::Internal {
            reason: "committed consensus membership is empty".to_string(),
        });
    }
    for node_id in committed {
        let member = state
            .members
            .get(&node_id.to_string())
            .filter(|member| {
                member.state
                    == bmux_cluster_plugin_api::cluster_types::ClusterMemberState::Active
            })
            .ok_or_else(|| ControlServiceError::Internal {
                reason: format!(
                    "committed consensus member {node_id} has no active replicated membership record"
                ),
            })?;
        crate::membership::verify_membership_credential(member, now_unix_ms).map_err(|reason| {
            ControlServiceError::Internal {
                reason: format!("committed consensus member {node_id} is untrusted: {reason}"),
            }
        })?;
        if member.negotiated_protocol.schema_version < u32::from(write_schema_floor)
            || !member
                .negotiated_protocol
                .features
                .iter()
                .any(|supported| supported == feature)
        {
            return Err(ControlServiceError::Rejected {
                error:
                    bmux_cluster_plugin_api::cluster_types::ControlCommandError::InvalidTransition {
                        reason: format!(
                            "committed consensus member {node_id} does not support schema {write_schema_floor} and feature '{feature}'"
                        ),
                    },
            });
        }
    }
    if read_schema_floor > write_schema_floor {
        return Err(ControlServiceError::Rejected {
            error: bmux_cluster_plugin_api::cluster_types::ControlCommandError::InvalidTransition {
                reason: "read schema floor exceeds write schema floor".to_string(),
            },
        });
    }
    Ok(())
}

impl<C> bmux_cluster_plugin_api::cluster_capability_publication::ClusterCapabilityPublicationService
    for ControlServiceHandle<C>
where
    C: ServiceCaller + Send + Sync + 'static,
{
    fn submit<'a>(
        &'a self,
        reports: Vec<bmux_cluster_plugin_api::cluster_capability_types::CapabilityReport>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<ControlResponse, ControlServiceError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move { self.active()?.publish_capabilities(reports).await })
    }
}

impl<C> bmux_cluster_plugin_api::cluster_protocol_refresh::ClusterProtocolRefreshService
    for ControlServiceHandle<C>
where
    C: ServiceCaller + Send + Sync + 'static,
{
    fn submit<'a>(
        &'a self,
        command_id: bmux_cluster_plugin_api::cluster_types::CommandId,
        replacement: bmux_cluster_plugin_api::cluster_types::ClusterMember,
        expected_serial: String,
        expected_revision: u64,
        node_signature: Vec<u8>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<ControlResponse, ControlServiceError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            self.active()?
                .refresh_member_protocol(
                    command_id,
                    replacement,
                    expected_serial,
                    expected_revision,
                    node_signature,
                )
                .await
        })
    }
}

impl<C> bmux_cluster_plugin_api::cluster_principal_bootstrap::ClusterPrincipalBootstrapService
    for ControlServiceHandle<C>
where
    C: ServiceCaller + Send + Sync + 'static,
{
    fn submit<'a>(
        &'a self,
        proof: bmux_cluster_plugin_api::cluster_principal_bootstrap_types::BootstrapProof,
        expected_control_revision: u64,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<ControlResponse, ControlServiceError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            // Return typed leader guidance; callers negotiate this interface at
            // the authority, rather than forwarding a caller-selected timestamp.
            self.active()?
                .bootstrap_principal(proof, expected_control_revision)
                .await
        })
    }
}

impl<C> bmux_cluster_plugin_api::cluster_control_command_v2::ClusterControlCommandService
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

    fn activate_feature<'a>(
        &'a self,
        command_id: bmux_cluster_plugin_api::cluster_types::CommandId,
        principal_id: String,
        issued_at_unix_ms: u64,
        expected_control_revision: u64,
        read_schema_floor: u16,
        write_schema_floor: u16,
        feature: String,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<ControlResponse, ControlServiceError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            self.activate_feature_or_forward(
                command_id,
                principal_id,
                issued_at_unix_ms,
                expected_control_revision,
                read_schema_floor,
                write_schema_floor,
                feature,
            )
            .await
        })
    }
}

impl<C> bmux_cluster_plugin_api::cluster_control_state_v2::ClusterControlStateService
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

    fn feature_floor<'a>(
        &'a self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        bmux_cluster_plugin_api::cluster_types::FeatureFloorState,
                        ControlServiceError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let state = self
                .active()?
                .linearizable_control_state()
                .await
                .map_err(control_read_service_error)?;
            Ok(bmux_cluster_plugin_api::cluster_types::FeatureFloorState {
                control_revision: state.revision,
                read_schema_floor: state.read_schema_floor,
                write_schema_floor: state.write_schema_floor,
                activated_features: state.activated_features.into_iter().collect(),
            })
        })
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

impl<C> bmux_cluster_plugin_api::cluster_raft_decoder::ClusterRaftDecoderService
    for RaftRpcServiceHandle<C>
where
    C: crate::ClusterRuntimeOps + Send + Sync + 'static,
{
    fn probe<'a>(
        &'a self,
        request: RaftRpcRequest,
    ) -> std::pin::Pin<
        Box<
            dyn Future<
                    Output = Result<
                        bmux_cluster_plugin_api::cluster_types::RaftDecoderProof,
                        String,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            // Authenticate before attesting; the request signature and fresh challenge
            // bind this proof to one requester and target, not an endpoint cache.
            let signed = decoder_proof_payload(&request)?;
            let (_, payload) = authenticate_request(
                self.caller.as_ref(),
                DECODER_PROBE_OPERATION,
                self.local_node_id,
                request,
            )?;
            if !payload.is_empty() {
                return Err("decoder probe payload must be empty".into());
            }
            let identity = crate::load_or_create_node_identity(self.caller.as_ref())?;
            if *identity.node_id() != self.local_node_id {
                return Err("decoder signer does not match service identity".into());
            }
            Ok(bmux_cluster_plugin_api::cluster_types::RaftDecoderProof {
                signature: identity.sign(&signed),
            })
        })
    }
}

impl<C> bmux_cluster_plugin_api::cluster_raft_rpc_v2::ClusterRaftRpcService
    for RaftRpcServiceHandle<C>
where
    C: crate::ClusterRuntimeOps + Send + Sync + 'static,
{
    fn append_entries<'a>(
        &'a self,
        request: RaftRpcRequest,
    ) -> std::pin::Pin<Box<dyn Future<Output = RaftRpcResponse> + Send + 'a>> {
        Box::pin(
            self.dispatch("append_entries_v2", request, |raft, request| {
                Box::pin(async move {
                    raft.append_entries(request)
                        .await
                        .map_err(|error| error.to_string())
                })
            }),
        )
    }
    fn install_snapshot<'a>(
        &'a self,
        request: RaftRpcRequest,
    ) -> std::pin::Pin<Box<dyn Future<Output = RaftRpcResponse> + Send + 'a>> {
        Box::pin(
            self.dispatch("install_snapshot_v2", request, |raft, request| {
                Box::pin(async move {
                    raft.install_snapshot(request)
                        .await
                        .map_err(|error| error.to_string())
                })
            }),
        )
    }
    fn vote<'a>(
        &'a self,
        request: RaftRpcRequest,
    ) -> std::pin::Pin<Box<dyn Future<Output = RaftRpcResponse> + Send + 'a>> {
        Box::pin(self.dispatch("vote_v2", request, |raft, request| {
            Box::pin(async move { raft.vote(request).await.map_err(|error| error.to_string()) })
        }))
    }
}

/// Unregistered owned append provider. Construction does not advertise capability
/// or initialize another authority; the lifecycle owner supplies its executor.
pub struct OwnedAppendService<C> {
    pub caller: Arc<C>,
    pub local: NodeId,
    pub executor: Arc<crate::owned_vote_store::VoteExecutor>,
}

impl<C: crate::ClusterRuntimeOps + Send + Sync + 'static> OwnedAppendService<C> {
    /// Register generated dispatch against an executor supplied by its lifecycle owner.
    ///
    /// This neither opens storage nor advertises negotiation support. The owner
    /// must stop admission and shut down the same executor when removing service.
    /// # Errors
    /// Returns generated descriptor registration failures.
    pub fn register(
        registry: &mut bmux_plugin_sdk::TypedServiceRegistry,
        caller: Arc<C>,
        local: NodeId,
        executor: Arc<crate::owned_vote_store::VoteExecutor>,
    ) -> Result<(), Box<bmux_plugin_sdk::PluginError>> {
        bmux_cluster_plugin_api::cluster_consensus_rpc::register_provider(
            registry,
            Arc::new(Self {
                caller,
                local,
                executor,
            }),
        )
        .map_err(Box::new)
    }
}

impl<C: crate::ClusterRuntimeOps + Send + Sync + 'static>
    bmux_cluster_plugin_api::cluster_consensus_rpc::ClusterConsensusRpcService
    for OwnedAppendService<C>
{
    fn vote<'a>(
        &'a self,
        request: bmux_cluster_plugin_api::cluster_consensus_vote_types::VoteRequest,
    ) -> std::pin::Pin<
        Box<
            dyn Future<
                    Output = Result<
                        bmux_cluster_plugin_api::cluster_consensus_vote_types::VoteResponse,
                        bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            use bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError;
            owned_vote_signing_payload(&request).map_err(|_| RpcError::InvalidRequest)?;
            let identity = crate::load_or_create_node_identity(self.caller.as_ref())
                .map_err(|_| RpcError::Unauthorized)?;
            if *identity.node_id() != self.local {
                return Err(RpcError::Unauthorized);
            }
            let decision = self
                .executor
                .vote_authenticated(self.caller.clone(), self.local, request.clone())
                .await?;
            let mut response =
                bmux_cluster_plugin_api::cluster_consensus_vote_types::VoteResponse {
                    request_id: request.request_id,
                    source: self.local.as_bytes().to_vec(),
                    term: decision.term.0,
                    granted: decision.granted,
                    signature: Vec::new(),
                };
            response.signature = identity.sign(
                &owned_vote_response_signing_payload(&request, &response)
                    .map_err(|_| RpcError::InvalidRequest)?,
            );
            Ok(response)
        })
    }

    fn append<'a>(
        &'a self,
        request: bmux_cluster_plugin_api::cluster_consensus_rpc_types::AppendRequest,
    ) -> std::pin::Pin<
        Box<
            dyn Future<
                    Output = Result<
                        bmux_cluster_plugin_api::cluster_consensus_rpc_types::AppendResponse,
                        bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            use crate::consensus_model::{AppendOutcome as DecisionOutcome, AppendReconciliation};
            use bmux_cluster_plugin_api::cluster_consensus_rpc_types::{
                AppendOutcome, AppendResponse, RpcError,
            };
            let request_id = request.request_id;
            let next_index = request
                .previous
                .as_ref()
                .map_or(0, |position| position.index);
            let result = self
                .executor
                .append_authenticated(self.caller.clone(), self.local, request)
                .await;
            let result = match result {
                Err(crate::owned_vote_store::AuthenticatedAppendError::MissingTransfer {
                    reference,
                    term,
                }) => {
                    return Ok(AppendResponse {
                        request_id,
                        source: self.local.as_bytes().to_vec(),
                        term: term.0,
                        outcome: AppendOutcome::MissingTransfer {
                            reference: *reference,
                        },
                    });
                }
                other => other,
            };
            let decision = result.map_err(|error| match error {
                crate::owned_vote_store::AuthenticatedAppendError::Unauthorized => {
                    RpcError::Unauthorized
                }
                crate::owned_vote_store::AuthenticatedAppendError::Invalid(_) => {
                    RpcError::InvalidRequest
                }
                crate::owned_vote_store::AuthenticatedAppendError::Storage(_) => {
                    RpcError::StorageFailure
                }
                crate::owned_vote_store::AuthenticatedAppendError::Executor(
                    crate::consensus_model::VoteGateError::Stopped,
                ) => RpcError::Stopped,
                crate::owned_vote_store::AuthenticatedAppendError::TermMismatch => {
                    RpcError::StaleTerm
                }
                crate::owned_vote_store::AuthenticatedAppendError::MissingTransfer { .. }
                | crate::owned_vote_store::AuthenticatedAppendError::Executor(_) => {
                    RpcError::ExecutionFailed
                }
            })?;
            let outcome = match decision.outcome {
                Ok(DecisionOutcome::StaleTerm) => return Err(RpcError::StaleTerm),
                Ok(DecisionOutcome::Reconciled(AppendReconciliation::PreviousMismatch)) => {
                    AppendOutcome::Conflict { next_index }
                }
                Ok(DecisionOutcome::Reconciled(AppendReconciliation::Matched {
                    through, ..
                })) => AppendOutcome::Durable {
                    matched: through.map(|identity| {
                        bmux_cluster_plugin_api::cluster_consensus_transfer_types::LogIdentity {
                            term: identity.term.0,
                            leader: identity.leader.as_bytes().to_vec(),
                            index: identity.index.0,
                        }
                    }),
                },
                Err(_) => return Err(RpcError::ExecutionFailed),
            };
            Ok(AppendResponse {
                request_id,
                source: self.local.as_bytes().to_vec(),
                term: decision.term.0,
                outcome,
            })
        })
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
    use bmux_cluster_plugin_api::cluster_types::{
        ClusterConsensusRole, ClusterNodeCapabilities, ControlCommandError, PeerAuthChallenge,
        PeerAuthProof,
    };

    #[test]
    fn transfer_signing_rejects_oversized_proof_and_invalid_descriptor() {
        use bmux_cluster_plugin_api::cluster_consensus_transfer_types::{
            ChunkRequest, EntryPurpose, LogIdentity, TransferHeader, TransferProof, TransferPurpose,
        };
        let node = NodeIdentity::new_for_test(1);
        let mut request = ChunkRequest {
            header: TransferHeader {
                cluster_id: "cluster".into(),
                sender: node.node_id().as_bytes().to_vec(),
                recipient: node.node_id().as_bytes().to_vec(),
                leader_term: 1,
                sender_incarnation: uuid::Uuid::from_u128(1),
                sequence: 1,
                purpose: TransferPurpose::Entry {
                    value: EntryPurpose {
                        identity: LogIdentity {
                            term: 1,
                            leader: node.node_id().as_bytes().to_vec(),
                            index: 0,
                        },
                        previous: None,
                    },
                },
                total_bytes: 1,
                checksum: vec![0; 32],
            },
            offset: 0,
            data: vec![1],
            proof: TransferProof {
                credential_serial: "serial".into(),
                signature: vec![0; 64],
                peer: PeerAuthProof {
                    challenge: PeerAuthChallenge {
                        protocol_version: 1,
                        cluster_id: "cluster".into(),
                        verifier_node_id: node.node_id().to_string(),
                        verifier_credential_serial: "serial".into(),
                        audience_node_id: node.node_id().to_string(),
                        nonce: "nonce".into(),
                        issued_at_unix_ms: 0,
                        expires_at_unix_ms: 1,
                        signature: String::new(),
                    },
                    claimant_node_id: node.node_id().to_string(),
                    claimant_credential_serial: "serial".into(),
                    claimant_signature: String::new(),
                },
            },
        };
        let mut append = bmux_cluster_plugin_api::cluster_consensus_rpc_types::AppendRequest {
            cluster_id: request.header.cluster_id.clone(),
            sender: request.header.sender.clone(),
            recipient: request.header.recipient.clone(),
            request_id: uuid::Uuid::from_u128(2),
            term: 1,
            previous: None,
            entries: Vec::new(),
            leader_commit: None,
            proof: request.proof.clone(),
        };
        let append_signed = owned_append_signing_payload(&append).unwrap();
        append.proof.signature.fill(1);
        assert_eq!(
            append_signed,
            owned_append_signing_payload(&append).unwrap()
        );
        append.term = 2;
        assert_ne!(
            append_signed,
            owned_append_signing_payload(&append).unwrap()
        );
        append.proof.peer.claimant_node_id = "x".repeat(512 * 1024);
        assert!(owned_append_signing_payload(&append).is_err());
        let signed = transfer_signing_payload("chunk", &request).unwrap();
        request.proof.signature.fill(1);
        assert_eq!(signed, transfer_signing_payload("chunk", &request).unwrap());
        request.data[0] = 2;
        assert_ne!(signed, transfer_signing_payload("chunk", &request).unwrap());
        request.proof.peer.claimant_node_id = "x".repeat(512 * 1024);
        assert_eq!(
            transfer_signing_payload("chunk", &request).unwrap_err(),
            "transfer metadata exceeds allocation budget"
        );
        request.proof.peer.claimant_node_id = node.node_id().to_string();
        request.header.sender.pop();
        assert_eq!(
            transfer_signing_payload("chunk", &request).unwrap_err(),
            "invalid transfer node length"
        );
    }

    fn activation_member(
        issuer: &NodeIdentity,
        cluster_id: crate::membership::ClusterId,
        node: &NodeIdentity,
        now: u64,
    ) -> bmux_cluster_plugin_api::cluster_types::ClusterMember {
        crate::membership::issue_test_member(
            issuer,
            cluster_id,
            node,
            "memory://node",
            ClusterNodeCapabilities {
                consensus_role: ClusterConsensusRole::Voter,
                worker: true,
                ingress: true,
            },
            now,
        )
    }

    #[test]
    fn feature_activation_requires_every_committed_member_support() {
        let now = crate::now_unix_ms();
        let issuer = NodeIdentity::new_for_test(1);
        let cluster_id = crate::membership::ClusterId::generate();
        let first = NodeIdentity::new_for_test(2);
        let second = NodeIdentity::new_for_test(3);
        let first_member = activation_member(&issuer, cluster_id, &first, now);
        let mut second_member = activation_member(&issuer, cluster_id, &second, now);
        let mut state = crate::control_state::ControlState::new(cluster_id.to_string());
        state.revision = 8;
        state
            .members
            .insert(first_member.node_id.clone(), first_member);
        state
            .members
            .insert(second_member.node_id.clone(), second_member.clone());
        let committed = std::collections::BTreeSet::from([*first.node_id(), *second.node_id()]);
        let voters = std::collections::BTreeSet::from([*first.node_id()]);
        validate_feature_activation_member_sets(
            &state,
            &committed,
            &voters,
            &committed,
            8,
            2,
            2,
            crate::membership::ATOMIC_LAYOUT_FEATURE,
            now,
        )
        .unwrap();

        second_member.negotiated_protocol.schema_version = 1;
        second_member.negotiated_protocol.features.clear();
        second_member = crate::membership::issue_test_member_with_protocol(
            &issuer,
            cluster_id,
            &second,
            "memory://node",
            second_member.capabilities.clone(),
            second_member.negotiated_protocol.clone(),
            now,
        );
        state
            .members
            .insert(second_member.node_id.clone(), second_member);
        assert!(matches!(
            validate_feature_activation_member_sets(
                &state,
                &committed,
                &voters,
                &committed,
                8,
                2,
                2,
                crate::membership::ATOMIC_LAYOUT_FEATURE,
                now,
            ),
            Err(ControlServiceError::Rejected {
                error: ControlCommandError::InvalidTransition { .. }
            })
        ));
        assert!(matches!(
            validate_feature_activation_member_sets(
                &state,
                &committed,
                &voters,
                &committed,
                7,
                2,
                2,
                crate::membership::ATOMIC_LAYOUT_FEATURE,
                now,
            ),
            Err(ControlServiceError::Rejected {
                error: ControlCommandError::RevisionConflict { .. }
            })
        ));
    }

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
        let decoder_signed = decoder_proof_payload(&request).unwrap();
        let decoder_signature = identity.sign(&decoder_signed);
        identity
            .verify(&decoder_signed, &decoder_signature)
            .unwrap();
        let mut replay = request.clone();
        replay.proof.challenge.nonce.push('x');
        assert!(
            identity
                .verify(&decoder_proof_payload(&replay).unwrap(), &decoder_signature)
                .is_err()
        );
        replay = request.clone();
        replay.target_node_id = identity.node_id().to_string();
        assert!(
            identity
                .verify(&decoder_proof_payload(&replay).unwrap(), &decoder_signature)
                .is_err()
        );
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
