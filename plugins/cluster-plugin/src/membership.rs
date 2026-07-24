#![allow(clippy::wildcard_imports)] // Private domain modules share crate-private models.

use super::*;
use iroh::SecretKey;
use std::fmt;
use std::str::FromStr;
use std::sync::{Mutex, OnceLock};
use uuid::Uuid;

pub const CLUSTER_IDENTITY_VERSION: u8 = 1;
pub const CLUSTER_ID_STORAGE_KEY: &str = "cluster.identity.cluster.v1";
pub const NODE_IDENTITY_STORAGE_KEY: &str = "cluster.identity.node.v1";

pub const MEMBERSHIP_STATE_STORAGE_KEY: &str = "cluster.membership.state.v1";
pub const PENDING_LEAVE_STORAGE_KEY: &str = "cluster.membership.pending_leave.v1";
const PEER_AUTH_STATE_STORAGE_KEY: &str = "cluster.peer_auth.state.v1";
const PEER_AUTH_PROTOCOL_VERSION: u16 = 1;
const PEER_AUTH_CHALLENGE_TTL_MS: u64 = 30_000;
const MAX_PEER_AUTH_CLOCK_SKEW_MS: u64 = 5_000;
const MAX_PEER_AUTH_TRACKED_ENTRIES: usize = 1_024;
const DEFAULT_ENROLLMENT_TTL_MS: u64 = 10 * 60 * 1_000;
const MAX_ENROLLMENT_TTL_MS: u64 = 10 * 60 * 1_000;
const MEMBERSHIP_CREDENTIAL_TTL_MS: u64 = 24 * 60 * 60 * 1_000;
const ENROLLMENT_TOKEN_PREFIX: &str = "bmux-enroll-v1";
const CLUSTER_PEER_REVISION_MIN: u32 = 1;
const CLUSTER_PEER_REVISION_MAX: u32 = 1;
const CLUSTER_SCHEMA_VERSION_MIN: u32 = 1;
const CLUSTER_SCHEMA_VERSION_MAX: u32 = 1;
const CLUSTER_PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");
const CLUSTER_PROTOCOL_FEATURES: &[&str] = &[
    "membership-credential-v1",
    "node-possession-proof-v1",
    "single-use-enrollment-v1",
];

pub const fn initializer_capabilities() -> ClusterNodeCapabilities {
    ClusterNodeCapabilities {
        consensus_role: ClusterConsensusRole::Voter,
        worker: true,
        ingress: true,
    }
}

pub const fn default_join_capabilities() -> ClusterNodeCapabilities {
    ClusterNodeCapabilities {
        consensus_role: ClusterConsensusRole::ObserverEdge,
        worker: true,
        ingress: false,
    }
}

#[must_use]
pub fn current_protocol_offer() -> ClusterProtocolOffer {
    ClusterProtocolOffer {
        wire_epoch: bmux_ipc::CURRENT_WIRE_EPOCH,
        peer_revision_min: CLUSTER_PEER_REVISION_MIN,
        peer_revision_max: CLUSTER_PEER_REVISION_MAX,
        schema_version_min: CLUSTER_SCHEMA_VERSION_MIN,
        schema_version_max: CLUSTER_SCHEMA_VERSION_MAX,
        plugin_version: CLUSTER_PLUGIN_VERSION.to_string(),
        features: CLUSTER_PROTOCOL_FEATURES
            .iter()
            .map(|feature| (*feature).to_string())
            .collect(),
    }
}

fn negotiate_protocol(
    local: &ClusterProtocolOffer,
    remote: &ClusterProtocolOffer,
) -> Result<ClusterNegotiatedProtocol, String> {
    validate_protocol_offer(local, "local")?;
    validate_protocol_offer(remote, "joining")?;
    if local.wire_epoch != remote.wire_epoch {
        return Err(format!(
            "incompatible cluster wire epoch: local={} joining={}",
            local.wire_epoch, remote.wire_epoch
        ));
    }
    let peer_revision_min = local.peer_revision_min.max(remote.peer_revision_min);
    let peer_revision = local.peer_revision_max.min(remote.peer_revision_max);
    if peer_revision_min > peer_revision {
        return Err(format!(
            "no compatible cluster peer revision: local={}..={} joining={}..={}",
            local.peer_revision_min,
            local.peer_revision_max,
            remote.peer_revision_min,
            remote.peer_revision_max
        ));
    }
    let schema_version_min = local.schema_version_min.max(remote.schema_version_min);
    let schema_version = local.schema_version_max.min(remote.schema_version_max);
    if schema_version_min > schema_version {
        return Err(format!(
            "no compatible cluster schema version: local={}..={} joining={}..={}",
            local.schema_version_min,
            local.schema_version_max,
            remote.schema_version_min,
            remote.schema_version_max
        ));
    }
    for required in CLUSTER_PROTOCOL_FEATURES {
        if !remote.features.iter().any(|feature| feature == required) {
            return Err(format!(
                "joining node is missing mandatory cluster feature '{required}'"
            ));
        }
    }
    let mut features = local
        .features
        .iter()
        .filter(|feature| remote.features.contains(feature))
        .cloned()
        .collect::<Vec<_>>();
    features.sort();
    features.dedup();
    Ok(ClusterNegotiatedProtocol {
        wire_epoch: local.wire_epoch,
        peer_revision,
        schema_version,
        local_plugin_version: local.plugin_version.clone(),
        remote_plugin_version: remote.plugin_version.clone(),
        features,
    })
}

fn validate_protocol_offer(offer: &ClusterProtocolOffer, source: &str) -> Result<(), String> {
    if offer.peer_revision_min == 0 || offer.peer_revision_min > offer.peer_revision_max {
        return Err(format!("{source} cluster peer revision range is invalid"));
    }
    if offer.schema_version_min == 0 || offer.schema_version_min > offer.schema_version_max {
        return Err(format!("{source} cluster schema version range is invalid"));
    }
    if offer.plugin_version.trim().is_empty() || offer.plugin_version.len() > 128 {
        return Err(format!("{source} cluster plugin version is invalid"));
    }
    if offer.features.len() > 128
        || offer
            .features
            .iter()
            .any(|feature| feature.is_empty() || feature.len() > 128)
    {
        return Err(format!("{source} cluster feature advertisement is invalid"));
    }
    Ok(())
}

static IDENTITY_INIT_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// Durable identifier for one federated cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClusterId(Uuid);

impl ClusterId {
    #[must_use]
    pub fn generate() -> Self {
        Self(Uuid::new_v4())
    }
}

impl fmt::Display for ClusterId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "cluster:{}", self.0)
    }
}

impl FromStr for ClusterId {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let uuid = value
            .strip_prefix("cluster:")
            .ok_or_else(|| "cluster ID must start with 'cluster:'".to_string())?
            .parse::<Uuid>()
            .map_err(|error| format!("invalid cluster UUID: {error}"))?;
        if uuid.is_nil() {
            return Err("cluster ID cannot be nil".to_string());
        }
        Ok(Self(uuid))
    }
}

/// Self-authenticating node identifier derived from an Ed25519 public key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(String);

impl NodeId {
    pub(crate) fn from_secret_key(secret_key: &SecretKey) -> Self {
        Self(format!("node:{}", secret_key.public()))
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for NodeId {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let encoded = value
            .strip_prefix("node:")
            .ok_or_else(|| "node ID must start with 'node:'".to_string())?;
        let public_key = encoded
            .parse::<iroh::PublicKey>()
            .map_err(|error| format!("invalid node public key: {error}"))?;
        Ok(Self(format!("node:{public_key}")))
    }
}

#[derive(Debug, Clone)]
pub struct NodeIdentity {
    node_id: NodeId,
    secret_key: SecretKey,
}

impl NodeIdentity {
    #[must_use]
    pub const fn node_id(&self) -> &NodeId {
        &self.node_id
    }

    #[must_use]
    pub fn public_key(&self) -> iroh::PublicKey {
        self.secret_key.public()
    }

