//! Authenticated peer transport helpers, control services and the local
//! consensus node registry.

use crate::consensus_runtime::ConsensusNode;
use crate::endpoint::EndpointDispatchClient;
use crate::membership::{NodeId, NodeIdentity, authenticate_peer_proof, verify_node_signature};
use bmux_cluster_plugin_api::cluster_types::{
    ControlResponse, ControlServiceError, ControlStateView, RaftRpcRequest,
};
use bmux_plugin::ServiceCaller;
use bmux_plugin_sdk::encode_service_message;
use std::future::Future;
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

/// Bind an owned append response to its signed request and intended responder.
/// # Errors
/// Rejects mismatched identities and oversized response fields before encoding.
pub fn owned_append_response_signing_payload(
    request: &bmux_cluster_plugin_api::cluster_consensus_rpc_types::AppendRequest,
    response: &bmux_cluster_plugin_api::cluster_consensus_rpc_types::AppendResponse,
) -> Result<Vec<u8>, String> {
    use bmux_cluster_plugin_api::cluster_consensus_rpc_types::AppendOutcome;
    let request_bytes = owned_append_signing_payload(request)?;
    if response.source != request.recipient {
        return Err("append responder mismatch".into());
    }
    if response.request_id != request.request_id || response.signature.len() > 64 {
        return Err("append response binding mismatch".into());
    }
    match &response.outcome {
        AppendOutcome::Durable { matched: Some(position) } => {
            transfer_node(&position.leader)?;
        }
        AppendOutcome::MissingTransfer { reference }
            if !request.entries.iter().any(|entry| matches!(entry,
                bmux_cluster_plugin_api::cluster_consensus_rpc_types::AppendEntry::Staged { reference: candidate }
                    if candidate == reference)) => {
                return Err("unknown missing transfer".into());
        }
        _ => {}
    }
    let mut unsigned = response.clone();
    unsigned.signature.clear();
    let mut signed = b"bmux.cluster.consensus-append.v1\0response\0".to_vec();
    signed.extend(
        encode_service_message(&(request_bytes, &request.proof.signature, unsigned))
            .map_err(|error| error.to_string())?,
    );
    Ok(signed)
}

/// Verify an append response against the exact locally retained signed request.
/// This proves its origin and binding, not current leadership or quorum commit.
/// # Errors
/// Invalid signatures and request/responder mismatches reject the response.
pub fn verify_owned_append_response(
    request: &bmux_cluster_plugin_api::cluster_consensus_rpc_types::AppendRequest,
    response: &bmux_cluster_plugin_api::cluster_consensus_rpc_types::AppendResponse,
) -> Result<(), String> {
    if response.signature.len() != 64 {
        return Err("invalid append response signature length".into());
    }
    let payload = owned_append_response_signing_payload(request, response)?;
    let peer = transfer_node(&request.recipient)?;
    verify_node_signature(&peer.to_string(), &payload, &response.signature)
}

/// Canonical append-only V2 request signature input. V1 signatures are not reusable.
/// # Errors
/// Rejects the same malformed or oversized request fields as V1.
pub fn owned_append_v2_signing_payload(
    request: &bmux_cluster_plugin_api::cluster_consensus_rpc_types::AppendRequest,
) -> Result<Vec<u8>, String> {
    // Reuse representation validation, never the V1 signature domain.
    if request.proof.signature.len() > 64 {
        return Err("invalid append V2 request signature length".into());
    }
    let _ = owned_append_signing_payload(request)?;
    let mut unsigned = request.clone();
    unsigned.proof.signature.clear();
    let mut signed = b"bmux.cluster.consensus-append.v2\0request\0".to_vec();
    signed.extend(encode_service_message(&unsigned).map_err(|error| error.to_string())?);
    Ok(signed)
}

