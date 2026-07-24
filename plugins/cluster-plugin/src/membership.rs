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
const DEFAULT_ENROLLMENT_TTL_MS: u64 = 10 * 60 * 1_000;
const MAX_ENROLLMENT_TTL_MS: u64 = 24 * 60 * 60 * 1_000;
const ENROLLMENT_TOKEN_PREFIX: &str = "bmux-enroll-v1";

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
    state
        .members
        .entry(node_identity.node_id().to_string())
        .or_insert_with(|| ClusterMember {
            cluster_id: cluster_id.to_string(),
            node_id: node_identity.node_id().to_string(),
            public_key: node_identity.public_key().to_string(),
            endpoint: None,
            capabilities: initializer_capabilities(),
            joined_at_unix_ms: now,
            updated_at_unix_ms: now,
            state: ClusterMemberState::Active,
        });
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
    let member = ClusterMember {
        cluster_id: cluster_id.to_string(),
        node_id: request.node_id.clone(),
        public_key: request.public_key.clone(),
        endpoint: request.endpoint.clone(),
        capabilities: token.claims.capabilities.clone(),
        joined_at_unix_ms: joined_at,
        updated_at_unix_ms: now,
        state: ClusterMemberState::Active,
    };
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
    let local_node_id = load_node_identity(caller)?.map(|identity| identity.node_id().to_string());
    let Some(members) = value
        .get_mut("members")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return Ok(false);
    };
    let mut migrated = false;
    for (node_id, member) in members {
        let Some(member) = member.as_object_mut() else {
            continue;
        };
        if member.contains_key("capabilities") {
            continue;
        }
        let capabilities =
            if issuer_endpoint_missing && local_node_id.as_deref() == Some(node_id.as_str()) {
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

fn encode_hex(bytes: &[u8]) -> String {
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