    pub fn sign(&self, payload: &[u8]) -> Vec<u8> {
        self.secret_key.sign(payload).to_bytes().to_vec()
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StoredClusterIdentity {
    pub version: u8,
    pub cluster_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StoredNodeIdentity {
    pub version: u8,
    pub node_id: String,
    pub public_key: String,
    pub secret_key: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MembershipState {
    version: u8,
    cluster_id: String,
    issuer_endpoint: Option<String>,
    members: BTreeMap<String, ClusterMember>,
    enrollment_tokens: BTreeMap<String, StoredEnrollment>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredEnrollment {
    request_id: String,
    expires_at_unix_ms: u64,
    token: String,
    consumed_by: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollmentClaims {
    pub version: u8,
    pub cluster_id: String,
    pub issuer_node_id: String,
    pub issuer_public_key: String,
    pub issuer_endpoint: String,
    pub capabilities: ClusterNodeCapabilities,
    pub request_id: String,
    pub nonce: String,
    pub issued_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NodePossessionClaims {
    version: u8,
    token_nonce: String,
    cluster_id: String,
    node_id: String,
    public_key: String,
    endpoint: Option<String>,
    protocol: ClusterProtocolOffer,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MembershipCredentialClaims {
    version: u8,
    serial: String,
    cluster_id: String,
    node_id: String,
    public_key: String,
    capabilities: ClusterNodeCapabilities,
    negotiated_protocol: ClusterNegotiatedProtocol,
    issued_at_unix_ms: u64,
    expires_at_unix_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedEnrollmentToken {
    pub claims: EnrollmentClaims,
    pub signature: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaveClaims {
    pub version: u8,
    pub leave_id: String,
    pub cluster_id: String,
    pub node_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingLeave {
    transaction: String,
    cluster: String,
    node: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PeerAuthState {
    version: u16,
    challenges: BTreeMap<String, StoredPeerAuthChallenge>,
    consumed: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredPeerAuthChallenge {
    challenge: PeerAuthChallenge,
    claimant_node_id: String,
}

pub fn adopt_join_result(
    caller: &impl ClusterRuntimeOps,
    request: &ClusterCommandJoinRequest,
) -> Result<ClusterJoinResult, String> {
    let token = decode_and_verify_enrollment_token(&request.token, now_unix_ms())?;
    let issuer_endpoint = request.issuer.trim();
    if issuer_endpoint.is_empty() {
        return Err("join issuer endpoint cannot be empty".to_string());
    }
    let local_identity = load_or_create_node_identity(caller)?;
    if let Some(existing_cluster) = load_cluster_id(caller)? {
        return existing_join_result(caller, &token, &local_identity, existing_cluster);
    }
    validate_join_result(&token, &local_identity, &request.enrollment_result)?;

    let _guard = identity_init_guard()?;
    if let Some(existing_cluster) = load_cluster_id(caller)? {
        return existing_join_result(caller, &token, &local_identity, existing_cluster);
    }
    let cluster_id = token.claims.cluster_id.parse::<ClusterId>()?;
    let state = MembershipState {
        version: CLUSTER_IDENTITY_VERSION,
        cluster_id: cluster_id.to_string(),
        issuer_endpoint: Some(issuer_endpoint.to_string()),
        members: request
            .enrollment_result
            .members
            .iter()
            .cloned()
            .map(|member| (member.node_id.clone(), member))
            .collect(),
        enrollment_tokens: BTreeMap::new(),
    };
    store_membership_state(caller, &state)?;
    store_cluster_id(caller, cluster_id)?;
    Ok(request.enrollment_result.clone())
}

fn existing_join_result(
    caller: &impl ClusterRuntimeOps,
    token: &SignedEnrollmentToken,
    identity: &NodeIdentity,
    cluster_id: ClusterId,
) -> Result<ClusterJoinResult, String> {
    if cluster_id.to_string() != token.claims.cluster_id {
        return Err("this node already belongs to a different cluster".to_string());
    }
    let state = require_membership_state(caller, cluster_id)?;
    let member = state
        .members
        .get(&identity.node_id().to_string())
        .cloned()
        .ok_or_else(|| "local membership snapshot omits this node".to_string())?;
    Ok(ClusterJoinResult {
        identity: ClusterIdentityResponse {
            cluster_id: Some(cluster_id.to_string()),
            node_id: token.claims.issuer_node_id.clone(),
            public_key: token.claims.issuer_public_key.clone(),
            capabilities: state
                .members
                .get(&token.claims.issuer_node_id)
                .map(|issuer| issuer.capabilities.clone()),
            protocol: current_protocol_offer(),
        },
        member,
        members: state.members.into_values().collect(),
    })
}

pub fn prepare_leave(caller: &impl ClusterRuntimeOps) -> Result<ClusterLeaveRequest, String> {
    let cluster_id =
        load_cluster_id(caller)?.ok_or_else(|| "this node is not a cluster member".to_string())?;
    let identity = load_node_identity(caller)?
        .ok_or_else(|| "node identity is not initialized".to_string())?;
    let state = require_membership_state(caller, cluster_id)?;
    let claims = LeaveClaims {
        version: CLUSTER_IDENTITY_VERSION,
        leave_id: Uuid::new_v4().to_string(),
        cluster_id: cluster_id.to_string(),
        node_id: identity.node_id().to_string(),
    };
    let request = ClusterLeaveRequest {
        leave_id: claims.leave_id.clone(),
        issuer_endpoint: state.issuer_endpoint,
        cluster_id: claims.cluster_id.clone(),
        node_id: claims.node_id.clone(),
        signature: identity.sign(&canonical_leave_claims(&claims)?),
    };
    store_identity_record(
        caller,
        PENDING_LEAVE_STORAGE_KEY,
        &PendingLeave {
            transaction: claims.leave_id,
            cluster: claims.cluster_id,
            node: claims.node_id,
        },
    )?;
    Ok(request)
}

pub fn commit_leave(
    caller: &impl ClusterRuntimeOps,
    leave_id: &str,
) -> Result<ClusterLeaveResult, String> {
    let value = load_identity_record(caller, PENDING_LEAVE_STORAGE_KEY)?
        .ok_or_else(|| "no prepared cluster leave exists".to_string())?;
    let pending = serde_json::from_slice::<PendingLeave>(&value)
        .map_err(|error| format!("pending leave state is corrupt: {error}"))?;
    if pending.transaction != leave_id {
        return Err("leave commit ID does not match prepared leave".to_string());
    }
    let current_cluster = load_cluster_id(caller)?
        .ok_or_else(|| "cluster membership was already cleared".to_string())?;
    let current_node = load_node_identity(caller)?
        .ok_or_else(|| "node identity is not initialized".to_string())?;
    if pending.cluster != current_cluster.to_string()
        || pending.node != current_node.node_id().to_string()
    {
        return Err("prepared leave does not match current cluster membership".to_string());
    }
    clear_local_cluster_membership(caller)?;
    clear_identity_record(caller, PENDING_LEAVE_STORAGE_KEY)?;
    Ok(ClusterLeaveResult {
        leave_id: pending.transaction,
        node_id: pending.node,
        left: true,
    })
}

pub fn accept_leave(
    caller: &impl ClusterRuntimeOps,
    request: &ClusterCommandAcceptLeaveRequest,
) -> Result<ClusterLeaveResult, String> {
    let cluster_id =
        load_cluster_id(caller)?.ok_or_else(|| "cluster is not initialized".to_string())?;
    if request.cluster_id != cluster_id.to_string() {
        return Err("leave request belongs to a different cluster".to_string());
    }
    let _guard = identity_init_guard()?;
    let mut state = require_membership_state(caller, cluster_id)?;
    let member = state
        .members
        .get(&request.node_id)
        .ok_or_else(|| "leave request node is not a cluster member".to_string())?;
    let public_key = member
        .public_key
        .parse::<iroh::PublicKey>()
        .map_err(|error| format!("member public key is invalid: {error}"))?;
    if request.node_id != format!("node:{public_key}") {
        return Err("member node ID does not match member public key".to_string());
    }
    let signature = iroh::Signature::try_from(request.signature.as_slice())
        .map_err(|error| format!("invalid leave signature: {error}"))?;
    let claims = LeaveClaims {
        version: CLUSTER_IDENTITY_VERSION,
        leave_id: request.leave_id.clone(),
        cluster_id: request.cluster_id.clone(),
        node_id: request.node_id.clone(),
    };
    public_key
        .verify(&canonical_leave_claims(&claims)?, &signature)
        .map_err(|_| "leave signature verification failed".to_string())?;
    if member.state == ClusterMemberState::Left {
        return Ok(ClusterLeaveResult {
            leave_id: request.leave_id.clone(),
            node_id: request.node_id.clone(),
            left: true,
        });
    }
    let local_node_id = load_node_identity(caller)?
        .ok_or_else(|| "local node identity is not initialized".to_string())?
        .node_id()
        .to_string();
    if request.node_id == local_node_id
        && state.members.values().any(|candidate| {
            candidate.node_id != request.node_id && candidate.state == ClusterMemberState::Active
        })
    {
        return Err("initializer cannot leave while other active members remain".to_string());
    }
    let member = state
        .members
        .get_mut(&request.node_id)
        .ok_or_else(|| "leave request node disappeared before update".to_string())?;
    member.state = ClusterMemberState::Left;
    member.updated_at_unix_ms = now_unix_ms();
    store_membership_state(caller, &state)?;
    Ok(ClusterLeaveResult {
        leave_id: request.leave_id.clone(),
        node_id: request.node_id.clone(),
        left: true,
    })
}

fn validate_join_result(
    token: &SignedEnrollmentToken,
    local_identity: &NodeIdentity,
    result: &ClusterJoinResult,
) -> Result<(), String> {
    if result.identity.cluster_id.as_deref() != Some(token.claims.cluster_id.as_str()) {
        return Err("issuer returned a different cluster ID".to_string());
    }
    if result.identity.node_id != token.claims.issuer_node_id
        || result.identity.public_key != token.claims.issuer_public_key
    {
        return Err("issuer response identity does not match enrollment token".to_string());
    }
    let expected_protocol =
        negotiate_protocol(&current_protocol_offer(), &result.identity.protocol)?;
    if result.member.negotiated_protocol != expected_protocol {
        return Err("issuer returned a different negotiated protocol".to_string());
    }
    verify_membership_credential(&result.member, now_unix_ms())?;
    if result.member.credential_issuer_node_id != token.claims.issuer_node_id
        || result.member.credential_issuer_public_key != token.claims.issuer_public_key
    {
        return Err("membership credential issuer does not match enrollment token".to_string());
    }
    if result.member.node_id != local_identity.node_id().to_string()
        || result.member.public_key != local_identity.public_key().to_string()
        || result.member.cluster_id != token.claims.cluster_id
        || result.member.capabilities != token.claims.capabilities
        || result.member.state != ClusterMemberState::Active
    {
        return Err("issuer returned invalid joining member state".to_string());
    }
    if !result.members.iter().any(|member| member == &result.member) {
        return Err("issuer member snapshot omitted joining member".to_string());
    }
    Ok(())
}

fn clear_identity_record(caller: &impl ClusterRuntimeOps, key: &str) -> Result<(), String> {
    caller
        .storage_set(&StorageSetRequest::new(
            bmux_plugin_sdk::StorageKey::new(key)
                .map_err(|error| format!("invalid identity storage key: {error}"))?,
            Vec::new(),
        ))
        .map_err(|error| format!("failed clearing cluster identity state: {error}"))
}

fn clear_local_cluster_membership(caller: &impl ClusterRuntimeOps) -> Result<(), String> {
    clear_identity_record(caller, CLUSTER_ID_STORAGE_KEY)?;
    clear_identity_record(caller, MEMBERSHIP_STATE_STORAGE_KEY)
}

pub fn canonical_leave_claims(claims: &LeaveClaims) -> Result<Vec<u8>, String> {
    serde_json::to_vec(claims).map_err(|error| format!("failed encoding leave claims: {error}"))
}

fn canonical_possession_claims(claims: &NodePossessionClaims) -> Result<Vec<u8>, String> {
    serde_json::to_vec(claims)
        .map_err(|error| format!("failed encoding node possession claims: {error}"))
}

fn canonical_membership_credential_claims(
    claims: &MembershipCredentialClaims,
) -> Result<Vec<u8>, String> {
    serde_json::to_vec(claims)
        .map_err(|error| format!("failed encoding membership credential claims: {error}"))
}

pub fn canonical_peer_challenge(challenge: &PeerAuthChallenge) -> Result<Vec<u8>, String> {
    #[derive(Serialize)]
    struct UnsignedPeerChallenge<'a> {
        protocol_version: u16,
        cluster_id: &'a str,
        verifier_node_id: &'a str,
        verifier_credential_serial: &'a str,
        audience_node_id: &'a str,
        nonce: &'a str,
        issued_at_unix_ms: u64,
        expires_at_unix_ms: u64,
    }
    serde_json::to_vec(&UnsignedPeerChallenge {
        protocol_version: challenge.protocol_version,
        cluster_id: &challenge.cluster_id,
        verifier_node_id: &challenge.verifier_node_id,
        verifier_credential_serial: &challenge.verifier_credential_serial,
        audience_node_id: &challenge.audience_node_id,
        nonce: &challenge.nonce,
        issued_at_unix_ms: challenge.issued_at_unix_ms,
        expires_at_unix_ms: challenge.expires_at_unix_ms,
    })
    .map_err(|error| format!("failed encoding peer authentication challenge: {error}"))
}

pub fn canonical_peer_proof(proof: &PeerAuthProof) -> Result<Vec<u8>, String> {
    #[derive(Serialize)]
    struct UnsignedPeerProof<'a> {
        challenge: &'a PeerAuthChallenge,
        claimant_node_id: &'a str,
        claimant_credential_serial: &'a str,
    }
    serde_json::to_vec(&UnsignedPeerProof {
        challenge: &proof.challenge,
        claimant_node_id: &proof.claimant_node_id,
        claimant_credential_serial: &proof.claimant_credential_serial,
    })
    .map_err(|error| format!("failed encoding peer authentication proof: {error}"))
}

pub fn create_peer_auth_challenge(
    caller: &impl ClusterRuntimeOps,
    request: &ClusterPeerChallengeRequest,
) -> Result<PeerAuthChallenge, String> {
    let claimant_node_id = request.claimant_node_id.parse::<NodeId>()?.to_string();
    let cluster_id = load_cluster_id(caller)?
        .ok_or_else(|| "peer authentication requires initialized cluster membership".to_string())?;
    let identity = load_node_identity(caller)?
        .ok_or_else(|| "peer authentication requires a local node identity".to_string())?;
    let membership = require_membership_state(caller, cluster_id)?;
    let verifier =
        active_valid_member(&membership, &identity.node_id().to_string(), now_unix_ms())?;
    active_valid_member(&membership, &claimant_node_id, now_unix_ms())?;
    let now = now_unix_ms();
    let mut challenge = PeerAuthChallenge {
        protocol_version: PEER_AUTH_PROTOCOL_VERSION,
        cluster_id: cluster_id.to_string(),
        verifier_node_id: identity.node_id().to_string(),
        verifier_credential_serial: verifier.credential_serial.clone(),
        audience_node_id: claimant_node_id.clone(),
        nonce: Uuid::new_v4().to_string(),
        issued_at_unix_ms: now,
        expires_at_unix_ms: now
            .checked_add(PEER_AUTH_CHALLENGE_TTL_MS)
            .ok_or_else(|| "peer authentication challenge expiry overflow".to_string())?,
        signature: String::new(),
    };
    challenge.signature = encode_hex(&identity.sign(&canonical_peer_challenge(&challenge)?));
    let _guard = identity_init_guard()?;
    let mut auth_state = load_peer_auth_state(caller)?;
    prune_peer_auth_state(&mut auth_state, now);
    auth_state.challenges.insert(
        challenge.nonce.clone(),
        StoredPeerAuthChallenge {
            challenge: challenge.clone(),
            claimant_node_id,
        },
    );
    enforce_peer_auth_state_bound(&mut auth_state);
    store_peer_auth_state(caller, &auth_state)?;
    Ok(challenge)
}

pub fn create_peer_auth_proof(
    caller: &impl ClusterRuntimeOps,
    challenge: PeerAuthChallenge,
) -> Result<PeerAuthProof, String> {
    let now = now_unix_ms();
    let cluster_id = load_cluster_id(caller)?
        .ok_or_else(|| "peer authentication requires initialized cluster membership".to_string())?;
    let identity = load_node_identity(caller)?
        .ok_or_else(|| "peer authentication requires a local node identity".to_string())?;
    let membership = require_membership_state(caller, cluster_id)?;
    let claimant = active_valid_member(&membership, &identity.node_id().to_string(), now)?;
    validate_peer_auth_challenge(
        &challenge,
        &membership,
        &identity.node_id().to_string(),
        now,
    )?;
    let mut proof = PeerAuthProof {
        challenge,
        claimant_node_id: identity.node_id().to_string(),
        claimant_credential_serial: claimant.credential_serial.clone(),
        claimant_signature: String::new(),
    };
    proof.claimant_signature = encode_hex(&identity.sign(&canonical_peer_proof(&proof)?));
    Ok(proof)
}

pub fn authenticate_peer(
    caller: &impl ClusterRuntimeOps,
    request: &ClusterPeerAuthenticateRequest,
) -> Result<AuthenticatedPeer, String> {
    let proof = &request.proof;
    let now = now_unix_ms();
    let cluster_id = load_cluster_id(caller)?
        .ok_or_else(|| "peer authentication requires initialized cluster membership".to_string())?;
    let identity = load_node_identity(caller)?
        .ok_or_else(|| "peer authentication requires a local node identity".to_string())?;
    let membership = require_membership_state(caller, cluster_id)?;
    validate_peer_auth_challenge(&proof.challenge, &membership, &proof.claimant_node_id, now)?;
    if proof.challenge.verifier_node_id != identity.node_id().to_string() {
        return Err("peer authentication challenge targets a different verifier".to_string());
    }
    if proof.challenge.audience_node_id != proof.claimant_node_id {
        return Err("peer authentication proof has the wrong audience".to_string());
    }
    let claimant = active_valid_member(&membership, &proof.claimant_node_id, now)?;
    if proof.claimant_credential_serial != claimant.credential_serial {
        return Err("peer authentication proof uses a stale claimant credential".to_string());
    }
    let claimant_key = claimant
        .public_key
        .parse::<iroh::PublicKey>()
        .map_err(|error| format!("claimant membership public key is invalid: {error}"))?;
    let claimant_signature_bytes = decode_hex(&proof.claimant_signature)
        .map_err(|error| format!("invalid peer authentication proof signature: {error}"))?;
    let claimant_signature = iroh::Signature::try_from(claimant_signature_bytes.as_slice())
        .map_err(|error| format!("invalid peer authentication proof signature: {error}"))?;
    claimant_key
        .verify(&canonical_peer_proof(proof)?, &claimant_signature)
        .map_err(|_| "peer authentication proof signature verification failed".to_string())?;

    let _guard = identity_init_guard()?;
    let mut auth_state = load_peer_auth_state(caller)?;
    prune_peer_auth_state(&mut auth_state, now);
    if auth_state.consumed.contains_key(&proof.challenge.nonce) {
        return Err("peer authentication challenge was already consumed".to_string());
    }
    let stored = auth_state
        .challenges
        .remove(&proof.challenge.nonce)
        .ok_or_else(|| "peer authentication challenge is unknown or expired".to_string())?;
    if stored.challenge != proof.challenge || stored.claimant_node_id != proof.claimant_node_id {
        return Err("peer authentication challenge does not match issued state".to_string());
    }
    auth_state.consumed.insert(
        proof.challenge.nonce.clone(),
        proof.challenge.expires_at_unix_ms,
    );
    enforce_peer_auth_state_bound(&mut auth_state);
    store_peer_auth_state(caller, &auth_state)?;
    Ok(AuthenticatedPeer {
        cluster_id: cluster_id.to_string(),
        node_id: claimant.node_id.clone(),
        capabilities: claimant.capabilities.clone(),
        credential_serial: claimant.credential_serial.clone(),
        authenticated_at_unix_ms: now,
    })
}

fn validate_peer_auth_challenge(
    challenge: &PeerAuthChallenge,
    membership: &MembershipState,
    expected_audience: &str,
    now: u64,
) -> Result<(), String> {
    if challenge.protocol_version != PEER_AUTH_PROTOCOL_VERSION {
        return Err("unsupported peer authentication protocol version".to_string());
    }
    if challenge.cluster_id != membership.cluster_id {
        return Err("peer authentication challenge belongs to a different cluster".to_string());
    }
    if challenge.audience_node_id != expected_audience {
        return Err("peer authentication challenge has the wrong audience".to_string());
    }
    if challenge.issued_at_unix_ms > now.saturating_add(MAX_PEER_AUTH_CLOCK_SKEW_MS) {
        return Err("peer authentication challenge was issued in the future".to_string());
    }
    if challenge.expires_at_unix_ms < now
        || challenge
            .expires_at_unix_ms
            .saturating_sub(challenge.issued_at_unix_ms)
            > PEER_AUTH_CHALLENGE_TTL_MS
    {
        return Err("peer authentication challenge is expired or has invalid validity".to_string());
    }
    let verifier = active_valid_member(membership, &challenge.verifier_node_id, now)?;
    if verifier.credential_serial != challenge.verifier_credential_serial {
        return Err("peer authentication challenge uses a stale verifier credential".to_string());
    }
    let verifier_key = verifier
        .public_key
        .parse::<iroh::PublicKey>()
        .map_err(|error| format!("verifier membership public key is invalid: {error}"))?;
    let signature_bytes = decode_hex(&challenge.signature)
        .map_err(|error| format!("invalid peer authentication challenge signature: {error}"))?;
    let signature = iroh::Signature::try_from(signature_bytes.as_slice())
        .map_err(|error| format!("invalid peer authentication challenge signature: {error}"))?;
    verifier_key
        .verify(&canonical_peer_challenge(challenge)?, &signature)
        .map_err(|_| "peer authentication challenge signature verification failed".to_string())
}

fn active_valid_member<'a>(
    membership: &'a MembershipState,
    node_id: &str,
    now: u64,
) -> Result<&'a ClusterMember, String> {
    let member = membership
        .members
        .get(node_id)
        .ok_or_else(|| "peer is not a cluster member".to_string())?;
    if member.state != ClusterMemberState::Active {
        return Err("peer membership is not active".to_string());
    }
    verify_membership_credential(member, now)?;
    Ok(member)
}

fn load_peer_auth_state(caller: &impl ClusterRuntimeOps) -> Result<PeerAuthState, String> {
    let Some(value) = load_identity_record(caller, PEER_AUTH_STATE_STORAGE_KEY)? else {
        return Ok(PeerAuthState {
            version: PEER_AUTH_PROTOCOL_VERSION,
            ..PeerAuthState::default()
        });
    };
    let state = serde_json::from_slice::<PeerAuthState>(&value)
        .map_err(|error| format!("peer authentication state is corrupt: {error}"))?;
    if state.version != PEER_AUTH_PROTOCOL_VERSION {
        return Err("unsupported peer authentication state version".to_string());
    }
    Ok(state)
}

fn store_peer_auth_state(
    caller: &impl ClusterRuntimeOps,
    state: &PeerAuthState,
) -> Result<(), String> {
    store_identity_record(caller, PEER_AUTH_STATE_STORAGE_KEY, state)
}

fn prune_peer_auth_state(state: &mut PeerAuthState, now: u64) {
    state
        .challenges
        .retain(|_, stored| stored.challenge.expires_at_unix_ms >= now);
    state.consumed.retain(|_, expires_at| *expires_at >= now);
}

fn enforce_peer_auth_state_bound(state: &mut PeerAuthState) {
    while state.challenges.len() + state.consumed.len() > MAX_PEER_AUTH_TRACKED_ENTRIES {
        let oldest_challenge = state
            .challenges
            .iter()
            .min_by_key(|(_, stored)| stored.challenge.expires_at_unix_ms)
            .map(|(nonce, stored)| (nonce.clone(), stored.challenge.expires_at_unix_ms));
        let oldest_consumed = state
            .consumed
            .iter()
            .min_by_key(|(_, expires_at)| **expires_at)
            .map(|(nonce, expires_at)| (nonce.clone(), *expires_at));
        match (oldest_challenge, oldest_consumed) {
            (Some((nonce, challenge_expiry)), Some((consumed_nonce, consumed_expiry))) => {
                if challenge_expiry <= consumed_expiry {
                    state.challenges.remove(&nonce);
                } else {
                    state.consumed.remove(&consumed_nonce);
                }
            }
            (Some((nonce, _)), None) => {
                state.challenges.remove(&nonce);
            }
            (None, Some((nonce, _))) => {
                state.consumed.remove(&nonce);
            }
            (None, None) => break,
        }
    }
}

pub fn create_enrollment_possession_proof(
    caller: &impl ClusterRuntimeOps,
    token: &SignedEnrollmentToken,
    endpoint: Option<String>,
    protocol: &ClusterProtocolOffer,
) -> Result<Vec<u8>, String> {
    let identity = load_or_create_node_identity(caller)?;
    let claims = NodePossessionClaims {
        version: CLUSTER_IDENTITY_VERSION,
        token_nonce: token.claims.nonce.clone(),
        cluster_id: token.claims.cluster_id.clone(),
        node_id: identity.node_id().to_string(),
        public_key: identity.public_key().to_string(),
        endpoint,
        protocol: protocol.clone(),
    };
    Ok(identity.sign(&canonical_possession_claims(&claims)?))
}

fn verify_enrollment_possession(
    token: &SignedEnrollmentToken,
    request: &ClusterCommandRedeemEnrollmentRequest,
    public_key: iroh::PublicKey,
) -> Result<(), String> {
    let claims = NodePossessionClaims {
        version: CLUSTER_IDENTITY_VERSION,
        token_nonce: token.claims.nonce.clone(),
        cluster_id: token.claims.cluster_id.clone(),
        node_id: request.node_id.clone(),
        public_key: request.public_key.clone(),
        endpoint: request.endpoint.clone(),
        protocol: request.protocol.clone(),
    };
    let signature = iroh::Signature::try_from(request.possession_signature.as_slice())
        .map_err(|error| format!("invalid node possession signature: {error}"))?;
    public_key
        .verify(&canonical_possession_claims(&claims)?, &signature)
        .map_err(|_| "joining node possession proof verification failed".to_string())
}

fn issue_membership_credential(
    issuer: &NodeIdentity,
    cluster_id: ClusterId,
    node_id: String,
    public_key: String,
    capabilities: ClusterNodeCapabilities,
    negotiated_protocol: ClusterNegotiatedProtocol,
    issued_at_unix_ms: u64,
) -> Result<ClusterMember, String> {
    let expires_at_unix_ms = issued_at_unix_ms
        .checked_add(MEMBERSHIP_CREDENTIAL_TTL_MS)
        .ok_or_else(|| "membership credential expiry overflow".to_string())?;
    let claims = MembershipCredentialClaims {
        version: CLUSTER_IDENTITY_VERSION,
        serial: Uuid::new_v4().to_string(),
        cluster_id: cluster_id.to_string(),
        node_id: node_id.clone(),
        public_key: public_key.clone(),
        capabilities: capabilities.clone(),
        negotiated_protocol: negotiated_protocol.clone(),
        issued_at_unix_ms,
        expires_at_unix_ms,
    };
    let signature = issuer.sign(&canonical_membership_credential_claims(&claims)?);
    Ok(ClusterMember {
        cluster_id: cluster_id.to_string(),
        node_id,
        public_key,
        endpoint: None,
        capabilities,
        credential_serial: claims.serial,
        credential_issuer_node_id: issuer.node_id().to_string(),
        credential_issuer_public_key: issuer.public_key().to_string(),
        credential_issued_at_unix_ms: issued_at_unix_ms,
        credential_expires_at_unix_ms: expires_at_unix_ms,
        credential_signature: encode_hex(&signature),
        negotiated_protocol,
        joined_at_unix_ms: issued_at_unix_ms,
        updated_at_unix_ms: issued_at_unix_ms,
        state: ClusterMemberState::Active,
    })
}

pub fn verify_membership_credential(
    member: &ClusterMember,
    now_unix_ms: u64,
) -> Result<(), String> {
    let issued_at_unix_ms = member.credential_issued_at_unix_ms;
    let expires_at_unix_ms = member.credential_expires_at_unix_ms;
    if expires_at_unix_ms < now_unix_ms {
        return Err("membership credential has expired".to_string());
    }
    if issued_at_unix_ms > now_unix_ms || issued_at_unix_ms >= expires_at_unix_ms {
        return Err("membership credential validity interval is invalid".to_string());
    }
    let issuer_node_id = member.credential_issuer_node_id.parse::<NodeId>()?;
    let issuer_public_key = member
        .credential_issuer_public_key
        .parse::<iroh::PublicKey>()
        .map_err(|error| format!("invalid membership credential issuer key: {error}"))?;
    if issuer_node_id.to_string() != format!("node:{issuer_public_key}") {
        return Err("membership credential issuer node ID does not match public key".to_string());
    }
    let claims = MembershipCredentialClaims {
        version: CLUSTER_IDENTITY_VERSION,
        serial: member.credential_serial.clone(),
        cluster_id: member.cluster_id.clone(),
        node_id: member.node_id.clone(),
        public_key: member.public_key.clone(),
        capabilities: member.capabilities.clone(),
        negotiated_protocol: member.negotiated_protocol.clone(),
        issued_at_unix_ms,
        expires_at_unix_ms,
    };
    let signature_bytes = decode_hex(&member.credential_signature)
        .map_err(|error| format!("invalid membership credential signature: {error}"))?;
    let signature = iroh::Signature::try_from(signature_bytes.as_slice())
        .map_err(|error| format!("invalid membership credential signature: {error}"))?;
    issuer_public_key
        .verify(
            &canonical_membership_credential_claims(&claims)?,
            &signature,
        )
        .map_err(|_| "membership credential signature verification failed".to_string())
}

pub fn initialize_cluster(
    caller: &impl ClusterRuntimeOps,
) -> Result<ClusterIdentityResponse, String> {
    let _guard = identity_init_guard()?;
    let cluster_id = load_cluster_id(caller)?.unwrap_or_else(ClusterId::generate);
    let node_identity = load_node_identity(caller)?
        .ok_or_else(|| "node identity must be initialized before cluster init".to_string())?;
    if load_cluster_id(caller)?.is_none() {
        store_cluster_id(caller, cluster_id)?;
    }
    let mut state = load_membership_state(caller)?.unwrap_or_else(|| MembershipState {
        version: CLUSTER_IDENTITY_VERSION,
        cluster_id: cluster_id.to_string(),
        issuer_endpoint: None,
        members: BTreeMap::new(),
        enrollment_tokens: BTreeMap::new(),
    });
    if state.cluster_id != cluster_id.to_string() {
        return Err("membership state cluster ID does not match local cluster ID".to_string());
    }
    let now = now_unix_ms();
    let node_id = node_identity.node_id().to_string();
    if let std::collections::btree_map::Entry::Vacant(entry) = state.members.entry(node_id) {
        let member = issue_membership_credential(
            &node_identity,
            cluster_id,
            node_identity.node_id().to_string(),
            node_identity.public_key().to_string(),
            initializer_capabilities(),
            negotiate_protocol(&current_protocol_offer(), &current_protocol_offer())?,
            now,
        )?;
        entry.insert(member);
    }
    let capabilities = state
        .members
        .get(&node_identity.node_id().to_string())
        .map(|member| member.capabilities.clone());
    store_membership_state(caller, &state)?;
    Ok(public_identity(cluster_id, &node_identity, capabilities))
}

pub fn current_node_identity(
    caller: &impl ClusterRuntimeOps,
) -> Result<ClusterIdentityResponse, String> {
    let identity = load_or_create_node_identity(caller)?;
    let cluster_id = load_cluster_id(caller)?;
    let capabilities = if let Some(cluster_id) = cluster_id {
        require_membership_state(caller, cluster_id)?
            .members
            .get(&identity.node_id().to_string())
            .map(|member| member.capabilities.clone())
    } else {
        None
    };
    Ok(ClusterIdentityResponse {
        cluster_id: cluster_id.map(|value| value.to_string()),
        node_id: identity.node_id().to_string(),
        public_key: identity.public_key().to_string(),
        capabilities,
        protocol: current_protocol_offer(),
    })
}

pub fn list_members(caller: &impl ClusterRuntimeOps) -> Result<ClusterMemberList, String> {
    let cluster_id = load_cluster_id(caller)?.map(|value| value.to_string());
    let members = load_membership_state(caller)?
        .map(|state| state.members.into_values().collect())
        .unwrap_or_default();
    Ok(ClusterMemberList {
        cluster_id,
        members,
    })
}

pub fn create_enrollment_token(
    caller: &impl ClusterRuntimeOps,
    request_id: &str,
    endpoint: &str,
    ttl_ms: Option<u64>,
    capabilities: Option<ClusterNodeCapabilities>,
) -> Result<EnrollmentTokenResult, String> {
    let request_id = request_id.trim();
    if request_id.is_empty() || request_id.len() > 128 {
        return Err("enrollment request_id must contain 1..=128 characters".to_string());
    }
    let endpoint = endpoint.trim();
    if endpoint.is_empty() {
        return Err("enrollment endpoint cannot be empty".to_string());
    }
    let ttl_ms = ttl_ms.unwrap_or(DEFAULT_ENROLLMENT_TTL_MS);
    if ttl_ms == 0 || ttl_ms > MAX_ENROLLMENT_TTL_MS {
        return Err(format!(
            "enrollment ttl_ms must be between 1 and {MAX_ENROLLMENT_TTL_MS}"
        ));
    }
    let capabilities = capabilities.unwrap_or_else(default_join_capabilities);
    let _guard = identity_init_guard()?;
    let cluster_id = load_cluster_id(caller)?
        .ok_or_else(|| "cluster is not initialized; run 'cluster init' first".to_string())?;
    let node_identity = load_node_identity(caller)?
        .ok_or_else(|| "node identity is not initialized".to_string())?;
    let mut state = require_membership_state(caller, cluster_id)?;
    if let Some(existing) = state
        .enrollment_tokens
        .values()
        .find(|enrollment| enrollment.request_id == request_id)
    {
        let signed = decode_and_verify_enrollment_token(&existing.token, now_unix_ms())?;
        if signed.claims.issuer_endpoint != endpoint
            || signed
                .claims
                .expires_at_unix_ms
                .saturating_sub(signed.claims.issued_at_unix_ms)
                != ttl_ms
            || signed.claims.capabilities != capabilities
        {
            return Err("enrollment request_id was reused with different arguments".to_string());
        }
        return Ok(EnrollmentTokenResult {
            token: existing.token.clone(),
            expires_at_unix_ms: existing.expires_at_unix_ms,
        });
    }
    let now = now_unix_ms();
    let expires_at_unix_ms = now
        .checked_add(ttl_ms)
        .ok_or_else(|| "enrollment expiry overflow".to_string())?;
    let claims = EnrollmentClaims {
        version: CLUSTER_IDENTITY_VERSION,
        cluster_id: cluster_id.to_string(),
        issuer_node_id: node_identity.node_id().to_string(),
        issuer_public_key: node_identity.public_key().to_string(),
        issuer_endpoint: endpoint.to_string(),
        capabilities,
        request_id: request_id.to_string(),
        nonce: Uuid::new_v4().to_string(),
        issued_at_unix_ms: now,
        expires_at_unix_ms,
    };
    let claims_bytes = canonical_claims(&claims)?;
    let signed = SignedEnrollmentToken {
        claims: claims.clone(),
        signature: node_identity.sign(&claims_bytes),
    };
    let token = encode_enrollment_token(&signed)?;
    state.enrollment_tokens.insert(
        claims.nonce,
        StoredEnrollment {
            request_id: request_id.to_string(),
            expires_at_unix_ms,
            token: token.clone(),
            consumed_by: None,
        },
    );
    store_membership_state(caller, &state)?;
    Ok(EnrollmentTokenResult {
        token,
        expires_at_unix_ms,
    })
}

pub fn redeem_enrollment(
    caller: &impl ClusterRuntimeOps,
    request: &ClusterCommandRedeemEnrollmentRequest,
) -> Result<ClusterJoinResult, String> {
    let token = decode_and_verify_enrollment_token(&request.token, now_unix_ms())?;
    let request_node_id = request.node_id.parse::<NodeId>()?;
    let request_public_key = request
        .public_key
        .parse::<iroh::PublicKey>()
        .map_err(|error| format!("invalid joining public key: {error}"))?;
    if request_node_id.to_string() != format!("node:{request_public_key}") {
        return Err("joining node ID does not match joining public key".to_string());
    }
    verify_enrollment_possession(&token, request, request_public_key)?;
    let negotiated_protocol = negotiate_protocol(&current_protocol_offer(), &request.protocol)?;
    let _guard = identity_init_guard()?;
    let cluster_id =
        load_cluster_id(caller)?.ok_or_else(|| "issuer cluster is not initialized".to_string())?;
    if token.claims.cluster_id != cluster_id.to_string() {
        return Err("enrollment token belongs to a different cluster".to_string());
    }
    let local_identity = load_node_identity(caller)?
        .ok_or_else(|| "issuer node identity is not initialized".to_string())?;
    if token.claims.issuer_node_id != local_identity.node_id().to_string()
        || token.claims.issuer_public_key != local_identity.public_key().to_string()
    {
        return Err("enrollment token issuer does not match this node".to_string());
    }
    let mut state = require_membership_state(caller, cluster_id)?;
    let stored = state
        .enrollment_tokens
        .get_mut(&token.claims.nonce)
        .ok_or_else(|| "enrollment token was not issued by this node".to_string())?;
    if stored.token != request.token {
        return Err("enrollment token does not match issued token".to_string());
    }
    if stored.expires_at_unix_ms < now_unix_ms() {
        return Err("enrollment token has expired".to_string());
    }
    if let Some(consumed_by) = &stored.consumed_by {
        if consumed_by != &request.node_id {
            return Err("enrollment token was already consumed by another node".to_string());
        }
        let member = state
            .members
            .get(&request.node_id)
            .cloned()
            .ok_or_else(|| "consumed enrollment token is missing its member record".to_string())?;
        if member.public_key != request.public_key
            || member.endpoint != request.endpoint
            || member.capabilities != token.claims.capabilities
            || member.negotiated_protocol != negotiated_protocol
        {
            return Err("enrollment retry does not match the committed member".to_string());
        }
        return Ok(ClusterJoinResult {
            identity: public_identity(
                cluster_id,
                &local_identity,
                state
                    .members
                    .get(&local_identity.node_id().to_string())
                    .map(|issuer| issuer.capabilities.clone()),
            ),
            member,
            members: state.members.into_values().collect(),
        });
    }
    stored.consumed_by = Some(request.node_id.clone());
    let now = now_unix_ms();
    let joined_at = state
        .members
        .get(&request.node_id)
        .map_or(now, |member| member.joined_at_unix_ms);
    let mut member = issue_membership_credential(
        &local_identity,
        cluster_id,
        request.node_id.clone(),
        request.public_key.clone(),
        token.claims.capabilities.clone(),
        negotiated_protocol,
        now,
    )?;
    member.endpoint.clone_from(&request.endpoint);
    member.joined_at_unix_ms = joined_at;
    state
        .members
        .insert(request.node_id.clone(), member.clone());
    store_membership_state(caller, &state)?;
    Ok(ClusterJoinResult {
        identity: public_identity(
            cluster_id,
            &local_identity,
            state
                .members
                .get(&local_identity.node_id().to_string())
                .map(|issuer| issuer.capabilities.clone()),
        ),
        member,
        members: state.members.into_values().collect(),
    })
}

fn public_identity(
    cluster_id: ClusterId,
    identity: &NodeIdentity,
    capabilities: Option<ClusterNodeCapabilities>,
) -> ClusterIdentityResponse {
    ClusterIdentityResponse {
        cluster_id: Some(cluster_id.to_string()),
        node_id: identity.node_id().to_string(),
        public_key: identity.public_key().to_string(),
        capabilities,
        protocol: current_protocol_offer(),
    }
}

fn require_membership_state(
    caller: &impl ClusterRuntimeOps,
    cluster_id: ClusterId,
) -> Result<MembershipState, String> {
    let state = load_membership_state(caller)?
        .ok_or_else(|| "cluster membership state is missing; run 'cluster init'".to_string())?;
    if state.cluster_id != cluster_id.to_string() {
        return Err("membership state cluster ID does not match local cluster ID".to_string());
    }
    Ok(state)
}

fn load_membership_state(
    caller: &impl ClusterRuntimeOps,
) -> Result<Option<MembershipState>, String> {
    let Some(value) = load_identity_record(caller, MEMBERSHIP_STATE_STORAGE_KEY)? else {
        return Ok(None);
    };
    let mut value = serde_json::from_slice::<serde_json::Value>(&value)
        .map_err(|error| format!("cluster membership state is corrupt: {error}"))?;
    let migrated = migrate_legacy_member_capabilities(caller, &mut value)?;
    let state = serde_json::from_value::<MembershipState>(value)
        .map_err(|error| format!("cluster membership state is corrupt: {error}"))?;
    ensure_identity_version(state.version, "membership")?;
    if migrated {
        store_membership_state(caller, &state)?;
    }
    Ok(Some(state))
}

fn migrate_legacy_member_capabilities(
    caller: &impl ClusterRuntimeOps,
    value: &mut serde_json::Value,
) -> Result<bool, String> {
    let issuer_endpoint_missing = value
        .get("issuer_endpoint")
        .is_none_or(serde_json::Value::is_null);
    let local_identity = load_node_identity(caller)?
        .ok_or_else(|| "node identity is required to migrate membership state".to_string())?;
    let local_node_id = local_identity.node_id().to_string();
    let cluster_id = value
        .get("cluster_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "legacy membership state omits cluster_id".to_string())?
        .parse::<ClusterId>()?;
    let Some(members) = value
        .get_mut("members")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return Ok(false);
    };
    let mut migrated = false;
    for (node_id, member_value) in members {
        let Some(member) = member_value.as_object_mut() else {
            continue;
        };
        if !member.contains_key("capabilities") {
            let capabilities = if issuer_endpoint_missing && local_node_id == *node_id {
                initializer_capabilities()
            } else {
                default_join_capabilities()
            };
            member.insert(
                "capabilities".to_string(),
                serde_json::to_value(capabilities)
                    .map_err(|error| format!("failed encoding migrated capabilities: {error}"))?,
            );
            migrated = true;
        }
        if !member.contains_key("credential_serial") {
            let public_key = member
                .get("public_key")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "legacy member omits public_key".to_string())?
                .to_string();
            let capabilities = serde_json::from_value::<ClusterNodeCapabilities>(
                member
                    .get("capabilities")
                    .cloned()
                    .ok_or_else(|| "legacy member omits capabilities".to_string())?,
            )
            .map_err(|error| format!("legacy member capabilities are invalid: {error}"))?;
            let endpoint = member
                .get("endpoint")
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string);
            let joined_at_unix_ms = member
                .get("joined_at_unix_ms")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or_else(now_unix_ms);
            let updated_at_unix_ms = member
                .get("updated_at_unix_ms")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(joined_at_unix_ms);
            let state = member
                .get("state")
                .cloned()
                .map(serde_json::from_value::<ClusterMemberState>)
                .transpose()
                .map_err(|error| format!("legacy member state is invalid: {error}"))?
                .unwrap_or(ClusterMemberState::Active);
            let mut migrated_member = issue_membership_credential(
                &local_identity,
                cluster_id,
                node_id.clone(),
                public_key,
                capabilities,
                negotiate_protocol(&current_protocol_offer(), &current_protocol_offer())?,
                now_unix_ms(),
            )?;
            migrated_member.endpoint = endpoint;
            migrated_member.joined_at_unix_ms = joined_at_unix_ms;
            migrated_member.updated_at_unix_ms = updated_at_unix_ms;
            migrated_member.state = state;
            *member_value = serde_json::to_value(migrated_member)
                .map_err(|error| format!("failed encoding migrated member: {error}"))?;
            migrated = true;
        }
    }
    Ok(migrated)
}

fn store_membership_state(
    caller: &impl ClusterRuntimeOps,
    state: &MembershipState,
) -> Result<(), String> {
    store_identity_record(caller, MEMBERSHIP_STATE_STORAGE_KEY, state)
}

fn canonical_claims(claims: &EnrollmentClaims) -> Result<Vec<u8>, String> {
    serde_json::to_vec(claims)
        .map_err(|error| format!("failed encoding enrollment claims: {error}"))
}

pub fn encode_enrollment_token(token: &SignedEnrollmentToken) -> Result<String, String> {
    let bytes = serde_json::to_vec(token)
        .map_err(|error| format!("failed encoding enrollment token: {error}"))?;
    Ok(format!("{ENROLLMENT_TOKEN_PREFIX}:{}", encode_hex(&bytes)))
}

pub fn decode_and_verify_enrollment_token(
    token: &str,
    now_unix_ms: u64,
) -> Result<SignedEnrollmentToken, String> {
    let encoded = token
        .strip_prefix(&format!("{ENROLLMENT_TOKEN_PREFIX}:"))
        .ok_or_else(|| "invalid enrollment token prefix".to_string())?;
    let bytes = decode_hex(encoded)?;
    let signed = serde_json::from_slice::<SignedEnrollmentToken>(&bytes)
        .map_err(|error| format!("invalid enrollment token: {error}"))?;
    ensure_identity_version(signed.claims.version, "enrollment token")?;
    if signed.claims.issued_at_unix_ms > now_unix_ms {
        return Err("enrollment token was issued in the future".to_string());
    }
    if signed.claims.expires_at_unix_ms < now_unix_ms {
        return Err("enrollment token has expired".to_string());
    }
    let issuer_node_id = signed.claims.issuer_node_id.parse::<NodeId>()?;
    let issuer_public_key = signed
        .claims
        .issuer_public_key
        .parse::<iroh::PublicKey>()
        .map_err(|error| format!("invalid enrollment issuer public key: {error}"))?;
    if issuer_node_id.to_string() != format!("node:{issuer_public_key}") {
        return Err("enrollment issuer node ID does not match public key".to_string());
    }
    let signature = iroh::Signature::try_from(signed.signature.as_slice())
        .map_err(|error| format!("invalid enrollment signature: {error}"))?;
    issuer_public_key
        .verify(&canonical_claims(&signed.claims)?, &signature)
        .map_err(|_| "enrollment token signature verification failed".to_string())?;
    Ok(signed)
}

pub fn encode_hex(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

fn decode_hex(value: &str) -> Result<Vec<u8>, String> {
    if !value.len().is_multiple_of(2) {
        return Err("enrollment token hex payload has odd length".to_string());
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|chunk| {
            let pair = std::str::from_utf8(chunk)
                .map_err(|error| format!("invalid enrollment token hex: {error}"))?;
            u8::from_str_radix(pair, 16)
                .map_err(|error| format!("invalid enrollment token hex: {error}"))
        })
        .collect()
}

pub fn load_cluster_id(caller: &impl ClusterRuntimeOps) -> Result<Option<ClusterId>, String> {
    let Some(value) = load_identity_record(caller, CLUSTER_ID_STORAGE_KEY)? else {
        return Ok(None);
    };
    let record = serde_json::from_slice::<StoredClusterIdentity>(&value)
        .map_err(|error| format!("cluster identity record is corrupt: {error}"))?;
    ensure_identity_version(record.version, "cluster")?;
    record.cluster_id.parse().map(Some)
}

fn store_cluster_id(caller: &impl ClusterRuntimeOps, cluster_id: ClusterId) -> Result<(), String> {
    store_identity_record(
        caller,
        CLUSTER_ID_STORAGE_KEY,
        &StoredClusterIdentity {
            version: CLUSTER_IDENTITY_VERSION,
            cluster_id: cluster_id.to_string(),
        },
    )
}

pub fn load_or_create_node_identity(
    caller: &impl ClusterRuntimeOps,
) -> Result<NodeIdentity, String> {
    let _guard = identity_init_guard()?;
    if let Some(identity) = load_node_identity(caller)? {
        return Ok(identity);
    }
    let secret_key = SecretKey::generate();
    let node_id = NodeId::from_secret_key(&secret_key);
    let record = StoredNodeIdentity {
        version: CLUSTER_IDENTITY_VERSION,
        node_id: node_id.to_string(),
        public_key: secret_key.public().to_string(),
        secret_key: secret_key.as_signing_key().to_bytes().to_vec(),
    };
    store_identity_record(caller, NODE_IDENTITY_STORAGE_KEY, &record)?;
    let persisted = load_node_identity(caller)?
        .ok_or_else(|| "node identity disappeared after persistence".to_string())?;
    if persisted.node_id != node_id {
        return Err("persisted node identity does not match generated identity".to_string());
    }
    Ok(persisted)
}

pub fn load_node_identity(caller: &impl ClusterRuntimeOps) -> Result<Option<NodeIdentity>, String> {
    let Some(value) = load_identity_record(caller, NODE_IDENTITY_STORAGE_KEY)? else {
        return Ok(None);
    };
    let record = serde_json::from_slice::<StoredNodeIdentity>(&value)
        .map_err(|error| format!("node identity record is corrupt: {error}"))?;
    ensure_identity_version(record.version, "node")?;
    let secret_bytes: [u8; 32] = record
        .secret_key
        .try_into()
        .map_err(|value: Vec<u8>| format!("node private key has invalid length {}", value.len()))?;
    let secret_key = SecretKey::from(secret_bytes);
    let public_key = secret_key.public();
    if public_key.to_string() != record.public_key {
        return Err("node identity public key does not match private key".to_string());
    }
    let node_id = record.node_id.parse::<NodeId>()?;
    let derived = NodeId::from_secret_key(&secret_key);
    if node_id != derived {
        return Err("node ID does not match persisted private key".to_string());
    }
    Ok(Some(NodeIdentity {
        node_id,
        secret_key,
    }))
}

fn identity_init_guard() -> Result<std::sync::MutexGuard<'static, ()>, String> {
    IDENTITY_INIT_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| "cluster identity initialization lock is poisoned".to_string())
}

fn ensure_identity_version(version: u8, kind: &str) -> Result<(), String> {
    if version == CLUSTER_IDENTITY_VERSION {
        Ok(())
    } else {
        Err(format!(
            "unsupported {kind} identity record version {version}; expected {CLUSTER_IDENTITY_VERSION}"
        ))
    }
}

pub fn load_identity_record(
    caller: &impl ClusterRuntimeOps,
    key: &str,
) -> Result<Option<Vec<u8>>, String> {
    let key = bmux_plugin_sdk::StorageKey::new(key)
        .map_err(|error| format!("invalid identity storage key: {error}"))?;
    caller
        .storage_get(&StorageGetRequest::new(key))
        .map(|response| response.value.filter(|value| !value.is_empty()))
        .map_err(|error| format!("failed reading cluster identity: {error}"))
}

fn store_identity_record(
    caller: &impl ClusterRuntimeOps,
    key: &str,
    record: &impl Serialize,
) -> Result<(), String> {
    let key = bmux_plugin_sdk::StorageKey::new(key)
        .map_err(|error| format!("invalid identity storage key: {error}"))?;
    let value = serde_json::to_vec(record)
        .map_err(|error| format!("failed encoding cluster identity: {error}"))?;
    caller
        .storage_set(&StorageSetRequest::new(key, value))
        .map_err(|error| format!("failed writing cluster identity: {error}"))
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct ClusterSettings {
    pub clusters: BTreeMap<String, ClusterDefinition>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct ClusterDefinition {
    pub hosts: Vec<ClusterHostRef>,
    pub targets: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ClusterHostRef {
    Target(String),
    Object {
        target: Option<String>,
        host: Option<String>,
        name: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct ClusterInventory {
    pub clusters: BTreeMap<String, Vec<String>>,
    pub known_targets: BTreeSet<String>,
}

#[derive(Debug, Clone, Copy)]
pub enum HealthProbe {
    Test,
    Doctor,
}

pub fn collect_statuses(
    context: &NativeCommandContext,
    probe: HealthProbe,
) -> Result<Vec<ClusterHostStatus>, String> {
    let inventory = load_cluster_inventory(context)?;
    collect_statuses_for_selector(
        context,
        &inventory,
        positional_argument(&context.arguments),
        probe,
    )
}

pub fn collect_statuses_for_selector(
    caller: &impl ClusterRuntimeOps,
    inventory: &ClusterInventory,
    selector: Option<&str>,
    probe: HealthProbe,
) -> Result<Vec<ClusterHostStatus>, String> {
    if inventory.clusters.is_empty() {
        return Err(
            "no clusters configured in [plugins.settings.\"bmux.cluster\"].clusters".to_string(),
        );
    }

    let mut statuses = Vec::new();
    if let Some(selector) = selector {
        if let Some(hosts) = inventory.clusters.get(selector) {
            collect_cluster_statuses(
                caller,
                selector,
                hosts,
                &inventory.known_targets,
                probe,
                &mut statuses,
            );
            return Ok(statuses);
        }

        let mut matched_any = false;
        for (cluster_name, hosts) in &inventory.clusters {
            if hosts.iter().any(|host| host == selector) {
                matched_any = true;
                let selected = vec![selector.to_string()];
                collect_cluster_statuses(
                    caller,
                    cluster_name,
                    &selected,
                    &inventory.known_targets,
                    probe,
                    &mut statuses,
                );
            }
        }
        if matched_any {
            return Ok(statuses);
        }

        return Err(format!("unknown cluster or target '{selector}'"));
    }

    for (cluster_name, hosts) in &inventory.clusters {
        collect_cluster_statuses(
            caller,
            cluster_name,
            hosts,
            &inventory.known_targets,
            probe,
            &mut statuses,
        );
    }
    Ok(statuses)
}

pub fn collect_cluster_statuses(
    caller: &impl ClusterRuntimeOps,
    cluster_name: &str,
    hosts: &[String],
    known_targets: &BTreeSet<String>,
    probe: HealthProbe,
    statuses: &mut Vec<ClusterHostStatus>,
) {
    for host in hosts {
        if !known_targets.contains(host) {
            statuses.push(ClusterHostStatus {
                cluster: cluster_name.to_string(),
                target: host.clone(),
                state: ClusterHostState::Degraded,
                reason: Some("target is missing from [connections.targets]".to_string()),
            });
            continue;
        }

        match run_health_probe(caller, host, probe) {
            Ok(()) => statuses.push(ClusterHostStatus {
                cluster: cluster_name.to_string(),
                target: host.clone(),
                state: ClusterHostState::Ready,
                reason: None,
            }),
            Err(error) => statuses.push(ClusterHostStatus {
                cluster: cluster_name.to_string(),
                target: host.clone(),
                state: ClusterHostState::Degraded,
                reason: Some(error),
            }),
        }
    }
}

pub fn run_health_probe(
    caller: &impl ClusterRuntimeOps,
    target: &str,
    probe: HealthProbe,
) -> Result<(), String> {
    let command_path = match probe {
        HealthProbe::Test => vec!["remote".to_string(), "test".to_string()],
        HealthProbe::Doctor => vec!["remote".to_string(), "doctor".to_string()],
    };
    let request = CoreCliCommandRequest::new(command_path, vec![target.to_string()]);
    let response = caller
        .core_cli_command_run_path(&request)
        .map_err(|error| format!("probe failed to run: {error}"))?;
    if response.exit_code == EXIT_OK {
        Ok(())
    } else {
        Err(format!("probe exited with status {}", response.exit_code))
    }
}

pub fn load_cluster_inventory(context: &NativeCommandContext) -> Result<ClusterInventory, String> {
    load_cluster_inventory_for_context(&context.connection.config_dir, context.settings.clone())
}

pub fn load_cluster_inventory_for_context(
    config_dir: &str,
    settings: Option<toml::Value>,
) -> Result<ClusterInventory, String> {
    let config_path = PathBuf::from(config_dir).join("bmux.toml");
    let config = BmuxConfig::load_from_path(&config_path)
        .map_err(|error| format!("failed loading config {}: {error}", config_path.display()))?;

    let settings_value = settings
        .or_else(|| config.plugins.settings.get("bmux.cluster").cloned())
        .unwrap_or_else(|| toml::Value::Table(toml::map::Map::new()));
    let settings: ClusterSettings = settings_value
        .try_into()
        .map_err(|error| format!("invalid bmux.cluster settings: {error}"))?;

    let mut clusters = BTreeMap::new();
    for (name, definition) in settings.clusters {
        let mut targets = Vec::new();
        for host in &definition.hosts {
            if let Some(target) = target_from_host_ref(host) {
                targets.push(target);
            }
        }
        for target in definition.targets {
            if !target.trim().is_empty() {
                targets.push(target.trim().to_string());
            }
        }
        let unique = dedupe_preserve_order(targets);
        clusters.insert(name, unique);
    }

    let known_targets = config.connections.targets.keys().cloned().collect();

    Ok(ClusterInventory {
        clusters,
        known_targets,
    })
}

pub fn target_from_host_ref(host: &ClusterHostRef) -> Option<String> {
    match host {
        ClusterHostRef::Target(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        ClusterHostRef::Object { target, host, name } => target
            .as_deref()
            .or(host.as_deref())
            .or(name.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string),
    }
}

pub fn dedupe_preserve_order(values: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut deduped = Vec::new();
    for value in values {
        if seen.insert(value.clone()) {
            deduped.push(value);
        }
    }
    deduped
}