/// Bind the receiver's current durable term and outcome to the exact signed V2 request.
///
/// The executor must authenticate/admit the request and supply durable state before
/// signing; this helper neither reads state nor authorizes a response.
/// # Errors
/// Rejects mismatched bindings and impossible stale-term/progress combinations.
pub fn owned_append_v2_response_signing_payload(
    request: &bmux_cluster_plugin_api::cluster_consensus_rpc_types::AppendRequest,
    response: &bmux_cluster_plugin_api::cluster_consensus_rpc_v2_types::AppendResponse,
) -> Result<Vec<u8>, String> {
    use bmux_cluster_plugin_api::cluster_consensus_rpc_v2_types::AppendOutcome;
    let request_bytes = owned_append_v2_signing_payload(request)?;
    // Bind the reply origin and correlation identity to the issued request.
    if (&response.source, response.request_id) != (&request.recipient, request.request_id)
        || response.signature.len() > 64
        || request.proof.signature.len() != 64
    {
        return Err("append V2 response binding mismatch".into());
    }
    match &response.outcome {
        AppendOutcome::StaleTerm if response.term <= request.term => {
            return Err("append V2 stale rejection requires a higher term".into());
        }
        AppendOutcome::Accepted { value } => {
            if response.term != request.term {
                return Err("append V2 progress requires the request term".into());
            }
            // Apply existing outcome/reference validation without signing V1 bytes.
            let _ = owned_append_response_signing_payload(
                request,
                &bmux_cluster_plugin_api::cluster_consensus_rpc_types::AppendResponse {
                    request_id: response.request_id,
                    source: response.source.clone(),
                    term: response.term,
                    outcome: value.clone(),
                    signature: Vec::new(),
                },
            )?;
        }
        AppendOutcome::StaleTerm => {}
    }
    let mut unsigned = response.clone();
    unsigned.signature.clear();
    let mut signed = b"bmux.cluster.consensus-append.v2\0response\0".to_vec();
    signed.extend(
        encode_service_message(&(request_bytes, &request.proof.signature, unsigned))
            .map_err(|error| error.to_string())?,
    );
    Ok(signed)
}

/// Verify against the exact locally retained issued V2 request before observing term.
///
/// This proves request-bound origin only, not current peer eligibility, leadership,
/// durable local term settlement, or quorum progress. Those belong to the executor.
/// Bare RPC errors are deliberately not accepted as term evidence.
/// # Errors
/// Rejects untrusted, malformed, cross-version or incorrectly bound responses.
pub fn verify_owned_append_v2_response(
    request: &bmux_cluster_plugin_api::cluster_consensus_rpc_types::AppendRequest,
    response: &bmux_cluster_plugin_api::cluster_consensus_rpc_v2_types::AppendResponse,
) -> Result<(), String> {
    if response.signature.len() != 64 || request.proof.signature.len() != 64 {
        return Err("invalid append V2 signature length".into());
    }
    let payload = owned_append_v2_response_signing_payload(request, response)?;
    let peer = transfer_node(&request.recipient)?;
    verify_node_signature(&peer.to_string(), &payload, &response.signature)
}

/// Explicit append V2 selection in addition to the unchanged owned V1 bundle.
///
/// Call only with an authoritatively authenticated negotiated member record.
/// False means incompatible for V2, never permission to retry the request as V1.
#[must_use]
pub fn supports_owned_append_v2(
    member: &bmux_cluster_plugin_api::cluster_types::ClusterMember,
) -> bool {
    supports_owned_consensus(member)
        && member.negotiated_protocol.features.iter().any(|feature| {
            feature == bmux_cluster_plugin_api::cluster_consensus_rpc_v2::INTERFACE_ID.as_str()
        })
}

/// Authenticate V2 append using members supplied by the serialized lifecycle owner.
/// Eligibility, term decisions and durable mutation still belong to that owner.
/// # Errors
/// Rejects missing explicit V2 selection, invalid signatures and revoked/replayed proofs.
pub fn authenticate_owned_append_v2(
    caller: &impl crate::ClusterRuntimeOps,
    local: NodeId,
    members: &[bmux_cluster_plugin_api::cluster_types::ClusterMember],
    request: &bmux_cluster_plugin_api::cluster_consensus_rpc_types::AppendRequest,
) -> Result<NodeId, String> {
    let signed = owned_append_v2_signing_payload(request)?;
    let source = request.proof.peer.claimant_node_id.parse::<NodeId>()?;
    if !members
        .iter()
        .any(|member| member.node_id == source.to_string() && supports_owned_append_v2(member))
    {
        return Err("peer has not negotiated owned append V2".into());
    }
    if request.recipient.as_slice() != local.as_bytes()
        || request.sender.as_slice() != source.as_bytes()
        || request.cluster_id != request.proof.peer.challenge.cluster_id
        || request.proof.credential_serial != request.proof.peer.claimant_credential_serial
        || request.proof.signature.len() != 64
    {
        return Err("invalid append V2 identity or proof binding".into());
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

/// Whether a durable member negotiated the owned consensus contracts this
/// runtime serves: `cluster-consensus-rpc` v1 (vote) and v2 (append).
/// Capability advertisement alone is not admission to this engine.
// TODO(ADR-0015 snapshot transfer): staged transfer and snapshot installation
// are negotiated separately once their providers are served.
pub(crate) fn supports_owned_consensus(
    member: &bmux_cluster_plugin_api::cluster_types::ClusterMember,
) -> bool {
    [
        bmux_cluster_plugin_api::cluster_consensus_rpc::INTERFACE_ID,
        bmux_cluster_plugin_api::cluster_consensus_rpc_v2::INTERFACE_ID,
    ]
    .iter()
    .all(|interface| {
        member
            .negotiated_protocol
            .features
            .iter()
            .any(|feature| feature == interface.as_str())
    })
}

/// Snapshot installation requires its own explicit selection, in addition to
/// the owned RPC selection and staged transfer. No legacy fallback is permitted.
pub(crate) fn supports_owned_snapshot_install(
    member: &bmux_cluster_plugin_api::cluster_types::ClusterMember,
) -> bool {
    supports_owned_consensus(member)
        && [
            bmux_cluster_plugin_api::cluster_consensus_transfer::INTERFACE_ID,
            bmux_cluster_plugin_api::cluster_consensus_snapshot::INTERFACE_ID,
        ]
        .iter()
        .all(|interface| {
            member
                .negotiated_protocol
                .features
                .iter()
                .any(|feature| feature == interface.as_str())
        })
}

/// Bounded operation-specific input; this is not a transfer finish signature.
/// The complete immutable reference is signed, including both checksum domains.
pub(crate) fn owned_snapshot_install_signing_payload(
    request: &bmux_cluster_plugin_api::cluster_consensus_snapshot_types::InstallRequest,
) -> Result<Vec<u8>, String> {
    use bmux_cluster_plugin_api::cluster_consensus_transfer_types::TransferPurpose;
    let header = &request.reference;
    validate_transfer_metadata_allocation(header, &request.proof, &[])?;
    validate_transfer_descriptor(header)?;
    let TransferPurpose::Snapshot { value } = &header.purpose else {
        return Err("install requires snapshot-purpose transfer".into());
    };
    if value.last_included.is_none()
        || value.membership.is_none()
        || header.total_bytes > 300 * 1024 * 1024
        || request.proof.signature.len() > 64
    {
        return Err("invalid install snapshot reference".into());
    }
    let mut unsigned = request.clone();
    unsigned.proof.signature.clear();
    let encoded = encode_service_message(&unsigned).map_err(|error| error.to_string())?;
    if encoded.len() > 1024 * 1024 - 1024 {
        return Err("install envelope exceeds limit".into());
    }
    let mut signed = b"bmux.cluster.consensus-snapshot.v1\0install\0".to_vec();
    signed.extend(encoded);
    Ok(signed)
}

/// Authenticate using canonical members supplied by the owning executor.
/// This does not establish effective voter eligibility, term fencing, sealed
/// transfer ownership, or durable installation; the executor must check those
/// in the same authoritative admission as its side effect.
pub(crate) fn authenticate_owned_snapshot_install(
    caller: &impl crate::ClusterRuntimeOps,
    local: NodeId,
    members: &[bmux_cluster_plugin_api::cluster_types::ClusterMember],
    request: &bmux_cluster_plugin_api::cluster_consensus_snapshot_types::InstallRequest,
) -> Result<NodeId, String> {
    let signed = owned_snapshot_install_signing_payload(request)?;
    let source = request.proof.peer.claimant_node_id.parse::<NodeId>()?;
    if !members.iter().any(|member| {
        member.node_id == source.to_string() && supports_owned_snapshot_install(member)
    }) {
        return Err("peer has not negotiated owned snapshot installation".into());
    }
    let header = &request.reference;
    if header.recipient.as_slice() != local.as_bytes()
        || header.sender.as_slice() != source.as_bytes()
        || header.cluster_id != request.proof.peer.challenge.cluster_id
        || request.proof.credential_serial != request.proof.peer.claimant_credential_serial
        || request.proof.signature.len() != 64
    {
        return Err("invalid install identity or proof binding".into());
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

/// Response binding proves the exact request, origin and outcome, not a quorum.
pub(crate) fn owned_snapshot_install_response_signing_payload(
    request: &bmux_cluster_plugin_api::cluster_consensus_snapshot_types::InstallRequest,
    response: &bmux_cluster_plugin_api::cluster_consensus_snapshot_types::InstallResponse,
) -> Result<Vec<u8>, String> {
    use bmux_cluster_plugin_api::cluster_consensus_snapshot_types::InstallOutcome;
    use bmux_cluster_plugin_api::cluster_consensus_transfer_types::TransferPurpose;
    let request_bytes = owned_snapshot_install_signing_payload(request)?;
    if response.source != request.reference.recipient
        || response.request_id != request.request_id
        || response.signature.len() > 64
    {
        return Err("install response binding mismatch".into());
    }
    if let InstallOutcome::Installed { boundary } = &response.outcome {
        let TransferPurpose::Snapshot { value } = &request.reference.purpose else {
            return Err("install requires snapshot reference".into());
        };
        if value.last_included.as_ref() != Some(boundary)
            || response.term != request.reference.leader_term
        {
            return Err("install durable boundary or term mismatch".into());
        }
    }
    let mut unsigned = response.clone();
    unsigned.signature.clear();
    let mut signed = b"bmux.cluster.consensus-snapshot.v1\0response\0".to_vec();
    signed.extend(
        encode_service_message(&(request_bytes, &request.proof.signature, unsigned))
            .map_err(|error| error.to_string())?,
    );
    Ok(signed)
}

pub fn verify_owned_snapshot_install_response(
    request: &bmux_cluster_plugin_api::cluster_consensus_snapshot_types::InstallRequest,
    response: &bmux_cluster_plugin_api::cluster_consensus_snapshot_types::InstallResponse,
) -> Result<(), String> {
    if response.signature.len() != 64 {
        return Err("invalid install response signature length".into());
    }
    let payload = owned_snapshot_install_response_signing_payload(request, response)?;
    let peer = transfer_node(&request.reference.recipient)?;
    verify_node_signature(&peer.to_string(), &payload, &response.signature)
}

/// Send a signed install request over generated transport and accept only a
/// reply bound to that exact request and signed by its recipient. This is
/// transport evidence only; it never advances local progress.
pub(crate) async fn exchange_snapshot_install<C>(
    caller: &C,
    endpoint: &str,
    request: &bmux_cluster_plugin_api::cluster_consensus_snapshot_types::InstallRequest,
) -> Result<
    bmux_cluster_plugin_api::cluster_consensus_snapshot_types::InstallResponse,
    bmux_cluster_plugin_api::cluster_consensus_snapshot_types::InstallError,
>
where
    C: bmux_plugin::ServiceCaller + Sync,
{
    use bmux_cluster_plugin_api::cluster_consensus_snapshot_types::InstallError;
    let mut remote = crate::endpoint::EndpointDispatchClient::new(caller, endpoint);
    let response = switchy_async::time::timeout(
        std::time::Duration::from_secs(5),
        bmux_cluster_plugin_api::cluster_consensus_snapshot::client::install(
            &mut remote,
            request.clone(),
        ),
    )
    .await
    .map_err(|_| InstallError::ExecutionFailed)?
    .map_err(|_| InstallError::ExecutionFailed)??;
    verify_owned_snapshot_install_response(request, &response)
        .map_err(|_| InstallError::Unauthorized)?;
    Ok(response)
}

fn require_owned_consensus(
    members: &[bmux_cluster_plugin_api::cluster_types::ClusterMember],
    source: NodeId,
) -> Result<(), String> {
    if members
        .iter()
        .any(|member| member.node_id == source.to_string() && supports_owned_consensus(member))
    {
        Ok(())
    } else {
        Err("peer has not negotiated the owned consensus protocol".into())
    }
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
    require_owned_consensus(members, source)?;
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
    require_owned_consensus(members, source)?;
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
    validate_transfer_metadata_allocation(&request.header, &request.proof, &request.data)
}

fn validate_transfer_metadata_allocation(
    header: &bmux_cluster_plugin_api::cluster_consensus_transfer_types::TransferHeader,
    proof: &bmux_cluster_plugin_api::cluster_consensus_transfer_types::TransferProof,
    data: &[u8],
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
    let h = header;
    let p = proof;
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
    for bytes in [&h.sender, &h.recipient, &h.checksum, &p.signature, data] {
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

/// Registered `cluster-consensus-rpc` v1/v2 provider. Typed services register
/// before the consensus node starts, so each call resolves the running local
/// node and delegates to its authenticated wire service.
pub struct ConsensusRpcHandle {
    local_node_id: NodeId,
    nodes: ConsensusNodeRegistry,
}

impl ConsensusRpcHandle {
    #[must_use]
    pub const fn new(local_node_id: NodeId, nodes: ConsensusNodeRegistry) -> Self {
        Self {
            local_node_id,
            nodes,
        }
    }

    fn providers(
        &self,
    ) -> Result<
        (
            Arc<
                dyn bmux_cluster_plugin_api::cluster_consensus_rpc::ClusterConsensusRpcService
                    + Send
                    + Sync,
            >,
            Arc<
                dyn bmux_cluster_plugin_api::cluster_consensus_rpc_v2::ClusterConsensusRpcService
                    + Send
                    + Sync,
            >,
        ),
        bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError,
    > {
        self.nodes
            .get(self.local_node_id)
            .and_then(|node| node.rpc())
            .map_err(|_| bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError::Stopped)
    }
}

type ConsensusRpc<'a, T> = std::pin::Pin<
    Box<
        dyn Future<
                Output = Result<T, bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError>,
            > + Send
            + 'a,
    >,
>;

impl bmux_cluster_plugin_api::cluster_consensus_rpc::ClusterConsensusRpcService
    for ConsensusRpcHandle
{
    fn vote(
        &self,
        request: bmux_cluster_plugin_api::cluster_consensus_vote_types::VoteRequest,
    ) -> ConsensusRpc<'_, bmux_cluster_plugin_api::cluster_consensus_vote_types::VoteResponse> {
        Box::pin(async move {
            let (v1, _) = self.providers()?;
            v1.vote(request).await
        })
    }

    fn append(
        &self,
        request: bmux_cluster_plugin_api::cluster_consensus_rpc_types::AppendRequest,
    ) -> ConsensusRpc<'_, bmux_cluster_plugin_api::cluster_consensus_rpc_types::AppendResponse>
    {
        Box::pin(async move {
            let (v1, _) = self.providers()?;
            v1.append(request).await
        })
    }
}

impl bmux_cluster_plugin_api::cluster_consensus_rpc_v2::ClusterConsensusRpcService
    for ConsensusRpcHandle
{
    fn append(
        &self,
        request: bmux_cluster_plugin_api::cluster_consensus_rpc_types::AppendRequest,
    ) -> ConsensusRpc<'_, bmux_cluster_plugin_api::cluster_consensus_rpc_v2_types::AppendResponse>
    {
        Box::pin(async move {
            let (_, v2) = self.providers()?;
            v2.append(request).await
        })
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

    #[derive(Default)]
    struct SnapshotAuthCaller {
        storage: std::sync::Mutex<std::collections::BTreeMap<String, Vec<u8>>>,
        scope: crate::durable_storage::TestScope,
    }

    impl bmux_plugin::ServiceCaller for SnapshotAuthCaller {
        fn host_connection(&self) -> Option<&bmux_plugin_sdk::HostConnectionInfo> {
            Some(&self.scope.connection)
        }

        fn call_service_raw(
            &self,
            _capability: &str,
            _kind: bmux_plugin_sdk::ServiceKind,
            interface: &str,
            operation: &str,
            payload: Vec<u8>,
        ) -> bmux_plugin_sdk::Result<Vec<u8>> {
            match (interface, operation) {
                ("storage-query/v1", "get") => {
                    let request: bmux_plugin_sdk::StorageGetRequest =
                        decode_service_message(&payload)?;
                    encode_service_message(&bmux_plugin_sdk::StorageGetResponse {
                        value: self
                            .storage
                            .lock()
                            .unwrap()
                            .get(request.key.as_str())
                            .cloned(),
                    })
                }
                ("storage-command/v1", "set") => {
                    let request: bmux_plugin_sdk::StorageSetRequest =
                        decode_service_message(&payload)?;
                    self.storage
                        .lock()
                        .unwrap()
                        .insert(request.key.as_str().into(), request.value);
                    encode_service_message(&())
                }
                _ => Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
                    operation: "snapshot auth test",
                }),
            }
        }
        fn execute_kernel_request(
            &self,
            _request: bmux_ipc::Request,
        ) -> bmux_plugin_sdk::Result<bmux_ipc::ResponsePayload> {
            Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
                operation: "snapshot auth test kernel",
            })
        }
    }

    fn authenticated_snapshot_install_fixture() -> (
        SnapshotAuthCaller,
        NodeIdentity,
        Vec<bmux_cluster_plugin_api::cluster_types::ClusterMember>,
        bmux_cluster_plugin_api::cluster_consensus_snapshot_types::InstallRequest,
    ) {
        let caller = SnapshotAuthCaller::default();
        let identity = crate::membership::load_or_create_node_identity(&caller).unwrap();
        crate::membership::initialize_cluster(&caller).unwrap();
        let mut members = crate::membership::list_members(&caller).unwrap().members;
        let member = members
            .iter_mut()
            .find(|member| member.node_id == identity.node_id().to_string())
            .unwrap();
        member.negotiated_protocol.features = vec![
            bmux_cluster_plugin_api::cluster_consensus_rpc::INTERFACE_ID.to_string(),
            bmux_cluster_plugin_api::cluster_consensus_transfer::INTERFACE_ID.to_string(),
            bmux_cluster_plugin_api::cluster_consensus_snapshot::INTERFACE_ID.to_string(),
        ];
        *member = crate::membership::issue_membership_credential(
            &identity,
            member.cluster_id.parse().unwrap(),
            member.node_id.clone(),
            member.public_key.clone(),
            member.capabilities.clone(),
            member.negotiated_protocol.clone(),
            crate::now_unix_ms(),
        )
        .unwrap();
        let challenge = crate::membership::create_peer_auth_challenge_for_members(
            &caller,
            &crate::ClusterPeerChallengeRequest {
                claimant_node_id: identity.node_id().to_string(),
            },
            &members,
        )
        .unwrap();
        let mut proof = PeerAuthProof {
            challenge,
            claimant_node_id: identity.node_id().to_string(),
            claimant_credential_serial: members[0].credential_serial.clone(),
            claimant_signature: String::new(),
        };
        proof.claimant_signature = crate::membership::encode_hex(
            &identity.sign(&crate::membership::canonical_peer_proof(&proof).unwrap()),
        );
        let mut request = snapshot_install_request();
        request.reference.sender = identity.node_id().as_bytes().to_vec();
        request.reference.recipient = identity.node_id().as_bytes().to_vec();
        request
            .reference
            .cluster_id
            .clone_from(&members[0].cluster_id);
        let bmux_cluster_plugin_api::cluster_consensus_transfer_types::TransferPurpose::Snapshot {
            value,
        } = &mut request.reference.purpose
        else {
            unreachable!()
        };
        value.cluster_id.clone_from(&members[0].cluster_id);
        request.proof.peer = proof;
        request
            .proof
            .credential_serial
            .clone_from(&request.proof.peer.claimant_credential_serial);
        request.proof.signature =
            identity.sign(&owned_snapshot_install_signing_payload(&request).unwrap());
        (caller, identity, members, request)
    }

    #[test]
    fn snapshot_install_auth_rejects_wrong_origin_revocation_and_missing_selection() {
        let (caller, identity, members, request) = authenticated_snapshot_install_fixture();
        let mut unsupported = members.clone();
        unsupported[0].negotiated_protocol.features.pop();
        assert!(
            authenticate_owned_snapshot_install(
                &caller,
                *identity.node_id(),
                &unsupported,
                &request
            )
            .is_err()
        );
        let mut revoked = members.clone();
        revoked[0].state = bmux_cluster_plugin_api::cluster_types::ClusterMemberState::Revoked;
        assert!(
            authenticate_owned_snapshot_install(&caller, *identity.node_id(), &revoked, &request)
                .is_err()
        );
        let wrong_target = NodeIdentity::new_for_test(9);
        assert!(
            authenticate_owned_snapshot_install(
                &caller,
                *wrong_target.node_id(),
                &members,
                &request
            )
            .is_err()
        );
        let mut forged = request.clone();
        forged.proof.signature =
            wrong_target.sign(&owned_snapshot_install_signing_payload(&forged).unwrap());
        assert!(
            authenticate_owned_snapshot_install(&caller, *identity.node_id(), &members, &forged)
                .is_err()
        );
        assert_eq!(
            authenticate_owned_snapshot_install(&caller, *identity.node_id(), &members, &request)
                .unwrap(),
            *identity.node_id()
        );
        assert!(
            authenticate_owned_snapshot_install(&caller, *identity.node_id(), &members, &request)
                .is_err(),
            "consumed challenge is not reusable install authority"
        );
    }

    fn snapshot_install_request()
    -> bmux_cluster_plugin_api::cluster_consensus_snapshot_types::InstallRequest {
        use bmux_cluster_plugin_api::cluster_consensus_transfer_types::{
            LogIdentity, NodeAddress, SnapshotDescriptor, StoredMembership, TransferHeader,
            TransferProof, TransferPurpose, VoterConfiguration,
        };
        let source = NodeIdentity::new_for_test(1);
        let target = NodeIdentity::new_for_test(2);
        let position = LogIdentity {
            term: 3,
            leader: source.node_id().as_bytes().to_vec(),
            index: 10,
        };
        bmux_cluster_plugin_api::cluster_consensus_snapshot_types::InstallRequest {
            request_id: uuid::Uuid::from_u128(3),
            reference: TransferHeader {
                cluster_id: "cluster".into(),
                sender: source.node_id().as_bytes().to_vec(),
                recipient: target.node_id().as_bytes().to_vec(),
                leader_term: 3,
                sender_incarnation: uuid::Uuid::from_u128(1),
                sequence: 7,
                purpose: TransferPurpose::Snapshot {
                    value: SnapshotDescriptor {
                        format_version: 1,
                        cluster_id: "cluster".into(),
                        snapshot_id: "snapshot".into(),
                        last_included: Some(position.clone()),
                        total_bytes: 1024,
                        checksum: vec![8; 32],
                        membership: Some(StoredMembership {
                            introduced_at: position,
                            configurations: vec![VoterConfiguration {
                                voters: vec![source.node_id().as_bytes().to_vec()],
                            }],
                            nodes: vec![NodeAddress {
                                node: source.node_id().as_bytes().to_vec(),
                                endpoint: "memory://source".into(),
                            }],
                        }),
                    },
                },
                total_bytes: 1024,
                checksum: vec![9; 32],
            },
            proof: TransferProof {
                credential_serial: "serial".into(),
                signature: Vec::new(),
                peer: PeerAuthProof {
                    challenge: PeerAuthChallenge {
                        protocol_version: 1,
                        cluster_id: "cluster".into(),
                        verifier_node_id: target.node_id().to_string(),
                        verifier_credential_serial: "target-serial".into(),
                        audience_node_id: source.node_id().to_string(),
                        nonce: "nonce".into(),
                        issued_at_unix_ms: 1,
                        expires_at_unix_ms: 2,
                        signature: String::new(),
                    },
                    claimant_node_id: source.node_id().to_string(),
                    claimant_credential_serial: "serial".into(),
                    claimant_signature: String::new(),
                },
            },
        }
    }

    #[test]
    fn snapshot_install_signatures_bind_operation_reference_and_response() {
        use bmux_cluster_plugin_api::cluster_consensus_snapshot_types::{
            InstallOutcome, InstallRejection, InstallResponse,
        };
        use bmux_cluster_plugin_api::cluster_consensus_transfer_types::{
            ChunkRequest, TransferPurpose,
        };
        let source = NodeIdentity::new_for_test(1);
        let target = NodeIdentity::new_for_test(2);
        let mut request = snapshot_install_request();
        let encoded = encode_service_message(&request).unwrap();
        assert_eq!(
            decode_service_message::<
                bmux_cluster_plugin_api::cluster_consensus_snapshot_types::InstallRequest,
            >(&encoded)
            .unwrap(),
            request
        );
        let original = owned_snapshot_install_signing_payload(&request).unwrap();
        request.proof.signature = source.sign(&original);
        let transfer = ChunkRequest {
            header: request.reference.clone(),
            offset: 0,
            data: Vec::new(),
            proof: request.proof.clone(),
        };
        let finish_payload = transfer_signing_payload("finish", &transfer).unwrap();
        assert_ne!(original, finish_payload);
        assert!(
            verify_node_signature(
                &source.node_id().to_string(),
                &finish_payload,
                &request.proof.signature
            )
            .is_err(),
            "install signature must not authorize finish"
        );
        let TransferPurpose::Snapshot { value } = &request.reference.purpose else {
            unreachable!()
        };
        let mut response = InstallResponse {
            request_id: request.request_id,
            source: request.reference.recipient.clone(),
            term: 3,
            outcome: InstallOutcome::Installed {
                boundary: value.last_included.clone().unwrap(),
            },
            signature: Vec::new(),
        };
        response.signature = target
            .sign(&owned_snapshot_install_response_signing_payload(&request, &response).unwrap());
        assert!(verify_owned_snapshot_install_response(&request, &response).is_ok());
        for change in 0..6 {
            let mut changed = request.clone();
            match change {
                0 => changed.request_id = uuid::Uuid::from_u128(4),
                1 => changed.reference.sequence += 1,
                2 => changed.reference.sender_incarnation = uuid::Uuid::from_u128(4),
                3 => changed.reference.checksum[0] ^= 1,
                4 => changed.proof.signature[0] ^= 1,
                _ => {
                    let TransferPurpose::Snapshot { value } = &mut changed.reference.purpose else {
                        unreachable!()
                    };
                    value.checksum[0] ^= 1;
                }
            }
            assert!(
                verify_owned_snapshot_install_response(&changed, &response).is_err(),
                "mutation {change}"
            );
        }
        let mut changed = response.clone();
        changed.signature = source
            .sign(&owned_snapshot_install_response_signing_payload(&request, &changed).unwrap());
        assert!(
            verify_owned_snapshot_install_response(&request, &changed).is_err(),
            "wrong signer"
        );
        changed = response.clone();
        changed.source = request.reference.sender.clone();
        assert!(verify_owned_snapshot_install_response(&request, &changed).is_err());
        changed = response.clone();
        changed.term += 1;
        assert!(verify_owned_snapshot_install_response(&request, &changed).is_err());
        changed = response;
        changed.outcome = InstallOutcome::Rejected {
            reason: InstallRejection::UnknownTransfer,
        };
        assert!(
            verify_owned_snapshot_install_response(&request, &changed).is_err(),
            "outcome substitution"
        );
    }

    #[test]
    fn snapshot_install_reference_rejects_missing_boundary_and_oversize() {
        use bmux_cluster_plugin_api::cluster_consensus_transfer_types::TransferPurpose;
        for mode in 0..5 {
            let mut request = snapshot_install_request();
            let TransferPurpose::Snapshot { value } = &mut request.reference.purpose else {
                unreachable!()
            };
            match mode {
                0 => value.last_included = None,
                1 => value.membership = None,
                2 => {
                    value.total_bytes = 301 * 1024 * 1024;
                    request.reference.total_bytes = value.total_bytes;
                }
                3 => value.cluster_id = "foreign".into(),
                _ => value.membership.as_mut().unwrap().introduced_at.index = 11,
            }
            assert!(
                owned_snapshot_install_signing_payload(&request).is_err(),
                "invalid reference {mode}"
            );
        }
        let mut request = snapshot_install_request();
        request.reference.sender.pop();
        assert!(owned_snapshot_install_signing_payload(&request).is_err());
    }

    #[test]
    fn snapshot_install_bounds_and_explicit_selection() {
        use bmux_cluster_plugin_api::cluster_consensus_transfer_types::{
            EntryPurpose, TransferPurpose,
        };
        let mut request = snapshot_install_request();
        let TransferPurpose::Snapshot { value } = &request.reference.purpose else {
            unreachable!()
        };
        request.reference.purpose = TransferPurpose::Entry {
            value: EntryPurpose {
                identity: value.last_included.clone().unwrap(),
                previous: None,
            },
        };
        assert!(owned_snapshot_install_signing_payload(&request).is_err());
        request = snapshot_install_request();
        request.proof.peer.claimant_node_id = "x".repeat(512 * 1024);
        assert!(owned_snapshot_install_signing_payload(&request).is_err());
        request = snapshot_install_request();
        request.reference.checksum.pop();
        assert!(owned_snapshot_install_signing_payload(&request).is_err());
        let source = NodeIdentity::new_for_test(1);
        let mut member = activation_member(
            &source,
            crate::membership::ClusterId::generate(),
            &source,
            crate::now_unix_ms(),
        );
        member.negotiated_protocol.features = vec![
            bmux_cluster_plugin_api::cluster_consensus_rpc::INTERFACE_ID.to_string(),
            bmux_cluster_plugin_api::cluster_consensus_transfer::INTERFACE_ID.to_string(),
        ];
        assert!(supports_owned_consensus(&member));
        assert!(!supports_owned_snapshot_install(&member));
        member
            .negotiated_protocol
            .features
            .push(bmux_cluster_plugin_api::cluster_consensus_snapshot::INTERFACE_ID.to_string());
        assert!(supports_owned_snapshot_install(&member));
        member.negotiated_protocol.features.remove(0);
        assert!(!supports_owned_snapshot_install(&member));
    }

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
    fn peer_rpc_signature_binds_operation_target_proof_and_payload() {
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
        let mut replay = request.clone();
        replay.proof.challenge.nonce.push('x');
        assert!(
            identity
                .verify(
                    &signing_payload(
                        "vote",
                        &replay.target_node_id,
                        &replay.proof,
                        &replay.payload
                    )
                    .unwrap(),
                    &request.signature,
                )
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
